//! The [`Repository`] abstraction over an on-disk git repository, plus the
//! typed git-object wrappers, identity resolution, and repository-discovery
//! helpers.
//!
//! This module was split out of a single large file; the submodules below hold
//! focused pieces of the same public surface, and everything that used to be
//! `pub` here is re-exported so `operations::git::repository::*` import paths
//! keep working. The git spawn layer (`exec_git*`, `spawn_git*`, internal-git
//! profiles/env) lives in `crate::clients::git_cli`
//! for the same reason.

mod ancestry;
mod commits;
mod core;
mod diff;
mod discovery;
mod discovery_no_exec;
mod git_objects;
mod identity;
mod object_reads;
mod policy;

pub use commits::{Commit, CommitRange, CommitRangeIterator, Object, Parents};
pub use core::Repository;
pub use diff::{parse_diff_added_lines_with_insertions, parse_git_version};
pub(crate) use discovery::batch_read_paths_at_treeishes;
pub use discovery::{
    find_repository, find_repository_for_file, find_repository_in_path, group_files_by_repository,
    resolve_command_base_dir,
};
pub use discovery_no_exec::{
    config_get_str_for_path_no_git_exec, discover_repository_in_path_no_git_exec,
    from_bare_repository, worktree_storage_ai_dir,
};
pub use git_objects::{Blob, Reference, References, Tree, TreeEntry};
pub use identity::{
    GitAuthorIdentity, GitConfigIdentityResolution, GitIdentityResolution,
    current_git_committer_identity_resolution, global_git_config_committer_identity,
    global_git_config_identity_resolution, parse_git_var_identity, resolve_api_author_identity,
};
pub(crate) use policy::{
    discover_repository_policy_location_no_git_exec, load_repository_policy_context_no_git_exec,
};
