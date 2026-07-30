pub(crate) mod alias_parser;
pub mod canonicalize;
pub(crate) mod cat_file;
pub mod cli_parser;
pub mod command_classification;
pub(crate) mod command_policy;
pub mod fast_reader;
pub mod notes_api;
pub(crate) mod notes_store;
pub mod oid;
pub(crate) mod patch_id;
pub mod path_format;
pub mod refs;
pub mod repo_state;
pub mod repository;
pub(crate) mod sequencer_args;

pub mod authorship_traversal;

#[cfg(any(test, feature = "test-support"))]
pub mod test_utils;

#[allow(unused_imports)]
pub use repository::{
    GitAuthorIdentity, find_repository, find_repository_for_file, find_repository_in_path,
    from_bare_repository, group_files_by_repository,
};
pub mod repo_storage;
pub mod status;
pub mod sync_authorship;
pub(crate) mod trace2_validation;
