//! Repository discovery that never shells out to git.
//!
//! Walks the filesystem to locate the git/common dirs and workdir for a path
//! (handling worktrees and submodules), builds a [`Repository`] from those
//! paths, and resolves gitconfig via `gix_config` directly. Used by the
//! latency-sensitive daemon paths where spawning git is not acceptable.

use super::core::Repository;
use crate::error::GitAiError;
use crate::operations::git::repo_state::{
    common_dir_for_git_dir, git_dir_for_worktree, is_valid_git_dir, worktree_root_for_path,
};
use crate::operations::git::repo_storage::RepoStorage;
use std::path::{Path, PathBuf};

// Text-dedup constructors shared with `discovery.rs`'s git-exec-based
// discovery, which validates and reports the same paths. Display output is
// unchanged from the sites they replace.
pub(super) fn require_dir(kind: &str, path: &Path) -> Result<(), GitAiError> {
    if !path.is_dir() {
        return Err(GitAiError::Generic(format!(
            "{} does not exist: {}",
            kind,
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn canonicalize_workdir(workdir: &Path) -> Result<PathBuf, GitAiError> {
    workdir.canonicalize().map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to canonicalize working directory {}: {}",
            workdir.display(),
            e
        ))
    })
}

pub(super) fn no_parent_error(kind: &str, path: &Path) -> GitAiError {
    GitAiError::Generic(format!("{} has no parent: {}", kind, path.display()))
}

fn unable_to_resolve(what: &str, path: &Path) -> GitAiError {
    GitAiError::Generic(format!("Unable to resolve {}: {}", what, path.display()))
}

#[doc(hidden)]
pub fn worktree_storage_ai_dir(git_dir: &Path, git_common_dir: &Path) -> PathBuf {
    if git_dir == git_common_dir {
        return git_common_dir.join("ai");
    }

    let worktrees_root = git_common_dir.join("worktrees");
    if let Ok(relative_worktree_path) = git_dir.strip_prefix(&worktrees_root)
        && !relative_worktree_path.as_os_str().is_empty()
    {
        return git_common_dir
            .join("ai")
            .join("worktrees")
            .join(relative_worktree_path);
    }

    let canonical_git_dir = crate::operations::git::canonicalize::canonicalize_or_self(git_dir);
    let canonical_common_dir =
        crate::operations::git::canonicalize::canonicalize_or_self(git_common_dir);

    if canonical_git_dir == canonical_common_dir {
        return git_common_dir.join("ai");
    }

    let canonical_worktrees_root = canonical_common_dir.join("worktrees");
    if let Ok(relative_worktree_path) = canonical_git_dir.strip_prefix(&canonical_worktrees_root)
        && !relative_worktree_path.as_os_str().is_empty()
    {
        return git_common_dir
            .join("ai")
            .join("worktrees")
            .join(relative_worktree_path);
    }

    let fallback_name = git_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".to_string());
    git_common_dir
        .join("ai")
        .join("worktrees")
        .join(fallback_name)
}

pub(super) struct DiscoveredRepositoryPaths {
    pub(super) command_root: PathBuf,
    pub(super) workdir: PathBuf,
    pub(super) git_dir: PathBuf,
    pub(super) git_common_dir: PathBuf,
}

pub(super) fn discover_repository_paths_no_git_exec(
    path: &Path,
) -> Result<DiscoveredRepositoryPaths, GitAiError> {
    let start = if path.file_name().and_then(|name| name.to_str()) == Some(".git") || path.is_dir()
    {
        path.to_path_buf()
    } else {
        path.parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf())
    };

    if start.file_name().and_then(|name| name.to_str()) == Some(".git") {
        // An invalid directory-form .git (no HEAD) falls through to the
        // worktree-root walk below, which resolves to the enclosing repo.
        if start.is_dir() && is_valid_git_dir(&start) {
            let workdir = start.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Git directory has no parent workdir: {}",
                    start.display()
                ))
            })?;
            let git_common_dir = common_dir_for_git_dir(&start)
                .ok_or_else(|| unable_to_resolve("common dir for git dir", &start))?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: workdir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir: start,
                git_common_dir,
            });
        }

        if start.is_file() {
            let workdir = start.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    ".git file has no parent workdir: {}",
                    start.display()
                ))
            })?;
            let git_dir = git_dir_for_worktree(workdir)
                .ok_or_else(|| unable_to_resolve("git dir for worktree", workdir))?;
            let git_common_dir = common_dir_for_git_dir(&git_dir)
                .ok_or_else(|| unable_to_resolve("common dir for git dir", &git_dir))?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: workdir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir,
                git_common_dir,
            });
        }
    }

    if let Some(worktree_root) = worktree_root_for_path(&start) {
        let git_dir = git_dir_for_worktree(&worktree_root)
            .ok_or_else(|| unable_to_resolve("git dir for worktree", &worktree_root))?;
        let git_common_dir = common_dir_for_git_dir(&git_dir)
            .ok_or_else(|| unable_to_resolve("common dir for git dir", &git_dir))?;
        return Ok(DiscoveredRepositoryPaths {
            command_root: worktree_root.clone(),
            workdir: worktree_root,
            git_dir,
            git_common_dir,
        });
    }

    let mut current = Some(start.as_path());
    while let Some(dir) = current {
        if is_valid_git_dir(dir) && dir.join("objects").is_dir() {
            let workdir = dir
                .parent()
                .ok_or_else(|| no_parent_error("Git directory", dir))?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: dir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir: dir.to_path_buf(),
                git_common_dir: dir.to_path_buf(),
            });
        }
        current = dir.parent();
    }

    Err(GitAiError::Generic(format!(
        "No git repository found for path without exec: {}",
        path.display()
    )))
}

pub(super) fn git_config_file_for_repo_paths(
    git_dir: &Path,
    git_common_dir: &Path,
) -> Result<gix_config::File<'static>, GitAiError> {
    let mut config =
        gix_config::File::from_globals().map_err(|e| GitAiError::GixError(e.to_string()))?;

    let home = dirs::home_dir();
    let options = gix_config::file::init::Options {
        includes: gix_config::file::includes::Options::follow(
            gix_config::path::interpolate::Context {
                home_dir: home.as_deref(),
                ..Default::default()
            },
            gix_config::file::includes::conditional::Context {
                git_dir: Some(git_dir),
                branch_name: None,
            },
        ),
        ..Default::default()
    };

    config
        .resolve_includes(options)
        .map_err(|e| GitAiError::GixError(e.to_string()))?;

    let local_config_path = git_common_dir.join("config");
    let local_config =
        Repository::load_optional_config_file(&local_config_path, gix_config::Source::Local)?;
    let worktree_config_enabled = local_config
        .as_ref()
        .and_then(|cfg| cfg.boolean("extensions.worktreeConfig"))
        .and_then(Result::ok)
        .unwrap_or(false);

    if let Some(mut local_config) = local_config {
        local_config
            .resolve_includes(options)
            .map_err(|e| GitAiError::GixError(e.to_string()))?;
        config.append(local_config);
    }

    if worktree_config_enabled {
        let worktree_config_path = git_dir.join("config.worktree");
        if let Some(mut worktree_config) = Repository::load_optional_config_file(
            &worktree_config_path,
            gix_config::Source::Worktree,
        )? {
            worktree_config
                .resolve_includes(options)
                .map_err(|e| GitAiError::GixError(e.to_string()))?;
            config.append(worktree_config);
        }
    }

    config.append(
        gix_config::File::from_environment_overrides()
            .map_err(|e| GitAiError::GixError(e.to_string()))?,
    );

    Ok(config)
}

pub fn config_get_str_for_path_no_git_exec(
    path: &Path,
    key: &str,
) -> Result<Option<String>, GitAiError> {
    let paths = discover_repository_paths_no_git_exec(path)?;
    git_config_file_for_repo_paths(&paths.git_dir, &paths.git_common_dir)
        .map(|cfg| cfg.string(key).map(|cow| cow.to_string()))
}

pub(super) fn repository_object_hash_kind_for_path_no_git_exec(
    path: &Path,
) -> Result<gix_index::hash::Kind, GitAiError> {
    match config_get_str_for_path_no_git_exec(path, "extensions.objectformat")?
        .as_deref()
        .map(str::trim)
    {
        None | Some("") | Some("sha1") => Ok(gix_index::hash::Kind::Sha1),
        Some("sha256") => Err(GitAiError::Generic(
            "SHA-256 repositories are not supported while reading the git index".to_string(),
        )),
        Some(other) => Err(GitAiError::Generic(format!(
            "Unsupported git object format '{}' while reading index",
            other
        ))),
    }
}

#[allow(dead_code)]
pub fn from_bare_repository(git_dir: &Path) -> Result<Repository, GitAiError> {
    let workdir = git_dir
        .parent()
        .ok_or_else(|| GitAiError::Generic("Git directory has no parent".to_string()))?
        .to_path_buf();
    let global_args = vec!["-C".to_string(), git_dir.to_string_lossy().to_string()];

    let canonical_workdir = crate::operations::git::canonicalize::canonicalize_or_self(&workdir);

    let worktree_ai_dir = worktree_storage_ai_dir(git_dir, git_dir);
    let storage = if worktree_ai_dir == git_dir.join("ai") {
        RepoStorage::for_repo_path(git_dir, &workdir)?
    } else {
        RepoStorage::for_isolated_worktree_storage(&worktree_ai_dir, &workdir)?
    };

    Ok(Repository {
        global_args,
        storage,
        git_dir: git_dir.to_path_buf(),
        git_common_dir: git_dir.to_path_buf(),
        pre_command_base_commit: None,
        pre_command_refname: None,
        pre_reset_target_commit: None,
        pre_update_ref_refname: None,
        pre_update_ref_old_target: None,
        pre_update_ref_affects_checked_out_branch: None,
        workdir,
        canonical_workdir,
        cached_author_identity: std::sync::OnceLock::new(),
    })
}

pub(super) fn repository_from_discovered_paths(
    command_root: &Path,
    workdir: &Path,
    git_dir: &Path,
    git_common_dir: &Path,
) -> Result<Repository, GitAiError> {
    require_dir("Git directory", git_dir)?;
    require_dir("Git common directory", git_common_dir)?;
    require_dir("Work directory", workdir)?;

    let canonical_workdir = canonicalize_workdir(workdir)?;

    let worktree_ai_dir = worktree_storage_ai_dir(git_dir, git_common_dir);
    let storage = if worktree_ai_dir == git_dir.join("ai") {
        RepoStorage::for_repo_path(git_dir, workdir)?
    } else {
        RepoStorage::for_isolated_worktree_storage(&worktree_ai_dir, workdir)?
    };

    Ok(Repository {
        global_args: vec!["-C".to_string(), command_root.to_string_lossy().to_string()],
        storage,
        git_dir: git_dir.to_path_buf(),
        git_common_dir: git_common_dir.to_path_buf(),
        pre_command_base_commit: None,
        pre_command_refname: None,
        pre_reset_target_commit: None,
        pre_update_ref_refname: None,
        pre_update_ref_old_target: None,
        pre_update_ref_affects_checked_out_branch: None,
        workdir: workdir.to_path_buf(),
        canonical_workdir,
        cached_author_identity: std::sync::OnceLock::new(),
    })
}

pub fn discover_repository_in_path_no_git_exec(path: &Path) -> Result<Repository, GitAiError> {
    let paths = discover_repository_paths_no_git_exec(path)?;
    repository_from_discovered_paths(
        &paths.command_root,
        &paths.workdir,
        &paths.git_dir,
        &paths.git_common_dir,
    )
}

/// Check if any directory between `workdir` and `file_path` contains a `.git`
/// entry that represents a **separate** git repository boundary.
///
/// Valid `.git` directories (nested independent repos, i.e. containing a
/// `HEAD` file) and `.git` files that point to a *linked worktree* (i.e.,
/// `gitdir: .../worktrees/…`) are treated as boundaries — a file inside such
/// a directory belongs to a different repo. An invalid directory-form `.git`
/// (e.g. an empty `mkdir .git`) is not a repository and is transparent.
///
/// `.git` files that point to a *submodule* (i.e., `gitdir: .git/modules/…`)
/// are intentionally transparent: the parent repo tracks the submodule's
/// files, so they should still be considered part of the parent's workdir.
pub(super) fn has_intervening_git_dir(file_path: &Path, workdir: &Path) -> bool {
    let Ok(relative) = file_path.strip_prefix(workdir) else {
        return false;
    };

    // Walk parent directories of the relative path (excluding the file itself
    // and the empty path). For "subrepo/src/file.ts" we check:
    //   workdir/subrepo/src/.git
    //   workdir/subrepo/.git
    let mut current = relative;
    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        let potential_git = workdir.join(parent).join(".git");
        if potential_git.is_dir() {
            // Only a valid .git directory (containing HEAD) indicates a
            // separate independent repo; an invalid one is transparent.
            if is_valid_git_dir(&potential_git) {
                return true;
            }
        } else if potential_git.is_file() {
            // A .git file is either a submodule pointer or a linked-worktree
            // pointer.  Only linked worktrees (gitdir points to …/worktrees/…)
            // represent a separate working-tree boundary; submodule pointers
            // (gitdir points to …/modules/…) are transparent to the parent.
            if is_linked_worktree_git_file(&potential_git) {
                return true;
            }
        }
        current = parent;
    }
    false
}

/// Returns `true` if `git_file` is a `.git` file that points to a linked
/// worktree (i.e., the `gitdir:` target path contains `/worktrees/`).
fn is_linked_worktree_git_file(git_file: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(git_file) else {
        return false;
    };
    // Format: "gitdir: <path>\n"
    let Some(gitdir) = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
    else {
        return false;
    };
    // A linked worktree's gitdir resolves to something like
    // `/repo/.git/worktrees/<name>`.  A submodule's gitdir looks like
    // `../.git/modules/<name>`.
    gitdir.contains("/.git/worktrees/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn has_intervening_empty_git_dir_is_transparent() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path();
        fs::create_dir_all(workdir.join("sub/.git")).unwrap();

        let file_path = workdir.join("sub").join("file.txt");
        assert!(!has_intervening_git_dir(&file_path, workdir));
    }

    #[test]
    fn has_intervening_valid_git_dir_is_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path();
        fs::create_dir_all(workdir.join("sub/.git")).unwrap();
        fs::write(workdir.join("sub/.git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let file_path = workdir.join("sub").join("file.txt");
        assert!(has_intervening_git_dir(&file_path, workdir));
    }
}
