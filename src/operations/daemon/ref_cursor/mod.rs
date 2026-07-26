use crate::error::GitAiError;
use crate::model::domain::{Confidence, FamilyKey, FamilyState, NormalizedCommand, RefChange};
use crate::operations::daemon::analyzers::{command_args, normalized_args};
use crate::operations::daemon::parsed_invocation_for_normalized_command;
use crate::operations::git::cli_parser::{
    explicit_rebase_branch_arg, parse_git_cli_args, summarize_rebase_args,
};
use crate::operations::git::find_repository_in_path;
use crate::operations::git::oid::{
    is_full_oid as is_valid_git_oid, is_non_zero_oid as valid_non_zero_oid,
};
use crate::operations::git::repo_state::{common_dir_for_worktree, git_dir_for_worktree};
use crate::operations::git::sequencer_args::{
    cherry_pick_source_args_from_command_args, revert_source_args_from_command_args,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

mod branch_lifecycle;
mod cherry_pick_revert;
mod command_matchers;
mod cursor_core;
mod enrichment;
mod rebase_pull;
mod reflog_io;
mod span_clamping;
mod stash_update_ref;
mod types;

pub use types::RefCursor;

pub(crate) use reflog_io::capture_reflog_start_offsets_for_worktree;

// Re-export the module-private items so every submodule's `use super::*;` can
// see the shared types and free functions defined in sibling modules.
use command_matchers::*;
use reflog_io::*;
use span_clamping::*;
use types::*;

#[cfg(test)]
mod tests_commit;
#[cfg(test)]
mod tests_fixtures;
#[cfg(test)]
mod tests_pull_stash;
#[cfg(test)]
mod tests_rebase_cherry_pick;
#[cfg(test)]
mod tests_reflog_rebase;
#[cfg(test)]
mod tests_reset_update_ref;
