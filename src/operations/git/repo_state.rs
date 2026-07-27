use std::fs;
use std::path::{Path, PathBuf};

pub use super::oid::is_full_oid as is_valid_git_oid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
}

/// A directory-form `.git` entry is a repository boundary only if it contains
/// a `HEAD` file. `HEAD` is present in normal git dirs, bare repos, and
/// linked-worktree git dirs (`.git/worktrees/<name>/`). Callers must already
/// know `path` is a directory; this performs exactly one extra stat.
pub fn is_valid_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

pub fn worktree_root_for_path(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        let dot_git = candidate.join(".git");
        // A directory-form .git without HEAD (e.g. `mkdir .git`) is not a
        // repository boundary -- keep walking up.
        if let Ok(metadata) = fs::metadata(&dot_git)
            && (metadata.is_file() || (metadata.is_dir() && is_valid_git_dir(&dot_git)))
        {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

pub fn git_dir_for_worktree(worktree: &Path) -> Option<PathBuf> {
    let worktree_root = worktree_root_for_path(worktree)?;
    let dot_git = worktree_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let contents = fs::read_to_string(&dot_git).ok()?;
    let pointer = contents.strip_prefix("gitdir:")?.trim();
    let candidate = PathBuf::from(pointer);
    if candidate.is_absolute() {
        return Some(candidate);
    }
    Some(worktree_root.join(candidate))
}

pub fn common_dir_for_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let parent = git_dir.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("worktrees") {
        return parent.parent().map(PathBuf::from);
    }
    Some(git_dir.to_path_buf())
}

pub fn common_dir_for_worktree(worktree: &Path) -> Option<PathBuf> {
    let git_dir = git_dir_for_worktree(worktree)?;
    common_dir_for_git_dir(&git_dir)
}

pub fn common_dir_for_repo_path(path: &Path) -> Option<PathBuf> {
    if let Some(common_dir) = common_dir_for_worktree(path) {
        return Some(common_dir);
    }

    if path.is_dir() && is_valid_git_dir(path) {
        return common_dir_for_git_dir(path);
    }

    if path.file_name().and_then(|name| name.to_str()) == Some(".git") && path.is_file() {
        let contents = fs::read_to_string(path).ok()?;
        let pointer = contents.strip_prefix("gitdir:")?.trim();
        let candidate = PathBuf::from(pointer);
        let git_dir = if candidate.is_absolute() {
            candidate
        } else {
            path.parent()?.join(candidate)
        };
        return common_dir_for_git_dir(&git_dir);
    }

    None
}

pub fn read_head_state_for_worktree(worktree: &Path) -> Option<HeadState> {
    use crate::operations::git::fast_reader::{FastRefReader, HeadKind};
    let git_dir = git_dir_for_worktree(worktree)?;
    let common_dir = common_dir_for_git_dir(&git_dir)?;
    let reader = FastRefReader::new(&git_dir, &common_dir);
    match reader.try_read_head()? {
        HeadKind::Symbolic(refname) => {
            let branch = refname.strip_prefix("refs/heads/").map(|s| s.to_string());
            let detached = branch.is_none();
            let head = reader.try_resolve_ref(&refname);
            Some(HeadState {
                head,
                branch,
                detached,
            })
        }
        HeadKind::Detached(oid) => Some(HeadState {
            head: Some(oid),
            branch: None,
            detached: true,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn worktree_root_for_path_walks_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path();
        let nested = worktree.join("src").join("lib");
        fs::create_dir_all(&nested).unwrap();
        write_file(&worktree.join(".git/HEAD"), "ref: refs/heads/main\n");

        let resolved = worktree_root_for_path(&nested).unwrap();
        assert_eq!(resolved, worktree);
    }

    #[test]
    fn is_valid_git_dir_requires_head_file() {
        let temp = tempfile::tempdir().unwrap();
        let dot_git = temp.path().join(".git");
        fs::create_dir_all(&dot_git).unwrap();
        assert!(!is_valid_git_dir(&dot_git));

        write_file(&dot_git.join("HEAD"), "ref: refs/heads/main\n");
        assert!(is_valid_git_dir(&dot_git));
    }

    #[test]
    fn worktree_root_for_path_skips_empty_nested_git_dir() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path();
        write_file(&worktree.join(".git/HEAD"), "ref: refs/heads/main\n");
        let nested = worktree.join("sub").join("deeper");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(worktree.join("sub/.git")).unwrap();

        let resolved = worktree_root_for_path(&nested).unwrap();
        assert_eq!(resolved, worktree);
    }

    #[test]
    fn worktree_root_for_path_none_when_only_empty_git_dir() {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub");
        fs::create_dir_all(sub.join(".git")).unwrap();

        assert_eq!(worktree_root_for_path(&sub), None);
    }

    #[test]
    fn read_head_state_for_nested_path_uses_worktree_root() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path();
        let nested = worktree.join("src").join("lib");
        fs::create_dir_all(&nested).unwrap();
        write_file(&worktree.join(".git/HEAD"), "ref: refs/heads/main\n");
        write_file(
            &worktree.join(".git/refs/heads/main"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        );

        let state = read_head_state_for_worktree(&nested).unwrap();
        assert_eq!(
            state.head.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert!(!state.detached);
    }
}
