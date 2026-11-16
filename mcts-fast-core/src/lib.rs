//! Sovereign PUCT MCTS engine for embedding-guided candidate ranking.
//!
//! This crate owns the hot inner loop used by `mcts-fast-py` and `mcts-fast-node`.
//! It is **not** the gateway ontology-navigation MCTS
//! (`mycelia-gateway/src/reasoning/mcts/`); that engine expands legal graph
//! actions under Crepe constraints. Here the caller supplies the legal tree
//! (via `add_root` / `add_child`); the engine only selects, simulates, and
//! backpropagates over that pre-built frontier.
//!
//! # Boundary (do not merge)
//!
//! | Concern | Owner |
//! |---------|-------|
//! | Legal edge / action expansion | Caller (Python navigate, Flight) or gateway Crepe MCTS |
//! | PUCT + cosine leaf reward + virtual loss | **this crate** |
//! | OWL2-RL reasoning | Crepe in gateway only |
//!
//! Shared math (PUCT formula shape, virtual-loss semantics) is intentionally
//! duplicated rather than linked into the gateway: gateway nodes carry
//! ontology evidence / entity hashes that this ranking engine does not need,
//! and Crepe-legal action expansion must stay gateway-owned.
//!
//! # Deferred SOTA (documented, not implemented here)
//!
//! - **RAVE/AMAF** — needs per-action stats; labels are not unique move IDs.
//! - **Transposition table** — no stable DAG identity beyond tree-local arena
//!   indices; would require a caller-supplied key this API does not expose.
//!
//! # Invariants
//!
//! - Selection uses AlphaZero-style PUCT with `parent_visits.max(1)` so the
//!   first descent explores rather than collapsing to child index 0.
//! - `max_children` is enforced at expansion time (illegal branching rejected).
//! - Search stops after `iterations` simulations (budget).
//! - Best path is the greedy most-visited walk from the root (standard MCTS
//!   action selection), with provenance labels preserved.
//! - `num_parallel > 1` applies virtual loss **incrementally during descent**
//!   so concurrent simulations see the penalty before the next `select_child`.
//! - Progressive widening (`pw_k > 0`) limits selection to the top-prior
//!   `floor(k * N^α)` children; cosine remains the default leaf reward.

#![deny(missing_docs)]
#![deny(unsafe_code)]

mod engine;
mod math;
mod types;

pub use engine::{
    mcts_rank, rank_deterministic, MctsEngine, NodeInfo, RankOutput, RankedCandidate, SearchResult,
};
pub use math::{fast_cosine, l2_norm, puct_score};
pub use types::{
    DeterministicCandidate, DeterministicRankConfig, DeterministicRankOutput,
    DeterministicRankedCandidate, MctsConfig, MctsError, MctsNode,
};
