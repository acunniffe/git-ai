// Regression tests for rebase note-shifting under the HTTP notes backend.
//
// Under `notes_backend.kind = "http"`, authorship notes live in the local
// notes-db (and the Weave HTTP backend) — `refs/notes/ai` is never published
// to the git remote. Before shifting notes onto rebased commits, the rewrite
// path calls `fetch_missing_notes_for_commits` for the rebase's source
// commits. When any source commit has no note anywhere (e.g. a commit created
// via `commit-tree` + `update-ref` plumbing, or the output of an earlier
// failed shift), that function falls back to `git fetch origin
// +refs/notes/ai:...` — a ref that never exists on the remote for HTTP-backend
// deployments. That fetch failing (off-VPN, broken SSH, unreachable remote)
// must not abort the shift: source commits whose notes ARE in the local
// notes-db still need their notes migrated onto the rewritten commits.

use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::config::{ConfigPatch, NotesBackendConfig, NotesBackendKind};
use git_ai::notes::db::NotesDatabase;
use std::fs;
use std::path::Path;

/// Build a TestRepo whose daemon and CLI both run with the HTTP notes backend,
/// with the daemon's notes-db at an isolated, test-readable path.
fn new_http_backend_repo(notes_db_path: &Path) -> TestRepo {
    // The daemon owns note writes and the rewrite shift, so the DAEMON must run
    // with the HTTP backend. The test-home config.json writer does not cover
    // notes_backend and the daemon caches config at startup, so pass the patch
    // via env at daemon spawn.
    let daemon_patch = ConfigPatch {
        exclude_prompts_in_repositories: Some(vec![]),
        prompt_storage: Some("notes".to_string()),
        notes_backend: Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        }),
        ..Default::default()
    };
    let daemon_patch_json =
        serde_json::to_string(&daemon_patch).expect("serialize daemon config patch");
    let notes_db_path_string = notes_db_path.to_string_lossy().to_string();
    let mut repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_TEST_CONFIG_PATCH", daemon_patch_json.as_str()),
        ("GIT_AI_TEST_NOTES_DB_PATH", notes_db_path_string.as_str()),
    ]);
    // CLI invocations (checkpoint) should use the HTTP backend too.
    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });
    repo
}

/// Poll the notes-db for a commit's note: HTTP-backend note writes land in the
/// daemon's notes-db (never refs/notes/ai), so the harness's usual
/// "note visible in refs/notes/ai" assertions cannot be used here.
fn read_note_from_db(notes_db_path: &Path, sha: &str) -> Option<String> {
    for _ in 0..100 {
        if let Ok(db) = NotesDatabase::open_at_path(notes_db_path)
            && let Ok(Some(content)) = db.get_note(sha)
        {
            return Some(content);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

/// Shared setup: a `feature` branch holding
///   A: an AI-attributed commit whose note is in the local notes-db, then
///   B: a commit created via `commit-tree` + `update-ref` plumbing
///      (Graphite-style), which never gets an authorship note,
/// with `main` advanced by one commit so `git rebase main` rewrites both.
/// Returns (sha_a, sha_b).
fn setup_feature_branch_with_noted_and_noteless_commits(
    repo: &TestRepo,
    notes_db_path: &Path,
) -> (String, String) {
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit A: AI-attributed, note written to the notes-db by the daemon.
    let file_path = repo.path().join("feature.txt");
    fs::write(&file_path, "Human line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "feature.txt"])
        .unwrap();
    fs::write(&file_path, "Human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "AI commit"]).unwrap();
    repo.sync_daemon();
    let sha_a = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    let note_a = read_note_from_db(notes_db_path, &sha_a)
        .expect("AI commit should have a note in the notes-db");
    let log_a = AuthorshipLog::deserialize_from_string(&note_a).expect("parse AI commit note");
    assert!(
        !log_a.attestations.is_empty(),
        "AI commit note should carry attestations"
    );

    // Commit B: created with plumbing (`commit-tree` + `update-ref`), the way
    // Graphite restacks create commits. No CommitCreated flows through the
    // daemon, so B has no authorship note anywhere.
    fs::write(repo.path().join("plumbing.txt"), "plumbing content\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    let tree = repo.git(&["write-tree"]).unwrap().trim().to_string();
    let sha_b = repo
        .git(&["commit-tree", &tree, "-p", &sha_a, "-m", "plumbing commit"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["update-ref", "refs/heads/feature", &sha_b, &sha_a])
        .unwrap();
    repo.sync_daemon();
    assert!(
        NotesDatabase::open_at_path(notes_db_path)
            .unwrap()
            .get_note(&sha_b)
            .unwrap()
            .is_none(),
        "plumbing commit should have no note (it is the noteless rebase source)"
    );

    // Advance main so the rebase rewrites both feature commits.
    repo.git(&["checkout", "main"]).unwrap();
    fs::write(repo.path().join("main.txt"), "main change\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "advance main"]).unwrap();
    repo.git(&["checkout", "feature"]).unwrap();
    repo.sync_daemon();

    (sha_a, sha_b)
}

/// Regression test for a production note-loss cascade (Weave, Jul 2026): one
/// noteless rebase source plus a failing notes fetch made
/// `fetch_missing_notes_for_commits` return Err, and the `?` in the rewrite
/// handlers aborted the whole shift — so the rebased counterpart of a commit
/// whose note IS in the local notes-db ended up with no note at all. The
/// fetch is best-effort now; locally available notes must survive.
#[test]
fn test_rebase_preserves_local_notes_when_source_note_fetch_fails() {
    let notes_db_dir = tempfile::tempdir().expect("create isolated notes-db directory");
    let notes_db_path = notes_db_dir.path().join("notes-db");
    let repo = new_http_backend_repo(&notes_db_path);

    let (sha_a, _sha_b) =
        setup_feature_branch_with_noted_and_noteless_commits(&repo, &notes_db_path);

    // Unreachable origin: the notes fetch for the noteless source B will fail
    // hard (models off-VPN / broken GitHub SSH in production).
    let missing_remote = repo.path().join("no-such-remote");
    repo.git(&["remote", "add", "origin", missing_remote.to_str().unwrap()])
        .unwrap();

    repo.git(&["rebase", "main"]).unwrap();
    // On regression this sync panics with the daemon-side error "failed to
    // fetch authorship notes for source commits [<B>]: ... exit code 128" —
    // the rewrite side effect aborting instead of shifting best-effort.
    repo.sync_daemon();

    let rebased_a = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(rebased_a, sha_a, "rebase should rewrite the AI commit");

    let rebased_note = read_note_from_db(&notes_db_path, &rebased_a).expect(
        "rebased AI commit should keep its authorship note even though the notes fetch \
         for an unrelated noteless source commit failed",
    );
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    assert!(
        !rebased_log.attestations.is_empty(),
        "rebased AI commit note should still carry attestations"
    );
}

/// Control: identical scenario but with no remote configured. With no remote
/// there is no fetch to fail, so the shift proceeds and the noted commit's
/// note survives the rebase — proving the loss above is caused specifically
/// by the failed notes fetch aborting the shift.
#[test]
fn test_rebase_preserves_local_notes_with_noteless_sibling_and_no_remote() {
    let notes_db_dir = tempfile::tempdir().expect("create isolated notes-db directory");
    let notes_db_path = notes_db_dir.path().join("notes-db");
    let repo = new_http_backend_repo(&notes_db_path);

    let (sha_a, _sha_b) =
        setup_feature_branch_with_noted_and_noteless_commits(&repo, &notes_db_path);

    repo.git(&["rebase", "main"]).unwrap();
    repo.sync_daemon();

    let rebased_a = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(rebased_a, sha_a, "rebase should rewrite the AI commit");

    let rebased_note = read_note_from_db(&notes_db_path, &rebased_a)
        .expect("rebased AI commit should keep its authorship note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    assert!(
        !rebased_log.attestations.is_empty(),
        "rebased AI commit note should still carry attestations"
    );
}
