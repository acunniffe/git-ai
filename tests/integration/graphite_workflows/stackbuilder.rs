//! Builds the pre-workflow repository state for a scenario: trunk `main` with
//! a raw base commit, the requested stack shape of 100%-AI branches, an
//! optional bare `origin` remote, and the trunk-divergence fuel.
//!
//! Contract: the caller controls the daemon lifecycle and must hand this
//! module a `TestRepo` whose daemon is already running — the committed AI work
//! is created with `mock_ai` checkpoints plus traced commits so authorship
//! notes exist before the workflow under test runs.

use super::scenario::{Attribution, Scenario, StackShape, TrunkState};
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use std::fs;

/// Trunk branch name used by every scenario.
pub const TRUNK: &str = "main";

/// Env that disables trace2 so raw setup/rewrite commands stay invisible to
/// the daemon (same shape as `cold_trace2_repo.rs`).
pub const TRACE2_DISABLED_ENV: [(&str, &str); 3] = [
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

const TRUNK_ADVANCE_TEMP_BRANCH: &str = "gt-sim-trunk-advance";

/// One AI-authored file on a branch, with the line-level expectations the
/// post-workflow assertions must reproduce via `git-ai blame`.
#[derive(Debug, Clone)]
pub struct AiFileState {
    pub path: String,
    /// Lines committed (with a mock_ai checkpoint) before the workflow.
    pub committed_ai_lines: Vec<String>,
    /// Checkpointed lines still in the working log; the assertion phase
    /// commits them (traced) and then requires AI attribution.
    pub uncommitted_ai_lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BranchState {
    pub name: String,
    pub files: Vec<AiFileState>,
    /// Current tip; workflows keep this up to date as refs move.
    pub tip_sha: String,
    /// The commit whose authorship note must survive; workflows retarget this
    /// when the commit is rewritten. `None` for pending-only (empty) branches.
    pub ai_commit_sha: Option<String>,
}

#[derive(Debug)]
pub struct StackState {
    pub scenario_id: String,
    /// Trunk commit the stack forked from.
    pub fork_sha: String,
    /// Trunk commit the stack is currently based on (updated by restacks).
    pub stack_base_sha: String,
    /// Local `main` tip.
    pub local_trunk_sha: String,
    /// `origin/main` tip when the scenario has a remote whose trunk advanced.
    pub remote_trunk_sha: Option<String>,
    /// Stack branches, bottom-up (`b1`, `b2`, `b3`).
    pub branches: Vec<BranchState>,
    /// Index into `branches` of the change-under-test branch.
    pub under_test: usize,
    /// Non-stack branch off trunk used as the `gt move --onto` target.
    pub side_branch: Option<BranchState>,
    /// Bare `origin`; kept alive here so its temp dir outlives the scenario.
    pub remote: Option<TestRepo>,
}

impl StackState {
    pub fn under_test_branch(&self) -> &BranchState {
        &self.branches[self.under_test]
    }
}

// ---------------------------------------------------------------------------
// Shared low-level helpers (also used by gt_sim / assertions)
// ---------------------------------------------------------------------------

pub fn raw_git(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &TRACE2_DISABLED_ENV)
        .unwrap_or_else(|error| panic!("raw trace-disabled git {:?} failed: {}", args, error))
}

pub fn raw_head(repo: &TestRepo) -> String {
    raw_git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

pub fn write_file(repo: &TestRepo, path: &str, content: &str) {
    let full_path = repo.path().join(path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full_path, content).unwrap();
}

pub fn ai_lines(prefix: &str, count: usize) -> Vec<String> {
    (1..=count)
        .map(|index| format!("{prefix} ai line {index}"))
        .collect()
}

/// Write `lines` to `path` and record them as AI via a `mock_ai` checkpoint.
pub fn write_ai_file(repo: &TestRepo, path: &str, lines: &[String]) {
    write_file(repo, path, &(lines.join("\n") + "\n"));
    repo.git_ai(&["checkpoint", "mock_ai", path])
        .unwrap_or_else(|error| panic!("mock_ai checkpoint for {path} failed: {error}"));
}

fn traced_git(repo: &TestRepo, args: &[&str]) {
    repo.git(args)
        .unwrap_or_else(|error| panic!("traced git {:?} failed: {}", args, error));
}

fn traced_commit_all(repo: &TestRepo, message: &str) -> String {
    repo.stage_all_and_commit(message)
        .unwrap_or_else(|error| panic!("commit {message:?} failed: {error}"))
        .commit_sha
}

// ---------------------------------------------------------------------------
// Family knobs
// ---------------------------------------------------------------------------

/// Families whose workflow fetches from / pushes to `origin`.
fn needs_remote(family: &str) -> bool {
    matches!(
        family,
        "SYNC_FF"
            | "SYNC_RESTACK"
            | "SUBMIT"
            | "LIFECYCLE"
            | "NESTED_CONFLICT"
            | "PARTIAL_STAGE"
            | "FLAVOR_17X"
            | "REFSTDIN"
    )
}

/// Whether trunk gains commits after the fork. `ff` trunks only advance for
/// families whose whole point is the trunk fast-forward move.
fn trunk_advances(scenario: &Scenario) -> bool {
    match scenario.trunk_state() {
        TrunkState::Diverged | TrunkState::Overlap => true,
        TrunkState::Ff => matches!(scenario.family.as_str(), "SYNC_FF" | "PARTIAL_STAGE"),
    }
}

/// Sync-style families see the advance on `origin/main` (local `main` stays at
/// the fork so the workflow's fetch + trunk move does the forwarding); local
/// restack-style families (including SUBMIT's restack-if-needed) advance local
/// `main` directly.
fn advances_remotely(family: &str) -> bool {
    needs_remote(family) && family != "SUBMIT"
}

fn stack_layout(shape: StackShape) -> (usize, usize) {
    match shape {
        StackShape::Single => (1, 0),
        StackShape::Stack2Bot => (2, 0),
        StackShape::Stack2Top => (2, 1),
        StackShape::Stack3Mid => (3, 1),
        StackShape::Stack3All => (3, 2),
    }
}

/// Path of the pending-only AI file used by the `uncommitted` attribution.
pub fn pending_file_path(branch: &str) -> String {
    format!("{branch}_pending_ai.txt")
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

pub fn build(repo: &TestRepo, scenario: &Scenario) -> StackState {
    let (branch_count, under_test) = stack_layout(scenario.stack_shape());

    // Trunk base commit (raw, human) + deterministic trunk name.
    write_file(repo, "base.txt", "base human line 1\nbase human line 2\n");
    raw_git(repo, &["add", "-A"]);
    raw_git(repo, &["commit", "-m", "raw trunk base"]);
    raw_git(repo, &["branch", "-M", TRUNK]);

    // RENAME seed: an AI file committed on trunk (traced) so the under-test
    // branch commit can express a rename diff against its parent.
    let rename_seed = (scenario.family == "RENAME").then(|| {
        let lines = ai_lines("ren", 3);
        write_ai_file(repo, "ren_ai.txt", &lines);
        traced_commit_all(repo, "ai rename seed on trunk");
        lines
    });

    let fork_sha = raw_head(repo);

    let remote = needs_remote(&scenario.family).then(|| {
        let remote = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
        remote
            .git_og(&["symbolic-ref", "HEAD", &format!("refs/heads/{TRUNK}")])
            .expect("remote default branch");
        raw_git(
            repo,
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        raw_git(repo, &["push", "-u", "origin", &format!("{TRUNK}:{TRUNK}")]);
        remote
    });

    // Stack branches, bottom-up. Every branch gets a committed 100%-AI file;
    // the attribution dimension only alters the change-under-test branch.
    let mut branches = Vec::with_capacity(branch_count);
    for index in 0..branch_count {
        let name = format!("b{}", index + 1);
        traced_git(repo, &["checkout", "-b", &name]);
        branches.push(build_branch(
            repo,
            scenario,
            &name,
            index == under_test,
            &rename_seed,
        ));
    }

    // Side branch off trunk: the non-trunk `gt move --onto` target.
    let side_branch = (scenario.family == "MOVE_ONTO").then(|| {
        traced_git(repo, &["checkout", TRUNK]);
        traced_git(repo, &["checkout", "-b", "side"]);
        let lines = ai_lines("side", 3);
        write_ai_file(repo, "side_ai.txt", &lines);
        let commit = traced_commit_all(repo, "ai work on side");
        BranchState {
            name: "side".to_string(),
            files: vec![AiFileState {
                path: "side_ai.txt".to_string(),
                committed_ai_lines: lines,
                uncommitted_ai_lines: Vec::new(),
            }],
            tip_sha: commit.clone(),
            ai_commit_sha: Some(commit),
        }
    });

    let under_test_name = branches[under_test].name.clone();
    traced_git(repo, &["checkout", &under_test_name]);

    // Trunk divergence / conflict fuel (raw commits, per the trunk dimension).
    let mut local_trunk_sha = fork_sha.clone();
    let mut remote_trunk_sha = remote.as_ref().map(|_| fork_sha.clone());
    if scenario.family == "MERGE_IN_STACK" {
        // Advance trunk, pull-merge it into the under-test branch, advance
        // again — leaves a merge commit inside the stack for the restack core.
        advance_trunk_locally(
            repo,
            &under_test_name,
            &[(
                "main_only_1.txt".to_string(),
                "main only line 1\n".to_string(),
            )],
        );
        traced_git(repo, &["merge", TRUNK, "-m", "merge trunk into stack"]);
        branches[under_test].tip_sha = raw_head(repo);
        local_trunk_sha = advance_trunk_locally(
            repo,
            &under_test_name,
            &[(
                "main_only_2.txt".to_string(),
                "main only line 2\n".to_string(),
            )],
        );
    } else if trunk_advances(scenario) {
        let files = trunk_advance_files(scenario, &branches);
        if advances_remotely(&scenario.family) {
            remote_trunk_sha = Some(advance_trunk_remotely(repo, &under_test_name, &files));
        } else {
            local_trunk_sha = advance_trunk_locally(repo, &under_test_name, &files);
        }
    }

    // Working-log-only AI edits on the under-test branch. Applied last so the
    // raw trunk checkout dance above never runs with a dirty tree.
    apply_pending_edits(repo, scenario, &mut branches[under_test]);

    repo.sync_daemon_force();

    StackState {
        scenario_id: scenario.id.clone(),
        fork_sha: fork_sha.clone(),
        stack_base_sha: fork_sha,
        local_trunk_sha,
        remote_trunk_sha,
        branches,
        under_test,
        side_branch,
        remote,
    }
}

fn build_branch(
    repo: &TestRepo,
    scenario: &Scenario,
    name: &str,
    is_under_test: bool,
    rename_seed: &Option<Vec<String>>,
) -> BranchState {
    let attribution = scenario.attribution_kind();

    // `uncommitted` leaves the change-under-test branch without a commit of
    // its own (ref == parent tip): the AI work exists only in the working log.
    // Stack3All keeps its committed commit so all branches carry AI commits.
    if is_under_test
        && attribution == Attribution::Uncommitted
        && scenario.stack_shape() != StackShape::Stack3All
    {
        return BranchState {
            name: name.to_string(),
            files: Vec::new(),
            tip_sha: raw_head(repo),
            ai_commit_sha: None,
        };
    }

    let mut files = Vec::new();
    if is_under_test && scenario.family == "RENAME" {
        let seed = rename_seed.as_ref().expect("RENAME trunk seed");
        repo.git(&["mv", "ren_ai.txt", "ren_ai_renamed.txt"])
            .expect("git mv should succeed");
        let mut lines = seed.clone();
        lines.push(format!("{name} renamed ai line"));
        write_ai_file(repo, "ren_ai_renamed.txt", &lines);
        files.push(AiFileState {
            path: "ren_ai_renamed.txt".to_string(),
            committed_ai_lines: lines,
            uncommitted_ai_lines: Vec::new(),
        });
    } else {
        files.push(committed_ai_file(repo, &format!("{name}_ai.txt"), name));
    }

    if is_under_test && attribution == Attribution::Multifile {
        for extra in 2..=3 {
            let path = format!("{name}_ai_{extra}.txt");
            files.push(committed_ai_file(
                repo,
                &path,
                &format!("{name} extra {extra}"),
            ));
        }
    }

    let commit = traced_commit_all(repo, &format!("ai work on {name}"));
    BranchState {
        name: name.to_string(),
        files,
        tip_sha: commit.clone(),
        ai_commit_sha: Some(commit),
    }
}

fn committed_ai_file(repo: &TestRepo, path: &str, line_prefix: &str) -> AiFileState {
    let lines = ai_lines(line_prefix, 3);
    write_ai_file(repo, path, &lines);
    AiFileState {
        path: path.to_string(),
        committed_ai_lines: lines,
        uncommitted_ai_lines: Vec::new(),
    }
}

/// Contents trunk gains after the fork. `overlap` recreates the bottom
/// branch's AI file with conflicting human content (add/add conflict fuel);
/// NESTED_CONFLICT overlaps the bottom TWO stack levels. `diverged`/`ff`
/// advances touch main-only files.
fn trunk_advance_files(scenario: &Scenario, branches: &[BranchState]) -> Vec<(String, String)> {
    match scenario.trunk_state() {
        TrunkState::Overlap => {
            let overlap_levels = if scenario.family == "NESTED_CONFLICT" {
                2
            } else {
                1
            };
            branches
                .iter()
                .take(overlap_levels)
                .map(|branch| {
                    let path = branch
                        .files
                        .first()
                        .map(|file| file.path.clone())
                        .unwrap_or_else(|| pending_file_path(&branch.name));
                    (
                        path,
                        "trunk conflicting line 1\ntrunk conflicting line 2\n".to_string(),
                    )
                })
                .collect()
        }
        TrunkState::Diverged | TrunkState::Ff => vec![
            (
                "main_only_1.txt".to_string(),
                "main only line 1\n".to_string(),
            ),
            (
                "main_only_2.txt".to_string(),
                "main only line 2\n".to_string(),
            ),
        ],
    }
}

/// Raw human commits directly on local `main` (checkout dance, trace2 off).
fn advance_trunk_locally(
    repo: &TestRepo,
    return_branch: &str,
    files: &[(String, String)],
) -> String {
    raw_git(repo, &["checkout", TRUNK]);
    for (path, content) in files {
        write_file(repo, path, content);
        raw_git(repo, &["add", "-A"]);
        raw_git(
            repo,
            &["commit", "-m", &format!("raw trunk advance {path}")],
        );
    }
    let tip = raw_head(repo);
    raw_git(repo, &["checkout", return_branch]);
    tip
}

/// Raw human commits pushed to `origin/main` via a temp branch, leaving local
/// `main` at the fork — the shape of trunk advancing remotely.
fn advance_trunk_remotely(
    repo: &TestRepo,
    return_branch: &str,
    files: &[(String, String)],
) -> String {
    raw_git(repo, &["checkout", "-b", TRUNK_ADVANCE_TEMP_BRANCH, TRUNK]);
    for (path, content) in files {
        write_file(repo, path, content);
        raw_git(repo, &["add", "-A"]);
        raw_git(
            repo,
            &["commit", "-m", &format!("raw trunk advance {path}")],
        );
    }
    let tip = raw_head(repo);
    raw_git(
        repo,
        &[
            "push",
            "origin",
            &format!("{TRUNK_ADVANCE_TEMP_BRANCH}:{TRUNK}"),
        ],
    );
    raw_git(repo, &["checkout", return_branch]);
    raw_git(repo, &["branch", "-D", TRUNK_ADVANCE_TEMP_BRANCH]);
    tip
}

fn apply_pending_edits(repo: &TestRepo, scenario: &Scenario, branch: &mut BranchState) {
    match scenario.attribution_kind() {
        Attribution::Uncommitted => {
            let path = pending_file_path(&branch.name);
            let lines = ai_lines(&format!("{} pending", branch.name), 3);
            write_ai_file(repo, &path, &lines);
            branch.files.push(AiFileState {
                path,
                committed_ai_lines: Vec::new(),
                uncommitted_ai_lines: lines,
            });
        }
        Attribution::Mixed => {
            let file = branch
                .files
                .first_mut()
                .expect("mixed attribution requires a committed AI file");
            let pending = vec![
                format!("{} pending ai line 1", branch.name),
                format!("{} pending ai line 2", branch.name),
            ];
            let mut full = file.committed_ai_lines.clone();
            full.extend(pending.iter().cloned());
            write_ai_file(repo, &file.path.clone(), &full);
            file.uncommitted_ai_lines = pending;
        }
        Attribution::Committed | Attribution::Multifile => {}
    }
}
