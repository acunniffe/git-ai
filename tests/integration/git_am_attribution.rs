use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{
    checkpoint_codex_bash_hook, setup_codex_bash_repo, setup_codex_bash_repo_with_db_path,
};
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::git::find_repository_in_path;
#[cfg(unix)]
use serde_json::json;
use std::fs;
use std::thread;
use std::time::Duration;

fn checkpoint_git_am_bash_hook(
    repo: &TestRepo,
    transcript_path: &std::path::Path,
    event: &str,
    command: &str,
) {
    checkpoint_codex_bash_hook(
        repo,
        transcript_path,
        "git-am-bash-session",
        "git-am-bash-tool",
        event,
        command,
    );
}

fn assert_codex_bash_session(repo: &TestRepo, commit_sha: &str) {
    let note = repo
        .read_authorship_note(commit_sha)
        .unwrap_or_else(|| panic!("commit {commit_sha} should have an authorship note"));
    let log = AuthorshipLog::deserialize_from_string(&note)
        .unwrap_or_else(|error| panic!("commit {commit_sha} note should deserialize: {error}"));
    assert!(
        log.metadata.sessions.values().any(|session| {
            session.agent_id.tool == "codex" && session.agent_id.id == "git-am-bash-session"
        }),
        "commit {commit_sha} should be attributed to the active Codex Bash session; sessions={:?}",
        log.metadata.sessions
    );
}

/// Regression coverage for an agent invoking `git am` from its Bash tool.
///
/// `git am` creates the commit itself, so there is no later `git commit` for
/// git-ai to observe. The applied lines must still be attributed to the active
/// AI session rather than falling through to unknown/human attribution.
#[test]
fn test_git_am_inside_codex_bash_attributes_applied_commit_to_ai() {
    let source = TestRepo::new();
    fs::write(source.path().join("from-am.txt"), "created by patch\n")
        .expect("source file should be writable");
    source
        .stage_all_and_commit("Create patch content")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("incoming.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Initial target commit");
    let command = format!("git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);

    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("git am should succeed");

    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);
    target.sync_daemon();

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &applied_commit);

    let mut file = target.filename("from-am.txt");
    file.assert_committed_lines(lines!["created by patch".ai()]);
    let mut base = target.filename("base.txt");
    base.assert_committed_lines(lines!["base".unattributed_human()]);
}

/// A mailbox can contain an arbitrary commit series. Every destination commit
/// needs its own note; attributing only the final tip would leave lines last
/// touched by an earlier commit unknown in blame.
#[test]
fn test_git_am_series_inside_codex_bash_writes_ai_note_for_every_commit() {
    let source = TestRepo::new();
    fs::write(source.path().join("first.txt"), "first patch line\n")
        .expect("first source file should be writable");
    source
        .stage_all_and_commit("First patch")
        .expect("first source commit should succeed");
    fs::write(source.path().join("second.txt"), "second patch line\n")
        .expect("second source file should be writable");
    source
        .stage_all_and_commit("Second patch")
        .expect("second source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("series.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Initial target commit");
    let base_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("target base should resolve")
        .trim()
        .to_string();
    let command = format!("git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("git am series should succeed");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);
    target.sync_daemon();

    let commits = target
        .git(&["rev-list", "--reverse", &format!("{base_commit}..HEAD")])
        .expect("applied commits should be listed")
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 2, "mailbox should apply two commits");
    for commit in &commits {
        assert_codex_bash_session(&target, commit);
    }

    let mut first = target.filename("first.txt");
    first.assert_committed_lines(lines!["first patch line".ai()]);
    let mut second = target.filename("second.txt");
    second.assert_committed_lines(lines!["second patch line".ai()]);
}

/// `git am` is not itself evidence that AI authored a patch. Without an
/// overlapping agent tool call, imported lines must remain unattributed.
#[test]
fn test_git_am_outside_agent_bash_does_not_invent_ai_attribution() {
    let source = TestRepo::new();
    fs::write(
        source.path().join("outside.txt"),
        "mail authored elsewhere\n",
    )
    .expect("source file should be writable");
    source
        .stage_all_and_commit("Patch from outside agent")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("outside.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let target = TestRepo::new();
    fs::write(target.path().join("base.txt"), "existing base\n")
        .expect("target base should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("git am should succeed");

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    let note = target
        .read_authorship_note(&applied_commit)
        .expect("the applied commit should still receive an unknown-attribution note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("note should deserialize");
    assert!(
        log.metadata.sessions.is_empty(),
        "plain git am must not invent an AI session"
    );
    let mut file = target.filename("outside.txt");
    file.assert_committed_lines(lines!["mail authored elsewhere".unattributed_human()]);
}

/// `--3way` is the dominant non-default production shape. Its successful
/// fallback still creates ordinary `am:` commits and must use the same
/// attribution path.
#[test]
fn test_git_am_three_way_keep_cr_inside_codex_bash_attributes_applied_commit() {
    let source = TestRepo::new();
    fs::write(source.path().join("three-way.txt"), "three way line\r\n")
        .expect("source file should be writable");
    source
        .stage_all_and_commit("Three-way patch")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("three-way.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Initial target commit");
    let command = format!("git am --3way --keep-cr {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    target
        .git(&["am", "--3way", "--keep-cr", patch_path.to_str().unwrap()])
        .expect("git am --3way --keep-cr should succeed");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &applied_commit);
    let mut file = target.filename("three-way.txt");
    file.assert_committed_lines(lines!["three way line".ai()]);
}

/// `git am` can commit an initial prefix and then exit nonzero on a later mail.
/// The successful prefix is durable history and must be finalized even though
/// the root command failed.
#[test]
fn test_git_am_nonzero_after_successful_prefix_attributes_prefix_commit() {
    let source = TestRepo::new();
    fs::write(source.path().join("conflict.txt"), "base value\n")
        .expect("source base should be writable");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base commit should succeed")
        .commit_sha;
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(source.path().join("prefix.txt"), "successful prefix\n")
        .expect("prefix source file should be writable");
    source
        .stage_all_and_commit("Successful prefix")
        .expect("prefix source commit should succeed");
    source
        .filename("prefix.txt")
        .assert_committed_lines(lines!["successful prefix".unattributed_human()]);
    fs::write(source.path().join("conflict.txt"), "patch side\n")
        .expect("conflicting source file should be writable");
    source
        .stage_all_and_commit("Conflicting tail")
        .expect("conflicting source commit should succeed");
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["patch side".unattributed_human()]);
    let range = format!("{source_base}..HEAD");
    let patch = source
        .git_og(&["format-patch", "--stdout", &range])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("partial-series.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Harness base");
    fs::write(target.path().join("conflict.txt"), "base value\n")
        .expect("target base should be writable");
    target
        .stage_all_and_commit("Target base")
        .expect("target base commit should succeed");
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(target.path().join("conflict.txt"), "target side\n")
        .expect("target divergence should be writable");
    let head_before_am = target
        .stage_all_and_commit("Target divergence")
        .expect("target divergence commit should succeed")
        .commit_sha;
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);

    let command = format!("git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    let am_result = target.git(&["am", patch_path.to_str().unwrap()]);
    assert!(am_result.is_err(), "the mailbox tail should conflict");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    let commits = target
        .git(&["rev-list", "--reverse", &format!("{head_before_am}..HEAD")])
        .expect("successful prefix should be listed")
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 1, "only the successful prefix should commit");
    assert_codex_bash_session(&target, &commits[0]);
    let mut prefix = target.filename("prefix.txt");
    prefix.assert_committed_lines(lines!["successful prefix".ai()]);
}

/// A continuation can create the resolved commit and then apply the remaining
/// mailbox tail. Both commits belong to the Bash call that performs the
/// resolution/continue phase.
#[test]
fn test_git_am_continue_inside_codex_bash_attributes_resolution_and_tail() {
    let source = TestRepo::new();
    fs::write(source.path().join("conflict.txt"), "base value\n")
        .expect("source base should be writable");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base commit should succeed")
        .commit_sha;
    fs::write(source.path().join("conflict.txt"), "patch side\n")
        .expect("conflicting source file should be writable");
    source
        .stage_all_and_commit("Conflicting first patch")
        .expect("conflicting source commit should succeed");
    fs::write(source.path().join("tail.txt"), "remaining tail\n")
        .expect("tail source file should be writable");
    source
        .stage_all_and_commit("Remaining tail")
        .expect("tail source commit should succeed");
    let range = format!("{source_base}..HEAD");
    let patch = source
        .git_og(&["format-patch", "--stdout", &range])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("continue-series.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Harness base");
    fs::write(target.path().join("conflict.txt"), "base value\n")
        .expect("target base should be writable");
    target
        .stage_all_and_commit("Target base")
        .expect("target base commit should succeed");
    fs::write(target.path().join("conflict.txt"), "target side\n")
        .expect("target divergence should be writable");
    let head_before_am = target
        .stage_all_and_commit("Target divergence")
        .expect("target divergence commit should succeed")
        .commit_sha;
    assert!(
        target.git(&["am", patch_path.to_str().unwrap()]).is_err(),
        "first patch should conflict"
    );

    let command =
        "printf 'resolved by ai\\n' > conflict.txt && git add conflict.txt && git am --continue";
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", command);
    fs::write(target.path().join("conflict.txt"), "resolved by ai\n")
        .expect("resolution should be writable");
    target
        .git(&["add", "conflict.txt"])
        .expect("resolution should stage");
    target
        .git(&["-c", "user.name=Test User", "am", "--continue"])
        .expect("git am --continue should succeed");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", command);

    let commits = target
        .git(&["rev-list", "--reverse", &format!("{head_before_am}..HEAD")])
        .expect("continued commits should be listed")
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        commits.len(),
        2,
        "continue should create resolution and tail"
    );
    for commit in &commits {
        assert_codex_bash_session(&target, commit);
    }
    let mut conflict = target.filename("conflict.txt");
    conflict.assert_committed_lines(lines!["resolved by ai".ai()]);
    let mut tail = target.filename("tail.txt");
    tail.assert_committed_lines(lines!["remaining tail".ai()]);
}

/// File mtimes from the final worktree cannot recover a line introduced by an
/// early patch and deleted by a later one. The exact command-window path must
/// still put an AI attestation in the early commit's note.
#[test]
fn test_git_am_series_attributes_early_file_deleted_before_command_returns() {
    let source = TestRepo::new();
    fs::write(source.path().join("base.txt"), "base\n").expect("source base should write");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base commit should succeed")
        .commit_sha;
    fs::write(source.path().join("ephemeral.txt"), "ephemeral ai line\n")
        .expect("ephemeral file should write");
    source
        .stage_all_and_commit("Add ephemeral file")
        .expect("first source commit should succeed");
    fs::remove_file(source.path().join("ephemeral.txt")).expect("ephemeral file should delete");
    fs::write(source.path().join("survivor.txt"), "surviving ai line\n")
        .expect("survivor file should write");
    source
        .stage_all_and_commit("Delete ephemeral and add survivor")
        .expect("second source commit should succeed");
    let range = format!("{source_base}..HEAD");
    let patch = source
        .git_og(&["format-patch", "--stdout", &range])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("delete-series.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Target base");
    let target_base = target
        .git(&["rev-parse", "HEAD"])
        .expect("target base should resolve")
        .trim()
        .to_string();
    let command = format!("git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("git am should succeed");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    let commits = target
        .git(&["rev-list", "--reverse", &format!("{target_base}..HEAD")])
        .expect("applied commits should be listed")
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 2);
    let early_note = target
        .read_authorship_note(&commits[0])
        .expect("early commit should have a note");
    let early_log =
        AuthorshipLog::deserialize_from_string(&early_note).expect("early note should parse");
    assert!(
        early_log
            .attestations
            .iter()
            .any(|file| { file.file_path == "ephemeral.txt" && !file.entries.is_empty() }),
        "early deleted file should retain an AI attestation in its owning commit"
    );
    assert_codex_bash_session(&target, &commits[0]);
    assert_codex_bash_session(&target, &commits[1]);
    assert!(!target.path().join("ephemeral.txt").exists());
    let mut survivor = target.filename("survivor.txt");
    survivor.assert_committed_lines(lines!["surviving ai line".ai()]);
}

/// Open Bash calls used to get a synthetic three-second end window. A real
/// long-running mailbox command must still correlate when its Git trace starts
/// after that grace period but before PostToolUse.
#[test]
fn test_git_am_after_long_open_bash_window_keeps_ai_attribution() {
    let source = TestRepo::new();
    fs::write(source.path().join("long.txt"), "long running am\n")
        .expect("source file should be writable");
    source
        .stage_all_and_commit("Long patch")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("long.patch");
    fs::write(&patch_path, patch).expect("patch should be writable");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Target base");
    let command = format!("sleep 4 && git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    thread::sleep(Duration::from_millis(3_500));
    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("git am should succeed");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &applied_commit);
    let mut file = target.filename("long.txt");
    file.assert_committed_lines(lines!["long running am".ai()]);
}

/// Abort is a hard history restoration, not another applied patch. It must
/// remove the abandoned tip's working log so later work on the restored base
/// cannot inherit conflict-phase attribution.
#[test]
fn test_git_am_abort_restores_head_and_discards_abandoned_working_log() {
    let source = TestRepo::new();
    fs::write(source.path().join("conflict.txt"), "base value\n")
        .expect("source base should be writable");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base commit should succeed")
        .commit_sha;
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(source.path().join("prefix.txt"), "successful prefix\n")
        .expect("prefix file should write");
    source
        .stage_all_and_commit("Successful prefix")
        .expect("prefix source commit should succeed");
    source
        .filename("prefix.txt")
        .assert_committed_lines(lines!["successful prefix".unattributed_human()]);
    fs::write(source.path().join("conflict.txt"), "patch side\n")
        .expect("conflicting source file should write");
    source
        .stage_all_and_commit("Conflicting tail")
        .expect("conflicting source commit should succeed");
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["patch side".unattributed_human()]);
    let patch = source
        .git_og(&["format-patch", "--stdout", &format!("{source_base}..HEAD")])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("abort-series.patch");
    fs::write(&patch_path, patch).expect("patch should write");

    let target = TestRepo::new();
    fs::write(target.path().join("conflict.txt"), "base value\n")
        .expect("target base should write");
    target
        .stage_all_and_commit("Target base")
        .expect("target base commit should succeed");
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(target.path().join("conflict.txt"), "target side\n")
        .expect("target divergence should write");
    let head_before_am = target
        .stage_all_and_commit("Target divergence")
        .expect("target divergence commit should succeed")
        .commit_sha;
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);
    assert!(
        target.git(&["am", patch_path.to_str().unwrap()]).is_err(),
        "mailbox tail should conflict"
    );
    let abandoned_tip = target
        .git(&["rev-parse", "HEAD"])
        .expect("partial tip should resolve")
        .trim()
        .to_string();
    assert_ne!(abandoned_tip, head_before_am);
    target
        .filename("prefix.txt")
        .assert_committed_lines(lines!["successful prefix".unattributed_human()]);

    fs::write(target.path().join("scratch.txt"), "conflict phase ai\n")
        .expect("scratch file should write");
    target
        .git_ai(&["checkpoint", "mock_ai", "scratch.txt"])
        .expect("conflict-phase checkpoint should succeed");
    target.sync_daemon();
    let repository = find_repository_in_path(target.path().to_str().unwrap())
        .expect("target repository should resolve");
    assert!(repository.storage.has_working_log(&abandoned_tip));

    target
        .git(&["am", "--abort"])
        .expect("git am --abort should succeed");
    target.sync_daemon();
    assert_eq!(
        target
            .git(&["rev-parse", "HEAD"])
            .expect("restored HEAD should resolve")
            .trim(),
        head_before_am
    );
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);
    let repository = find_repository_in_path(target.path().to_str().unwrap())
        .expect("target repository should resolve after abort");
    assert!(
        !repository.storage.has_working_log(&abandoned_tip),
        "abort should delete the abandoned tip's working log"
    );
}

#[test]
fn test_git_am_skip_inside_codex_bash_attributes_remaining_tail_only() {
    let source = TestRepo::new();
    fs::write(source.path().join("conflict.txt"), "base value\n")
        .expect("source base should write");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base should commit")
        .commit_sha;
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(source.path().join("conflict.txt"), "patch side\n")
        .expect("conflicting source should write");
    source
        .stage_all_and_commit("Skipped conflict")
        .expect("conflicting source should commit");
    fs::write(source.path().join("tail.txt"), "tail after skip\n").expect("tail should write");
    source
        .stage_all_and_commit("Tail after skip")
        .expect("tail should commit");
    let patch = source
        .git_og(&["format-patch", "--stdout", &format!("{source_base}..HEAD")])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should create");
    let patch_path = patch_dir.path().join("skip-series.patch");
    fs::write(&patch_path, patch).expect("patch should write");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Harness base");
    fs::write(target.path().join("conflict.txt"), "base value\n")
        .expect("target base should write");
    target
        .stage_all_and_commit("Target base")
        .expect("target base should commit");
    fs::write(target.path().join("conflict.txt"), "target side\n")
        .expect("target divergence should write");
    let head_before_am = target
        .stage_all_and_commit("Target divergence")
        .expect("target divergence should commit")
        .commit_sha;
    assert!(target.git(&["am", patch_path.to_str().unwrap()]).is_err());

    let command = "git am --skip";
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", command);
    target
        .git(&["am", "--skip"])
        .expect("git am --skip should apply the tail");
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", command);

    let commits = target
        .git(&["rev-list", "--reverse", &format!("{head_before_am}..HEAD")])
        .expect("tail commit should list")
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 1);
    assert_codex_bash_session(&target, &commits[0]);
    let mut tail = target.filename("tail.txt");
    tail.assert_committed_lines(lines!["tail after skip".ai()]);
}

#[test]
fn test_git_am_quit_keeps_successful_prefix_and_its_attribution() {
    let source = TestRepo::new();
    fs::write(source.path().join("conflict.txt"), "base value\n")
        .expect("source base should write");
    let source_base = source
        .stage_all_and_commit("Patch base")
        .expect("source base should commit")
        .commit_sha;
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(source.path().join("prefix.txt"), "prefix before quit\n")
        .expect("prefix should write");
    source
        .stage_all_and_commit("Prefix before quit")
        .expect("prefix should commit");
    source
        .filename("prefix.txt")
        .assert_committed_lines(lines!["prefix before quit".unattributed_human()]);
    fs::write(source.path().join("conflict.txt"), "patch side\n").expect("conflict should write");
    source
        .stage_all_and_commit("Conflict before quit")
        .expect("conflict should commit");
    source
        .filename("conflict.txt")
        .assert_committed_lines(lines!["patch side".unattributed_human()]);
    let patch = source
        .git_og(&["format-patch", "--stdout", &format!("{source_base}..HEAD")])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should create");
    let patch_path = patch_dir.path().join("quit-series.patch");
    fs::write(&patch_path, patch).expect("patch should write");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Harness base");
    fs::write(target.path().join("conflict.txt"), "base value\n")
        .expect("target base should write");
    target
        .stage_all_and_commit("Target base")
        .expect("target base should commit");
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["base value".unattributed_human()]);
    fs::write(target.path().join("conflict.txt"), "target side\n")
        .expect("target divergence should write");
    target
        .stage_all_and_commit("Target divergence")
        .expect("target divergence should commit");
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);
    let command = format!("git am {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    assert!(target.git(&["am", patch_path.to_str().unwrap()]).is_err());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);
    let prefix_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("prefix should resolve")
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &prefix_commit);
    target
        .filename("prefix.txt")
        .assert_committed_lines(lines!["prefix before quit".ai()]);
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);
    let prefix_note_before_show = target
        .read_authorship_note(&prefix_commit)
        .expect("prefix note should exist");
    let current_patch = target
        .git(&["am", "--show-current-patch=diff"])
        .expect("show-current-patch should be read-only and succeed");
    assert!(current_patch.contains("diff --git"));
    assert_eq!(
        target
            .git(&["rev-parse", "HEAD"])
            .expect("HEAD should resolve after inspection")
            .trim(),
        prefix_commit
    );
    assert_eq!(
        target.read_authorship_note(&prefix_commit).as_deref(),
        Some(prefix_note_before_show.as_str())
    );

    target
        .git(&["am", "--quit"])
        .expect("git am --quit should succeed");
    assert_eq!(
        target
            .git(&["rev-parse", "HEAD"])
            .expect("HEAD should resolve after quit")
            .trim(),
        prefix_commit
    );
    assert_codex_bash_session(&target, &prefix_commit);
    target
        .filename("prefix.txt")
        .assert_committed_lines(lines!["prefix before quit".ai()]);
    target
        .filename("conflict.txt")
        .assert_committed_lines(lines!["target side".unattributed_human()]);
    assert!(
        target.git(&["am", "--show-current-patch"]).is_err(),
        "quit should remove the am state"
    );
}

#[test]
fn test_git_am_incompatible_failure_inside_bash_does_not_mutate_attribution() {
    let patch_dir = tempfile::tempdir().expect("patch tempdir should create");
    let patch_path = patch_dir.path().join("malformed.patch");
    fs::write(&patch_path, "not an email patch\n").expect("malformed patch should write");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Target base");
    let base_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("target base should resolve")
        .trim()
        .to_string();
    let base_note_before = target
        .read_authorship_note(&base_commit)
        .expect("base should have a note");
    let command = format!("git am --3way --reject {}", patch_path.display());
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    assert!(
        target
            .git(&["am", "--3way", "--reject", patch_path.to_str().unwrap()])
            .is_err(),
        "incompatible options should fail"
    );
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    assert_eq!(
        target
            .git(&["rev-parse", "HEAD"])
            .expect("HEAD should resolve")
            .trim(),
        base_commit
    );
    assert_eq!(
        target.read_authorship_note(&base_commit).as_deref(),
        Some(base_note_before.as_str()),
        "failed am must not rewrite the existing note"
    );
    let mut base = target.filename("base.txt");
    base.assert_committed_lines(lines!["base".unattributed_human()]);
}

#[test]
#[cfg(target_os = "linux")]
fn test_git_am_from_parent_cwd_with_timeout_and_explicit_c_attributes_target_repo() {
    let source = TestRepo::new();
    fs::write(source.path().join("parent-cwd.txt"), "parent cwd am\n")
        .expect("source file should write");
    source
        .stage_all_and_commit("Parent cwd patch")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should create");
    let patch_path = patch_dir.path().join("parent-cwd.patch");
    fs::write(&patch_path, patch).expect("patch should write");

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("Target base");
    let target_root = target.canonical_path();
    let parent_cwd = target_root.parent().unwrap().to_path_buf();
    let repo_name = target_root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let command = format!(
        "timeout 30 git -C {} am {}",
        repo_name,
        patch_path.display()
    );
    let pre_hook_input = json!({
        "session_id": "git-am-bash-session",
        "cwd": parent_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_use_id": "git-am-bash-tool",
        "tool_input": { "command": command },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    target
        .git_ai_from_working_dir(
            &parent_cwd,
            &["checkpoint", "codex", "--hook-input", &pre_hook_input],
        )
        .expect("parent-cwd pre hook should succeed");
    target
        .shell_git_from_working_dir(
            &parent_cwd,
            &format!(
                "timeout 30 {{git}} -C {} am {}",
                repo_name,
                patch_path.display()
            ),
        )
        .expect("explicit -C git am should succeed");
    let post_hook_input = json!({
        "session_id": "git-am-bash-session",
        "cwd": parent_cwd.to_string_lossy().to_string(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_use_id": "git-am-bash-tool",
        "tool_input": { "command": command },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string();
    target
        .git_ai_from_working_dir(
            &parent_cwd,
            &["checkpoint", "codex", "--hook-input", &post_hook_input],
        )
        .expect("parent-cwd post hook should succeed");

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &applied_commit);
    let mut file = target.filename("parent-cwd.txt");
    file.assert_committed_lines(lines!["parent cwd am".ai()]);
}

#[test]
#[cfg(target_os = "linux")]
fn test_git_am_actual_timeout_conditional_wrapper_attributes_target_repo() {
    let source = TestRepo::new();
    fs::write(
        source.path().join("conditional-am.txt"),
        "conditional am AI\n",
    )
    .unwrap();
    source.stage_all_and_commit("conditional patch").unwrap();
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .unwrap();
    let patch_dir = tempfile::tempdir().unwrap();
    let patch_path = patch_dir.path().join("conditional.patch");
    fs::write(&patch_path, patch).unwrap();

    let (_bash_db_dir, target, transcript_path) = setup_codex_bash_repo("base");
    fs::write(target.path().join("ready.marker"), "ready\n").unwrap();
    let command = format!(
        "test -f ready.marker && timeout 30 git am {}",
        patch_path.display()
    );
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PreToolUse", &command);
    target
        .shell_git(&format!(
            "test -f ready.marker && timeout 30 {{git}} am {}",
            patch_path.display()
        ))
        .unwrap();
    checkpoint_git_am_bash_hook(&target, &transcript_path, "PostToolUse", &command);

    let commit = target
        .git(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    assert_codex_bash_session(&target, &commit);
    let mut file = target.filename("conditional-am.txt");
    file.assert_committed_lines(lines!["conditional am AI".ai()]);
}

#[test]
fn test_git_am_does_not_steal_overlapping_bash_call_from_another_repo() {
    let source = TestRepo::new();
    fs::write(
        source.path().join("other-repo.txt"),
        "must remain unknown\n",
    )
    .expect("source file should write");
    source
        .stage_all_and_commit("Other repo patch")
        .expect("source commit should succeed");
    let patch = source
        .git_og(&["format-patch", "--stdout", "--root", "HEAD"])
        .expect("format-patch should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should create");
    let patch_path = patch_dir.path().join("other-repo.patch");
    fs::write(&patch_path, patch).expect("patch should write");

    let (_bash_db_dir, bash_db_value, bash_repo, transcript_path) =
        setup_codex_bash_repo_with_db_path("Bash repo base");
    let env = [(
        "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
        bash_db_value.as_str(),
    )];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("base.txt"), "base\n").expect("target base should write");
    target
        .stage_all_and_commit("Target base")
        .expect("target base should commit");
    let unrelated_command = "git status && echo unrelated";
    checkpoint_git_am_bash_hook(
        &bash_repo,
        &transcript_path,
        "PreToolUse",
        unrelated_command,
    );
    target
        .git(&["am", patch_path.to_str().unwrap()])
        .expect("target git am should succeed");
    checkpoint_git_am_bash_hook(
        &bash_repo,
        &transcript_path,
        "PostToolUse",
        unrelated_command,
    );

    let applied_commit = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve")
        .trim()
        .to_string();
    let note = target
        .read_authorship_note(&applied_commit)
        .expect("applied commit should have unknown note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("note should parse");
    assert!(
        log.metadata.sessions.is_empty(),
        "Bash call from another discovered repo must not be selected"
    );
    let mut file = target.filename("other-repo.txt");
    file.assert_committed_lines(lines!["must remain unknown".unattributed_human()]);
}
