pub mod cli_parser;
pub mod command_classification;
pub mod fast_reader;
pub mod notes_api;
pub mod refs;
pub(crate) mod reftable;
pub mod repo_state;
pub mod repository;

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
