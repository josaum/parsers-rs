//! Arena-based MCTS engine with PUCT selection and cosine simulation.

use crate::math::{fast_cosine, l2_norm, puct_score};
use crate::types::{
    DeterministicCandidate, DeterministicRankConfig, DeterministicRankOutput,
    DeterministicRankedCandidate, MctsConfig, MctsError, MctsNode,
};
use rand::Rng;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

/// Result of [`MctsEngine::search`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Greedy most-visited label path from the root.
    pub best_path: Vec<String>,
    /// Q-value of the final node on `best_path`.
    pub best_reward: f64,
    /// Visit count per root-child label.
    pub visit_counts: HashMap<String, u32>,
    /// Number of simulations run.
    pub iterations: u32,
    /// Total nodes in the arena after search.
    pub total_nodes: usize,
}

/// One ranked candidate from [`mcts_rank`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedCandidate {
    /// Candidate label.
    pub label: String,
    /// Visit count accumulated for this candidate.
    pub visits: u32,
    /// Q-value (mean reward) for this candidate.
    pub score: f64,
}

/// Result of [`mcts_rank`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankOutput {
    /// Candidates sorted by visit count (descending).
    pub ranked: Vec<RankedCandidate>,
    /// Number of simulations run.
    pub iterations: u32,
    /// Greedy most-visited label path from the root.
    pub best_path: Vec<String>,
}

/// Snapshot of a single arena node (for bindings / debugging).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Node label.
    pub label: String,
    /// Number of times this node was visited.
    pub visit_count: u32,
    /// Mean reward (`total_reward / visit_count`).
    pub q_value: f64,
    /// Policy prior used in PUCT selection.
    pub prior: f32,
    /// Number of direct children.
    pub children_count: usize,
    /// Depth from the root (root is 0).
    pub depth: u32,
    /// Current virtual-loss count (non-zero only mid parallel descent).
    pub virtual_loss: u32,
}

/// Arena-based MCTS engine.
///
/// Callers expand the tree with [`MctsEngine::add_root`] / [`MctsEngine::add_child`]
/// (legal edges only — the engine does not invent graph neighbors). [`MctsEngine::search`]
/// then runs PUCT selection + cosine simulation + backpropagation within the budget.
///
/// Topology (`children` / `parent`) is immutable during search. Parallel sims
/// (`num_parallel > 1`) mutate only atomic visit/reward/virtual-loss counters.
#[derive(Debug, Clone)]
pub struct MctsEngine {
    config: MctsConfig,
    arena: Vec<MctsNode>,
}

impl MctsEngine {
    /// Create an empty engine with the given configuration.
    pub fn new(config: MctsConfig) -> Self {
        Self {
            config,
            arena: Vec::with_capacity(1024),
        }
    }

    /// Borrow the engine configuration.
    pub fn config(&self) -> &MctsConfig {
        &self.config
    }

    /// Number of nodes currently in the arena.
    pub fn node_count(&self) -> usize {
        self.arena.len()
    }

    /// Borrow a node by arena index (for tests / introspection).
    pub fn node(&self, idx: usize) -> Result<&MctsNode, MctsError> {
        self.arena.get(idx).ok_or(MctsError::InvalidNodeIndex {
            idx,
            arena_len: self.arena.len(),
        })
    }

    /// Clear the arena, dropping all nodes.
    pub fn reset(&mut self) {
        self.arena.clear();
    }

    /// Add the root node. Returns its arena index (always 0 on a fresh engine).
    pub fn add_root(&mut self, embedding: Vec<f32>, label: String) -> usize {
        let norm = l2_norm(&embedding);
        let idx = self.arena.len();
        self.arena
            .push(MctsNode::new(label, embedding, norm, None, 1.0));
        idx
    }

    /// Add a child under `parent_idx`. Rejects invalid parents and `max_children` overflow.
    ///
    /// Negative priors are clamped to 0 so PUCT cannot invert exploration.
    pub fn add_child(
        &mut self,
        parent_idx: usize,
        embedding: Vec<f32>,
        label: String,
        prior: f32,
    ) -> Result<usize, MctsError> {
        if parent_idx >= self.arena.len() {
            return Err(MctsError::InvalidParentIndex {
                parent_idx,
                arena_len: self.arena.len(),
            });
        }
        let max_children = self.config.max_children as usize;
        if self.arena[parent_idx].children.len() >= max_children {
            return Err(MctsError::MaxChildrenExceeded {
                parent_idx,
                max_children: self.config.max_children,
            });
        }
        let norm = l2_norm(&embedding);
        let child_idx = self.arena.len();
        self.arena.push(MctsNode::new(
            label,
            embedding,
            norm,
            Some(parent_idx),
            prior.max(0.0),
        ));
        self.arena[parent_idx].children.push(child_idx);
        Ok(child_idx)
    }

    /// Return a [`NodeInfo`] snapshot for the node at arena index `idx`.
    pub fn get_node(&self, idx: usize) -> Result<NodeInfo, MctsError> {
        if idx >= self.arena.len() {
            return Err(MctsError::InvalidNodeIndex {
                idx,
                arena_len: self.arena.len(),
            });
        }
        let node = &self.arena[idx];
        let mut depth = 0u32;
        let mut n = idx;
        while let Some(p) = self.arena[n].parent {
            n = p;
            depth += 1;
        }
        Ok(NodeInfo {
            label: node.label.clone(),
            visit_count: node.visit_count(),
            q_value: node.q_value(),
            prior: node.prior,
            children_count: node.children.len(),
            depth,
            virtual_loss: node.virtual_loss(),
        })
    }

    /// Run MCTS from the root (index 0) using cosine-similarity simulation.
    pub fn search(&mut self, query_embedding: &[f32]) -> Result<SearchResult, MctsError> {
        if self.arena.is_empty() {
            return Err(MctsError::EmptyTree);
        }
        let root = 0usize;
        let iterations = self.config.iterations;
        self.run_mcts_simulations(root, query_embedding);
        let (best_path, best_reward) = self.extract_best_path(root);
        let mut visit_counts = HashMap::with_capacity(self.arena[root].children.len());
        for &child_idx in &self.arena[root].children {
            visit_counts.insert(
                self.arena[child_idx].label.clone(),
                self.arena[child_idx].visit_count(),
            );
        }
        Ok(SearchResult {
            best_path,
            best_reward,
            visit_counts,
            iterations,
            total_nodes: self.arena.len(),
        })
    }

    /// Same as [`MctsEngine::search`] but serialized to JSON (Flight / IPC).
    pub fn search_json(&mut self, query_embedding: &[f32]) -> Result<String, MctsError> {
        let result = self.search(query_embedding)?;
        Ok(serde_json::to_string(&result)
            .unwrap_or_else(|e| format!(r#"{{"error":"JSON serialization failed: {e}"}}"#)))
    }

    /// How many children progressive widening allows at this parent.
    #[inline]
    fn pw_allowed(&self, parent_visits: u32, n_children: usize) -> usize {
        let k = self.config.pw_k;
        if k <= 0.0 || n_children == 0 {
            return n_children;
        }
        let alpha = self.config.pw_alpha;
        let allowed = (k * (parent_visits.max(1) as f64).powf(alpha)).floor() as usize;
        allowed.clamp(1, n_children)
    }

    /// Select best child under PUCT, optionally restricting to top-prior PW window.
    fn select_child(&self, current: usize, c: f64) -> usize {
        let parent = &self.arena[current];
        let children = &parent.children;
        let parent_visits = parent.visit_count();
        let allowed = self.pw_allowed(parent_visits, children.len());

        // Progressive widening: consider only the `allowed` highest-prior children.
        // When PW is off, `allowed == children.len()` and we score everyone.
        let mut best_idx = children[0];
        let mut best_score = f64::NEG_INFINITY;

        let score_child = |child_idx: usize| {
            let child = &self.arena[child_idx];
            puct_score(
                child.q_value(),
                child.prior as f64,
                parent_visits,
                child.effective_visits(),
                c,
            )
        };

        if allowed >= children.len() {
            for &child_idx in children {
                let score = score_child(child_idx);
                if score > best_score {
                    best_score = score;
                    best_idx = child_idx;
                }
            }
        } else {
            // Collect (prior, idx), sort top-`allowed` by prior desc.
            let mut ranked: Vec<(f32, usize)> = children
                .iter()
                .map(|&idx| (self.arena[idx].prior, idx))
                .collect();
            ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            for &(_, child_idx) in ranked.iter().take(allowed) {
                let score = score_child(child_idx);
                if score > best_score {
                    best_score = score;
                    best_idx = child_idx;
                }
            }
        }
        best_idx
    }

    /// One simulation: select → cosine leaf reward → backprop (+ optional VL).
    ///
    /// When `use_virtual_loss` is set, VL is applied **incrementally during
    /// descent** (after each step) so concurrent workers see the penalty before
    /// their next `select_child`. Applying VL only after the full path is chosen
    /// leaves a race window where all workers chase the same spine.
    fn simulate_once(
        &self,
        root: usize,
        query_embedding: &[f32],
        query_norm: f32,
        use_virtual_loss: bool,
        rng: &mut impl Rng,
    ) {
        let c = self.config.exploration_constant;
        let gamma = self.config.gamma;
        let max_depth = self.config.max_depth as usize;

        let mut path = Vec::with_capacity(max_depth + 1);
        let mut current = root;
        let mut depth = 0;
        path.push(current);
        if use_virtual_loss {
            self.arena[current].apply_virtual_loss();
        }

        while !self.arena[current].children.is_empty() && depth < max_depth {
            current = self.select_child(current, c);
            path.push(current);
            if use_virtual_loss {
                self.arena[current].apply_virtual_loss();
            }
            depth += 1;
        }

        let node = &self.arena[current];
        let sim = fast_cosine(&node.embedding, query_embedding, node.norm, query_norm);
        let noise: f64 = rng.gen::<f64>() * 0.01;
        let reward = (sim as f64) * gamma.powi(depth as i32) + noise;

        for &idx in &path {
            self.arena[idx].update(reward);
            if use_virtual_loss {
                self.arena[idx].remove_virtual_loss();
            }
        }
    }

    fn run_mcts_simulations(&mut self, root: usize, query_embedding: &[f32]) {
        let iterations = self.config.iterations;
        let num_parallel = self.config.num_parallel.max(1);
        let query_norm = l2_norm(query_embedding);

        if num_parallel <= 1 {
            let mut rng = rand::thread_rng();
            for _ in 0..iterations {
                self.simulate_once(root, query_embedding, query_norm, false, &mut rng);
            }
            return;
        }

        // Parallel path: virtual loss keeps concurrent descents from thrashing
        // the same high-prior spine. Topology is immutable; only atomics mutate.
        let remaining = AtomicU32::new(iterations);
        (0..num_parallel).into_par_iter().for_each(|_| {
            let mut rng = rand::thread_rng();
            loop {
                let prev = remaining.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    if v == 0 {
                        None
                    } else {
                        Some(v - 1)
                    }
                });
                if prev.is_err() {
                    break;
                }
                self.simulate_once(root, query_embedding, query_norm, true, &mut rng);
            }
        });
    }

    fn extract_best_path(&self, root: usize) -> (Vec<String>, f64) {
        let mut path = vec![self.arena[root].label.clone()];
        let mut current = root;
        let mut best_reward = 0.0f64;
        while !self.arena[current].children.is_empty() {
            let mut best_child = self.arena[current].children[0];
            let mut best_visits = self.arena[best_child].visit_count();
            for &idx in &self.arena[current].children[1..] {
                let visits = self.arena[idx].visit_count();
                if visits > best_visits {
                    best_visits = visits;
                    best_child = idx;
                }
            }
            // Do not claim unvisited descendants as the chosen path.
            if best_visits == 0 {
                break;
            }
            path.push(self.arena[best_child].label.clone());
            best_reward = self.arena[best_child].q_value();
            current = best_child;
        }
        (path, best_reward)
    }
}

/// One-shot flat ranking: root = query, children = candidates with cosine priors.
pub fn mcts_rank(
    query_embedding: &[f32],
    candidate_embeddings: Vec<Vec<f32>>,
    candidate_labels: Vec<String>,
    iterations: u32,
    exploration_constant: f64,
) -> Result<String, MctsError> {
    if candidate_embeddings.len() != candidate_labels.len() {
        return Err(MctsError::CandidateLengthMismatch {
            embeddings: candidate_embeddings.len(),
            labels: candidate_labels.len(),
        });
    }

    let config = MctsConfig::new(iterations, exploration_constant, 2, 0.99, 64);
    let mut engine = MctsEngine::new(config);
    let query_norm = l2_norm(query_embedding);
    let root = engine.add_root(query_embedding.to_vec(), "query".to_string());

    for (emb, label) in candidate_embeddings.into_iter().zip(candidate_labels) {
        let norm = l2_norm(&emb);
        let prior = fast_cosine(query_embedding, &emb, query_norm, norm);
        let _ = engine.add_child(root, emb, label, prior);
    }

    engine.run_mcts_simulations(root, query_embedding);
    let (path, _) = engine.extract_best_path(root);

    let mut ranked: Vec<RankedCandidate> = Vec::with_capacity(engine.arena[root].children.len());
    for &child_idx in &engine.arena[root].children {
        ranked.push(RankedCandidate {
            label: engine.arena[child_idx].label.clone(),
            visits: engine.arena[child_idx].visit_count(),
            score: engine.arena[child_idx].q_value(),
        });
    }
    ranked.sort_by_key(|b| std::cmp::Reverse(b.visits));

    let output = RankOutput {
        ranked,
        iterations,
        best_path: path,
    };
    Ok(serde_json::to_string(&output)
        .unwrap_or_else(|e| format!(r#"{{"error":"JSON serialization failed: {e}"}}"#)))
}

#[derive(Clone, Copy)]
struct DeterministicStats {
    visits: u32,
    total_reward_micros: i128,
}

fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut current = value;
    let mut next = (current + value / current) / 2;
    while next < current {
        current = next;
        next = (current + value / current) / 2;
    }
    current
}

fn deterministic_score(
    candidate: &DeterministicCandidate,
    stats: DeterministicStats,
    parent_visits: u32,
    config: DeterministicRankConfig,
) -> i128 {
    let mean = if stats.visits == 0 {
        0
    } else {
        stats.total_reward_micros / i128::from(stats.visits)
    };
    // Integer-only PUCT: c * prior * sqrt(N) / (1 + n). All inputs and
    // outputs remain in millionths, avoiding platform-dependent float noise.
    let sqrt_parent_scaled = integer_sqrt(u128::from(parent_visits.max(1)) * 1_000_000);
    let numerator = i128::from(config.exploration_micros)
        * i128::from(candidate.prior_micros)
        * i128::try_from(sqrt_parent_scaled).unwrap_or(i128::MAX);
    let denominator = i128::from(stats.visits + 1) * 1_000_000_000;
    mean + numerator / denominator
}

/// Rank only caller-supplied admissible explanation paths using serial,
/// integer-only MCTS. This function performs no embedding, RNG, parallelism,
/// model call, or external lookup.
pub fn rank_deterministic(
    candidates: &[DeterministicCandidate],
    config: DeterministicRankConfig,
) -> Result<DeterministicRankOutput, MctsError> {
    if candidates.is_empty() {
        return Err(MctsError::EmptyDeterministicCandidates);
    }
    if config.iterations == 0 {
        return Err(MctsError::InvalidDeterministicIterations);
    }

    let mut ordered = candidates.to_vec();
    ordered.sort_by(|left, right| left.label.cmp(&right.label));
    for candidate in &ordered {
        if candidate.prior_micros > 1_000_000 {
            return Err(MctsError::DeterministicPriorOutOfRange {
                label: candidate.label.clone(),
                prior_micros: candidate.prior_micros,
            });
        }
    }
    for pair in ordered.windows(2) {
        if pair[0].label == pair[1].label {
            return Err(MctsError::DuplicateDeterministicLabel {
                label: pair[0].label.clone(),
            });
        }
    }

    let mut stats = vec![
        DeterministicStats {
            visits: 0,
            total_reward_micros: 0,
        };
        ordered.len()
    ];
    for iteration in 0..config.iterations {
        let parent_visits = iteration + 1;
        let mut selected = 0usize;
        let mut selected_score = deterministic_score(&ordered[0], stats[0], parent_visits, config);
        for index in 1..ordered.len() {
            let score = deterministic_score(&ordered[index], stats[index], parent_visits, config);
            let candidate = &ordered[index];
            let winner = &ordered[selected];
            if score > selected_score
                || (score == selected_score
                    && (candidate.prior_micros > winner.prior_micros
                        || (candidate.prior_micros == winner.prior_micros
                            && (candidate.reward_micros > winner.reward_micros
                                || (candidate.reward_micros == winner.reward_micros
                                    && candidate.label < winner.label)))))
            {
                selected = index;
                selected_score = score;
            }
        }
        stats[selected].visits += 1;
        stats[selected].total_reward_micros += i128::from(ordered[selected].reward_micros);
    }

    let mut ranked: Vec<DeterministicRankedCandidate> = ordered
        .iter()
        .zip(stats)
        .map(|(candidate, stats)| DeterministicRankedCandidate {
            label: candidate.label.clone(),
            visits: stats.visits,
            mean_reward_micros: if stats.visits == 0 {
                0
            } else {
                i64::try_from(stats.total_reward_micros / i128::from(stats.visits)).unwrap_or_else(
                    |_| {
                        if stats.total_reward_micros.is_negative() {
                            i64::MIN
                        } else {
                            i64::MAX
                        }
                    },
                )
            },
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .visits
            .cmp(&left.visits)
            .then_with(|| right.mean_reward_micros.cmp(&left.mean_reward_micros))
            .then_with(|| left.label.cmp(&right.label))
    });
    let best_path = vec![ranked[0].label.clone()];
    Ok(DeterministicRankOutput {
        ranked,
        iterations: config.iterations,
        best_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DeterministicCandidate, DeterministicRankConfig, MctsConfig};

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n < 1e-9 {
            v.to_vec()
        } else {
            v.iter().map(|x| x / n).collect()
        }
    }

    #[test]
    fn virtual_loss_diverts_concurrent_descent() {
        // WHY: parallel sims without VL all chase the same high-prior spine;
        // applying VL mid-flight must make a sibling win the next selection.
        let config = MctsConfig {
            iterations: 1,
            exploration_constant: 10.0,
            max_depth: 2,
            gamma: 1.0,
            ..MctsConfig::default()
        };
        let mut engine = MctsEngine::new(config);
        let root = engine.add_root(unit(&[1.0, 0.0]), "q".into());
        let hot = engine
            .add_child(root, unit(&[1.0, 0.0]), "hot".into(), 0.9)
            .unwrap();
        let cool = engine
            .add_child(root, unit(&[0.0, 1.0]), "cool".into(), 0.8)
            .unwrap();

        // Without VL, high prior wins.
        assert_eq!(engine.select_child(root, 10.0), hot);

        // Simulate an in-flight worker sitting on `hot`.
        engine.arena[hot].apply_virtual_loss();
        assert_eq!(
            engine.select_child(root, 10.0),
            cool,
            "VL on hot must divert the next descent to cool"
        );
        engine.arena[hot].remove_virtual_loss();
        assert_eq!(engine.select_child(root, 10.0), hot);
    }

    #[test]
    fn parallel_descent_applies_vl_before_sibling_select() {
        // WHY: VL must be visible to other workers *during* descent, not only
        // after the full path is chosen — otherwise all workers race the same
        // spine before any penalty lands.
        let config = MctsConfig {
            iterations: 1,
            exploration_constant: 10.0,
            max_depth: 2,
            gamma: 1.0,
            num_parallel: 2,
            ..MctsConfig::default()
        };
        let mut engine = MctsEngine::new(config);
        let root = engine.add_root(unit(&[1.0, 0.0]), "q".into());
        let hot = engine
            .add_child(root, unit(&[1.0, 0.0]), "hot".into(), 0.9)
            .unwrap();
        let cool = engine
            .add_child(root, unit(&[0.0, 1.0]), "cool".into(), 0.8)
            .unwrap();

        // Mimic worker A after choosing `hot` but before leaf eval: VL already on.
        engine.arena[root].apply_virtual_loss();
        engine.arena[hot].apply_virtual_loss();
        assert_eq!(
            engine.select_child(root, 10.0),
            cool,
            "sibling select must see in-descent VL on hot"
        );
        engine.arena[hot].remove_virtual_loss();
        engine.arena[root].remove_virtual_loss();
    }

    fn candidate(label: &str, prior_micros: u32, reward_micros: i64) -> DeterministicCandidate {
        DeterministicCandidate {
            label: label.to_owned(),
            prior_micros,
            reward_micros,
        }
    }

    #[test]
    fn deterministic_rank_replays_byte_identically() {
        let candidates = vec![
            candidate("proof:a", 900_000, 800_000),
            candidate("proof:b", 800_000, 900_000),
        ];
        let config = DeterministicRankConfig::default();
        let first = rank_deterministic(&candidates, config).expect("rank candidates");
        for _ in 0..100 {
            assert_eq!(
                rank_deterministic(&candidates, config).expect("rank candidates"),
                first
            );
        }
    }

    #[test]
    fn deterministic_rank_returns_only_supplied_candidates() {
        let output = rank_deterministic(
            &[candidate("proof:a", 1_000_000, 1_000_000)],
            DeterministicRankConfig::default(),
        )
        .expect("rank candidate");
        assert_eq!(output.best_path, vec!["proof:a"]);
        assert!(output.ranked.iter().all(|ranked| ranked.label == "proof:a"));
    }
}
