//! Bounded compatibility shim for the retired glyph atlas feature.
//!
//! Atlas parsing never reads or writes user caches and never ships a generated
//! glyph corpus. The optional feature remains buildable for downstream feature
//! resolution, but deliberately provides no learned mappings.
//!
//! # Why there is no corpus, not just why nothing is persisted
//!
//! The atlas was meant as a model-free OCR substitute: when a font's
//! `/ToUnicode` CMap is missing or corrupt, hash each glyph's normalized
//! contour and look the hash up to recover the character. It cannot work,
//! because the key is unsuitable for identity — see
//! [`crate::PdfDocument::vector_glyph_clusters`], which documents the
//! measurement: an exact FNV hash over a 1/255 grid flips when a contour point
//! sits on a cell boundary under float noise, so with ~40 points per glyph
//! almost every instance lands in its own bucket. Measured on one real
//! contract: 7.4k distinct hashes for 15.7k glyphs, roughly 2x dedup where the
//! truth is nearer 50x.
//!
//! The corpus that grew from that was therefore a record of the hash's
//! brittleness — thousands of shapes for a few hundred characters, each entry
//! serving an instance that does not recur — and lookups were exact
//! (`binary_search_by_key`), so a re-rendered glyph that crossed a cell
//! boundary returned `None` with no tolerant fallback to catch it.
//!
//! Shape identity is solved elsewhere and correctly: `vector_glyph_clusters`
//! matches by contour structure plus an `eps` tolerance and yields a stable
//! document-scoped `cluster_id`. Recovering a grapheme from a cluster is a
//! labeling decision, not a parser decision, and belongs to the caller.

/// A glyph mapping entry.
#[derive(Debug, Clone, Copy)]
pub struct AtlasEntry {
    /// The mapped grapheme.
    pub grapheme: &'static str,
}

/// Provenance accepted by the compatibility hook.
#[derive(Debug, Clone, Copy)]
pub enum AtlasSource {
    /// Caller-supplied, unpersisted correlation.
    Corpus,
}

/// No generated corpus is available in the read-only parser plane.
pub fn lookup_entry(_hash: u64) -> Option<AtlasEntry> {
    None
}

/// Discard compatibility observations rather than persisting state.
pub fn record_learned(_hash: u64, _grapheme: &str, _confidence: f32, _source: AtlasSource) {}

#[cfg(test)]
#[allow(missing_docs)]
pub fn env_lock_for_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[allow(missing_docs)]
pub fn pending_learned_count() -> usize {
    0
}

#[cfg(test)]
#[allow(missing_docs)]
pub fn learned_sink_for_test_clear() {}

#[cfg(test)]
#[allow(missing_docs)]
pub fn save_process_atlas() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(missing_docs)]
#[derive(Default)]
pub struct GlyphAtlas;

#[cfg(test)]
#[allow(missing_docs)]
impl GlyphAtlas {
    pub fn new() -> Self {
        Self
    }
    pub fn load_default_cache(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    pub fn lookup(&self, _hash: u64) -> Option<AtlasEntry> {
        None
    }
}
