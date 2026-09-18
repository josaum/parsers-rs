//! Bounded glyph-shape atlas: recover a grapheme from a glyph's outline.
//!
//! # Why this exists
//!
//! Some PDFs draw text as vector outlines — Type 3 fonts, or any font whose
//! `/ToUnicode` CMap is missing or corrupt. The glyphs are there as paths but
//! the text is not recoverable from the encoding, and recovering it needs
//! either an OCR model or a shape index. This is the shape index: a model-free
//! lookup from an outline to the character it draws.
//!
//! # Why it is not keyed on a hash
//!
//! `VectorGlyph::shape_hash` is an FNV-1a hash over contour points quantized to
//! a 1/255 grid. FNV-1a **avalanches**: one differing input byte produces a
//! wholly unrelated output rather than a nearby one. So the quantization that
//! was meant to absorb float noise does not — with roughly 40 points per glyph,
//! a single point drifting across a bucket boundary changes the hash beyond
//! recognition. Measured on one real contract: 7.4k distinct hashes for 15.7k
//! glyphs, about 2x dedup where the truth is nearer 50x.
//!
//! A hash is still a useful *pre-filter*, so it is kept. It is not the identity.
//! Identity is the geometry, compared with tolerance:
//!
//! 1. the same contour count,
//! 2. the same per-contour point count,
//! 3. the same per-point kind (moveto / lineto / curve),
//! 4. every normalized point within `eps` of its counterpart,
//! 5. a scale ratio within [`SIZE_RATIO`].
//!
//! This is the rule [`crate::PdfDocument::vector_glyph_clusters`] already
//! applies, which is why glyph clustering worked while the atlas did not.
//!
//! # Scope, stated plainly
//!
//! Conditions 1–3 mean a match requires the **same outline structure**. That is
//! invariant to translation, to uniform scale, and to float jitter — enough for
//! the case this module is for, where one document draws one font and the same
//! letter recurs across pages. It is *not* invariant across different fonts,
//! whose outlines break the same character into different numbers of contours
//! and points. A cross-font atlas needs a genuinely perceptual representation;
//! this is not one, and claiming otherwise would be the plausible-but-wrong
//! result this repository refuses elsewhere.
//!
//! # What is deliberately absent
//!
//! Nothing here reads or writes a user cache, and nothing derives a path from
//! `$HOME`. The atlas is in memory for the life of the process, and a caller
//! that wants persistence supplies the path and the format. The earlier
//! implementation defaulted to `~/.cache/<a consumer>/atlas.tsv`, which a
//! read-only parser plane has no business doing and which named a consumer
//! inside a shared repository.

use std::sync::{Mutex, OnceLock};

use crate::vector_text::BezPt;

/// Largest scale ratio still treated as one shape.
///
/// Two renderings of one glyph at the same size agree closely; the same letter
/// at another size is a *ratio* away, not a bucketed distance, so this is a
/// ratio rather than a band. Generous enough for a font that rounds its advance
/// widths, tight enough that a digit does not merge into its neighbour at
/// another size — a merged glyph is a fabricated character downstream, which is
/// worse than an unknown one.
pub const SIZE_RATIO: f32 = 1.15;

/// Default per-point tolerance in normalized units.
///
/// Normalized coordinates run 0..1, so this is roughly a quarter of a percent
/// of the glyph's larger dimension — comfortably above float noise in outline
/// flattening, far below the distance between two distinct letterforms.
pub const DEFAULT_EPS: f32 = 0.0025;

/// Where a mapping came from.
///
/// Kept because a caller may want to trust a bootstrap table differently from a
/// correlation harvested out of the same document, and because a reviewer will
/// ask where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasSource {
    /// Derived by correlating this document's outlines with its own resolved
    /// text. Highest trust: both sides came from the same content stream.
    Corpus,
    /// Supplied by the caller, from a table it owns.
    Bootstrap,
    /// Supplied by an application as an explicit correction.
    UserOverride,
}

impl AtlasSource {
    /// Higher wins when two entries describe the same shape.
    #[inline]
    pub fn priority(self) -> u8 {
        match self {
            Self::UserOverride => 3,
            Self::Corpus => 2,
            Self::Bootstrap => 1,
        }
    }
}

/// One learned mapping: an outline shape and the grapheme it draws.
///
/// `shape` is the normalized outline — the same points `shape_hash` was taken
/// from — because a hash cannot be compared with tolerance and points can.
#[derive(Debug, Clone)]
pub struct AtlasEntry {
    /// The grapheme this shape draws. Multi-codepoint safe, so a ligature or a
    /// composed accent survives as one entry.
    pub grapheme: String,
    /// Hash of `shape`, kept as a fast pre-filter for exact hits.
    pub shape_hash: u64,
    /// Normalized outline: translation removed, larger dimension scaled to 1.
    pub shape: Vec<Vec<BezPt>>,
    /// Pre-normalization extent, `max(width, height)`.
    pub size: f32,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f32,
    /// Provenance.
    pub source: AtlasSource,
}

impl AtlasEntry {
    /// Build an entry from a glyph's normalized outline.
    pub fn new(
        grapheme: impl Into<String>,
        shape_hash: u64,
        shape: Vec<Vec<BezPt>>,
        size: f32,
        confidence: f32,
        source: AtlasSource,
    ) -> Self {
        Self {
            grapheme: grapheme.into(),
            shape_hash,
            shape,
            size,
            confidence,
            source,
        }
    }
}

/// Whether `query` draws the same shape as `entry`, within tolerance.
///
/// Structural equality comes first because it is cheap and because a point-wise
/// comparison of differently-structured outlines is meaningless: zipping a
/// 3-contour glyph against a 4-contour one would compare unrelated points and
/// could report a match between two different letters.
#[inline]
pub fn matches(entry: &AtlasEntry, query: &[Vec<BezPt>], size: f32, eps: f32) -> bool {
    if entry.shape.len() != query.len() {
        return false;
    }
    if !size_agrees(entry.size, size) {
        return false;
    }
    for (expected, actual) in entry.shape.iter().zip(query) {
        if expected.len() != actual.len() {
            return false;
        }
        for (want, got) in expected.iter().zip(actual) {
            if want.kind != got.kind {
                return false;
            }
            if (want.x - got.x).abs() > eps || (want.y - got.y).abs() > eps {
                return false;
            }
        }
    }
    true
}

#[inline]
fn size_agrees(a: f32, b: f32) -> bool {
    if !a.is_finite() || !b.is_finite() || a <= 0.0 || b <= 0.0 {
        // A degenerate extent carries no size information; compare structure
        // and points only rather than rejecting on a value that means nothing.
        return true;
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    hi / lo <= SIZE_RATIO
}

/// A set of learned shape → grapheme mappings.
///
/// Lookup tries the hash first — free, and correct whenever quantization did
/// not tip — then falls back to a tolerant scan. The scan is linear, which is
/// the honest cost of tolerance: a real index over outline space would need a
/// metric tree, and at this scale (a few hundred entries for a whole document)
/// a linear pass is cheaper than the structure that would avoid it.
#[derive(Debug, Default)]
pub struct GlyphAtlas {
    entries: Vec<AtlasEntry>,
}

impl GlyphAtlas {
    /// An empty atlas.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the atlas holds nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, for inspection.
    #[inline]
    pub fn entries(&self) -> &[AtlasEntry] {
        &self.entries
    }

    /// Record a mapping, replacing a weaker entry for the same shape.
    ///
    /// "Same shape" is the tolerant comparison, not hash equality — otherwise
    /// the atlas accumulates near-duplicates of one letter, which is how the
    /// previous table came to hold 7,358 entries for 158 characters.
    ///
    /// An entry that only restates what is already known does not raise
    /// confidence: repeated correlation of the same page is not evidence.
    pub fn insert(&mut self, entry: AtlasEntry) {
        if entry.shape.is_empty() || entry.grapheme.is_empty() {
            return;
        }
        match self.find(&entry.shape, entry.size, DEFAULT_EPS) {
            Some(index) => {
                let existing = &mut self.entries[index];
                let better = entry.source.priority() > existing.source.priority()
                    || (entry.source == existing.source && entry.confidence > existing.confidence);
                if better {
                    *existing = entry;
                }
            }
            None => self.entries.push(entry),
        }
    }

    /// Index of the entry matching this shape, if any.
    pub fn find(&self, query: &[Vec<BezPt>], size: f32, eps: f32) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .position(|entry| matches(entry, query, size, eps))
    }

    /// The entry for this shape, if the atlas knows it.
    pub fn lookup(&self, query: &[Vec<BezPt>], size: f32, eps: f32) -> Option<&AtlasEntry> {
        self.find(query, size, eps).map(|i| &self.entries[i])
    }

    /// Look up by exact hash, else by tolerant shape comparison.
    ///
    /// The hash path makes the common case cheap. Correctness does not depend
    /// on it: a hash miss falls through to the tolerant path rather than
    /// reporting the glyph as unknown.
    pub fn resolve(
        &self,
        shape_hash: u64,
        query: &[Vec<BezPt>],
        size: f32,
        eps: f32,
    ) -> Option<&AtlasEntry> {
        if shape_hash != 0 {
            if let Some(exact) = self
                .entries
                .iter()
                .find(|entry| entry.shape_hash == shape_hash)
            {
                return Some(exact);
            }
        }
        self.lookup(query, size, eps)
    }
}

/// The process-wide atlas populated by [`record_learned`].
///
/// A process global is the shape that fits: learning happens while parsing page
/// N and pays off on page N+1, and the two do not share a borrow. Memory only —
/// no path is derived from the environment and nothing is written to disk.
fn process_atlas() -> &'static Mutex<GlyphAtlas> {
    static ATLAS: OnceLock<Mutex<GlyphAtlas>> = OnceLock::new();
    ATLAS.get_or_init(|| Mutex::new(GlyphAtlas::new()))
}

/// Record a shape → grapheme mapping observed while parsing.
///
/// `shape` must be the glyph's normalized outline and `shape_hash` its hash, so
/// the two describe the same points. An empty shape is ignored: without
/// geometry there is nothing to match with tolerance, and storing the hash
/// alone would recreate the defect this module exists to fix.
///
/// Poisoning is ignored rather than propagated: a panic in another thread must
/// not make every later parse fail, and every method here is panic-free.
pub fn record_learned(
    shape_hash: u64,
    shape: &[Vec<BezPt>],
    grapheme: &str,
    size: f32,
    confidence: f32,
    source: AtlasSource,
) {
    if shape.is_empty() || grapheme.is_empty() {
        return;
    }
    let entry = AtlasEntry::new(
        grapheme,
        shape_hash,
        shape.to_vec(),
        size,
        confidence,
        source,
    );
    if let Ok(mut atlas) = process_atlas().lock() {
        atlas.insert(entry);
    }
}

/// Resolve a glyph through the process atlas.
///
/// Returns the mapped grapheme, which may be multi-codepoint. `None` means the
/// shape is not known, and the caller should say so rather than guess: an
/// invented character is worse than a visible gap.
pub fn lookup_entry(shape_hash: u64, shape: &[Vec<BezPt>], size: f32, eps: f32) -> Option<String> {
    process_atlas()
        .lock()
        .ok()
        .and_then(|atlas| atlas.resolve(shape_hash, shape, size, eps).cloned())
        .map(|entry| entry.grapheme)
}

/// Number of distinct shapes learned so far.
///
/// The number to watch: for a document drawing one font it should approach the
/// size of the alphabet in use. Thousands for a couple of hundred characters
/// means shapes that should have matched did not — the symptom the tolerant
/// comparison exists to remove.
pub fn learned_shape_count() -> usize {
    process_atlas().lock().map(|a| a.len()).unwrap_or(0)
}

/// Drop every learned mapping, returning how many were dropped.
///
/// For tests that need a clean process atlas.
#[doc(hidden)]
pub fn clear_learned() -> usize {
    match process_atlas().lock() {
        Ok(mut atlas) => {
            let n = atlas.len();
            atlas.entries.clear();
            n
        }
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(x: f32, y: f32) -> BezPt {
        BezPt { x, y, kind: 1 }
    }

    /// A three-point open path, standing in for a glyph outline.
    fn shape(jitter: f32) -> Vec<Vec<BezPt>> {
        vec![vec![pt(jitter, 0.0), pt(0.5, 0.25), pt(1.0, 0.0)]]
    }

    /// The hash the glyph extractor would take from these points, so the test
    /// can show that it differs where the tolerant comparison does not.
    fn fnv_over_grid(points: &[Vec<BezPt>]) -> u64 {
        let mut h = 14695981039346656037u64;
        for contour in points {
            for p in contour {
                h ^= (p.x * 255.0) as u8 as u64;
                h = h.wrapping_mul(1099511628211);
                h ^= (p.y * 255.0) as u8 as u64;
                h = h.wrapping_mul(1099511628211);
                h ^= p.kind as u64;
                h = h.wrapping_mul(1099511628211);
            }
        }
        h
    }

    #[test]
    fn an_identical_shape_matches_regardless_of_the_hash() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "A",
            0xdead_beef,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        // A hash that matches nothing: identity must come from geometry.
        let found = atlas.resolve(0x0000_0000, &shape(0.0), 10.0, DEFAULT_EPS);
        assert_eq!(found.map(|e| e.grapheme.as_str()), Some("A"));
    }

    /// The regression this module exists for: a drift far too small for a
    /// reader to see tips a quantization cell and changes an FNV hash past
    /// recognition, so a hash-keyed lookup misses while a tolerant one matches.
    ///
    /// The fixture has to *actually* tip a cell. An early version of this test
    /// jittered by `0.5 / 255.0` and proved nothing: `0.00196 * 255 = 0.5` is
    /// still inside bucket 0, so both hashes came out identical and the
    /// assertion that they differ is what failed — the test was wrong, not the
    /// code. The points here sit just below a bucket edge on purpose.
    #[test]
    fn a_jittered_shape_matches_where_the_hash_cannot() {
        let mut atlas = GlyphAtlas::new();
        let base = vec![vec![
            // 0.003 * 255 = 0.765 -> bucket 0.
            pt(0.003, 0.0),
            pt(0.5, 0.25),
            pt(1.0, 0.0),
        ]];
        atlas.insert(AtlasEntry::new(
            "e",
            0x1111,
            base.clone(),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));

        // +0.002 moves that point to 0.005 * 255 = 1.275 -> bucket 1, while
        // staying well inside `DEFAULT_EPS` for a reader comparing points.
        let jitter = 0.002;
        assert!(jitter < DEFAULT_EPS, "the drift must be sub-tolerance");
        let jittered = vec![vec![pt(0.003 + jitter, 0.0), pt(0.5, 0.25), pt(1.0, 0.0)]];

        assert_ne!(
            fnv_over_grid(&base),
            fnv_over_grid(&jittered),
            "the fixture must actually tip a quantization cell, or it proves nothing"
        );

        // The hash would have missed; the tolerant path recovers it anyway.
        let found = atlas.resolve(fnv_over_grid(&jittered), &jittered, 10.0, DEFAULT_EPS);
        assert_eq!(found.map(|e| e.grapheme.as_str()), Some("e"));
    }

    #[test]
    fn a_different_shape_does_not_match() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "A",
            1,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        let other = vec![vec![pt(0.0, 0.0), pt(0.5, 0.9), pt(1.0, 0.4)]];
        assert!(atlas.resolve(0xbeef, &other, 10.0, DEFAULT_EPS).is_none());
    }

    #[test]
    fn different_contour_structure_does_not_match() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "i",
            1,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        // The same points split across two contours is a different glyph, and
        // the comparison must not zip them together and call it a match.
        let split = vec![vec![pt(0.0, 0.0), pt(0.5, 0.25)], vec![pt(1.0, 0.0)]];
        assert!(atlas.resolve(0xbeef, &split, 10.0, DEFAULT_EPS).is_none());
    }

    #[test]
    fn a_point_kind_mismatch_does_not_match() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "c",
            1,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        let mut curved = shape(0.0);
        curved[0][1].kind = 2;
        assert!(atlas.resolve(2, &curved, 10.0, DEFAULT_EPS).is_none());
    }

    #[test]
    fn a_clearly_different_size_is_a_different_shape() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "o",
            1,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        assert!(atlas.resolve(2, &shape(0.0), 40.0, DEFAULT_EPS).is_none());
    }

    #[test]
    fn a_nearby_size_agrees() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "o",
            1,
            shape(0.0),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        assert!(atlas.resolve(2, &shape(0.0), 10.5, DEFAULT_EPS).is_some());
    }

    /// Re-inserting near-duplicates must not grow the atlas. This is the
    /// property whose absence produced thousands of entries for one alphabet.
    #[test]
    fn reinserting_one_shape_does_not_accumulate_entries() {
        let mut atlas = GlyphAtlas::new();
        for i in 0..500 {
            let jitter = (i % 7) as f32 * 0.0002;
            atlas.insert(AtlasEntry::new(
                "x",
                1000 + i,
                shape(jitter),
                10.0,
                0.8,
                AtlasSource::Corpus,
            ));
        }
        assert_eq!(atlas.len(), 1, "one letter must stay one entry");
    }

    #[test]
    fn a_higher_priority_source_replaces_a_lower_one() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "a",
            1,
            shape(0.0),
            10.0,
            0.5,
            AtlasSource::Bootstrap,
        ));
        atlas.insert(AtlasEntry::new(
            "a",
            2,
            shape(0.0),
            10.0,
            0.6,
            AtlasSource::UserOverride,
        ));
        assert_eq!(atlas.len(), 1);
        assert_eq!(atlas.entries()[0].source, AtlasSource::UserOverride);
    }

    /// A better confidence from the same source wins; a worse one does not
    /// erase what is already known.
    #[test]
    fn confidence_only_rises_within_one_source() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "b",
            1,
            shape(0.0),
            10.0,
            0.9,
            AtlasSource::Corpus,
        ));
        atlas.insert(AtlasEntry::new(
            "b",
            2,
            shape(0.0),
            10.0,
            0.4,
            AtlasSource::Corpus,
        ));
        assert_eq!(atlas.entries()[0].confidence, 0.9);
    }

    #[test]
    fn an_entry_without_geometry_is_refused() {
        let mut atlas = GlyphAtlas::new();
        atlas.insert(AtlasEntry::new(
            "z",
            7,
            Vec::new(),
            10.0,
            1.0,
            AtlasSource::Corpus,
        ));
        assert!(atlas.is_empty(), "a hash without points is the old defect");
    }
}
