//! pdf-fast-node: N-API bindings mirroring `pdf-fast-py` (full parity).
//!
//! Text/layout/table extraction over `pdf-fast-core`. The pure-Rust helpers
//! (`compute_full_text`, run separators, hyphenation/CJK logic) are ported
//! verbatim from `pdf-fast-py`.
//!
//! Arrow transport note: `pdf-fast-py` exposes both `*_arrow_ipc` (IPC bytes)
//! and `*_c_data` (raw pyarrow C-Data capsule pointers). Node.js has no safe
//! consumer for raw C-Data capsule pointers, so this crate exposes the Arrow
//! **IPC Buffer** variants for every batch (consume with the `apache-arrow`
//! package). The single-page multimodal batch that Python only exposed via
//! `page_multimodal_c_data` is preserved here as `pageMultimodalIpc(pageIndex)`.
use arrow::record_batch::RecordBatch;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use pdf_fast_core::{OcrTextRun, ParseOptions, PdfDocument};
use serde_json::json;
use std::fmt::Write as _;
use std::io::Cursor;

fn to_err(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

fn open_document(path: &str) -> Result<PdfDocument> {
    PdfDocument::open(
        path,
        ParseOptions {
            keep_decoded_streams: false,
            extract_text: true,
            extract_cmaps: true,
            ..ParseOptions::default()
        },
    )
    .map_err(to_err)
}

fn record_batch_to_ipc_bytes(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::with_capacity(65536));
    {
        let mut writer =
            arrow::ipc::writer::StreamWriter::try_new(&mut out, &batch.schema()).map_err(to_err)?;
        writer.write(batch).map_err(to_err)?;
        writer.finish().map_err(to_err)?;
    }
    Ok(out.into_inner())
}

fn parse_ocr_runs(ocr_runs_json: &str) -> Result<Vec<OcrTextRun>> {
    serde_json::from_str(ocr_runs_json).map_err(to_err)
}

/// Diagnostics for `fullText` decoding paths.
#[napi(object)]
#[derive(Default)]
pub struct FullTextDiagnostics {
    pub pages: u32,
    pub runs: u32,
    pub chars: u32,
    pub fallback_chars: u32,
    pub via_to_unicode: u32,
    pub via_type0: u32,
    pub via_differences: u32,
    pub via_named: u32,
    pub via_standard: u32,
    pub via_raw_byte: u32,
}

/// Result of `fullText`: the extracted text plus decoding diagnostics.
#[napi(object)]
pub struct FullTextResult {
    pub text: String,
    pub diagnostics: FullTextDiagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Separator {
    None,
    Space,
    Newline,
}

fn is_cjk_ideograph(c: char) -> bool {
    matches!(
        c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FFFF
    )
}

fn is_compound_prefix(stem: &str) -> bool {
    matches!(
        stem,
        "a" | "all"
            | "anti"
            | "auto"
            | "bi"
            | "bio"
            | "co"
            | "cross"
            | "de"
            | "dis"
            | "e"
            | "ex"
            | "extra"
            | "full"
            | "half"
            | "high"
            | "inter"
            | "intra"
            | "long"
            | "low"
            | "mega"
            | "meta"
            | "mid"
            | "multi"
            | "neo"
            | "new"
            | "non"
            | "off"
            | "old"
            | "on"
            | "out"
            | "over"
            | "pan"
            | "post"
            | "pre"
            | "pro"
            | "pseudo"
            | "re"
            | "self"
            | "semi"
            | "short"
            | "sub"
            | "super"
            | "tele"
            | "trans"
            | "ultra"
            | "un"
            | "under"
            | "well"
            | "x"
    )
}

#[allow(clippy::too_many_arguments)]
fn run_separator(
    prev_x: Option<f32>,
    prev_y: Option<f32>,
    prev_font_size: Option<f32>,
    prev_w: Option<f32>,
    prev_chars: usize,
    curr_x: Option<f32>,
    curr_y: Option<f32>,
    curr_font_size: Option<f32>,
    per_glyph_mode: bool,
) -> Separator {
    let (Some(py), Some(cy)) = (prev_y, curr_y) else {
        return Separator::Newline;
    };
    let fs = curr_font_size.or(prev_font_size).unwrap_or(10.0).max(1.0);
    let same_baseline = (py - cy).abs() < 0.5 * fs;
    if !same_baseline {
        return Separator::Newline;
    }
    let (Some(px), Some(cx)) = (prev_x, curr_x) else {
        return Separator::Space;
    };
    let advance = cx - px;
    if per_glyph_mode {
        if advance > 0.7 * fs || advance < -0.7 * fs {
            Separator::Space
        } else {
            Separator::None
        }
    } else if advance.abs() < 0.05 * fs {
        Separator::None
    } else {
        let prev_fs = prev_font_size.unwrap_or(fs).max(1.0);
        let prev_end = px + prev_w.unwrap_or(prev_chars as f32 * 0.5 * prev_fs);
        let gap = cx - prev_end;
        if gap > 0.3 * fs || gap < -0.3 * fs {
            Separator::Space
        } else {
            Separator::None
        }
    }
}

fn utf8_prefix_at_most_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        s
    } else {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Ported verbatim from pdf-fast-py: page-aware text assembly with reading-order
/// routing, hyphenation joins, CJK handling, and decoding diagnostics.
fn compute_full_text(doc: &PdfDocument, reading_order: bool) -> (String, FullTextDiagnostics) {
    let pages = doc.pages();
    let mut buf = String::with_capacity(pages.len() * 2048);
    let mut diag = FullTextDiagnostics::default();

    for (idx, page) in pages.iter().enumerate() {
        let runs = doc.semantic_text_runs_for_page(page);
        diag.runs += runs.len() as u32;

        let run_order: Vec<usize> = if reading_order && runs.len() >= 2 {
            #[cfg(not(feature = "reading-order"))]
            {
                (0..runs.len()).collect()
            }
            #[cfg(feature = "reading-order")]
            {
                let paragraphs = pdf_fast_core::paragraph::merge_paragraphs(&runs);
                if paragraphs.len() < 2 {
                    (0..runs.len()).collect()
                } else {
                    use layout_fast_core::LayoutEngine;
                    let blocks: Vec<layout_fast_core::Block> = paragraphs
                        .iter()
                        .map(|p| layout_fast_core::Block {
                            id: p.id,
                            bbox: layout_fast_core::BBox::new(p.x, p.y, p.x + p.w, p.y + p.h),
                            font_size: Some(p.font_size),
                            font_id: None,
                            text: Some(p.text.clone()),
                            language: None,
                            source: layout_fast_core::BlockSource::PdfTextStream,
                            kind: None,
                        })
                        .collect();
                    let engine = layout_fast_core::RoutedEngine::with_default_config();
                    let order = engine.order(&blocks);
                    let valid = order.len() == paragraphs.len() && {
                        let mut seen = vec![false; paragraphs.len()];
                        order.iter().all(|&id| {
                            if id >= paragraphs.len() || seen[id] {
                                false
                            } else {
                                seen[id] = true;
                                true
                            }
                        })
                    };
                    if !valid {
                        (0..runs.len()).collect()
                    } else {
                        let mut emitted = vec![false; runs.len()];
                        let mut flat: Vec<usize> = Vec::with_capacity(runs.len());
                        for &p_id in &order {
                            for &run_idx in &paragraphs[p_id].line_run_indices {
                                if run_idx < runs.len() && !emitted[run_idx] {
                                    emitted[run_idx] = true;
                                    flat.push(run_idx);
                                }
                            }
                        }
                        for (i, done) in emitted.iter().enumerate() {
                            if !done {
                                flat.push(i);
                            }
                        }
                        flat
                    }
                }
            }
        } else {
            (0..runs.len()).collect()
        };

        let per_glyph_mode = {
            let mut total = 0usize;
            let mut single = 0usize;
            for r in &runs {
                if r.text.is_empty() {
                    continue;
                }
                total += 1;
                if r.text.chars().count() <= 1 {
                    single += 1;
                }
            }
            total >= 8 && single * 10 >= total * 7
        };

        let mut prev_x: Option<f32> = None;
        let mut prev_y: Option<f32> = None;
        let mut prev_font_size: Option<f32> = None;
        let mut prev_w: Option<f32> = None;
        let mut prev_char_count: usize = 0;
        let mut first_on_page = true;

        for &order_idx in &run_order {
            let run = &runs[order_idx];
            diag.chars += run.text.chars().count() as u32;
            diag.fallback_chars += run.cmap_fallback_chars;
            match run.decoding_path {
                pdf_fast_core::DecodingPath::ToUnicodeCMap => diag.via_to_unicode += 1,
                pdf_fast_core::DecodingPath::Type0Utf16Be => diag.via_type0 += 1,
                pdf_fast_core::DecodingPath::EncodingDifferences => diag.via_differences += 1,
                pdf_fast_core::DecodingPath::EncodingNamed => diag.via_named += 1,
                pdf_fast_core::DecodingPath::StandardImplicit => diag.via_standard += 1,
                pdf_fast_core::DecodingPath::RawByteFallback => diag.via_raw_byte += 1,
            }
            if run.text.is_empty() {
                continue;
            }
            if !first_on_page {
                let separator = run_separator(
                    prev_x,
                    prev_y,
                    prev_font_size,
                    prev_w,
                    prev_char_count,
                    run.x,
                    run.y,
                    run.font_size,
                    per_glyph_mode,
                );
                let last = buf.chars().last();
                match separator {
                    Separator::Newline => {
                        let prev_was_hyphen = buf.ends_with('-');
                        let next_is_lower =
                            run.text.chars().next().is_some_and(|c| c.is_lowercase());
                        if prev_was_hyphen && next_is_lower {
                            let stem: String = buf
                                .chars()
                                .rev()
                                .skip(1)
                                .take_while(|c| c.is_alphabetic())
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect::<String>()
                                .to_lowercase();
                            if !is_compound_prefix(&stem) {
                                buf.pop();
                            }
                        } else {
                            buf.push('\n');
                        }
                    }
                    Separator::Space => {
                        let next_is_cjk = run.text.chars().next().is_some_and(is_cjk_ideograph);
                        let last_is_cjk = last.is_some_and(is_cjk_ideograph);
                        if last_is_cjk && next_is_cjk {
                        } else if !matches!(last, Some(c) if c.is_whitespace()) {
                            buf.push(' ');
                        }
                    }
                    Separator::None => {}
                }
            }
            buf.push_str(&run.text);
            first_on_page = false;
            prev_y = run.y;
            prev_font_size = run.font_size;
            prev_x = run.x;
            prev_w = run.w;
            prev_char_count = run.text.chars().count();
        }
        if idx + 1 < pages.len() {
            buf.push('\u{000C}');
        }
    }
    diag.pages = pages.len() as u32;
    (buf, diag)
}

/// Streaming PDF scanner. Mirrors `pdf-fast-py` `PdfScanner`.
#[napi]
pub struct PdfScanner {
    path: String,
}

#[napi]
impl PdfScanner {
    #[napi(constructor, catch_unwind)]
    pub fn new(path: String) -> Self {
        Self { path }
    }

    #[napi(catch_unwind)]
    pub fn scan_json(&self) -> Result<String> {
        let doc = open_document(&self.path)?;
        serde_json::to_string(&doc).map_err(to_err)
    }

    #[napi(catch_unwind)]
    pub fn semantic_runs_json(&self) -> Result<String> {
        let doc = open_document(&self.path)?;
        serde_json::to_string(&doc.semantic_text_runs()).map_err(to_err)
    }

    /// Page-by-page paragraphs via `pdf_fast_core::paragraph::merge_paragraphs`.
    #[napi(catch_unwind)]
    pub fn paragraphs_json(&self) -> Result<String> {
        let doc = open_document(&self.path)?;
        let runs = doc.semantic_text_runs();
        let paragraphs = pdf_fast_core::paragraph::merge_paragraphs(&runs);
        serde_json::to_string(&paragraphs).map_err(to_err)
    }

    /// Whole-document semantic text runs as Arrow IPC bytes.
    #[napi(catch_unwind)]
    pub fn semantic_runs_arrow_ipc(&self) -> Result<Buffer> {
        let doc = open_document(&self.path)?;
        let batch = doc
            .semantic_text_runs_record_batch_parallel()
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    /// Whole-document page multimodal batch as Arrow IPC bytes.
    #[napi(catch_unwind)]
    pub fn page_multimodal_arrow_ipc(&self) -> Result<Buffer> {
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_multimodal_record_batch_parallel()
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    /// Single-page multimodal batch as Arrow IPC bytes.
    /// Node replacement for Python's `page_multimodal_c_data`.
    #[napi(catch_unwind)]
    pub fn page_multimodal_ipc(&self, page_index: u32) -> Result<Buffer> {
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_multimodal_record_batch(page_index as usize)
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    #[napi(catch_unwind)]
    pub fn page_bitmap_components_json(&self, page_index: u32) -> Result<String> {
        let doc = open_document(&self.path)?;
        let components = doc
            .page_bitmap_components(
                page_index as usize,
                pdf_fast_core::BitmapComponentOptions::default(),
            )
            .map_err(to_err)?;
        serde_json::to_string(&components).map_err(to_err)
    }

    /// Page table layout batch as Arrow IPC bytes. `epsilon` defaults to 2.0.
    #[napi(catch_unwind)]
    pub fn page_table_layout_arrow_ipc(
        &self,
        page_index: u32,
        epsilon: Option<f64>,
    ) -> Result<Buffer> {
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_table_layout_record_batch(page_index as usize, epsilon.unwrap_or(2.0) as f32)
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    /// Page table layout (from OCR runs) as Arrow IPC bytes.
    /// `epsilon` defaults to 2.0, `dpi` to 300.0.
    #[napi(catch_unwind)]
    pub fn page_table_layout_from_ocr_arrow_ipc(
        &self,
        page_index: u32,
        ocr_runs_json: String,
        epsilon: Option<f64>,
        dpi: Option<f64>,
    ) -> Result<Buffer> {
        let ocr_runs = parse_ocr_runs(&ocr_runs_json)?;
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_table_layout_record_batch_from_ocr(
                page_index as usize,
                epsilon.unwrap_or(2.0) as f32,
                dpi.unwrap_or(300.0) as f32,
                &ocr_runs,
            )
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    /// Page table batch as Arrow IPC bytes. `epsilon` defaults to 2.0.
    #[napi(catch_unwind)]
    pub fn page_table_arrow_ipc(&self, page_index: u32, epsilon: Option<f64>) -> Result<Buffer> {
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_table_record_batch(page_index as usize, epsilon.unwrap_or(2.0) as f32)
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    /// Page table (from OCR runs) as Arrow IPC bytes.
    /// `epsilon` defaults to 2.0, `dpi` to 300.0.
    #[napi(catch_unwind)]
    pub fn page_table_from_ocr_arrow_ipc(
        &self,
        page_index: u32,
        ocr_runs_json: String,
        epsilon: Option<f64>,
        dpi: Option<f64>,
    ) -> Result<Buffer> {
        let ocr_runs = parse_ocr_runs(&ocr_runs_json)?;
        let doc = open_document(&self.path)?;
        let batch = doc
            .page_table_record_batch_from_ocr(
                page_index as usize,
                epsilon.unwrap_or(2.0) as f32,
                dpi.unwrap_or(300.0) as f32,
                &ocr_runs,
            )
            .map_err(to_err)?;
        Ok(record_batch_to_ipc_bytes(&batch)?.into())
    }

    #[napi(catch_unwind)]
    pub fn page_count(&self) -> Result<u32> {
        let doc = open_document(&self.path)?;
        Ok(doc.pages().len() as u32)
    }

    /// Per-page effective geometry (CropBox preferred, else MediaBox) as JSON.
    #[napi(catch_unwind)]
    pub fn page_dimensions_json(&self) -> Result<String> {
        let doc = open_document(&self.path)?;
        let mut out: Vec<serde_json::Value> = Vec::new();
        for page in doc.pages() {
            let (x0, y0, width, height) = match page.page_box() {
                Some([a, b, c, d]) => {
                    let w = (c - a).abs();
                    let h = (d - b).abs();
                    (a.min(c), b.min(d), w, h)
                }
                None => (0.0f32, 0.0f32, 0.0f32, 0.0f32),
            };
            out.push(json!({
                "page_index": page.page_index,
                "x0": x0,
                "y0": y0,
                "width": width,
                "height": height,
                "rotate": page.rotate,
            }));
        }
        serde_json::to_string(&out).map_err(to_err)
    }

    /// Rasterize a page to PNG bytes. `dpi` defaults to 180.0. Returns null if unavailable.
    #[napi(catch_unwind)]
    pub fn page_rasterized_png(&self, page_index: u32, dpi: Option<f64>) -> Result<Option<Buffer>> {
        let doc = open_document(&self.path)?;
        let png_opt = doc
            .page_rasterized_png(page_index as usize, dpi.unwrap_or(180.0) as f32)
            .map_err(to_err)?;
        Ok(png_opt.map(Buffer::from))
    }

    /// Debug helper: human-readable reading-order block dump for a page.
    #[napi(catch_unwind)]
    pub fn debug_reading_order_blocks(&self, page_index: u32) -> Result<String> {
        let doc = open_document(&self.path)?;
        let page = doc
            .pages()
            .into_iter()
            .find(|p| p.page_index == page_index as usize)
            .ok_or_else(|| Error::from_reason("page not found"))?;
        let runs = doc.semantic_text_runs_for_page(&page);
        let blocks = doc
            .reading_order_blocks(
                &page,
                pdf_fast_core::tessellation::TessellationOptions::default(),
            )
            .map_err(to_err)?;
        let mut out = String::with_capacity(4096);
        let _ = writeln!(
            &mut out,
            "page_box={:?}  runs={}  blocks={}",
            page.page_box(),
            runs.len(),
            blocks.len()
        );
        for (i, b) in blocks.iter().enumerate() {
            let preview: String = b
                .run_indices
                .iter()
                .filter_map(|&ri| runs.get(ri))
                .take(4)
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            let _ = writeln!(
                &mut out,
                "  [{:3}] bbox=({:.0},{:.0})-({:.0},{:.0}) runs={:3}  preview={:?}",
                i,
                b.x_min,
                b.y_min,
                b.x_max,
                b.y_max,
                b.run_indices.len(),
                utf8_prefix_at_most_bytes(&preview, 80)
            );
        }
        Ok(out)
    }

    #[napi(catch_unwind)]
    pub fn reading_order_blocks_json(&self, page_index: u32) -> Result<String> {
        let doc = open_document(&self.path)?;
        let page = doc
            .pages()
            .into_iter()
            .find(|p| p.page_index == page_index as usize)
            .ok_or_else(|| Error::from_reason("page not found"))?;
        let blocks = doc
            .reading_order_blocks(
                &page,
                pdf_fast_core::tessellation::TessellationOptions::default(),
            )
            .map_err(to_err)?;
        let payload: Vec<serde_json::Value> = blocks
            .into_iter()
            .map(|b| {
                json!({
                    "bbox": [b.x_min, b.y_min, b.x_max, b.y_max],
                    "run_indices": b.run_indices,
                })
            })
            .collect();
        serde_json::to_string(&payload).map_err(to_err)
    }

    /// Assemble the full document text. `readingOrder` defaults to false.
    #[napi(catch_unwind)]
    pub fn full_text(&self, reading_order: Option<bool>) -> Result<FullTextResult> {
        let doc = open_document(&self.path)?;
        let (text, diagnostics) = compute_full_text(&doc, reading_order.unwrap_or(false));
        Ok(FullTextResult { text, diagnostics })
    }
}

// ---------------------------------------------------------------------------
// Standalone convenience functions mirroring the PdfScanner methods.
// ---------------------------------------------------------------------------

#[napi(catch_unwind)]
pub fn scan_pdf_json(path: String) -> Result<String> {
    PdfScanner::new(path).scan_json()
}

#[napi(catch_unwind)]
pub fn scan_pdf_semantic_runs_json(path: String) -> Result<String> {
    PdfScanner::new(path).semantic_runs_json()
}

#[napi(catch_unwind)]
pub fn scan_pdf_paragraphs_json(path: String) -> Result<String> {
    PdfScanner::new(path).paragraphs_json()
}

#[napi(catch_unwind)]
pub fn scan_pdf_semantic_runs_arrow_ipc(path: String) -> Result<Buffer> {
    PdfScanner::new(path).semantic_runs_arrow_ipc()
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_multimodal_arrow_ipc(path: String) -> Result<Buffer> {
    PdfScanner::new(path).page_multimodal_arrow_ipc()
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_multimodal_ipc(path: String, page_index: u32) -> Result<Buffer> {
    PdfScanner::new(path).page_multimodal_ipc(page_index)
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_bitmap_components_json(path: String, page_index: u32) -> Result<String> {
    PdfScanner::new(path).page_bitmap_components_json(page_index)
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_table_layout_arrow_ipc(
    path: String,
    page_index: u32,
    epsilon: Option<f64>,
) -> Result<Buffer> {
    PdfScanner::new(path).page_table_layout_arrow_ipc(page_index, epsilon)
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_table_layout_from_ocr_arrow_ipc(
    path: String,
    page_index: u32,
    ocr_runs_json: String,
    epsilon: Option<f64>,
    dpi: Option<f64>,
) -> Result<Buffer> {
    PdfScanner::new(path).page_table_layout_from_ocr_arrow_ipc(
        page_index,
        ocr_runs_json,
        epsilon,
        dpi,
    )
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_table_arrow_ipc(
    path: String,
    page_index: u32,
    epsilon: Option<f64>,
) -> Result<Buffer> {
    PdfScanner::new(path).page_table_arrow_ipc(page_index, epsilon)
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_table_from_ocr_arrow_ipc(
    path: String,
    page_index: u32,
    ocr_runs_json: String,
    epsilon: Option<f64>,
    dpi: Option<f64>,
) -> Result<Buffer> {
    PdfScanner::new(path).page_table_from_ocr_arrow_ipc(page_index, ocr_runs_json, epsilon, dpi)
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_count(path: String) -> Result<u32> {
    PdfScanner::new(path).page_count()
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_dimensions_json(path: String) -> Result<String> {
    PdfScanner::new(path).page_dimensions_json()
}

#[napi(catch_unwind)]
pub fn scan_pdf_page_rasterized_png(
    path: String,
    page_index: u32,
    dpi: Option<f64>,
) -> Result<Option<Buffer>> {
    PdfScanner::new(path).page_rasterized_png(page_index, dpi)
}

#[napi(catch_unwind)]
pub fn scan_pdf_full_text(path: String, reading_order: Option<bool>) -> Result<FullTextResult> {
    PdfScanner::new(path).full_text(reading_order)
}

#[cfg(test)]
mod tests {
    //! Arrow IPC round-trip test for the exact serialization path the bindings
    //! use (`record_batch_to_ipc_bytes`), decoded back with the arrow crate's
    //! `StreamReader` and validated column-by-column.
    use super::record_batch_to_ipc_bytes;
    use arrow::array::{Array, Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::reader::StreamReader;
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    #[test]
    fn ipc_bytes_round_trip_preserves_schema_and_values() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("Hello"), None, Some("world")])),
            ],
        )
        .expect("record batch");

        let bytes = record_batch_to_ipc_bytes(&batch).expect("ipc bytes");
        // Encapsulated Arrow IPC stream: continuation marker + metadata length.
        assert!(bytes.len() > 8, "ipc stream too short: {}", bytes.len());
        assert_eq!(
            &bytes[0..4],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            "missing continuation marker"
        );

        let reader =
            StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("stream reader");
        let decoded: Vec<RecordBatch> = reader.map(|b| b.expect("batch")).collect();
        assert_eq!(decoded.len(), 1, "expected exactly one batch");
        let got = &decoded[0];

        assert_eq!(got.num_rows(), 3);
        let decoded_schema = got.schema();
        let field_names: Vec<&str> = decoded_schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        assert_eq!(field_names, vec!["id", "text"]);

        let ids = got
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("Int64Array");
        assert_eq!((ids.value(0), ids.value(1), ids.value(2)), (1, 2, 3));

        let text = got
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("StringArray");
        assert_eq!(text.value(0), "Hello");
        assert!(text.is_null(1), "row 1 text should be null");
        assert_eq!(text.value(2), "world");
    }

    /// Offset-correct minimal 1-page PDF with a `(Hello) Tj` text op.
    fn minimal_pdf_bytes() -> Vec<u8> {
        let stream = b"BT /F1 24 Tf 72 700 Td (Hello) Tj ET".to_vec();
        let objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            {
                let mut v = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
                v.extend_from_slice(&stream);
                v.extend_from_slice(b"\nendstream");
                v
            },
            b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize];
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref_pos = out.len();
        let n = objs.len() + 1;
        out.extend_from_slice(format!("xref\n0 {n}\n0000000000 65535 f \n").as_bytes());
        for off in &offsets[1..] {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    #[test]
    fn semantic_runs_arrow_ipc_decodes_with_text_column() {
        // Real parser output: generate a PDF, parse it, build the semantic-runs
        // RecordBatch through the exact binding path, then decode it back with
        // arrow's StreamReader and assert the schema + extracted text.
        let path = std::env::temp_dir().join(format!(
            "pdf-fast-node-runs-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::write(&path, minimal_pdf_bytes()).expect("write pdf");
        let result = (|| {
            let doc = super::open_document(path.to_str().expect("utf8 path"))?;
            let batch = doc
                .semantic_text_runs_record_batch_parallel()
                .map_err(super::to_err)?;
            super::record_batch_to_ipc_bytes(&batch)
        })();
        std::fs::remove_file(&path).ok();
        let bytes = result.expect("semantic runs ipc bytes");

        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
        let decoded_schema = reader.schema();
        let names: Vec<&str> = decoded_schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        assert!(
            names.contains(&"text"),
            "semantic-runs schema should have a `text` column, got {names:?}"
        );
        let text_idx = decoded_schema.index_of("text").expect("text column");

        let mut saw_hello = false;
        let mut total_rows = 0usize;
        for maybe_batch in reader {
            let batch = maybe_batch.expect("batch");
            total_rows += batch.num_rows();
            if let Some(col) = batch
                .column(text_idx)
                .as_any()
                .downcast_ref::<StringArray>()
            {
                for i in 0..col.len() {
                    if !col.is_null(i) && col.value(i).contains("Hello") {
                        saw_hello = true;
                    }
                }
            }
        }
        assert!(total_rows >= 1, "expected at least one semantic text run");
        assert!(saw_hello, "expected a semantic run containing 'Hello'");
    }
}
