#[macro_use]
#[path = "integration/repos/mod.rs"]
mod repos;

// Graphite-style restacks rewrite commits with `git commit-tree` + `git update-ref`.
// These tests model that plumbing path directly so they do not depend on `gt`.

use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::daemon::open_local_socket_stream_with_timeout;
use git_ai::git::find_repository_in_path;
use git_ai::git::notes_api::read_note;
use git_ai::git::repository::Repository as GitAiRepository;
use repos::test_file::ExpectedLineExt;
use repos::test_repo::{TestRepo, new_daemon_test_sync_session_id, real_git_executable};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const TRACE_ROOT_REFLOG_START_OFFSETS_FIELD: &str = "git_ai_root_reflog_start_offsets";

fn setup_initial_commit(repo: &TestRepo) {
    let mut readme = repo.filename("README.md");
    readme.set_contents(lines!["# Test Repo"]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
}

fn open_repo(repo: &TestRepo) -> GitAiRepository {
    find_repository_in_path(repo.path().to_str().unwrap())
        .expect("failed to open git-ai repository")
}

fn head_sha(repo: &TestRepo) -> String {
    repo.git(&["rev-parse", "HEAD"])
        .expect("rev-parse HEAD should succeed")
        .trim()
        .to_string()
}

fn assert_note_has_ai_for_file(repo: &TestRepo, commit_sha: &str, file_path: &str) {
    let note = repo
        .read_authorship_note(commit_sha)
        .unwrap_or_else(|| panic!("commit {} should have authorship note", &commit_sha[..8]));
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse authorship note");
    let attestation = log
        .attestations
        .iter()
        .find(|attestation| attestation.file_path == file_path)
        .unwrap_or_else(|| {
            panic!(
                "commit {} should have attestation for {}: {:?}",
                &commit_sha[..8],
                file_path,
                log.attestations
            )
        });
    assert!(
        attestation.entries.iter().any(|entry| {
            let author_id = entry.hash.split("::").next().unwrap_or(&entry.hash);
            log.metadata.sessions.contains_key(author_id)
                || log.metadata.prompts.contains_key(&entry.hash)
        }),
        "commit {} attestation for {} should contain AI entry: {:?}",
        &commit_sha[..8],
        file_path,
        attestation.entries
    );
}

fn ai_attested_lines_for_file(
    log: &AuthorshipLog,
    file_path: &str,
) -> std::collections::BTreeSet<u32> {
    log.attestations
        .iter()
        .find(|attestation| attestation.file_path == file_path)
        .map(|attestation| {
            attestation
                .entries
                .iter()
                .filter(|entry| {
                    let author_id = entry.hash.split("::").next().unwrap_or(&entry.hash);
                    log.metadata.sessions.contains_key(author_id)
                        || log.metadata.prompts.contains_key(&entry.hash)
                })
                .flat_map(|entry| entry.line_ranges.iter().flat_map(|range| range.expand()))
                .collect()
        })
        .unwrap_or_default()
}

fn raw_traced_git(repo: &TestRepo, args: &[&str]) -> String {
    let mut command = Command::new(real_git_executable());
    command.arg("-C").arg(repo.path()).args(args);
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env(
        "GIT_TRACE2_EVENT",
        git_ai::daemon::DaemonConfig::trace2_event_target_for_path(
            &repo.daemon_trace_socket_path(),
        ),
    );
    command.env(
        "GIT_TRACE2_EVENT_NESTING",
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string()),
    );

    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run raw traced git {:?}: {}", args, error));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "raw traced git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    if stdout.is_empty() {
        stderr
    } else if stderr.is_empty() {
        stdout
    } else {
        format!("{}{}", stdout, stderr)
    }
}

fn raw_traced_git_stdin(repo: &TestRepo, args: &[&str], stdin: &str) -> String {
    let mut command = Command::new(real_git_executable());
    command.arg("-C").arg(repo.path()).args(args);
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env(
        "GIT_TRACE2_EVENT",
        git_ai::daemon::DaemonConfig::trace2_event_target_for_path(
            &repo.daemon_trace_socket_path(),
        ),
    );
    command.env(
        "GIT_TRACE2_EVENT_NESTING",
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string()),
    );
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run raw traced git {:?}: {}", args, error));
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(stdin.as_bytes())
        .expect("write stdin to raw traced git");
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for raw traced git {:?}: {}", args, error));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "raw traced git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    combined_output(stdout, stderr)
}

fn raw_traced_git_with_session(repo: &TestRepo, args: &[&str], session: &str) -> String {
    let session_arg = format!("git-ai.testSyncSession={session}");
    let mut command = Command::new(real_git_executable());
    command
        .arg("-C")
        .arg(repo.path())
        .arg("-c")
        .arg(&session_arg)
        .args(args);
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env(
        "GIT_TRACE2_EVENT",
        git_ai::daemon::DaemonConfig::trace2_event_target_for_path(
            &repo.daemon_trace_socket_path(),
        ),
    );
    command.env(
        "GIT_TRACE2_EVENT_NESTING",
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string()),
    );

    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run raw traced git {:?}: {}", args, error));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "raw traced git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    combined_output(stdout, stderr)
}

fn raw_untraced_git(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &[("GIT_TRACE2_EVENT", "0")])
        .unwrap_or_else(|error| panic!("raw untraced git {:?} failed: {}", args, error))
}

fn raw_git_trace_to_file(repo: &TestRepo, args: &[&str], trace_path: &Path) -> String {
    let output = raw_git_trace_to_file_output(repo, args, trace_path);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "raw traced git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    combined_output(stdout, stderr)
}

fn raw_git_trace_to_file_output(repo: &TestRepo, args: &[&str], trace_path: &Path) -> Output {
    let _ = fs::remove_file(trace_path);
    let mut command = Command::new(real_git_executable());
    command.arg("-C").arg(repo.path()).args(args);
    command.env("HOME", repo.test_home_path());
    command.env(
        "GIT_CONFIG_GLOBAL",
        repo.test_home_path().join(".gitconfig"),
    );
    command.env("XDG_CONFIG_HOME", repo.test_home_path().join(".config"));
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_TRACE2_EVENT", trace_path);
    command.env(
        "GIT_TRACE2_EVENT_NESTING",
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string()),
    );

    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run raw traced git {:?}: {}", args, error))
}

fn combined_output(stdout: String, stderr: String) -> String {
    if stdout.is_empty() {
        stderr
    } else if stderr.is_empty() {
        stdout
    } else {
        format!("{}{}", stdout, stderr)
    }
}

fn replay_trace_file_to_daemon(repo: &TestRepo, trace_path: &Path) {
    let trace = fs::read(trace_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {}", trace_path.display(), error));
    let mut stream = open_local_socket_stream_with_timeout(
        &repo.daemon_trace_socket_path(),
        Duration::from_secs(2),
    )
    .expect("connect to daemon trace socket");
    stream
        .write_all(&trace)
        .expect("write delayed trace payload to daemon");
    stream.flush().expect("flush delayed trace payload");
}

fn replay_trace_payloads_to_daemon(repo: &TestRepo, payloads: &[Value]) {
    let mut stream = open_local_socket_stream_with_timeout(
        &repo.daemon_trace_socket_path(),
        Duration::from_secs(2),
    )
    .expect("connect to daemon trace socket");
    for payload in payloads {
        let line = serde_json::to_string(payload).expect("serialize trace payload");
        stream
            .write_all(line.as_bytes())
            .expect("write trace payload to daemon");
        stream.write_all(b"\n").expect("write trace newline");
    }
    stream.flush().expect("flush trace payloads");
}

fn open_unfinished_mutating_trace_root(
    repo: &TestRepo,
    sid: &str,
) -> git_ai::daemon::DaemonClientStream {
    let mut stream = open_local_socket_stream_with_timeout(
        &repo.daemon_trace_socket_path(),
        Duration::from_secs(2),
    )
    .expect("connect unfinished trace root to daemon");
    let line = serde_json::to_string(&json!({
        "event": "start",
        "sid": sid,
        "argv": ["git", "commit", "-m", "unfinished earlier command"],
        "time_ns": 1u64,
    }))
    .expect("serialize unfinished trace start");
    stream
        .write_all(line.as_bytes())
        .expect("write unfinished trace start");
    stream
        .write_all(b"\n")
        .expect("write unfinished trace newline");
    stream.flush().expect("flush unfinished trace start");
    stream
}

fn daemon_completed_session(repo: &TestRepo, session: &str) -> bool {
    repo.daemon_completion_entries()
        .iter()
        .any(|entry| entry.test_sync_session.as_deref() == Some(session))
}

fn current_reflog_offsets(repo: &TestRepo) -> serde_json::Map<String, Value> {
    let git_dir = repo.path().join(".git").canonicalize().unwrap();
    let mut offsets = serde_json::Map::new();
    let head_log = git_dir.join("logs").join("HEAD");
    if let Ok(metadata) = fs::metadata(&head_log) {
        offsets.insert(
            format!("worktree:{}:HEAD", git_dir.to_string_lossy()),
            json!(metadata.len()),
        );
    }
    let branch = repo.current_branch();
    let branch_ref = format!("refs/heads/{branch}");
    let branch_log = git_dir.join("logs").join(&branch_ref);
    if let Ok(metadata) = fs::metadata(&branch_log) {
        offsets.insert(format!("common:{branch_ref}"), json!(metadata.len()));
    }
    offsets
}

fn commit_tree_rewrite_current_branch(
    repo: &TestRepo,
    branch: &str,
    new_parent: &str,
    message: &str,
) -> (String, String) {
    let old_head = head_sha(repo);
    let tree = repo
        .git(&["rev-parse", &format!("{}^{{tree}}", old_head)])
        .expect("rev-parse HEAD^{tree} should succeed")
        .trim()
        .to_string();

    let new_head = repo
        .git(&["commit-tree", &tree, "-p", new_parent, "-m", message])
        .expect("git commit-tree should succeed")
        .trim()
        .to_string();

    repo.git(&[
        "update-ref",
        &format!("refs/heads/{}", branch),
        &new_head,
        &old_head,
    ])
    .expect("git update-ref should succeed");

    (old_head, new_head)
}

fn commit_tree_from_existing_tree(
    repo: &TestRepo,
    treeish: &str,
    new_parent: &str,
    message: &str,
) -> String {
    let tree = repo
        .git(&["rev-parse", &format!("{}^{{tree}}", treeish)])
        .expect("rev-parse tree should succeed")
        .trim()
        .to_string();

    repo.git(&["commit-tree", &tree, "-p", new_parent, "-m", message])
        .expect("git commit-tree should succeed")
        .trim()
        .to_string()
}

fn graphite_style_restack_child_branch(
    repo: &TestRepo,
    branch: &str,
    old_head: &str,
    new_parent: &str,
    message: &str,
) -> String {
    let old_parent = repo
        .git(&["rev-parse", &format!("{}^", old_head)])
        .expect("rev-parse old parent should succeed")
        .trim()
        .to_string();
    let old_grandparent = repo
        .git(&["rev-parse", &format!("{}^", old_parent)])
        .expect("rev-parse old grandparent should succeed")
        .trim()
        .to_string();

    let synthetic_parent = commit_tree_from_existing_tree(repo, new_parent, &old_grandparent, "_");
    let merged_tree = repo
        .git(&[
            "merge-tree",
            "--allow-unrelated-histories",
            &synthetic_parent,
            old_head,
        ])
        .expect("git merge-tree should succeed")
        .trim()
        .to_string();

    let new_head = repo
        .git(&["commit-tree", &merged_tree, "-p", new_parent, "-m", message])
        .expect("git commit-tree for rewritten child should succeed")
        .trim()
        .to_string();

    repo.git(&[
        "update-ref",
        &format!("refs/heads/{}", branch),
        &new_head,
        old_head,
    ])
    .expect("git update-ref should succeed");

    new_head
}

#[test]
fn test_soft_reset_amend_then_branch_move_preserves_squashed_child_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "parent"])
        .expect("checkout parent should succeed");
    let mut parent_file = repo.filename("csf_parent.txt");
    parent_file.set_contents(lines!["parent line 1", "parent line 2"]);
    repo.stage_all_and_commit("parent")
        .expect("parent commit should succeed");

    repo.git(&["checkout", "-b", "child"])
        .expect("checkout child should succeed");
    let mut child_file = repo.filename("csf_child.txt");
    child_file.set_contents(lines!["child ai 1".ai()]);
    let child_one = repo
        .stage_all_and_commit("child commit 1")
        .expect("child commit 1 should succeed");

    child_file.set_contents(lines!["child ai 1".ai(), "child ai 2".ai()]);
    repo.stage_all_and_commit("child commit 2")
        .expect("child commit 2 should succeed");

    repo.sync_daemon();
    let reset_session = new_daemon_test_sync_session_id();
    let amend_session = new_daemon_test_sync_session_id();
    let switch_session = new_daemon_test_sync_session_id();

    raw_traced_git_with_session(
        &repo,
        &["reset", "--soft", &child_one.commit_sha],
        &reset_session,
    );
    raw_traced_git_with_session(
        &repo,
        &["commit", "--amend", "-m", "squashed child"],
        &amend_session,
    );
    raw_traced_git_with_session(&repo, &["switch", "-C", "parent", "HEAD"], &switch_session);
    repo.sync_daemon_external_completion_sessions(&[reset_session, amend_session, switch_session]);

    parent_file.assert_lines_and_blame(lines!["parent line 1".human(), "parent line 2".human(),]);
    child_file.assert_lines_and_blame(lines!["child ai 1".ai(), "child ai 2".ai()]);
}

#[test]
fn test_delayed_soft_reset_amend_then_branch_move_preserves_squashed_child_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "parent"])
        .expect("checkout parent should succeed");
    let mut parent_file = repo.filename("delayed_csf_parent.txt");
    parent_file.set_contents(lines!["parent line 1", "parent line 2"]);
    repo.stage_all_and_commit("parent")
        .expect("parent commit should succeed");

    repo.git(&["checkout", "-b", "child"])
        .expect("checkout child should succeed");
    let mut child_file = repo.filename("delayed_csf_child.txt");
    child_file.set_contents(lines!["child ai 1".ai()]);
    let child_one = repo
        .stage_all_and_commit("child commit 1")
        .expect("child commit 1 should succeed");

    child_file.set_contents(lines!["child ai 1".ai(), "child ai 2".ai()]);
    repo.stage_all_and_commit("child commit 2")
        .expect("child commit 2 should succeed");

    repo.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let reset_trace = trace_dir.path().join("soft-reset.trace2");
    let amend_trace = trace_dir.path().join("amend.trace2");
    let switch_trace = trace_dir.path().join("switch.trace2");
    let reset_session = new_daemon_test_sync_session_id();
    let amend_session = new_daemon_test_sync_session_id();
    let switch_session = new_daemon_test_sync_session_id();
    let reset_session_arg = format!("git-ai.testSyncSession={reset_session}");
    let amend_session_arg = format!("git-ai.testSyncSession={amend_session}");
    let switch_session_arg = format!("git-ai.testSyncSession={switch_session}");

    raw_git_trace_to_file(
        &repo,
        &[
            "-c",
            &reset_session_arg,
            "reset",
            "--soft",
            &child_one.commit_sha,
        ],
        &reset_trace,
    );
    raw_git_trace_to_file(
        &repo,
        &[
            "-c",
            &amend_session_arg,
            "commit",
            "--amend",
            "-m",
            "squashed child",
        ],
        &amend_trace,
    );
    raw_git_trace_to_file(
        &repo,
        &["-c", &switch_session_arg, "switch", "-C", "parent", "HEAD"],
        &switch_trace,
    );

    replay_trace_file_to_daemon(&repo, &reset_trace);
    replay_trace_file_to_daemon(&repo, &amend_trace);
    replay_trace_file_to_daemon(&repo, &switch_trace);
    repo.sync_daemon_external_completion_sessions(&[reset_session, amend_session, switch_session]);

    parent_file.assert_lines_and_blame(lines!["parent line 1".human(), "parent line 2".human(),]);
    child_file.assert_lines_and_blame(lines!["child ai 1".ai(), "child ai 2".ai()]);
}

#[test]
fn test_split_trace_metadata_still_sequences_amend_authorship() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    let mut file = repo.filename("split_trace.txt");
    file.set_contents(lines!["split trace ai".ai()]);
    repo.stage_all_and_commit("split trace base")
        .expect("base commit should succeed");
    file.assert_committed_lines(lines!["split trace ai".ai()]);
    repo.sync_daemon();

    let offsets = current_reflog_offsets(&repo);
    let session = new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    raw_untraced_git(&repo, &["commit", "--amend", "-m", "split trace amended"]);
    let amended = head_sha(&repo);

    let sid = "20260411T120000.000000-Psplitmetadata";
    let mut start = json!({
        "event": "start",
        "sid": sid,
        "argv": ["git", "-c", session_arg, "commit", "--amend", "-m", "split trace amended"],
        "time_ns": 2u64,
    });
    start.as_object_mut().unwrap().insert(
        TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
        Value::Object(offsets),
    );
    replay_trace_payloads_to_daemon(
        &repo,
        &[
            json!({
                "event": "def_repo",
                "sid": sid,
                "worktree": repo.path().to_string_lossy().to_string(),
                "time_ns": 1u64,
            }),
            start,
            json!({
                "event": "atexit",
                "sid": sid,
                "code": 0,
                "time_ns": 3u64,
            }),
        ],
    );
    repo.sync_daemon_external_completion_sessions(&[session]);

    assert_note_has_ai_for_file(&repo, &amended, "split_trace.txt");
    file.assert_committed_lines(lines!["split trace ai".ai()]);
}

#[test]
fn test_back_to_back_raw_commits_do_not_span_later_ref_move() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    fs::write(repo.path().join("first.txt"), "first ai\n").unwrap();
    fs::write(repo.path().join("second.txt"), "second ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "first.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "second.txt"])
        .unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    raw_untraced_git(&repo, &["add", "first.txt"]);
    raw_traced_git(&repo, &["commit", "-m", "first raw commit"]);
    let first_commit = head_sha(&repo);

    raw_untraced_git(&repo, &["add", "second.txt"]);
    raw_traced_git(&repo, &["commit", "-m", "second raw commit"]);
    let second_commit = head_sha(&repo);

    repo.wait_for_daemon_total_completion_count(baseline, baseline + 2);

    assert_note_has_ai_for_file(&repo, &first_commit, "first.txt");
    assert_note_has_ai_for_file(&repo, &second_commit, "second.txt");
}

#[test]
fn test_raw_commit_trace2_does_not_record_created_commit_oid() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    fs::write(repo.path().join("trace-only.txt"), "trace only\n").unwrap();
    raw_untraced_git(&repo, &["add", "trace-only.txt"]);

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let commit_trace = trace_dir.path().join("commit.trace2");

    raw_git_trace_to_file(&repo, &["commit", "-m", "trace only"], &commit_trace);
    let commit_sha = head_sha(&repo);
    let trace = fs::read_to_string(&commit_trace).expect("read trace2 file");

    assert!(
        !trace.contains(&commit_sha),
        "stock trace2 should not contain the created commit oid"
    );
}

#[test]
fn test_delayed_commit_trace_replay_attributes_matching_commit_not_later_commit() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    fs::write(repo.path().join("first-delayed.txt"), "first delayed ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "first-delayed.txt"])
        .unwrap();
    raw_untraced_git(&repo, &["add", "first-delayed.txt"]);
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let commit_trace = trace_dir.path().join("commit.trace2");

    raw_git_trace_to_file(&repo, &["commit", "-m", "first delayed"], &commit_trace);
    let first_commit = head_sha(&repo);

    fs::write(repo.path().join("later-delayed.txt"), "later untraced\n").unwrap();
    raw_untraced_git(&repo, &["add", "later-delayed.txt"]);
    raw_untraced_git(&repo, &["commit", "-m", "later untraced commit"]);
    let later_commit = head_sha(&repo);

    replay_trace_file_to_daemon(&repo, &commit_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    assert_note_has_ai_for_file(&repo, &first_commit, "first-delayed.txt");
    assert!(
        repo.read_authorship_note(&later_commit).is_none(),
        "delayed commit trace replay must not attach attribution to a later commit"
    );
}

#[cfg(not(windows))]
#[test]
fn test_trace_listener_bootstrap_captures_commit_ref_transition_before_worker_spawn_delay() {
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_TRACE_LISTENER_WORKER_SPAWN_DELAY_MS",
        "200",
    )]);
    fs::write(repo.path().join("README.md"), "base\n").unwrap();
    repo.git_og(&["add", "README.md"]).unwrap();
    repo.git_og(&["commit", "-m", "base"]).unwrap();

    fs::write(
        repo.path().join("bootstrap-race.txt"),
        "bootstrap race ai\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "bootstrap-race.txt"])
        .unwrap();
    repo.git(&["add", "bootstrap-race.txt"]).unwrap();
    let committed = repo.commit("bootstrap race").unwrap();

    assert_note_has_ai_for_file(&repo, &committed.commit_sha, "bootstrap-race.txt");
}

#[test]
#[ignore = "stock trace2 does not record merge --squash source oid after SQUASH_MSG is gone"]
fn test_delayed_squash_merge_trace_replay_preserves_source_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("main.txt");

    file.set_contents(lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, lines!["feature ai".ai()]);
    repo.stage_all_and_commit("feature ai").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let merge_trace = trace_dir.path().join("merge.trace2");
    let commit_trace = trace_dir.path().join("commit.trace2");

    raw_git_trace_to_file(&repo, &["merge", "--squash", "feature"], &merge_trace);
    raw_git_trace_to_file(&repo, &["commit", "-m", "squash feature"], &commit_trace);
    let squash_commit = head_sha(&repo);

    replay_trace_file_to_daemon(&repo, &merge_trace);
    replay_trace_file_to_daemon(&repo, &commit_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 2);

    assert_note_has_ai_for_file(&repo, &squash_commit, "main.txt");
}

#[test]
fn test_delayed_stash_apply_trace_replay_preserves_named_stash_attribution() {
    let repo = TestRepo::new();
    let mut readme = repo.filename("README.md");
    readme.set_contents(lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();

    let mut first = repo.filename("first.txt");
    first.set_contents(lines!["first stash ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai", "first.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "first"]).unwrap();

    let mut second = repo.filename("second.txt");
    second.set_contents(lines!["second stash ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai", "second.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "second"]).unwrap();

    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let apply_trace = trace_dir.path().join("stash-apply.trace2");

    raw_git_trace_to_file(&repo, &["stash", "apply", "stash@{1}"], &apply_trace);
    repo.git_og(&["stash", "drop", "stash@{1}"])
        .expect("drop applied stash after raw apply");

    replay_trace_file_to_daemon(&repo, &apply_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    repo.stage_all_and_commit("apply first stash").unwrap();
    first.assert_committed_lines(lines!["first stash ai".ai()]);
}

#[test]
fn test_delayed_stash_pop_trace_replay_preserves_popped_stash_attribution() {
    let repo = TestRepo::new();
    let mut readme = repo.filename("README.md");
    readme.set_contents(lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();

    let mut first = repo.filename("first.txt");
    first.set_contents(lines!["first stash ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai", "first.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "first"]).unwrap();

    let mut second = repo.filename("second.txt");
    second.set_contents(lines!["second stash ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai", "second.txt"])
        .unwrap();
    repo.git(&["stash", "push", "-m", "second"]).unwrap();

    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let pop_trace = trace_dir.path().join("stash-pop.trace2");

    raw_git_trace_to_file(&repo, &["stash", "pop"], &pop_trace);
    repo.git_og(&["stash", "drop", "stash@{0}"])
        .expect("drop remaining stash after raw pop");

    replay_trace_file_to_daemon(&repo, &pop_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    repo.stage_all_and_commit("apply second stash").unwrap();
    second.assert_committed_lines(lines!["second stash ai".ai()]);
}

fn delayed_checkout_switch_merge_trace_replay_does_not_attribute_later_uncheckpointed_edit(
    command: &[&str],
) {
    let repo = TestRepo::new();
    let mut file = repo.filename("merge-carry.txt");

    file.set_contents(lines!["one", "two"]);
    repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(repo.path().join("merge-carry.txt"), "one feature\ntwo\n").unwrap();
    repo.stage_all_and_commit("feature edit").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    fs::write(repo.path().join("merge-carry.txt"), "one\ntwo ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "merge-carry.txt"])
        .unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let trace = trace_dir.path().join("checkout-switch-merge.trace2");

    raw_git_trace_to_file(&repo, command, &trace);
    fs::write(
        repo.path().join("merge-carry.txt"),
        "one feature\ntwo ai\nlater untracked\n",
    )
    .unwrap();

    replay_trace_file_to_daemon(&repo, &trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    repo.stage_all_and_commit("commit carried merge").unwrap();
    file.assert_committed_lines(lines![
        "one feature".human(),
        "two ai".ai(),
        "later untracked".ai(),
    ]);
}

#[test]
fn test_delayed_switch_merge_trace_replay_does_not_attribute_later_uncheckpointed_edit() {
    delayed_checkout_switch_merge_trace_replay_does_not_attribute_later_uncheckpointed_edit(&[
        "switch", "--merge", "feature",
    ]);
}

#[test]
fn test_delayed_checkout_merge_trace_replay_does_not_attribute_later_uncheckpointed_edit() {
    delayed_checkout_switch_merge_trace_replay_does_not_attribute_later_uncheckpointed_edit(&[
        "checkout", "--merge", "feature",
    ]);
}

#[test]
fn test_delayed_switch_trace_replay_renames_working_log_for_uncommitted_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(repo.path().join("feature-only.txt"), "feature only\n").unwrap();
    repo.stage_all_and_commit("feature only").unwrap();
    repo.git(&["checkout", &default_branch]).unwrap();

    let mut file = repo.filename("plain-switch.txt");
    file.set_contents(lines!["plain switch ai".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai", "plain-switch.txt"])
        .unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let switch_trace = trace_dir.path().join("switch.trace2");

    raw_git_trace_to_file(&repo, &["switch", "feature"], &switch_trace);
    replay_trace_file_to_daemon(&repo, &switch_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    repo.stage_all_and_commit("commit after plain switch")
        .unwrap();
    file.assert_committed_lines(lines!["plain switch ai".ai()]);
}

#[test]
#[ignore = "stock trace2 does not record rebased output commit oids"]
fn test_delayed_rebase_trace_replay_preserves_rebased_commit_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("feature.txt");

    file.set_contents(lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(lines!["base", "feature ai".ai()]);
    let original_feature = repo.stage_all_and_commit("feature ai").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    fs::write(repo.path().join("upstream.txt"), "upstream\n").unwrap();
    repo.stage_all_and_commit("upstream").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let rebase_trace = trace_dir.path().join("rebase.trace2");

    raw_git_trace_to_file(&repo, &["rebase", &default_branch], &rebase_trace);
    let rebased_feature = head_sha(&repo);
    assert_ne!(original_feature.commit_sha, rebased_feature);

    fs::write(repo.path().join("later.txt"), "later\n").unwrap();
    repo.git_og(&["add", "later.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "later untraced commit"])
        .unwrap();

    replay_trace_file_to_daemon(&repo, &rebase_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    assert_note_has_ai_for_file(&repo, &rebased_feature, "feature.txt");
}

#[test]
fn test_delayed_reset_trace_replay_reconstructs_reset_working_log_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    let mut file = repo.filename("reset-delayed.txt");
    file.set_contents(lines!["reset delayed ai".ai()]);
    let original_commit = repo.stage_all_and_commit("reset delayed ai").unwrap();

    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let reset_trace = trace_dir.path().join("reset.trace2");

    raw_git_trace_to_file(&repo, &["reset", "--mixed", "HEAD~1"], &reset_trace);
    replay_trace_file_to_daemon(&repo, &reset_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    let recommit = repo.stage_all_and_commit("recommit reset work").unwrap();
    assert_ne!(original_commit.commit_sha, recommit.commit_sha);
    file.assert_committed_lines(lines!["reset delayed ai".ai()]);
}

#[test]
fn test_delayed_cherry_pick_trace_replay_preserves_picked_commit_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("picked.txt");

    file.set_contents(lines!["base"]);
    repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(lines!["base", "picked ai".ai()]);
    let source = repo.stage_all_and_commit("picked ai").unwrap();

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let cherry_pick_trace = trace_dir.path().join("cherry-pick.trace2");

    raw_git_trace_to_file(
        &repo,
        &["cherry-pick", &source.commit_sha],
        &cherry_pick_trace,
    );
    let picked_commit = head_sha(&repo);

    fs::write(repo.path().join("later.txt"), "later\n").unwrap();
    repo.git_og(&["add", "later.txt"]).unwrap();
    repo.git_og(&["commit", "-m", "later untraced commit"])
        .unwrap();

    replay_trace_file_to_daemon(&repo, &cherry_pick_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    assert_note_has_ai_for_file(&repo, &picked_commit, "picked.txt");
}

#[test]
fn test_delayed_multi_cherry_pick_trace_replay_starts_at_first_pick_when_intermediate_ref_known() {
    let repo = TestRepo::new();
    let mut file = repo.filename("multi-picked.txt");

    file.set_contents(lines!["base"]);
    let base = repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.set_contents(lines!["base", "first picked ai".ai()]);
    repo.stage_all_and_commit("first picked ai").unwrap();
    let source_one = head_sha(&repo);
    file.set_contents(lines![
        "base",
        "first picked ai".ai(),
        "second picked ai".ai(),
    ]);
    repo.stage_all_and_commit("second picked ai").unwrap();
    let source_two = head_sha(&repo);

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let cherry_pick_trace = trace_dir.path().join("multi-cherry-pick.trace2");
    let session = new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    raw_git_trace_to_file(
        &repo,
        &["-c", &session_arg, "cherry-pick", &source_one, &source_two],
        &cherry_pick_trace,
    );
    let picked_commits = repo
        .git_og(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", base.commit_sha),
        ])
        .expect("rev-list picked commits should succeed")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(picked_commits.len(), 2);

    repo.git(&["branch", "known-intermediate-pick", &picked_commits[0]])
        .expect("creating intermediate branch should succeed");
    repo.sync_daemon();

    replay_trace_file_to_daemon(&repo, &cherry_pick_trace);
    repo.sync_daemon_external_completion_sessions(&[session]);

    assert_note_has_ai_for_file(&repo, &picked_commits[0], "multi-picked.txt");
    assert_note_has_ai_for_file(&repo, &picked_commits[1], "multi-picked.txt");
    file.assert_committed_lines(lines![
        "base".ai(),
        "first picked ai".ai(),
        "second picked ai".ai(),
    ]);
}

#[test]
fn test_delayed_failed_cherry_pick_with_unresolved_source_does_not_consume_later_pick() {
    let repo = TestRepo::new();
    let mut file = repo.filename("file.txt");

    file.set_contents(lines!["base line"]);
    repo.stage_all_and_commit("initial").unwrap();
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(1, lines!["AI line 1".ai()]);
    repo.stage_all_and_commit("AI commit 1").unwrap();
    let source_one = head_sha(&repo);

    file.insert_at(2, lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("AI commit 2").unwrap();
    let source_two = head_sha(&repo);

    repo.git(&["checkout", &default_branch]).unwrap();
    repo.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let failed_trace = trace_dir.path().join("failed-cherry-pick.trace2");
    let good_trace = trace_dir.path().join("good-cherry-pick.trace2");
    let failed_session = new_daemon_test_sync_session_id();
    let good_session = new_daemon_test_sync_session_id();
    let failed_session_arg = format!("git-ai.testSyncSession={failed_session}");
    let good_session_arg = format!("git-ai.testSyncSession={good_session}");
    let bad_source_arg = format!("{source_one} {source_two}");

    let failed = raw_git_trace_to_file_output(
        &repo,
        &["-c", &failed_session_arg, "cherry-pick", &bad_source_arg],
        &failed_trace,
    );
    assert!(
        !failed.status.success(),
        "combined cherry-pick source should be invalid\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );

    raw_git_trace_to_file(
        &repo,
        &["-c", &good_session_arg, "cherry-pick", &source_one],
        &good_trace,
    );
    let picked_commit = head_sha(&repo);

    replay_trace_file_to_daemon(&repo, &failed_trace);
    replay_trace_file_to_daemon(&repo, &good_trace);
    repo.sync_daemon_external_completion_sessions(&[failed_session, good_session]);

    assert_note_has_ai_for_file(&repo, &picked_commit, "file.txt");
    file.assert_committed_lines(lines!["base line".ai(), "AI line 1".ai()]);
}

#[test]
fn test_delayed_pull_rebase_trace_replay_starts_at_start_when_intermediate_ref_known() {
    let (local, _upstream) = TestRepo::new_with_remote();
    let mut file = local.filename("pull-rebase-picked.txt");

    file.set_contents(lines!["base"]);
    let initial = local.stage_all_and_commit("initial").unwrap();
    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    file.set_contents(lines!["base", "first local ai".ai()]);
    local.stage_all_and_commit("first local ai").unwrap();
    file.set_contents(lines![
        "base",
        "first local ai".ai(),
        "second local ai".ai(),
    ]);
    let local_tip = local.stage_all_and_commit("second local ai").unwrap();
    let branch = local.current_branch();

    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial should succeed");
    let mut upstream_file = local.filename("pull-rebase-upstream.txt");
    upstream_file.set_contents(lines!["upstream"]);
    let upstream_tip = local.stage_all_and_commit("upstream").unwrap();
    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("push upstream divergence should succeed");
    local
        .git(&["reset", "--hard", &local_tip.commit_sha])
        .expect("reset to local tip should succeed");
    local.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let pull_trace = trace_dir.path().join("pull-rebase.trace2");
    let session = new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    raw_git_trace_to_file(
        &local,
        &["-c", &session_arg, "pull", "--rebase", "origin", &branch],
        &pull_trace,
    );
    let rebased_commits = local
        .git_og(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", upstream_tip.commit_sha),
        ])
        .expect("rev-list rebased commits should succeed")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(rebased_commits.len(), 2);

    local
        .git(&[
            "branch",
            "known-pull-rebase-intermediate",
            &rebased_commits[0],
        ])
        .expect("creating intermediate pull branch should succeed");
    local.sync_daemon();

    replay_trace_file_to_daemon(&local, &pull_trace);
    local.sync_daemon_external_completion_sessions(&[session]);

    assert_note_has_ai_for_file(&local, &rebased_commits[0], "pull-rebase-picked.txt");
    assert_note_has_ai_for_file(&local, &rebased_commits[1], "pull-rebase-picked.txt");
    file.assert_committed_lines(lines![
        "base".ai(),
        "first local ai".ai(),
        "second local ai".ai(),
    ]);
}

#[test]
fn test_delayed_multi_revert_trace_replay_starts_at_first_revert_when_intermediate_ref_known() {
    let repo = TestRepo::new();
    let mut first_file = repo.filename("multi-reverted-first.txt");
    let mut second_file = repo.filename("multi-reverted-second.txt");

    first_file.set_contents(lines!["first revert-restored ai".ai()]);
    let first_ai = repo.stage_all_and_commit("first ai").unwrap();
    first_file.set_contents(lines!["first human replacement"]);
    let replace_first = repo.stage_all_and_commit("replace first ai").unwrap();
    second_file.set_contents(lines!["second revert-restored ai".ai()]);
    let second_ai = repo.stage_all_and_commit("second ai").unwrap();
    second_file.set_contents(lines!["second human replacement"]);
    let replace_second = repo.stage_all_and_commit("replace second ai").unwrap();
    assert_note_has_ai_for_file(&repo, &first_ai.commit_sha, "multi-reverted-first.txt");
    assert_note_has_ai_for_file(&repo, &second_ai.commit_sha, "multi-reverted-second.txt");
    repo.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let revert_trace = trace_dir.path().join("multi-revert.trace2");
    let session = new_daemon_test_sync_session_id();
    let session_arg = format!("git-ai.testSyncSession={session}");
    raw_git_trace_to_file(
        &repo,
        &[
            "-c",
            &session_arg,
            "revert",
            "--no-edit",
            &replace_second.commit_sha,
            &replace_first.commit_sha,
        ],
        &revert_trace,
    );
    let revert_commits = repo
        .git_og(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", replace_second.commit_sha),
        ])
        .expect("rev-list revert commits should succeed")
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(revert_commits.len(), 2);

    repo.git(&["branch", "known-intermediate-revert", &revert_commits[0]])
        .expect("creating intermediate revert branch should succeed");
    repo.sync_daemon();

    replay_trace_file_to_daemon(&repo, &revert_trace);
    repo.sync_daemon_external_completion_sessions(&[session]);

    assert_note_has_ai_for_file(&repo, &revert_commits[0], "multi-reverted-second.txt");
    assert_note_has_ai_for_file(&repo, &revert_commits[1], "multi-reverted-first.txt");
    first_file.assert_committed_lines(lines!["first revert-restored ai".ai()]);
    second_file.assert_committed_lines(lines!["second revert-restored ai".ai()]);
}

#[test]
fn test_delayed_commit_trace_uses_committed_tree_not_later_worktree() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    let file_rel = "delayed-commit-race.txt";
    let file_path = repo.path().join(file_rel);

    fs::write(&file_path, "first ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", file_rel]).unwrap();
    repo.git_og(&["add", file_rel]).unwrap();
    repo.sync_daemon();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let commit_trace = trace_dir.path().join("commit.trace2");
    raw_git_trace_to_file(&repo, &["commit", "-m", "first ai"], &commit_trace);
    let first_commit = head_sha(&repo);

    fs::write(&file_path, "first ai\nsecond ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", file_rel]).unwrap();
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    replay_trace_file_to_daemon(&repo, &commit_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);
    repo.sync_daemon();

    let mut file = repo.filename(file_rel);
    file.assert_committed_lines(lines!["first ai".ai()]);

    repo.stage_all_and_commit("second ai")
        .expect("second commit should succeed");
    file.assert_committed_lines(lines!["first ai".ai(), "second ai".ai()]);

    assert_note_has_ai_for_file(&repo, &first_commit, file_rel);
}

#[test]
fn test_commit_tree_update_ref_preserves_authorship_notes_on_reparent() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["human line", "ai line".ai()]);
    let feature_commit = repo
        .stage_all_and_commit("feature commit")
        .expect("feature commit should succeed");

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &feature_commit.commit_sha).is_some(),
        "expected initial feature commit to have an authorship note",
    );

    repo.git(&["checkout", "main"])
        .expect("checkout main should succeed");
    let mut trunk_file = repo.filename("trunk.txt");
    trunk_file.set_contents(lines!["trunk update"]);
    let main_commit = repo
        .stage_all_and_commit("main update")
        .expect("main update should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    let (old_head, new_head) = commit_tree_rewrite_current_branch(
        &repo,
        "feature",
        &main_commit.commit_sha,
        "feature commit",
    );

    repo.sync_daemon();

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &new_head).is_some(),
        "expected rewritten commit {} to preserve authorship note from {}",
        new_head,
        old_head,
    );

    let mut rewritten_file = repo.filename("feature.txt");
    rewritten_file.assert_lines_and_blame(lines!["human line".human(), "ai line".ai()]);
}

#[test]
fn test_commit_tree_update_ref_moves_working_log_to_rewritten_head() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["human line", "committed ai".ai()]);
    repo.stage_all_and_commit("feature commit")
        .expect("feature commit should succeed");

    repo.git(&["checkout", "main"])
        .expect("checkout main should succeed");
    let mut trunk_file = repo.filename("trunk.txt");
    trunk_file.set_contents(lines!["trunk update"]);
    let main_commit = repo
        .stage_all_and_commit("main update")
        .expect("main update should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    feature_file.set_contents_no_stage(lines![
        "human line",
        "committed ai".ai(),
        "pending ai".ai(),
    ]);

    repo.sync_daemon();

    let old_head = head_sha(&repo);
    let git_ai_repo = open_repo(&repo);
    assert!(
        git_ai_repo.storage.has_working_log(&old_head),
        "expected dirty branch to have a working log before rewrite",
    );

    let (_, new_head) = commit_tree_rewrite_current_branch(
        &repo,
        "feature",
        &main_commit.commit_sha,
        "feature commit",
    );

    repo.sync_daemon();

    let git_ai_repo = open_repo(&repo);
    assert!(
        git_ai_repo.storage.has_working_log(&new_head),
        "expected working log to follow rewritten HEAD from {} to {}",
        old_head,
        new_head,
    );
    assert!(
        !git_ai_repo.storage.has_working_log(&old_head),
        "expected working log for old HEAD {} to be renamed away",
        old_head,
    );

    repo.git(&["add", "-A"]).expect("git add should succeed");
    repo.commit("commit after plumbing rewrite")
        .expect("commit after plumbing rewrite should succeed");

    let mut rewritten_file = repo.filename("feature.txt");
    rewritten_file.assert_lines_and_blame(lines![
        "human line".human(),
        "committed ai".ai(),
        "pending ai".ai(),
    ]);
}

#[test]
fn test_reset_keep_rewrite_preserves_authorship_notes_on_current_branch() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    let mut feature_file = repo.filename("feature.txt");
    feature_file.set_contents(lines!["human line", "ai line".ai()]);
    let feature_commit = repo
        .stage_all_and_commit("feature commit")
        .expect("feature commit should succeed");

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &feature_commit.commit_sha).is_some(),
        "expected initial feature commit to have an authorship note",
    );

    repo.git(&["checkout", "main"])
        .expect("checkout main should succeed");
    let mut trunk_file = repo.filename("trunk.txt");
    trunk_file.set_contents(lines!["trunk update"]);
    let main_commit = repo
        .stage_all_and_commit("main update")
        .expect("main update should succeed");

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");
    let old_head = head_sha(&repo);
    let new_head =
        commit_tree_from_existing_tree(&repo, &old_head, &main_commit.commit_sha, "feature commit");

    repo.git(&["reset", "--keep", &new_head])
        .expect("git reset --keep should succeed");

    repo.sync_daemon();

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &new_head).is_some(),
        "expected rewritten current-branch commit {} to preserve authorship note from {}",
        new_head,
        old_head,
    );

    let mut rewritten_file = repo.filename("feature.txt");
    rewritten_file.assert_lines_and_blame(lines!["human line".human(), "ai line".ai()]);
}

#[test]
fn test_update_ref_restack_after_parent_amend_preserves_child_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "parent"])
        .expect("checkout parent should succeed");
    let mut parent_file = repo.filename("parent.txt");
    parent_file.set_contents(lines!["parent ai".ai(), "parent human"]);
    let parent_commit = repo
        .stage_all_and_commit("parent")
        .expect("parent commit should succeed");

    repo.git(&["checkout", "-b", "child"])
        .expect("checkout child should succeed");
    let mut child_file = repo.filename("child.txt");
    child_file.set_contents(lines!["child ai".ai(), "child human"]);
    let child_commit = repo
        .stage_all_and_commit("child")
        .expect("child commit should succeed");

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &child_commit.commit_sha).is_some(),
        "expected initial child commit to have an authorship note",
    );

    repo.git(&["checkout", "parent"])
        .expect("checkout parent should succeed");
    let mut parent_file2 = repo.filename("parent2.txt");
    parent_file2.set_contents(lines!["parent2 ai".ai()]);
    repo.git(&["add", "-A"]).expect("git add should succeed");
    repo.git(&["commit", "--amend", "-m", "modified parent"])
        .expect("git commit --amend should succeed");

    let amended_parent_head = head_sha(&repo);
    assert_ne!(
        amended_parent_head, parent_commit.commit_sha,
        "expected parent amend to rewrite the parent branch"
    );

    let new_child_head = graphite_style_restack_child_branch(
        &repo,
        "child",
        &child_commit.commit_sha,
        &amended_parent_head,
        "child",
    );

    repo.sync_daemon();

    let git_ai_repo = open_repo(&repo);
    assert!(
        read_note(&git_ai_repo, &new_child_head).is_some(),
        "expected rewritten child commit {} to preserve authorship note from {}",
        new_child_head,
        child_commit.commit_sha,
    );

    repo.git(&["checkout", "child"])
        .expect("checkout child should succeed");
    let mut rewritten_child_file = repo.filename("child.txt");
    rewritten_child_file.assert_lines_and_blame(lines!["child ai".ai(), "child human".human()]);
}

/// Test Graphite-style rebase: replay multiple feature commits via commit-tree,
/// then move the branch with ONE update-ref from old tip to new tip.
///
/// This matches actual `gt sync` behavior where Graphite replays all commits
/// using plumbing commands and issues a single atomic update-ref at the end.
/// git-ai must detect the N-commit rewrite and remap all N authorship notes.
#[test]
fn test_graphite_style_multi_commit_single_update_ref() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);
    let default_branch = repo.current_branch();

    // Create feature branch with 3 AI commits
    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature");

    let mut file_a = repo.filename("a.txt");
    file_a.set_contents(lines!["a1 ai".ai(), "a2 human"]);
    repo.stage_all_and_commit("feat: add file a")
        .expect("feat 1");

    let mut file_b = repo.filename("b.txt");
    file_b.set_contents(lines!["b1 ai".ai(), "b2 ai".ai()]);
    repo.stage_all_and_commit("feat: add file b")
        .expect("feat 2");

    file_a.set_contents(lines!["a1 ai".ai(), "a2 human", "a3 ai".ai()]);
    repo.stage_all_and_commit("feat: extend file a")
        .expect("feat 3");

    // Collect feature commits (oldest to newest)
    let feature_commits_str = repo
        .git(&[
            "rev-list",
            "--reverse",
            &format!("{}..HEAD", default_branch),
        ])
        .expect("rev-list");
    let feature_commits: Vec<&str> = feature_commits_str
        .trim()
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(feature_commits.len(), 3, "expected 3 feature commits");

    // Verify all 3 have authorship notes pre-rebase
    let git_ai_repo = open_repo(&repo);
    for &sha in &feature_commits {
        assert!(
            read_note(&git_ai_repo, sha).is_some(),
            "pre-rebase: commit {} should have authorship note",
            sha
        );
    }

    // Advance main so rebase has new base
    repo.git(&["checkout", &default_branch])
        .expect("checkout main");
    let mut trunk = repo.filename("trunk.txt");
    trunk.set_contents(lines!["trunk line 1"]);
    repo.stage_all_and_commit("main advance 1").expect("main 1");
    trunk.set_contents(lines!["trunk line 1", "trunk line 2"]);
    repo.stage_all_and_commit("main advance 2").expect("main 2");
    let main_tip = head_sha(&repo);

    // Switch back to feature for the replay
    repo.git(&["checkout", "feature"])
        .expect("checkout feature");
    let old_tip = head_sha(&repo);

    // Replay all commits via commit-tree (no update-ref yet)
    let mut new_parent = main_tip.clone();
    for &feature_sha in &feature_commits {
        let old_parent = repo
            .git(&["rev-parse", &format!("{}^", feature_sha)])
            .expect("rev-parse parent")
            .trim()
            .to_string();

        let merged_tree_output = repo
            .git(&[
                "merge-tree",
                "--write-tree",
                "--merge-base",
                &old_parent,
                &new_parent,
                feature_sha,
            ])
            .expect("merge-tree");
        let merged_tree = merged_tree_output
            .trim()
            .lines()
            .next()
            .unwrap()
            .to_string();

        let message = repo
            .git(&["log", "-1", "--format=%s", feature_sha])
            .expect("log message")
            .trim()
            .to_string();

        let new_commit = repo
            .git(&[
                "commit-tree",
                &merged_tree,
                "-p",
                &new_parent,
                "-m",
                &message,
            ])
            .expect("commit-tree")
            .trim()
            .to_string();

        new_parent = new_commit;
    }

    // ONE atomic update-ref (matches Graphite's actual behavior)
    let new_tip = new_parent;
    repo.git(&["update-ref", "refs/heads/feature", &new_tip, &old_tip])
        .expect("update-ref");
    repo.git(&["reset", "--hard", &new_tip]).expect("reset");

    repo.sync_daemon();

    // Verify all 3 rebased commits have authorship notes
    let rebased_commits_str = repo
        .git(&["rev-list", "--reverse", &format!("{}..HEAD", main_tip)])
        .expect("rev-list rebased");
    let rebased_commits: Vec<&str> = rebased_commits_str
        .trim()
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(rebased_commits.len(), 3, "expected 3 rebased commits");

    let git_ai_repo = open_repo(&repo);
    for (idx, &sha) in rebased_commits.iter().enumerate() {
        assert!(
            read_note(&git_ai_repo, sha).is_some(),
            "post-rebase: rebased commit {} (index {}) should have authorship note",
            sha,
            idx
        );
    }

    // Verify attribution on file_b (single-commit, straightforward)
    file_b.assert_lines_and_blame(lines!["b1 ai".ai(), "b2 ai".ai()]);
}

#[test]
fn test_update_ref_head_with_new_content_then_amend_preserves_attribution() {
    use std::fs;

    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    let file_path = repo.path().join("feature.txt");

    // Write AI content and checkpoint
    fs::write(&file_path, "ai line 1\nai line 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();

    // Stage
    repo.git(&["add", "-A"]).unwrap();

    // Plumbing: write-tree, commit-tree, update-ref HEAD
    let parent_sha = head_sha(&repo);
    let tree_sha = repo.git(&["write-tree"]).unwrap().trim().to_string();
    let commit_sha = repo
        .git(&[
            "commit-tree",
            &tree_sha,
            "-p",
            &parent_sha,
            "-m",
            "plumbing commit",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "HEAD", &commit_sha, &parent_sha])
        .unwrap();

    let mut feature_file = repo.filename("feature.txt");
    feature_file.assert_lines_and_blame(lines!["ai line 1".ai(), "ai line 2".ai()]);
}

#[test]
fn test_update_ref_current_branch_with_new_content_preserves_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    fs::write(repo.path().join("branch-plumbing.txt"), "branch ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "branch-plumbing.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();

    let parent_sha = head_sha(&repo);
    let tree_sha = repo.git(&["write-tree"]).unwrap().trim().to_string();
    let commit_sha = repo
        .git(&[
            "commit-tree",
            &tree_sha,
            "-p",
            &parent_sha,
            "-m",
            "branch plumbing commit",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/feature", &commit_sha, &parent_sha])
        .unwrap();

    let mut feature_file = repo.filename("branch-plumbing.txt");
    feature_file.assert_lines_and_blame(lines!["branch ai".ai()]);
}

#[test]
fn test_update_ref_fast_forward_bounds_committed_hunks_to_final_commit() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    let file_rel = "ff-overlap.txt";
    let file_path = repo.path().join(file_rel);
    fs::write(&file_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", file_rel])
        .unwrap();
    repo.stage_all_and_commit("add fast-forward overlap base")
        .unwrap();
    let mut file = repo.filename(file_rel);
    file.assert_committed_lines(lines!["base".human()]);

    let old_tip = head_sha(&repo);
    let final_content = "base\nintermediate pulled line\nfinal checkpointed line\n";
    fs::write(&file_path, final_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", file_rel]).unwrap();
    repo.sync_daemon();

    fs::write(&file_path, "base\nintermediate pulled line\n").unwrap();
    raw_untraced_git(&repo, &["add", file_rel]);
    let intermediate_tree = raw_untraced_git(&repo, &["write-tree"]).trim().to_string();
    let intermediate_commit = raw_untraced_git(
        &repo,
        &[
            "commit-tree",
            &intermediate_tree,
            "-p",
            &old_tip,
            "-m",
            "intermediate pulled commit",
        ],
    )
    .trim()
    .to_string();

    fs::write(&file_path, final_content).unwrap();
    raw_untraced_git(&repo, &["add", file_rel]);
    let final_tree = raw_untraced_git(&repo, &["write-tree"]).trim().to_string();
    let final_commit = raw_untraced_git(
        &repo,
        &[
            "commit-tree",
            &final_tree,
            "-p",
            &intermediate_commit,
            "-m",
            "final pulled commit",
        ],
    )
    .trim()
    .to_string();

    repo.git(&["update-ref", "HEAD", &final_commit, &old_tip])
        .unwrap();
    let note = repo
        .read_authorship_note(&final_commit)
        .expect("fast-forward final commit should have an authorship note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("parse authorship note");
    let ai_lines = ai_attested_lines_for_file(&log, file_rel);

    assert!(
        !ai_lines.contains(&2),
        "intermediate pulled line must not be attributed from the old-tip..new-head diff: {ai_lines:?}"
    );
    assert!(
        ai_lines.contains(&3),
        "final commit line should remain attributed to the checkpointed AI edit: {ai_lines:?}"
    );

    file.assert_committed_lines(lines![
        "base".human(),
        "intermediate pulled line".human(),
        "final checkpointed line".ai(),
    ]);
}

#[test]
fn test_delayed_current_branch_update_ref_trace_preserves_new_commit_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    fs::write(
        repo.path().join("delayed-branch-plumbing.txt"),
        "branch ai\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "delayed-branch-plumbing.txt"])
        .unwrap();
    raw_untraced_git(&repo, &["add", "-A"]);
    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();

    let parent_sha = head_sha(&repo);
    let tree_sha = raw_untraced_git(&repo, &["write-tree"]).trim().to_string();
    let commit_sha = raw_untraced_git(
        &repo,
        &[
            "commit-tree",
            &tree_sha,
            "-p",
            &parent_sha,
            "-m",
            "delayed branch plumbing commit",
        ],
    )
    .trim()
    .to_string();

    let trace_dir = tempfile::tempdir().expect("trace temp dir");
    let update_ref_trace = trace_dir.path().join("update-ref.trace2");
    raw_git_trace_to_file(
        &repo,
        &["update-ref", "refs/heads/feature", &commit_sha, &parent_sha],
        &update_ref_trace,
    );

    replay_trace_file_to_daemon(&repo, &update_ref_trace);
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    assert_note_has_ai_for_file(&repo, &commit_sha, "delayed-branch-plumbing.txt");
    let mut feature_file = repo.filename("delayed-branch-plumbing.txt");
    feature_file.assert_lines_and_blame(lines!["branch ai".ai()]);
}

#[test]
fn test_update_ref_side_effect_waits_for_prior_open_trace_root_without_family() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout feature should succeed");

    fs::write(
        repo.path().join("sequenced-branch-plumbing.txt"),
        "sequenced branch ai\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "sequenced-branch-plumbing.txt"])
        .unwrap();
    raw_untraced_git(&repo, &["add", "-A"]);
    repo.sync_daemon();

    let parent_sha = head_sha(&repo);
    let tree_sha = raw_untraced_git(&repo, &["write-tree"]).trim().to_string();
    let commit_sha = raw_untraced_git(
        &repo,
        &[
            "commit-tree",
            &tree_sha,
            "-p",
            &parent_sha,
            "-m",
            "sequenced branch plumbing commit",
        ],
    )
    .trim()
    .to_string();

    let unfinished_trace =
        open_unfinished_mutating_trace_root(&repo, "20260411T120000.000000-Punfinished-root");
    std::thread::sleep(Duration::from_millis(100));

    let session = new_daemon_test_sync_session_id();
    raw_traced_git_with_session(
        &repo,
        &["update-ref", "refs/heads/feature", &commit_sha, &parent_sha],
        &session,
    );

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        assert!(
            !daemon_completed_session(&repo, &session),
            "update-ref side effect completed while an earlier mutating trace root was still open without family metadata"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    drop(unfinished_trace);
    repo.sync_daemon_external_completion_sessions(&[session]);

    assert_note_has_ai_for_file(&repo, &commit_sha, "sequenced-branch-plumbing.txt");
    let mut feature_file = repo.filename("sequenced-branch-plumbing.txt");
    feature_file.assert_lines_and_blame(lines!["sequenced branch ai".ai()]);
}

#[test]
fn test_update_ref_stdin_head_with_new_content_preserves_attribution() {
    let repo = TestRepo::new();
    setup_initial_commit(&repo);

    fs::write(repo.path().join("stdin.txt"), "stdin ai\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "stdin.txt"])
        .unwrap();
    raw_untraced_git(&repo, &["add", "-A"]);

    let parent_sha = head_sha(&repo);
    let tree_sha = raw_untraced_git(&repo, &["write-tree"]).trim().to_string();
    let commit_sha = raw_untraced_git(
        &repo,
        &[
            "commit-tree",
            &tree_sha,
            "-p",
            &parent_sha,
            "-m",
            "stdin commit",
        ],
    )
    .trim()
    .to_string();

    repo.sync_daemon();
    let baseline = repo.daemon_total_completion_count();
    raw_traced_git_stdin(
        &repo,
        &["update-ref", "--stdin"],
        &format!("update HEAD {} {}\n", commit_sha, parent_sha),
    );
    repo.wait_for_daemon_total_completion_count(baseline, baseline + 1);

    assert_note_has_ai_for_file(&repo, &commit_sha, "stdin.txt");
}
