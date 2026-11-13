//! Vector helpers and shared PUCT math for embedding-guided simulation.

/// L2 norm of a float vector.
#[inline(always)]
pub fn l2_norm(vec: &[f32]) -> f32 {
    vec.iter().map(|&x| x * x).sum::<f32>().sqrt()
}

/// Pure f32 cosine similarity using precomputed norms.
///
/// Avoids scalar f64 casts so LLVM can auto-vectorize the hot path.
#[inline(always)]
pub fn fast_cosine(a: &[f32], b: &[f32], norm_a: f32, norm_b: f32) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let denom = norm_a * norm_b;
    if denom < 1e-9 {
        0.0
    } else {
        (dot / denom).clamp(-1.0, 1.0)
    }
}

/// AlphaZero-style PUCT score (shared formula; gateway MCTS mirrors this shape).
///
/// `PUCT = Q + c_puct * P * sqrt(max(N_parent, 1)) / (1 + N_child_eff)`
/// where `N_child_eff` includes virtual losses for parallel search.
///
/// Kept as a pure helper so ranking (`mcts-fast`) and ontology-nav (gateway)
/// stay formula-aligned without merging the engines.
#[inline(always)]
pub fn puct_score(
    q: f64,
    prior: f64,
    parent_visits: u32,
    child_effective_visits: u32,
    c_puct: f64,
) -> f64 {
    let parent_sqrt = (parent_visits.max(1) as f64).sqrt();
    let u = c_puct * prior.max(0.0) * parent_sqrt / (1.0 + child_effective_visits as f64);
    q + u
}
