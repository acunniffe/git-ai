//! Faithful replays of the Graphite CLI's git command sequences, per the
//! empirical catalog in `PLAN.md` Part 1. One function per workflow family;
//! all git traffic goes through [`Exec`], which runs either traced (real git
//! wired to the per-test daemon via trace2) or blind (`git_og_with_env` +
//! trace2 disabled) for the rewrite phase of `observation = blind` scenarios.
//!
//! Daemon-sync discipline: traced commands ride the async trace2 pipeline
//! (closer to production and much faster); `sync_daemon_force` barriers run
//! only at flow boundaries — the end of each workflow round, before a blind
//! daemon restart, before a linked worktree is removed, and at the start of
//! the assertion phase (git-ai reads also pre-sync on their own).
//!
//! Key shapes implemented here:
//! - synthetic-base restack: `cat-file -p <tip>~` / `commit-tree
//!   <new-parent>^{tree} -p <tip>~ -m _` / `merge-tree
//!   --allow-unrelated-histories <synthetic> <tip>` / `commit-tree <merged>
//!   -p <new-parent> -m <original msg>` / ref move (`update-ref`, one
//!   `update-ref --stdin` batch, or `reset -q --keep` for the checked-out
//!   branch) — no `git rebase` anywhere on the happy path;
//! - conflict fallback: real `git rebase <new-base> <branch>`, then either
//!   resolve-by-taking-AI-content + `rebase --continue` or `rebase --abort`;
//! - `stash create` + `ls-files --others` snapshots at flow boundaries;
//! - `gt undo`: `reset -q --keep` / `update-ref` back to pre-restack tips.

use super::assertions;
use super::scenario::{Attribution, Observation, Scenario};
use super::stackbuilder::{
    self, AiFileState, BranchState, StackState, TRACE2_DISABLED_ENV, TRUNK, ai_lines,
    write_ai_file, write_file,
};
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefStyle {
    /// One `update-ref refs/heads/<branch> <sha>` per moved ref.
    Individual,
    /// All ref moves of a flow batched into a single `update-ref --stdin`.
    Stdin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Resolve every conflicted AI file by taking the branch's AI content,
    /// then `git rebase --continue` (gt continue).
    TakeAiContinue,
    /// `git rebase --abort` (gt abort): the branch returns to its old tip.
    Abort,
}

/// Options for the restack-style flows.
#[derive(Debug, Clone)]
pub struct FlowOpts {
    pub refstyle: RefStyle,
    /// Per-conflict resolutions, consumed in stack order; conflicts beyond the
    /// list reuse the last entry (empty list means TakeAiContinue).
    pub policies: Vec<ConflictResolution>,
    /// FLAVOR_17X: interleave `hash-object -w --stdin` storms and eager
    /// metadata `update-ref`s into the flow.
    pub storm: bool,
}

impl Default for FlowOpts {
    fn default() -> Self {
        Self {
            refstyle: RefStyle::Individual,
            policies: Vec::new(),
            storm: false,
        }
    }
}

impl FlowOpts {
    fn for_scenario(scenario: &Scenario) -> Self {
        Self {
            refstyle: if scenario.family == "REFSTDIN" {
                RefStyle::Stdin
            } else {
                RefStyle::Individual
            },
            policies: if scenario.family == "CONFLICT_ABORT" {
                vec![ConflictResolution::Abort]
            } else {
                Vec::new()
            },
            storm: scenario.family == "FLAVOR_17X",
        }
    }

    fn policy_for(&self, conflict_index: usize) -> ConflictResolution {
        self.policies
            .get(conflict_index)
            .or(self.policies.last())
            .copied()
            .unwrap_or(ConflictResolution::TakeAiContinue)
    }
}

// ---------------------------------------------------------------------------
// Execution layer
// ---------------------------------------------------------------------------

/// Runs the workflow's git commands either traced or blind, optionally from a
/// linked worktree (`cwd`).
pub struct Exec<'a> {
    repo: &'a TestRepo,
    blind: bool,
    cwd: Option<PathBuf>,
}

impl<'a> Exec<'a> {
    pub fn for_scenario(repo: &'a TestRepo, scenario: &Scenario) -> Self {
        Self {
            repo,
            blind: scenario.observation_mode() == Observation::Blind,
            cwd: None,
        }
    }

    pub fn traced(repo: &'a TestRepo) -> Self {
        Self {
            repo,
            blind: false,
            cwd: None,
        }
    }

    pub fn repo(&self) -> &'a TestRepo {
        self.repo
    }

    /// Same mode, executed from `cwd` (linked-worktree scenarios).
    pub fn at_cwd(&self, cwd: PathBuf) -> Exec<'a> {
        Exec {
            repo: self.repo,
            blind: self.blind,
            cwd: Some(cwd),
        }
    }

    /// Directory the workflow's files live in.
    pub fn workdir(&self) -> &Path {
        self.cwd.as_deref().unwrap_or_else(|| self.repo.path())
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.try_git(args)
            .unwrap_or_else(|error| panic!("gt-sim git {:?} failed: {}", args, error))
    }

    pub fn try_git(&self, args: &[&str]) -> Result<String, String> {
        self.try_git_env(args, &[])
    }

    pub fn try_git_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
        if self.blind {
            let mut env: Vec<(&str, &str)> = TRACE2_DISABLED_ENV.to_vec();
            env.extend_from_slice(envs);
            let cwd_arg;
            let mut full: Vec<&str> = Vec::new();
            if let Some(cwd) = &self.cwd {
                cwd_arg = cwd
                    .to_str()
                    .expect("worktree path should be utf-8")
                    .to_string();
                full.push("-C");
                full.push(&cwd_arg);
            }
            full.extend_from_slice(args);
            return self.repo.git_og_with_env(&full, &env);
        }

        // Tracked commands record their test-sync sessions inside
        // git_with_env; the next sync_barrier / git-ai read waits for them.
        self.repo.git_with_env(args, envs, self.cwd.as_deref())
    }

    pub fn git_stdin(&self, args: &[&str], input: &[u8]) -> String {
        let result = if self.blind {
            self.repo
                .git_og_with_stdin_and_env(args, &TRACE2_DISABLED_ENV, input)
        } else {
            self.repo.git_with_stdin(args, input)
        };
        result.unwrap_or_else(|error| panic!("gt-sim git stdin {:?} failed: {}", args, error))
    }

    /// Flow-boundary barrier: wait for the daemon to process everything
    /// issued so far. No-op for blind executions (the daemon saw nothing).
    pub fn sync_barrier(&self) {
        if !self.blind {
            self.repo.sync_daemon_force();
        }
    }

    pub fn rev_parse(&self, rev: &str) -> String {
        self.git(&["rev-parse", rev]).trim().to_string()
    }

    pub fn current_branch(&self) -> String {
        self.git(&["branch", "--show-current"]).trim().to_string()
    }
}

// ---------------------------------------------------------------------------
// Shared building blocks
// ---------------------------------------------------------------------------

/// gt's refless snapshot at flow boundaries: `stash create` + untracked-file
/// inventory.
pub fn stash_snapshot(x: &Exec) {
    x.git(&["stash", "create"]);
    x.git(&["ls-files", "--others", "--exclude-standard"]);
}

fn prologue(x: &Exec) {
    x.git(&["branch", "--show-current"]);
    x.git(&["rev-parse", "HEAD"]);
    stash_snapshot(x);
}

fn branch_ref(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// `gt sync`'s fetch prologue; returns the fetched `origin/main` tip.
fn fetch_trunk(x: &Exec) -> String {
    x.git(&[
        "fetch",
        "--no-write-fetch-head",
        "--no-tags",
        "-f",
        "origin",
        &format!("refs/heads/{TRUNK}:refs/remotes/origin/{TRUNK}"),
    ]);
    x.rev_parse(&format!("refs/remotes/origin/{TRUNK}"))
}

/// Move the checked-out branch: `update-index --refresh; status -z;
/// reset -q --keep <sha> --` (the #1976 shape). Returns false on a gt-style
/// refusal (e.g. an untracked file in the way); the flow then proceeds, like
/// gt does after a mid-restack failure.
pub fn checked_out_move(x: &Exec, target: &str) -> bool {
    let _ = x.try_git(&["update-index", "--refresh"]);
    let _ = x.try_git(&["status", "-z"]);
    x.try_git(&["reset", "-q", "--keep", target, "--"]).is_ok()
}

/// Deferred ref moves for `RefStyle::Stdin`: one `update-ref --stdin`
/// invocation applies every queued move.
#[derive(Default)]
pub struct RefBatch {
    lines: Vec<String>,
}

impl RefBatch {
    fn push(&mut self, branch: &str, new_sha: &str) {
        self.lines
            .push(format!("update {} {}\n", branch_ref(branch), new_sha));
    }

    fn flush(&mut self, x: &Exec) {
        if self.lines.is_empty() {
            return;
        }
        let payload = self.lines.concat();
        x.git_stdin(&["update-ref", "--stdin"], payload.as_bytes());
        self.lines.clear();
    }
}

fn move_ref(x: &Exec, batch: &mut RefBatch, refstyle: RefStyle, branch: &str, target: &str) {
    match refstyle {
        RefStyle::Stdin => batch.push(branch, target),
        RefStyle::Individual => {
            x.git(&["update-ref", &branch_ref(branch), target]);
        }
    }
}

/// gt 1.7.x metadata write: `hash-object -w --stdin` + a metadata
/// `update-ref` pointing at the blob.
pub fn write_metadata_ref(x: &Exec, branch: &str) {
    let payload = format!("{{\"branch\":\"{branch}\"}}\n");
    let oid = x.git_stdin(&["hash-object", "-w", "--stdin"], payload.as_bytes());
    x.git(&[
        "update-ref",
        &format!("refs/branch-metadata/{branch}"),
        oid.trim(),
    ]);
}

/// gt 1.7.x `hash-object -w --stdin` storm.
pub fn hash_object_storm(x: &Exec, count: usize) {
    for index in 0..count {
        let payload = format!("gt 1.7.x hash-object storm payload {index}\n");
        x.git_stdin(&["hash-object", "-w", "--stdin"], payload.as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Synthetic-base restack core
// ---------------------------------------------------------------------------

/// Restack `state.branches[first..]` bottom-up onto `new_parent`, replaying
/// each branch commit via the synthetic-base merge-tree core.
/// `old_parent_of_first` is the tip the first branch was previously based on
/// (used only to recognize pending-only/empty branches, which gt moves by ref
/// alone). Deferred `RefStyle::Stdin` moves accumulate in `batch`; the caller
/// flushes.
fn restack_range(
    x: &Exec,
    state: &mut StackState,
    first: usize,
    old_parent_of_first: &str,
    new_parent_of_first: &str,
    opts: &FlowOpts,
    batch: &mut RefBatch,
) {
    let current_branch = x.current_branch();
    let mut old_parent = old_parent_of_first.to_string();
    let mut new_parent = new_parent_of_first.to_string();
    let mut conflict_index = 0usize;

    for index in first..state.branches.len() {
        let name = state.branches[index].name.clone();
        let old_tip = state.branches[index].tip_sha.clone();
        let is_checked_out = current_branch == name;
        if opts.storm {
            hash_object_storm(x, 20);
        }

        // old->new shas of this branch's replayed commits, for retargeting
        // ai_commit_sha through rewrites (A1 -> A1', not blindly to the tip).
        let mut commit_map: HashMap<String, String> = HashMap::new();
        let new_tip = if old_tip == new_parent {
            // Already exactly at the new parent — nothing to do.
            old_tip.clone()
        } else if old_tip == old_parent {
            // Pending-only branch (no commits of its own): gt moves the ref
            // straight to the new parent.
            if is_checked_out {
                if checked_out_move(x, &new_parent) {
                    new_parent.clone()
                } else {
                    old_tip.clone()
                }
            } else {
                move_ref(x, batch, opts.refstyle, &name, &new_parent);
                new_parent.clone()
            }
        } else {
            match replay_branch_commits(x, &old_parent, &old_tip, &new_parent) {
                Ok((minted_tip, map)) => {
                    commit_map = map;
                    if minted_tip == old_tip {
                        // Already based on the new parent (idempotent second
                        // run): nothing moved.
                        old_tip.clone()
                    } else if is_checked_out {
                        if checked_out_move(x, &minted_tip) {
                            minted_tip
                        } else {
                            old_tip.clone()
                        }
                    } else {
                        move_ref(x, batch, opts.refstyle, &name, &minted_tip);
                        minted_tip
                    }
                }
                Err(()) => {
                    // merge-tree conflict (the cleanRebaseMergeTree failure):
                    // gt falls back to a real rebase for this branch.
                    let policy = opts.policy_for(conflict_index);
                    conflict_index += 1;
                    rebase_fallback(x, state, index, &new_parent, policy, &current_branch)
                }
            }
        };

        if opts.storm {
            write_metadata_ref(x, &name);
        }

        let rewritten = new_tip != old_tip && old_tip != old_parent;
        let branch = &mut state.branches[index];
        if let Some(ai_commit) = branch.ai_commit_sha.clone() {
            if let Some(mapped) = commit_map.get(&ai_commit) {
                branch.ai_commit_sha = Some(mapped.clone());
            } else if rewritten {
                // No per-commit map (rebase fallback): the branch collapsed to
                // a rewritten tip.
                branch.ai_commit_sha = Some(new_tip.clone());
            }
        }
        branch.tip_sha = new_tip.clone();
        old_parent = old_tip;
        new_parent = new_tip;
    }

    if first == 0 {
        state.stack_base_sha = new_parent_of_first.to_string();
    }
}

/// Replay every commit the branch carries (its first-parent chain over
/// `old_parent..old_tip`) onto `new_parent`, bottom-up, threading the new
/// parent and recording per-commit old->new mappings. Non-merge commits use
/// the synthetic-base replay; merge commits are minted as REAL merges (first
/// parent remapped, non-first parents kept as-is unless they were replayed).
/// Commits already sitting on the right parent are skipped (idempotent
/// re-runs). Returns the new tip plus the mapping; Err on the first
/// merge-tree conflict (the caller falls back to a real rebase).
fn replay_branch_commits(
    x: &Exec,
    old_parent: &str,
    old_tip: &str,
    new_parent: &str,
) -> Result<(String, HashMap<String, String>), ()> {
    let commits = x.git(&[
        "rev-list",
        "--reverse",
        "--first-parent",
        &format!("{old_parent}..{old_tip}"),
    ]);
    let mut map = HashMap::new();
    let mut cursor = new_parent.to_string();
    for commit in commits.lines().map(str::trim).filter(|c| !c.is_empty()) {
        let parents = commit_parents(x, commit);
        let first_parent = parents.first().cloned().unwrap_or_default();
        let minted = if first_parent == cursor {
            commit.to_string()
        } else if parents.len() <= 1 {
            replay_branch_commit(x, commit, &cursor)?
        } else {
            replay_merge_commit(x, commit, &parents, &cursor, &map)?
        };
        map.insert(commit.to_string(), minted.clone());
        cursor = minted;
    }
    Ok((cursor, map))
}

fn commit_parents(x: &Exec, commit: &str) -> Vec<String> {
    x.git(&["rev-list", "--parents", "-n", "1", commit])
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// Mint a rewritten merge commit: merged tree via the synthetic-base trick
/// against the new first parent, then a real merge commit keeping every
/// non-first parent (remapped when it was itself replayed).
fn replay_merge_commit(
    x: &Exec,
    old_merge: &str,
    old_parents: &[String],
    new_first_parent: &str,
    map: &HashMap<String, String>,
) -> Result<String, ()> {
    x.git(&["cat-file", "-p", &format!("{old_merge}~")]);
    let synthetic = x
        .git(&[
            "commit-tree",
            &format!("{new_first_parent}^{{tree}}"),
            "-p",
            &format!("{old_merge}~"),
            "-m",
            "_",
        ])
        .trim()
        .to_string();
    let merged = x
        .try_git(&[
            "merge-tree",
            "--allow-unrelated-histories",
            &synthetic,
            old_merge,
        ])
        .map_err(|_| ())?;
    let tree = merged
        .lines()
        .next()
        .expect("merge-tree should print a tree oid")
        .trim()
        .to_string();
    let message = x
        .git(&["log", "-1", "--format=%B", old_merge])
        .trim()
        .to_string();

    let mut args: Vec<String> = vec![
        "commit-tree".to_string(),
        tree,
        "-p".to_string(),
        new_first_parent.to_string(),
    ];
    for parent in &old_parents[1..] {
        args.push("-p".to_string());
        args.push(map.get(parent).cloned().unwrap_or_else(|| parent.clone()));
    }
    args.push("-m".to_string());
    args.push(message);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(x.git(&arg_refs).trim().to_string())
}

/// Mint the restacked commit for a single (non-merge) commit via the observed
/// plumbing sequence. Returns Err on a merge-tree conflict.
fn replay_branch_commit(x: &Exec, old_tip: &str, new_parent: &str) -> Result<String, ()> {
    x.git(&["cat-file", "-p", &format!("{old_tip}~")]);
    let synthetic = x
        .git(&[
            "commit-tree",
            &format!("{new_parent}^{{tree}}"),
            "-p",
            &format!("{old_tip}~"),
            "-m",
            "_",
        ])
        .trim()
        .to_string();
    let merged = x
        .try_git(&[
            "merge-tree",
            "--allow-unrelated-histories",
            &synthetic,
            old_tip,
        ])
        .map_err(|_| ())?;
    let tree = merged
        .lines()
        .next()
        .expect("merge-tree should print a tree oid")
        .trim()
        .to_string();
    let message = x
        .git(&["log", "-1", "--format=%B", old_tip])
        .trim()
        .to_string();
    Ok(
        x.git(&["commit-tree", &tree, "-p", new_parent, "-m", &message])
            .trim()
            .to_string(),
    )
}

/// Real `git rebase <new-base> <branch>` fallback, then continue or abort.
/// Returns the branch tip after the fallback.
fn rebase_fallback(
    x: &Exec,
    state: &StackState,
    index: usize,
    new_parent: &str,
    policy: ConflictResolution,
    original_branch: &str,
) -> String {
    let name = state.branches[index].name.clone();
    let old_tip = state.branches[index].tip_sha.clone();

    // gt refuses to restack over unstaged tracked changes; the sanctioned
    // recovery shape is a stash round-trip.
    let dirty = !x
        .git(&["status", "--porcelain", "--untracked-files=no"])
        .trim()
        .is_empty();
    if dirty {
        x.git(&["stash", "push", "-q"]);
    }

    let new_tip = match x.try_git(&["rebase", new_parent, &name]) {
        Ok(_) => x.rev_parse(&branch_ref(&name)),
        Err(_) => match policy {
            ConflictResolution::TakeAiContinue => {
                resolve_conflicts_taking_ai_content(x, &state.branches[index]);
                x.try_git_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")])
                    .unwrap_or_else(|error| {
                        panic!("{}: rebase --continue failed: {}", state.scenario_id, error)
                    });
                x.rev_parse(&branch_ref(&name))
            }
            ConflictResolution::Abort => {
                x.git(&["rebase", "--abort"]);
                old_tip
            }
        },
    };

    // The rebase leaves HEAD on <branch>; gt returns to where the user was.
    if original_branch != name {
        x.git(&["checkout", original_branch]);
    }
    if dirty {
        x.git(&["stash", "pop", "-q"]);
    }
    new_tip
}

fn resolve_conflicts_taking_ai_content(x: &Exec, branch: &BranchState) {
    let conflicted = x.git(&["diff", "--name-only", "--diff-filter=U"]);
    for path in conflicted.lines().map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(file) = branch.files.iter().find(|file| file.path == path) {
            let content = file.committed_ai_lines.join("\n") + "\n";
            write_worktree_file(x, path, &content);
        } else {
            // Conflict on a non-AI path: take the branch side.
            x.git(&["checkout", "--theirs", "--", path]);
        }
        x.git(&["add", "--", path]);
    }
}

fn write_worktree_file(x: &Exec, path: &str, content: &str) {
    let full_path = x.workdir().join(path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full_path, content).unwrap();
}

// ---------------------------------------------------------------------------
// Workflow families
// ---------------------------------------------------------------------------

/// gt sync, trunk fast-forward variant: fetch, then move trunk (forward
/// `reset -q --keep` when trunk is checked out, `update-ref` otherwise). No
/// branch restacks.
pub fn sync_ff(x: &Exec, state: &mut StackState) {
    prologue(x);
    let target = fetch_trunk(x);
    move_trunk(
        x,
        state,
        &target,
        RefStyle::Individual,
        &mut RefBatch::default(),
    );
    stash_snapshot(x);
}

fn move_trunk(
    x: &Exec,
    state: &mut StackState,
    target: &str,
    refstyle: RefStyle,
    batch: &mut RefBatch,
) {
    if state.local_trunk_sha == target {
        return;
    }
    if x.current_branch() == TRUNK {
        if !checked_out_move(x, target) {
            return;
        }
    } else {
        move_ref(x, batch, refstyle, TRUNK, target);
    }
    state.local_trunk_sha = target.to_string();
}

/// gt sync with restack: fetch + trunk move, then the synthetic-base rewrite
/// core bottom-up through the stack.
pub fn sync_restack(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    prologue(x);
    if opts.storm {
        hash_object_storm(x, 20);
    }
    let target = fetch_trunk(x);
    let mut batch = RefBatch::default();
    move_trunk(x, state, &target, opts.refstyle, &mut batch);
    let old_base = state.stack_base_sha.clone();
    restack_range(x, state, 0, &old_base, &target, opts, &mut batch);
    batch.flush(x);
    stash_snapshot(x);
}

/// gt restack: the rewrite core onto local trunk, no fetch.
pub fn restack(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    prologue(x);
    let target = x.rev_parse(&branch_ref(TRUNK));
    let mut batch = RefBatch::default();
    let old_base = state.stack_base_sha.clone();
    restack_range(x, state, 0, &old_base, &target, opts, &mut batch);
    batch.flush(x);
    stash_snapshot(x);
}

/// gt create: stage fresh AI work, snapshot, `checkout -b`, stash-wrapped
/// commit, metadata refs. The created branch stacks on the current top and is
/// appended to `state.branches`.
pub fn create(x: &Exec, state: &mut StackState, round: usize) {
    let top = state
        .branches
        .last()
        .expect("stack should have branches")
        .name
        .clone();
    if x.current_branch() != top {
        x.git(&["checkout", &top]);
    }

    let name = format!("gt-created-{}", round + 1);
    let path = format!("gt_created_{}_ai.txt", round + 1);
    let lines = ai_lines(&format!("created {}", round + 1), 3);
    write_ai_file(x.repo(), &path, &lines);
    x.git(&["add", "--", &path]);
    x.git(&["stash", "create"]);
    x.git(&["checkout", "-b", &name]);
    x.git(&["commit", "-q", "-m", &format!("gt create {name}")]);
    let tip = x.rev_parse("HEAD");
    write_metadata_ref(x, &name);

    state.branches.push(BranchState {
        name,
        files: vec![AiFileState {
            path,
            committed_ai_lines: lines,
            uncommitted_ai_lines: Vec::new(),
        }],
        tip_sha: tip.clone(),
        ai_commit_sha: Some(tip),
    });
}

/// gt modify on `state.branches[index]`: checkpointed AI edit + `commit
/// --amend`, then descendant restack via the rewrite core. On a pending-only
/// branch this creates the branch's first commit instead of amending.
pub fn modify_branch(
    x: &Exec,
    state: &mut StackState,
    index: usize,
    round: usize,
    opts: &FlowOpts,
) {
    let name = state.branches[index].name.clone();
    if x.current_branch() != name {
        x.git(&["checkout", &name]);
    }

    let path = format!("{name}_amend_ai.txt");
    let line = format!("{} amend ai line r{}", name, round + 1);
    let branch = &mut state.branches[index];
    let file = if let Some(file) = branch.files.iter_mut().find(|file| file.path == path) {
        file
    } else {
        branch.files.push(AiFileState {
            path: path.clone(),
            committed_ai_lines: Vec::new(),
            uncommitted_ai_lines: Vec::new(),
        });
        branch.files.last_mut().unwrap()
    };
    file.committed_ai_lines.push(line);
    let content = file.committed_ai_lines.clone();
    write_ai_file(x.repo(), &path, &content);
    x.git(&["add", "--", &path]);
    x.git(&["stash", "create"]);

    let old_tip = state.branches[index].tip_sha.clone();
    let parent_tip = if index == 0 {
        state.stack_base_sha.clone()
    } else {
        state.branches[index - 1].tip_sha.clone()
    };
    if old_tip == parent_tip {
        // Pending-only branch: gt modify creates its first commit.
        x.git(&[
            "commit",
            "-q",
            "-m",
            &format!("gt modify {name} r{}", round + 1),
        ]);
    } else {
        x.git(&["commit", "--amend", "--no-edit", "-q"]);
    }
    let new_tip = x.rev_parse("HEAD");
    state.branches[index].tip_sha = new_tip.clone();
    state.branches[index].ai_commit_sha = Some(new_tip.clone());

    let mut batch = RefBatch::default();
    restack_range(x, state, index + 1, &old_tip, &new_tip, opts, &mut batch);
    batch.flush(x);
}

/// gt submit: restack-if-needed, then per-branch
/// `push origin --progress --no-verify --atomic --force-with-lease`.
pub fn submit(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    let trunk_sha = x.rev_parse(&branch_ref(TRUNK));
    if state.stack_base_sha != trunk_sha {
        restack(x, state, opts);
    }
    for index in 0..state.branches.len() {
        let name = state.branches[index].name.clone();
        x.git(&[
            "push",
            "origin",
            "--progress",
            "--no-verify",
            "--atomic",
            "--force-with-lease",
            &format!("{0}:{0}", branch_ref(&name)),
        ]);
    }
}

/// gt undo of a completed restack: run the restack, then restore every moved
/// branch to its pre-restack tip — `reset -q --keep` backward for the
/// checked-out branch (#1978/#1983 shape), `update-ref` for the rest.
pub fn undo_restack(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    let pre: Vec<(String, Option<String>)> = state
        .branches
        .iter()
        .map(|branch| (branch.tip_sha.clone(), branch.ai_commit_sha.clone()))
        .collect();
    let pre_base = state.stack_base_sha.clone();

    restack(x, state, opts);

    let current = x.current_branch();
    for (index, (old_tip, old_ai)) in pre.into_iter().enumerate() {
        if state.branches[index].tip_sha == old_tip {
            continue;
        }
        let name = state.branches[index].name.clone();
        let restored = if name == current {
            checked_out_move(x, &old_tip)
        } else {
            x.git(&["update-ref", &branch_ref(&name), &old_tip]);
            true
        };
        if restored {
            state.branches[index].tip_sha = old_tip;
            state.branches[index].ai_commit_sha = old_ai;
        }
    }
    state.stack_base_sha = pre_base;
}

/// gt move --onto: rewrite core applied with the side branch (non-trunk) as
/// the new parent; descendants restack via the same cascade.
pub fn move_onto(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    let onto = state
        .side_branch
        .as_ref()
        .expect("MOVE_ONTO scenarios build a side branch")
        .tip_sha
        .clone();
    prologue(x);
    let mut batch = RefBatch::default();
    let old_base = state.stack_base_sha.clone();
    restack_range(x, state, 0, &old_base, &onto, opts, &mut batch);
    batch.flush(x);
    stash_snapshot(x);
}

/// gt checkout/track/trunk/info control group: reads, metadata refs, and a
/// checkout round-trip. Must be attribution-inert.
pub fn housekeeping(x: &Exec, state: &StackState) {
    x.git(&["branch", "--show-current"]);
    x.git(&["rev-parse", "HEAD"]);
    x.git(&["log", "-1", "--format=%H"]);
    for branch in &state.branches {
        write_metadata_ref(x, &branch.name);
    }
    let current = x.current_branch();
    x.git(&["checkout", TRUNK]);
    x.git(&["checkout", &current]);
}

/// SYNC_RESTACK-style core executed from a linked worktree that has trunk
/// checked out (the under-test branch owns the main worktree, so its ref is
/// moved from the linked worktree — per-worktree HEAD reflog territory).
pub fn worktree_restack(x: &Exec, state: &mut StackState, opts: &FlowOpts) {
    let worktree_path = std::env::temp_dir().join(format!(
        "gt-sim-wt-{}-{}",
        std::process::id(),
        state.scenario_id.to_lowercase().replace('_', "-")
    ));
    let worktree_str = worktree_path.to_str().expect("worktree path utf-8");
    x.git(&["worktree", "add", worktree_str, TRUNK]);

    let wx = x.at_cwd(worktree_path.clone());
    restack(&wx, state, opts);

    // Let the daemon process the worktree's commands (per-worktree reflogs)
    // before the worktree disappears.
    x.sync_barrier();
    x.git(&["worktree", "remove", "--force", worktree_str]);
}

/// PARTIAL_STAGE: seed a working tree mixing staged AI edits, unstaged AI
/// edits, and an untracked file, then run the sync core for the trunk state.
pub fn partial_stage(x: &Exec, state: &mut StackState, scenario: &Scenario) {
    let under_test = state.under_test;
    let name = state.branches[under_test].name.clone();

    // Staged AI file.
    let staged_path = format!("{name}_staged_ai.txt");
    let staged_lines = ai_lines(&format!("{name} staged"), 3);
    write_ai_file(x.repo(), &staged_path, &staged_lines);
    x.git(&["add", "--", &staged_path]);
    state.branches[under_test].files.push(AiFileState {
        path: staged_path,
        committed_ai_lines: Vec::new(),
        uncommitted_ai_lines: staged_lines,
    });

    // Unstaged AI edit to a tracked AI file (mixed scenarios have one).
    if let Some(file) = state.branches[under_test]
        .files
        .iter_mut()
        .find(|file| !file.committed_ai_lines.is_empty())
    {
        let line = format!("{name} unstaged ai line");
        let mut full = file.committed_ai_lines.clone();
        full.extend(file.uncommitted_ai_lines.iter().cloned());
        full.push(line.clone());
        file.uncommitted_ai_lines.push(line);
        write_ai_file(x.repo(), &file.path.clone(), &full);
    }

    // Untracked, unattributed scratch file (ls-files --others fodder).
    write_file(x.repo(), "scratch_untracked.txt", "scratch\n");

    match scenario.trunk_state() {
        super::scenario::TrunkState::Ff => sync_ff(x, state),
        _ => sync_restack(x, state, &FlowOpts::default()),
    }
}

/// LIFECYCLE composite: create -> modify -> sync(restack) -> submit.
pub fn lifecycle(x: &Exec, state: &mut StackState, round: usize, opts: &FlowOpts) {
    create(x, state, round);
    let created = state.branches.len() - 1;
    modify_branch(x, state, created, round, opts);
    sync_restack(x, state, opts);
    submit(x, state, opts);
}

// ---------------------------------------------------------------------------
// Scenario driver
// ---------------------------------------------------------------------------

/// Per-scenario wall-clock budget; a hung daemon costs one scenario (reported
/// as a TIMEOUT violation), not the whole bucket.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(180);

/// Full pipeline for one scenario: fresh repo + daemon, stack build, workflow
/// replay (with blind recovery when applicable), attribution assertions.
/// Returns the collected violations (empty = pass). Bounded by
/// [`SCENARIO_DEADLINE`]; on timeout the scenario thread is abandoned and a
/// single "TIMEOUT" violation is returned. Scenario panics propagate to the
/// caller unchanged.
pub fn run_scenario(scenario: &Scenario) -> Vec<String> {
    // Warm the once-per-process git-ai binary build before the clock starts,
    // so the deadline and the timing line measure the scenario itself.
    crate::repos::test_repo::get_binary_path();
    let started = std::time::Instant::now();
    let (sender, receiver) = mpsc::channel();
    let scenario_for_thread = scenario.clone();
    let handle = thread::Builder::new()
        .name(format!("gt-sim-{}", scenario.id))
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_scenario_inner(&scenario_for_thread)
            }));
            let _ = sender.send(result);
        })
        .expect("spawn scenario thread");

    let violations = match receiver.recv_timeout(SCENARIO_DEADLINE) {
        Ok(Ok(violations)) => {
            let _ = handle.join();
            violations
        }
        Ok(Err(panic_payload)) => {
            let _ = handle.join();
            std::panic::resume_unwind(panic_payload)
        }
        Err(_) => vec![format!(
            "{}: TIMEOUT — scenario exceeded {}s wall-clock (daemon likely hung); thread abandoned",
            scenario.id,
            SCENARIO_DEADLINE.as_secs()
        )],
    };
    eprintln!(
        "[gt-sim] {} finished in {:.1}s ({} violations)",
        scenario.id,
        started.elapsed().as_secs_f64(),
        violations.len()
    );
    violations
}

fn run_scenario_inner(scenario: &Scenario) -> Vec<String> {
    let mut repo = TestRepo::new_with_daemon_scope(match scenario.observation_mode() {
        // Blind scenarios restart the daemon mid-test, which requires a
        // dedicated daemon; traced scenarios share the pool daemon.
        Observation::Blind => DaemonTestScope::Dedicated,
        Observation::Traced => DaemonTestScope::Shared,
    });
    let mut state = stackbuilder::build(&repo, scenario);
    run_workflow(&mut repo, &mut state, scenario);
    assertions::assert_attribution(&repo, &state)
}

/// Run the scenario's workflow (honoring `repeat`), then the blind-recovery
/// step for `observation = blind`. A sync barrier closes every round so
/// back-to-back rounds (and the daemon restart below) start from a fully
/// processed daemon state.
pub fn run_workflow(repo: &mut TestRepo, state: &mut StackState, scenario: &Scenario) {
    let rounds = scenario.repeat_mode().rounds();
    for round in 0..rounds {
        let x = Exec::for_scenario(repo, scenario);
        dispatch_family(&x, state, scenario, round);
        // End-of-round barrier; also settles pending mock_ai checkpoint
        // completions issued during blind rounds.
        repo.sync_daemon_force();
    }
    if scenario.observation_mode() == Observation::Blind {
        recover_blind(repo);
    }
}

fn dispatch_family(x: &Exec, state: &mut StackState, scenario: &Scenario, round: usize) {
    let opts = FlowOpts::for_scenario(scenario);
    match scenario.family.as_str() {
        "SYNC_FF" => {
            // With a clean tree the user can sit on trunk, exercising the
            // forward reset --keep on a checked-out trunk (#1976).
            let clean = matches!(
                scenario.attribution_kind(),
                Attribution::Committed | Attribution::Multifile
            );
            if clean {
                x.git(&["checkout", TRUNK]);
            }
            sync_ff(x, state);
            if clean {
                let name = state.under_test_branch().name.clone();
                x.git(&["checkout", &name]);
            }
        }
        "SYNC_RESTACK" | "NESTED_CONFLICT" | "REFSTDIN" | "FLAVOR_17X" => {
            sync_restack(x, state, &opts)
        }
        "RESTACK" | "RENAME" | "MERGE_IN_STACK" | "CONFLICT_CONTINUE" | "CONFLICT_ABORT" => {
            restack(x, state, &opts)
        }
        "CREATE" => create(x, state, round),
        "MODIFY" => modify_branch(x, state, state.under_test, round, &opts),
        "SUBMIT" => submit(x, state, &opts),
        "UNDO" => undo_restack(x, state, &opts),
        "MOVE_ONTO" => move_onto(x, state, &opts),
        "HOUSEKEEPING" => housekeeping(x, state),
        "WORKTREE" => worktree_restack(x, state, &opts),
        "PARTIAL_STAGE" => partial_stage(x, state, scenario),
        "LIFECYCLE" => lifecycle(x, state, round, &opts),
        other => panic!("{}: no gt_sim workflow for family {}", scenario.id, other),
    }
}

/// Blind recovery per PLAN: restart the daemon, then one traced no-op commit
/// so the reflog cursor reconciles the unobserved ref moves.
fn recover_blind(repo: &mut TestRepo) {
    // Barrier before restart: the restart asserts no pending daemon work.
    repo.sync_daemon_force();
    repo.restart_dedicated_daemon_for_test();
    repo.git(&["commit", "--allow-empty", "-m", "poke"])
        .expect("blind-recovery poke commit should succeed");
    repo.sync_daemon_force();
}
