# mcts-fast-core

Arena-based PUCT MCTS with embedding-guided (cosine) simulation.

Shared Rust core for [`mcts-fast-py`](../mcts-fast-py) and [`mcts-fast-node`](../mcts-fast-node).

## Role vs gateway MCTS

| Surface | Location | Role |
|---------|----------|------|
| **mcts-fast-core** | `office-parsers-rs/mcts-fast-core` | Embedding-space ranking / KB path search over a **caller-built** tree |
| **Gateway MCTS** | `mycelia-gateway/src/reasoning/mcts/` | Ontology-graph navigation with legal `MctsAction` expansion + Crepe |

This crate does **not** invent graph edges. Callers expand legal children via `add_child`; the engine selects with PUCT, simulates with cosine similarity to the query embedding, and backpropagates discounted rewards.

**Do not merge** the engines: gateway nodes carry ontology evidence / entity hashes and expand Crepe-legal actions. Shared formula shape lives in `puct_score` here; gateway keeps its own copy for the ontology-nav hot path (linking `mcts-fast-core` into the gateway is intentionally avoided — different node payloads, Crepe action expansion, and workspace boundaries).

## Config (additive)

| Field | Default | Meaning |
|-------|---------|---------|
| `iterations` | 200 | Simulation budget |
| `exploration_constant` | 1.414 | c_puct |
| `max_depth` | 20 | Selection depth cap |
| `gamma` | 0.99 | Depth discount |
| `max_children` | 64 | Expansion hard cap |
| `num_parallel` | 1 | Parallel sims; `>1` enables virtual loss |
| `pw_k` | 0.0 | Progressive widening k (`0` = off) |
| `pw_alpha` | 0.5 | Progressive widening α |

## Invariants

- PUCT uses `parent_visits.max(1)` so the first descent explores by prior
- `max_children` rejected at expansion (not silently ignored)
- Search budget = `iterations` simulations
- Best path = greedy most-visited walk; stops if no child has visits > 0
- Negative priors clamped to 0
- Parallel search applies virtual loss **incrementally during descent** (after each step) and clears it on backprop
- Progressive widening (when `pw_k > 0`) scores only the top-prior `floor(k * N^α)` children
- Cosine similarity to the query embedding remains the default leaf reward

## Deferred (by design)

| Gap | Status | Why |
|-----|--------|-----|
| **Legal-edge expansion** | by-design | Caller / gateway owns legal children via `add_child`; this crate never invents graph neighbors |
| **RAVE / AMAF** | deferred | Needs per-action AMAF stats orthogonal to per-node `visit_count` / `total_reward`. Labels are not unique action IDs, and there is no separate move identity — bolting AMAF onto label strings would silently merge unrelated edges |
| **Graph transposition table** | deferred | No stable DAG node identity: `label` can repeat under different parents; arena indices are tree-local. A TT would require a caller-supplied key (IRI / entity hash) that this ranking API does not carry |
| **Full gateway merge** | by-design | Crepe-legal `MctsAction` expansion + ontology evidence stay in `mycelia-gateway`; keep `puct_score` formula-aligned by duplication, not crate linkage |

## Tests

```bash
CARGO_TARGET_DIR=/Users/josaum/projects/mycelia-workspace/target \
  cargo test -p mcts-fast-core --manifest-path office-parsers-rs/Cargo.toml
```
