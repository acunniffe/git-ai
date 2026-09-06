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

/// Every worktree of a repository as `(git_dir, worktree)` pairs, from the
/// filesystem alone: the main worktree (a `.git` common dir's parent, or the
/// `core.worktree` a submodule / `--separate-git-dir` repository points at)
/// plus the linked worktrees registered under `<common_dir>/worktrees/`. Bare
/// repositories and linked worktrees whose directory is gone are omitted.
pub fn worktrees_for_common_dir(common_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    if let Some(main) = main_worktree_for_common_dir(common_dir) {
        out.push((common_dir.to_path_buf(), main));
    }
    let Ok(linked) = fs::read_dir(common_dir.join("worktrees")) else {
        return out;
    };
    let mut linked = linked
        .flatten()
        .filter_map(|entry| {
            let git_dir = entry.path();
            let dot_git = fs::read_to_string(git_dir.join("gitdir")).ok()?;
            // Since git 2.48 (`worktree.useRelativePaths`) the pointer may be
            // relative to the linked git dir.
            let dot_git = git_dir.join(dot_git.trim());
            let worktree = dot_git.parent()?.to_path_buf();
            worktree.is_dir().then_some((git_dir, worktree))
        })
        .collect::<Vec<_>>();
    linked.sort();
    out.extend(linked);
    out
}

/// Whether `worktree` is still the worktree whose git dir is `git_dir`: a
/// remembered pair may be stale once either path has been reused by an
/// unrelated repository.
pub fn worktree_belongs_to_git_dir(worktree: &Path, git_dir: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    git_dir_for_worktree(worktree)
        .is_some_and(|resolved| canonical(&resolved) == canonical(git_dir))
}

fn main_worktree_for_common_dir(common_dir: &Path) -> Option<PathBuf> {
    // The ordinary layout needs no config read (the daemon asks about every
    // known family on every tick).
    if common_dir.file_name().is_some_and(|name| name == ".git") {
        let main = common_dir.parent()?.to_path_buf();
        return main.is_dir().then_some(main);
    }
    let config =
        crate::git::repository::git_config_file_for_repo_paths(common_dir, common_dir).ok();
    let config_value = |key: &str| {
        config
            .as_ref()
            .and_then(|config| config.string(key))
            .map(|value| value.to_string())
    };
    if config_value("core.bare").is_some_and(|bare| bare.eq_ignore_ascii_case("true")) {
        return None;
    }
    let main = common_dir.join(config_value("core.worktree")?);
    main.is_dir().then_some(main)
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

    #[test]
    fn worktrees_for_common_dir_lists_main_and_linked_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("repo");
        let common_dir = main.join(".git");
        write_file(&common_dir.join("HEAD"), "ref: refs/heads/main\n");
        let linked = temp.path().join("feature");
        fs::create_dir_all(&linked).unwrap();
        let linked_git_dir = common_dir.join("worktrees").join("feature");
        write_file(
            &linked_git_dir.join("gitdir"),
            &format!("{}\n", linked.join(".git").display()),
        );
        // A registration whose worktree directory was deleted is skipped.
        write_file(
            &common_dir.join("worktrees").join("gone").join("gitdir"),
            &format!("{}\n", temp.path().join("gone").join(".git").display()),
        );

        assert_eq!(
            worktrees_for_common_dir(&common_dir),
            vec![(common_dir.clone(), main), (linked_git_dir, linked)]
        );
    }

    #[test]
    fn worktree_belongs_to_git_dir_rejects_reused_paths() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("repo");
        let common_dir = main.join(".git");
        write_file(&common_dir.join("HEAD"), "ref: refs/heads/main\n");
        assert!(worktree_belongs_to_git_dir(&main, &common_dir));

        // A linked worktree whose `.git` file points at this repository...
        let linked = temp.path().join("feature");
        let linked_git_dir = common_dir.join("worktrees").join("feature");
        write_file(&linked_git_dir.join("HEAD"), "ref: refs/heads/feature\n");
        write_file(
            &linked.join(".git"),
            &format!("gitdir: {}\n", linked_git_dir.display()),
        );
        assert!(worktree_belongs_to_git_dir(&linked, &linked_git_dir));

        // ...and the same path later reused by an unrelated repository.
        let other = temp.path().join("other/.git");
        write_file(&other.join("HEAD"), "ref: refs/heads/main\n");
        write_file(
            &linked.join(".git"),
            &format!("gitdir: {}\n", other.display()),
        );
        assert!(!worktree_belongs_to_git_dir(&linked, &linked_git_dir));
        assert!(!worktree_belongs_to_git_dir(
            &temp.path().join("missing"),
            &common_dir
        ));
    }

    #[test]
    fn worktrees_for_common_dir_skips_bare_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let bare = temp.path().join("repo.git");
        write_file(&bare.join("HEAD"), "ref: refs/heads/main\n");
        write_file(&bare.join("config"), "[core]\n\tbare = true\n");

        assert!(worktrees_for_common_dir(&bare).is_empty());
    }

    #[test]
    fn worktrees_for_common_dir_follows_core_worktree_and_relative_pointers() {
        // A submodule's git dir (`.git/modules/sub`) names its worktree through
        // core.worktree; a linked worktree may register a relative gitdir.
        let temp = tempfile::tempdir().unwrap();
        let common_dir = temp.path().join("super/.git/modules/sub");
        let sub = temp.path().join("super/sub");
        fs::create_dir_all(&sub).unwrap();
        write_file(&common_dir.join("HEAD"), "ref: refs/heads/main\n");
        write_file(
            &common_dir.join("config"),
            "[core]\n\tbare = false\n\tworktree = ../../../sub\n",
        );
        let linked = temp.path().join("sub-feature");
        fs::create_dir_all(&linked).unwrap();
        let linked_git_dir = common_dir.join("worktrees").join("feature");
        write_file(
            &linked_git_dir.join("gitdir"),
            "../../../../../../sub-feature/.git\n",
        );

        let worktrees = worktrees_for_common_dir(&common_dir);
        assert_eq!(worktrees.len(), 2, "{worktrees:?}");
        assert_eq!(worktrees[0].0, common_dir);
        assert_eq!(
            worktrees[0].1.canonicalize().unwrap(),
            sub.canonicalize().unwrap()
        );
        assert_eq!(worktrees[1].0, linked_git_dir);
        assert_eq!(
            worktrees[1].1.canonicalize().unwrap(),
            linked.canonicalize().unwrap()
        );
    }
}
