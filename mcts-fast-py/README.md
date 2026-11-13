# mcts-fast

Arena-based PUCT Monte Carlo Tree Search with embedding-guided simulation, powered
by Rust and exposed to Python via PyO3 (`abi3`, Python ≥ 3.8).

Part of the sovereign [parsers-rs](https://github.com/josaum/parsers-rs) suite:
one source, one version, one artifact, with cryptographic provenance.

```python
from mcts_fast_py import MctsConfig, MctsEngine, mcts_rank
```

The Rust core lives in [`mcts-fast-core`](../mcts-fast-core). Licensed MIT OR Apache-2.0.
