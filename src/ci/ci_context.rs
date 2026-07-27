use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::rewrite::{RewriteEvent, handle_rewrite_event};
use crate::error::GitAiError;
use crate::git::notes_api::{read_authorship_v3, read_note};
use crate::git::refs::{
    AI_AUTHORSHIP_FORK_TRACKING_REF, copy_missing_notes_for_commits_from_ref, ref_exists,
};
use crate::git::repository::{
    CommitRange, Repository, exec_git, exec_git_allow_nonzero, exec_git_stdin,
};
use crate::git::sync_authorship::fetch_authorship_notes;
use std::fs;
use std::path::PathBuf;

#[cfg(windows)]
const NULL_HOOKS: &str = "NUL";
#[cfg(not(windows))]
const NULL_HOOKS: &str = "/dev/null";

#[derive(Debug)]
pub enum CiEvent {
    Merge {
        merge_commit_sha: String,
        head_ref: String,
        head_sha: String,
        base_ref: String,
        base_sha: String,
        /// Clone URL of the fork repository, if this PR came from a fork.
        /// When set, notes will be fetched from the fork before processing.
        fork_clone_url: Option<String>,
    },
    Sync {
        previous_head_sha: String,
        head_sha: String,
        base_ref: String,
        base_sha: String,
        previous_base_sha: Option<String>,
        previous_head_fetch_remote: Option<String>,
    },
}

/// Result of running CiContext
#[derive(Debug)]
pub enum CiRunResult {
    /// Authorship was successfully rewritten for a squash/rebase merge
    AuthorshipRewritten {
        #[allow(dead_code)]
        authorship_log: AuthorshipLog,
    },
    /// Authorship was successfully rewritten for one or more rebased commits
    SyncAuthorshipRewritten {
        #[allow(dead_code)]
        commit_count: usize,
    },
    /// Skipped: merge commit has multiple parents (simple merge - authorship already present)
    SkippedSimpleMerge,
    /// Skipped: merge commit equals head (fast-forward - no rewrite needed)
    SkippedFastForward,
    /// Skipped: the PR synchronize event was not a rebase-like rewrite
    SkippedNonRebaseSync,
    /// Skipped: one or more current PR commits already have authorship notes
    SkippedExistingSyncNotes,
    /// Authorship already exists for this commit
    AlreadyExists {
        #[allow(dead_code)]
        authorship_log: AuthorshipLog,
    },
    /// Fork notes were fetched and preserved for a merge commit from a fork
    ForkNotesPreserved,
    /// No AI authorship to track (pre-git-ai commits or human-only code)
    NoAuthorshipAvailable,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CiRunOptions {
    pub skip_fetch_notes: bool,
    pub skip_fetch_base: bool,
    pub skip_fetch_fork_notes: bool,
    pub skip_fetch_sync_refs: bool,
    pub skip_push: bool,
}

#[derive(Debug)]
pub struct CiContext {
    pub repo: Repository,
    pub event: CiEvent,
    pub temp_dir: PathBuf,
}

impl CiContext {
    /// Create a CiContext with an existing repository (no automatic cleanup)
    #[allow(dead_code)]
    pub fn with_repository(repo: Repository, event: CiEvent) -> Self {
        CiContext {
            repo,
            event,
            temp_dir: PathBuf::new(), // Empty path indicates no cleanup needed
        }
    }

    pub fn run(&self) -> Result<CiRunResult, GitAiError> {
        self.run_with_options(CiRunOptions::default())
    }

    pub fn run_with_options(&self, options: CiRunOptions) -> Result<CiRunResult, GitAiError> {
        match &self.event {
            CiEvent::Merge {
                merge_commit_sha,
                head_ref: _,
                head_sha,
                base_ref,
                base_sha,
                fork_clone_url,
            } => {
                println!("Working repository is in {}", self.repo.path().display());

                if options.skip_fetch_notes {
                    println!("Skipping authorship history fetch");
                } else {
                    println!("Fetching authorship history");
                    // Ensure we have the full authorship history before checking for existing notes
                    fetch_authorship_notes(&self.repo, "origin")?;
                    println!("Fetched authorship history");
                }

                // Check if authorship already exists for this commit
                match read_authorship_v3(&self.repo, merge_commit_sha) {
                    Ok(existing_log) => {
                        println!("{} already has authorship", merge_commit_sha);
                        return Ok(CiRunResult::AlreadyExists {
                            authorship_log: existing_log,
                        });
                    }
                    Err(e) => {
                        if read_note(&self.repo, merge_commit_sha).is_some() {
                            return Err(e);
                        }
                    }
                }

                // Only handle squash or rebase-like merges.
                // Skip simple merge commits (2+ parents) and fast-forward merges (merge commit == head).
                let merge_commit = self.repo.find_commit(merge_commit_sha.clone())?;
                let parent_count = merge_commit.parents().count();
                if parent_count > 1 {
                    // For fork PRs with merge commits, the merged commits keep
                    // their fork SHAs. Import only notes for those PR commits,
                    // then push the scoped local authorship ref.
                    if fork_clone_url.is_some() {
                        let (_source_base, original_commits) = self
                            .original_pr_commits_for_merge_commit(
                                &merge_commit,
                                head_sha,
                                base_ref,
                                base_sha,
                            );
                        let fork_notes_imported = self.import_fork_notes_for_commits(
                            fork_clone_url,
                            &original_commits,
                            options,
                        )?;
                        if !self.has_notes_for_any_commit(&original_commits)? {
                            println!(
                                "No local authorship notes available for fork PR commits; skipping fork note push"
                            );
                            return Ok(CiRunResult::SkippedSimpleMerge);
                        }

                        println!(
                            "{} has {} parents (merge commit from fork) - preserving fork notes",
                            merge_commit_sha, parent_count
                        );
                        if fork_notes_imported > 0 {
                            println!(
                                "Imported {} fork authorship notes for PR commits",
                                fork_notes_imported
                            );
                        } else {
                            println!(
                                "Using existing local authorship notes (no additional fork notes fetched)"
                            );
                        }
                        if options.skip_push {
                            println!("Skipping authorship push (--skip-push). Done.");
                        } else {
                            println!("Pushing authorship...");
                            self.repo.push_authorship("origin")?;
                            println!("Pushed authorship. Done.");
                        }
                        return Ok(CiRunResult::ForkNotesPreserved);
                    }
                    println!(
                        "{} has {} parents (simple merge)",
                        merge_commit_sha, parent_count
                    );
                    return Ok(CiRunResult::SkippedSimpleMerge);
                }

                if merge_commit_sha == head_sha {
                    if fork_clone_url.is_some() {
                        let (_source_base, original_commits) =
                            self.original_pr_commits(head_sha, base_ref, base_sha);
                        let fork_notes_imported = self.import_fork_notes_for_commits(
                            fork_clone_url,
                            &original_commits,
                            options,
                        )?;
                        if self.has_notes_for_any_commit(&original_commits)? {
                            println!(
                                "{} equals head {} (fast-forward from fork) - preserving fork notes",
                                merge_commit_sha, head_sha
                            );
                            println!(
                                "Imported {} fork authorship notes for PR commits",
                                fork_notes_imported
                            );
                            if options.skip_push {
                                println!("Skipping authorship push (--skip-push). Done.");
                            } else {
                                println!("Pushing authorship...");
                                self.repo.push_authorship("origin")?;
                                println!("Pushed authorship. Done.");
                            }
                            return Ok(CiRunResult::ForkNotesPreserved);
                        }
                    }
                    println!(
                        "{} equals head {} (fast-forward)",
                        merge_commit_sha, head_sha
                    );
                    return Ok(CiRunResult::SkippedFastForward);
                }
                println!(
                    "Rewriting authorship for {} -> {} (squash or rebase-like merge)",
                    head_sha, merge_commit_sha
                );
                if options.skip_fetch_base {
                    println!("Skipping base branch fetch for {}", base_ref);
                    self.repo.revparse_single(base_ref).map_err(|e| {
                        GitAiError::Generic(format!(
                            "Failed to resolve base ref '{}' locally while --skip-fetch-base is set: {}",
                            base_ref, e
                        ))
                    })?;
                } else {
                    println!("Fetching base branch {}", base_ref);
                    // Ensure we have all the required commits from the base branch
                    self.repo.fetch_branch(base_ref, "origin").map_err(|e| {
                        GitAiError::Generic(format!(
                            "Failed to fetch base branch '{}': {}",
                            base_ref, e
                        ))
                    })?;
                    println!("Fetched base branch.");
                }

                // Detect squash vs rebase merge by counting commits:
                //   squash: N original commits → 1 merge commit
                //   rebase: N original commits → N rebased commits
                let (original_commits_base, original_commits) =
                    self.original_pr_commits(head_sha, base_ref, base_sha);

                println!(
                    "Original commits in PR: {} (from {:?})",
                    original_commits.len(),
                    original_commits_base
                );

                self.import_fork_notes_for_commits(fork_clone_url, &original_commits, options)?;

                // For multi-commit PRs, decide whether the merge is a rebase
                // (N original → N new commits) or a squash (N → 1) by walking
                // back from merge_commit_sha.
                let is_rebase_merge = if original_commits.len() > 1 {
                    let mut new_commits =
                        self.get_rebased_commits(merge_commit_sha, original_commits.len());

                    // #1473: on a linear base branch the first-parent walk above can
                    // return pre-existing base commits rather than rebased PR commits,
                    // so a squash merge's count matches a rebase's and gets
                    // misclassified (PR notes then land on unrelated commits). Restrict
                    // to commits the merge actually introduced
                    // (`base_sha..merge_commit_sha`; see gitrevisions(7)) — a squash
                    // yields exactly one, so it can't look like a rebase. An empty
                    // `base_sha` (transient API failure) safely skips the filter and
                    // falls back to the pre-#1473 behavior.
                    if !base_sha.is_empty() {
                        let introduced: std::collections::HashSet<String> =
                            CommitRange::new_infer_refname(
                                &self.repo,
                                base_sha.clone(),
                                merge_commit_sha.to_string(),
                                None,
                            )
                            .map(|r| r.all_commits())
                            .unwrap_or_default()
                            .into_iter()
                            .collect();
                        if !introduced.is_empty() {
                            new_commits.retain(|sha| introduced.contains(sha));
                        }
                    }

                    new_commits.len() == original_commits.len()
                } else {
                    false
                };

                if is_rebase_merge {
                    println!(
                        "Detected rebase merge: {} original commits → {} new commits",
                        original_commits.len(),
                        original_commits.len()
                    );
                    // Rebase merge — shift each original commit's note onto its
                    // rebased counterpart via the range-diff/hunk-shift path.
                    handle_rewrite_event(
                        &self.repo,
                        RewriteEvent::NonFastForward {
                            old_tip: head_sha.to_string(),
                            new_tip: merge_commit_sha.to_string(),
                            onto: if base_sha.is_empty() {
                                None
                            } else {
                                Some(base_sha.to_string())
                            },
                        },
                    )?;
                } else {
                    println!(
                        "Detected squash merge: {} original commit(s) → 1 merge commit",
                        original_commits.len()
                    );
                    // Squash merge — reconstruct the single merge commit's
                    // authorship by unioning every source commit's note, using the
                    // exact same handler the local daemon uses for `merge --squash`.
                    let onto = if base_sha.is_empty() {
                        // No base SHA: fall back to the merge commit's first parent
                        // so the squash handler can still enumerate source commits.
                        self.repo
                            .find_commit(merge_commit_sha.to_string())
                            .ok()
                            .and_then(|c| c.parent(0).ok())
                            .map(|p| p.id())
                            .unwrap_or_else(|| base_ref.to_string())
                    } else {
                        base_sha.to_string()
                    };
                    handle_rewrite_event(
                        &self.repo,
                        RewriteEvent::SquashMerge {
                            source_head: head_sha.to_string(),
                            squash_commit: merge_commit_sha.to_string(),
                            onto,
                        },
                    )?;
                }
                println!("Rewrote authorship.");

                // Check if authorship was created for THIS specific commit
                match read_authorship_v3(&self.repo, merge_commit_sha) {
                    Ok(authorship_log) => {
                        // A note may be reconstructed with only human attestations
                        // (e.g. a PR whose contributor never used git-ai, so there
                        // are no AI sessions/prompts to carry forward). There is no
                        // AI authorship to track in that case.
                        let has_ai_authorship = !authorship_log.metadata.sessions.is_empty()
                            || !authorship_log.metadata.prompts.is_empty();
                        if !has_ai_authorship {
                            println!(
                                "No AI authorship to track for this commit (no AI-touched files in PR)"
                            );
                            return Ok(CiRunResult::NoAuthorshipAvailable);
                        }
                        if options.skip_push {
                            println!("Skipping authorship push (--skip-push). Done.");
                        } else {
                            println!("Pushing authorship...");
                            self.repo.push_authorship("origin")?;
                            println!("Pushed authorship. Done.");
                        }
                        Ok(CiRunResult::AuthorshipRewritten { authorship_log })
                    }
                    Err(e) => {
                        if read_note(&self.repo, merge_commit_sha).is_some() {
                            return Err(e);
                        }
                        println!(
                            "No AI authorship to track for this commit (no AI-touched files in PR)"
                        );
                        Ok(CiRunResult::NoAuthorshipAvailable)
                    }
                }
            }
            CiEvent::Sync {
                previous_head_sha,
                head_sha,
                base_ref,
                base_sha,
                previous_base_sha,
                previous_head_fetch_remote,
            } => {
                println!("Working repository is in {}", self.repo.path().display());

                if options.skip_fetch_notes {
                    println!("Skipping authorship history fetch");
                } else {
                    println!("Fetching authorship history");
                    fetch_authorship_notes(&self.repo, "origin")?;
                    println!("Fetched authorship history");
                }

                if previous_head_sha == head_sha {
                    println!(
                        "{} equals previous head {} (no head rewrite)",
                        head_sha, previous_head_sha
                    );
                    return Ok(CiRunResult::SkippedFastForward);
                }
                ensure_commit_available_for_sync(
                    &self.repo,
                    previous_head_sha,
                    previous_head_fetch_remote.as_deref().unwrap_or("origin"),
                    "refs/git-ai/ci-sync/previous-head",
                    options.skip_fetch_sync_refs,
                )?;
                ensure_commit_available_for_sync(
                    &self.repo,
                    head_sha,
                    "origin",
                    "refs/git-ai/ci-sync/head",
                    options.skip_fetch_sync_refs,
                )?;

                if commit_is_ancestor(&self.repo, previous_head_sha, head_sha)? {
                    println!(
                        "{} is an ancestor of {} (fast-forward PR update)",
                        previous_head_sha, head_sha
                    );
                    return Ok(CiRunResult::SkippedFastForward);
                }

                let base_target =
                    if !base_sha.is_empty() && self.repo.revparse_single(base_sha).is_ok() {
                        base_sha.as_str()
                    } else {
                        base_ref.as_str()
                    };
                let resolved_previous_base_sha = match previous_base_sha {
                    Some(previous_base_sha) if !previous_base_sha.is_empty() => {
                        previous_base_sha.clone()
                    }
                    _ => self
                        .repo
                        .merge_base(previous_head_sha.clone(), base_target.to_string())?,
                };
                let resolved_base_sha = self
                    .repo
                    .merge_base(head_sha.clone(), base_target.to_string())?;
                let resolved_base_target_sha = self.repo.revparse_single(base_target)?.id();

                if resolved_base_sha != resolved_base_target_sha {
                    println!(
                        "Skipping PR sync authorship rewrite: current PR head is not based on {}",
                        resolved_base_target_sha
                    );
                    return Ok(CiRunResult::SkippedNonRebaseSync);
                }

                if resolved_previous_base_sha == resolved_base_sha {
                    println!(
                        "Skipping PR sync authorship rewrite: PR base did not advance during sync"
                    );
                    return Ok(CiRunResult::SkippedNonRebaseSync);
                }

                if !commit_is_ancestor(&self.repo, &resolved_previous_base_sha, &resolved_base_sha)?
                {
                    println!(
                        "Skipping PR sync authorship rewrite: previous PR base is not an ancestor of current PR base"
                    );
                    return Ok(CiRunResult::SkippedNonRebaseSync);
                }

                let original_commits = commits_in_range_oldest_first(
                    &self.repo,
                    &resolved_previous_base_sha,
                    previous_head_sha,
                    "previous PR",
                )?;
                let new_commits = commits_in_range_oldest_first(
                    &self.repo,
                    &resolved_base_sha,
                    head_sha,
                    "current PR",
                )?;

                println!(
                    "Detected non-fast-forward PR sync: {} original commits -> {} new commits",
                    original_commits.len(),
                    new_commits.len()
                );

                if original_commits.is_empty() || new_commits.is_empty() {
                    println!("No AI authorship to track for this PR rebase (empty commit range)");
                    return Ok(CiRunResult::NoAuthorshipAvailable);
                }

                let notes_before = count_commits_with_authorship_notes(&self.repo, &new_commits);
                if notes_before > 0 {
                    println!(
                        "Skipping PR sync authorship rewrite: {} of {} current PR commits already have authorship notes",
                        notes_before,
                        new_commits.len()
                    );
                    return Ok(CiRunResult::SkippedExistingSyncNotes);
                }

                if !commit_ranges_have_same_patch_ids(&self.repo, &original_commits, &new_commits)?
                {
                    println!(
                        "Skipping PR sync authorship rewrite: previous and current commit ranges are not rebase-equivalent"
                    );
                    return Ok(CiRunResult::SkippedNonRebaseSync);
                }

                println!(
                    "Rewriting authorship for rebased PR head: {} -> {}",
                    previous_head_sha, head_sha
                );

                handle_rewrite_event(
                    &self.repo,
                    RewriteEvent::NonFastForward {
                        old_tip: previous_head_sha.to_string(),
                        new_tip: head_sha.to_string(),
                        onto: Some(resolved_base_sha.clone()),
                    },
                )?;
                println!("Rewrote authorship.");

                let notes_after = count_commits_with_authorship_notes(&self.repo, &new_commits);
                if notes_after == 0 {
                    println!(
                        "No AI authorship to track for this PR rebase (no AI-touched files in PR)"
                    );
                    return Ok(CiRunResult::NoAuthorshipAvailable);
                }

                if options.skip_push {
                    println!("Skipping authorship push (--skip-push). Done.");
                } else {
                    println!("Pushing authorship...");
                    self.repo.push_authorship("origin")?;
                    println!("Pushed authorship. Done.");
                }

                Ok(CiRunResult::SyncAuthorshipRewritten {
                    commit_count: notes_after,
                })
            }
        }
    }

    pub fn teardown(&self) -> Result<(), GitAiError> {
        // Skip cleanup if temp_dir is empty (repository was provided externally)
        if self.temp_dir.as_os_str().is_empty() {
            return Ok(());
        }
        fs::remove_dir_all(self.temp_dir.clone())?;
        Ok(())
    }

    /// Fetch authorship notes from a fork repository URL into the fork tracking ref.
    /// Returns Ok(true) if notes were found and fetched,
    /// Ok(false) if no notes exist on the fork.
    fn fetch_fork_notes(repo: &Repository, fork_url: &str) -> Result<bool, GitAiError> {
        let tracking_ref = AI_AUTHORSHIP_FORK_TRACKING_REF;

        // Check if the fork has notes
        let mut ls_remote_args = repo.global_args_for_exec();
        ls_remote_args.push("ls-remote".to_string());
        ls_remote_args.push(fork_url.to_string());
        ls_remote_args.push("refs/notes/ai".to_string());

        match exec_git(&ls_remote_args) {
            Ok(output) => {
                let result = String::from_utf8_lossy(&output.stdout).to_string();
                if result.trim().is_empty() {
                    return Ok(false);
                }
            }
            Err(e) => {
                return Err(GitAiError::Generic(format!(
                    "Failed to check fork for authorship notes: {}",
                    e
                )));
            }
        }

        // Fetch notes from the fork URL into a tracking ref
        let fetch_refspec = format!("+refs/notes/ai:{}", tracking_ref);
        let mut fetch_args = repo.global_args_for_exec();
        fetch_args.push("-c".to_string());
        fetch_args.push(format!("core.hooksPath={}", NULL_HOOKS));
        fetch_args.push("fetch".to_string());
        fetch_args.push("--no-tags".to_string());
        fetch_args.push("--recurse-submodules=no".to_string());
        fetch_args.push("--no-write-fetch-head".to_string());
        fetch_args.push("--no-write-commit-graph".to_string());
        fetch_args.push("--no-auto-maintenance".to_string());
        fetch_args.push(fork_url.to_string());
        fetch_args.push(fetch_refspec);

        exec_git(&fetch_args)?;

        Ok(true)
    }

    fn import_fork_notes_for_commits(
        &self,
        fork_clone_url: &Option<String>,
        commit_shas: &[String],
        options: CiRunOptions,
    ) -> Result<usize, GitAiError> {
        let Some(fork_url) = fork_clone_url else {
            return Ok(0);
        };
        if commit_shas.is_empty() {
            println!("No PR commits found; skipping fork authorship note import");
            return Ok(0);
        }

        let source_ref_available = if options.skip_fetch_fork_notes {
            println!(
                "Skipping fork authorship notes fetch; using {} if it exists",
                AI_AUTHORSHIP_FORK_TRACKING_REF
            );
            ref_exists(&self.repo, AI_AUTHORSHIP_FORK_TRACKING_REF)
        } else {
            println!(
                "Fetching authorship notes from fork into {}...",
                AI_AUTHORSHIP_FORK_TRACKING_REF
            );
            match Self::fetch_fork_notes(&self.repo, fork_url) {
                Ok(true) => {
                    println!("Fetched authorship notes from fork");
                    true
                }
                Ok(false) => {
                    println!("No authorship notes found on fork");
                    false
                }
                Err(e) => {
                    println!(
                        "Warning: Failed to fetch fork notes ({}), continuing without them",
                        e
                    );
                    false
                }
            }
        };

        if !source_ref_available {
            return Ok(0);
        }

        let copied = copy_missing_notes_for_commits_from_ref(
            &self.repo,
            AI_AUTHORSHIP_FORK_TRACKING_REF,
            commit_shas,
        )?;
        println!(
            "Imported {} fork authorship notes for {} PR commits from {}",
            copied,
            commit_shas.len(),
            AI_AUTHORSHIP_FORK_TRACKING_REF
        );
        Ok(copied)
    }

    fn has_notes_for_any_commit(&self, commit_shas: &[String]) -> Result<bool, GitAiError> {
        // Backend-aware: on the HTTP backend notes live in the notes-db cache,
        // so a refs/notes/ai-only check would always report "no notes".
        Ok(!crate::git::notes_api::commits_with_notes(&self.repo, commit_shas)?.is_empty())
    }

    fn original_pr_commits(
        &self,
        head_sha: &str,
        base_ref: &str,
        base_sha: &str,
    ) -> (Option<String>, Vec<String>) {
        if !base_sha.is_empty()
            && let Ok(mut commits) = CommitRange::new_infer_refname(
                &self.repo,
                base_sha.to_string(),
                head_sha.to_string(),
                None,
            )
            .map(|r| r.all_commits())
            && !commits.is_empty()
        {
            commits.reverse();
            return (Some(format!("base_sha {}", base_sha)), commits);
        }

        let merge_base = self
            .repo
            .merge_base(head_sha.to_string(), base_ref.to_string())
            .ok();

        if let Some(ref base) = merge_base
            && let Ok(mut commits) =
                CommitRange::new_infer_refname(&self.repo, base.clone(), head_sha.to_string(), None)
                    .map(|r| r.all_commits())
            && !commits.is_empty()
        {
            commits.reverse();
            return (Some(format!("merge-base {}", base)), commits);
        }

        let resolved_head = self
            .repo
            .revparse_single(head_sha)
            .map(|obj| obj.id())
            .unwrap_or_else(|_| head_sha.to_string());
        (
            merge_base.map(|base| format!("merge-base {}", base)),
            vec![resolved_head],
        )
    }

    fn original_pr_commits_for_merge_commit(
        &self,
        merge_commit: &crate::git::repository::Commit<'_>,
        head_sha: &str,
        base_ref: &str,
        base_sha: &str,
    ) -> (Option<String>, Vec<String>) {
        if let Ok(first_parent) = merge_commit.parent(0) {
            let first_parent_sha = first_parent.id();
            if let Ok(mut commits) = CommitRange::new_infer_refname(
                &self.repo,
                first_parent_sha.clone(),
                head_sha.to_string(),
                None,
            )
            .map(|r| r.all_commits())
                && !commits.is_empty()
            {
                commits.reverse();
                return (
                    Some(format!("merge first-parent {}", first_parent_sha)),
                    commits,
                );
            }
        }

        self.original_pr_commits(head_sha, base_ref, base_sha)
    }

    /// Get the rebased commits by walking back from merge_commit_sha.
    /// For a rebase merge with N original commits, there should be N new commits
    /// ending at merge_commit_sha.
    #[doc(hidden)]
    pub fn get_rebased_commits(
        &self,
        merge_commit_sha: &str,
        expected_count: usize,
    ) -> Vec<String> {
        let mut commits = Vec::new();
        // Resolve to a full SHA up front so the entries are comparable to the
        // full 40-char SHAs produced by `git rev-list`. Callers like
        // `git-ai ci local merge` may pass an abbreviated `merge_commit_sha`; the
        // remaining entries already come from parent ids, which are full.
        let mut current_sha = self
            .repo
            .revparse_single(merge_commit_sha)
            .map(|obj| obj.id())
            .unwrap_or_else(|_| merge_commit_sha.to_string());

        for _ in 0..expected_count {
            commits.push(current_sha.clone());

            // Get the parent of current commit
            match self.repo.find_commit(current_sha.clone()) {
                Ok(commit) => {
                    let parents: Vec<_> = commit.parents().collect();
                    if parents.len() != 1 {
                        // Not a linear chain (merge commit or root), stop here
                        break;
                    }
                    current_sha = parents[0].id().to_string();
                }
                Err(_) => break,
            }
        }

        // Reverse to get oldest-to-newest order (same as original_commits)
        commits.reverse();
        commits
    }
}

fn commits_in_range_oldest_first(
    repo: &Repository,
    start_sha: &str,
    end_sha: &str,
    label: &str,
) -> Result<Vec<String>, GitAiError> {
    if start_sha == end_sha {
        return Ok(Vec::new());
    }

    let mut commits =
        CommitRange::new_infer_refname(repo, start_sha.to_string(), end_sha.to_string(), None)
            .map_err(|e| {
                GitAiError::Generic(format!(
                    "Failed to resolve {} commit range {}..{}: {}",
                    label, start_sha, end_sha, e
                ))
            })?
            .all_commits();

    commits.reverse();
    Ok(commits)
}

fn count_commits_with_authorship_notes(repo: &Repository, commits: &[String]) -> usize {
    commits
        .iter()
        .filter(|sha| read_note(repo, sha).is_some())
        .count()
}

fn ensure_commit_available_for_sync(
    repo: &Repository,
    commit_sha: &str,
    fetch_remote: &str,
    fetch_ref: &str,
    skip_fetch: bool,
) -> Result<(), GitAiError> {
    let commit_spec = format!("{}^{{commit}}", commit_sha);
    if repo.revparse_single(&commit_spec).is_ok() {
        return Ok(());
    }
    if skip_fetch {
        return Err(GitAiError::Generic(format!(
            "Commit {} is not available locally and sync ref fetch is disabled",
            commit_sha
        )));
    }

    println!("Fetching PR sync commit {} into {}", commit_sha, fetch_ref);
    let mut args = repo.global_args_for_exec();
    args.push("fetch".to_string());
    if sync_fetch_remote_supports_lazy_blobs(repo, fetch_remote)? {
        args.push("--filter=blob:none".to_string());
    }
    args.push("--no-tags".to_string());
    args.push(fetch_remote.to_string());
    args.push(format!("{}:{}", commit_sha, fetch_ref));
    exec_git(&args)?;
    repo.revparse_single(&commit_spec)?;
    Ok(())
}

fn sync_fetch_remote_supports_lazy_blobs(
    repo: &Repository,
    fetch_remote: &str,
) -> Result<bool, GitAiError> {
    if fetch_remote.contains("://") || fetch_remote.contains('@') {
        return Ok(false);
    }

    let mut args = repo.global_args_for_exec();
    args.push("config".to_string());
    args.push("--bool".to_string());
    args.push("--get".to_string());
    args.push(format!("remote.{}.promisor", fetch_remote));

    let output = exec_git_allow_nonzero(&args)?;
    match output.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&output.stdout).trim() == "true"),
        Some(1) => Ok(false),
        code => Err(GitAiError::GitCliError {
            code,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            args,
        }),
    }
}

fn stable_patch_id_for_commit(repo: &Repository, commit_sha: &str) -> Result<String, GitAiError> {
    let mut show_args = repo.global_args_for_exec();
    show_args.extend(
        [
            "show",
            "--format=",
            "--no-notes",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--diff-algorithm=default",
            "--indent-heuristic",
            commit_sha,
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    let patch = exec_git(&show_args)?;

    let mut patch_id_args = repo.global_args_for_exec();
    patch_id_args.push("patch-id".to_string());
    patch_id_args.push("--stable".to_string());
    let patch_id_output = exec_git_stdin(&patch_id_args, &patch.stdout)?;
    let stdout = String::from_utf8_lossy(&patch_id_output.stdout);
    let patch_id = stdout
        .split_whitespace()
        .next()
        .map(|value| value.to_string())
        .unwrap_or_else(|| "empty-patch".to_string());
    Ok(patch_id)
}

fn stable_patch_ids_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Result<Vec<String>, GitAiError> {
    commit_shas
        .iter()
        .map(|sha| stable_patch_id_for_commit(repo, sha))
        .collect()
}

fn commit_ranges_have_same_patch_ids(
    repo: &Repository,
    original_commits: &[String],
    new_commits: &[String],
) -> Result<bool, GitAiError> {
    if original_commits.len() != new_commits.len() {
        return Ok(false);
    }

    let original_patch_ids = stable_patch_ids_for_commits(repo, original_commits)?;
    let new_patch_ids = stable_patch_ids_for_commits(repo, new_commits)?;
    Ok(original_patch_ids == new_patch_ids)
}

fn commit_is_ancestor(
    repo: &Repository,
    ancestor_sha: &str,
    descendant_sha: &str,
) -> Result<bool, GitAiError> {
    let ancestor = repo.revparse_single(ancestor_sha)?.id();
    let descendant = repo.revparse_single(descendant_sha)?.id();

    let mut args = repo.global_args_for_exec();
    args.push("merge-base".to_string());
    args.push("--is-ancestor".to_string());
    args.push(ancestor);
    args.push(descendant);

    let output = exec_git_allow_nonzero(&args)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(GitAiError::GitCliError {
            code,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            args,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_utils::TmpRepo;

    #[test]
    fn test_ci_event_debug() {
        let event = CiEvent::Merge {
            merge_commit_sha: "abc123".to_string(),
            head_ref: "feature".to_string(),
            head_sha: "def456".to_string(),
            base_ref: "main".to_string(),
            base_sha: "ghi789".to_string(),
            fork_clone_url: None,
        };

        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("Merge"));
        assert!(debug_str.contains("abc123"));
        assert!(debug_str.contains("feature"));
    }

    #[test]
    fn test_ci_run_result_debug() {
        let result = CiRunResult::SkippedSimpleMerge;
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("SkippedSimpleMerge"));

        let result2 = CiRunResult::SkippedFastForward;
        let debug_str2 = format!("{:?}", result2);
        assert!(debug_str2.contains("SkippedFastForward"));

        let result3 = CiRunResult::NoAuthorshipAvailable;
        let debug_str3 = format!("{:?}", result3);
        assert!(debug_str3.contains("NoAuthorshipAvailable"));
    }

    #[test]
    fn commit_is_ancestor_returns_false_for_unrelated_histories() {
        let repo = TmpRepo::new().expect("test repo");
        repo.write_file("main.txt", "main", false)
            .expect("write main");
        let main_sha = repo.commit_all("main commit").expect("main commit");

        repo.git_command(&["switch", "--orphan", "unrelated"])
            .expect("orphan branch");
        repo.git_command(&["rm", "-rf", "--ignore-unmatch", "."])
            .expect("clear tree");
        repo.write_file("unrelated.txt", "unrelated", false)
            .expect("write unrelated");
        let unrelated_sha = repo
            .commit_all("unrelated commit")
            .expect("unrelated commit");

        assert!(
            !commit_is_ancestor(repo.gitai_repo(), &main_sha, &unrelated_sha)
                .expect("unrelated histories should not error")
        );
    }

    #[test]
    fn commit_is_ancestor_errors_for_invalid_descendant() {
        let repo = TmpRepo::new().expect("test repo");
        repo.write_file("main.txt", "main", false)
            .expect("write main");
        let main_sha = repo.commit_all("main commit").expect("main commit");

        assert!(commit_is_ancestor(repo.gitai_repo(), &main_sha, "not-a-sha").is_err());
    }

    #[test]
    fn sync_fetch_uses_blobless_only_for_named_promisor_remote() {
        let repo = TmpRepo::new().expect("test repo");

        assert!(
            !sync_fetch_remote_supports_lazy_blobs(repo.gitai_repo(), "origin")
                .expect("missing promisor config should be false")
        );

        repo.git_command(&["config", "remote.origin.promisor", "true"])
            .expect("set promisor config");
        assert!(
            sync_fetch_remote_supports_lazy_blobs(repo.gitai_repo(), "origin")
                .expect("named promisor remote should allow blobless fetch")
        );

        assert!(
            !sync_fetch_remote_supports_lazy_blobs(
                repo.gitai_repo(),
                "https://github.com/acme/repo.git"
            )
            .expect("direct URL should not use blobless fetch")
        );
        assert!(
            !sync_fetch_remote_supports_lazy_blobs(
                repo.gitai_repo(),
                "git@github.com:acme/repo.git"
            )
            .expect("direct SSH URL should not use blobless fetch")
        );
    }
}
