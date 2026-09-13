//! Vector-glyph extraction from PDF content streams.
//!
//! Walks the content-stream operator sequence (`q`/`Q` save/restore, `cm`
//! transforms, `m`/`l`/`c`/`v`/`y`/`h` path ops) to recover the glyph
//! outlines a typesetter laid down on the page. Each rendered glyph becomes a
//! [`VectorGlyph`] (normalized contours + a stable `shape_hash`) grouped into
//! page-local [`GlyphRun`]s for the table/text reconstruction layer above.
//!
//! With the `glyph-atlas` feature, [`learn_page_glyph_mappings`] correlates
//! these shape hashes against CMap-resolved (`/ToUnicode`) text so the
//! scanner can learn glyph→grapheme mappings for downstream pages that lack
//! a CMap.

use crate::*;
use anyhow::Result;
#[cfg(feature = "serde")]
use serde::Serialize;

/// 2×3 affine transform stored row-major as `[a b c d e f]`, matching the PDF
/// content-stream `cm` operand order.
pub type Matrix = [f32; 6];

/// Identity transform (no rotation, scale, or translation).
pub const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// Compose two 2×3 affine matrices in PDF convention: `a` applied first,
/// then `b`. Returns the combined transform.
pub fn compose(a: Matrix, b: Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

/// Apply an affine transform to a point in PDF space.
pub fn transform_pt(matrix: Matrix, x: f32, y: f32) -> (f32, f32) {
    (
        matrix[0] * x + matrix[2] * y + matrix[4],
        matrix[1] * x + matrix[3] * y + matrix[5],
    )
}

/// Uniform scale matrix, used to fold `Tf`'s size into the glyph transform.
fn scale_matrix(scale: f32) -> Matrix {
    [scale, 0.0, 0.0, scale, 0.0, 0.0]
}

/// Read a PDF literal string, returning its bytes and the index just past it.
///
/// Handles nested unescaped parentheses, backslash escapes, and a backslash at
/// end of line. A string that never closes is taken to end at the stream end
/// rather than failing the page: a malformed glyph stream should cost one
/// glyph, not the document.
fn read_literal_string(stream: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut i = start;
    while i < stream.len() {
        match stream[i] {
            b'\\' => {
                i += 1;
                if i >= stream.len() {
                    break;
                }
                match stream[i] {
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'\n' => {}
                    b'\r' => {
                        if stream.get(i + 1) == Some(&b'\n') {
                            i += 1;
                        }
                    }
                    // Octal escape: up to three digits.
                    d @ b'0'..=b'7' => {
                        let mut value = (d - b'0') as u32;
                        let mut digits = 1;
                        while digits < 3 {
                            match stream.get(i + 1) {
                                Some(&n @ b'0'..=b'7') => {
                                    value = value * 8 + (n - b'0') as u32;
                                    i += 1;
                                    digits += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(value as u8);
                    }
                    other => out.push(other),
                }
                i += 1;
            }
            b'(' => {
                depth += 1;
                if depth > 1 {
                    out.push(b'(');
                }
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return (out, i);
                }
                out.push(b')');
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    (out, stream.len())
}

/// Read a PDF hex string, returning its bytes and the index just past it.
fn read_hex_string(stream: &[u8], start: usize) -> (Vec<u8>, usize) {
    let mut nibbles: Vec<u8> = Vec::new();
    let mut i = start + 1;
    while i < stream.len() && stream[i] != b'>' {
        let b = stream[i];
        let value = match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        };
        if let Some(v) = value {
            nibbles.push(v);
        }
        i += 1;
    }
    let past = if i < stream.len() { i + 1 } else { i };
    // An odd final digit is padded with 0, per the spec.
    let mut out = Vec::with_capacity(nibbles.len() / 2 + 1);
    for pair in nibbles.chunks(2) {
        let hi = pair[0];
        let lo = pair.get(1).copied().unwrap_or(0);
        out.push((hi << 4) | lo);
    }
    (out, past)
}



/// One block of glyphs sharing the same text-matrix block (between PDF
/// `q`/`Q` save/restore pairs). Positions are in page space with `page_y`
/// already flipped to a top-down origin.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct GlyphRun {
    /// Run origin x in page space (left edge).
    pub page_x: f32,
    /// Run origin y in page space (top-down: 0 at top of page).
    pub page_y: f32,
    /// Effective glyph scale recovered from the run's text matrix.
    pub scale: f32,
    /// Translation x recovered from the run's text matrix (PDF-space).
    pub tx: f32,
    /// Translation y recovered from the run's text matrix (PDF-space).
    pub ty: f32,
    /// Glyphs in the run, in left-to-right reading order.
    pub glyphs: Vec<VectorGlyph>,
}

/// A single vector glyph rebuilt from path ops. `shape_hash` is the
/// FNV-1a hash of the contours after translation+scale normalization, so
/// glyph shapes can be deduplicated across runs and pages.
///
/// `normalized` holds that normalized geometry, kept alongside the hash rather
/// than recomputed on demand. The hash alone cannot carry identity — see
/// [`crate::glyph_atlas`] — so a shape lookup needs the points, and recomputing
/// normalization at every lookup would both cost more and risk disagreeing with
/// the normalization the hash was taken from.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct VectorGlyph {
    /// Glyph left x-offset in run-local coordinates.
    pub x_offset: f32,
    /// Glyph contours; each inner `Vec<BezPt>` is one closed subpath.
    pub contours: Vec<Vec<BezPt>>,
    /// The same contours translated by `(-min_x, -min_y)` and divided by
    /// `max(width, height)`, so two renderings of one glyph shape agree within
    /// float noise. Empty when the glyph had no geometry.
    pub normalized: Vec<Vec<BezPt>>,
    /// Stable hash of the normalized glyph shape (0 means "no geometry").
    pub shape_hash: u64,
    /// Pre-normalization ink-bottom y of the glyph in run-local units
    /// (min contour y). `GlyphRun::page_y` is the RUN origin: on real
    /// documents runs on one visual line sit at origins scattered by several
    /// points while the contour y-offsets compensate, so line banding must
    /// use `page_y - y_offset` (per-glyph ink position), not the run origin.
    pub y_offset: f32,
    /// Pre-normalization glyph extent, `max(width, height)` in run-local
    /// units — the scale factor divided out by normalization. Two instances
    /// of the same glyph at the same font size report the same `size`; the
    /// value is what lets identity clustering keep small degenerate glyphs
    /// (superscript digits flattened to a handful of points) apart from
    /// same-structured glyphs at other sizes.
    pub size: f32,
}

/// One operand of a `TJ` array.
///
/// `TJ` interleaves strings with numbers: the strings are shown, and each number
/// between them is a kerning adjustment in thousandths of an em applied to the
/// text matrix before the next string. Both have to be kept in order, which is
/// why the array is collected rather than treated as a run of strings.
#[derive(Debug, Clone)]
enum TjPart {
    /// A string operand to show.
    Text(Vec<u8>),
    /// A kerning adjustment in thousandths of an em; positive moves left.
    Kern(f32),
}

/// A font whose outlines this parser can reach.
///
/// Two subtypes qualify, for different reasons. A Type 3 font *is* its outlines
/// — each glyph is a content stream, so drawing them means executing them. An
/// embedded TrueType program carries its outlines in `glyf`, so drawing them
/// means emitting points. Everything else keeps its outlines in a form this
/// crate does not read, and the atlas gets nothing rather than an approximation.
#[derive(Debug, Clone)]
pub(crate) enum DrawableFont {
    /// Glyphs are content streams in `/CharProcs`.
    Type3(Type3Font),
    /// Glyphs are outlines in an embedded `glyf` table.
    TrueType(crate::truetype::TrueTypeFont),
}

/// Where one glyph's outline comes from.
enum GlyphSource {
    /// A Type 3 glyph: a content stream to execute.
    Stream(Vec<u8>),
    /// A TrueType glyph: already-parsed points in font units.
    Outline(crate::truetype::GlyphOutline),
}

/// Font matrix for a TrueType program: font units to text space.
///
/// TrueType outlines are in design units, so the matrix is a uniform scale by
/// the reciprocal of the em size — the same shape a Type 3 `/FontMatrix`
/// declares explicitly.
fn true_type_matrix(units_per_em: f32) -> Matrix {
    let scale = if units_per_em > 0.0 {
        1.0 / units_per_em
    } else {
        0.0
    };
    [scale, 0.0, 0.0, scale, 0.0, 0.0]
}

/// A Type 3 font resolved far enough to draw its glyphs.
///
/// A Type 3 font defines each glyph as a content stream in `/CharProcs` and
/// shows it with the ordinary text operators, so its outlines are reachable
/// only by resolving the text operators and executing those streams. Nothing
/// else in this parser does that: the semantic pass reads the characters, but
/// with no `/ToUnicode` it decodes byte codes directly and reports every one as
/// unverified, and a font program is never consulted for outlines.
///
/// Only the narrow path needed to draw a glyph is resolved. `/Widths` is read
/// because text must advance; `/Encoding /Differences` is read because it is
/// how a code reaches a glyph name. A font this parser cannot resolve draws
/// nothing rather than drawing at a guessed position.
#[derive(Debug, Default, Clone)]
pub(crate) struct Type3Font {
    /// Character code -> glyph name, from `/Encoding /Differences`.
    pub(crate) differences: HashMap<u8, String>,
    /// Glyph name -> the stream that draws it.
    pub(crate) char_procs: HashMap<String, Vec<u8>>,
    /// Font matrix, mapping glyph space to text space.
    pub(crate) font_matrix: Matrix,
    /// `/FirstChar` and the widths that follow it; absent widths advance by 0
    /// so a font without `/Widths` still draws, just without spacing.
    pub(crate) first_char: u8,
    pub(crate) widths: Vec<f32>,
}

impl Type3Font {
    /// The glyph name for a character code, if `/Differences` names one.
    fn glyph_name(&self, code: u8) -> Option<&str> {
        self.differences.get(&code).map(String::as_str)
    }

    /// The stream that draws a glyph, if the font defines one.
    fn char_proc(&self, name: &str) -> Option<&[u8]> {
        self.char_procs.get(name).map(Vec::as_slice)
    }

    /// Advance for a character code, in glyph space.
    fn width(&self, code: u8) -> f32 {
        let index = code.wrapping_sub(self.first_char) as usize;
        self.widths.get(index).copied().unwrap_or(0.0)
    }
}

/// One point on a glyph contour. `kind` encodes the role of the point:
/// `0` = moveto, `1` = lineto, `2` = Bézier control or endpoint.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct BezPt {
    /// X coordinate in run-local space.
    pub x: f32,
    /// Y coordinate in run-local space.
    pub y: f32,
    /// Point kind: 0 = moveto, 1 = lineto, 2 = Bézier control / endpoint.
    pub kind: u8,
}

struct VectorTextExtractor {
    page_height: f32,
    skip_first_cm: bool,
    stack: Vec<Matrix>,
    runs: Vec<GlyphRun>,

    in_run: bool,
    trans_cms: usize,
    block_entry: Matrix,

    current_scale: f32,
    current_tx: f32,
    current_ty: f32,

    glyphs: Vec<VectorGlyph>,
    current_contour: Vec<BezPt>,
    /// Contours of the path being constructed, held until the paint op
    /// reveals whether they are ink or invisible (white) geometry.
    pending: Vec<Vec<BezPt>>,
    /// Approximate luminance of the current non-stroking / stroking color
    /// (0 = black ink, 1 = white paper). Content streams that never set a
    /// color default to black, per the PDF graphics state.
    fill_lum: f32,
    stroke_lum: f32,
    /// Most recent name token (`/Fm0`), consumed by `Do` to resolve the
    /// Form XObject being invoked.
    last_name: Option<Vec<u8>>,
    /// Most recent string operand, consumed by `Tj` and its relatives.
    last_string: Option<Vec<u8>>,
    /// Operands of the `TJ` array currently being read, in order.
    tj_parts: Vec<TjPart>,
    /// Depth inside a `[` ... `]` operand array. Only `TJ` uses one.
    in_array: bool,
    /// Transform that maps the glyph stream currently executing into page
    /// user space (`CTM . Tm . Fm`). Contours are recorded through it, so a
    /// text-drawn glyph arrives in the same space as one drawn by a path on the
    /// page — the flush path already knows how to place those.
    type3_ctm: Matrix,
    /// Set while executing a Type 3 glyph stream.
    ///
    /// Text-drawn glyphs are positioned by the text matrix, so they are not the
    /// absolute-coordinate shapes the no-run flush branch was written for. That
    /// branch keeps only punctuation-scale ink (<= 3.5pt) because its transform
    /// model is a heuristic, and it would silently discard every Type 3 glyph at
    /// any real font size — measured: 12pt text produced 7.2pt outlines, all
    /// dropped, which is why outline extraction appeared to produce nothing.
    in_type3_glyph: bool,
    /// Active indirect Form objects, used to reject cycles and bound nested
    /// traversal before materializing another stream.
    form_stack: Vec<(u32, u16)>,

    /// Text object state, live only between `BT` and `ET`.
    ///
    /// The extractor's other inputs are painted paths, which carry their own
    /// coordinates. Text carries none: a glyph's position is the product of the
    /// text matrix, the font matrix, and the CTM, and it advances per glyph. So
    /// this state has to exist to place a Type 3 glyph where the rasterizer
    /// would put it.
    text_matrix: Matrix,
    text_line_matrix: Matrix,
    /// The `Tf`-selected font, when it is one whose outlines this parser can
    /// reach: a Type 3 font (outlines are content streams) or an embedded
    /// TrueType program (outlines are in `glyf`). `None` for a font whose
    /// outlines live in CFF, or that embeds no program at all — for those there
    /// is nothing here to draw, and guessing at a shape is worse than drawing
    /// nothing.
    drawable: Option<DrawableFont>,
    /// Font size from `Tf`, scaling the font matrix.
    font_size: f32,
    in_text: bool,

    win: [f32; 6],
}

/// Document context for Form XObject traversal. Without it (unit tests over
/// raw byte streams), `Do` is a no-op — exactly the old behavior.
pub(crate) struct XObjectCtx<'a> {
    pub doc: &'a PdfDocument,
    pub resources: Option<&'a PdfDictionary>,
    pub depth: usize,
}

impl VectorTextExtractor {
    fn new(base_ctm: Matrix, page_height: f32) -> Self {
        Self {
            page_height,
            skip_first_cm: false,
            stack: vec![base_ctm],
            runs: Vec::new(),
            in_run: false,
            trans_cms: 0,
            block_entry: IDENTITY,
            current_scale: 1.0,
            current_tx: 0.0,
            current_ty: 0.0,
            glyphs: Vec::new(),
            current_contour: Vec::new(),
            pending: Vec::new(),
            fill_lum: 0.0,
            stroke_lum: 0.0,
            last_name: None,
            last_string: None,
            tj_parts: Vec::new(),
            in_array: false,
            in_type3_glyph: false,
            type3_ctm: IDENTITY,
            form_stack: Vec::new(),
            text_matrix: IDENTITY,
            text_line_matrix: IDENTITY,
            drawable: None,
            font_size: 0.0,
            in_text: false,
            win: [0.0; 6],
        }
    }

    fn flush(&mut self) {
        self.finalize_contour();
        // Paths still unpainted at a run boundary keep the old committed
        // behavior, gated only by the current fill color.
        self.commit_pending(self.fill_lum);

        // Glyphs drawn OUTSIDE a translate-cm "run" (absolute path
        // coordinates, no positioning cm) used to be discarded here — which
        // is where a DocuSign contract's punctuation died: every `.` `/` `-`
        // is emitted as a tiny absolute-coordinate path that the rasterizer
        // paints but this extractor dropped at flush.
        if (self.in_run || self.in_type3_glyph) && !self.glyphs.is_empty() {
            let (px, py_pdf) = transform_pt(self.block_entry, self.current_tx, self.current_ty);
            let page_y = self.page_height - py_pdf;

            self.normalize_and_hash_glyphs();

            self.runs.push(GlyphRun {
                page_x: px,
                page_y,
                scale: self.current_scale,
                tx: self.current_tx,
                ty: self.current_ty,
                glyphs: std::mem::take(&mut self.glyphs),
            });
        } else if !self.glyphs.is_empty() {
            // No positioning run: contours are in ABSOLUTE user space with
            // PDF's y-up orientation, so the in-run offset convention flips
            // sign (measured: naive reuse placed glyphs at y up to 1463 on an
            // 842pt page). Place each glyph the way the rasterizer would:
            // transform its contours through the current CTM, anchor the run
            // at (0, page_height), and store per-glyph offsets from the
            // transformed bbox — ink-left for x, negated ink-top for y so
            // `page_y + y_offset` is top-down ink-top like every other glyph.
            let ctm = *self.stack.last().unwrap_or(&IDENTITY);
            let mut anchors = Vec::with_capacity(self.glyphs.len());
            for g in &mut self.glyphs {
                let mut min_x = f32::MAX;
                let mut max_x = f32::MIN;
                let mut min_y = f32::MAX;
                let mut max_y = f32::MIN;
                for contour in &mut g.contours {
                    for pt in contour.iter_mut() {
                        let (tx, ty) = transform_pt(ctm, pt.x, pt.y);
                        pt.x = tx;
                        pt.y = ty;
                        min_x = min_x.min(tx);
                        max_x = max_x.max(tx);
                        min_y = min_y.min(ty);
                        max_y = max_y.max(ty);
                    }
                }
                anchors.push((min_x, max_y, (max_x - min_x).max(max_y - min_y)));
            }
            // Punctuation-scale ONLY. The extractor's transform model is a
            // heuristic, and for some no-run blocks the flush-time CTM is not
            // the drawing CTM — letter-scale no-run glyphs then land inside
            // foreign lines and poison banding (measured: "Ate" fragments
            // interleaved into a CNPJ row, CER 30%→41%). Dots, commas,
            // hyphens and slashes — the class that was actually missing — are
            // ≤ ~3.5pt and place correctly; anything larger waits until
            // transform tracking is interpreter-grade.
            let keep: Vec<bool> = anchors.iter().map(|a| a.2 <= 3.5).collect();
            let mut it = keep.iter();
            self.glyphs.retain(|_| *it.next().unwrap());
            let anchors: Vec<(f32, f32)> = anchors
                .into_iter()
                .zip(keep)
                .filter_map(|(a, k)| k.then_some((a.0, a.1)))
                .collect();
            self.normalize_and_hash_glyphs();
            for (g, (min_x, max_y)) in self.glyphs.iter_mut().zip(anchors) {
                g.x_offset = min_x;
                g.y_offset = -max_y;
            }
            self.runs.push(GlyphRun {
                page_x: 0.0,
                page_y: self.page_height,
                scale: self.current_scale,
                tx: 0.0,
                ty: 0.0,
                glyphs: std::mem::take(&mut self.glyphs),
            });
        }
        self.in_run = false;
        self.trans_cms = 0;
        self.glyphs.clear();
        self.current_contour.clear();
    }

    fn finalize_contour(&mut self) {
        if self.current_contour.is_empty() {
            return;
        }
        let contour = std::mem::take(&mut self.current_contour);
        self.pending.push(contour);
    }

    /// Commit (or drop) the current path's contours at PAINT time, when the
    /// fill/stroke color is finally known. White-ish paint (`lum > 0.85`) is
    /// paper, not ink: DocuSign-class PDFs carry white-filled duplicate text
    /// layers that the rasterizer paints invisibly but that used to enter the
    /// glyph stream, interleaving phantom glyphs into real lines.
    fn commit_pending(&mut self, lum: f32) {
        if lum > 0.85 {
            self.pending.clear();
            return;
        }
        let into_page = self.in_type3_glyph.then_some(self.type3_ctm);
        for mut contour in std::mem::take(&mut self.pending) {
            // A glyph stream draws in glyph space: its `0 0 m 600 0 l` means
            // "this outline in the font's own units", not a point on the page.
            // Paths drawn directly on a page are already in user space, so only
            // the glyph case is transformed here.
            if let Some(ctm) = into_page {
                for pt in contour.iter_mut() {
                    let (x, y) = transform_pt(ctm, pt.x, pt.y);
                    pt.x = x;
                    pt.y = y;
                }
            }
            self.merge_contour(contour);
        }
    }

    fn merge_contour(&mut self, contour: Vec<BezPt>) {
        let (c_min, c_max) = x_bounds_contour(&contour);

        if let Some(last) = self.glyphs.last_mut() {
            let (l_min, l_max) = x_bounds_contours(&last.contours);
            let overlap = (c_max.min(l_max) - c_min.max(l_min)).max(0.0);
            let cw = (c_max - c_min).max(0.001);
            let lw = (l_max - l_min).max(0.001);

            // SIGNIFICANT OVERLAP: "the X-span significantly overlaps"
            if overlap > 0.05 * cw || overlap > 0.05 * lw {
                last.contours.push(contour);
                return;
            }
        }

        self.glyphs.push(VectorGlyph {
            x_offset: c_min,
            contours: vec![contour],
            normalized: Vec::new(),
            shape_hash: 0,
            y_offset: 0.0,
            size: 0.0,
        });
    }

    fn normalize_and_hash_glyphs(&mut self) {
        for g in &mut self.glyphs {
            let (mut min_x, mut max_x) = (f32::MAX, f32::MIN);
            let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
            for contour in &g.contours {
                for pt in contour {
                    min_x = min_x.min(pt.x);
                    max_x = max_x.max(pt.x);
                    min_y = min_y.min(pt.y);
                    max_y = max_y.max(pt.y);
                }
            }

            let w = (max_x - min_x).max(0.0001);
            let h = (max_y - min_y).max(0.0001);
            let scale_factor = w.max(h);
            g.size = scale_factor;
            g.y_offset = min_y;

            let mut hash = 14695981039346656037u64;
            let fnv_prime = 1099511628211u64;

            for contour in &mut g.contours {
                for pt in contour {
                    pt.x = (pt.x - min_x) / scale_factor;
                    pt.y = (pt.y - min_y) / scale_factor;

                    let qx = (pt.x * 255.0).clamp(0.0, 255.0) as u8;
                    let qy = (pt.y * 255.0).clamp(0.0, 255.0) as u8;
                    let qk = pt.kind;

                    hash ^= qx as u64;
                    hash = hash.wrapping_mul(fnv_prime);
                    hash ^= qy as u64;
                    hash = hash.wrapping_mul(fnv_prime);
                    hash ^= qk as u64;
                    hash = hash.wrapping_mul(fnv_prime);
                }
            }
            g.shape_hash = hash;
            // `g.contours` is normalized in place above, so this is a copy of
            // exactly the points the hash was taken from — not a recomputation
            // that could drift from them.
            g.normalized = g.contours.clone();
        }
    }

    fn apply_cm(&mut self, cm_matrix: Matrix) {
        if cm_matrix[0] == 1.0 && cm_matrix[1] == 0.0 && cm_matrix[2] == 0.0 && cm_matrix[3] == 1.0
        {
            // pure translate
            self.trans_cms += 1;
            if self.trans_cms == 1 {
                self.in_run = true;
                self.current_tx = cm_matrix[4];
                self.current_ty = cm_matrix[5];
            }
        } else if cm_matrix[0] == 0.0 && cm_matrix[3] == 0.0 {
            // Assume rotate/scale matrix (0, -s, s, 0, 0, 0)
            self.current_scale = cm_matrix[2].abs().max(cm_matrix[1].abs());
        }

        if let Some(last) = self.stack.last_mut() {
            *last = compose(*last, cm_matrix);
        }
    }

    #[allow(dead_code)] // ctx-free entry kept for unit tests
    fn parse_stream(&mut self, stream: &[u8]) -> Result<()> {
        self.parse_stream_ctx(stream, None)
    }

    fn parse_stream_ctx(&mut self, stream: &[u8], ctx: Option<&XObjectCtx<'_>>) -> Result<()> {
        let mut cursor = 0;
        let mut num_cm_seen = 0;

        while cursor < stream.len() {
            while cursor < stream.len() && is_whitespace(stream[cursor]) {
                cursor += 1;
            }
            if cursor >= stream.len() {
                break;
            }

            // String operands are read delimiters-first. The whitespace token
            // scan below cannot see them: a literal string may contain spaces
            // and escaped parentheses, so `(A B)` would arrive as two tokens
            // and `(a\)b)` would end early. Only `Tj` and its relatives need
            // this, but the reader has to be here, ahead of the token split.
            // `[` is a PDF delimiter, not whitespace: `[(A) -2000 (B)]` has no
            // space between the bracket and the string, so a tokenizer that
            // only splits on whitespace sees `[(A)` as one token and never
            // recognizes the array at all. Consuming it here is what makes the
            // array reachable; `]` needs no handling because it trails the last
            // operand, which is already read by its own branch.
            if stream[cursor] == b'[' {
                self.in_array = true;
                self.tj_parts.clear();
                cursor += 1;
                continue;
            }
            if stream[cursor] == b'(' {
                let (bytes, next) = read_literal_string(stream, cursor);
                self.win.copy_within(1..6, 0);
                self.win[5] = 0.0;
                if self.in_array {
                    self.tj_parts.push(TjPart::Text(bytes));
                } else {
                    self.last_string = Some(bytes);
                }
                cursor = next;
                continue;
            }
            if stream[cursor] == b'<'
                && stream.get(cursor + 1).is_some_and(|b| *b != b'<')
            {
                let (bytes, next) = read_hex_string(stream, cursor);
                self.win.copy_within(1..6, 0);
                self.win[5] = 0.0;
                if self.in_array {
                    self.tj_parts.push(TjPart::Text(bytes));
                } else {
                    self.last_string = Some(bytes);
                }
                cursor = next;
                continue;
            }

            let start = cursor;
            while cursor < stream.len() && !is_whitespace(stream[cursor]) {
                // Stop at a delimiter so `(B)]` and `]TJ` do not become one
                // token. `(` and `[` are handled above, before the scan starts.
                if stream[cursor] == b']' && cursor > start {
                    break;
                }
                cursor += 1;
            }
            let tok = &stream[start..cursor];
            // A lone `]` closes the operand list; the operator after it
            // consumes what was collected, so nothing to do here.
            if tok == b"]" {
                if self.in_array {
                    self.in_array = false;
                }
                continue;
            }

            if let Some(v) = fast_parse_f32(tok) {
                // Inside a `TJ` array a number is a kerning operand, not a
                // text-state one: pushing it through `win` would let a later
                // operator read it as its argument.
                if self.in_array {
                    self.tj_parts.push(TjPart::Kern(v));
                    continue;
                }
                self.win.copy_within(1..6, 0);
                self.win[5] = v;
                continue;
            }

            if tok.first() == Some(&b'/') && tok.len() > 1 {
                self.last_name = Some(tok[1..].to_vec());
            }

            match tok {
                b"q" => {
                    self.flush();
                    let current = *self.stack.last().unwrap_or(&IDENTITY);
                    self.stack.push(current);
                    self.block_entry = current; // Context before cm
                }
                b"Q" => {
                    self.flush();
                    if self.stack.len() > 1 {
                        self.stack.pop();
                    }
                }
                b"cm" => {
                    num_cm_seen += 1;
                    if num_cm_seen == 1 && self.skip_first_cm && ctx.is_none_or(|c| c.depth == 0) {
                        continue;
                    }

                    let cm_matrix = [
                        self.win[0],
                        self.win[1],
                        self.win[2],
                        self.win[3],
                        self.win[4],
                        self.win[5],
                    ];
                    self.apply_cm(cm_matrix);
                }
                b"m" => {
                    self.finalize_contour();
                    self.current_contour.push(BezPt {
                        x: self.win[4],
                        y: self.win[5],
                        kind: 0,
                    });
                }
                b"l" => {
                    self.current_contour.push(BezPt {
                        x: self.win[4],
                        y: self.win[5],
                        kind: 1,
                    });
                }
                b"c" => {
                    // 3 points
                    self.current_contour.push(BezPt {
                        x: self.win[0],
                        y: self.win[1],
                        kind: 2,
                    }); // cp1
                    self.current_contour.push(BezPt {
                        x: self.win[2],
                        y: self.win[3],
                        kind: 2,
                    }); // cp2
                    self.current_contour.push(BezPt {
                        x: self.win[4],
                        y: self.win[5],
                        kind: 2,
                    }); // end pt
                }
                b"v" => {
                    // cp2 and end
                    self.current_contour.push(BezPt {
                        x: self.win[2],
                        y: self.win[3],
                        kind: 2,
                    }); // cp2
                    self.current_contour.push(BezPt {
                        x: self.win[4],
                        y: self.win[5],
                        kind: 2,
                    }); // end
                }
                b"y" => {
                    // cp1 and end
                    self.current_contour.push(BezPt {
                        x: self.win[2],
                        y: self.win[3],
                        kind: 2,
                    }); // cp1
                    self.current_contour.push(BezPt {
                        x: self.win[4],
                        y: self.win[5],
                        kind: 2,
                    }); // end
                }
                b"re" => {
                    // Rectangle op: `x y w h re`. Periods, hyphens, slash
                    // bars and table rules are commonly emitted as `re f`
                    // instead of explicit path ops — without this, every
                    // thin punctuation glyph silently never enters the
                    // vector stream (measured: a CNPJ table row extracted
                    // 18 glyphs for 23 characters, the missing 5 being
                    // exactly `. . . / -`).
                    self.finalize_contour();
                    let (x, y, w, h) = (self.win[2], self.win[3], self.win[4], self.win[5]);
                    self.current_contour.push(BezPt { x, y, kind: 0 });
                    self.current_contour.push(BezPt {
                        x: x + w,
                        y,
                        kind: 1,
                    });
                    self.current_contour.push(BezPt {
                        x: x + w,
                        y: y + h,
                        kind: 1,
                    });
                    self.current_contour.push(BezPt {
                        x,
                        y: y + h,
                        kind: 1,
                    });
                    self.current_contour.push(BezPt { x, y, kind: 1 });
                    self.finalize_contour();
                }
                b"h" => {
                    if let Some(first) = self.current_contour.first().copied() {
                        self.current_contour.push(BezPt {
                            x: first.x,
                            y: first.y,
                            kind: 1,
                        });
                    }
                    self.finalize_contour();
                }
                b"f" | b"f*" | b"F" | b"B" | b"B*" | b"b" | b"b*" => {
                    self.finalize_contour();
                    self.commit_pending(self.fill_lum);
                }
                b"S" | b"s" => {
                    self.finalize_contour();
                    self.commit_pending(self.stroke_lum);
                }
                b"n" => {
                    // path painted with nothing (clip-path terminator): the
                    // geometry never becomes ink
                    self.finalize_contour();
                    self.pending.clear();
                }
                // Non-stroking / stroking color: track approximate luminance so
                // the paint ops above can tell ink from white geometry. Args
                // are the trailing entries of the numeric window.
                b"g" => self.fill_lum = self.win[5].clamp(0.0, 1.0),
                b"G" => self.stroke_lum = self.win[5].clamp(0.0, 1.0),
                b"rg" => self.fill_lum = rgb_lum(self.win[3], self.win[4], self.win[5]),
                b"RG" => self.stroke_lum = rgb_lum(self.win[3], self.win[4], self.win[5]),
                b"k" => {
                    self.fill_lum = cmyk_lum(self.win[2], self.win[3], self.win[4], self.win[5]);
                }
                b"K" => {
                    self.stroke_lum = cmyk_lum(self.win[2], self.win[3], self.win[4], self.win[5]);
                }
                b"Do" => {
                    // Form XObject invocation. Punctuation and other small
                    // glyphs are routinely emitted inside forms (measured on
                    // a DocuSign contract: every `.` `/` `-` of a CNPJ table
                    // row lived in a form the rasterizer painted but this
                    // extractor never walked). Semantics mirror
                    // `q; /Matrix cm; <form stream>; Q`.
                    let name = self.last_name.take();
                    if let (Some(ctx), Some(name)) = (ctx, name) {
                        let name = String::from_utf8_lossy(&name).into_owned();
                        if let Some(form) = ctx.doc.resolve_form_xobject(
                            ctx.resources,
                            &name,
                            self.form_stack.as_slice(),
                        )? {
                            self.flush();
                            let current = *self.stack.last().unwrap_or(&IDENTITY);
                            self.stack.push(current);
                            self.block_entry = current;
                            if form.matrix != IDENTITY {
                                self.apply_cm(form.matrix);
                            }
                            let inner = XObjectCtx {
                                doc: ctx.doc,
                                resources: form.resources.as_ref().or(ctx.resources),
                                depth: ctx.depth + 1,
                            };
                            self.form_stack.push(form.object_id);
                            let nested_result = self.parse_stream_ctx(&form.bytes, Some(&inner));
                            self.form_stack.pop();
                            if nested_result.is_ok() {
                                self.flush();
                            }
                            if self.stack.len() > 1 {
                                self.stack.pop();
                            }
                            nested_result?;
                        }
                    }
                }
                b"W" | b"W*" => {
                    self.flush();
                }

                // ── Text objects ────────────────────────────────────────
                //
                // Added for Type 3 fonts, whose glyphs are content streams and
                // therefore outline-reachable. For every other font these
                // operators keep the text state and draw nothing, because a
                // font program is not an outline this extractor can read.
                b"BT" => {
                    self.flush();
                    self.in_text = true;
                    self.text_matrix = IDENTITY;
                    self.text_line_matrix = IDENTITY;
                }
                b"ET" => {
                    self.flush();
                    self.in_text = false;
                    self.drawable = None;
                }
                b"Tf" => {
                    self.font_size = self.win[5];
                    let name = self.last_name.take();
                    self.drawable = match (ctx, name) {
                        (Some(ctx), Some(name)) => {
                            let name = String::from_utf8_lossy(&name).into_owned();
                            ctx.doc.resolve_drawable_font(ctx.resources, &name)
                        }
                        _ => None,
                    };
                }
                // Td: translate the line matrix; TD also sets the leading,
                // which this extractor does not track because nothing here
                // needs it.
                b"Td" | b"TD" => {
                    let (tx, ty) = (self.win[4], self.win[5]);
                    self.text_line_matrix =
                        compose(self.text_line_matrix, [1.0, 0.0, 0.0, 1.0, tx, ty]);
                    self.text_matrix = self.text_line_matrix;
                }
                // T*: next line using the leading, which is only reachable when
                // TL set one; without it this is a no-op rather than a guess.
                b"T*" => {}
                b"Tm" => {
                    self.text_line_matrix = [
                        self.win[0],
                        self.win[1],
                        self.win[2],
                        self.win[3],
                        self.win[4],
                        self.win[5],
                    ];
                    self.text_matrix = self.text_line_matrix;
                }
                b"Tj" => {
                    if let Some(text) = self.last_string.take() {
                        self.show_text(&text);
                    }
                }
                b"'" => {
                    if let Some(text) = self.last_string.take() {
                        self.show_text(&text);
                    }
                }
                b"\"" => {
                    if let Some(text) = self.last_string.take() {
                        self.show_text(&text);
                    }
                }
                b"TJ" => {
                    // The array's strings were collected as they were read and
                    // the numbers between them are kerning adjustments, in
                    // thousandths of an em, applied to the text matrix. A
                    // positive adjustment moves *left*, per the spec.
                    let parts = std::mem::take(&mut self.tj_parts);
                    // Cleared here rather than at `]`, so a malformed array
                    // that never closes cannot leave every later number in the
                    // stream being read as a kerning operand.
                    self.in_array = false;
                    for part in parts {
                        match part {
                            TjPart::Text(bytes) => self.show_text(&bytes),
                            TjPart::Kern(amount) => {
                                self.kern(amount);
                            }
                        }
                    }
                }

                _ => {}
            }
        }
        self.flush();
        Ok(())
    }

    /// Apply a `TJ` kerning adjustment to the text matrix.
    ///
    /// The value is in thousandths of an em and is subtracted: a positive
    /// adjustment moves the next glyph left, per PDF 32000-1 §9.4.3. Text state
    /// only, so the shown strings stay put.
    fn kern(&mut self, amount: f32) {
        if self.drawable.is_none() || amount == 0.0 {
            return;
        }
        let shift = -(amount / 1000.0) * self.font_size;
        self.text_matrix = compose([1.0, 0.0, 0.0, 1.0, shift, 0.0], self.text_matrix);
    }

    /// Draw one shown string by executing each Type 3 glyph's own stream.
    ///
    /// A no-op unless `Tf` selected a Type 3 font this parser resolved. Codes
    /// that `/Encoding /Differences` does not name are skipped rather than
    /// guessed at: with no mapping there is no outline to draw, and inventing a
    /// position would put a fabricated glyph into the text.
    ///
    /// Each glyph is executed under `CTM · Tm · Fm`, pushed as a matrix so the
    /// glyph's own `m`/`l`/`c`/`f` operators run through the ordinary path
    /// handling below. That is what the rasterizer does, and it means the
    /// outline arrives in page space with no separate transform stage.
    fn show_text(&mut self, bytes: &[u8]) {
        let Some(font) = self.drawable.clone() else {
            return;
        };
        self.flush();

        for &code in bytes {
            // A Type 3 glyph is a content stream to execute; a TrueType glyph
            // is a point stream to emit. Both arrive in font units, y-up, and
            // both are then placed by the same `CTM * Tm * Fm` transform.
            let (outline, font_matrix) = match &font {
                DrawableFont::Type3(type3) => {
                    let Some(name) = type3.glyph_name(code) else {
                        continue;
                    };
                    let Some(proc_bytes) = type3.char_proc(name) else {
                        continue;
                    };
                    (GlyphSource::Stream(proc_bytes.to_vec()), type3.font_matrix)
                }
                DrawableFont::TrueType(program) => {
                    let Some(index) = program.glyph_index(code as u32) else {
                        continue;
                    };
                    let Some(outline) = program.outline(index) else {
                        continue;
                    };
                    (
                        GlyphSource::Outline(outline),
                        true_type_matrix(program.units_per_em()),
                    )
                }
            };

            let glyph_matrix = compose(
                compose(self.text_matrix, font_matrix),
                scale_matrix(self.font_size),
            );
            self.in_type3_glyph = true;
            let current = self.stack.last().copied().unwrap_or(IDENTITY);
            let composed = compose(current, glyph_matrix);
            self.type3_ctm = composed;
            self.stack.push(composed);
            self.block_entry = current;
            // A CharProc is a content stream. A failure inside one glyph must
            // not abandon the page: skip that glyph and keep the rest, which is
            // the same choice the `Do` path makes for an unreadable form.
            match &outline {
                GlyphSource::Stream(bytes) => {
                    let _ = self.parse_stream_ctx(bytes, None);
                }
                GlyphSource::Outline(parsed) => {
                    // Font units, y-up, straight into the pending contours the
                    // commit step transforms into page space.
                    for contour in &parsed.contours {
                        self.pending.push(
                            contour
                                .iter()
                                .map(|p| BezPt {
                                    x: p.x,
                                    y: p.y,
                                    // TrueType's off-curve points are quadratic
                                    // controls, the same role kind 2 carries
                                    // for Type 3 outlines.
                                    kind: if p.on_curve { 1 } else { 2 },
                                })
                                .collect(),
                        );
                    }
                    self.commit_pending(0.0);
                }
            }
            // Flushed while the flag is still set, so the glyph takes the
            // positioned branch. Clearing it first sent every text-drawn glyph
            // down the no-run path, where the punctuation-scale guard discards
            // it — the glyph vanished with no error and no empty result to
            // notice, only a wrong size in what survived.
            self.flush();
            self.in_type3_glyph = false;
            self.stack.pop();

            // Advance the text matrix by the glyph width, in text space:
            // Tm = translate(w * size, 0) · Tm.
            let advance = match &font {
                DrawableFont::Type3(type3) => type3.width(code),
                DrawableFont::TrueType(program) => program.advance_units(code as u32).unwrap_or(0.0),
            } * self.font_size;
            self.text_matrix = compose([1.0, 0.0, 0.0, 1.0, advance, 0.0], self.text_matrix);
        }
    }
}

/// Rec. 601 luminance of an RGB non-stroking color.
fn rgb_lum(r: f32, g: f32, b: f32) -> f32 {
    (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 1.0)
}

/// Approximate luminance of a CMYK color (1 = white paper, 0 = full ink).
fn cmyk_lum(c: f32, m: f32, y: f32, k: f32) -> f32 {
    (1.0 - (0.299 * c + 0.587 * m + 0.114 * y + k)).clamp(0.0, 1.0)
}

fn x_bounds_contour(contour: &[BezPt]) -> (f32, f32) {
    let mut min = f32::MAX;
    let mut max = f32::MIN;
    for pt in contour {
        min = min.min(pt.x);
        max = max.max(pt.x);
    }
    if min <= max {
        (min, max)
    } else {
        (0.0, 0.0)
    }
}

fn x_bounds_contours(contours: &[Vec<BezPt>]) -> (f32, f32) {
    let mut min = f32::MAX;
    let mut max = f32::MIN;
    for contour in contours {
        for pt in contour {
            min = min.min(pt.x);
            max = max.max(pt.x);
        }
    }
    if min <= max {
        (min, max)
    } else {
        (0.0, 0.0)
    }
}

fn is_whitespace(b: u8) -> bool {
    b == b' ' || b == b'\n' || b == b'\r' || b == b'\t' || b == 0 || b == 12 // PDF whitespaces
}

fn fast_parse_f32(tok: &[u8]) -> Option<f32> {
    std::str::from_utf8(tok)
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
}

/// Scan a content stream for its leading page-CTM `cm` operator and return
/// the recovered matrix, or [`IDENTITY`] if none is found.
pub fn detect_page_ctm(stream: &[u8]) -> Matrix {
    let mut cursor = 0;
    let mut win = [0.0; 6];

    while cursor < stream.len() {
        while cursor < stream.len() && is_whitespace(stream[cursor]) {
            cursor += 1;
        }
        if cursor >= stream.len() {
            break;
        }

        let start = cursor;
        while cursor < stream.len() && !is_whitespace(stream[cursor]) {
            cursor += 1;
        }
        let tok = &stream[start..cursor];

        if let Some(v) = fast_parse_f32(tok) {
            win.copy_within(1..6, 0);
            win[5] = v;
        } else if tok == b"cm" {
            let m = [win[0], win[1], win[2], win[3], win[4], win[5]];
            // We expect the page CTM to translate Y by some page_height (e.g. 841.92)
            if m[1] == 0.0 && m[2] == 0.0 {
                return m;
            }
        }
    }
    IDENTITY
}

/// Returns true if the first `cm` in `stream` matches `target` within a
/// small epsilon. Used to detect when the extractor should skip the page-CTM
/// transform (it has already been factored into the run's base matrix).
pub fn check_first_cm_matches(stream: &[u8], target: Matrix) -> bool {
    let ctm = detect_page_ctm(stream);
    // Compare floats with epsilon
    for i in 0..6 {
        if (ctm[i] - target[i]).abs() > 0.001 {
            return false;
        }
    }
    true
}

/// Extract vector glyph runs for a single PDF page, sorted top-to-bottom and
/// left-to-right (4-unit row buckets, x within bucket). Empty if no content
/// streams or no glyph geometry was emitted.
pub fn extract_page_vector_text_runs(doc: &PdfDocument, page: &PdfPage) -> Vec<GlyphRun> {
    extract_page_vector_text_runs_checked(doc, page).unwrap_or_default()
}

/// Checked page-level vector extraction. Unlike
/// [`extract_page_vector_text_runs`], governed resource and bounded Form
/// traversal failures are returned to the caller.
pub fn extract_page_vector_text_runs_checked(
    doc: &PdfDocument,
    page: &PdfPage,
) -> Result<Vec<GlyphRun>> {
    let mut all_runs = Vec::new();

    // Re-implemented to fetch streams similar to page_content_streams_for_raster
    let streams = doc.page_content_streams_for_raster(page)?;
    if streams.is_empty() {
        return Ok(all_runs);
    }

    let page_ctm = detect_page_ctm(&streams[0]);
    let page_height = page
        .page_box()
        .map(|b| (b[3] - b[1]).abs().max(1.0))
        .unwrap_or(841.92);

    let page_dict = doc
        .object_by_id(page.object_number, page.generation)
        .and_then(|object| object.dictionary.as_ref());
    let resources = page
        .resources
        .as_ref()
        .or_else(|| page_dict.and_then(|d| doc.resolve_inherited_resources(d)));
    for stream in streams {
        let mut extractor = VectorTextExtractor::new(page_ctm, page_height);
        extractor.skip_first_cm = check_first_cm_matches(&stream, page_ctm);
        let ctx = XObjectCtx {
            doc,
            resources,
            depth: 0,
        };
        extractor.parse_stream_ctx(&stream, Some(&ctx))?;
        all_runs.extend(extractor.runs);
    }

    // Sort rows top-to-bottom, columns left-to-right
    // Top-to-bottom = increasing Y if page_y is 0 at top, or maybe it's bottom up?
    // "page_y_topdown = 841.92 - py_pdf" -> meaning Y = 0 is top.
    all_runs.sort_by(|a, b| {
        let bucket_a = (a.page_y / 4.0).round() as i32;
        let bucket_b = (b.page_y / 4.0).round() as i32;
        bucket_a.cmp(&bucket_b).then_with(|| {
            a.page_x
                .partial_cmp(&b.page_x)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });

    Ok(all_runs)
}

/// Resolve a glyph through the shape atlas, or `None` when the atlas is not
/// compiled in (feature `glyph-atlas` disabled).
///
/// Takes the glyph's geometry, not just its hash. The hash cannot carry shape
/// identity — see [`crate::glyph_atlas`] — so a lookup keyed on it alone misses
/// glyphs the atlas has already learned, which is why recovery never worked.
///
/// Returns the mapped grapheme (multi-codepoint safe) rather than a single
/// `char` so ligatures and composed accents round-trip through the scanner.
#[inline]
fn atlas_lookup(glyph: &VectorGlyph) -> Option<String> {
    #[cfg(feature = "glyph-atlas")]
    {
        if glyph.normalized.is_empty() {
            return None;
        }
        crate::glyph_atlas::lookup_entry(
            glyph.shape_hash,
            &glyph.normalized,
            glyph.size,
            crate::glyph_atlas::DEFAULT_EPS,
        )
    }
    #[cfg(not(feature = "glyph-atlas"))]
    {
        let _ = glyph;
        None
    }
}

/// Summary of a learning pass: how many pairs were inspected, filtered, emitted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LearningStats {
    /// Glyph runs produced by the vector extractor for the page.
    pub glyph_runs: usize,
    /// Semantic (CMap-resolved) text runs for the page.
    pub semantic_runs: usize,
    /// Pairs (shape_hash, grapheme) emitted to the learning sink.
    pub emitted: usize,
    /// Pairs skipped because of whitespace, zero hash, or CMap fallback.
    pub skipped: usize,
    /// `true` when this page contributed no learning — counts imbalanced,
    /// CMap fell back, or no runs at all.
    pub nothing_to_learn: bool,
}

/// Correlate vector-glyph runs with semantic (CMap-resolved) text runs and
/// push high-confidence `(shape_hash → grapheme)` pairs into the process
/// learning sink (`crate::glyph_atlas::record_learned`).
///
/// # Correlation strategy
///
/// Both passes walk the same content stream ops, so the Nth `Tj`/`TJ`
/// produces the Nth run on each side — modulo `q`/`Q` save/restore flushes
/// on the vector side. We use two layers:
///
/// 1. **Index-aligned**: when `glyph_runs.len() == semantic_runs.len()` AND
///    every semantic run resolved fully via `/ToUnicode`
///    (`cmap_fallback_chars == 0`), pair them by index. For each aligned
///    pair where the glyph count equals the visible-char count, emit per-
///    character mappings.
///
/// 2. **Total-flatten fallback**: if the counts don't line up 1:1 but the
///    total glyph count equals the total visible-char count across the
///    page, pair by flattened index. Less precise but still high-signal
///    when /ToUnicode is uniformly reliable.
///
/// Pairs are skipped when the glyph's `shape_hash == 0` (no normalised
/// geometry), when the char is whitespace (the vector path does not emit
/// glyph shapes for spaces), or when CMap fallback occurred.
///
/// The emitted entries use `AtlasSource::Corpus` and a conservative 0.85
/// confidence — real ground-truth but subject to our correlation heuristic.
#[cfg(feature = "glyph-atlas")]
pub fn learn_page_glyph_mappings(doc: &PdfDocument, page: &PdfPage) -> LearningStats {
    learn_page_glyph_mappings_checked(doc, page).unwrap_or_else(|_| LearningStats {
        nothing_to_learn: true,
        ..LearningStats::default()
    })
}

/// Checked page glyph-learning surface. Governed vector extraction failures
/// abort before any mapping is promoted to the process learning sink.
#[cfg(feature = "glyph-atlas")]
pub fn learn_page_glyph_mappings_checked(
    doc: &PdfDocument,
    page: &PdfPage,
) -> Result<LearningStats> {
    use crate::glyph_atlas::{record_learned, AtlasSource};

    let glyph_runs = extract_page_vector_text_runs_checked(doc, page)?;
    let semantic_runs = doc.semantic_text_runs_for_page(page);
    let mut stats = LearningStats {
        glyph_runs: glyph_runs.len(),
        semantic_runs: semantic_runs.len(),
        ..Default::default()
    };
    if glyph_runs.is_empty() || semantic_runs.is_empty() {
        stats.nothing_to_learn = true;
        return Ok(stats);
    }

    // Any CMap fallback on the page poisons the whole page — if even one
    // char fell through to "byte as char" we can't trust the positional
    // alignment either, since the vector path skips glyphs that never
    // appeared in the content stream.
    let any_fallback = semantic_runs.iter().any(|r| r.cmap_fallback_chars > 0);
    if any_fallback {
        stats.nothing_to_learn = true;
        return Ok(stats);
    }

    // ── Layer 1: index-aligned runs ──────────────────────────────────
    if glyph_runs.len() == semantic_runs.len() {
        for (g_run, t_run) in glyph_runs.iter().zip(semantic_runs.iter()) {
            let chars: Vec<char> = t_run.text.chars().filter(|c| !c.is_whitespace()).collect();
            let glyphs: Vec<&VectorGlyph> = g_run.glyphs.iter().collect();
            if chars.len() != glyphs.len() {
                continue;
            }
            for (glyph, ch) in glyphs.iter().zip(chars.iter()) {
                if glyph.shape_hash == 0 {
                    stats.skipped += 1;
                    continue;
                }
                let s = ch.to_string();
                record_learned(
                    glyph.shape_hash,
                    &glyph.normalized,
                    &s,
                    glyph.size,
                    0.85,
                    AtlasSource::Corpus,
                );
                stats.emitted += 1;
            }
        }
        if stats.emitted > 0 {
            return Ok(stats);
        }
    }

    // ── Layer 2: flattened-index fallback ────────────────────────────
    let all_glyphs: Vec<&VectorGlyph> = glyph_runs
        .iter()
        .flat_map(|r| r.glyphs.iter())
        .filter(|g| g.shape_hash != 0)
        .collect();
    let all_chars: Vec<char> = semantic_runs
        .iter()
        .flat_map(|r| r.text.chars())
        .filter(|c| !c.is_whitespace())
        .collect();

    if all_glyphs.len() == all_chars.len() && !all_glyphs.is_empty() {
        for (glyph, ch) in all_glyphs.iter().zip(all_chars.iter()) {
            let s = ch.to_string();
            record_learned(
                glyph.shape_hash,
                &glyph.normalized,
                &s,
                glyph.size,
                0.80,
                AtlasSource::Corpus,
            );
            stats.emitted += 1;
        }
        return Ok(stats);
    }

    stats.nothing_to_learn = stats.emitted == 0;
    Ok(stats)
}

/// Page-iteration convenience: run the learner across every page of a
/// document and return the aggregate stats. Pages that cannot be learned
/// from are quietly skipped.
#[cfg(feature = "glyph-atlas")]
pub fn learn_document_glyph_mappings(doc: &PdfDocument) -> LearningStats {
    learn_document_glyph_mappings_checked(doc).unwrap_or_else(|_| LearningStats {
        nothing_to_learn: true,
        ..LearningStats::default()
    })
}

/// Checked document-wide glyph learning. Any page resource or bounded Form
/// traversal failure aborts the document operation.
#[cfg(feature = "glyph-atlas")]
pub fn learn_document_glyph_mappings_checked(doc: &PdfDocument) -> Result<LearningStats> {
    let mut agg = LearningStats::default();
    for page in doc.pages() {
        let s = learn_page_glyph_mappings_checked(doc, &page)?;
        agg.glyph_runs += s.glyph_runs;
        agg.semantic_runs += s.semantic_runs;
        agg.emitted += s.emitted;
        agg.skipped += s.skipped;
    }
    agg.nothing_to_learn = agg.emitted == 0;
    Ok(agg)
}

/// Render a sequence of [`GlyphRun`]s back into approximate text by looking
/// each glyph up in the atlas (feature `glyph-atlas`). Unknown glyphs render
/// as `_`. Newlines are inserted on >6pt y-jumps and spaces on >6pt x-gaps —
/// a coarse heuristic intended for diagnostic / preview output, not for
/// extraction-quality text.
pub fn runs_to_string(runs: &[GlyphRun]) -> String {
    let mut text = String::new();
    let mut last_y = f32::MIN;
    let mut last_x = f32::MIN;

    for run in runs {
        if last_y != f32::MIN && (run.page_y - last_y).abs() > 6.0 {
            text.push('\n');
        } else if last_x != f32::MIN && (run.page_x - last_x) > 6.0 {
            // Rough space heuristic, might need tuning based on font scale
            text.push(' ');
        }

        for glyph in &run.glyphs {
            match atlas_lookup(glyph) {
                Some(grapheme) if !grapheme.is_empty() && grapheme != "?" => {
                    text.push_str(&grapheme);
                }
                _ => text.push('_'), // Unknown glyph, stated rather than guessed
            }
        }

        last_y = run.page_y;
        last_x = run.page_x + (run.glyphs.len() as f32 * 6.0 * run.scale); // simple approx width
    }

    text
}

#[cfg(test)]
#[cfg(feature = "glyph-atlas")]
mod learning_tests {
    use super::*;
    use crate::glyph_atlas;
    use crate::{ParseOptions, PdfDocument};

    #[test]
    fn learning_is_noop_when_no_vector_glyphs() {
        // A page whose content stream draws no paths. The vector-text extractor
        // produces zero glyph runs, so the learner emits zero pairs and says so
        // rather than reporting a clean pass over nothing.
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Kids [3 0 R] /Count 1 >>
endobj
3 0 obj
<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>
endobj
4 0 obj
<< /Length 14 >>
stream
q 1 0 0 1 0 0 cm Q
endstream
endobj
"#;
        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let stats = learn_document_glyph_mappings(&doc);
        assert_eq!(stats.emitted, 0);
    }

    #[test]
    fn learning_stats_zeroed_on_empty_doc() {
        // No Kids, no content.
        let pdf = br#"%PDF-1.7
1 0 obj
<< /Type /Catalog /Pages 2 0 R >>
endobj
2 0 obj
<< /Type /Pages /Pages [] /Count 0 >>
endobj
"#;
        let doc = PdfDocument::from_slice(pdf, ParseOptions::default()).unwrap();
        let stats = learn_document_glyph_mappings(&doc);
        assert_eq!(stats.glyph_runs, 0);
        assert_eq!(stats.semantic_runs, 0);
        assert_eq!(stats.emitted, 0);
        assert!(stats.nothing_to_learn);
    }

    /// A learned mapping is findable again, and it is findable through geometry
    /// rather than through a hash that float noise can invalidate.
    ///
    /// This is the end-to-end shape of the bug: the learner always recorded
    /// something, and the lookup never found it. The unit tests in
    /// `glyph_atlas` cover the comparison itself; this covers the wiring, using
    /// the same public functions the parser does.
    #[test]
    fn a_learned_shape_is_recovered_through_geometry() {
        glyph_atlas::clear_learned();

        let glyph = |jitter: f32| VectorGlyph {
            x_offset: 0.0,
            contours: Vec::new(),
            normalized: vec![vec![
                BezPt { x: jitter, y: 0.0, kind: 1 },
                BezPt { x: 0.5, y: 0.30, kind: 1 },
                BezPt { x: 1.0, y: 0.0, kind: 1 },
            ]],
            shape_hash: if jitter == 0.0 { 0xa110_00bb } else { 0x0 },
            y_offset: 0.0,
            size: 12.0,
        };

        // Learn from one rendering of the shape.
        let learned = glyph(0.0);
        glyph_atlas::record_learned(
            learned.shape_hash,
            &learned.normalized,
            "\u{03a9}",
            learned.size,
            0.9,
            glyph_atlas::AtlasSource::Corpus,
        );
        assert_eq!(glyph_atlas::learned_shape_count(), 1, "one shape kept");

        // Recover it from a different rendering of the same shape, whose hash
        // is deliberately not the learned one — the case that used to fail.
        let later = glyph(0.6 / 255.0);
        let found = atlas_lookup(&later);
        assert_eq!(
            found.as_deref(),
            Some("\u{03a9}"),
            "geometry must recover a shape whose hash moved"
        );

        // A glyph with no geometry is not guessed at.
        let blank = VectorGlyph {
            normalized: Vec::new(),
            ..later
        };
        assert!(atlas_lookup(&blank).is_none());

        glyph_atlas::clear_learned();
    }

    /// One letter stays one entry no matter how many renderings are learned.
    ///
    /// The measured defect was 7.4k hashes for 15.7k glyphs on one contract;
    /// the atlas is expected to converge on the alphabet instead.
    #[test]
    fn repeated_learning_converges_on_one_entry_per_shape() {
        glyph_atlas::clear_learned();
        let shape = vec![vec![
            BezPt { x: 0.0, y: 0.0, kind: 1 },
            BezPt { x: 0.5, y: 0.30, kind: 1 },
            BezPt { x: 1.0, y: 0.0, kind: 1 },
        ]];
        for i in 0..200 {
            let jitter = (i % 5) as f32 * 0.0002;
            let mut jittered = shape.clone();
            for c in jittered.iter_mut() {
                for p in c.iter_mut() {
                    p.x += jitter;
                }
            }
            glyph_atlas::record_learned(1000 + i, &jittered, "A", 12.0, 0.8, glyph_atlas::AtlasSource::Corpus);
        }
        assert_eq!(
            glyph_atlas::learned_shape_count(),
            1,
            "200 renderings of one letter must collapse to one entry"
        );
        glyph_atlas::clear_learned();
    }
}
