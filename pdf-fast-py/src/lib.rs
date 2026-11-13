use arrow::array::{Array, ArrayData, StructArray};
use arrow::compute::concat_batches;
use arrow::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
use arrow::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow::record_batch::RecordBatch;
use pdf_fast_core::{OcrTextRun, ParseOptions, PdfDocument};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCapsule, PyList, PyString};
use std::ffi::CStr;
use std::fmt::Write;
use std::io::Cursor;
use std::sync::Arc;

/// OCR intake after Python → Rust conversion.
///
/// Arrow sources stay as [`RecordBatch`] so core can walk columns without
/// allocating `Vec<OcrTextRun>`. JSON / dict / list paths still materialize runs.
enum OcrInput {
    Batch(RecordBatch),
    Runs(Vec<OcrTextRun>),
}

#[pyclass(skip_from_py_object)]
struct PageDimension {
    #[pyo3(get)]
    page_index: usize,
    #[pyo3(get)]
    x0: f32,
    #[pyo3(get)]
    y0: f32,
    #[pyo3(get)]
    width: f32,
    #[pyo3(get)]
    height: f32,
    #[pyo3(get)]
    rotate: u32,
}

#[pymethods]
impl PageDimension {
    fn __repr__(&self) -> String {
        format!(
            "PageDimension(page_index={}, x0={}, y0={}, width={}, height={}, rotate={})",
            self.page_index, self.x0, self.y0, self.width, self.height, self.rotate
        )
    }
}

#[derive(Clone, Copy)]
struct PageDimensionData {
    page_index: usize,
    x0: f32,
    y0: f32,
    width: f32,
    height: f32,
    rotate: u32,
}

impl PageDimensionData {
    fn to_py(self) -> PageDimension {
        PageDimension {
            page_index: self.page_index,
            x0: self.x0,
            y0: self.y0,
            width: self.width,
            height: self.height,
            rotate: self.rotate,
        }
    }
}

#[pyclass]
struct PdfScanner {
    #[pyo3(get)]
    path: String,
    doc: Arc<PdfDocument>,
    #[pyo3(get)]
    page_count: usize,
    page_dimensions: Arc<Vec<PageDimensionData>>,
}

#[pymethods]
impl PdfScanner {
    #[new]
    fn new(path: String) -> PyResult<Self> {
        // Opening parses the whole file. Every caller holds the GIL, so it is
        // released here for the parse and taken back before returning.
        let (doc, page_dimensions) = Python::attach(|py| {
            py.detach(|| -> PyResult<_> {
                let doc = open_document(&path)?;
                let page_dimensions = collect_page_dimensions(&doc);
                Ok((doc, page_dimensions))
            })
        })?;
        let page_dimensions = Arc::new(page_dimensions);
        let page_count = page_dimensions.len();
        Ok(Self {
            path,
            doc: Arc::new(doc),
            page_count,
            page_dimensions,
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "PdfScanner(path={:?}, page_count={})",
            self.path, self.page_count
        )
    }

    fn __len__(&self) -> usize {
        self.page_count
    }

    fn scan_json(&self, py: Python<'_>) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || serde_json::to_string(&doc).map_err(to_py_err))
    }

    fn semantic_runs_json(&self, py: Python<'_>) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || serde_json::to_string(&doc.semantic_text_runs()).map_err(to_py_err))
    }

    /// Page-by-page paragraphs via `pdf_fast_core::paragraph::merge_paragraphs`.
    fn paragraphs_json(&self, py: Python<'_>) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let runs = doc.semantic_text_runs();
            let paragraphs = pdf_fast_core::paragraph::merge_paragraphs(&runs);
            serde_json::to_string(&paragraphs).map_err(to_py_err)
        })
    }

    fn semantic_runs_arrow_ipc<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch = doc
                .semantic_text_runs_record_batch_parallel()
                .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    fn page_multimodal_arrow_ipc<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch = doc
                .page_multimodal_record_batch_parallel()
                .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    fn page_multimodal_c_data(
        &self,
        py: Python<'_>,
        page_index: usize,
    ) -> PyResult<(usize, usize)> {
        // Deprecated: prefer [`Self::page_multimodal_arrow_c_array`] (PyCapsule).
        // Raw FFI pointers require a matching `release_arrow_c_data` call when
        // not imported into PyArrow.
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let batch = doc
                .page_multimodal_record_batch(page_index)
                .map_err(to_py_err)?;
            record_batch_to_c_data(batch)
        })
    }

    /// Zero-copy page multimodal batch as Arrow PyCapsules (`arrow_schema`, `arrow_array`).
    ///
    /// Prefer this over [`Self::page_multimodal_c_data`] — capsules own the
    /// buffers and import via ``RecordBatch._import_from_c_capsule`` without a
    /// manual free path.
    fn page_multimodal_arrow_c_array<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let doc = Arc::clone(&self.doc);
        let batch = py.detach(move || {
            doc.page_multimodal_record_batch(page_index)
                .map_err(to_py_err)
        })?;
        record_batch_to_arrow_c_array_capsules(py, batch)
    }

    fn page_bitmap_components_json(&self, py: Python<'_>, page_index: usize) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let components = doc
                .page_bitmap_components(
                    page_index,
                    pdf_fast_core::BitmapComponentOptions::default(),
                )
                .map_err(to_py_err)?;
            serde_json::to_string(&components).map_err(to_py_err)
        })
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_layout_arrow_ipc<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch = doc
                .page_table_layout_record_batch(page_index, epsilon)
                .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_layout_c_data(
        &self,
        py: Python<'_>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<(usize, usize)> {
        // Deprecated: prefer [`Self::page_table_layout_arrow_c_array`].
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let batch = doc
                .page_table_layout_record_batch(page_index, epsilon)
                .map_err(to_py_err)?;
            record_batch_to_c_data(batch)
        })
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_layout_arrow_c_array<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let doc = Arc::clone(&self.doc);
        let batch = py.detach(move || {
            doc.page_table_layout_record_batch(page_index, epsilon)
                .map_err(to_py_err)
        })?;
        record_batch_to_arrow_c_array_capsules(py, batch)
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_layout_from_ocr_arrow_ipc<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        // Convert on the calling thread so we never move Python objects across detach.
        let ocr = ocr_input_from_py(ocr_runs)?;
        self.table_layout_ipc_from_ocr(py, page_index, ocr, epsilon, dpi)
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_layout_from_ocr_c_data(
        &self,
        py: Python<'_>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<(usize, usize)> {
        // Deprecated: prefer [`Self::page_table_layout_from_ocr_arrow_c_array`].
        let doc = Arc::clone(&self.doc);
        let ocr = ocr_input_from_py(ocr_runs)?;
        py.detach(move || {
            let batch =
                page_table_layout_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr)
                    .map_err(to_py_err)?;
            record_batch_to_c_data(batch)
        })
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_layout_from_ocr_arrow_c_array<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let doc = Arc::clone(&self.doc);
        let ocr = ocr_input_from_py(ocr_runs)?;
        let batch = py.detach(move || {
            page_table_layout_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr)
                .map_err(to_py_err)
        })?;
        record_batch_to_arrow_c_array_capsules(py, batch)
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_arrow_ipc<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch = doc
                .page_table_record_batch(page_index, epsilon)
                .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_c_data(
        &self,
        py: Python<'_>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<(usize, usize)> {
        // Deprecated: prefer [`Self::page_table_arrow_c_array`].
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let batch = doc
                .page_table_record_batch(page_index, epsilon)
                .map_err(to_py_err)?;
            record_batch_to_c_data(batch)
        })
    }

    #[pyo3(signature = (page_index, epsilon=2.0))]
    fn page_table_arrow_c_array<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        epsilon: f32,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let doc = Arc::clone(&self.doc);
        let batch = py.detach(move || {
            doc.page_table_record_batch(page_index, epsilon)
                .map_err(to_py_err)
        })?;
        record_batch_to_arrow_c_array_capsules(py, batch)
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_from_ocr_arrow_ipc<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let ocr = ocr_input_from_py(ocr_runs)?;
        self.table_ipc_from_ocr(py, page_index, ocr, epsilon, dpi)
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_from_ocr_c_data(
        &self,
        py: Python<'_>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<(usize, usize)> {
        // Deprecated: prefer [`Self::page_table_from_ocr_arrow_c_array`].
        let doc = Arc::clone(&self.doc);
        let ocr = ocr_input_from_py(ocr_runs)?;
        py.detach(move || {
            let batch = page_table_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr)
                .map_err(to_py_err)?;
            record_batch_to_c_data(batch)
        })
    }

    #[pyo3(signature = (page_index, ocr_runs, epsilon=2.0, dpi=300.0))]
    fn page_table_from_ocr_arrow_c_array<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr_runs: &Bound<'_, PyAny>,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let doc = Arc::clone(&self.doc);
        let ocr = ocr_input_from_py(ocr_runs)?;
        let batch = py.detach(move || {
            page_table_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr).map_err(to_py_err)
        })?;
        record_batch_to_arrow_c_array_capsules(py, batch)
    }

    /// Per-page effective geometry (CropBox preferred, else MediaBox).
    ///
    /// Cached at open. Widths/heights are in PDF user-space points — the
    /// primitive callers use to normalize text-run boxes to page-relative
    /// `[0, 1]` space.
    fn page_dimensions(&self) -> Vec<PageDimension> {
        self.page_dimensions
            .iter()
            .copied()
            .map(PageDimensionData::to_py)
            .collect()
    }

    /// JSON form of [`Self::page_dimensions`] for callers that still want a string.
    /// Text-line boxes in PAGE units from vector glyph geometry — the
    /// detector-free path for OCR on outlined-glyph PDFs. Returns
    /// `[[x0, y0, x1, y1], ...]`, top-down then left-to-right. Callers scale by
    /// `dpi / 72` to index the rasterised page and feed crops straight to a
    /// text recogniser, skipping neural text detection entirely.
    /// `split_columns=True` yields one box per table cell (cell-level
    /// provenance); `False` yields one box per visual line — fewer, wider
    /// crops, which is what a text recogniser wants (its cost scales with the
    /// number of crops).
    #[pyo3(signature = (page_index, line_tolerance=2.0, split_columns=true))]
    fn page_text_line_boxes(
        &self,
        py: Python<'_>,
        page_index: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> PyResult<Vec<[f32; 4]>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            doc.page_text_line_boxes_with(page_index, line_tolerance, split_columns)
                .map_err(to_py_err)
        })
    }

    /// Per-glyph origins for outlined-glyph pages: `[(page_x, page_y,
    /// shape_hash), ...]` in PAGE units, reading order not guaranteed —
    /// callers band by baseline. `shape_hash` is the stable hash of the
    /// normalized glyph contours (0 = no geometry): two glyphs with the same
    /// hash are the SAME shape, which is what makes self-labelled glyph
    /// decoding possible (recognise a shape once, read it everywhere).
    fn page_vector_glyph_origins(
        &self,
        py: Python<'_>,
        page_index: usize,
    ) -> PyResult<Vec<(f32, f32, f32)>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let runs = doc.page_vector_text_runs(page_index).map_err(to_py_err)?;
            let mut out = Vec::new();
            for run in runs {
                for glyph in &run.glyphs {
                    out.push((run.page_x + glyph.x_offset, run.page_y, glyph.y_offset));
                }
            }
            Ok(out)
        })
    }

    /// Document-wide glyph SHAPE clusters with tolerance `eps` (normalized
    /// units). Returns one list per page of `(page_x, page_y, cluster_id)`.
    /// Unlike `shape_hash` (exact grid hash, brittle under float noise),
    /// cluster ids are stable identity: same shape → same id across the whole
    /// document, which is the substrate for self-labelled glyph decoding.
    #[pyo3(signature = (eps=0.02))]
    fn vector_glyph_clusters(
        &self,
        py: Python<'_>,
        eps: f32,
    ) -> PyResult<Vec<Vec<(f32, f32, u32)>>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || doc.vector_glyph_clusters_checked(eps).map_err(to_py_err))
    }

    /// Visual text lines with per-glyph cluster membership, per page:
    /// `[[(box4, [(x, cluster_id), ...]), ...], ...]`. Box and membership come
    /// from the same banding pass, so the recognised crop and its glyph
    /// sequence always agree about which glyphs are on the line — the
    /// substrate for self-labelled glyph decoding.
    #[pyo3(signature = (eps=0.03, line_tolerance=2.0, split_columns=false))]
    fn vector_glyph_cluster_lines(
        &self,
        py: Python<'_>,
        eps: f32,
        line_tolerance: f32,
        split_columns: bool,
    ) -> PyResult<Vec<Vec<GlyphClusterLine>>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            doc.vector_glyph_cluster_lines_checked(eps, line_tolerance, split_columns)
                .map_err(to_py_err)
        })
    }

    /// Smooth glyph shape descriptors + line banding from one enumeration.
    /// Per page: `(glyphs, lines)` where `glyphs = [(page_x, page_y, size,
    /// [desc…2k floats]), …]` and `lines = [(box4, [glyph_index, …]), …]`.
    /// Identity lives in the descriptor METRIC space (no eps cliff): match by
    /// nearest prototype with a margin, reject to the neural recognizer.
    #[pyo3(signature = (k=32, line_tolerance=2.0, split_columns=false))]
    #[allow(clippy::type_complexity)]
    fn vector_glyph_descriptor_lines(
        &self,
        py: Python<'_>,
        k: usize,
        line_tolerance: f32,
        split_columns: bool,
    ) -> PyResult<Vec<(Vec<(f32, f32, f32, Vec<f32>)>, Vec<([f32; 4], Vec<u32>)>)>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            doc.vector_glyph_descriptor_lines_checked(k, line_tolerance, split_columns)
                .map_err(to_py_err)
        })
    }

    /// TRUE-SCAN glyph geometry: same output shape as
    /// `vector_glyph_descriptor_lines` but extracted from the page RASTER
    /// (binarize → connected components → Moore contour → descriptor), so
    /// scanned documents feed the identical decoding stack.
    #[pyo3(signature = (page_index, dpi=200.0, k=32, threshold=140, line_tolerance=2.0, split_columns=false, interpreted=false))]
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn raster_glyph_descriptor_lines(
        &self,
        py: Python<'_>,
        page_index: usize,
        dpi: f32,
        k: usize,
        threshold: u8,
        line_tolerance: f32,
        split_columns: bool,
        interpreted: bool,
    ) -> PyResult<Option<(Vec<(f32, f32, f32, Vec<f32>)>, Vec<([f32; 4], Vec<u32>)>)>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            doc.raster_glyph_descriptor_lines_with(
                page_index,
                dpi,
                k,
                threshold,
                line_tolerance,
                split_columns,
                interpreted,
            )
            .map_err(to_py_err)
        })
    }

    /// Full-interpreter render (fonts included) — see core docs: the
    /// vectors-only fast path returns rule-only renders for born-digital
    /// font-text pages.
    fn page_rasterized_png_interpreted(
        &self,
        py: Python<'_>,
        page_index: usize,
        dpi: f32,
    ) -> PyResult<Option<Vec<u8>>> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            doc.page_rasterized_png_interpreted_checked(page_index, dpi)
                .map_err(to_py_err)
        })
    }

    fn page_dimensions_json(&self) -> PyResult<String> {
        let payload: Vec<serde_json::Value> = self
            .page_dimensions
            .iter()
            .map(|d| {
                serde_json::json!({
                    "page_index": d.page_index,
                    "x0": d.x0,
                    "y0": d.y0,
                    "width": d.width,
                    "height": d.height,
                    "rotate": d.rotate,
                })
            })
            .collect();
        serde_json::to_string(&payload).map_err(to_py_err)
    }

    fn page_rasterized_png<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        dpi: Option<f32>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let doc = Arc::clone(&self.doc);
        let dpi_val = dpi.unwrap_or(180.0);
        let png_opt = py.detach(move || {
            doc.page_rasterized_png(page_index, dpi_val)
                .map_err(to_py_err)
        })?;

        match png_opt {
            Some(png) => Ok(Some(PyBytes::new(py, &png))),
            None => Ok(None),
        }
    }

    /// Debug helper: return the reading-order blocks for a page so we can inspect tessellation.
    fn debug_reading_order_blocks(&self, py: Python<'_>, page_index: usize) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let page = doc
                .pages()
                .into_iter()
                .find(|p| p.page_index == page_index)
                .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("page not found"))?;
            let runs = doc.semantic_text_runs_for_page(&page);
            let blocks = doc
                .reading_order_blocks(
                    &page,
                    pdf_fast_core::tessellation::TessellationOptions::default(),
                )
                .map_err(to_py_err)?;

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

                // Zero-allocation formatting append
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
        })
    }

    fn reading_order_blocks_json(&self, py: Python<'_>, page_index: usize) -> PyResult<String> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || {
            let page = doc
                .pages()
                .into_iter()
                .find(|p| p.page_index == page_index)
                .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("page not found"))?;
            let blocks = doc
                .reading_order_blocks(
                    &page,
                    pdf_fast_core::tessellation::TessellationOptions::default(),
                )
                .map_err(to_py_err)?;
            let payload: Vec<serde_json::Value> = blocks
                .into_iter()
                .map(|b| {
                    serde_json::json!({
                        "bbox": [b.x_min, b.y_min, b.x_max, b.y_max],
                        "run_indices": b.run_indices,
                    })
                })
                .collect();
            serde_json::to_string(&payload).map_err(to_py_err)
        })
    }

    #[pyo3(signature = (reading_order=false))]
    fn full_text(
        &self,
        py: Python<'_>,
        reading_order: bool,
    ) -> PyResult<(String, FullTextDiagnostics)> {
        let doc = Arc::clone(&self.doc);
        py.detach(move || compute_full_text(&doc, reading_order))
    }
}

/// Page count above which full_text fans out across rayon threads. Below it,
/// the sequential path wins: thread spin-up + the final join dominate a
/// handful of pages, and most documents are small. An 835-page contract is
/// where per-page decode latency (~6-7 ms/page) makes parallelism pay.
/// One visual text line: its `[x0, y0, x1, y1]` box plus the `(x, cluster_id)`
/// membership of every glyph on it.
///
/// Named so the per-page nesting (`Vec<Vec<GlyphClusterLine>>`) stays readable.
type GlyphClusterLine = ([f32; 4], Vec<(f32, u32)>);

const FULL_TEXT_PARALLEL_MIN_PAGES: usize = 32;

/// Extracted core logic for full_text.
///
/// Each page's text is INDEPENDENT — `first_on_page` resets the run-separator
/// state at every page boundary and pages join with a form-feed — so the work
/// is embarrassingly parallel. `PdfDocument` is owned data + an mmap with no
/// interior mutability (`Send + Sync`), and the reading-order engine
/// (`RoutedEngine::shared()`) is a shared immutable singleton, so `&doc` and
/// the per-page routine cross threads safely. Output is byte-for-byte
/// identical to the sequential path: same pages, same order, same separators.
fn compute_full_text(
    doc: &PdfDocument,
    reading_order: bool,
) -> PyResult<(String, FullTextDiagnostics)> {
    // Validated once, before any page work: `emit_page_text` runs inside a
    // rayon reduce, where threading a `Result` through the per-page closure to
    // report a build-configuration problem would cost more than it explains.
    ensure_reading_order_available(reading_order)?;

    let pages = doc.pages();
    let n = pages.len();

    if n >= FULL_TEXT_PARALLEL_MIN_PAGES {
        use rayon::prelude::*;
        // Per-page (text, diag), computed in parallel, reduced in page order.
        let (buf, diag) = pages
            .par_iter()
            .enumerate()
            .map(|(idx, page)| {
                let mut page_buf = String::with_capacity(2048);
                let mut page_diag = FullTextDiagnostics::default();
                emit_page_text(doc, page, reading_order, &mut page_buf, &mut page_diag);
                page_diag.pages = 1;
                // Form-feed BETWEEN pages, matching the sequential separator.
                if idx + 1 < n {
                    page_buf.push('\u{000C}');
                }
                (page_buf, page_diag)
            })
            .reduce(
                || (String::new(), FullTextDiagnostics::default()),
                |mut acc, (page_buf, page_diag)| {
                    acc.0.push_str(&page_buf);
                    acc.1.merge(&page_diag);
                    acc
                },
            );
        return Ok((buf, diag));
    }

    // Heuristic Preallocation: Prevents dozens of OS-level memory resizes when scanning dense papers
    let mut buf = String::with_capacity(n * 2048);
    let mut diag = FullTextDiagnostics::default();

    for (idx, page) in pages.iter().enumerate() {
        emit_page_text(doc, page, reading_order, &mut buf, &mut diag);
        diag.pages += 1;
        if idx + 1 < n {
            buf.push('\u{000C}');
        }
    }
    Ok((buf, diag))
}

/// Reject `reading_order = true` when this build has no reading-order engine.
///
/// The alternative — quietly returning stream order — would hand the caller a
/// different ordering than it asked for, in text that looks entirely plausible.
/// A build-configuration problem must not be reported as a successful result.
#[cfg(not(feature = "reading-order"))]
fn ensure_reading_order_available(reading_order: bool) -> PyResult<()> {
    if reading_order {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "reading_order requires the `reading-order` feature, which is not \
             compiled into this build of pdf_fast; install a wheel built with it \
             or call with reading_order=False",
        ));
    }
    Ok(())
}

/// The reading-order engine is compiled in; nothing to reject.
#[cfg(feature = "reading-order")]
fn ensure_reading_order_available(_reading_order: bool) -> PyResult<()> {
    Ok(())
}

/// Order a page's text runs, either by reading order or in stream order.
///
/// The reading-order branch is the only consumer of the layout engine, and it
/// is compiled out entirely without the `reading-order` feature — so the
/// default build cannot regress into depending on it.
///
/// Without the feature this returns stream order. That is safe because
/// [`compute_full_text`] rejects `reading_order = true` before reaching here;
/// the argument is defensive so a future caller cannot get a silent
/// substitution.
fn ordered_run_indices(
    doc: &PdfDocument,
    runs: &[pdf_fast_core::PdfTextRun],
    reading_order: bool,
) -> Vec<usize> {
    let stream_order = || (0..runs.len()).collect::<Vec<usize>>();

    if !reading_order || runs.len() < 2 {
        return stream_order();
    }

    #[cfg(not(feature = "reading-order"))]
    {
        // Unreachable via `compute_full_text`, which refuses first.
        let _ = doc;
        stream_order()
    }

    #[cfg(feature = "reading-order")]
    {
        let paragraphs = pdf_fast_core::paragraph::merge_paragraphs(runs);
        if paragraphs.len() < 2 {
            return stream_order();
        }

        use layout_fast_core::LayoutEngine;
        // Geometry-only blocks: RoutedEngine / spectral / recursive never read
        // `text` on this path, so skip cloning paragraph strings into every
        // Block (hot path on dense papers).
        let blocks: Vec<layout_fast_core::Block> = paragraphs
            .iter()
            .map(|p| layout_fast_core::Block {
                id: p.id,
                bbox: layout_fast_core::BBox::new(p.x, p.y, p.x + p.w, p.y + p.h),
                font_size: Some(p.font_size),
                font_id: None,
                text: None,
                language: None,
                source: layout_fast_core::BlockSource::PdfTextStream,
                // Native PDF paragraphs carry no detector kind; RoutedEngine
                // treats `None` as plain body text.
                kind: None,
            })
            .collect();

        // Routed reading-order policy — 92.3% on olmOCR-bench `multi_column`,
        // which is exactly this native-PDF-text paragraph path. Supersedes the
        // prior hardcoded spectral-only σ (7.5, 25.0); the policy and σ are
        // pinned inside RoutedEngine::shared().
        let engine = layout_fast_core::RoutedEngine::shared();
        let order = engine.order(&blocks);

        // Shared dense-id permutation check (same contract as
        // layout_fast_core::is_valid_permutation for id==index).
        if !layout_fast_core::is_valid_permutation(&blocks, &order) {
            return stream_order();
        }

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
        for (i, &done) in emitted.iter().enumerate() {
            if !done {
                flat.push(i);
            }
        }
        flat
    }
}

/// Emit ONE page's text into `buf` and accumulate its diagnostics.
fn emit_page_text(
    doc: &PdfDocument,
    page: &pdf_fast_core::PdfPage,
    reading_order: bool,
    buf: &mut String,
    diag: &mut FullTextDiagnostics,
) {
    {
        let runs = doc.semantic_text_runs_for_page(page);
        diag.runs += runs.len();

        let run_order: Vec<usize> = ordered_run_indices(doc, &runs, reading_order);

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

        for &__order_idx in &run_order {
            let run = &runs[__order_idx];
            diag.chars += run.text.chars().count();
            diag.fallback_chars += run.cmap_fallback_chars as usize;
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
                            if !trailing_hyphen_is_compound_prefix(buf) {
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
                            // no separator
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
    }
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

/// `buf` ends with `-`; return true when the alphabetic stem before it is a
/// compound prefix that should keep the hyphen (e.g. `well-known`).
fn trailing_hyphen_is_compound_prefix(buf: &str) -> bool {
    let Some(before) = buf.strip_suffix('-') else {
        return false;
    };
    let mut start = before.len();
    for (i, c) in before.char_indices().rev() {
        if !c.is_alphabetic() {
            break;
        }
        start = i;
    }
    is_compound_prefix(&before[start..])
}

fn is_compound_prefix(stem: &str) -> bool {
    // All entries are ASCII; compare case-insensitively without allocating.
    const PREFIXES: &[&str] = &[
        "a", "all", "anti", "auto", "bi", "bio", "co", "cross", "de", "dis", "e", "ex", "extra",
        "full", "half", "high", "inter", "intra", "long", "low", "mega", "meta", "mid", "multi",
        "neo", "new", "non", "off", "old", "on", "out", "over", "pan", "post", "pre", "pro",
        "pseudo", "re", "self", "semi", "short", "sub", "super", "tele", "trans", "ultra", "un",
        "under", "well", "x",
    ];
    PREFIXES
        .iter()
        .any(|prefix| stem.eq_ignore_ascii_case(prefix))
}

// Reading-order separator decision from the previous and current run's
// geometry (position, font size, width, char count) plus glyph-mode flag.
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
    } else {
        if advance.abs() < 0.05 * fs {
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
}

#[pyclass(skip_from_py_object)]
#[derive(Default, Clone)]
struct FullTextDiagnostics {
    #[pyo3(get)]
    pages: usize,
    #[pyo3(get)]
    runs: usize,
    #[pyo3(get)]
    chars: usize,
    #[pyo3(get)]
    fallback_chars: usize,
    #[pyo3(get)]
    via_to_unicode: usize,
    #[pyo3(get)]
    via_type0: usize,
    #[pyo3(get)]
    via_differences: usize,
    #[pyo3(get)]
    via_named: usize,
    #[pyo3(get)]
    via_standard: usize,
    #[pyo3(get)]
    via_raw_byte: usize,
}

impl FullTextDiagnostics {
    /// Fold another page/shard's counters in. Summation is associative and
    /// commutative, so the parallel reduce yields the same totals as the
    /// sequential accumulation regardless of thread scheduling.
    fn merge(&mut self, other: &FullTextDiagnostics) {
        self.pages += other.pages;
        self.runs += other.runs;
        self.chars += other.chars;
        self.fallback_chars += other.fallback_chars;
        self.via_to_unicode += other.via_to_unicode;
        self.via_type0 += other.via_type0;
        self.via_differences += other.via_differences;
        self.via_named += other.via_named;
        self.via_standard += other.via_standard;
        self.via_raw_byte += other.via_raw_byte;
    }
}

#[pymethods]
impl FullTextDiagnostics {
    fn __repr__(&self) -> String {
        format!(
            "FullTextDiagnostics(pages={}, runs={}, chars={}, fallback_chars={}, via_to_unicode={}, via_type0={}, via_differences={}, via_named={}, via_standard={}, via_raw_byte={})",
            self.pages,
            self.runs,
            self.chars,
            self.fallback_chars,
            self.via_to_unicode,
            self.via_type0,
            self.via_differences,
            self.via_named,
            self.via_standard,
            self.via_raw_byte,
        )
    }
}

/// Helper decoupled from Python allocations. Serializes Arrow memory purely outside the GIL.
fn record_batch_to_ipc_bytes(batch: &arrow::record_batch::RecordBatch) -> PyResult<Vec<u8>> {
    let mut out = Cursor::new(Vec::with_capacity(65536)); // Preallocate 64KB
    {
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(&mut out, &batch.schema())
            .map_err(to_py_err)?;
        writer.write(batch).map_err(to_py_err)?;
        writer.finish().map_err(to_py_err)?;
    }
    Ok(out.into_inner())
}

fn record_batch_to_c_data(batch: arrow::record_batch::RecordBatch) -> PyResult<(usize, usize)> {
    let struct_array = StructArray::from(batch);
    let data_type = struct_array.data_type().clone();
    let data = struct_array.to_data();
    let ffi_array = FFI_ArrowArray::new(&data);
    let ffi_schema = FFI_ArrowSchema::try_from(&data_type).map_err(to_py_err)?;
    let array_ptr = Box::into_raw(Box::new(ffi_array)) as usize;
    let schema_ptr = Box::into_raw(Box::new(ffi_schema)) as usize;
    Ok((array_ptr, schema_ptr))
}

fn record_batch_to_arrow_c_array_capsules<'py>(
    py: Python<'py>,
    batch: RecordBatch,
) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
    let struct_array = StructArray::from(batch);
    let data_type = struct_array.data_type().clone();
    let data = struct_array.to_data();
    let ffi_array = FFI_ArrowArray::new(&data);
    let ffi_schema = FFI_ArrowSchema::try_from(&data_type).map_err(to_py_err)?;

    // Capsule destructors free the FFI structs. PyArrow's
    // `_import_from_c_capsule` moves ownership out and clears the release
    // callbacks before the capsules are GC'd, so the destructors become no-ops.
    let schema_capsule = PyCapsule::new_with_value_and_destructor(
        py,
        ffi_schema,
        c"arrow_schema",
        |schema: FFI_ArrowSchema, _| drop(schema),
    )?;
    let array_capsule = PyCapsule::new_with_value_and_destructor(
        py,
        ffi_array,
        c"arrow_array",
        |array: FFI_ArrowArray, _| drop(array),
    )?;
    Ok((schema_capsule, array_capsule))
}

/// Columns an OCR Arrow batch must carry, with the types the table routing
/// can read from each.
const OCR_BATCH_COLUMNS: [(&str, &str); 4] = [
    ("text", "Utf8 or LargeUtf8"),
    ("px_x", "Float32 or Float64"),
    ("px_y", "Float32 or Float64"),
    ("px_height", "Float32 or Float64"),
];

/// Refuses an OCR Arrow batch whose schema lacks a required column or gives
/// one a type the table routing cannot read, naming the column.
///
/// The core reads these columns only when the page has a table and only row
/// by row, so without this a wrong batch fails late, or not at all.
fn validate_ocr_batch_schema(batch: &RecordBatch) -> PyResult<()> {
    use arrow::datatypes::DataType;
    let schema = batch.schema();
    for (name, expected) in OCR_BATCH_COLUMNS {
        let field = schema.field_with_name(name).map_err(|_| {
            PyValueError::new_err(format!(
                "ocr_runs Arrow batch is missing required column '{name}' \
                 (expected text, px_x, px_y, px_height)"
            ))
        })?;
        let readable = match name {
            "text" => matches!(field.data_type(), DataType::Utf8 | DataType::LargeUtf8),
            _ => matches!(field.data_type(), DataType::Float32 | DataType::Float64),
        };
        if !readable {
            return Err(PyValueError::new_err(format!(
                "ocr_runs column '{name}' must be {expected}, got {:?}",
                field.data_type()
            )));
        }
    }
    Ok(())
}

/// Converts OCR runs from Python and, for an Arrow batch, checks its schema
/// before any other work.
fn ocr_input_from_py(value: &Bound<'_, PyAny>) -> PyResult<OcrInput> {
    let input = ocr_input_from_py_unchecked(value)?;
    if let OcrInput::Batch(batch) = &input {
        validate_ocr_batch_schema(batch)?;
    }
    Ok(input)
}

impl PdfScanner {
    /// Table layout of one page, routed with already converted OCR runs, as
    /// Arrow IPC bytes.
    fn table_layout_ipc_from_ocr<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr: OcrInput,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch =
                page_table_layout_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr)
                    .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Tables of one page, routed with already converted OCR runs, as Arrow
    /// IPC bytes.
    fn table_ipc_from_ocr<'py>(
        &self,
        py: Python<'py>,
        page_index: usize,
        ocr: OcrInput,
        epsilon: f32,
        dpi: f32,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let doc = Arc::clone(&self.doc);
        let bytes = py.detach(move || {
            let batch = page_table_batch_from_ocr_input(&doc, page_index, epsilon, dpi, &ocr)
                .map_err(to_py_err)?;
            record_batch_to_ipc_bytes(&batch)
        })?;
        Ok(PyBytes::new(py, &bytes))
    }
}

fn ocr_input_from_py_unchecked(value: &Bound<'_, PyAny>) -> PyResult<OcrInput> {
    if value.is_instance_of::<PyString>() {
        let text: String = value.extract()?;
        return Ok(OcrInput::Runs(parse_ocr_runs_json(&text)?));
    }
    if value.is_instance_of::<PyBytes>() {
        let bytes: &[u8] = value.extract()?;
        return Ok(OcrInput::Batch(record_batch_from_ipc_bytes(bytes)?));
    }
    // Arrow PyCapsule array protocol (RecordBatch / Array).
    if value.hasattr("__arrow_c_array__")? {
        let batch = record_batch_from_arrow_c_array(value)?;
        return Ok(OcrInput::Batch(batch));
    }
    // Arrow PyCapsule stream protocol (Table / RecordBatchReader).
    if value.hasattr("__arrow_c_stream__")? {
        return Ok(OcrInput::Batch(record_batch_from_arrow_c_stream(value)?));
    }
    // Legacy PyArrow private export.
    if value.hasattr("_export_to_c")? {
        let batch = record_batch_from_pyarrow_export(value)?;
        return Ok(OcrInput::Batch(batch));
    }
    if value.is_instance_of::<PyList>() {
        let list = value.cast::<PyList>()?;
        let mut runs = Vec::with_capacity(list.len());
        for (idx, item) in list.iter().enumerate() {
            runs.push(ocr_run_from_mapping(&item, idx)?);
        }
        return Ok(OcrInput::Runs(runs));
    }
    // Accept any sequence of mappings (tuple, etc.).
    let seq = value.extract::<Vec<Bound<'_, PyAny>>>()?;
    let mut runs = Vec::with_capacity(seq.len());
    for (idx, item) in seq.into_iter().enumerate() {
        runs.push(ocr_run_from_mapping(&item, idx)?);
    }
    Ok(OcrInput::Runs(runs))
}

fn page_table_layout_batch_from_ocr_input(
    doc: &PdfDocument,
    page_index: usize,
    epsilon: f32,
    dpi: f32,
    ocr: &OcrInput,
) -> anyhow::Result<RecordBatch> {
    match ocr {
        OcrInput::Batch(batch) => {
            doc.page_table_layout_record_batch_from_ocr_batch(page_index, epsilon, dpi, batch)
        }
        OcrInput::Runs(runs) => {
            doc.page_table_layout_record_batch_from_ocr(page_index, epsilon, dpi, runs)
        }
    }
}

fn page_table_batch_from_ocr_input(
    doc: &PdfDocument,
    page_index: usize,
    epsilon: f32,
    dpi: f32,
    ocr: &OcrInput,
) -> anyhow::Result<RecordBatch> {
    match ocr {
        OcrInput::Batch(batch) => {
            doc.page_table_record_batch_from_ocr_batch(page_index, epsilon, dpi, batch)
        }
        OcrInput::Runs(runs) => {
            doc.page_table_record_batch_from_ocr(page_index, epsilon, dpi, runs)
        }
    }
}

fn record_batch_from_arrow_c_array(value: &Bound<'_, PyAny>) -> PyResult<RecordBatch> {
    let pair = value.call_method0("__arrow_c_array__").map_err(|err| {
        PyValueError::new_err(format!(
            "ocr_runs Arrow PyCapsule export failed via __arrow_c_array__: {err}"
        ))
    })?;
    let schema_obj = pair.get_item(0)?;
    let array_obj = pair.get_item(1)?;
    let schema_capsule = schema_obj.cast::<PyCapsule>().map_err(|_| {
        PyValueError::new_err("ocr_runs __arrow_c_array__()[0] must be an arrow_schema PyCapsule")
    })?;
    let array_capsule = array_obj.cast::<PyCapsule>().map_err(|_| {
        PyValueError::new_err("ocr_runs __arrow_c_array__()[1] must be an arrow_array PyCapsule")
    })?;

    const SCHEMA_NAME: &CStr = c"arrow_schema";
    const ARRAY_NAME: &CStr = c"arrow_array";

    let schema_ptr = schema_capsule.pointer_checked(Some(SCHEMA_NAME))?;
    let array_ptr = array_capsule.pointer_checked(Some(ARRAY_NAME))?;

    // SAFETY: Capsule pointers are Arrow C Data structs produced by the
    // producer. We move them onto the stack and empty the capsule payloads so
    // the capsule destructors become no-ops; `from_ffi` then owns the buffers.
    let (ffi_array, ffi_schema) = unsafe {
        let schema_raw = schema_ptr.as_ptr() as *mut FFI_ArrowSchema;
        let array_raw = array_ptr.as_ptr() as *mut FFI_ArrowArray;
        let ffi_schema = std::ptr::read(schema_raw);
        let ffi_array = std::ptr::read(array_raw);
        std::ptr::write(schema_raw, FFI_ArrowSchema::empty());
        std::ptr::write(array_raw, FFI_ArrowArray::empty());
        (ffi_array, ffi_schema)
    };
    let data = unsafe { from_ffi(ffi_array, &ffi_schema) }.map_err(to_py_err)?;
    array_data_to_record_batch(data)
}

fn record_batch_from_arrow_c_stream(value: &Bound<'_, PyAny>) -> PyResult<RecordBatch> {
    let capsule_obj = value.call_method0("__arrow_c_stream__").map_err(|err| {
        PyValueError::new_err(format!(
            "ocr_runs Arrow stream export failed via __arrow_c_stream__: {err}"
        ))
    })?;
    let capsule = capsule_obj.cast::<PyCapsule>().map_err(|_| {
        PyValueError::new_err(
            "ocr_runs __arrow_c_stream__() must return an arrow_array_stream PyCapsule",
        )
    })?;

    const STREAM_NAME: &CStr = c"arrow_array_stream";
    let stream_ptr = capsule.pointer_checked(Some(STREAM_NAME))?;

    // SAFETY: Capsule holds an FFI_ArrowArrayStream. `from_raw` moves ownership
    // out and empties the capsule payload so its destructor becomes a no-op.
    let mut reader = unsafe {
        let raw = stream_ptr.as_ptr() as *mut FFI_ArrowArrayStream;
        let stream = FFI_ArrowArrayStream::from_raw(raw);
        ArrowArrayStreamReader::try_new(stream).map_err(to_py_err)?
    };

    let mut batches = Vec::new();
    for batch in &mut reader {
        batches.push(batch.map_err(to_py_err)?);
    }
    concat_ocr_batches(batches)
}

fn concat_ocr_batches(batches: Vec<RecordBatch>) -> PyResult<RecordBatch> {
    match batches.len() {
        0 => Err(PyValueError::new_err(
            "ocr_runs Arrow stream produced zero batches \
             (expected columns text, px_x, px_y, px_height)",
        )),
        1 => {
            let mut batches = batches;
            Ok(batches.remove(0))
        }
        _ => {
            let schema = batches[0].schema();
            concat_batches(&schema, &batches).map_err(to_py_err)
        }
    }
}

fn record_batch_from_pyarrow_export(value: &Bound<'_, PyAny>) -> PyResult<RecordBatch> {
    let mut ffi_array = FFI_ArrowArray::empty();
    let mut ffi_schema = FFI_ArrowSchema::empty();
    let array_ptr = &mut ffi_array as *mut FFI_ArrowArray as usize;
    let schema_ptr = &mut ffi_schema as *mut FFI_ArrowSchema as usize;
    value
        .call_method1("_export_to_c", (array_ptr, schema_ptr))
        .map_err(|err| {
            PyValueError::new_err(format!(
                "ocr_runs Arrow export failed via _export_to_c: {err}"
            ))
        })?;
    // SAFETY: pointers were filled by PyArrow's `_export_to_c` into our stack
    // FFI structs; ownership of the buffers transfers to `from_ffi`.
    let data = unsafe { from_ffi(ffi_array, &ffi_schema) }.map_err(to_py_err)?;
    array_data_to_record_batch(data)
}

fn array_data_to_record_batch(data: ArrayData) -> PyResult<RecordBatch> {
    let array = arrow::array::make_array(data);
    let struct_array = array
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| {
            PyValueError::new_err(
                "ocr_runs Arrow input must be a RecordBatch/StructArray with columns \
                 text, px_x, px_y, px_height",
            )
        })?;
    // `StructArray -> RecordBatch` is infallible.
    Ok(RecordBatch::from(struct_array.clone()))
}

fn record_batch_from_ipc_bytes(bytes: &[u8]) -> PyResult<RecordBatch> {
    let reader =
        arrow::ipc::reader::StreamReader::try_new(Cursor::new(bytes), None).map_err(to_py_err)?;
    let mut batches = Vec::new();
    for batch in reader {
        batches.push(batch.map_err(to_py_err)?);
    }
    concat_ocr_batches(batches)
}

/// Validate OCR Arrow schema/nullability without building `Vec<OcrTextRun>`.
///
/// Used by unit tests; production Arrow intake keeps the batch and lets core
/// walk columns via `page_table_*_from_ocr_batch`.
#[cfg(test)]
fn validate_ocr_record_batch(batch: &RecordBatch) -> Result<(), String> {
    let text = required_column_inner(batch, "text")?;
    let px_x = required_column_inner(batch, "px_x")?;
    let px_y = required_column_inner(batch, "px_y")?;
    let px_height = required_column_inner(batch, "px_height")?;

    let n = batch.num_rows();
    for i in 0..n {
        if text.is_null(i) || px_x.is_null(i) || px_y.is_null(i) || px_height.is_null(i) {
            return Err(format!(
                "ocr_runs Arrow row {i} has null in a required column \
                 (text/px_x/px_y/px_height)"
            ));
        }
        // Type-check first row values (and every row for utf8/float downcasts).
        let _ = utf8_value_inner(text, i)?;
        let _ = f32_value_inner(px_x, i, "px_x")?;
        let _ = f32_value_inner(px_y, i, "px_y")?;
        let _ = f32_value_inner(px_height, i, "px_height")?;
    }
    Ok(())
}

/// Legacy materializer kept for tests that assert row-wise `OcrTextRun` fields.
#[cfg(test)]
fn ocr_runs_from_record_batch_inner(batch: &RecordBatch) -> Result<Vec<OcrTextRun>, String> {
    validate_ocr_record_batch(batch)?;
    let text = required_column_inner(batch, "text")?;
    let px_x = required_column_inner(batch, "px_x")?;
    let px_y = required_column_inner(batch, "px_y")?;
    let px_height = required_column_inner(batch, "px_height")?;

    let n = batch.num_rows();
    let mut runs = Vec::with_capacity(n);
    for i in 0..n {
        runs.push(OcrTextRun {
            text: utf8_value_inner(text, i)?,
            px_x: f32_value_inner(px_x, i, "px_x")?,
            px_y: f32_value_inner(px_y, i, "px_y")?,
            px_height: f32_value_inner(px_height, i, "px_height")?,
        });
    }
    Ok(runs)
}

#[cfg(test)]
fn required_column_inner<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a Arc<dyn Array>, String> {
    batch.column_by_name(name).ok_or_else(|| {
        format!(
            "ocr_runs Arrow batch missing required column '{name}' \
             (expected text, px_x, px_y, px_height)"
        )
    })
}

#[cfg(test)]
fn utf8_value_inner(array: &Arc<dyn Array>, index: usize) -> Result<String, String> {
    use arrow::array::{LargeStringArray, StringArray};
    if let Some(arr) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(arr.value(index).to_owned());
    }
    if let Some(arr) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Ok(arr.value(index).to_owned());
    }
    Err(format!(
        "ocr_runs column 'text' must be Utf8 or LargeUtf8, got {:?}",
        array.data_type()
    ))
}

#[cfg(test)]
fn f32_value_inner(array: &Arc<dyn Array>, index: usize, name: &str) -> Result<f32, String> {
    use arrow::array::{Float32Array, Float64Array};
    if let Some(arr) = array.as_any().downcast_ref::<Float32Array>() {
        return Ok(arr.value(index));
    }
    if let Some(arr) = array.as_any().downcast_ref::<Float64Array>() {
        return Ok(arr.value(index) as f32);
    }
    Err(format!(
        "ocr_runs column '{name}' must be Float32 or Float64, got {:?}",
        array.data_type()
    ))
}

fn ocr_run_from_mapping(item: &Bound<'_, PyAny>, idx: usize) -> PyResult<OcrTextRun> {
    let text: String = item
        .get_item("text")
        .map_err(|_| {
            PyValueError::new_err(format!(
                "ocr_runs[{idx}] missing required key 'text' (expected mapping with text/px_x/px_y/px_height)"
            ))
        })?
        .extract()?;
    let px_x: f32 = item
        .get_item("px_x")
        .map_err(|_| PyValueError::new_err(format!("ocr_runs[{idx}] missing required key 'px_x'")))?
        .extract()?;
    let px_y: f32 = item
        .get_item("px_y")
        .map_err(|_| PyValueError::new_err(format!("ocr_runs[{idx}] missing required key 'px_y'")))?
        .extract()?;
    let px_height: f32 = item
        .get_item("px_height")
        .map_err(|_| {
            PyValueError::new_err(format!("ocr_runs[{idx}] missing required key 'px_height'"))
        })?
        .extract()?;
    Ok(OcrTextRun {
        text,
        px_x,
        px_y,
        px_height,
    })
}

fn parse_ocr_runs_json(ocr_runs_json: &str) -> PyResult<Vec<OcrTextRun>> {
    serde_json::from_str(ocr_runs_json).map_err(to_py_err)
}

// ==============================================================================================
// Standalone python functions matching the PdfScanner architecture
// ==============================================================================================

#[pyfunction]
fn scan_pdf_json_py(py: Python<'_>, path: &str) -> PyResult<String> {
    PdfScanner::new(path.to_string())?.scan_json(py)
}

#[pyfunction]
fn scan_pdf_semantic_runs_json_py(py: Python<'_>, path: &str) -> PyResult<String> {
    PdfScanner::new(path.to_string())?.semantic_runs_json(py)
}

#[pyfunction]
fn scan_pdf_paragraphs_json_py(py: Python<'_>, path: &str) -> PyResult<String> {
    PdfScanner::new(path.to_string())?.paragraphs_json(py)
}

#[pyfunction]
fn scan_pdf_semantic_runs_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    PdfScanner::new(path.to_string())?.semantic_runs_arrow_ipc(py)
}

#[pyfunction]
fn scan_pdf_page_multimodal_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
) -> PyResult<Bound<'py, PyBytes>> {
    PdfScanner::new(path.to_string())?.page_multimodal_arrow_ipc(py)
}

#[pyfunction]
fn scan_pdf_page_multimodal_c_data_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
) -> PyResult<(usize, usize)> {
    PdfScanner::new(path.to_string())?.page_multimodal_c_data(py, page_index)
}

#[pyfunction]
fn scan_pdf_page_bitmap_components_json_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
) -> PyResult<String> {
    PdfScanner::new(path.to_string())?.page_bitmap_components_json(py, page_index)
}

#[pyfunction]
#[pyo3(signature = (path, page_index, epsilon=None))]
fn scan_pdf_page_table_layout_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
    page_index: usize,
    epsilon: Option<f32>,
) -> PyResult<Bound<'py, PyBytes>> {
    PdfScanner::new(path.to_string())?.page_table_layout_arrow_ipc(
        py,
        page_index,
        epsilon.unwrap_or(2.0),
    )
}

#[pyfunction]
#[pyo3(signature = (path, page_index, epsilon=None))]
fn scan_pdf_page_table_layout_c_data_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
    epsilon: Option<f32>,
) -> PyResult<(usize, usize)> {
    PdfScanner::new(path.to_string())?.page_table_layout_c_data(
        py,
        page_index,
        epsilon.unwrap_or(2.0),
    )
}

#[pyfunction]
#[pyo3(signature = (path, page_index, ocr_runs, epsilon=None, dpi=None))]
fn scan_pdf_page_table_layout_from_ocr_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
    page_index: usize,
    ocr_runs: &Bound<'_, PyAny>,
    epsilon: Option<f32>,
    dpi: Option<f32>,
) -> PyResult<Bound<'py, PyBytes>> {
    // The batch is checked before the document is opened.
    let ocr = ocr_input_from_py(ocr_runs)?;
    PdfScanner::new(path.to_string())?.table_layout_ipc_from_ocr(
        py,
        page_index,
        ocr,
        epsilon.unwrap_or(2.0),
        dpi.unwrap_or(300.0),
    )
}

#[pyfunction]
#[pyo3(signature = (path, page_index, ocr_runs, epsilon=None, dpi=None))]
fn scan_pdf_page_table_layout_from_ocr_c_data_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
    ocr_runs: &Bound<'_, PyAny>,
    epsilon: Option<f32>,
    dpi: Option<f32>,
) -> PyResult<(usize, usize)> {
    PdfScanner::new(path.to_string())?.page_table_layout_from_ocr_c_data(
        py,
        page_index,
        ocr_runs,
        epsilon.unwrap_or(2.0),
        dpi.unwrap_or(300.0),
    )
}

#[pyfunction]
#[pyo3(signature = (path, page_index, epsilon=None))]
fn scan_pdf_page_table_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
    page_index: usize,
    epsilon: Option<f32>,
) -> PyResult<Bound<'py, PyBytes>> {
    PdfScanner::new(path.to_string())?.page_table_arrow_ipc(py, page_index, epsilon.unwrap_or(2.0))
}

#[pyfunction]
#[pyo3(signature = (path, page_index, epsilon=None))]
fn scan_pdf_page_table_c_data_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
    epsilon: Option<f32>,
) -> PyResult<(usize, usize)> {
    PdfScanner::new(path.to_string())?.page_table_c_data(py, page_index, epsilon.unwrap_or(2.0))
}

#[pyfunction]
#[pyo3(signature = (path, page_index, ocr_runs, epsilon=None, dpi=None))]
fn scan_pdf_page_table_from_ocr_arrow_ipc_py<'py>(
    py: Python<'py>,
    path: &str,
    page_index: usize,
    ocr_runs: &Bound<'_, PyAny>,
    epsilon: Option<f32>,
    dpi: Option<f32>,
) -> PyResult<Bound<'py, PyBytes>> {
    // The batch is checked before the document is opened.
    let ocr = ocr_input_from_py(ocr_runs)?;
    PdfScanner::new(path.to_string())?.table_ipc_from_ocr(
        py,
        page_index,
        ocr,
        epsilon.unwrap_or(2.0),
        dpi.unwrap_or(300.0),
    )
}

#[pyfunction]
#[pyo3(signature = (path, page_index, ocr_runs, epsilon=None, dpi=None))]
fn scan_pdf_page_table_from_ocr_c_data_py(
    py: Python<'_>,
    path: &str,
    page_index: usize,
    ocr_runs: &Bound<'_, PyAny>,
    epsilon: Option<f32>,
    dpi: Option<f32>,
) -> PyResult<(usize, usize)> {
    PdfScanner::new(path.to_string())?.page_table_from_ocr_c_data(
        py,
        page_index,
        ocr_runs,
        epsilon.unwrap_or(2.0),
        dpi.unwrap_or(300.0),
    )
}

#[pyfunction]
fn scan_pdf_page_count_py(path: &str) -> PyResult<usize> {
    Ok(PdfScanner::new(path.to_string())?.page_count)
}

#[pyfunction]
fn scan_pdf_page_dimensions_json_py(path: &str) -> PyResult<String> {
    PdfScanner::new(path.to_string())?.page_dimensions_json()
}

#[pyfunction]
fn scan_pdf_page_rasterized_png_py<'py>(
    py: Python<'py>,
    path: &str,
    page_index: usize,
    dpi: Option<f32>,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    PdfScanner::new(path.to_string())?.page_rasterized_png(py, page_index, dpi)
}

#[pyfunction]
#[pyo3(signature = (path, reading_order=false))]
fn scan_pdf_full_text_py(
    py: Python<'_>,
    path: &str,
    reading_order: bool,
) -> PyResult<(String, FullTextDiagnostics)> {
    PdfScanner::new(path.to_string())?.full_text(py, reading_order)
}

fn ensure_pdf_source_within_limit(
    source_len: usize,
    max_document_bytes: usize,
) -> Result<(), String> {
    if source_len > max_document_bytes {
        return Err(format!(
            "PDF source is {source_len} bytes; maximum is {max_document_bytes} bytes"
        ));
    }
    Ok(())
}

#[pyfunction]
#[pyo3(signature = (source, reading_order=false))]
fn scan_pdf_full_text_bytes_py(
    py: Python<'_>,
    source: &Bound<'_, PyBytes>,
    reading_order: bool,
) -> PyResult<(String, FullTextDiagnostics)> {
    let options = ParseOptions {
        keep_decoded_streams: false,
        extract_text: true,
        extract_cmaps: true,
        ..ParseOptions::default()
    };
    let source = source.as_bytes();
    ensure_pdf_source_within_limit(source.len(), options.max_document_bytes)
        .map_err(PyValueError::new_err)?;
    let source = source.to_vec();
    py.detach(move || {
        let doc = PdfDocument::from_bytes(source, options).map_err(to_py_err)?;
        compute_full_text(&doc, reading_order)
    })
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

#[pyfunction]
fn release_arrow_c_data_py(array_ptr: usize, schema_ptr: usize) -> PyResult<()> {
    if array_ptr == 0 || schema_ptr == 0 {
        return Err(PyValueError::new_err("null Arrow C Data pointer"));
    }
    // SAFETY: pointers must come from `record_batch_to_c_data` and must not
    // have been imported into PyArrow or freed already.
    unsafe {
        drop(Box::from_raw(array_ptr as *mut FFI_ArrowArray));
        drop(Box::from_raw(schema_ptr as *mut FFI_ArrowSchema));
    }
    Ok(())
}

fn collect_page_dimensions(doc: &PdfDocument) -> Vec<PageDimensionData> {
    doc.pages()
        .into_iter()
        .map(|page| {
            let (x0, y0, width, height) = match page.page_box() {
                Some([a, b, c, d]) => {
                    let w = (c - a).abs();
                    let h = (d - b).abs();
                    (a.min(c), b.min(d), w, h)
                }
                None => (0.0f32, 0.0f32, 0.0f32, 0.0f32),
            };
            PageDimensionData {
                page_index: page.page_index,
                x0,
                y0,
                width,
                height,
                rotate: page.rotate,
            }
        })
        .collect()
}

fn open_document(path: &str) -> PyResult<PdfDocument> {
    PdfDocument::open(
        path,
        ParseOptions {
            keep_decoded_streams: false,
            extract_text: true,
            extract_cmaps: true,
            ..ParseOptions::default()
        },
    )
    .map_err(to_py_err)
}

fn to_py_err(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

/// TRUE-SCAN entry for arbitrary images (public scanned benchmarks):
/// glyph geometry + line bands from a raw grayscale buffer. Same output
/// shape as the PDF paths; coordinates in PIXELS (caller owns units).
#[pyfunction]
#[pyo3(signature = (width, height, luma, k=32, threshold=140, min_px=4, max_px=120, line_tolerance=2.0, split_columns=false, px_per_pt=1.0))]
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn raster_glyph_lines_from_luma(
    py: Python<'_>,
    width: usize,
    height: usize,
    luma: Vec<u8>,
    k: usize,
    threshold: u8,
    min_px: usize,
    max_px: usize,
    line_tolerance: f32,
    split_columns: bool,
    px_per_pt: f32,
) -> PyResult<(Vec<(f32, f32, f32, Vec<f32>)>, Vec<([f32; 4], Vec<u32>)>)> {
    py.detach(move || {
        // Banding constants (gap floor, leading clamp) are POINT-tuned; the
        // image entry must band in points too or dense-table leading
        // collapses onto the clamp floor (measured: 28 bands of height
        // 7.8px for a 50-row table at 200dpi). Coordinates are returned in
        // POINTS when px_per_pt is provided.
        let inv = 1.0 / px_per_pt.max(1e-6);
        let glyphs: Vec<(f32, f32, f32, Vec<f32>)> =
            pdf_fast_core::raster_glyphs::glyphs_from_luma(
                width, height, &luma, threshold, k, min_px, max_px,
            )
            .into_iter()
            .map(|(x, y, s, d)| (x * inv, y * inv, s * inv, d))
            .collect();
        let items: Vec<(f32, f32, u32)> = glyphs
            .iter()
            .enumerate()
            .map(|(i, g)| (g.0, g.1, i as u32))
            .collect();
        // same density-adaptive selection as the core raster path
        let y_lo = items.iter().map(|i| i.1).fold(f32::MAX, f32::min);
        let y_hi = items.iter().map(|i| i.1).fold(f32::MIN, f32::max);
        let density = items.len() as f32 / (y_hi - y_lo).max(1.0);
        let banded = if density > 7.0 {
            pdf_fast_core::band_by_projection(items, (line_tolerance * 0.2).max(0.3), split_columns)
        } else {
            pdf_fast_core::band_text_lines_public(items, line_tolerance, split_columns)
        };
        let lines = banded
            .into_iter()
            .map(|(b, m)| (b, m.into_iter().map(|(_, i)| i).collect()))
            .collect();
        Ok((glyphs, lines))
    })
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PdfScanner>()?;
    m.add_class::<PageDimension>()?;
    m.add_class::<FullTextDiagnostics>()?;
    m.add_function(wrap_pyfunction!(scan_pdf_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_semantic_runs_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_paragraphs_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_semantic_runs_arrow_ipc_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_multimodal_arrow_ipc_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_multimodal_c_data_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        scan_pdf_page_bitmap_components_json_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        scan_pdf_page_table_layout_arrow_ipc_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_table_layout_c_data_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        scan_pdf_page_table_layout_from_ocr_arrow_ipc_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        scan_pdf_page_table_layout_from_ocr_c_data_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_table_arrow_ipc_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_table_c_data_py, m)?)?;
    m.add_function(wrap_pyfunction!(
        scan_pdf_page_table_from_ocr_arrow_ipc_py,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_table_from_ocr_c_data_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_count_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_dimensions_json_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_page_rasterized_png_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_full_text_py, m)?)?;
    m.add_function(wrap_pyfunction!(scan_pdf_full_text_bytes_py, m)?)?;
    m.add_function(wrap_pyfunction!(release_arrow_c_data_py, m)?)?;
    m.add_function(wrap_pyfunction!(raster_glyph_lines_from_luma, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_pdf_source_within_limit, ocr_runs_from_record_batch_inner, validate_ocr_record_batch,
    };
    use arrow::array::{ArrayRef, Float32Array, Float64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    fn sample_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("px_x", DataType::Float32, false),
            Field::new("px_y", DataType::Float32, false),
            Field::new("px_height", DataType::Float32, false),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec!["A1", "B2"]));
        let px_x: ArrayRef = Arc::new(Float32Array::from(vec![10.0_f32, 100.0]));
        let px_y: ArrayRef = Arc::new(Float32Array::from(vec![20.0_f32, 40.0]));
        let px_height: ArrayRef = Arc::new(Float32Array::from(vec![12.0_f32, 12.0]));
        RecordBatch::try_new(schema, vec![text, px_x, px_y, px_height]).unwrap()
    }

    #[test]
    fn parses_ocr_runs_from_float32_record_batch() {
        let runs = ocr_runs_from_record_batch_inner(&sample_batch()).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "A1");
        assert_eq!(runs[0].px_x, 10.0);
        assert_eq!(runs[1].text, "B2");
        assert_eq!(runs[1].px_y, 40.0);
    }

    #[test]
    fn validates_ocr_record_batch_without_materializing_runs() {
        validate_ocr_record_batch(&sample_batch()).unwrap();
    }

    #[test]
    fn accepts_float64_numeric_columns() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("px_x", DataType::Float64, false),
            Field::new("px_y", DataType::Float64, false),
            Field::new("px_height", DataType::Float64, false),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec!["X"]));
        let px_x: ArrayRef = Arc::new(Float64Array::from(vec![1.5_f64]));
        let px_y: ArrayRef = Arc::new(Float64Array::from(vec![2.5_f64]));
        let px_height: ArrayRef = Arc::new(Float64Array::from(vec![3.5_f64]));
        let batch = RecordBatch::try_new(schema, vec![text, px_x, px_y, px_height]).unwrap();
        validate_ocr_record_batch(&batch).unwrap();
        let runs = ocr_runs_from_record_batch_inner(&batch).unwrap();
        assert_eq!(runs[0].px_x, 1.5);
        assert_eq!(runs[0].px_height, 3.5);
    }

    #[test]
    fn rejects_missing_required_column() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("px_x", DataType::Float32, false),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec!["X"]));
        let px_x: ArrayRef = Arc::new(Float32Array::from(vec![1.0_f32]));
        let batch = RecordBatch::try_new(schema, vec![text, px_x]).unwrap();
        let err = validate_ocr_record_batch(&batch).unwrap_err();
        assert!(err.contains("px_y"), "{err}");
    }

    #[test]
    fn rejects_null_in_required_column() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, true),
            Field::new("px_x", DataType::Float32, false),
            Field::new("px_y", DataType::Float32, false),
            Field::new("px_height", DataType::Float32, false),
        ]));
        let text: ArrayRef = Arc::new(StringArray::from(vec![None::<&str>]));
        let px_x: ArrayRef = Arc::new(Float32Array::from(vec![1.0_f32]));
        let px_y: ArrayRef = Arc::new(Float32Array::from(vec![2.0_f32]));
        let px_height: ArrayRef = Arc::new(Float32Array::from(vec![3.0_f32]));
        let batch = RecordBatch::try_new(schema, vec![text, px_x, px_y, px_height]).unwrap();
        let err = validate_ocr_record_batch(&batch).unwrap_err();
        assert!(err.contains("null"), "{err}");
    }

    /// The parallel full_text reduce folds per-page diagnostics with `merge`.
    /// Summation must be associative so the totals are identical regardless of
    /// how rayon partitions the pages — this pins that contract.
    #[test]
    fn full_text_diagnostics_merge_is_summation() {
        use crate::FullTextDiagnostics;
        let mut a = FullTextDiagnostics {
            pages: 1,
            runs: 10,
            chars: 100,
            via_to_unicode: 8,
            via_named: 2,
            ..Default::default()
        };
        let b = FullTextDiagnostics {
            pages: 1,
            runs: 5,
            chars: 40,
            via_to_unicode: 3,
            via_raw_byte: 1,
            ..Default::default()
        };
        a.merge(&b);
        assert_eq!(a.pages, 2);
        assert_eq!(a.runs, 15);
        assert_eq!(a.chars, 140);
        assert_eq!(a.via_to_unicode, 11);
        assert_eq!(a.via_named, 2);
        assert_eq!(a.via_raw_byte, 1);
    }

    #[test]
    fn full_text_parallel_threshold_is_reasonable() {
        // A guard, not a magic number: small docs must stay on the sequential
        // path (thread spin-up isn't worth it) and the threshold must be > 1.
        //
        // Checked at compile time — the value is a `const`, so a runtime
        // `assert!` could never fail at test time anyway. This way a bad edit
        // breaks the build instead of waiting for the suite to run.
        const _: () = assert!(crate::FULL_TEXT_PARALLEL_MIN_PAGES >= 8);
        const _: () = assert!(crate::FULL_TEXT_PARALLEL_MIN_PAGES <= 128);
    }

    #[test]
    fn bytes_boundary_rejects_source_before_copy() {
        assert!(ensure_pdf_source_within_limit(9, 8).is_err());
        assert!(ensure_pdf_source_within_limit(8, 8).is_ok());
    }
}
