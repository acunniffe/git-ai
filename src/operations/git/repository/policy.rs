use super::discovery_no_exec::{
    canonicalize_workdir, discover_repository_paths_no_git_exec, git_config_file_for_repo_paths,
    require_dir,
};
use crate::config;
use crate::error::GitAiError;
use std::path::{Path, PathBuf};

pub(crate) struct RepositoryPolicyContext {
    canonical_workdir: PathBuf,
    remotes: Vec<(String, String)>,
}

pub(crate) struct RepositoryPolicyLocation {
    canonical_workdir: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
}

impl RepositoryPolicyLocation {
    pub(crate) fn repository_root(&self) -> &Path {
        &self.canonical_workdir
    }
}

impl RepositoryPolicyContext {
    pub(crate) fn is_collection_allowed(&self, config: &config::Config) -> bool {
        config
            .is_allowed_repository_with_context(Some(&self.remotes), Some(&self.canonical_workdir))
    }
}

pub(crate) fn discover_repository_policy_location_no_git_exec(
    path: &Path,
) -> Result<RepositoryPolicyLocation, GitAiError> {
    let paths = discover_repository_paths_no_git_exec(path)?;
    require_dir("Git directory", &paths.git_dir)?;
    require_dir("Git common directory", &paths.git_common_dir)?;
    require_dir("Work directory", &paths.workdir)?;
    let canonical_workdir = canonicalize_workdir(&paths.workdir)?;
    Ok(RepositoryPolicyLocation {
        canonical_workdir,
        git_dir: paths.git_dir,
        git_common_dir: paths.git_common_dir,
    })
}

pub(crate) fn load_repository_policy_context_no_git_exec(
    location: &RepositoryPolicyLocation,
) -> Result<RepositoryPolicyContext, GitAiError> {
    let config = git_config_file_for_repo_paths(&location.git_dir, &location.git_common_dir)?;
    let remotes = remotes_with_urls_from_config(&config);
    Ok(RepositoryPolicyContext {
        canonical_workdir: location.canonical_workdir.clone(),
        remotes,
    })
}

pub(super) fn remotes_with_urls_from_config(
    config: &gix_config::File<'static>,
) -> Vec<(String, String)> {
    config
        .sections()
        .filter_map(|section| {
            if !section.header().name().eq_ignore_ascii_case(b"remote") {
                return None;
            }
            let name = section.header().subsection_name()?;
            let url = section.body().value("url")?;
            Some((name.to_string(), url.to_string()))
        })
        .collect()
}
