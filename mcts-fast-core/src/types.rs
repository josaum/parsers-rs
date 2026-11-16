//! Shared MCTS types for the sovereign embedding-guided engine.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Configuration for PUCT search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MctsConfig {
    /// Number of select→simulate→backprop iterations.
    pub iterations: u32,
    /// PUCT exploration constant (c_puct). Higher = more exploration.
    pub exploration_constant: f64,
    /// Maximum selection depth before forcing a leaf simulation.
    pub max_depth: u32,
    /// Exponential reward discount γ applied as `gamma^depth`.
    pub gamma: f64,
    /// Hard cap on children per node (enforced at `add_child`).
    pub max_children: u32,
    /// Parallel simulation workers. `1` = serial (default). When `> 1`,
    /// virtual loss is applied during descent so concurrent sims diverge.
    #[serde(default = "default_num_parallel")]
    pub num_parallel: u32,
    /// Progressive widening constant `k`. `0.0` disables PW (all children
    /// visible). When `> 0`, at most `floor(k * N^α)` highest-prior children
    /// are considered during selection.
    #[serde(default)]
    pub pw_k: f64,
    /// Progressive widening exponent α (typically 0.5). Ignored when `pw_k == 0`.
    #[serde(default = "default_pw_alpha")]
    pub pw_alpha: f64,
}

fn default_num_parallel() -> u32 {
    1
}

fn default_pw_alpha() -> f64 {
    0.5
}

impl Default for MctsConfig {
    fn default() -> Self {
        Self {
            iterations: 200,
            exploration_constant: 1.414,
            max_depth: 20,
            gamma: 0.99,
            max_children: 64,
            num_parallel: 1,
            pw_k: 0.0,
            pw_alpha: 0.5,
        }
    }
}

impl MctsConfig {
    /// Construct an MCTS configuration with the core search knobs.
    pub fn new(
        iterations: u32,
        exploration_constant: f64,
        max_depth: u32,
        gamma: f64,
        max_children: u32,
    ) -> Self {
        Self {
            iterations,
            exploration_constant,
            max_depth,
            gamma,
            max_children,
            ..Self::default()
        }
    }
}

/// One admissible, receipt-derived explanation candidate.
///
/// Both scores use fixed-point millionths. No embedding or probabilistic value
/// is accepted by this boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicCandidate {
    /// Stable path identifier copied from the inference receipt.
    pub label: String,
    /// Governed prior in `[0, 1_000_000]`.
    pub prior_micros: u32,
    /// Deterministic reward in fixed-point millionths.
    pub reward_micros: i64,
}

/// Fixed, serial budget for deterministic receipt-path ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicRankConfig {
    /// Exact number of root-bandit simulations.
    pub iterations: u32,
    /// Fixed-point PUCT exploration constant.
    pub exploration_micros: u32,
}

impl Default for DeterministicRankConfig {
    fn default() -> Self {
        Self {
            iterations: 256,
            exploration_micros: 1_414_000,
        }
    }
}

/// One candidate after deterministic MCTS ranking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicRankedCandidate {
    /// Stable path identifier supplied by the receipt.
    pub label: String,
    /// Exact simulation visit count.
    pub visits: u32,
    /// Mean fixed-point reward accumulated by the candidate.
    pub mean_reward_micros: i64,
}

/// Reproducible ranking over only the supplied admissible candidates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicRankOutput {
    /// Candidates ordered by visits, reward, then stable label.
    pub ranked: Vec<DeterministicRankedCandidate>,
    /// Exact configured simulation count.
    pub iterations: u32,
    /// Winning path; contains exactly one supplied path identifier.
    pub best_path: Vec<String>,
}

/// Arena node. Embeddings are owned; labels are owned strings for FFI simplicity.
///
/// Visit / reward / virtual-loss counters are atomic so parallel simulations
/// can select and backpropagate without locking the arena topology (which is
/// immutable during [`crate::MctsEngine::search`]).
#[derive(Debug)]
pub struct MctsNode {
    /// Node label preserved in search results / best paths.
    pub label: String,
    /// Embedding vector used for cosine leaf reward.
    pub embedding: Vec<f32>,
    /// Precomputed L2 norm for zero-overhead cosine similarity.
    pub norm: f32,
    /// Parent arena index (`None` for root).
    pub parent: Option<usize>,
    /// Child arena indices.
    pub children: Vec<usize>,
    visit_count: AtomicU32,
    /// Cumulative reward stored as `f64` bit pattern for lock-free adds.
    total_reward_bits: AtomicU64,
    /// Policy prior for PUCT (clamped ≥ 0 at insertion).
    pub prior: f32,
    /// Virtual loss counter — raised during parallel descent, cleared on backprop.
    virtual_loss: AtomicU32,
}

impl Clone for MctsNode {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            embedding: self.embedding.clone(),
            norm: self.norm,
            parent: self.parent,
            children: self.children.clone(),
            visit_count: AtomicU32::new(self.visit_count()),
            total_reward_bits: AtomicU64::new(self.total_reward_bits.load(Ordering::Relaxed)),
            prior: self.prior,
            virtual_loss: AtomicU32::new(self.virtual_loss()),
        }
    }
}

impl MctsNode {
    pub(crate) fn new(
        label: String,
        embedding: Vec<f32>,
        norm: f32,
        parent: Option<usize>,
        prior: f32,
    ) -> Self {
        Self {
            label,
            embedding,
            norm,
            parent,
            children: Vec::new(),
            visit_count: AtomicU32::new(0),
            total_reward_bits: AtomicU64::new(f64::to_bits(0.0)),
            prior,
            virtual_loss: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    /// Load the current visit count.
    pub fn visit_count(&self) -> u32 {
        self.visit_count.load(Ordering::Relaxed)
    }

    #[inline(always)]
    /// Load the current virtual-loss count.
    pub fn virtual_loss(&self) -> u32 {
        self.virtual_loss.load(Ordering::Relaxed)
    }

    #[inline(always)]
    /// Load the cumulative reward.
    pub fn total_reward(&self) -> f64 {
        f64::from_bits(self.total_reward_bits.load(Ordering::Relaxed))
    }

    #[inline(always)]
    /// Mean reward for this node, or 0.0 when unvisited.
    pub fn q_value(&self) -> f64 {
        let n = self.visit_count();
        if n == 0 {
            0.0
        } else {
            self.total_reward() / n as f64
        }
    }

    /// Effective visits for PUCT exploration (real visits + in-flight virtual losses).
    #[inline(always)]
    pub fn effective_visits(&self) -> u32 {
        self.visit_count() + self.virtual_loss()
    }

    #[inline]
    pub(crate) fn apply_virtual_loss(&self) {
        self.virtual_loss.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn remove_virtual_loss(&self) {
        let _ = self
            .virtual_loss
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if v > 0 {
                    Some(v - 1)
                } else {
                    None
                }
            });
    }

    #[inline]
    pub(crate) fn update(&self, reward: f64) {
        self.visit_count.fetch_add(1, Ordering::Relaxed);
        // CAS loop on f64 bit pattern (same pattern as gateway MCTS).
        loop {
            let old_bits = self.total_reward_bits.load(Ordering::Relaxed);
            let new_bits = f64::to_bits(f64::from_bits(old_bits) + reward);
            if self
                .total_reward_bits
                .compare_exchange_weak(old_bits, new_bits, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }
}

/// Errors from tree construction / search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MctsError {
    /// Parent arena index does not exist.
    InvalidParentIndex {
        /// Requested parent index.
        parent_idx: usize,
        /// Current arena length.
        arena_len: usize,
    },
    /// Node arena index does not exist.
    InvalidNodeIndex {
        /// Requested node index.
        idx: usize,
        /// Current arena length.
        arena_len: usize,
    },
    /// Search was requested before adding a root.
    EmptyTree,
    /// Parent already has the configured maximum number of children.
    MaxChildrenExceeded {
        /// Parent index that exceeded the child limit.
        parent_idx: usize,
        /// Configured maximum children.
        max_children: u32,
    },
    /// Candidate embeddings and labels have different lengths.
    CandidateLengthMismatch {
        /// Candidate embedding count.
        embeddings: usize,
        /// Candidate label count.
        labels: usize,
    },
    /// Deterministic ranking received no admissible paths.
    EmptyDeterministicCandidates,
    /// Deterministic ranking requires a positive fixed simulation budget.
    InvalidDeterministicIterations,
    /// A fixed-point prior exceeded the closed `[0, 1_000_000]` interval.
    DeterministicPriorOutOfRange {
        /// Candidate that carried the invalid prior.
        label: String,
        /// Invalid fixed-point prior.
        prior_micros: u32,
    },
    /// Receipt candidates must have unique stable path identifiers.
    DuplicateDeterministicLabel {
        /// Duplicate candidate path identifier.
        label: String,
    },
}

impl std::fmt::Display for MctsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParentIndex {
                parent_idx,
                arena_len,
            } => write!(
                f,
                "Invalid parent index {parent_idx} (arena len {arena_len})"
            ),
            Self::InvalidNodeIndex { idx, arena_len } => {
                write!(f, "Invalid node index {idx} (arena len {arena_len})")
            }
            Self::EmptyTree => write!(f, "Tree is empty — add_root first"),
            Self::MaxChildrenExceeded {
                parent_idx,
                max_children,
            } => write!(
                f,
                "Parent {parent_idx} already has max_children={max_children}"
            ),
            Self::CandidateLengthMismatch { embeddings, labels } => write!(
                f,
                "candidate_embeddings ({embeddings}) and candidate_labels ({labels}) length mismatch"
            ),
            Self::EmptyDeterministicCandidates => {
                write!(f, "deterministic ranking requires at least one candidate")
            }
            Self::InvalidDeterministicIterations => {
                write!(f, "deterministic ranking iterations must be positive")
            }
            Self::DeterministicPriorOutOfRange {
                label,
                prior_micros,
            } => write!(
                f,
                "candidate {label} has prior_micros={prior_micros}, expected <= 1000000"
            ),
            Self::DuplicateDeterministicLabel { label } => {
                write!(f, "duplicate deterministic candidate label: {label}")
            }
        }
    }
}

impl std::error::Error for MctsError {}
