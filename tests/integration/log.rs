use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::config::{NotesBackendConfig, NotesBackendKind};
use git_ai::model::repository::notes_db::NotesDatabase;

#[test]
fn log_default_shows_stats_without_raw_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.txt");
    file.set_contents(lines!["AI-authored line".ai()]);
    repo.stage_all_and_commit("feat: add ai line").unwrap();

    let output = repo
        .git_ai(&["log", "--no-pager", "-n", "1"])
        .expect("git-ai log should succeed");

    assert!(output.contains("feat: add ai line"), "output:\n{}", output);
    assert!(output.contains("Git AI stats:"), "output:\n{}", output);
    assert!(output.contains("you"), "output:\n{}", output);
    assert!(output.contains("ai"), "output:\n{}", output);
    assert!(
        !output.contains("schema_version"),
        "raw note data should be hidden by default:\n{}",
        output
    );
}

#[test]
fn log_raw_shows_authorship_note() {
    // This regression test needs attribution collection to be deterministic.
    // Give it a dedicated daemon so another parallel TestRepo cannot rewrite
    // the daemon's home config, and allow the exact repo/worktree it created.
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.allow_only_self_for_collection();

    let mut file = repo.filename("raw.txt");
    file.set_contents(lines!["AI raw line".ai()]);
    repo.stage_all_and_commit("feat: raw note").unwrap();

    let output = repo
        .git_ai(&["log", "--no-pager", "--raw", "-n", "1"])
        .expect("git-ai log --raw should succeed");

    assert!(output.contains("Git AI stats:"), "output:\n{}", output);
    assert!(output.contains("Authorship note:"), "output:\n{}", output);
    assert!(
        output.contains("schema_version"),
        "raw note content should be visible:\n{}",
        output
    );
}

#[test]
fn log_plain_proxies_git_notes_backend() {
    let repo = TestRepo::new();
    let mut file = repo.filename("plain.txt");
    file.set_contents(lines!["AI plain line".ai()]);
    repo.stage_all_and_commit("feat: plain note").unwrap();

    let output = repo
        .git_ai(&["log", "--no-pager", "--plain", "-n", "1"])
        .expect("git-ai log --plain should proxy git log");

    assert!(output.contains("feat: plain note"), "output:\n{}", output);
    assert!(
        output.contains("Notes (ai):"),
        "plain output should use git's notes renderer:\n{}",
        output
    );
    assert!(
        output.contains("schema_version"),
        "plain output should include raw git note content:\n{}",
        output
    );
    assert!(
        !output.contains("Git AI stats:"),
        "plain output should not use git-ai's renderer:\n{}",
        output
    );
}

#[test]
fn log_multiple_commits_parse_record_boundaries() {
    // This test needs stable attribution across two commits. A shared daemon
    // can have its config rewritten by another test between those commits,
    // which makes the second commit legitimately produce no note.
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.allow_only_self_for_collection();
    let mut file = repo.filename("history.txt");

    file.set_contents(lines!["first ai line".ai()]);
    repo.stage_all_and_commit("feat: first").unwrap();

    file.set_contents(lines!["first ai line".ai(), "second ai line".ai()]);
    repo.stage_all_and_commit("feat: second").unwrap();

    let output = repo
        .git_ai(&["log", "--no-pager", "-n", "2"])
        .expect("git-ai log should parse multiple commits");

    assert!(output.contains("feat: second"), "output:\n{}", output);
    assert!(output.contains("feat: first"), "output:\n{}", output);
    assert_eq!(
        output.matches("Git AI stats:").count(),
        2,
        "output:\n{}",
        output
    );
    assert!(
        !output.contains("stats unavailable"),
        "both commits should have stats:\n{}",
        output
    );
}

#[test]
fn log_commit_body_is_separated_from_subject() {
    let repo = TestRepo::new();
    let mut file = repo.filename("body.txt");
    file.set_contents(lines!["AI body line".ai()]);
    repo.stage_all_and_commit("feat: subject\n\nBody paragraph")
        .unwrap();

    let output = repo
        .git_ai(&["log", "--no-pager", "-n", "1"])
        .expect("git-ai log should render commit body");

    assert!(
        output.contains("    feat: subject\n\n    Body paragraph"),
        "body should be separated from subject:\n{}",
        output
    );
}

#[test]
fn log_plain_rejects_http_backend() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    let err = repo
        .git_ai(&["log", "--no-pager", "--plain", "-n", "1"])
        .expect_err("git-ai log --plain should reject HTTP notes backend");

    assert!(
        err.contains("plain git log --notes=ai only supports the git_notes backend"),
        "error:\n{}",
        err
    );
}

#[test]
fn log_http_backend_reads_notes_db_without_git_notes_ref() {
    let mut repo = TestRepo::new();
    let mut file = repo.filename("http.txt");
    file.set_contents(lines!["AI http line".ai()]);
    repo.stage_all_and_commit("feat: http note").unwrap();

    let sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    let note = repo
        .read_authorship_note(&sha)
        .expect("authorship note should exist before deleting refs/notes/ai");

    repo.git_og(&["update-ref", "-d", "refs/notes/ai"])
        .expect("delete refs/notes/ai");
    assert!(
        repo.read_authorship_note(&sha).is_none(),
        "precondition failed: refs/notes/ai should be absent"
    );

    let notes_db_path = repo.test_home_path().join("http-log-notes.db");
    let mut db = NotesDatabase::open_at_path(&notes_db_path).expect("open notes db");
    db.cache_synced_notes(&[(sha.clone(), note)])
        .expect("seed notes db");

    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    let notes_db_path_string = notes_db_path.to_string_lossy().to_string();
    let output = repo
        .git_ai_with_env(
            &["log", "--no-pager", "--notes", "-n", "1"],
            &[("GIT_AI_TEST_NOTES_DB_PATH", notes_db_path_string.as_str())],
        )
        .expect("git-ai log should read HTTP notes cache");

    assert!(output.contains("feat: http note"), "output:\n{}", output);
    assert!(output.contains("Git AI stats:"), "output:\n{}", output);
    assert!(output.contains("Authorship note:"), "output:\n{}", output);
    assert!(
        output.contains("schema_version"),
        "raw note content should come from notes-db:\n{}",
        output
    );
}

crate::reuse_tests_in_worktree!(log_raw_shows_authorship_note,);
