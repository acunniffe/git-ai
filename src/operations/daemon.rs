// Submodules declared before the original extraction (pre-existing).
pub mod analyzers;
pub mod bash_sessions;
pub mod checkpoint;
pub mod control_api;
pub mod coordinator;
pub mod daemon_log_layer;
pub mod family_actor;
pub mod git_backend;
pub mod global_actor;
pub mod reducer;
pub mod ref_cursor;
pub mod rewrite_metrics;
pub mod sentry_layer;
pub mod stream_worker;
pub mod sweep_coordinator;
pub mod telemetry_handle;
pub mod telemetry_worker;
pub mod test_sync;
pub mod trace_normalizer;
pub mod transcript_redaction;

// New submodules created by decomposing the monolithic daemon.rs body.
// The impl-block-only modules (actor_coordinator_*) add methods to
// ActorDaemonCoordinator; they need no re-export since callers access those
// methods via the type itself.
pub(crate) mod actor_coordinator_base;
pub(crate) mod actor_coordinator_control;
pub(crate) mod actor_coordinator_drain;
pub(crate) mod actor_coordinator_ingest;
pub(crate) mod actor_coordinator_query;
pub(crate) mod actor_coordinator_rewrites;
pub(crate) mod actor_coordinator_seq;
pub(crate) mod actor_coordinator_side_effects;
pub(crate) mod actor_coordinator_trace;
pub(crate) mod actor_coordinator_worktree;
pub(crate) mod actor_types;
// Compiled out of test-support library builds: the checkpoint path's
// test-support stub never consults the limiter, while lib unit tests
// (cfg(test)) exercise it directly.
#[cfg(any(test, not(feature = "test-support")))]
pub(crate) mod agent_usage_limiter;
pub(crate) mod attribution_self_check;
pub(crate) mod checkpoint_stream_authority;
pub(crate) mod cherry_pick_helpers;
pub(crate) mod client_helpers;
pub(crate) mod daemon_config;
pub(crate) mod git_op_side_effects;
pub(crate) mod lifecycle;
pub(crate) mod log_setup;
pub(crate) mod memory_watchdog;
pub(crate) mod parent_diff_batch;
pub(crate) mod revert_rebase_helpers;
pub(crate) mod self_check;
pub(crate) mod side_effect_helpers;
pub(crate) mod side_effects_commit;
pub(crate) mod side_effects_git_ops;
pub(crate) mod socket_listeners;
pub(crate) mod trace_helpers;

// Re-export types and functions that external code accesses as
// `crate::operations::daemon::X`.  Only modules that define such public items
// need a glob re-export here; impl-block-only modules are omitted.
#[doc(hidden)]
pub use actor_types::*;
#[doc(hidden)]
pub use client_helpers::*;
#[doc(hidden)]
pub use daemon_config::*;
#[doc(hidden)]
pub use git_op_side_effects::*;
#[doc(hidden)]
pub use lifecycle::*;
#[doc(hidden)]
pub use log_setup::*;
#[doc(hidden)]
pub use side_effect_helpers::*;
#[doc(hidden)]
pub use socket_listeners::*;
#[doc(hidden)]
pub use trace_helpers::*;

// Public re-exports (original, unchanged).
pub use control_api::{
    BashSessionQueryResponse, BashSnapshotQueryResponse, ControlRequest, ControlResponse,
    FamilyStatus,
};
// `TelemetryEnvelope` is a pure DTO owned by `model`; re-exported here so the
// existing `operations::daemon::TelemetryEnvelope` path keeps resolving.
pub use crate::model::telemetry::TelemetryEnvelope;

// Test modules.
#[cfg(test)]
mod checkpoint_stream_authority_tests;
#[cfg(test)]
mod stream_worker_tests;
#[cfg(test)]
mod telemetry_worker_tests;

#[cfg(test)]
mod tests_coordinator;
#[cfg(test)]
mod tests_coordinator_drain;
#[cfg(test)]
mod tests_daemon_units;
#[cfg(test)]
mod tests_ingress;
