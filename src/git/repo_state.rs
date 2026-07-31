use std::fs;
use std::path::{Path, PathBuf};

pub fn is_valid_git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadState {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
}

/// Check the minimal required marker for a directory-form Git repository.
///
/// Using `HEAD` instead of the directory itself rejects empty `.git`
/// placeholders without adding a filesystem lookup to normal discovery.
pub fn is_valid_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

pub fn worktree_root_for_path(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        let dot_git = candidate.join(".git");
        if is_valid_git_dir(&dot_git) || dot_git.is_file() {
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

    if path.is_dir() && path.join("HEAD").is_file() {
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
    use crate::git::fast_reader::{FastRefReader, HeadKind};
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
