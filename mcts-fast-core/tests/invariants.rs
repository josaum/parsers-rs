//! Invariant tests that encode WHY MCTS behavior matters for graph navigation.
//!
//! These are not smoke tests: each failure mode maps to a real footgun
//! (illegal branching, unbounded search, inverted priors, broken selection).

use mcts_fast_core::{mcts_rank, MctsConfig, MctsEngine, MctsError};

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n < 1e-9 {
        v.to_vec()
    } else {
        v.iter().map(|x| x / n).collect()
    }
}

#[test]
fn max_children_rejects_illegal_expansion() {
    // WHY: graph legality is caller-supplied; the engine must still enforce
    // the configured branching budget so search cannot explode sideways.
    let config = MctsConfig {
        max_children: 2,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let root = engine.add_root(unit(&[1.0, 0.0]), "root".into());
    engine
        .add_child(root, unit(&[0.9, 0.1]), "a".into(), 0.5)
        .expect("first child");
    engine
        .add_child(root, unit(&[0.8, 0.2]), "b".into(), 0.5)
        .expect("second child");
    let err = engine
        .add_child(root, unit(&[0.7, 0.3]), "c".into(), 0.5)
        .expect_err("third child must be rejected");
    assert!(matches!(err, MctsError::MaxChildrenExceeded { .. }));
    assert_eq!(engine.node_count(), 3);
}

#[test]
fn invalid_parent_rejected() {
    let mut engine = MctsEngine::new(MctsConfig::default());
    engine.add_root(unit(&[1.0, 0.0]), "root".into());
    let err = engine
        .add_child(99, unit(&[0.0, 1.0]), "ghost".into(), 1.0)
        .expect_err("bogus parent");
    assert!(matches!(err, MctsError::InvalidParentIndex { .. }));
}

#[test]
fn empty_tree_search_errors() {
    let mut engine = MctsEngine::new(MctsConfig::default());
    let err = engine.search(&unit(&[1.0, 0.0])).expect_err("empty");
    assert!(matches!(err, MctsError::EmptyTree));
}

#[test]
fn budget_iterations_bound_search() {
    // WHY: callers rely on `iterations` as a hard compute budget for Flight
    // and Leio retrieve paths — overrunning burns latency SLOs.
    let config = MctsConfig {
        iterations: 17,
        exploration_constant: 1.414,
        max_depth: 4,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let root = engine.add_root(unit(&[1.0, 0.0]), "query".into());
    for i in 0..5 {
        let emb = unit(&[1.0 - i as f32 * 0.1, i as f32 * 0.1]);
        engine
            .add_child(root, emb, format!("c{i}"), 0.5)
            .expect("child");
    }
    let result = engine.search(&unit(&[1.0, 0.0])).expect("search");
    assert_eq!(result.iterations, 17);
    let total_child_visits: u32 = result.visit_counts.values().sum();
    // Each iteration backprops through one root child → child visits == iterations.
    assert_eq!(total_child_visits, 17);
}

#[test]
fn max_depth_stops_descent() {
    // WHY: without a depth cap, deep pre-expanded graphs turn selection into
    // an unbounded walk; Leio/navigate pass max_depth intentionally.
    let config = MctsConfig {
        iterations: 50,
        max_depth: 1,
        exploration_constant: 2.0,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let root = engine.add_root(unit(&[1.0, 0.0]), "root".into());
    let mid = engine
        .add_child(root, unit(&[0.9, 0.1]), "mid".into(), 1.0)
        .unwrap();
    engine
        .add_child(mid, unit(&[0.0, 1.0]), "deep".into(), 1.0)
        .unwrap();

    let result = engine.search(&unit(&[1.0, 0.0])).unwrap();
    // With max_depth=1, selection stops at the first-level child; best path
    // should not walk into "deep" via visit-greedy extraction from a never-
    // visited grandchild (visits stay 0 → path may still include it if we
    // greedily walk empty children). Assert depth-1 child got all visits.
    assert_eq!(result.visit_counts.get("mid").copied().unwrap_or(0), 50);
    let deep = engine.get_node(2).unwrap();
    assert_eq!(
        deep.visit_count, 0,
        "depth-2 node must never be selected when max_depth=1"
    );
}

#[test]
fn puct_prefers_high_prior_when_unvisited() {
    // WHY: priors encode graph/policy legality weights from the caller
    // (cosine to query, GEPA blend). Unvisited children must be ordered by
    // prior, not arbitrarily by insertion order.
    let config = MctsConfig {
        iterations: 1,
        exploration_constant: 10.0,
        max_depth: 2,
        gamma: 1.0,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let root = engine.add_root(unit(&[1.0, 0.0]), "q".into());
    // Low prior first (insertion order trap), high prior second.
    engine
        .add_child(root, unit(&[0.0, 1.0]), "low".into(), 0.01)
        .unwrap();
    engine
        .add_child(root, unit(&[1.0, 0.0]), "high".into(), 0.99)
        .unwrap();

    let result = engine.search(&unit(&[1.0, 0.0])).unwrap();
    assert_eq!(
        result.visit_counts.get("high").copied().unwrap_or(0),
        1,
        "first iteration must select the high-prior child"
    );
    assert_eq!(result.visit_counts.get("low").copied().unwrap_or(0), 0);
}

#[test]
fn puct_prefers_explored_high_reward() {
    // WHY: after exploration, exploitation must dominate — otherwise ranking
    // (mcts_rank / audit candidates) ignores evidence and stays prior-greedy.
    let config = MctsConfig {
        iterations: 200,
        exploration_constant: 0.1, // low exploration → exploit wins
        max_depth: 2,
        gamma: 1.0,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let q = unit(&[1.0, 0.0]);
    let root = engine.add_root(q.clone(), "q".into());
    // Near-orthogonal (low reward) with high prior — a trap for prior-only selection.
    engine
        .add_child(root, unit(&[0.0, 1.0]), "trap".into(), 0.9)
        .unwrap();
    // Aligned with query (high cosine reward) with modest prior.
    engine
        .add_child(root, unit(&[1.0, 0.0]), "good".into(), 0.2)
        .unwrap();

    let result = engine.search(&q).unwrap();
    let good = result.visit_counts.get("good").copied().unwrap_or(0);
    let trap = result.visit_counts.get("trap").copied().unwrap_or(0);
    assert!(
        good > trap,
        "high-reward child must accumulate more visits than high-prior trap (good={good}, trap={trap})"
    );
    assert_eq!(result.best_path.last().map(String::as_str), Some("good"));
}

#[test]
fn negative_prior_clamped() {
    let mut engine = MctsEngine::new(MctsConfig::default());
    let root = engine.add_root(unit(&[1.0, 0.0]), "q".into());
    let idx = engine
        .add_child(root, unit(&[1.0, 0.0]), "n".into(), -0.5)
        .unwrap();
    assert_eq!(engine.get_node(idx).unwrap().prior, 0.0);
}

#[test]
fn best_path_provenance_is_visit_greedy() {
    // WHY: consumers (Leio, Flight, health_audit) treat best_path as the
    // chosen reasoning trail — it must follow most-visited children, not Q.
    let config = MctsConfig {
        iterations: 80,
        exploration_constant: 1.0,
        max_depth: 3,
        gamma: 1.0,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let q = unit(&[1.0, 0.0]);
    let root = engine.add_root(q.clone(), "query".into());
    engine
        .add_child(root, unit(&[1.0, 0.0]), "doc_a".into(), 0.8)
        .unwrap();
    engine
        .add_child(root, unit(&[0.2, 0.8]), "doc_b".into(), 0.2)
        .unwrap();
    let result = engine.search(&q).unwrap();
    assert_eq!(result.best_path[0], "query");
    assert!(
        result.best_path.len() >= 2,
        "path must include at least one child"
    );
    let winner = &result.best_path[1];
    let winner_visits = result.visit_counts[winner];
    for (label, visits) in &result.visit_counts {
        assert!(
            winner_visits >= *visits,
            "best_path child {winner} must be most-visited (lost to {label})"
        );
    }
}

#[test]
fn mcts_rank_orders_by_visits() {
    let q = unit(&[1.0, 0.0]);
    let candidates = vec![unit(&[0.0, 1.0]), unit(&[1.0, 0.0]), unit(&[0.5, 0.5])];
    let labels = vec!["far".into(), "near".into(), "mid".into()];
    let json = mcts_rank(&q, candidates, labels, 100, 0.5).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let ranked = v["ranked"].as_array().unwrap();
    assert_eq!(ranked[0]["label"], "near");
    assert!(ranked[0]["visits"].as_u64().unwrap() >= ranked[1]["visits"].as_u64().unwrap());
}

#[test]
fn mcts_rank_length_mismatch() {
    let err = mcts_rank(
        &unit(&[1.0, 0.0]),
        vec![unit(&[1.0, 0.0])],
        vec!["a".into(), "b".into()],
        10,
        1.414,
    )
    .unwrap_err();
    assert!(matches!(err, MctsError::CandidateLengthMismatch { .. }));
}

#[test]
fn parallel_search_preserves_iteration_budget() {
    // WHY: Flight / navigate treat `iterations` as a hard compute budget.
    // Parallel workers must not overshoot or drop simulations.
    let config = MctsConfig {
        iterations: 64,
        num_parallel: 4,
        exploration_constant: 1.414,
        max_depth: 2,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let q = unit(&[1.0, 0.0]);
    let root = engine.add_root(q.clone(), "q".into());
    for i in 0..8 {
        engine
            .add_child(
                root,
                unit(&[1.0 - i as f32 * 0.05, i as f32 * 0.05]),
                format!("c{i}"),
                0.5,
            )
            .unwrap();
    }
    let result = engine.search(&q).unwrap();
    assert_eq!(result.iterations, 64);
    let total: u32 = result.visit_counts.values().sum();
    assert_eq!(
        total, 64,
        "each sim must backprop through exactly one root child"
    );
    assert_eq!(engine.get_node(root).unwrap().virtual_loss, 0);
}

#[test]
fn progressive_widening_hides_low_prior_early() {
    // WHY: without PW, wide caller-built frontiers burn the budget on
    // low-prior siblings before evidence accumulates. PW must keep early
    // selection inside the top-prior window.
    let config = MctsConfig {
        iterations: 20,
        exploration_constant: 2.0,
        max_depth: 2,
        gamma: 1.0,
        // floor(1.0 * 1^0.5) = 1 → only the single highest-prior child
        // is visible until parent visits grow.
        pw_k: 1.0,
        pw_alpha: 0.5,
        ..MctsConfig::default()
    };
    let mut engine = MctsEngine::new(config);
    let q = unit(&[1.0, 0.0]);
    let root = engine.add_root(q.clone(), "q".into());
    engine
        .add_child(root, unit(&[0.0, 1.0]), "low".into(), 0.01)
        .unwrap();
    engine
        .add_child(root, unit(&[1.0, 0.0]), "high".into(), 0.99)
        .unwrap();
    engine
        .add_child(root, unit(&[0.5, 0.5]), "mid".into(), 0.1)
        .unwrap();

    let result = engine.search(&q).unwrap();
    let high = result.visit_counts.get("high").copied().unwrap_or(0);
    let low = result.visit_counts.get("low").copied().unwrap_or(0);
    let mid = result.visit_counts.get("mid").copied().unwrap_or(0);
    assert!(
        high > low + mid,
        "PW must concentrate early visits on the top-prior child (high={high}, low={low}, mid={mid})"
    );
}

#[test]
fn puct_score_penalizes_virtual_loss() {
    // WHY: virtual loss exists so concurrent descents do not thrash the
    // same path — effective visits must lower the exploration bonus.
    use mcts_fast_core::puct_score;
    let without_vl = puct_score(0.5, 1.0, 10, 0, 1.414);
    let with_vl = puct_score(0.5, 1.0, 10, 3, 1.414);
    assert!(
        with_vl < without_vl,
        "in-flight VL must reduce PUCT (with={with_vl}, without={without_vl})"
    );
}
