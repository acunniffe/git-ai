//! Core entities, DTOs, and persistence-facing types.
//!
//! `model` holds the data shapes that flow between subsystems: the authorship
//! log and its serialization, working-log checkpoints, the daemon's normalized
//! command/event domain, stream/transcript types, and API request/response
//! DTOs. SQLite access lives in [`repository`].
//!
//! The previous module locations re-export from here (e.g.
//! `crate::model::working_log`, `crate::model::api_types`), so both paths are
//! valid during the migration.

pub mod api_types;
pub mod attribution;
pub mod attribution_tracker;
pub mod authorship_log;
pub mod authorship_log_serialization;
pub mod checkpoint_delivery;
pub mod checkpoint_request;
pub mod clock;
pub mod daemon_control;
pub mod diff_json;
pub mod domain;
pub mod hunk_shift;
pub mod imara_diff_utils;
pub mod metrics;
pub mod move_detection;
pub mod repository;
pub mod session_recovery_candidate;
pub mod stat_snapshot;
pub mod stream_types;
pub mod stream_watermark;
pub mod telemetry;
pub mod transcript;
pub mod working_log;
