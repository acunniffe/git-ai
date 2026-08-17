use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{
    codex_bash_hook_input as shared_codex_bash_hook_input, fixture_path,
    isolated_bash_history_db_path,
};
use std::fs;

fn codex_bash_hook_input(
    repo: &TestRepo,
    transcript_path: &std::path::Path,
    hook_event_name: &str,
    command: &str,
) -> String {
    shared_codex_bash_hook_input(
        repo,
        transcript_path,
        "git-apply-bash-session",
        "git-apply-bash-tool",
        hook_event_name,
        command,
    )
}

fn begin_codex_bash(repo: &TestRepo, command: &str) -> std::path::PathBuf {
    let transcript_path = repo.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript_path).unwrap();
    let input = codex_bash_hook_input(repo, &transcript_path, "PreToolUse", command);
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &input])
        .unwrap();
    transcript_path
}

fn end_codex_bash(repo: &TestRepo, transcript_path: &std::path::Path, command: &str) {
    let input = codex_bash_hook_input(repo, transcript_path, "PostToolUse", command);
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &input])
        .unwrap();
}

/// `git apply --cached` mutates only the index, so the Bash stat-diff cannot
/// observe the AI-authored file. The later commit must preserve the applied
/// patch's AI provenance without painting unrelated, previously staged work AI.
#[test]
fn test_git_apply_cached_then_commit_attributes_only_applied_patch_to_ai() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("base.txt"), "existing base\n")
        .expect("target base should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    let mut base = target.filename("base.txt");
    base.assert_committed_lines(lines!["existing base".unattributed_human()]);

    // This work was staged before the AI tool call and must not be claimed by
    // the overlapping Bash session.
    fs::write(target.path().join("human-staged.txt"), "staged by human\n")
        .expect("human-staged file should be writable");
    target
        .git_og(&["add", "human-staged.txt"])
        .expect("human file should be staged");

    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("index-only.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/ai-index-only.txt b/ai-index-only.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/ai-index-only.txt\n",
            "@@ -0,0 +1 @@\n",
            "+created in the index by AI\n",
        ),
    )
    .expect("patch should be writable");

    let transcript_path = target.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript_path)
        .expect("transcript fixture should be copied");
    let command = format!(
        "git apply --cached {} && git commit -m 'Apply generated patch'",
        patch_path.display()
    );
    let pre_hook_input = codex_bash_hook_input(&target, &transcript_path, "PreToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");

    target
        .git(&["apply", "--cached", patch_path.to_str().unwrap()])
        .expect("git apply --cached should succeed");
    target
        .git(&["commit", "-m", "Apply generated patch"])
        .expect("commit should succeed");

    let post_hook_input = codex_bash_hook_input(&target, &transcript_path, "PostToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");
    target.sync_daemon();

    // `--cached` intentionally leaves the new path absent from the worktree;
    // materialize it only so TestFile's blame helper can canonicalize it.
    target
        .git_og(&["checkout", "HEAD", "--", "ai-index-only.txt"])
        .expect("committed index-only file should be materialized for assertions");

    let mut ai_file = target.filename("ai-index-only.txt");
    ai_file.assert_committed_lines(lines!["created in the index by AI".ai()]);
    let mut human_file = target.filename("human-staged.txt");
    human_file.assert_committed_lines(lines!["staged by human".unattributed_human()]);
}

/// The baseline is line-granular, not merely path-granular: a file may contain
/// human changes staged before the Bash call and AI changes applied afterward.
#[test]
fn test_git_apply_index_preserves_pre_staged_human_line_in_same_file() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("mixed.txt"), "base one\nbase two\n")
        .expect("base file should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    let mut mixed = target.filename("mixed.txt");
    mixed.assert_committed_lines(lines![
        "base one".unattributed_human(),
        "base two".unattributed_human(),
    ]);

    fs::write(target.path().join("mixed.txt"), "human staged\nbase two\n")
        .expect("human edit should be writable");
    target
        .git_og(&["add", "mixed.txt"])
        .expect("human line should be staged");

    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("mixed-index.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/mixed.txt b/mixed.txt\n",
            "--- a/mixed.txt\n",
            "+++ b/mixed.txt\n",
            "@@ -1,2 +1,2 @@\n",
            " human staged\n",
            "-base two\n",
            "+changed in index and worktree by AI\n",
        ),
    )
    .expect("patch should be writable");

    let transcript_path = target.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript_path)
        .expect("transcript fixture should be copied");
    let command = format!(
        "git apply --index {} && git commit -m 'Mixed provenance'",
        patch_path.display()
    );
    let pre_hook_input = codex_bash_hook_input(&target, &transcript_path, "PreToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");
    target
        .git(&["apply", "--index", patch_path.to_str().unwrap()])
        .expect("git apply --index should succeed");
    target
        .git(&["commit", "-m", "Mixed provenance"])
        .expect("commit should succeed");
    let post_hook_input = codex_bash_hook_input(&target, &transcript_path, "PostToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");
    target.sync_daemon();

    mixed.assert_committed_lines(lines![
        "human staged".unattributed_human(),
        "changed in index and worktree by AI".ai(),
    ]);
}

/// A failed/read-only apply attempt must not leave pending provenance that can
/// leak into a later commit after the Bash call has ended.
#[test]
fn test_git_apply_check_failure_does_not_attribute_later_human_commit() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("base.txt"), "base\n").expect("base should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    let mut base = target.filename("base.txt");
    base.assert_committed_lines(lines!["base".unattributed_human()]);

    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("does-not-apply.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/base.txt b/base.txt\n",
            "--- a/base.txt\n",
            "+++ b/base.txt\n",
            "@@ -1 +1 @@\n",
            "-not the current content\n",
            "+should never apply\n",
        ),
    )
    .expect("patch should be writable");
    let transcript_path = target.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript_path)
        .expect("transcript fixture should be copied");
    let command = format!("git apply --check {}", patch_path.display());
    let pre_hook_input = codex_bash_hook_input(&target, &transcript_path, "PreToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");
    assert!(
        target
            .git(&["apply", "--check", patch_path.to_str().unwrap()])
            .is_err(),
        "incompatible apply --check should fail"
    );
    let post_hook_input = codex_bash_hook_input(&target, &transcript_path, "PostToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");

    fs::write(target.path().join("later.txt"), "later human work\n")
        .expect("later file should be writable");
    target
        .stage_all_and_commit("Later human commit")
        .expect("later commit should succeed");
    let mut later = target.filename("later.txt");
    later.assert_committed_lines(lines!["later human work".unattributed_human()]);
}

/// The dominant plain worktree form is checkpointed at PostToolUse and may be
/// committed later, outside the Bash call.
#[test]
fn test_git_apply_worktree_then_later_commit_retains_ai_line() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("plain.txt"), "human base\nold value\n")
        .expect("base should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    let mut plain = target.filename("plain.txt");
    plain.assert_committed_lines(lines![
        "human base".unattributed_human(),
        "old value".unattributed_human(),
    ]);
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("plain.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/plain.txt b/plain.txt\n",
            "--- a/plain.txt\n",
            "+++ b/plain.txt\n",
            "@@ -1,2 +1,2 @@\n",
            " human base\n",
            "-old value\n",
            "+AI patched value\n",
        ),
    )
    .expect("patch should be writable");
    let transcript_path = target.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript_path)
        .expect("transcript fixture should be copied");
    let command = format!("git apply {}", patch_path.display());
    let pre_hook_input = codex_bash_hook_input(&target, &transcript_path, "PreToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &pre_hook_input])
        .expect("codex pre-hook checkpoint should succeed");
    target
        .git(&["apply", patch_path.to_str().unwrap()])
        .expect("plain git apply should succeed");
    let post_hook_input = codex_bash_hook_input(&target, &transcript_path, "PostToolUse", &command);
    target
        .git_ai(&["checkpoint", "codex", "--hook-input", &post_hook_input])
        .expect("codex post-hook checkpoint should succeed");
    target.sync_daemon();
    target
        .stage_all_and_commit("Commit applied worktree patch")
        .expect("later commit should succeed");

    plain.assert_committed_lines(lines![
        "human base".unattributed_human(),
        "AI patched value".ai()
    ]);
}

#[test]
fn test_git_apply_check_success_is_read_only() {
    let target = TestRepo::new();
    fs::write(target.path().join("checked.txt"), "old\n").expect("base should be writable");
    target
        .stage_all_and_commit("Initial target commit")
        .expect("target commit should succeed");
    let mut checked = target.filename("checked.txt");
    checked.assert_committed_lines(lines!["old".unattributed_human()]);
    let head_before = target
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD should resolve");
    let status_before = target
        .git_og(&["status", "--porcelain=v2"])
        .expect("status should succeed");
    let patch_dir = tempfile::tempdir().expect("patch tempdir should be created");
    let patch_path = patch_dir.path().join("check.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/checked.txt b/checked.txt\n",
            "--- a/checked.txt\n",
            "+++ b/checked.txt\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
        ),
    )
    .expect("patch should be writable");
    target
        .git(&["apply", "--check", patch_path.to_str().unwrap()])
        .expect("valid git apply --check should succeed");
    assert_eq!(
        target
            .git(&["rev-parse", "HEAD"])
            .expect("HEAD should resolve"),
        head_before
    );
    assert_eq!(
        target
            .git_og(&["status", "--porcelain=v2"])
            .expect("status should succeed"),
        status_before
    );
}

#[test]
fn test_git_apply_three_way_inside_bash_attributes_result() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("threeway.txt"), "base\n").unwrap();
    target.stage_all_and_commit("base").unwrap();
    let mut file = target.filename("threeway.txt");
    file.assert_committed_lines(lines!["base".unattributed_human()]);
    fs::write(target.path().join("threeway.txt"), "threeway AI\n").unwrap();
    let patch = target
        .git_og(&["diff", "--full-index", "--binary", "--", "threeway.txt"])
        .unwrap();
    target.git_og(&["restore", "threeway.txt"]).unwrap();
    let patch_dir = tempfile::tempdir().unwrap();
    let patch_path = patch_dir.path().join("threeway.patch");
    fs::write(&patch_path, patch).unwrap();
    let command = format!("git apply --3way {}", patch_path.display());
    let transcript = begin_codex_bash(&target, &command);
    target
        .git(&["apply", "--3way", patch_path.to_str().unwrap()])
        .unwrap();
    end_codex_bash(&target, &transcript, &command);
    target.git(&["commit", "-m", "threeway apply"]).unwrap();
    file.assert_committed_lines(lines!["threeway AI".ai()]);
}

#[test]
fn test_git_apply_reverse_inside_bash_attributes_restored_bytes() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("reverse.txt"), "old value\n").unwrap();
    target.stage_all_and_commit("old").unwrap();
    let mut file = target.filename("reverse.txt");
    file.assert_committed_lines(lines!["old value".unattributed_human()]);
    fs::write(target.path().join("reverse.txt"), "forward value\n").unwrap();
    let patch = target.git_og(&["diff", "--", "reverse.txt"]).unwrap();
    target.stage_all_and_commit("forward human").unwrap();
    file.assert_committed_lines(lines!["forward value".unattributed_human()]);
    let patch_dir = tempfile::tempdir().unwrap();
    let patch_path = patch_dir.path().join("reverse.patch");
    fs::write(&patch_path, patch).unwrap();
    let command = format!("git apply --reverse {}", patch_path.display());
    let transcript = begin_codex_bash(&target, &command);
    target
        .git(&["apply", "--reverse", patch_path.to_str().unwrap()])
        .unwrap();
    end_codex_bash(&target, &transcript, &command);
    target.stage_all_and_commit("reverse apply").unwrap();
    file.assert_committed_lines(lines!["old value".ai()]);
}

#[test]
fn test_git_apply_reject_partial_success_attributes_only_applied_hunk() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(
        target.path().join("reject.txt"),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
    )
    .unwrap();
    target.stage_all_and_commit("base").unwrap();
    let mut file = target.filename("reject.txt");
    file.assert_committed_lines(lines![
        "one".unattributed_human(),
        "two".unattributed_human(),
        "three".unattributed_human(),
        "four".unattributed_human(),
        "five".unattributed_human(),
        "six".unattributed_human(),
        "seven".unattributed_human(),
        "eight".unattributed_human(),
    ]);
    let patch_dir = tempfile::tempdir().unwrap();
    let patch_path = patch_dir.path().join("partial.patch");
    fs::write(
        &patch_path,
        concat!(
            "diff --git a/reject.txt b/reject.txt\n",
            "--- a/reject.txt\n",
            "+++ b/reject.txt\n",
            "@@ -1,3 +1,3 @@\n",
            " one\n",
            "-two\n",
            "+two applied AI\n",
            " three\n",
            "@@ -6,3 +6,3 @@\n",
            " six\n",
            "-not-seven\n",
            "+seven rejected AI\n",
            " eight\n",
        ),
    )
    .unwrap();
    let command = format!("git apply --reject {}", patch_path.display());
    let transcript = begin_codex_bash(&target, &command);
    assert!(
        target
            .git(&["apply", "--reject", patch_path.to_str().unwrap()])
            .is_err()
    );
    end_codex_bash(&target, &transcript, &command);
    target.git(&["add", "reject.txt"]).unwrap();
    target
        .git(&["commit", "-m", "partial reject apply"])
        .unwrap();
    file.assert_committed_lines(lines![
        "one".unattributed_human(),
        "two applied AI".ai(),
        "three".unattributed_human(),
        "four".unattributed_human(),
        "five".unattributed_human(),
        "six".unattributed_human(),
        "seven".unattributed_human(),
        "eight".unattributed_human(),
    ]);
}

#[test]
fn test_git_apply_stdin_inside_bash_attributes_result() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str())];
    let target = TestRepo::new_with_daemon_env(&env);
    fs::write(target.path().join("stdin.txt"), "old\n").unwrap();
    target.stage_all_and_commit("base").unwrap();
    let mut file = target.filename("stdin.txt");
    file.assert_committed_lines(lines!["old".unattributed_human()]);
    let patch = concat!(
        "diff --git a/stdin.txt b/stdin.txt\n",
        "--- a/stdin.txt\n",
        "+++ b/stdin.txt\n",
        "@@ -1 +1 @@\n",
        "-old\n",
        "+stdin apply AI\n",
    );
    let command = "printf patch | git apply -";
    let transcript = begin_codex_bash(&target, command);
    target
        .git_with_stdin(&["apply", "-"], patch.as_bytes())
        .unwrap();
    end_codex_bash(&target, &transcript, command);
    target.stage_all_and_commit("stdin apply").unwrap();
    file.assert_committed_lines(lines!["stdin apply AI".ai()]);
}
