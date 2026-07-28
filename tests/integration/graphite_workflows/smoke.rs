//! End-to-end smoke test for the harness core: one scenario through the full
//! pipeline (stack build -> workflow replay -> attribution assertions).

use super::{gt_sim, scenario};

/// GT-SYNC_RESTACK-001: single branch, committed AI work, diverged trunk,
/// traced, once — the canonical `gt sync` restack (fetch, trunk `update-ref`,
/// synthetic-base commit-tree/merge-tree rewrite, checked-out branch moved via
/// `reset -q --keep`).
#[test]
fn smoke_gt_sync_restack_001() {
    let scenario = scenario::by_id("GT-SYNC_RESTACK-001");
    let violations = gt_sim::run_scenario(&scenario);
    assert!(
        violations.is_empty(),
        "{} attribution violations:\n{}",
        scenario.id,
        violations.join("\n")
    );
}
