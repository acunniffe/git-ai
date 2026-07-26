use crate::config;
use crate::error::GitAiError;
use crate::git::repo_state::{
    common_dir_for_git_dir, git_dir_for_worktree, is_valid_git_dir, worktree_root_for_path,
};
use crate::git::repo_storage::RepoStorage;
use crate::git::status::MAX_PATHSPEC_ARGS;
use crate::git::sync_authorship::push_authorship_notes;
#[cfg(windows)]
use crate::utils::is_interactive_terminal;
use unicode_normalization::UnicodeNormalization;

use gix_index::entry::Stage;
use regex::Regex;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(windows)]
use crate::utils::CREATE_NO_WINDOW;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

// Keep a thread-local depth for low-overhead checks on the active thread and a process-global
// depth so internal git spawned from background threads inherits suppression state.
thread_local! {
    static INTERNAL_GIT_HOOKS_DISABLED_DEPTH: Cell<usize> = const { Cell::new(0) };
}
static INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL: AtomicUsize = AtomicUsize::new(0);

pub struct InternalGitHooksGuard;

impl Drop for InternalGitHooksGuard {
    fn drop(&mut self) {
        INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| {
            let current = depth.get();
            if current > 0 {
                depth.set(current - 1);
            }
        });
        INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Disable managed git hooks for internal `git` subprocesses executed through `exec_git*`.
/// Use this guard around higher-level operations that already execute hook logic explicitly.
pub fn disable_internal_git_hooks() -> InternalGitHooksGuard {
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| depth.set(depth.get() + 1));
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.fetch_add(1, Ordering::Relaxed);
    InternalGitHooksGuard
}

fn should_disable_internal_git_hooks() -> bool {
    INTERNAL_GIT_HOOKS_DISABLED_DEPTH.with(|depth| depth.get() > 0)
        || INTERNAL_GIT_HOOKS_DISABLED_DEPTH_GLOBAL.load(Ordering::Relaxed) > 0
}

#[cfg(windows)]
fn null_hooks_path() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_hooks_path() -> &'static str {
    "/dev/null"
}

#[doc(hidden)]
pub fn args_with_disabled_hooks_if_needed(args: &[String]) -> Vec<String> {
    if !should_disable_internal_git_hooks() {
        return args.to_vec();
    }

    // Respect explicit hook-path overrides if a caller already set one.
    let already_overrides_hooks = args
        .windows(2)
        .any(|pair| pair[0] == "-c" && pair[1].starts_with("core.hooksPath="))
        || args.iter().any(|arg| {
            arg.starts_with("-ccore.hooksPath=") || arg.starts_with("--config=core.hooksPath=")
        });

    if already_overrides_hooks {
        return args.to_vec();
    }

    let mut out = Vec::with_capacity(args.len() + 2);
    out.push("-c".to_string());
    out.push(format!("core.hooksPath={}", null_hooks_path()));
    out.extend(args.iter().cloned());
    out
}

fn first_git_subcommand_index(args: &[String]) -> Option<usize> {
    let mut index = 0usize;

    while index < args.len() {
        let arg = &args[index];

        if !arg.starts_with('-') {
            return Some(index);
        }

        let takes_value = matches!(
            arg.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
        );

        index += if takes_value { 2 } else { 1 };
    }

    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InternalGitProfile {
    General,
    PatchParse,
    NumstatParse,
    RawDiffParse,
}

fn strip_profile_conflicts(args: Vec<String>, profile: InternalGitProfile) -> Vec<String> {
    if profile == InternalGitProfile::General {
        return args;
    }

    let Some(command_index) = first_git_subcommand_index(&args) else {
        return args;
    };

    let should_drop = |arg: &str| -> bool {
        match profile {
            InternalGitProfile::General => false,
            InternalGitProfile::PatchParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
                    || arg == "--no-prefix"
                    || arg == "--src-prefix"
                    || arg == "--dst-prefix"
                    || arg.starts_with("--src-prefix=")
                    || arg.starts_with("--dst-prefix=")
                    || arg.starts_with("--diff-algorithm=")
                    || arg == "--no-indent-heuristic"
                    || arg.starts_with("--inter-hunk-context=")
            }
            InternalGitProfile::NumstatParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
                    || arg == "--find-renames"
                    || arg.starts_with("--find-renames=")
                    || arg == "--find-copies"
                    || arg.starts_with("--find-copies=")
                    || arg == "--find-copies-harder"
                    || arg == "-M"
                    || arg.starts_with("-M")
                    || arg == "-C"
                    || arg.starts_with("-C")
            }
            InternalGitProfile::RawDiffParse => {
                arg == "--ext-diff"
                    || arg == "--textconv"
                    || arg == "--relative"
                    || arg.starts_with("--relative=")
                    || arg == "--color"
                    || arg.starts_with("--color=")
            }
        }
    };

    let mut out = Vec::with_capacity(args.len());
    out.extend(args[..=command_index].iter().cloned());

    let mut index = command_index + 1;
    while index < args.len() {
        if args[index] == "--" {
            out.extend(args[index..].iter().cloned());
            return out;
        }

        let drop_current = should_drop(&args[index]);
        if !drop_current {
            out.push(args[index].clone());
            index += 1;
            continue;
        }

        // Handle split-arg forms we intentionally strip (e.g. --src-prefix X).
        if matches!(profile, InternalGitProfile::PatchParse)
            && (args[index] == "--src-prefix" || args[index] == "--dst-prefix")
        {
            index += 1;
            if index < args.len() && args[index] != "--" {
                index += 1;
            }
            continue;
        }

        index += 1;
    }

    out
}

fn profile_options(profile: InternalGitProfile) -> &'static [&'static str] {
    match profile {
        InternalGitProfile::General => &[],
        InternalGitProfile::PatchParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-relative",
            "--no-color",
            "--diff-algorithm=default",
            "--indent-heuristic",
            "--inter-hunk-context=0",
        ],
        InternalGitProfile::NumstatParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
            "--no-renames",
        ],
        InternalGitProfile::RawDiffParse => &[
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-relative",
        ],
    }
}

#[doc(hidden)]
pub fn args_with_internal_git_profile(args: &[String], profile: InternalGitProfile) -> Vec<String> {
    if profile == InternalGitProfile::General {
        return args.to_vec();
    }

    let args = strip_profile_conflicts(args.to_vec(), profile);
    let Some(command_index) = first_git_subcommand_index(&args) else {
        return args;
    };

    let options = profile_options(profile);
    if options.is_empty() {
        return args;
    }

    let mut out = Vec::with_capacity(args.len() + options.len());
    out.extend(args[..=command_index].iter().cloned());
    for option in options {
        if !args.iter().any(|arg| arg == option) {
            out.push((*option).to_string());
        }
    }
    out.extend(args[command_index + 1..].iter().cloned());
    out
}

pub struct Object<'a> {
    repo: &'a Repository,
    oid: String,
}

impl<'a> Object<'a> {
    pub fn id(&self) -> String {
        self.oid.clone()
    }

    // Recursively peel an object until a commit is found.
    pub fn peel_to_commit(&self) -> Result<Commit<'a>, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        // args.push("-q".to_string());
        args.push("--verify".to_string());
        args.push(format!("{}^{}", self.oid, "{commit}"));
        let output = exec_git(&args)?;
        Ok(Commit {
            repo: self.repo,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }
}

#[derive(Debug, Clone)]

pub struct CommitRange<'a> {
    repo: &'a Repository,
    pub start_oid: String,
    pub end_oid: String,
    pub refname: String,
}

impl<'a> CommitRange<'a> {
    /// Create a new CommitRange with automatic refname inference.
    /// If refname is None, tries to find a single ref pointing to end_oid.
    /// If exactly one ref is found, uses that. Otherwise falls back to current HEAD.
    pub fn new_infer_refname(
        repo: &'a Repository,
        start_oid: String,
        end_oid: String,
        refname: Option<String>,
    ) -> Result<Self, GitAiError> {
        // Resolve start_oid and end_oid to actual commit SHAs
        let resolved_start = repo.revparse_single(&start_oid)?.oid;
        let resolved_end = repo.revparse_single(&end_oid)?.oid;

        let inferred_refname = match refname {
            Some(name) => name,
            None => {
                // Try to find refs pointing to resolved end_oid
                let mut args = repo.global_args_for_exec();
                args.push("for-each-ref".to_string());
                args.push("--points-at".to_string());
                args.push(resolved_end.clone());
                args.push("--format=%(refname)".to_string());

                let refs = match exec_git(&args) {
                    Ok(output) => {
                        let stdout = String::from_utf8(output.stdout).unwrap_or_default();
                        let refs: Vec<String> = stdout
                            .lines()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        refs
                    }
                    Err(_) => Vec::new(),
                };

                // If exactly one ref found, use it
                if refs.len() == 1 {
                    refs[0].clone()
                } else {
                    // Fall back to current HEAD
                    match repo.head() {
                        Ok(head_ref) => head_ref.name().unwrap_or("HEAD").to_string(),
                        Err(_) => "HEAD".to_string(),
                    }
                }
            }
        };

        Ok(Self {
            repo,
            start_oid: resolved_start,
            end_oid: resolved_end,
            refname: inferred_refname,
        })
    }

    pub fn repo(&self) -> &'a Repository {
        self.repo
    }

    pub fn is_valid(&self) -> Result<(), GitAiError> {
        const EMPTY_TREE_HASH: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

        // Check that both commits exist
        // Skip validation for empty tree hash - it's a special git object that may not exist in the repo
        if self.start_oid != EMPTY_TREE_HASH {
            self.repo.find_commit(self.start_oid.clone())?;
        }
        self.repo.find_commit(self.end_oid.clone())?;

        // Check that both commits exist on the refname
        // Use git merge-base --is-ancestor <commit> <refname>
        // Skip merge-base check for empty tree hash since it's not part of commit history
        if self.start_oid != EMPTY_TREE_HASH {
            let mut args = self.repo.global_args_for_exec();
            args.push("merge-base".to_string());
            args.push("--is-ancestor".to_string());
            args.push(self.start_oid.clone());
            args.push(self.refname.clone());

            exec_git(&args).map_err(|_| {
                GitAiError::Generic(format!(
                    "Commit {} is not reachable from refname {}",
                    self.start_oid, self.refname
                ))
            })?;
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("merge-base".to_string());
        args.push("--is-ancestor".to_string());
        args.push(self.end_oid.clone());
        args.push(self.refname.clone());

        exec_git(&args).map_err(|_| {
            GitAiError::Generic(format!(
                "Commit {} is not reachable from refname {}",
                self.end_oid, self.refname
            ))
        })?;

        // Check that start is an ancestor of end (direct path between them)
        // Skip for empty tree hash - it's not part of the commit DAG
        if self.start_oid != EMPTY_TREE_HASH {
            let mut args = self.repo.global_args_for_exec();
            args.push("merge-base".to_string());
            args.push("--is-ancestor".to_string());
            args.push(self.start_oid.clone());
            args.push(self.end_oid.clone());

            exec_git(&args).map_err(|_| {
                GitAiError::Generic(format!(
                    "Commit {} is not an ancestor of {}",
                    self.start_oid, self.end_oid
                ))
            })?;
        }

        Ok(())
    }

    pub fn all_commits(&self) -> Vec<String> {
        let mut commits = Vec::new();
        let itt = self.clone().into_iter();

        for commit in itt {
            commits.push(commit.oid.clone());
        }
        commits
    }
}

impl<'a> IntoIterator for CommitRange<'a> {
    type Item = Commit<'a>;
    type IntoIter = CommitRangeIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        // Empty range - return empty iterator
        if self.start_oid.is_empty() && self.end_oid.is_empty() {
            return CommitRangeIterator {
                repo: self.repo,
                commit_oids: Vec::new(),
                index: 0,
            };
        }

        // ie for single commit branches
        if self.start_oid == self.end_oid {
            return CommitRangeIterator {
                repo: self.repo,
                commit_oids: vec![self.end_oid.clone()],
                index: 0,
            };
        }

        // Use git rev-list to get all commits between start and end
        // Format: start_oid..end_oid means commits reachable from end_oid but not from start_oid
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-list".to_string());
        args.push(format!("{}..{}", self.start_oid, self.end_oid));

        let commit_oids: Vec<String> = match exec_git(&args) {
            Ok(output) => {
                let stdout = String::from_utf8(output.stdout).unwrap_or_default();
                stdout
                    .lines()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            Err(_) => Vec::new(), // If they don't share lineage or error occurs, return empty
        };

        CommitRangeIterator {
            repo: self.repo,
            commit_oids,
            index: 0,
        }
    }
}

pub struct CommitRangeIterator<'a> {
    repo: &'a Repository,
    commit_oids: Vec<String>,
    index: usize,
}

impl<'a> Iterator for CommitRangeIterator<'a> {
    type Item = Commit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.commit_oids.len() {
            return None;
        }
        let oid = self.commit_oids[self.index].clone();
        self.index += 1;
        Some(Commit {
            repo: self.repo,
            oid,
        })
    }
}

pub struct Commit<'a> {
    repo: &'a Repository,
    oid: String,
}

impl<'a> Commit<'a> {
    pub fn id(&self) -> String {
        self.oid.clone()
    }

    pub fn tree(&self) -> Result<Tree<'a>, GitAiError> {
        let reader = crate::git::fast_reader::FastObjectReader::new(&self.repo.git_common_dir);
        if let Some(tree_oid) = reader.try_read_commit_tree_oid(&self.oid) {
            return Ok(Tree {
                repo: self.repo,
                oid: tree_oid,
            });
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--verify".to_string());
        args.push(format!("{}^{}", self.oid, "{tree}"));
        let output = exec_git(&args)?;
        Ok(Tree {
            repo: self.repo,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }

    pub fn parent(&self, i: usize) -> Result<Commit<'a>, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        // args.push("-q".to_string());
        args.push("--verify".to_string());
        // libgit2 uses 0-based indexing; Git's rev syntax uses 1-based parent selectors.
        args.push(format!("{}^{}", self.oid, i + 1));
        let output = exec_git(&args)?;
        Ok(Commit {
            repo: self.repo,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }

    // Return an iterator over the parents of this commit.
    pub fn parents(&self) -> Parents<'a> {
        // Use `git show -s --format=%P <oid>` to get whitespace-separated parent OIDs
        let mut args = self.repo.global_args_for_exec();
        args.push("show".to_string());
        args.push("-s".to_string());
        args.push("--format=%P".to_string());
        args.push(self.oid.clone());

        let parent_oids: Vec<String> = match exec_git(&args) {
            Ok(output) => {
                let stdout = String::from_utf8(output.stdout).unwrap_or_default();
                stdout.split_whitespace().map(|s| s.to_string()).collect()
            }
            Err(_) => Vec::new(),
        };

        Parents {
            repo: self.repo,
            parent_oids,
            index: 0,
        }
    }

    // Get the number of parents of this commit.
    // Use the parents iterator to return an iterator over all parents.
    #[allow(dead_code)]
    pub fn parent_count(&self) -> Result<usize, GitAiError> {
        Ok(self.parents().count())
    }

    // Get the short "summary" of the git commit message. The returned message is the summary of the commit, comprising the first paragraph of the message with whitespace trimmed and squashed. None may be returned if an error occurs or if the summary is not valid utf-8.
    pub fn summary(&self) -> Result<String, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("show".to_string());
        args.push("-s".to_string());
        args.push("--no-notes".to_string());
        args.push("--encoding=UTF-8".to_string());
        args.push("--format=%s".to_string());
        args.push(self.oid.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Get the body of the git commit message (everything after the first paragraph).
    // Returns an empty string if there is no body.
    pub fn body(&self) -> Result<String, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("show".to_string());
        args.push("-s".to_string());
        args.push("--no-notes".to_string());
        args.push("--encoding=UTF-8".to_string());
        args.push("--format=%b".to_string());
        args.push(self.oid.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    /// Find the first parent that exists on the specified refname
    ///
    /// This is useful for merge commits where we want to find the parent on a specific branch
    /// (e.g., main) rather than just taking the first parent, which might not be correct in
    /// complex merge histories with back-and-forth merges.
    ///
    /// # Arguments
    /// * `refname` - The reference name to search for (e.g., "main", "refs/heads/main")
    ///
    /// # Returns
    /// The first parent commit that is reachable from the specified refname
    pub fn parent_on_refname(&self, refname: &str) -> Result<Commit<'a>, GitAiError> {
        // Normalize the refname to fully qualified form
        let fq_refname = {
            let mut rp_args = self.repo.global_args_for_exec();
            rp_args.push("rev-parse".to_string());
            rp_args.push("--verify".to_string());
            rp_args.push("--symbolic-full-name".to_string());
            rp_args.push(refname.to_string());

            match exec_git(&rp_args) {
                Ok(output) => {
                    let s = String::from_utf8(output.stdout).unwrap_or_default();
                    let s = s.trim();
                    if s.is_empty() {
                        if refname.starts_with("refs/") {
                            refname.to_string()
                        } else {
                            format!("refs/heads/{}", refname)
                        }
                    } else {
                        s.to_string()
                    }
                }
                Err(_) => {
                    if refname.starts_with("refs/") {
                        refname.to_string()
                    } else {
                        format!("refs/heads/{}", refname)
                    }
                }
            }
        };

        // Iterate through parents and find the first one that's on the refname
        for parent in self.parents() {
            let parent_sha = parent.id();

            // Check if this parent is an ancestor of the refname
            // git merge-base --is-ancestor <parent> <refname>
            let mut args = self.repo.global_args_for_exec();
            args.push("merge-base".to_string());
            args.push("--is-ancestor".to_string());
            args.push(parent_sha.clone());
            args.push(fq_refname.clone());

            if exec_git(&args).is_ok() {
                return Ok(parent);
            }
        }

        // If no parent is on the refname, return an error
        Err(GitAiError::Generic(format!(
            "No parent of commit {} is reachable from refname {}",
            self.oid, refname
        )))
    }
}

pub struct TreeEntry<'a> {
    _repo: std::marker::PhantomData<&'a Repository>,
    // Object id (SHA-1/oid) that this tree entry points to
    oid: String,
}

impl<'a> TreeEntry<'a> {
    // Get the id of the object pointed by the entry
    pub fn id(&self) -> String {
        self.oid.clone()
    }
}

pub struct Tree<'a> {
    repo: &'a Repository,
    oid: String,
}

impl<'a> Tree<'a> {
    // Get the id of the tree
    pub fn id(&self) -> String {
        self.oid.clone()
    }

    #[allow(dead_code)]
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> Tree<'a> {
        Tree {
            repo: self.repo,
            oid: self.oid.clone(),
        }
    }

    // Retrieve a tree entry contained in a tree or in any of its subtrees, given its relative path.
    pub fn get_path(&self, path: &Path) -> Result<TreeEntry<'a>, GitAiError> {
        let reader = crate::git::fast_reader::FastObjectReader::new(&self.repo.git_common_dir);
        if let Some(blob_oid) = reader.try_tree_entry_for_path(&self.oid, path) {
            return Ok(TreeEntry {
                _repo: std::marker::PhantomData,
                oid: blob_oid,
            });
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("ls-tree".to_string());
        args.push("-z".to_string());
        args.push("-r".to_string());
        args.push(self.oid.clone());
        args.push("--".to_string());
        let path_str = path.to_string_lossy().to_string();
        args.push(path_str.clone());

        let output = exec_git(&args)?;
        let bytes = output.stdout;

        // Each record: "<mode> <type> <object>\t<file>\0"
        // We expect at most one record for an exact path query.
        let mut found_entry: Option<TreeEntry<'a>> = None;

        for chunk in bytes.split(|b| *b == 0u8) {
            if chunk.is_empty() {
                continue;
            }
            // Split metadata and path on first tab
            let mut parts = chunk.splitn(2, |b| *b == b'\t');
            let meta = parts.next().unwrap_or(&[]);
            let file_bytes = parts.next().unwrap_or(&[]);

            // Parse meta: "<mode> <type> <object>"
            let meta_str = String::from_utf8_lossy(meta);
            let mut meta_iter = meta_str.split_whitespace();
            let mode = meta_iter.next().unwrap_or("").to_string();
            let object_type = meta_iter.next().unwrap_or("").to_string();
            let oid = meta_iter.next().unwrap_or("").to_string();

            if mode.is_empty() || object_type.is_empty() || oid.is_empty() {
                continue;
            }

            let file_path = String::from_utf8_lossy(file_bytes).to_string();

            // Prefer exact path match if multiple records somehow appear
            if found_entry.is_none() || file_path == path_str {
                found_entry = Some(TreeEntry {
                    _repo: std::marker::PhantomData,
                    oid,
                });
            }
        }

        match found_entry {
            Some(entry) => Ok(entry),
            None => Err(GitAiError::Generic(format!(
                "Path not found in tree: {}",
                path.to_string_lossy()
            ))),
        }
    }
}

pub struct Blob<'a> {
    repo: &'a Repository,
    oid: String,
}

impl<'a> Blob<'a> {
    #[allow(dead_code)]
    pub fn id(&self) -> String {
        self.oid.clone()
    }

    pub fn content(&self) -> Result<Vec<u8>, GitAiError> {
        let reader = crate::git::fast_reader::FastObjectReader::new(&self.repo.git_common_dir);
        if let Some(data) = reader.try_read_blob(&self.oid) {
            return Ok(data);
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("cat-file".to_string());
        args.push("blob".to_string());
        args.push(self.oid.clone());
        let output = exec_git(&args)?;
        Ok(output.stdout)
    }
}

pub struct Reference<'a> {
    repo: &'a Repository,
    ref_name: String,
}

impl<'a> Reference<'a> {
    pub fn name(&self) -> Option<&str> {
        Some(&self.ref_name)
    }

    pub fn shorthand(&self) -> Result<String, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--abbrev-ref".to_string());
        args.push(self.ref_name.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    pub fn target(&self) -> Result<String, GitAiError> {
        use crate::git::fast_reader::{FastRefReader, HeadKind};
        let reader = FastRefReader::new(&self.repo.git_dir, &self.repo.git_common_dir);
        if self.ref_name == "HEAD" {
            match reader.try_read_head() {
                Some(HeadKind::Detached(oid)) => return Ok(oid),
                Some(HeadKind::Symbolic(refname)) => {
                    if let Some(sha) = reader.try_resolve_ref(&refname) {
                        return Ok(sha);
                    }
                }
                None => {}
            }
        } else if let Some(sha) = reader.try_resolve_ref(&self.ref_name) {
            return Ok(sha);
        }

        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push(self.ref_name.clone());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Peel a reference to a commit This method recursively peels the reference until it reaches a commit.
    #[allow(dead_code)]
    pub fn peel_to_commit(&self) -> Result<Commit<'a>, GitAiError> {
        let mut args = self.repo.global_args_for_exec();
        args.push("rev-parse".to_string());
        // args.push("-q".to_string());
        args.push("--verify".to_string());
        args.push(format!("{}^{}", self.ref_name, "{commit}"));
        let output = exec_git(&args)?;
        Ok(Commit {
            repo: self.repo,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }
}

pub struct Parents<'a> {
    repo: &'a Repository,
    parent_oids: Vec<String>,
    index: usize,
}

impl<'a> Iterator for Parents<'a> {
    type Item = Commit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.parent_oids.len() {
            return None;
        }
        let oid = self.parent_oids[self.index].clone();
        self.index += 1;
        Some(Commit {
            repo: self.repo,
            oid,
        })
    }
}

pub struct References<'a> {
    repo: &'a Repository,
    refs: Vec<String>,
    index: usize,
}

impl<'a> Iterator for References<'a> {
    type Item = Result<Reference<'a>, GitAiError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.refs.len() {
            return None;
        }
        let ref_name = self.refs[self.index].clone();
        self.index += 1;
        Some(Ok(Reference {
            repo: self.repo,
            ref_name,
        }))
    }
}

/// A Git identity (name + email) for the current repository.
///
/// Resolved via `git var GIT_COMMITTER_IDENT` which respects the full git precedence
/// chain (env vars > config > system defaults), unlike a raw `git config user.name`
/// lookup which can miss identities configured via environment variables or system-level
/// defaults.
#[derive(Debug, Clone, Default)]
pub struct GitAuthorIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl GitAuthorIdentity {
    /// Apply git-ai's optional author config as a partial override.
    pub fn with_author_config(&self, author: &config::AuthorConfig) -> Self {
        GitAuthorIdentity {
            name: author.name.clone().or_else(|| self.name.clone()),
            email: author.email.clone().or_else(|| self.email.clone()),
        }
    }

    /// Format as `"Name <email>"`, `"Name"`, `"<email>"`, or `None`.
    pub fn formatted(&self) -> Option<String> {
        match (&self.name, &self.email) {
            (Some(n), Some(e)) => Some(format!("{} <{}>", n, e)),
            (Some(n), None) => Some(n.clone()),
            (None, Some(e)) => Some(format!("<{}>", e)),
            (None, None) => None,
        }
    }

    /// Return the full identity (`"Name <email>"`) or fall back to name-only / `"unknown"`.
    pub fn formatted_or_unknown(&self) -> String {
        self.formatted().unwrap_or_else(|| "unknown".to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct GitIdentityResolution {
    pub raw_git_var: Option<String>,
    pub identity: GitAuthorIdentity,
}

#[derive(Debug, Clone, Default)]
pub struct GitConfigIdentityResolution {
    pub raw_name: Option<String>,
    pub raw_email: Option<String>,
    pub identity: GitAuthorIdentity,
}

/// Parse `git var GIT_COMMITTER_IDENT` output into name and email.
///
/// The output format is: `Name <email> unix-timestamp timezone`
/// For example: `John Doe <john@example.com> 1234567890 +0000`
pub fn parse_git_var_identity(output: &str) -> GitAuthorIdentity {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return GitAuthorIdentity::default();
    }

    // Find email in angle brackets
    let email_start = trimmed.find('<');
    let email_end = trimmed.find('>');

    match (email_start, email_end) {
        (Some(start), Some(end)) if end > start => {
            let name = trimmed[..start].trim();
            let email = trimmed[start + 1..end].trim();
            GitAuthorIdentity {
                name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                email: if email.is_empty() {
                    None
                } else {
                    Some(email.to_string())
                },
            }
        }
        _ => {
            // No angle brackets - just treat the whole string as a name
            GitAuthorIdentity {
                name: Some(trimmed.to_string()),
                email: None,
            }
        }
    }
}

pub fn global_git_config_committer_identity() -> Result<GitAuthorIdentity, GitAiError> {
    Ok(global_git_config_identity_resolution()?.identity)
}

pub fn global_git_config_identity_resolution() -> Result<GitConfigIdentityResolution, GitAiError> {
    let config =
        gix_config::File::from_globals().map_err(|e| GitAiError::GixError(e.to_string()))?;
    Ok(git_config_identity_resolution_from_config(&config))
}

pub fn current_git_committer_identity_resolution() -> GitIdentityResolution {
    resolve_git_var_identity_with_args(Vec::new(), "GIT_COMMITTER_IDENT", || {
        global_git_config_committer_identity().unwrap_or_default()
    })
}

fn git_config_identity_resolution_from_config(
    config: &gix_config::File<'_>,
) -> GitConfigIdentityResolution {
    let raw_name = config.string("user.name").map(|cow| cow.to_string());
    let raw_email = config.string("user.email").map(|cow| cow.to_string());
    let name = raw_name
        .as_deref()
        .map(ToOwned::to_owned)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());
    let email = raw_email
        .as_deref()
        .map(ToOwned::to_owned)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string());

    GitConfigIdentityResolution {
        raw_name,
        raw_email,
        identity: GitAuthorIdentity { name, email },
    }
}

fn resolve_git_var_identity_with_args<F>(
    mut args: Vec<String>,
    git_var: &str,
    fallback_identity: F,
) -> GitIdentityResolution
where
    F: FnOnce() -> GitAuthorIdentity,
{
    args.push("var".to_string());
    args.push(git_var.to_string());

    if let Ok(output) = exec_git(&args)
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let identity = parse_git_var_identity(&stdout);
        if identity.name.is_some() || identity.email.is_some() {
            return GitIdentityResolution {
                raw_git_var: Some(stdout.trim().to_string()),
                identity,
            };
        }
    }

    GitIdentityResolution {
        raw_git_var: None,
        identity: fallback_identity(),
    }
}

#[derive(Debug, Clone)]
pub struct Repository {
    global_args: Vec<String>,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
    pub storage: RepoStorage,
    pub pre_command_base_commit: Option<String>,
    pub pre_command_refname: Option<String>,
    pub pre_reset_target_commit: Option<String>,
    pub pre_update_ref_refname: Option<String>,
    pub pre_update_ref_old_target: Option<String>,
    pub pre_update_ref_affects_checked_out_branch: Option<bool>,
    workdir: PathBuf,
    /// Canonical (absolute, resolved) version of workdir for reliable path comparisons
    /// On Windows, this uses the \\?\ UNC prefix format
    canonical_workdir: PathBuf,
    /// Cached git author identity resolved via `git var GIT_COMMITTER_IDENT`.
    cached_author_identity: std::sync::OnceLock<GitAuthorIdentity>,
}

impl Repository {
    // Util for preparing global args for execution
    pub fn global_args_for_exec(&self) -> Vec<String> {
        let mut args = self.global_args.clone();
        if !args.iter().any(|arg| arg == "--no-pager") {
            args.push("--no-pager".to_string());
        }
        args
    }

    pub fn require_pre_command_head(&mut self) {
        if self.pre_command_base_commit.is_some() || self.pre_command_refname.is_some() {
            return;
        }

        // Safely handle empty repositories
        if let Ok(head_ref) = self.head()
            && let Ok(target) = head_ref.target()
        {
            let target_string = target;
            let refname = head_ref.name().map(|n| n.to_string());
            self.pre_command_base_commit = Some(target_string);
            self.pre_command_refname = refname;
        }
    }

    // Internal util to get the git object type for a given OID
    fn object_type(&self, oid: &str) -> Result<String, GitAiError> {
        let reader = crate::git::fast_reader::FastObjectReader::new(&self.git_common_dir);
        if let Some(typ) = reader.try_read_object_type(oid) {
            return Ok(typ);
        }

        let mut args = self.global_args_for_exec();
        args.push("cat-file".to_string());
        args.push("-t".to_string());
        args.push(oid.to_string());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Retrieve and resolve the reference pointed at by HEAD.
    // If HEAD is a symbolic ref, return the refname (e.g., "refs/heads/main").
    // Otherwise, return "HEAD".
    pub fn head<'a>(&'a self) -> Result<Reference<'a>, GitAiError> {
        use crate::git::fast_reader::{FastRefReader, HeadKind};
        let reader = FastRefReader::new(&self.git_dir, &self.git_common_dir);
        match reader.try_read_head() {
            Some(HeadKind::Symbolic(refname)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: refname,
                });
            }
            Some(HeadKind::Detached(_)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: "HEAD".to_string(),
                });
            }
            None => {}
        }

        let mut args = self.global_args_for_exec();
        args.push("symbolic-ref".to_string());
        args.push("HEAD".to_string());

        let output = exec_git(&args);

        match output {
            Ok(output) if output.status.success() => {
                let refname = String::from_utf8(output.stdout)?;
                Ok(Reference {
                    repo: self,
                    ref_name: refname.trim().to_string(),
                })
            }
            _ => Ok(Reference {
                repo: self,
                ref_name: "HEAD".to_string(),
            }),
        }
    }

    // Returns the path to the .git folder for normal repositories or the repository itself for bare repositories.
    // TODO Test on bare repositories.
    pub fn path(&self) -> &Path {
        self.git_dir.as_path()
    }

    /// Returns the common git directory shared by linked worktrees.
    /// For non-worktree repositories, this is the same as `path()`.
    pub fn common_dir(&self) -> &Path {
        self.git_common_dir.as_path()
    }

    // Get the path of the working directory for this repository.
    // If this repository is bare, then None is returned.
    pub fn workdir(&self) -> Result<PathBuf, GitAiError> {
        // TODO Remove Result since this is determined at initialization now
        Ok(self.workdir.clone())
    }

    /// Returns true when this repository is bare.
    pub fn is_bare_repository(&self) -> Result<bool, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--is-bare-repository".to_string());
        let output = exec_git(&args)?;
        let value = String::from_utf8(output.stdout)?;
        Ok(value.trim() == "true")
    }

    /// Get the canonical (absolute, resolved) path of the working directory
    /// Check if a path is within the repository's working directory.
    ///
    /// Returns `false` for paths inside nested independent git repos (subdirectories
    /// with their own `.git/` directory), since those files belong to the nested repo,
    /// not this one. Submodules (`.git` file, not directory) are transparent and still
    /// considered part of this repo.
    pub fn path_is_in_workdir(&self, path: &Path) -> bool {
        // Try canonical comparison first (most reliable, especially on Windows)
        if let Ok(canonical_path) = path.canonicalize() {
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !has_intervening_git_dir(&canonical_path, &self.canonical_workdir);
        }

        // Fallback for paths that don't exist yet: try to canonicalize the parent directory
        // and append the filename. This handles cases where the path contains symlinks
        // (e.g., /var -> /private/var on macOS).
        if let Some(parent) = path.parent()
            && let Some(filename) = path.file_name()
            && let Ok(canonical_parent) = parent.canonicalize()
        {
            let canonical_path = canonical_parent.join(filename);
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !has_intervening_git_dir(&canonical_path, &self.canonical_workdir);
        }

        // Final fallback: normalize by resolving .. and . and check against both
        // canonical and non-canonical workdir (in case of symlinks)
        let normalized = path
            .components()
            .fold(std::path::PathBuf::new(), |mut acc, component| {
                match component {
                    std::path::Component::ParentDir => {
                        acc.pop();
                    }
                    std::path::Component::CurDir => {}
                    _ => acc.push(component),
                }
                acc
            });

        // Try both canonical and non-canonical workdir to handle symlinks
        let in_canonical = normalized.starts_with(&self.canonical_workdir);
        let in_workdir = normalized.starts_with(&self.workdir);

        if !in_canonical && !in_workdir {
            return false;
        }

        // Use canonical_workdir if path matches it, otherwise use workdir
        let base = if in_canonical {
            &self.canonical_workdir
        } else {
            &self.workdir
        };

        !has_intervening_git_dir(&normalized, base)
    }

    pub fn remotes(&self) -> Result<Vec<String>, GitAiError> {
        Ok(self
            .remotes_with_urls()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    // List all remotes with their URLs as tuples (name, url)
    pub fn remotes_with_urls(&self) -> Result<Vec<(String, String)>, GitAiError> {
        let config = self.get_git_config_file()?;
        let mut remotes = Vec::new();

        for section in config.sections() {
            if !section.header().name().eq_ignore_ascii_case(b"remote") {
                continue;
            }
            let Some(name) = section.header().subsection_name() else {
                continue;
            };
            let Some(url) = section.body().value("url") else {
                continue;
            };
            remotes.push((name.to_string(), url.to_string()));
        }

        Ok(remotes)
    }

    fn load_optional_config_file(
        path: &Path,
        source: gix_config::Source,
    ) -> Result<Option<gix_config::File<'static>>, GitAiError> {
        if !path.exists() {
            return Ok(None);
        }
        gix_config::File::from_path_no_includes(path.to_path_buf(), source)
            .map(Some)
            .map_err(|e| GitAiError::GixError(e.to_string()))
    }

    pub(crate) fn get_git_config_file(&self) -> Result<gix_config::File<'static>, GitAiError> {
        git_config_file_for_repo_paths(self.path(), self.common_dir())
    }

    /// Get config value for a given key as a String.
    pub fn config_get_str(&self, key: &str) -> Result<Option<String>, GitAiError> {
        self.get_git_config_file()
            .map(|cfg| cfg.string(key).map(|cow| cow.to_string()))
    }

    /// Get the effective raw Git user identity for this repository.
    ///
    /// Uses `git var GIT_COMMITTER_IDENT` which respects the full git identity precedence:
    /// `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    /// The result is cached per Repository instance for performance.
    ///
    /// For git-ai authorship metadata, use [`Self::effective_author_identity`] so the
    /// git-ai author config can override this raw Git identity.
    pub fn git_author_identity(&self) -> &GitAuthorIdentity {
        self.cached_author_identity
            .get_or_init(|| self.resolve_git_var_identity("GIT_COMMITTER_IDENT"))
    }

    pub fn git_author_identity_resolution(&self) -> GitIdentityResolution {
        self.resolve_git_var_identity_resolution("GIT_COMMITTER_IDENT")
    }

    /// Get the git-ai effective author identity for metadata and display.
    ///
    /// This starts from Git's effective committer identity, then overlays any
    /// configured `author.name` and/or `author.email` from git-ai config.
    pub fn effective_author_identity(&self) -> GitAuthorIdentity {
        let git_id = self.git_author_identity();
        git_id.with_author_config(&config::Config::fresh_author_cached())
    }

    /// Get the effective git commit author identity for this repository.
    ///
    /// Uses `git var GIT_AUTHOR_IDENT` which respects:
    /// `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    ///
    /// This is the correct method to use when resolving the commit **author** identity
    /// (as opposed to committer), e.g. in commit hooks.
    pub fn git_commit_author_identity(&self) -> GitAuthorIdentity {
        self.resolve_git_var_identity("GIT_AUTHOR_IDENT")
    }

    /// Internal: resolve git identity via the specified `git var` variable.
    fn resolve_git_var_identity(&self, git_var: &str) -> GitAuthorIdentity {
        self.resolve_git_var_identity_resolution(git_var).identity
    }

    fn resolve_git_var_identity_resolution(&self, git_var: &str) -> GitIdentityResolution {
        resolve_git_var_identity_with_args(self.global_args_for_exec(), git_var, || {
            self.get_git_config_file()
                .ok()
                .map(|config| git_config_identity_resolution_from_config(&config).identity)
                .unwrap_or_default()
        })
    }

    /// Get all config values matching a regex pattern.
    ///
    /// Regular expression matching is currently case-sensitive
    /// and done against a canonicalized version of the key
    /// in which section and variable names are lowercased, but subsection names are not.
    ///
    /// Returns a HashMap of key -> value for all matching config entries.
    pub fn config_get_regexp(
        &self,
        pattern: &str,
    ) -> Result<std::collections::HashMap<String, String>, GitAiError> {
        let re = Regex::new(pattern)
            .map_err(|e| GitAiError::Generic(format!("Invalid regex pattern: {}", e)))?;

        let config = self.get_git_config_file()?;
        let mut matches: HashMap<String, String> = HashMap::new();

        for section in config.sections() {
            let section_name = section.header().name().to_string().to_lowercase();
            let subsection = section.header().subsection_name();

            for value_name in section.body().value_names() {
                let value_name_str = value_name.to_string().to_lowercase();
                let full_key = if let Some(sub) = subsection {
                    format!("{}.{}.{}", section_name, sub, value_name_str)
                } else {
                    format!("{}.{}", section_name, value_name_str)
                };

                if re.is_match(&full_key)
                    && let Some(value) = section.body().value(value_name).map(|c| c.to_string())
                {
                    matches.insert(full_key, value);
                }
            }
        }

        Ok(matches)
    }

    /// Get the git version as a tuple (major, minor, patch).
    /// Returns None if the version cannot be parsed.
    pub fn git_version(&self) -> Option<(u32, u32, u32)> {
        let args = vec!["--version".to_string()];
        let output = exec_git(&args).ok()?;
        let version_str = String::from_utf8(output.stdout).ok()?;
        parse_git_version(&version_str)
    }

    /// Check if the current git version supports --ignore-revs-file flag for blame.
    /// This flag was added in git 2.23.0.
    pub fn git_supports_ignore_revs_file(&self) -> bool {
        if let Some((major, minor, _)) = self.git_version() {
            // --ignore-revs-file was added in git 2.23.0
            major > 2 || (major == 2 && minor >= 23)
        } else {
            // If we can't determine the version, assume it's supported
            // to avoid breaking existing functionality
            true
        }
    }

    // Write an in-memory buffer to the ODB as a blob.
    #[allow(dead_code)]
    pub fn remote_head(&self, remote_name: &str) -> Result<String, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("symbolic-ref".to_string());
        args.push(format!("refs/remotes/{}/HEAD", remote_name));
        args.push("--short".to_string());

        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Find a merge base between two commits
    pub fn merge_base(&self, one: String, two: String) -> Result<String, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("merge-base".to_string());
        args.push(one.to_string());
        args.push(two.to_string());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Find a single object, as specified by a revision string.
    pub fn revparse_single(&self, spec: &str) -> Result<Object<'_>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("rev-parse".to_string());
        // args.push("-q".to_string());
        args.push("--verify".to_string());
        args.push(spec.to_string());
        let output = exec_git(&args)?;
        Ok(Object {
            repo: self,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }

    // Non-standard method of getting a 'default' remote
    pub fn get_default_remote(&self) -> Result<Option<String>, GitAiError> {
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Ok(None);
        }
        // Prefer 'origin' if it exists
        for i in 0..remotes.len() {
            if let Some(name) = remotes.get(i)
                && name == "origin"
            {
                return Ok(Some("origin".to_string()));
            }
        }
        // Otherwise, just use the first remote
        Ok(remotes.first().map(|s| s.to_string()))
    }

    #[allow(dead_code)]
    pub fn push_authorship(&self, remote_name: &str) -> Result<(), GitAiError> {
        push_authorship_notes(self, remote_name)
    }

    pub fn upstream_remote(&self) -> Result<Option<String>, GitAiError> {
        // Get current branch name using exec_git
        let mut args = self.global_args_for_exec();
        args.push("branch".to_string());
        args.push("--show-current".to_string());
        let output = exec_git(&args)?;
        let branch = String::from_utf8(output.stdout)?.trim().to_string();
        if branch.is_empty() {
            return Ok(None);
        }
        let config_key = format!("branch.{}.remote", branch);
        self.config_get_str(&config_key)
    }

    pub fn resolve_author_spec(&self, author_spec: &str) -> Result<Option<String>, GitAiError> {
        // Use git rev-list to find the first commit by this author pattern
        let mut args = self.global_args_for_exec();
        args.push("rev-list".to_string());
        args.push("--all".to_string());
        args.push("-i".to_string());
        args.push("--max-count=1".to_string());
        args.push(format!("--author={}", author_spec));
        let output = match exec_git(&args) {
            Ok(output) => output,
            Err(GitAiError::GitCliError { code: Some(1), .. }) => {
                // No commit found
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let commit_oid = String::from_utf8(output.stdout)?.trim().to_string();
        if commit_oid.is_empty() {
            return Ok(None);
        }

        // Now get the author name/email from that commit
        let mut show_args = self.global_args_for_exec();
        show_args.push("show".to_string());
        show_args.push("-s".to_string());
        show_args.push("--format=%an <%ae>".to_string());
        show_args.push(commit_oid);
        let show_output = exec_git(&show_args)?;
        let author_line = String::from_utf8(show_output.stdout)?.trim().to_string();
        if author_line.is_empty() {
            Ok(None)
        } else {
            Ok(Some(author_line))
        }
    }

    // Lookup a reference to one of the commits in a repository.
    pub fn find_commit(&self, oid: String) -> Result<Commit<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "commit" {
            return Err(GitAiError::Generic(format!(
                "Object is not a commit: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Commit { repo: self, oid })
    }

    // Lookup a reference to one of the objects in a repository.
    pub fn find_blob(&self, oid: String) -> Result<Blob<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "blob" {
            return Err(GitAiError::Generic(format!(
                "Object is not a blob: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Blob { repo: self, oid })
    }

    // Lookup a reference to one of the objects in a repository.
    pub fn find_tree(&self, oid: String) -> Result<Tree<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "tree" {
            return Err(GitAiError::Generic(format!(
                "Object is not a tree: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Tree { repo: self, oid })
    }

    /// Read file content from a tree, using fast filesystem reads with git CLI fallback.
    pub fn read_file_blob_at_tree(
        &self,
        tree_oid: &str,
        path: &Path,
    ) -> Result<Vec<u8>, GitAiError> {
        let reader = crate::git::fast_reader::FastObjectReader::new(&self.git_common_dir);
        if let Some(blob_oid) = reader.try_tree_entry_for_path(tree_oid, path) {
            if let Some(content) = reader.try_read_blob(&blob_oid) {
                return Ok(content);
            }
            let blob = Blob {
                repo: self,
                oid: blob_oid,
            };
            return blob.content();
        }
        let tree = Tree {
            repo: self,
            oid: tree_oid.to_string(),
        };
        let entry = tree.get_path(path)?;
        let blob = Blob {
            repo: self,
            oid: entry.id(),
        };
        blob.content()
    }

    /// Get the content of a file at a specific commit
    /// Uses `git show <commit>:<path>` for efficient single-call retrieval
    #[allow(dead_code)]
    pub fn get_file_content(
        &self,
        file_path: &str,
        commit_hash: &str,
    ) -> Result<Vec<u8>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("show".to_string());
        args.push(format!("{}:{}", commit_hash, file_path));
        let output = exec_git(&args)?;
        Ok(output.stdout)
    }

    /// Get content of all staged files concurrently
    /// Returns a HashMap of file paths to their staged content as strings
    /// Skips files that fail to read or aren't valid UTF-8
    pub fn get_all_staged_files_content(
        &self,
        file_paths: &[String],
    ) -> Result<HashMap<String, String>, GitAiError> {
        use futures::future::join_all;
        use std::sync::Arc;

        const MAX_CONCURRENT: usize = 30;

        let repo_global_args = self.global_args_for_exec();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));

        let futures: Vec<_> = file_paths
            .iter()
            .map(|file_path| {
                let mut args = repo_global_args.clone();
                args.push("show".to_string());
                args.push(format!(":{}", file_path));
                let file_path = file_path.clone();
                let semaphore = semaphore.clone();

                async move {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .expect("staged file semaphore was closed");
                    let result = crate::tokio_runtime::spawn_blocking_result(move || {
                        exec_git(&args).and_then(|output| {
                            String::from_utf8(output.stdout)
                                .map_err(|e| GitAiError::Utf8Error(e.utf8_error()))
                        })
                    })
                    .await;
                    (file_path, result)
                }
            })
            .collect();

        let results = crate::tokio_runtime::block_on(async { join_all(futures).await });

        let mut staged_files = HashMap::new();
        for (file_path, result) in results {
            if let Ok(content) = result {
                staged_files.insert(file_path, content);
            }
        }

        Ok(staged_files)
    }

    /// Get blob OIDs for all stage-0 entries currently present in the index.
    pub fn get_all_staged_file_blob_oids(&self) -> Result<HashMap<String, String>, GitAiError> {
        let mut staged_blobs = HashMap::new();
        let object_hash = repository_object_hash_kind_for_path_no_git_exec(self.path())?;
        let index_path = self.path().join("index");
        let index = gix_index::File::at(index_path, object_hash, true, Default::default())
            .map_err(|err| GitAiError::GixError(err.to_string()))?;

        for entry in index.entries() {
            if entry.stage() != Stage::Unconflicted {
                continue;
            }
            let file_path = entry.path(&index).to_string();
            if !file_path.trim().is_empty() {
                staged_blobs.insert(file_path, entry.id.to_string());
            }
        }

        Ok(staged_blobs)
    }

    /// List all files changed in a commit
    /// Returns a HashSet of file paths relative to the repository root
    pub fn list_commit_files(
        &self,
        commit_sha: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<HashSet<String>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff-tree".to_string());
        args.push("--no-commit-id".to_string());
        args.push("--name-only".to_string());
        args.push("-r".to_string());
        args.push("-z".to_string()); // NUL-separated output for proper UTF-8 handling

        // Find the commit to check if it has a parent
        let commit = self.find_commit(commit_sha.to_string())?;

        // For initial commits (no parent), compare against the empty tree
        if commit.parent_count()? == 0 {
            let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
            args.push(empty_tree.to_string());
        }

        args.push(commit_sha.to_string());

        // Add pathspecs if provided (only as CLI args when under threshold)
        let needs_post_filter = if let Some(paths) = pathspecs {
            // for case where pathspec filter provided BUT not pathspecs.
            // otherwise it would default to full repo
            if paths.is_empty() {
                return Ok(HashSet::new());
            }
            if paths.len() > MAX_PATHSPEC_ARGS {
                true
            } else {
                args.push("--".to_string());
                for path in paths {
                    args.push(path.clone());
                }
                false
            }
        } else {
            false
        };

        let output = exec_git(&args)?;

        // With -z, output is NUL-separated. The output may contain a trailing NUL.
        let mut files: HashSet<String> = output
            .stdout
            .split(|&b| b == 0)
            .filter(|bytes| !bytes.is_empty())
            .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .collect();

        if needs_post_filter && let Some(paths) = pathspecs {
            files.retain(|path| paths.contains(path));
        }

        Ok(files)
    }

    /// Get added line ranges from git diff between two commits
    /// Returns a HashMap of file paths to vectors of added line numbers
    ///
    /// Uses `git diff -U0` to get unified diff with zero context lines,
    /// then parses the hunk headers to extract line numbers directly.
    /// This is much faster than fetching blobs and running TextDiff manually.
    pub fn diff_added_lines(
        &self,
        from_ref: &str,
        to_ref: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<HashMap<String, Vec<u32>>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string()); // Zero context lines
        args.push("--no-color".to_string());
        // Use permissive rename detection to properly handle renames
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        // Add pathspecs if provided (only as CLI args when under threshold).
        // Force post-filtering when any pathspec contains non-ASCII characters,
        // because NFC-normalised pathspecs may not match NFD entries in git's
        // index on macOS when core.precomposeunicode is false.
        let needs_post_filter = if let Some(paths) = pathspecs {
            if paths.is_empty() {
                return Ok(HashMap::new());
            }
            if paths.len() > MAX_PATHSPEC_ARGS || has_non_ascii_pathspec(paths) {
                true
            } else {
                args.push("--".to_string());
                for path in paths {
                    args.push(path.clone());
                }
                false
            }
        } else {
            false
        };

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        let (mut result, _deleted_count) = parse_diff_added_lines(&diff_output)?;

        if needs_post_filter && let Some(paths) = pathspecs {
            let nfc_paths: HashSet<String> = paths.iter().map(|s| s.nfc().collect()).collect();
            result.retain(|path, _| nfc_paths.contains(path));
        }

        Ok(result)
    }

    /// Like `diff_added_lines` but also returns the total number of deleted
    /// lines across all hunks in the diff.  Used by the post-commit stats-cost
    /// estimator to detect deletion-heavy commits without a second git invocation.
    pub fn diff_added_lines_with_deleted_count(
        &self,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<(HashMap<String, Vec<u32>>, usize), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string());
        args.push("--no-color".to_string());
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        parse_diff_added_lines(&diff_output)
    }

    /// Get list of changed files between two refs using `git diff --name-only`
    /// Returns a Vec of file paths that differ between the two refs
    pub fn diff_changed_files(
        &self,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<String>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("--name-only".to_string());
        args.push("-z".to_string()); // NUL-separated output for proper UTF-8 handling
        // Use permissive rename detection to properly handle renames
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        let output = exec_git_with_profile(&args, InternalGitProfile::RawDiffParse)?;

        // With -z, output is NUL-separated. The output may contain a trailing NUL.
        let files: Vec<String> = output
            .stdout
            .split(|&b| b == 0)
            .filter(|bytes| !bytes.is_empty())
            .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .collect();

        Ok(files)
    }

    /// Get added line ranges from git diff between a commit and the working directory
    /// Returns a HashMap of file paths to vectors of added line numbers
    ///
    /// Get added line ranges from git diff between a commit and the working directory,
    /// along with information about which lines are pure insertions (old_count=0).
    ///
    /// Returns (all_added_lines, pure_insertion_lines)
    /// Pure insertions are lines that were added without modifying existing lines at that position.
    #[allow(clippy::type_complexity)]
    pub fn diff_workdir_added_lines_with_insertions(
        &self,
        from_ref: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<(HashMap<String, Vec<u32>>, HashMap<String, Vec<u32>>), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string()); // Zero context lines
        args.push("--no-color".to_string());
        args.push("--no-renames".to_string());
        args.push(from_ref.to_string());

        // See diff_added_lines for why non-ASCII pathspecs need post-filtering.
        let needs_post_filter = if let Some(paths) = pathspecs {
            if paths.is_empty() {
                return Ok((HashMap::new(), HashMap::new()));
            }
            if paths.len() > MAX_PATHSPEC_ARGS || has_non_ascii_pathspec(paths) {
                true
            } else {
                args.push("--".to_string());
                for path in paths {
                    args.push(path.clone());
                }
                false
            }
        } else {
            false
        };

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        let (mut all_added, mut pure_insertions) =
            parse_diff_added_lines_with_insertions(&diff_output)?;

        if needs_post_filter && let Some(paths) = pathspecs {
            let nfc_paths: HashSet<String> = paths.iter().map(|s| s.nfc().collect()).collect();
            all_added.retain(|path, _| nfc_paths.contains(path));
            pure_insertions.retain(|path, _| nfc_paths.contains(path));
        }

        Ok((all_added, pure_insertions))
    }

    pub fn fetch_branch(&self, branch_name: &str, remote_name: &str) -> Result<(), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("fetch".to_string());
        args.push(remote_name.to_string());
        args.push(branch_name.to_string());
        exec_git(&args)?;
        Ok(())
    }
}

pub fn find_repository(global_args: &[String]) -> Result<Repository, GitAiError> {
    let mut rev_parse_args = global_args.to_owned();
    rev_parse_args.push("rev-parse".to_string());
    // Use --git-dir instead of --absolute-git-dir for compatibility with Git < 2.13
    // (--absolute-git-dir was added in Git 2.13; older versions output the literal
    // string "absolute-git-dir" instead of the resolved path).
    rev_parse_args.push("--is-bare-repository".to_string());
    rev_parse_args.push("--git-dir".to_string());
    rev_parse_args.push("--git-common-dir".to_string());

    let rev_parse_output = exec_git(&rev_parse_args)?;
    let rev_parse_stdout = String::from_utf8(rev_parse_output.stdout)?;
    let mut lines = rev_parse_stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());

    let is_bare = match lines.next() {
        Some("true") => true,
        Some("false") => false,
        Some(other) => {
            return Err(GitAiError::Generic(format!(
                "Unexpected --is-bare-repository output: {}",
                other
            )));
        }
        None => {
            return Err(GitAiError::Generic(
                "Missing --is-bare-repository output from git rev-parse".to_string(),
            ));
        }
    };

    let git_dir_str = lines.next().ok_or_else(|| {
        GitAiError::Generic("Missing --git-dir output from git rev-parse".to_string())
    })?;
    let git_common_dir_str = lines.next().ok_or_else(|| {
        GitAiError::Generic("Missing --git-common-dir output from git rev-parse".to_string())
    })?;
    let command_base_dir = resolve_command_base_dir(global_args)?;
    let git_dir = if Path::new(git_dir_str).is_relative() {
        command_base_dir.join(git_dir_str)
    } else {
        PathBuf::from(git_dir_str)
    };
    let git_common_dir = if Path::new(git_common_dir_str).is_relative() {
        command_base_dir.join(git_common_dir_str)
    } else {
        PathBuf::from(git_common_dir_str)
    };

    if !git_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git directory does not exist: {}",
            git_dir.display()
        )));
    }
    if !git_common_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git common directory does not exist: {}",
            git_common_dir.display()
        )));
    }

    let workdir = if is_bare {
        git_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Git directory has no parent: {}",
                git_dir.display()
            ))
        })?
    } else {
        let mut top_level_args = global_args.to_owned();
        top_level_args.push("rev-parse".to_string());
        top_level_args.push("--show-toplevel".to_string());
        let output = exec_git(&top_level_args)?;
        PathBuf::from(String::from_utf8(output.stdout)?.trim())
    };

    if !workdir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Work directory does not exist: {}",
            workdir.display()
        )));
    }

    // Ensure all internal git commands use a stable repository root consistently.
    let mut normalized_global_args = global_args.to_owned();
    let command_root = if is_bare {
        git_dir.display().to_string()
    } else {
        workdir.display().to_string()
    };

    if normalized_global_args.is_empty() {
        normalized_global_args = vec!["-C".to_string(), command_root];
    } else if normalized_global_args.len() == 2
        && normalized_global_args[0] == "-C"
        && normalized_global_args[1] != command_root
    {
        normalized_global_args[1] = command_root;
    }

    // Canonicalize workdir for reliable path comparisons (especially on Windows)
    // On Windows, canonical paths use the \\?\ UNC prefix, which makes path.starts_with()
    // comparisons work correctly. We store both regular and canonical versions.
    let canonical_workdir = workdir.canonicalize().map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to canonicalize working directory {}: {}",
            workdir.display(),
            e
        ))
    })?;

    let worktree_ai_dir = worktree_storage_ai_dir(&git_dir, &git_common_dir);
    let storage = if worktree_ai_dir == git_dir.join("ai") {
        RepoStorage::for_repo_path(&git_dir, &workdir)?
    } else {
        RepoStorage::for_isolated_worktree_storage(&worktree_ai_dir, &workdir)?
    };

    Ok(Repository {
        global_args: normalized_global_args,
        storage,
        git_dir,
        git_common_dir,
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

#[doc(hidden)]
pub fn resolve_command_base_dir(global_args: &[String]) -> Result<PathBuf, GitAiError> {
    let mut base: Option<PathBuf> = None;
    let mut idx = 0usize;

    while idx < global_args.len() {
        if global_args[idx] == "-C" {
            let path_arg = global_args.get(idx + 1).ok_or_else(|| {
                GitAiError::Generic("Missing path after -C in global git args".to_string())
            })?;

            let next_base = PathBuf::from(path_arg);
            base = Some(if next_base.is_absolute() {
                next_base
            } else {
                let current = match &base {
                    Some(existing) => existing.clone(),
                    None => std::env::current_dir().map_err(GitAiError::IoError)?,
                };
                current.join(next_base)
            });
            idx += 2;
            continue;
        }
        idx += 1;
    }

    match base {
        Some(base) => Ok(base),
        None => std::env::current_dir().map_err(GitAiError::IoError),
    }
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

    let canonical_git_dir = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf());
    let canonical_common_dir = git_common_dir
        .canonicalize()
        .unwrap_or_else(|_| git_common_dir.to_path_buf());

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

struct DiscoveredRepositoryPaths {
    command_root: PathBuf,
    workdir: PathBuf,
    git_dir: PathBuf,
    git_common_dir: PathBuf,
}

fn discover_repository_paths_no_git_exec(
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
        if is_valid_git_dir(&start) {
            let workdir = start.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Git directory has no parent workdir: {}",
                    start.display()
                ))
            })?;
            let git_common_dir = common_dir_for_git_dir(&start).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve common dir for git dir: {}",
                    start.display()
                ))
            })?;
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
            let git_dir = git_dir_for_worktree(workdir).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve git dir for worktree: {}",
                    workdir.display()
                ))
            })?;
            let git_common_dir = common_dir_for_git_dir(&git_dir).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "Unable to resolve common dir for git dir: {}",
                    git_dir.display()
                ))
            })?;
            return Ok(DiscoveredRepositoryPaths {
                command_root: workdir.to_path_buf(),
                workdir: workdir.to_path_buf(),
                git_dir,
                git_common_dir,
            });
        }
    }

    if let Some(worktree_root) = worktree_root_for_path(&start) {
        let git_dir = git_dir_for_worktree(&worktree_root).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Unable to resolve git dir for worktree: {}",
                worktree_root.display()
            ))
        })?;
        let git_common_dir = common_dir_for_git_dir(&git_dir).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Unable to resolve common dir for git dir: {}",
                git_dir.display()
            ))
        })?;
        return Ok(DiscoveredRepositoryPaths {
            command_root: worktree_root.clone(),
            workdir: worktree_root,
            git_dir,
            git_common_dir,
        });
    }

    let mut current = Some(start.as_path());
    while let Some(dir) = current {
        if dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
            let workdir = dir.parent().ok_or_else(|| {
                GitAiError::Generic(format!("Git directory has no parent: {}", dir.display()))
            })?;
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

fn git_config_file_for_repo_paths(
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

fn repository_object_hash_kind_for_path_no_git_exec(
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

    let canonical_workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.clone());

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

fn repository_from_discovered_paths(
    command_root: &Path,
    workdir: &Path,
    git_dir: &Path,
    git_common_dir: &Path,
) -> Result<Repository, GitAiError> {
    if !git_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git directory does not exist: {}",
            git_dir.display()
        )));
    }
    if !git_common_dir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Git common directory does not exist: {}",
            git_common_dir.display()
        )));
    }
    if !workdir.is_dir() {
        return Err(GitAiError::Generic(format!(
            "Work directory does not exist: {}",
            workdir.display()
        )));
    }

    let canonical_workdir = workdir.canonicalize().map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to canonicalize working directory {}: {}",
            workdir.display(),
            e
        ))
    })?;

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
/// `.git` directories (nested independent repos) and `.git` files that point
/// to a *linked worktree* (i.e., `gitdir: .../worktrees/…`) are treated as
/// boundaries — a file inside such a directory belongs to a different repo.
///
/// `.git` files that point to a *submodule* (i.e., `gitdir: .git/modules/…`)
/// are intentionally transparent: the parent repo tracks the submodule's
/// files, so they should still be considered part of the parent's workdir.
fn has_intervening_git_dir(file_path: &Path, workdir: &Path) -> bool {
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
        if is_valid_git_dir(&potential_git) {
            // A valid .git directory indicates a separate independent repo.
            return true;
        }
        if potential_git.is_file() {
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

pub fn find_repository_in_path(path: &str) -> Result<Repository, GitAiError> {
    let global_args = vec!["-C".to_string(), path.to_string()];
    find_repository(&global_args)
}

/// Find the git repository that contains the given file path by walking up the directory tree.
///
/// This function is useful when working with multi-repository workspaces where the workspace
/// root itself may not be a git repository, but contains multiple independent git repositories.
///
/// # Arguments
///  * `file_path` - Absolute path to a file
///  * `workspace_root` - Optional workspace root path. If provided, the search will stop at this
///    boundary to avoid finding repositories outside the workspace.
///
/// # Returns
/// * `Ok(Repository)` - The repository containing the file
/// * `Err(GitAiError)` - If no repository is found or other errors occur
pub fn find_repository_for_file(
    file_path: &str,
    workspace_root: Option<&str>,
) -> Result<Repository, GitAiError> {
    let file_path = PathBuf::from(file_path);

    // Get the directory containing the file (or the path itself if it's a directory)
    let start_dir = if file_path.is_dir() {
        file_path.clone()
    } else {
        file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| file_path.clone())
    };

    // Canonicalize paths for consistent comparison
    let start_dir = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.clone());

    let workspace_boundary = workspace_root.map(|root| {
        PathBuf::from(root)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(root))
    });

    // Walk up the directory tree looking for a .git directory
    let mut current_dir = Some(start_dir.as_path());

    while let Some(dir) = current_dir {
        // Check if we've reached the workspace boundary
        if let Some(ref boundary) = workspace_boundary {
            // Stop if we've gone above the workspace root
            if !dir.starts_with(boundary) && dir != boundary.as_path() {
                break;
            }
        }

        // Check for .git directory or file (file for submodules/worktrees)
        let git_path = dir.join(".git");
        let git_path_metadata = std::fs::metadata(&git_path).ok();
        let git_path_is_file = git_path_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.is_file());
        let git_path_is_dir = git_path_metadata.is_some_and(|metadata| metadata.is_dir());
        if git_path_is_file || (git_path_is_dir && is_valid_git_dir(&git_path)) {
            // Found a .git - but we need to check if this is a submodule
            // Submodules have a .git file (not directory) that points to the parent's .git/modules
            if git_path_is_file {
                // This is a submodule - read the file to check if it points to modules/
                if let Ok(content) = std::fs::read_to_string(&git_path)
                    && content.contains("gitdir:")
                    && content.contains("/modules/")
                {
                    // This is a submodule, skip it and continue searching up
                    current_dir = dir.parent();
                    continue;
                }
            }

            // Found a real git repository, use find_repository_in_path
            return find_repository_in_path(&dir.to_string_lossy());
        }

        current_dir = dir.parent();
    }

    Err(GitAiError::Generic(format!(
        "No git repository found for file: {}",
        file_path.display()
    )))
}

/// Group edited file paths by their containing git repository.
///
/// This function takes a list of file paths and groups them by the git repository
/// they belong to. Files that don't belong to any repository are collected separately.
///
/// # Arguments
/// * `file_paths` - List of absolute file paths to group
/// * `workspace_root` - Optional workspace root to limit repository detection
///
/// # Returns
/// A tuple of:
/// * `HashMap<PathBuf, (Repository, Vec<String>)>` - Map of repo root to (repo, file paths)
/// * `Vec<String>` - Files that couldn't be associated with any repository
#[allow(clippy::type_complexity)]
pub fn group_files_by_repository(
    file_paths: &[String],
    workspace_root: Option<&str>,
) -> (HashMap<PathBuf, (Repository, Vec<String>)>, Vec<String>) {
    let mut repo_files: HashMap<PathBuf, (Repository, Vec<String>)> = HashMap::new();
    let mut orphan_files: Vec<String> = Vec::new();

    for file_path in file_paths {
        match find_repository_for_file(file_path, workspace_root) {
            Ok(repo) => {
                let workdir = match repo.workdir() {
                    Ok(dir) => dir,
                    Err(_) => {
                        orphan_files.push(file_path.clone());
                        continue;
                    }
                };

                repo_files
                    .entry(workdir.clone())
                    .or_insert_with(|| (repo, Vec::new()))
                    .1
                    .push(file_path.clone());
            }
            Err(_) => {
                orphan_files.push(file_path.clone());
            }
        }
    }

    (repo_files, orphan_files)
}

/// Helper to execute a git command
pub fn exec_git(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command and return output regardless of exit status.
/// Callers that need success-only behavior should use `exec_git*`.
pub fn exec_git_allow_nonzero(args: &[String]) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile(args, InternalGitProfile::General)
}

/// Helper to execute a git command with an explicit internal profile and return output
/// regardless of exit status.
pub fn exec_git_allow_nonzero_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, profile, &[])
}

pub fn exec_git_allow_nonzero_with_env(
    args: &[String],
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    exec_git_allow_nonzero_with_profile_and_env(args, InternalGitProfile::General, envs)
}

#[cfg(feature = "test-support")]
fn spawn_probe_log(effective_args: &[String]) {
    let Ok(path) = std::env::var("GIT_AI_SPAWN_LOG") else {
        return;
    };
    let sub = effective_args
        .iter()
        .find(|a| !a.starts_with('-') && !a.contains('=') && !a.contains('/') && !a.contains('\\'))
        .cloned()
        .unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{}", sub);
    }
}

#[cfg(not(feature = "test-support"))]
#[inline]
fn spawn_probe_log(_effective_args: &[String]) {}

fn exec_git_allow_nonzero_with_profile_and_env(
    args: &[String],
    profile: InternalGitProfile,
    envs: &[(&str, &OsStr)],
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    spawn_probe_log(&effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args);
    apply_internal_git_env(&mut cmd);
    for (key, value) in envs {
        cmd.env(key, value);
    }

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.output().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdout piped and stderr inherited.
///
/// This is used by streaming consumers that cannot call `exec_git*` without
/// buffering all stdout in memory.
pub fn spawn_git_stdout(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

/// Spawn a git command with stdin/stdout/stderr inherited from git-ai.
///
/// This is used when a command intentionally delegates rendering and paging
/// behavior to git instead of consuming output internally.
pub fn spawn_git_passthrough(args: &[String]) -> Result<Child, GitAiError> {
    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(&effective_args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    cmd.spawn().map_err(GitAiError::IoError)
}

pub(crate) const INTERNAL_GIT_ENV_REMOVE: &[&str] = &[
    "GIT_EXTERNAL_DIFF",
    "GIT_DIFF_OPTS",
    "GIT_TRACE",
    "GIT_TRACE2_BRIEF",
    "GIT_TRACE2_CONFIG_PARAMS",
    "GIT_TRACE2_ENV_VARS",
    "GIT_TRACE2_EVENT_NESTING",
    "GIT_TRACE2_PARENT_NAME",
    "GIT_TRACE2_PARENT_SID",
];

pub(crate) const INTERNAL_GIT_ENV_SET: &[(&str, &str)] = &[
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

pub(crate) fn apply_internal_git_env(cmd: &mut Command) {
    for key in INTERNAL_GIT_ENV_REMOVE {
        cmd.env_remove(key);
    }
    for (key, value) in INTERNAL_GIT_ENV_SET {
        cmd.env(key, value);
    }
}

/// Helper to execute a git command with an explicit internal profile.
pub fn exec_git_with_profile(
    args: &[String],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let output = exec_git_allow_nonzero_with_profile(args, profile)?;

    if !output.status.success() {
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GitAiError::GitCliError {
            code,
            stderr,
            args: effective_args,
        });
    }

    Ok(output)
}

/// Helper to execute a git command with data provided on stdin
pub fn exec_git_stdin(args: &[String], stdin_data: &[u8]) -> Result<Output, GitAiError> {
    exec_git_stdin_with_profile(args, stdin_data, InternalGitProfile::General)
}

/// Spawn a fully-piped git child for `effective_args` and start a thread
/// writing `stdin_data` to it. Writing stdin in a separate thread avoids
/// deadlock: if we wrote all stdin before reading stdout, the child's stdout
/// pipe buffer could fill up, causing the child to block on write, which
/// prevents it from consuming more stdin, which would block our write_all.
type StdinWriterHandle = std::thread::JoinHandle<std::io::Result<()>>;

fn spawn_git_stdin_piped(
    effective_args: &[String],
    stdin_data: &[u8],
) -> Result<(Child, Option<StdinWriterHandle>), GitAiError> {
    spawn_probe_log(effective_args);
    let mut cmd = Command::new(config::Config::get().git_cmd());
    cmd.args(effective_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_internal_git_env(&mut cmd);

    #[cfg(windows)]
    {
        if !is_interactive_terminal() {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }

    let mut child = cmd.spawn().map_err(GitAiError::IoError)?;

    let stdin_handle = child.stdin.take().map(|mut stdin| {
        let data = stdin_data.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(&data)
        })
    });

    Ok((child, stdin_handle))
}

/// Like `exec_git_stdin`, but streams the child's stdout to `on_line` one line
/// at a time instead of buffering the entire output in memory. Use this for
/// commands whose output can be arbitrarily large (e.g. batched
/// `diff-tree --stdin -p`), where `wait_with_output()` would hold the full
/// output (plus a lossy-conversion copy) in memory at once.
///
/// Each line is lossily UTF-8 converted individually and passed without its
/// trailing `\n` (and at most one preceding `\r`), matching `str::lines()`.
pub fn exec_git_stdin_streaming(
    args: &[String],
    stdin_data: &[u8],
    mut on_line: impl FnMut(&str),
) -> Result<(), GitAiError> {
    use std::io::BufRead;

    let effective_args = args_with_internal_git_profile(
        &args_with_disabled_hooks_if_needed(args),
        InternalGitProfile::General,
    );
    let (mut child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    // Drain stderr concurrently so the child can never block on a full stderr
    // pipe while we are still reading stdout.
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let read_result = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break Ok(()),
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                on_line(&String::from_utf8_lossy(&buf));
            }
            Err(e) => break Err(e),
        }
    };
    if let Err(e) = read_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(GitAiError::IoError(e));
    }

    let status = child.wait().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !status.success() {
        let stderr_bytes = stderr_handle
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        return Err(GitAiError::GitCliError {
            code: status.code(),
            stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
            args: effective_args,
        });
    }

    Ok(())
}

/// Helper to execute a git command with data provided on stdin and an explicit profile.
pub fn exec_git_stdin_with_profile(
    args: &[String],
    stdin_data: &[u8],
    profile: InternalGitProfile,
) -> Result<Output, GitAiError> {
    // TODO Make sure to handle process signals, etc.
    let effective_args =
        args_with_internal_git_profile(&args_with_disabled_hooks_if_needed(args), profile);
    let (child, stdin_handle) = spawn_git_stdin_piped(&effective_args, stdin_data)?;

    let output = child.wait_with_output().map_err(GitAiError::IoError)?;

    if let Some(handle) = stdin_handle
        && let Err(e) = handle.join().expect("stdin writer thread panicked")
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(GitAiError::IoError(e));
    }

    if !output.status.success() {
        let code = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(GitAiError::GitCliError {
            code,
            stderr,
            args: effective_args,
        });
    }

    Ok(output)
}

pub(crate) fn batch_read_paths_at_treeishes(
    repo: &Repository,
    requests: &[(String, String)],
) -> Result<HashMap<(String, String), String>, GitAiError> {
    if requests.is_empty() {
        return Ok(HashMap::new());
    }

    let mut args = repo.global_args_for_exec();
    args.extend([
        "cat-file".to_string(),
        "--batch-check=%(objectname) %(objecttype)".to_string(),
    ]);

    let stdin_data = requests
        .iter()
        .map(|(treeish, path)| format!("{treeish}:{path}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let output = exec_git_stdin(&args, stdin_data.as_bytes())?;
    let stdout = String::from_utf8(output.stdout)?;
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() != requests.len() {
        return Err(GitAiError::Generic(format!(
            "git cat-file returned {} records for {} path requests",
            lines.len(),
            requests.len()
        )));
    }

    let mut request_blob_oids: HashMap<(String, String), String> = HashMap::new();
    let mut unique_blob_oids = Vec::new();
    let mut seen_blob_oids = HashSet::new();

    for (request, line) in requests.iter().zip(lines) {
        let mut parts = line.split_whitespace();
        let Some(oid) = parts.next() else {
            continue;
        };
        if parts.next() != Some("blob") {
            continue;
        }
        let oid = oid.to_string();
        request_blob_oids.insert(request.clone(), oid.clone());
        if seen_blob_oids.insert(oid.clone()) {
            unique_blob_oids.push(oid);
        }
    }

    let blob_contents = crate::git::authorship_traversal::batch_read_blobs_with_oids(
        &repo.global_args_for_exec(),
        &unique_blob_oids,
    )?;

    let mut contents = HashMap::new();
    for (request, blob_oid) in request_blob_oids {
        if let Some(content) = blob_contents.get(&blob_oid) {
            contents.insert(request, content.clone());
        }
    }
    Ok(contents)
}

/// Parse git version string (e.g., "git version 2.39.3 (Apple Git-146)") to extract major, minor, patch.
/// Returns None if the version cannot be parsed.
#[doc(hidden)]
pub fn parse_git_version(version_str: &str) -> Option<(u32, u32, u32)> {
    // Expected format: "git version X.Y.Z" or "git version X.Y.Z.windows.N" etc.
    let version_str = version_str.trim();
    let parts: Vec<&str> = version_str.split_whitespace().collect();

    // Find the version number part (usually the 3rd element)
    let version_part = parts.get(2)?;

    // Parse version like "2.39.3" or "2.39.3.windows.1"
    let version_nums: Vec<&str> = version_part.split('.').collect();
    if version_nums.len() < 2 {
        return None;
    }

    let major = version_nums.first()?.parse::<u32>().ok()?;
    let minor = version_nums.get(1)?.parse::<u32>().ok()?;
    let patch = version_nums
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Parse git diff output to extract added line numbers per file
///
/// Parses unified diff format hunk headers like:
/// @@ -10,2 +15,5 @@
///
/// This means: old file line 10 (2 lines), new file line 15 (5 lines)
/// We extract the "new file" line numbers to know which lines were added.
///
/// Also returns the total number of deleted lines across all hunks so that
/// callers can estimate the cost of a deletion-heavy commit without a second
/// git invocation.
fn parse_diff_added_lines(
    diff_output: &str,
) -> Result<(HashMap<String, Vec<u32>>, usize), GitAiError> {
    let parsed = parse_diff_added_lines_internal(diff_output);
    Ok((parsed.all_lines, parsed.total_deleted))
}

struct ParsedDiffAddedLines {
    all_lines: HashMap<String, Vec<u32>>,
    insertion_lines: HashMap<String, Vec<u32>>,
    total_deleted: usize,
}

struct ActiveDiffHunk {
    new_line: u32,
    is_pure_insertion: bool,
}

fn parse_diff_added_lines_internal(diff_output: &str) -> ParsedDiffAddedLines {
    let mut result: HashMap<String, Vec<u32>> = HashMap::new();
    let mut insertion_lines: HashMap<String, Vec<u32>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut current_hunk: Option<ActiveDiffHunk> = None;
    let mut total_deleted: usize = 0;

    for line in diff_output.lines() {
        if let Some(path_opt) = parse_new_file_path_from_plus_header_line(line) {
            current_file = path_opt;
            current_hunk = None;
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some((new_start, _new_count, old_count)) = parse_hunk_header_counts(line) {
                // Count deleted lines for ALL hunks, including those from purely
                // deleted files (where current_file is None because +++ /dev/null).
                total_deleted += old_count as usize;
                current_hunk = Some(ActiveDiffHunk {
                    new_line: new_start,
                    is_pure_insertion: old_count == 0,
                });
            }
        } else if let Some(hunk) = current_hunk.as_mut() {
            if line.starts_with('+') {
                if let Some(ref file) = current_file {
                    result.entry(file.clone()).or_default().push(hunk.new_line);
                    if hunk.is_pure_insertion {
                        insertion_lines
                            .entry(file.clone())
                            .or_default()
                            .push(hunk.new_line);
                    }
                }
                hunk.new_line += 1;
            } else if line.starts_with('-') || line.starts_with('\\') {
                // Removed lines and "\ No newline at end of file" markers do
                // not advance the new-file line cursor.
            } else {
                hunk.new_line += 1;
            }
        }
    }

    // Sort and deduplicate line numbers for each file
    for lines in result.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }
    for lines in insertion_lines.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    ParsedDiffAddedLines {
        all_lines: result,
        insertion_lines,
        total_deleted,
    }
}

/// Parses the unified diff output to extract line numbers of added lines,
/// along with information about which are pure insertions (old_count=0).
///
/// Returns (all_added_lines, pure_insertion_lines)
#[allow(clippy::type_complexity)]
#[doc(hidden)]
pub fn parse_diff_added_lines_with_insertions(
    diff_output: &str,
) -> Result<(HashMap<String, Vec<u32>>, HashMap<String, Vec<u32>>), GitAiError> {
    let parsed = parse_diff_added_lines_internal(diff_output);
    Ok((parsed.all_lines, parsed.insertion_lines))
}

/// Returns true if any path in the set contains non-ASCII characters.
/// Used to decide whether git pathspecs need post-filtering instead of CLI args,
/// since NFC-normalised pathspecs may not match NFD entries in git's index.
fn has_non_ascii_pathspec(paths: &HashSet<String>) -> bool {
    paths.iter().any(|s| !s.is_ascii())
}

fn normalize_diff_path_token(path: &str) -> String {
    let unescaped = crate::utils::unescape_git_path(path.trim_end());
    let prefixes = ["a/", "b/", "c/", "w/", "i/", "o/"];
    let stripped = prefixes
        .iter()
        .find_map(|prefix| unescaped.strip_prefix(prefix))
        .unwrap_or(&unescaped);
    // Apply NFC normalization so decomposed (NFD) paths from git diff match
    // NFC paths used internally (see normalize_to_posix).
    stripped.nfc().collect()
}

fn parse_new_file_path_from_plus_header_line(line: &str) -> Option<Option<String>> {
    let raw = line.strip_prefix("+++ ")?;
    if raw.trim_end() == "/dev/null" {
        return Some(None);
    }
    Some(Some(normalize_diff_path_token(raw)))
}

fn parse_hunk_header_counts(line: &str) -> Option<(u32, u32, u32)> {
    // Find the part between @@ and @@
    let parts: Vec<&str> = line.split("@@").collect();
    if parts.len() < 2 {
        return None;
    }

    let hunk_info = parts[1].trim();

    // Split by space to get old and new ranges
    let ranges: Vec<&str> = hunk_info.split_whitespace().collect();
    if ranges.len() < 2 {
        return None;
    }

    // Parse the old file range (starts with '-')
    let old_range = ranges
        .iter()
        .find(|r| r.starts_with('-'))?
        .trim_start_matches('-');

    // Parse "start,count" or just "start" for old range
    let old_parts: Vec<&str> = old_range.split(',').collect();
    let old_count: u32 = if old_parts.len() > 1 {
        old_parts[1].parse().ok()?
    } else {
        1 // If no count specified, it's 1 line
    };

    // Parse the new file range (starts with '+')
    let new_range = ranges
        .iter()
        .find(|r| r.starts_with('+'))?
        .trim_start_matches('+');

    // Parse "start,count" or just "start"
    let new_parts: Vec<&str> = new_range.split(',').collect();
    let start: u32 = new_parts[0].parse().ok()?;
    let count: u32 = if new_parts.len() > 1 {
        new_parts[1].parse().ok()?
    } else {
        1 // If no count specified, it's 1 line
    };

    Some((start, count, old_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn explicit_command_env(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.map(|v| v.to_string_lossy().to_string()))
    }

    #[test]
    fn internal_git_env_disables_trace2_targets() {
        let mut cmd = Command::new("git");
        for key in INTERNAL_GIT_ENV_REMOVE {
            cmd.env(key, "inherited");
        }
        for (key, _) in INTERNAL_GIT_ENV_SET {
            cmd.env(key, "inherited");
        }

        apply_internal_git_env(&mut cmd);

        for key in INTERNAL_GIT_ENV_REMOVE {
            assert_eq!(explicit_command_env(&cmd, key), Some(None));
        }
        for (key, value) in INTERNAL_GIT_ENV_SET {
            assert_eq!(
                explicit_command_env(&cmd, key),
                Some(Some((*value).to_string()))
            );
        }
    }

    #[test]
    fn author_config_overlays_full_identity() {
        let git_identity = GitAuthorIdentity {
            name: Some("Git User".to_string()),
            email: Some("git@example.com".to_string()),
        };
        let author = config::AuthorConfig {
            name: Some("Config User".to_string()),
            email: Some("config@example.com".to_string()),
        };

        assert_eq!(
            git_identity
                .with_author_config(&author)
                .formatted()
                .as_deref(),
            Some("Config User <config@example.com>")
        );
    }

    #[test]
    fn author_config_supports_partial_overrides() {
        let git_identity = GitAuthorIdentity {
            name: Some("Git User".to_string()),
            email: Some("git@example.com".to_string()),
        };

        let name_only = config::AuthorConfig {
            name: Some("Config User".to_string()),
            email: None,
        };
        assert_eq!(
            git_identity
                .with_author_config(&name_only)
                .formatted()
                .as_deref(),
            Some("Config User <git@example.com>")
        );

        let email_only = config::AuthorConfig {
            name: None,
            email: Some("config@example.com".to_string()),
        };
        assert_eq!(
            git_identity
                .with_author_config(&email_only)
                .formatted()
                .as_deref(),
            Some("Git User <config@example.com>")
        );
    }

    #[test]
    fn test_parse_git_version_standard() {
        // Standard git version format
        assert_eq!(parse_git_version("git version 2.39.3"), Some((2, 39, 3)));
        assert_eq!(parse_git_version("git version 2.23.0"), Some((2, 23, 0)));
        assert_eq!(parse_git_version("git version 1.8.5"), Some((1, 8, 5)));
    }

    #[test]
    fn test_parse_git_version_apple_git() {
        // macOS Apple Git format
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn test_parse_git_version_windows() {
        // Windows git format
        assert_eq!(
            parse_git_version("git version 2.42.0.windows.2"),
            Some((2, 42, 0))
        );
    }

    #[test]
    fn test_parse_git_version_no_patch() {
        // Version without patch number
        assert_eq!(parse_git_version("git version 2.39"), Some((2, 39, 0)));
    }

    #[test]
    fn test_parse_git_version_with_newline() {
        // Version string with trailing newline
        assert_eq!(parse_git_version("git version 2.39.3\n"), Some((2, 39, 3)));
    }

    #[test]
    fn test_parse_git_version_invalid() {
        // Invalid formats should return None
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("not a version"), None);
        assert_eq!(parse_git_version("git version"), None);
        assert_eq!(parse_git_version("git version x.y.z"), None);
    }

    #[test]
    fn disable_internal_git_hooks_guard_applies_to_spawned_threads() {
        let args = vec!["status".to_string()];
        let _guard = disable_internal_git_hooks();

        let spawned_args = args.clone();
        let forwarded =
            std::thread::spawn(move || args_with_disabled_hooks_if_needed(&spawned_args))
                .join()
                .expect("thread should join");

        assert_eq!(forwarded[0], "-c");
        assert!(forwarded[1].starts_with("core.hooksPath="));
    }

    #[test]
    fn patch_profile_applies_canonical_machine_parse_flags() {
        let args = vec!["diff".to_string(), "HEAD^".to_string(), "HEAD".to_string()];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--src-prefix=a/"));
        assert!(rewritten.iter().any(|arg| arg == "--dst-prefix=b/"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(
            rewritten
                .iter()
                .any(|arg| arg == "--diff-algorithm=default")
        );
        assert!(rewritten.iter().any(|arg| arg == "--indent-heuristic"));
        assert!(rewritten.iter().any(|arg| arg == "--inter-hunk-context=0"));
    }

    #[test]
    fn numstat_profile_disables_renames_and_external_renderers() {
        let args = vec![
            "diff".to_string(),
            "--numstat".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::NumstatParse);
        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
        assert!(rewritten.iter().any(|arg| arg == "--no-renames"));
    }

    #[test]
    fn numstat_profile_strips_short_rename_and_copy_flags() {
        let args = vec![
            "diff".to_string(),
            "--numstat".to_string(),
            "-M90%".to_string(),
            "-C".to_string(),
            "-C75%".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::NumstatParse);
        assert!(!rewritten.iter().any(|arg| arg == "-C"));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("-M")));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("-C")));
        assert!(rewritten.iter().any(|arg| arg == "--no-renames"));
    }

    #[test]
    fn general_profile_is_noop() {
        let args = vec!["status".to_string(), "--porcelain=v2".to_string()];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::General);
        assert_eq!(rewritten, args);
    }

    #[test]
    fn patch_profile_strips_conflicting_ext_diff_and_color_flags() {
        let args = vec![
            "diff".to_string(),
            "--ext-diff".to_string(),
            "--color=always".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(!rewritten.iter().any(|arg| arg == "--ext-diff"));
        assert!(!rewritten.iter().any(|arg| arg.starts_with("--color")));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
    }

    #[test]
    fn patch_profile_strips_split_prefix_args() {
        let args = vec![
            "diff".to_string(),
            "--src-prefix".to_string(),
            "SRC/".to_string(),
            "--dst-prefix".to_string(),
            "DST/".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);

        assert!(!rewritten.iter().any(|arg| arg == "--src-prefix"));
        assert!(!rewritten.iter().any(|arg| arg == "--dst-prefix"));
        assert!(!rewritten.iter().any(|arg| arg == "SRC/"));
        assert!(!rewritten.iter().any(|arg| arg == "DST/"));
        assert!(rewritten.iter().any(|arg| arg == "--src-prefix=a/"));
        assert!(rewritten.iter().any(|arg| arg == "--dst-prefix=b/"));
    }

    #[test]
    fn profile_rewrite_does_not_strip_pathspec_tokens_after_double_dash() {
        let args = vec![
            "diff".to_string(),
            "--color=always".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
            "--".to_string(),
            "--color".to_string(),
            "--relative".to_string(),
            "file.txt".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::PatchParse);
        let separator = rewritten
            .iter()
            .position(|arg| arg == "--")
            .expect("rewritten args should keep pathspec separator");
        assert_eq!(
            rewritten[separator + 1..],
            [
                "--color".to_string(),
                "--relative".to_string(),
                "file.txt".to_string()
            ]
        );
    }

    #[test]
    fn raw_diff_profile_keeps_rename_flags_untouched() {
        let args = vec![
            "diff".to_string(),
            "--raw".to_string(),
            "-z".to_string(),
            "-M".to_string(),
            "HEAD^".to_string(),
            "HEAD".to_string(),
        ];
        let rewritten = args_with_internal_git_profile(&args, InternalGitProfile::RawDiffParse);
        assert!(rewritten.iter().any(|arg| arg == "-M"));
        assert!(rewritten.iter().any(|arg| arg == "--no-ext-diff"));
        assert!(rewritten.iter().any(|arg| arg == "--no-textconv"));
        assert!(rewritten.iter().any(|arg| arg == "--no-color"));
        assert!(rewritten.iter().any(|arg| arg == "--no-relative"));
    }
}
