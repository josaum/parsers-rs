//! Paragraph-merge primitive for `PdfTextRun`s.
//!
//! `pdf-fast-core` emits one [`crate::PdfTextRun`] per text-showing
//! operator in the PDF content stream. In practice that means one run
//! per *visual line* — sometimes one per word, sometimes one per
//! glyph in the degenerate per-glyph mode. Downstream layout engines
//! (e.g. `layout-fast-core::SpectralEngine`) want **paragraph-level**
//! blocks: one entry per cohesive run of lines in the same column at
//! the same font size with no break between them. This module
//! produces that.
//!
//! ## Why paragraph granularity matters
//!
//! Carried over verbatim from the bench harness post-mortem on PR #112:
//!
//!   At line-level granularity the PDF producer (LaTeX / Word /
//!   InDesign) already emits text in column-then-line order — so
//!   `EmissionOrderEngine` looks like an oracle (90.1%) and the
//!   spectral engine loses (61.1%). Once line runs are coalesced
//!   into paragraphs, emission collapses to 31.9% (no producer
//!   signal left to lean on) and spectral with paragraph-tuned σ
//!   reaches **88.5%** — `+177 pairwise labels` on olmOCR-bench
//!   `multi_column`.
//!
//! The merge is therefore the lever that lets the layout engine
//! actually do useful work on native-text PDFs.
//!
//! ## Algorithm
//!
//! Greedy left-fold. For each unconsumed run (visited in
//! `(x_min bucket, y_min)` order so left-column runs come before
//! right-column at the same vertical band), extend the chain by either
//! a **vertical** successor (next line in the column) or a
//! **horizontal** successor (next word on the same line). Vertical
//! extension requires **all four** gates; horizontal extension is the
//! word-fragment path that collapses per-glyph / per-word PDF streams
//! into line blocks before the vertical fold continues:
//!
//!   1. **Same font size** (±10 %). A heading or footnote at a
//!      different size belongs to its own paragraph.
//!   2. **Same column** — either `|Δx_min| ≤ 4 pt` (column-aligned)
//!      **or** the horizontal overlap with the predecessor exceeds
//!      50 % of the *narrower* of the two runs' widths (covers
//!      first-line indent / hanging indent). The narrower-width
//!      threshold has a 1 pt floor so two narrow runs across a column
//!      gutter can't slip past on a sub-point overlap (Codex P1 on
//!      #118).
//!   3. **Stacked vertically** — the candidate's `y_min` sits between
//!      the predecessor's `y_max` and `y_max + 1.5 × font_size`.
//!      Bigger gaps signal a paragraph break, column wrap, or
//!      banner.
//!   4. **No intervening block** between the predecessor and the
//!      candidate in the column band. Prevents pulling a left-column
//!      line into a right-column paragraph when their y-extents
//!      happen to align.
//!
//!   H. **Same-line rightward** — same font, `|Δcy| ≤ 0.5 × font_size`,
//!      and the horizontal gap `jx_min − cx_max` is in
//!      `[-0.25 × fs, 2.0 × fs]`. Joins with a space (not `\n`).
//!      This is what collapses the olmOCR-bench hardcore residue of
//!      word-level fragments (`font_size` placeholders, width ≪ page)
//!      that the vertical-only fold left as separate blocks.
//!
//! Output is a [`PdfParagraph`] per chain, re-id'd `0..N-1` in
//! `(y_min, x_min)` order so downstream stays stable. The
//! `line_run_indices` field carries the provenance back to the
//! original `runs` slice — useful for label projection (see
//! `layout-fast-core/bench/extract_labels.py::line_to_paragraph_id`)
//! and for downstream consumers that need to highlight or annotate
//! the underlying runs.
//!
//! Deterministic. O(n²) worst case (n ≤ ~300 runs per page in
//! practice).
//!
//! ## Port provenance
//!
//! Algorithm is the Rust port of
//! `office-parsers-rs/layout-fast-core/bench/extract_labels.py::
//! _merge_paragraphs`. The Python version has seven in-file
//! `unittest` cases pinning the merge invariants; this module mirrors
//! the same case shapes as Rust unit tests below.

use crate::PdfTextRun;

#[cfg(feature = "serde")]
use serde::Serialize;

/// Working projection of a single text run during paragraph merge.
///
/// Layout: `(run_idx, page, x, y, w, h, font_size, text)`. `run_idx`
/// points back into the caller's `&[PdfTextRun]` slice so emitted
/// paragraphs can reference original runs via `line_run_indices`.
type ParagraphItem = (usize, usize, f32, f32, f32, f32, f32, String);

/// A paragraph-merged block over one or more [`PdfTextRun`]s.
///
/// Bounding-box convention follows [`crate::PdfTextBlock`]: `(x, y)` is
/// the top-left in PDF coordinates and `(x + w, y + h)` is the
/// bottom-right. Empty input produces an empty output; the function
/// never panics.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct PdfParagraph {
    /// Stable identifier within a single page's `Vec<PdfParagraph>`.
    /// Reassigned `0..N-1` in `(y_min, x_min)` emission order so that
    /// downstream label projection (line id → paragraph id) is
    /// independent of the input run order.
    pub id: usize,
    /// 0-based page index (carried over from the first contributing
    /// run; all runs in a paragraph share the same page).
    pub page_index: usize,
    /// Concatenated text. Lines joined with `\n` in source-run order
    /// of merging — *not* sorted; downstream that wants a re-sort
    /// should do so explicitly.
    pub text: String,
    /// Representative font size (the first contributing run's size).
    /// Within-paragraph variation is bounded by the ±10 % gate.
    pub font_size: f32,
    /// Left edge of the union bbox in PDF page units.
    pub x: f32,
    /// Top edge of the union bbox.
    pub y: f32,
    /// Width of the union bbox.
    pub w: f32,
    /// Height of the union bbox.
    pub h: f32,
    /// Indices into the input `&[PdfTextRun]` of every run that fed
    /// this paragraph. Order matches the chain-extension order, not
    /// the page emission order.
    pub line_run_indices: Vec<usize>,
}

impl PdfParagraph {
    /// Right edge of the union bbox (`x + w`). Convenience accessor.
    pub fn x_max(&self) -> f32 {
        self.x + self.w
    }
    /// Bottom edge of the union bbox (`y + h`). Convenience accessor.
    pub fn y_max(&self) -> f32 {
        self.y + self.h
    }
}

/// Coalesce per-line [`PdfTextRun`]s into paragraph-level
/// [`PdfParagraph`]s. See module docs for the merge predicate and
/// algorithmic complexity.
///
/// Runs that lack positional information (`x.is_none() || y.is_none()`)
/// or text (`text.is_empty()`) are silently dropped — they have no
/// place in a layout-engine input. Runs are partitioned by
/// `page_index` so a multi-page slice produces paragraphs that never
/// span pages.
pub fn merge_paragraphs(runs: &[PdfTextRun]) -> Vec<PdfParagraph> {
    if runs.is_empty() {
        return Vec::new();
    }

    // Project usable runs into the working shape. `(run_idx, page,
    // x, y, w, h, fs, text)` — keep run_idx so the output's
    // `line_run_indices` points back into the caller's slice.
    let mut items: Vec<ParagraphItem> = Vec::new();
    for (i, r) in runs.iter().enumerate() {
        let (Some(x), Some(y)) = (r.x, r.y) else {
            continue;
        };
        if r.text.is_empty() {
            continue;
        }
        let fs = r.font_size.unwrap_or(10.0);
        // pdf-fast `w` is 0.5 em * char count when not given; height
        // defaults to font_size. Match the Python harness so the
        // ported algorithm gates fire on the same widths.
        let w =
            r.w.unwrap_or_else(|| (r.text.chars().count() as f32 * 0.5 * fs).max(1.0));
        let h = r.h.unwrap_or(fs);
        items.push((i, r.page_index, x, y, x + w, y + h, fs, r.text.clone()));
    }

    if items.is_empty() {
        return Vec::new();
    }

    // Partition by page so paragraphs never cross page boundaries.
    let mut by_page: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (idx_in_items, &(_, page, _, _, _, _, _, _)) in items.iter().enumerate() {
        by_page.entry(page).or_default().push(idx_in_items);
    }

    let mut all_paragraphs: Vec<PdfParagraph> = Vec::new();
    for (_page, page_idxs) in by_page {
        let mut page_paragraphs = merge_one_page(&items, &page_idxs);
        all_paragraphs.append(&mut page_paragraphs);
    }

    // Re-id 0..N-1 globally after sorting (page, y_min, x_min). This
    // matches the Python contract on single-page input and extends
    // sensibly to multi-page input.
    all_paragraphs.sort_by(|a, b| {
        a.page_index
            .cmp(&b.page_index)
            .then_with(|| a.y.total_cmp(&b.y))
            .then_with(|| a.x.total_cmp(&b.x))
    });
    for (i, p) in all_paragraphs.iter_mut().enumerate() {
        p.id = i;
    }
    all_paragraphs
}

/// Inner per-page greedy left-fold. `items` is the full projection;
/// `page_idxs` are the indices of items on this page. Result is
/// unsorted at the global level — outer function sorts and re-ids.
fn merge_one_page(items: &[ParagraphItem], page_idxs: &[usize]) -> Vec<PdfParagraph> {
    let n = page_idxs.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        // Single run becomes a single paragraph carrying its own
        // provenance — matches the Python single-block short-circuit
        // that the Bugbot LOW on #118 forced into shape.
        let (run_idx, page, x, y, x_end, y_end, fs, ref text) = items[page_idxs[0]];
        return vec![PdfParagraph {
            id: 0,
            page_index: page,
            text: text.clone(),
            font_size: fs,
            x,
            y,
            w: x_end - x,
            h: y_end - y,
            line_run_indices: vec![run_idx],
        }];
    }

    // Visit order: bucket x_min by 10pt then y_min ascending. Keeps
    // left-column runs grouped before right-column when their y bands
    // overlap, which is what makes the greedy left-fold prefer
    // vertical chaining within a column.
    let mut order: Vec<usize> = page_idxs.to_vec();
    order.sort_by(|&a, &b| {
        let xa = (items[a].2 / 10.0).round_ties_even() as i64;
        let xb = (items[b].2 / 10.0).round_ties_even() as i64;
        xa.cmp(&xb).then_with(|| items[a].3.total_cmp(&items[b].3))
    });

    let mut consumed = vec![false; items.len()];
    let mut merged: Vec<PdfParagraph> = Vec::new();

    for &start_item in &order {
        if consumed[start_item] {
            continue;
        }
        let mut chain: Vec<usize> = vec![start_item];
        consumed[start_item] = true;
        let (_, page, mut cx0, mut cy0, mut cx1, mut cy1, cfs, _) = items[start_item].clone();
        // Tip of the *current line* for same-line coalesce. The paragraph
        // union (cx*/cy*) grows across lines; horizontal search must use
        // the last line's right edge or a word under a previous line's
        // overhang looks "to the left" and is skipped.
        let mut line_x0 = cx0;
        let mut line_y0 = cy0;
        let mut line_x1 = cx1;
        let mut line_y1 = cy1;

        // Separators between chain members: `\n` for a vertical
        // (next-line) extension, ` ` for a horizontal (same-line)
        // extension. Indexed as separator *before* chain[i+1].
        let mut seps: Vec<&'static str> = Vec::new();

        loop {
            let cw = cx1 - cx0;
            // Prefer same-line (horizontal) before next-line (vertical).
            // WHY: word-level PDF streams emit "The" then "quick" on one
            // line and "brown" then "fox" on the next. Visiting by
            // x-bucket would otherwise chain "The"→"brown" vertically
            // and leave "quick"/"fox" as a parallel column — the
            // hardcore fragment residue. Finish the current line first.
            let (j, horizontal) = if let Some(h) = find_horizontal_successor(
                items, &order, &consumed, line_x0, line_y0, line_x1, line_y1, cfs,
            ) {
                (h, true)
            } else {
                // Find the smallest-gap valid successor downward.
                let mut best: Option<usize> = None;
                let mut best_gap = f32::INFINITY;
                for &j in &order {
                    if consumed[j] {
                        continue;
                    }
                    let (_, _, jx0, jy0, jx1, _jy1, jfs, _) = items[j];
                    // Gate (1): same font size within 10 %.
                    if (jfs - cfs).abs() > 0.1 * cfs.max(jfs).max(1.0) {
                        continue;
                    }
                    // Gate (2): same column. Either left edges close OR
                    // ≥ 50 % overlap of the narrower-width box.
                    let jw = jx1 - jx0;
                    let overlap = (cx1.min(jx1) - cx0.max(jx0)).max(0.0);
                    let narrower = cw.min(jw).max(1.0);
                    if (jx0 - cx0).abs() > 4.0 && overlap <= 0.5 * narrower {
                        continue;
                    }
                    // Gate (3): stacked, candidate sits below current
                    // within 1.5 × font_size.
                    let gap = jy0 - cy1;
                    if gap < -0.5 * cfs || gap > 1.5 * cfs {
                        continue;
                    }
                    if gap < best_gap {
                        best_gap = gap;
                        best = Some(j);
                    }
                }
                let Some(j) = best else {
                    break;
                };
                // Gate (4): no intervening unconsumed block in the column
                // band between (cy1, by0).
                let (_, _, bx0, by0, bx1, _, _, _) = items[j];
                let col_lo = cx0.min(bx0);
                let col_hi = cx1.max(bx1);
                let mut blocked = false;
                for &k in page_idxs {
                    if k == j || consumed[k] {
                        continue;
                    }
                    let (_, _, kx0, ky0, kx1, ky1, _, _) = items[k];
                    if ky0 > cy1 && ky1 < by0 && !(kx1 < col_lo || kx0 > col_hi) {
                        blocked = true;
                        break;
                    }
                }
                if blocked {
                    break;
                }
                (j, false)
            };

            // Extend the chain.
            chain.push(j);
            seps.push(if horizontal { " " } else { "\n" });
            consumed[j] = true;
            let (_, _, bx0, by0, bx_end, by_end, _, _) = items[j].clone();
            cx0 = cx0.min(bx0);
            cy0 = cy0.min(by0);
            cx1 = cx1.max(bx_end);
            cy1 = cy1.max(by_end);
            if horizontal {
                line_x0 = line_x0.min(bx0);
                line_y0 = line_y0.min(by0);
                line_x1 = line_x1.max(bx_end);
                line_y1 = line_y1.max(by_end);
            } else {
                // New line — reset the tip to this run alone.
                line_x0 = bx0;
                line_y0 = by0;
                line_x1 = bx_end;
                line_y1 = by_end;
            }
        }

        // Emit the chain. Vertical joins use `\n`; same-line joins use
        // a space. Bbox is the union; font_size is taken from the
        // chain's first run (matches the Python `blocks[chain[0]]["font_size"]`).
        let mut text = String::new();
        for (i, &idx) in chain.iter().enumerate() {
            if i > 0 {
                text.push_str(seps[i - 1]);
            }
            text.push_str(&items[idx].7);
        }
        let xs0 = chain
            .iter()
            .map(|&i| items[i].2)
            .fold(f32::INFINITY, f32::min);
        let ys0 = chain
            .iter()
            .map(|&i| items[i].3)
            .fold(f32::INFINITY, f32::min);
        let xs1 = chain
            .iter()
            .map(|&i| items[i].4)
            .fold(f32::NEG_INFINITY, f32::max);
        let ys1 = chain
            .iter()
            .map(|&i| items[i].5)
            .fold(f32::NEG_INFINITY, f32::max);
        let first = &items[chain[0]];
        merged.push(PdfParagraph {
            id: 0, // re-id'd globally by outer function
            page_index: page,
            text,
            font_size: first.6,
            x: xs0,
            y: ys0,
            w: xs1 - xs0,
            h: ys1 - ys0,
            line_run_indices: chain.into_iter().map(|i| items[i].0).collect(),
        });
    }

    merged
}

/// Closest same-line rightward word fragment that may extend the chain.
///
/// WHY: vertical-only merge left word-level PDF fragments (per-glyph /
/// per-word streams) as separate blocks — the 21-label hardcore residue
/// on olmOCR-bench `multi_column`. Same-line coalesce collapses those
/// into line/paragraph blocks so the geometric engines see coherent
/// regions instead of `font_size≈1` placeholders.
///
/// Safety: only word-like runs coalesce. A candidate wider than ~8 em,
/// or a current union already column-wide abutting another wide run,
/// is treated as a column edge (see `two_column_lines_do_not_cross_merge`).
#[allow(clippy::too_many_arguments)] // private helper; mirrors page tip + font args
fn find_horizontal_successor(
    items: &[ParagraphItem],
    order: &[usize],
    consumed: &[bool],
    cx0: f32,
    cy0: f32,
    cx1: f32,
    cy1: f32,
    cfs: f32,
) -> Option<usize> {
    let ccy = (cy0 + cy1) * 0.5;
    let fs = cfs.max(1.0);
    let cw = (cx1 - cx0).max(1.0);
    let mut best: Option<usize> = None;
    let mut best_gap = f32::INFINITY;
    for &j in order {
        if consumed[j] {
            continue;
        }
        let (_, _, jx0, jy0, jx1, jy1, jfs, _) = items[j];
        if (jfs - cfs).abs() > 0.1 * cfs.max(jfs).max(1.0) {
            continue;
        }
        let jcy = (jy0 + jy1) * 0.5;
        if (jcy - ccy).abs() > 0.5 * fs {
            continue;
        }
        let jw = (jx1 - jx0).max(1.0);
        // Word-like only: reject full column-width lines.
        if jw > 8.0 * fs {
            continue;
        }
        // Must sit to the right of the current union (allow tiny
        // negative gap for overlapping / kerning fragments).
        let gap = jx0 - cx1;
        if gap < -0.25 * fs || gap > 2.0 * fs {
            continue;
        }
        // Abutting wide columns (gap ≈ 0, both sides column-wide) must
        // stay separate — the classic two-column edge case.
        if cw > 8.0 * fs && gap < 1.0 {
            continue;
        }
        if gap < best_gap {
            best_gap = gap;
            best = Some(j);
        }
    }
    best
}

// ── Unit tests ────────────────────────────────────────────────────────
//
// Mirror the seven Python cases from
// `office-parsers-rs/layout-fast-core/bench/extract_labels.py`
// (`ParagraphMergeTests`). Each Rust case has a `// PY:` comment
// pointing at its Python sibling so future ports stay aligned.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tessellation::{ColorSpace, PdfColor, TextRenderMode};
    use crate::{DecodingPath, PdfTextRun};

    fn black() -> PdfColor {
        PdfColor {
            space: ColorSpace::DeviceGray,
            components: vec![0.0],
        }
    }

    fn run(id: usize, x: f32, y: f32, w: f32, fs: f32, text: &str) -> PdfTextRun {
        let _ = id; // ids come from slice index; argument keeps tests
                    // readable when several runs share the same slot
        PdfTextRun {
            page_index: 0,
            stream_index: 0,
            operator: "Tj".into(),
            text: text.into(),
            cmap_fallback_chars: 0,
            decoding_path: DecodingPath::ToUnicodeCMap,
            font: None,
            font_object: None,
            font_generation: None,
            font_size: Some(fs),
            x: Some(x),
            y: Some(y),
            w: Some(w),
            h: Some(fs),
            fill_color: black(),
            stroke_color: black(),
            render_mode: TextRenderMode::Fill,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scaling: 100.0,
            text_rise: 0.0,
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(merge_paragraphs(&[]).is_empty());
    }

    // PY: ParagraphMergeTests::test_single_block_page_preserves_provenance
    #[test]
    fn single_run_preserves_provenance() {
        let runs = vec![run(0, 0.0, 0.0, 100.0, 10.0, "alone")];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "alone");
        assert_eq!(merged[0].line_run_indices, vec![0]);
        assert_eq!(merged[0].id, 0);
    }

    // PY: ParagraphMergeTests::test_adjacent_lines_in_one_column_merge
    #[test]
    fn adjacent_lines_in_one_column_merge() {
        let runs = vec![
            run(0, 0.0, 0.0, 100.0, 10.0, "first"),
            run(1, 0.0, 11.0, 100.0, 10.0, "second"),
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "first\nsecond");
        // Bbox is the union: (0..100) × (0..21).
        assert_eq!(merged[0].x, 0.0);
        assert_eq!(merged[0].y, 0.0);
        assert_eq!(merged[0].w, 100.0);
        assert_eq!(merged[0].h, 21.0);
        let mut ids = merged[0].line_run_indices.clone();
        ids.sort();
        assert_eq!(ids, vec![0, 1]);
    }

    // PY: ParagraphMergeTests::test_two_column_lines_do_not_cross_merge
    #[test]
    fn two_column_lines_do_not_cross_merge() {
        // Left and right columns at the same y with a sub-1pt
        // overlap. The 50%-of-narrower-width gate (Codex P1 on #118)
        // must keep them in separate paragraphs.
        let left = run(0, 0.0, 0.0, 100.0, 10.0, "left");
        let right = run(1, 100.5, 0.0, 100.0, 10.0, "right");
        let merged = merge_paragraphs(&[left, right]);
        assert_eq!(merged.len(), 2, "got: {:?}", merged);
    }

    // PY: ParagraphMergeTests::test_paragraph_break_at_big_y_gap
    #[test]
    fn paragraph_break_at_big_y_gap() {
        let runs = vec![
            run(0, 0.0, 0.0, 100.0, 10.0, "para A"),
            run(1, 0.0, 50.0, 100.0, 10.0, "para B"), // 40pt gap > 1.5×10
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 2);
    }

    // PY: ParagraphMergeTests::test_different_font_size_breaks_merge
    #[test]
    fn different_font_size_breaks_merge() {
        let runs = vec![
            run(0, 0.0, 0.0, 100.0, 14.0, "Heading"),
            run(1, 0.0, 15.0, 100.0, 10.0, "body"),
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 2);
    }

    // PY: ParagraphMergeTests::test_re_id_is_stable_in_y_order
    #[test]
    fn re_id_is_stable_in_y_order() {
        // Lines provided bottom-then-top — output should be y-sorted
        // top-to-bottom and re-id'd 0..N-1.
        let runs = vec![
            run(0, 0.0, 30.0, 100.0, 10.0, "bottom"),
            run(1, 0.0, 0.0, 100.0, 10.0, "top"),
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].text, "top");
        assert_eq!(merged[1].text, "bottom");
        assert_eq!(merged.iter().map(|p| p.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn runs_without_positional_metadata_are_dropped() {
        // A run missing `x` or `y` can't be laid out and must be
        // silently dropped rather than corrupt the merge.
        let mut bad = run(0, 0.0, 0.0, 100.0, 10.0, "ghost");
        bad.x = None;
        let good = run(1, 0.0, 0.0, 100.0, 10.0, "real");
        let merged = merge_paragraphs(&[bad, good]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "real");
        // Provenance points at the original index of the kept run
        // (1), not the index in the filtered list.
        assert_eq!(merged[0].line_run_indices, vec![1]);
    }

    #[test]
    fn multi_page_runs_do_not_merge_across_pages() {
        let mut p0 = run(0, 0.0, 0.0, 100.0, 10.0, "page 0 line 1");
        p0.page_index = 0;
        let mut p1 = run(1, 0.0, 11.0, 100.0, 10.0, "page 1 line 1");
        p1.page_index = 1;
        let merged = merge_paragraphs(&[p0, p1]);
        assert_eq!(merged.len(), 2);
        // Re-id stays dense across pages.
        assert_eq!(merged[0].id, 0);
        assert_eq!(merged[1].id, 1);
        assert_eq!(merged[0].page_index, 0);
        assert_eq!(merged[1].page_index, 1);
    }

    /// WHY: word-level PDF streams (per-glyph / per-word Tj) leave
    /// same-line fragments that the vertical-only fold never joins.
    /// Those fragments are the hardcore residue past the 92.3% geometric
    /// ceiling — coalesce them into one line block so layout engines see
    /// coherent regions.
    #[test]
    fn same_line_word_fragments_coalesce() {
        let runs = vec![
            run(0, 0.0, 0.0, 20.0, 10.0, "Hello"),
            run(1, 22.0, 0.0, 18.0, 10.0, "world"),
            run(2, 42.0, 0.5, 15.0, 10.0, "again"),
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 1, "got: {:?}", merged);
        assert_eq!(merged[0].text, "Hello world again");
        assert!((merged[0].w - 57.0).abs() < 0.01, "w={}", merged[0].w);
        let mut ids = merged[0].line_run_indices.clone();
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    /// WHY: a column gutter at the same y-band must NOT be treated as a
    /// word gap — otherwise two-column pages collapse into one block and
    /// the recursive arm loses its column cut.
    #[test]
    fn same_line_column_gutter_does_not_coalesce() {
        let left = run(0, 0.0, 0.0, 100.0, 10.0, "left col");
        let right = run(1, 160.0, 0.0, 100.0, 10.0, "right col"); // 60pt gutter
        let merged = merge_paragraphs(&[left, right]);
        assert_eq!(merged.len(), 2, "got: {:?}", merged);
    }

    /// WHY: same-line coalesce then vertical fold must still build a
    /// multi-line paragraph — fragments on line 1 + line 2 become one
    /// paragraph, not four blocks.
    #[test]
    fn same_line_then_vertical_builds_paragraph() {
        let runs = vec![
            run(0, 0.0, 0.0, 20.0, 10.0, "The"),
            run(1, 22.0, 0.0, 25.0, 10.0, "quick"),
            run(2, 0.0, 11.0, 20.0, 10.0, "brown"),
            run(3, 22.0, 11.0, 20.0, 10.0, "fox"),
        ];
        let merged = merge_paragraphs(&runs);
        assert_eq!(merged.len(), 1, "got: {:?}", merged);
        assert_eq!(merged[0].text, "The quick\nbrown fox");
    }
}
