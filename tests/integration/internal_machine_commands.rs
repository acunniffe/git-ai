use crate::repos::test_repo::{TestRepo, run_raw_git_plumbing};
use serde_json::json;
use std::fs;

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
    let note_blob = run_raw_git_plumbing(
        upstream.path(),
        &["hash-object", "-w", "--stdin"],
        Some(br#"{"author":"remote"}"#),
    );

    // Build inner tree (fanout: prefix/rest -> blob)
    let inner_tree_input = format!("100644 blob {}\t{}\n", note_blob, rest);
    let inner_tree = run_raw_git_plumbing(
        upstream.path(),
        &["mktree"],
        Some(inner_tree_input.as_bytes()),
    );

    // Build mixed tree: flat entry + subtree entry for same commit
    let mixed_tree_input = format!(
        "100644 blob {}\t{}\n040000 tree {}\t{}\n",
        note_blob, commit_sha, inner_tree, prefix
    );
    let mixed_tree = run_raw_git_plumbing(
        upstream.path(),
        &["mktree"],
        Some(mixed_tree_input.as_bytes()),
    );

    // Create a root commit (NO parent) — this ensures no common merge base
    // with the mirror's notes ref, which is what triggers the assertion.
    let corrupted_commit = run_raw_git_plumbing(
        upstream.path(),
        &[
            "commit-tree",
            &mixed_tree,
            "-m",
            "corrupted notes tree (orphan)",
        ],
        None,
    );
    run_raw_git_plumbing(
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
    let notes_list = run_raw_git_plumbing(upstream.path(), &["notes", "--ref=ai", "list"], None);
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

    // 1. Create initial commit and push
    fs::write(mirror.path().join("a.txt"), "a\n").expect("write a");
    let commit1 = mirror
        .stage_all_and_commit("first commit")
        .expect("commit1");
    mirror
        .git_og(&["push", "origin", "main"])
        .expect("setup branch push should succeed");

    // 2. Ensure mirror's initial notes are present on upstream. The preceding
    // branch push can already push authorship notes through the normal push
    // path, so set the bare fixture ref directly instead of racing remote
    // receive policy during test setup.
    run_raw_git_plumbing(
        upstream.path(),
        &[
            "fetch",
            mirror.path().to_str().unwrap(),
            "+refs/notes/ai:refs/notes/ai",
        ],
        None,
    );

    // 3. Create a second clone that simulates the concurrent pusher
    let clone2_path = mirror.path().with_extension("concurrent-clone");
    let _ = fs::remove_dir_all(&clone2_path);
    run_raw_git_plumbing(
        mirror.path(),
        &[
            "clone",
            upstream.path().to_str().unwrap(),
            clone2_path.to_str().unwrap(),
        ],
        None,
    );
    // Configure clone2 and fetch notes
    run_raw_git_plumbing(
        &clone2_path,
        &["config", "user.email", "other@test.com"],
        None,
    );
    run_raw_git_plumbing(&clone2_path, &["config", "user.name", "Other"], None);
    run_raw_git_plumbing(
        &clone2_path,
        &["fetch", "origin", "+refs/notes/ai:refs/notes/ai"],
        None,
    );

    // 4. Other clone makes a commit with a note and pushes notes to upstream.
    //    This advances remote refs/notes/ai beyond what mirror has fetched.
    fs::write(clone2_path.join("b.txt"), "b\n").expect("write b");
    run_raw_git_plumbing(&clone2_path, &["add", "b.txt"], None);
    run_raw_git_plumbing(&clone2_path, &["commit", "-m", "other commit"], None);
    let other_sha = run_raw_git_plumbing(&clone2_path, &["rev-parse", "HEAD"], None);
    run_raw_git_plumbing(
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
    run_raw_git_plumbing(
        &clone2_path,
        &["push", "origin", "refs/notes/ai:refs/notes/ai"],
        None,
    );

    // 5. Mirror makes another commit (notes auto-created by hook).
    //    Mirror's local refs/notes/ai is now behind remote.
    fs::write(mirror.path().join("c.txt"), "c\n").expect("write c");
    let _commit3 = mirror
        .stage_all_and_commit("mirror commit")
        .expect("commit3");

    // 6. Push authorship notes. The retry loop should:
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

    // 7. Verify all notes are present on upstream
    let notes_list = run_raw_git_plumbing(upstream.path(), &["notes", "--ref=ai", "list"], None);
    assert!(
        notes_list.contains(&commit1.commit_sha),
        "upstream should have note for mirror's first commit"
    );
    assert!(
        notes_list.contains(&other_sha),
        "upstream should have note from concurrent pusher"
    );
}
