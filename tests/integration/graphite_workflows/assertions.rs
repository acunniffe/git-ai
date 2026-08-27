//! Post-workflow invariant checker. Every scenario writes files whose content
//! is 100% AI-authored, so the invariant is: after the workflow, `git-ai
//! blame` must attribute every committed line of every AI file to the AI, and
//! every surviving AI commit must carry an authorship note with non-empty
//! attestations.
//!
//! Nothing here panics on an attribution mismatch — violations are collected
//! as descriptions (prefixed with the scenario ID) so family test files can
//! aggregate them per bucket. Harness bugs (e.g. a checkout that cannot run)
//! are also reported as violations rather than panics, keeping one scenario's
//! breakage from hiding another's result.

use super::stackbuilder::{BranchState, StackState};
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

/// Expected committed lines for one file, checked out on `branch`.
struct FileExpectation {
    branch: String,
    path: String,
    lines: Vec<String>,
}

/// A commit that must carry an authorship note with non-empty attestations.
struct NoteExpectation {
    branch: String,
    commit: String,
}

/// Check the final state against `state`'s expectations; returns one
/// human-readable description per violation (empty = pass).
pub fn assert_attribution(repo: &TestRepo, state: &StackState) -> Vec<String> {
    let sid = &state.scenario_id;
    let mut violations = Vec::new();
    // Assertion-phase barrier: everything the workflow issued must be
    // processed before we read blame/notes. (git-ai blame and `git notes`
    // reads below also pre-sync on their own, so no further explicit syncs
    // are needed in this phase.)
    repo.sync_daemon_force();

    // 1. Commit pending (working-log-only) AI edits with a traced commit so
    //    the invariant reduces to committed-line blame everywhere. The pending
    //    edits live in the working tree of whichever branch is checked out.
    let pending = commit_pending_edits(repo, state, &mut violations);

    // 2. Build per-branch expectations, folding pending lines into the branch
    //    that received the assertion commit.
    let (files, notes) = build_expectations(state, &pending);

    // 3. Verify, branch by branch (one traced checkout per branch).
    let mut all_branches: Vec<&BranchState> = state.branches.iter().collect();
    if let Some(side) = &state.side_branch {
        all_branches.push(side);
    }
    for branch in all_branches {
        if let Err(error) = repo.git(&["checkout", &branch.name]) {
            violations.push(format!(
                "{sid}: checkout of branch {} failed: {error}",
                branch.name
            ));
            continue;
        }
        for expectation in files.iter().filter(|f| f.branch == branch.name) {
            check_file(repo, sid, expectation, &mut violations);
        }
        for expectation in notes.iter().filter(|n| n.branch == branch.name) {
            check_note(repo, sid, expectation, &mut violations);
        }
    }

    violations
}

/// Branch that received the pending-edit assertion commit, plus its sha.
struct PendingCommit {
    branch: String,
    commit: String,
}

fn commit_pending_edits(
    repo: &TestRepo,
    state: &StackState,
    violations: &mut Vec<String>,
) -> Option<PendingCommit> {
    let has_pending = state.branches.iter().any(|branch| {
        branch
            .files
            .iter()
            .any(|f| !f.uncommitted_ai_lines.is_empty())
    });
    if !has_pending {
        return None;
    }

    let branch = repo.current_branch();
    match repo.stage_all_and_commit("gt-sim: commit pending ai edits for assertion") {
        Ok(commit) => Some(PendingCommit {
            branch,
            commit: commit.commit_sha,
        }),
        Err(error) => {
            violations.push(format!(
                "{}: committing pending AI edits for assertion failed: {}",
                state.scenario_id, error
            ));
            None
        }
    }
}

fn build_expectations(
    state: &StackState,
    pending: &Option<PendingCommit>,
) -> (Vec<FileExpectation>, Vec<NoteExpectation>) {
    let mut files = Vec::new();
    let mut notes = Vec::new();

    let mut all_branches: Vec<&BranchState> = state.branches.iter().collect();
    if let Some(side) = &state.side_branch {
        all_branches.push(side);
    }

    for branch in &all_branches {
        for file in &branch.files {
            if !file.committed_ai_lines.is_empty() {
                files.push(FileExpectation {
                    branch: branch.name.clone(),
                    path: file.path.clone(),
                    lines: file.committed_ai_lines.clone(),
                });
            }
        }
        if let Some(commit) = &branch.ai_commit_sha {
            notes.push(NoteExpectation {
                branch: branch.name.clone(),
                commit: commit.clone(),
            });
        }
    }

    // Pending lines were committed on `pending.branch` (typically the branch
    // checked out at workflow end); expect them there, appended after the
    // file's committed lines.
    if let Some(pending) = pending {
        for branch in &all_branches {
            for file in &branch.files {
                if file.uncommitted_ai_lines.is_empty() {
                    continue;
                }
                let mut lines = file.committed_ai_lines.clone();
                lines.extend(file.uncommitted_ai_lines.iter().cloned());
                if let Some(existing) = files
                    .iter_mut()
                    .find(|f| f.branch == pending.branch && f.path == file.path)
                {
                    existing.lines = lines;
                } else {
                    files.push(FileExpectation {
                        branch: pending.branch.clone(),
                        path: file.path.clone(),
                        lines,
                    });
                }
            }
        }
        notes.push(NoteExpectation {
            branch: pending.branch.clone(),
            commit: pending.commit.clone(),
        });
    }

    (files, notes)
}

fn check_file(
    repo: &TestRepo,
    sid: &str,
    expectation: &FileExpectation,
    violations: &mut Vec<String>,
) {
    let FileExpectation {
        branch,
        path,
        lines: expected,
    } = expectation;
    let blame = match repo.git_ai(&["blame", path]) {
        Ok(blame) => blame,
        Err(error) => {
            violations.push(format!(
                "{sid}: [{branch}] git-ai blame {path} failed: {error}"
            ));
            return;
        }
    };

    // Committed lines only ("Not Committed Yet" filtered), mirroring
    // TestFile::assert_committed_lines without the panics.
    let committed: Vec<(String, String)> = blame
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_blame_line)
        .filter(|(author, _)| author != "Not Committed Yet")
        .collect();

    if committed.len() != expected.len() {
        violations.push(format!(
            "{sid}: [{branch}] {path}: {} committed lines, expected {}\nblame:\n{}",
            committed.len(),
            expected.len(),
            blame.trim_end()
        ));
        return;
    }

    for (index, ((author, content), expected_line)) in
        committed.iter().zip(expected.iter()).enumerate()
    {
        let line_number = index + 1;
        if content.trim() != expected_line.trim() {
            violations.push(format!(
                "{sid}: [{branch}] {path} line {line_number}: content {:?}, expected {:?}",
                content, expected_line
            ));
        } else if !is_ai_author(author) {
            violations.push(format!(
                "{sid}: [{branch}] {path} line {line_number} ({:?}) blamed {:?}, expected ai",
                content, author
            ));
        }
    }
}

fn check_note(
    repo: &TestRepo,
    sid: &str,
    expectation: &NoteExpectation,
    violations: &mut Vec<String>,
) {
    let NoteExpectation { branch, commit } = expectation;
    let Some(note) = repo.read_authorship_note(commit) else {
        violations.push(format!(
            "{sid}: [{branch}] commit {commit} has no authorship note"
        ));
        return;
    };
    match AuthorshipLog::deserialize_from_string(&note) {
        Err(error) => violations.push(format!(
            "{sid}: [{branch}] commit {commit} authorship note failed to parse: {error}"
        )),
        Ok(log) => {
            if !log
                .attestations
                .iter()
                .any(|attestation| !attestation.entries.is_empty())
            {
                violations.push(format!(
                    "{sid}: [{branch}] commit {commit} authorship note has empty attestations"
                ));
            }
        }
    }
}

/// Parse `sha (author date line) content` — the same format
/// `TestFile::parse_blame_line` handles (reimplemented here because TestFile
/// re-runs blame on construction and panics on mismatch).
fn parse_blame_line(line: &str) -> (String, String) {
    if let Some(start_paren) = line.find('(')
        && let Some(end_paren) = line.find(')')
    {
        let author_section = &line[start_paren + 1..end_paren];
        let content = line[end_paren + 1..].trim();
        let author = author_section
            .split_whitespace()
            .take_while(|part| !part.chars().next().unwrap_or('a').is_ascii_digit())
            .collect::<Vec<_>>()
            .join(" ");
        return (author, content.to_string());
    }
    ("unknown".to_string(), line.to_string())
}

/// The harness authors all AI content as `mock_ai`.
fn is_ai_author(author: &str) -> bool {
    author.to_lowercase().contains("mock_ai")
}
