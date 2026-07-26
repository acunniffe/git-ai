mod attestation;
mod authorship_log_conversion;
mod authorship_log_split;
mod blame_loader;
mod carryover;
mod carryover_merge;
mod carryover_snapshot;
mod conflict_markers;
mod diff_utils;
mod foreign_prompt_loader;
mod initial_attribution;
mod log_split_stages;
mod merge;
mod persisted_log_loader;
mod types;
mod working_log_loader;
mod working_log_state;

pub use carryover::{restore_virtual_attribution_carryover, restore_working_log_carryover};
pub use carryover_snapshot::checkout_merge_final_state_snapshot;
pub use conflict_markers::{content_has_conflict_markers, strip_conflict_markers_keep_ours};
pub use merge::merge_attributions_favoring_first;
pub(crate) use types::AuthorshipLogDiffContext;
pub use types::VirtualAttributions;

pub(crate) use attestation::committed_hunks_from_diff_result;

#[cfg(test)]
mod tests_attestation;
#[cfg(test)]
mod tests_carryover_merge;
