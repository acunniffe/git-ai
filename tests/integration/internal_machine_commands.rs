use crate::repos::test_repo::{NewCommit, TestRepo, real_git_executable};
use serde_json::json;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn test_effective_ignore_patterns_internal_command_json() {
    let repo = TestRepo::new();

    fs::write(
        repo.path().join(".gitattributes"),
        "generated/** linguist-generated=true\n",
    )
    .expect("should write .gitattributes");
    fs::write(repo.path().join(".git-ai-ignore"), "custom/**\n")
        .expect("should write .git-ai-ignore");
    fs::write(repo.path().join("README.md"), "# repo\n").expect("should write README");
    repo.stage_all_and_commit("initial")
        .expect("initial commit");

    let request = json!({
        "user_patterns": ["user/**", "generated/**"],
        "extra_patterns": ["extra/**", "custom/**"]
    })
    .to_string();

    let output = repo
        .git_ai(&["effective-ignore-patterns", "--json", &request])
        .expect("internal command should succeed");
    let parsed: serde_json::Value = serde_json::from_str(output.trim()).expect("valid JSON output");

    let patterns = parsed["patterns"]
        .as_array()
        .expect("patterns should be an array")
        .iter()
        .map(|v| v.as_str().expect("pattern should be a string"))
        .collect::<Vec<_>>();

    assert!(patterns.contains(&"*.lock"));
    assert!(patterns.contains(&"generated/**"));
    assert!(patterns.contains(&"custom/**"));
    assert!(patterns.contains(&"extra/**"));
    assert!(patterns.contains(&"user/**"));

    let generated_count = patterns
        .iter()
        .filter(|pattern| **pattern == "generated/**")
        .count();
    assert_eq!(generated_count, 1);
}

#[test]
fn test_blame_analysis_internal_command_json() {
    let repo = TestRepo::new();

    fs::write(repo.path().join("analysis.txt"), "line1\nline2\nline3\n")
        .expect("should write analysis file");
    repo.stage_all_and_commit("initial")
        .expect("initial commit");

    let request = json!({
        "file_path": "analysis.txt",
        "options": {
            "line_ranges": [[2, 3]],
            "return_human_authors_as_human": true,
            "split_hunks_by_ai_author": false
        }
    })
    .to_string();

    let output = repo
        .git_ai(&["blame-analysis", "--json", &request])
        .expect("internal command should succeed");
    let parsed: serde_json::Value = serde_json::from_str(output.trim()).expect("valid JSON output");

    let line_authors = parsed["line_authors"]
        .as_object()
        .expect("line_authors should be an object");
    assert_eq!(line_authors.len(), 2);
    assert_eq!(
        line_authors.get("2").and_then(|v| v.as_str()),
        Some("human")
    );
    assert_eq!(
        line_authors.get("3").and_then(|v| v.as_str()),
        Some("human")
    );

    assert!(
        parsed["prompt_records"]
            .as_object()
            .expect("prompt_records should be object")
            .is_empty()
    );
    assert!(
        !parsed["blame_hunks"]
            .as_array()
            .expect("blame_hunks should be array")
            .is_empty()
    );
}

#[test]
fn test_internal_machine_commands_emit_json_errors() {
    let repo = TestRepo::new();

    let err = repo
        .git_ai(&["effective-ignore-patterns"])
        .expect_err("missing --json payload should fail");

    let parsed: serde_json::Value = serde_json::from_str(err.trim()).expect("error should be JSON");
    assert!(parsed["error"].as_str().is_some());
}

#[test]
fn test_fetch_and_push_authorship_notes_internal_commands_json() {
    let (mirror, _upstream) = TestRepo::new_with_remote();

    fs::write(mirror.path().join("sync.txt"), "sync authorship notes\n")
        .expect("should write sync file");
    mirror
        .stage_all_and_commit("create note source")
        .expect("commit should succeed");

    let request = json!({
        "remote_name": "origin"
    })
    .to_string();

    let fetch_before = mirror
        .git_ai(&["fetch-authorship-notes", "--json", &request])
        .expect("fetch command should succeed");
    let fetch_before_json: serde_json::Value =
        serde_json::from_str(fetch_before.trim()).expect("fetch output should be JSON");
    assert_eq!(fetch_before_json["notes_existence"], "not_found");

    let push_output = mirror
        .git_ai(&["push-authorship-notes", "--json", &request])
        .expect("push command should succeed");
    let push_json: serde_json::Value =
        serde_json::from_str(push_output.trim()).expect("push output should be JSON");
    assert_eq!(push_json["ok"], true);

    let fetch_after = mirror
        .git_ai(&["fetch_authorship_notes", "--json", &request])
        .expect("fetch alias command should succeed");
    let fetch_after_json: serde_json::Value =
        serde_json::from_str(fetch_after.trim()).expect("fetch output should be JSON");
    assert_eq!(fetch_after_json["notes_existence"], "found");
}

#[test]
fn test_fetch_authorship_notes_fails_when_local_notes_ref_cannot_update() {
    let (mirror, _upstream) = TestRepo::new_with_remote();

    fs::write(mirror.path().join("locked-notes.txt"), "locked notes\n")
        .expect("should write locked notes file");
    let commit = mirror
        .stage_all_and_commit("create remote note source")
        .expect("commit should succeed");

    mirror
        .git_og(&["push", "-u", "origin", "HEAD"])
        .expect("branch push should succeed");
    mirror
        .git_og(&["push", "origin", "refs/notes/ai"])
        .expect("notes push should succeed");

    mirror
        .git_og(&["update-ref", "-d", "refs/notes/ai"])
        .expect("local note ref should be removable");
    assert!(
        mirror.read_authorship_note(&commit.commit_sha).is_none(),
        "local note should be absent before fetch"
    );

    let notes_dir = mirror.path().join(".git/refs/notes");
    fs::create_dir_all(&notes_dir).expect("notes dir should be creatable");
    fs::write(notes_dir.join("ai.lock"), "stale lock\n").expect("notes lock should be writable");

    let request = json!({
        "remote_name": "origin"
    })
    .to_string();
    let err = mirror
        .git_ai(&["fetch-authorship-notes", "--json", &request])
        .expect_err("fetch should fail when refs/notes/ai cannot be updated");
    let parsed: serde_json::Value = serde_json::from_str(err.trim()).expect("error should be JSON");
    let error = parsed["error"].as_str().expect("error should be a string");
    assert!(
        error.contains("fetch_authorship_notes failed"),
        "unexpected error output: {}",
        err
    );
}

/// Helper to run a raw git command with stdin piped, returning trimmed stdout.
fn git_plumbing(repo_path: &std::path::Path, args: &[&str], stdin_data: Option<&[u8]>) -> String {
    let git = real_git_executable();
    let mut cmd = Command::new(git);
    cmd.arg("-C")
        .arg(repo_path)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("user.name=Test")
        .arg("-c")
        .arg("user.email=test@test.com")
        .args(args);
    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("failed to spawn git plumbing command");

    if let Some(data) = stdin_data {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(data)
            .expect("failed to write stdin");
    }

    let output = child.wait_with_output().expect("failed to wait for git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("non-utf8 git output")
        .trim()
        .to_string()
}

/// Shared fixture for the concurrent notes-pusher tests: mirror makes an
/// initial commit (note auto-created by hook) and pushes the branch, the bare
/// upstream's refs/notes/ai is seeded from mirror, and a second clone builds
/// its own note on top of the current remote notes and pushes that tip to
/// upstream at `notes_push_refspec`. The clone is deleted before returning.
/// Returns mirror's initial commit and the sha of the clone's annotated
/// commit.
fn setup_concurrent_notes_pusher(
    mirror: &TestRepo,
    upstream: &TestRepo,
    clone_suffix: &str,
    notes_push_refspec: &str,
) -> (NewCommit, String) {
    // Initial commit (note auto-created by hook) and branch push.
    fs::write(mirror.path().join("a.txt"), "a\n").expect("write a");
    let commit1 = mirror
        .stage_all_and_commit("first commit")
        .expect("commit1");
    mirror
        .git_og(&["push", "origin", "main"])
        .expect("setup branch push should succeed");

    // Ensure mirror's initial notes are present on upstream. The preceding
    // branch push can already push authorship notes through the normal push
    // path, so set the bare fixture ref directly instead of racing remote
    // receive policy during test setup.
    git_plumbing(
        upstream.path(),
        &[
            "fetch",
            mirror.path().to_str().unwrap(),
            "+refs/notes/ai:refs/notes/ai",
        ],
        None,
    );

    // A second clone simulates the concurrent pusher. Its committer identity
    // comes from git_plumbing's -c flags, so no repo config is needed.
    let clone2_path = mirror.path().with_extension(clone_suffix);
    let _ = fs::remove_dir_all(&clone2_path);
    git_plumbing(
        mirror.path(),
        &[
            "clone",
            upstream.path().to_str().unwrap(),
            clone2_path.to_str().unwrap(),
        ],
        None,
    );
    git_plumbing(
        &clone2_path,
        &["fetch", "origin", "+refs/notes/ai:refs/notes/ai"],
        None,
    );

    // The clone makes a commit, annotates it on top of the fetched remote
    // notes, and pushes that notes tip to `notes_push_refspec` on upstream.
    fs::write(clone2_path.join("b.txt"), "b\n").expect("write b");
    git_plumbing(&clone2_path, &["add", "b.txt"], None);
    git_plumbing(&clone2_path, &["commit", "-m", "other commit"], None);
    let other_sha = git_plumbing(&clone2_path, &["rev-parse", "HEAD"], None);
    git_plumbing(
        &clone2_path,
        &[
            "notes",
            "--ref=ai",
            "add",
            "-m",
            r#"{"author":"other"}"#,
            &other_sha,
        ],
        None,
    );
    git_plumbing(&clone2_path, &["push", "origin", notes_push_refspec], None);

    // The pushed objects now live on upstream; drop the clone so test runs do
    // not leak sibling directories in the system temp dir.
    fs::remove_dir_all(&clone2_path).expect("remove clone2");

    (commit1, other_sha)
}

/// Reproduces a bug where `git notes merge -s ours` crashes with:
///   Assertion failed: (is_null_oid(&mp->remote)), function diff_tree_remote,
///   file notes-merge.c, line 170.
///
/// This happens when the remote notes tree has mixed fanout — both a flat blob
/// entry (e.g. `aabbccdd…`) AND a subtree entry (e.g. `aa/bbccdd…`) for the
/// same annotated object. The fallback merge should handle this gracefully and
/// the push should succeed.
#[test]
fn test_push_authorship_notes_survives_corrupted_remote_notes_tree() {
    let (mirror, upstream) = TestRepo::new_with_remote();

    // 1. Create a commit on mirror and push it
    fs::write(mirror.path().join("test.txt"), "hello\n").expect("write file");
    let commit = mirror
        .stage_all_and_commit("initial commit")
        .expect("commit should succeed");
    mirror
        .git_og(&["push", "origin", "main"])
        .expect("setup branch push should succeed");
    let commit_sha = commit.commit_sha;

    // 2. Create a corrupted notes tree on upstream with NO common merge base
    //    relative to the mirror's local notes ref. This is the key condition that
    //    triggers the assertion in git's notes-merge.c: when there's no merge base,
    //    git uses an empty tree as the base, and the diff against the corrupted remote
    //    tree encounters the same annotated object twice (once flat, once fanout).
    let prefix = &commit_sha[..2];
    let rest = &commit_sha[2..];

    // Create a note blob on upstream (independent of mirror's notes)
    let note_blob = git_plumbing(
        upstream.path(),
        &["hash-object", "-w", "--stdin"],
        Some(br#"{"author":"remote"}"#),
    );

    // Build inner tree (fanout: prefix/rest -> blob)
    let inner_tree_input = format!("100644 blob {}\t{}\n", note_blob, rest);
    let inner_tree = git_plumbing(
        upstream.path(),
        &["mktree"],
        Some(inner_tree_input.as_bytes()),
    );

    // Build mixed tree: flat entry + subtree entry for same commit
    let mixed_tree_input = format!(
        "100644 blob {}\t{}\n040000 tree {}\t{}\n",
        note_blob, commit_sha, inner_tree, prefix
    );
    let mixed_tree = git_plumbing(
        upstream.path(),
        &["mktree"],
        Some(mixed_tree_input.as_bytes()),
    );

    // Create a root commit (NO parent) — this ensures no common merge base
    // with the mirror's notes ref, which is what triggers the assertion.
    let corrupted_commit = git_plumbing(
        upstream.path(),
        &[
            "commit-tree",
            &mixed_tree,
            "-m",
            "corrupted notes tree (orphan)",
        ],
        None,
    );
    git_plumbing(
        upstream.path(),
        &["update-ref", "refs/notes/ai", &corrupted_commit],
        None,
    );

    // 4. Add a new local note on mirror (so local and remote have diverged).
    //    The git-ai commit hook automatically creates notes, so just making
    //    a new commit is sufficient.
    fs::write(mirror.path().join("test2.txt"), "world\n").expect("write file2");
    let commit2 = mirror
        .stage_all_and_commit("second commit")
        .expect("second commit should succeed");

    // 5. Push authorship notes — this triggers fetch + merge + push.
    //    Without the fallback fix, the merge crashes and the push fails
    //    with "non-fast-forward".
    let request = json!({"remote_name": "origin"}).to_string();
    let push_output = mirror
        .git_ai(&["push-authorship-notes", "--json", &request])
        .expect("push-authorship-notes should succeed despite corrupted remote tree");
    let push_json: serde_json::Value =
        serde_json::from_str(push_output.trim()).expect("push output should be JSON");
    assert_eq!(
        push_json["ok"],
        true,
        "push should succeed via fallback merge, got: {}",
        push_output.trim()
    );

    // 6. Verify both notes are present on upstream after push
    let notes_list = git_plumbing(upstream.path(), &["notes", "--ref=ai", "list"], None);
    assert!(
        notes_list.contains(&commit_sha),
        "upstream should have note for first commit"
    );
    assert!(
        notes_list.contains(&commit2.commit_sha),
        "upstream should have note for second commit"
    );
}

/// Simulates the race condition on busy monorepos where another developer
/// pushes notes between our fetch-merge and push steps, causing a
/// non-fast-forward rejection. The retry loop should re-fetch, re-merge,
/// and push successfully.
#[test]
fn test_push_authorship_notes_retries_on_concurrent_push() {
    let (mirror, upstream) = TestRepo::new_with_remote();

    // 1. The concurrent pusher advances remote refs/notes/ai beyond what
    //    mirror has fetched.
    let (commit1, other_sha) = setup_concurrent_notes_pusher(
        &mirror,
        &upstream,
        "concurrent-clone",
        "refs/notes/ai:refs/notes/ai",
    );

    // 2. Mirror makes another commit (notes auto-created by hook).
    //    Mirror's local refs/notes/ai is now behind remote.
    fs::write(mirror.path().join("c.txt"), "c\n").expect("write c");
    let _commit3 = mirror
        .stage_all_and_commit("mirror commit")
        .expect("commit3");

    // 3. Push authorship notes. The retry loop should:
    //    - Attempt 1: fetch, merge, push → fails (non-fast-forward if
    //      remote was updated between merge and push, or succeeds on first try)
    //    - Attempt 2+: re-fetch, re-merge, push → succeeds
    //    In this test, the remote is already ahead, so the first attempt's
    //    fetch+merge will incorporate the other clone's notes, and push succeeds.
    let request = json!({"remote_name": "origin"}).to_string();
    let push_output = mirror
        .git_ai(&["push-authorship-notes", "--json", &request])
        .expect("push-authorship-notes should succeed after retry");
    let push_json: serde_json::Value =
        serde_json::from_str(push_output.trim()).expect("push output should be JSON");
    assert_eq!(
        push_json["ok"],
        true,
        "push should eventually succeed, got: {}",
        push_output.trim()
    );

    // 4. Verify all notes are present on upstream
    let notes_list = git_plumbing(upstream.path(), &["notes", "--ref=ai", "list"], None);
    assert!(
        notes_list.contains(&commit1.commit_sha),
        "upstream should have note for mirror's first commit"
    );
    assert!(
        notes_list.contains(&other_sha),
        "upstream should have note from concurrent pusher"
    );
}

/// Reproduces the production race where another user's notes push lands on the
/// remote *between* our fetch-merge and push. receive-pack's compare-and-swap
/// then rejects our push with a `[remote rejected]` line (git prints
/// `incorrect old value provided` or `failed to update ref` depending on
/// version) — none of which contain "non-fast-forward". The retry loop must
/// treat this as retryable, re-fetch, re-merge, and succeed on the next
/// attempt.
///
/// The race is injected deterministically with an `update` hook on the bare
/// upstream that advances refs/notes/ai (to a staged concurrent tip) exactly
/// once, mid-push, before the ref update is committed.
#[cfg(unix)]
#[test]
fn test_push_authorship_notes_retries_when_remote_notes_advance_mid_push() {
    let (mirror, upstream) = TestRepo::new_with_remote();

    // 1. The concurrent pusher's tip is staged on upstream as
    //    refs/notes/pending-ai: its objects are uploaded, but refs/notes/ai
    //    has not moved yet.
    let (commit1, other_sha) = setup_concurrent_notes_pusher(
        &mirror,
        &upstream,
        "mid-push-clone",
        "refs/notes/ai:refs/notes/pending-ai",
    );

    // 2. Upstream update hook: the first time refs/notes/ai is pushed, land
    // the concurrent pusher's tip before the pushed update is committed, so
    // the compare-and-swap fails. Hooks run with cwd = the bare repo dir. The
    // marker file is created only after the ref actually moved, so the race
    // assertion below cannot pass if the injection failed.
    let hook_path = upstream.path().join("hooks").join("update");
    fs::create_dir_all(hook_path.parent().unwrap()).expect("hooks dir");
    fs::write(
        &hook_path,
        "#!/bin/sh\n\
         if [ \"$1\" = \"refs/notes/ai\" ] && [ ! -f mid-push-race-done ]; then\n\
           git update-ref refs/notes/ai \"$(git rev-parse --verify refs/notes/pending-ai)\" \\\n\
             && : > mid-push-race-done\n\
         fi\n\
         exit 0\n",
    )
    .expect("write update hook");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))
            .expect("update hook should be executable");
    }

    // 3. Mirror makes another commit and pushes authorship notes. Attempt 1
    // fetch-merges the pre-race remote state and gets remote-rejected; the
    // retry must pick up the advanced ref, merge, and push successfully.
    fs::write(mirror.path().join("c.txt"), "c\n").expect("write c");
    let commit2 = mirror
        .stage_all_and_commit("mirror commit")
        .expect("commit2");

    let request = json!({"remote_name": "origin"}).to_string();
    let push_output = mirror
        .git_ai(&["push-authorship-notes", "--json", &request])
        .expect("push-authorship-notes should succeed by retrying the rejected push");
    let push_json: serde_json::Value =
        serde_json::from_str(push_output.trim()).expect("push output should be JSON");
    assert_eq!(
        push_json["ok"],
        true,
        "push should succeed after retry, got: {}",
        push_output.trim()
    );

    // The injected race must actually have fired, otherwise this test did not
    // exercise the retry path.
    assert!(
        upstream.path().join("mid-push-race-done").exists(),
        "update hook should have advanced refs/notes/ai mid-push"
    );

    // 4. Upstream ends up with the concurrent pusher's note AND both of
    // mirror's notes.
    let notes_list = git_plumbing(upstream.path(), &["notes", "--ref=ai", "list"], None);
    assert!(
        notes_list.contains(&commit1.commit_sha),
        "upstream should have note for mirror's first commit"
    );
    assert!(
        notes_list.contains(&other_sha),
        "upstream should have note from concurrent pusher"
    );
    assert!(
        notes_list.contains(&commit2.commit_sha),
        "upstream should have note for mirror's second commit"
    );
}
