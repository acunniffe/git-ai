//! Graphite workflow attribution suite.
//!
//! An exhaustive, empirically-grounded scenario suite proving that AI
//! attribution survives (or documenting exactly where it does not survive)
//! every Graphite workflow permutation. See `PLAN.md` in this directory for
//! the full design, `scenarios.json` for the 540-scenario manifest, and
//! `generate_matrix.py` for the manifest's single source of truth.
//!
//! Layout:
//! - [`scenario`]: manifest loader + typed dimension accessors;
//! - [`stackbuilder`]: pre-workflow repo state (stack shapes, AI files,
//!   remote, trunk divergence);
//! - [`gt_sim`]: faithful replays of gt's git command sequences, plus the
//!   per-scenario driver `gt_sim::run_scenario`;
//! - [`assertions`]: non-panicking post-workflow invariant checks;
//! - family test files (`sync_restack.rs`, ...) are added on top of this
//!   harness core, one `#[test]` per (family x observation x repeat) bucket.

// The harness core lands ahead of the family test files, so most of its
// surface is intentionally not yet referenced.
#[allow(dead_code)]
pub(crate) mod assertions;
#[allow(dead_code)]
pub(crate) mod gt_sim;
#[allow(dead_code)]
pub(crate) mod scenario;
#[allow(dead_code)]
pub(crate) mod stackbuilder;

mod smoke;

mod conflicts;
mod create_modify_submit;
mod lifecycle;
mod restack;
mod sync_ff;
mod sync_restack;
mod undo_move_misc;
mod worktree_shapes;
