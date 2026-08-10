use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::config::{NotesBackendConfig, NotesBackendKind};
use git_ai::notes::db::NotesDatabase;
use std::fs;

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
    let repo = TestRepo::new();
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
    let repo = TestRepo::new();
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

#[test]
fn http_backend_cache_miss_never_consults_git_notes_refs() {
    let mut repo = TestRepo::new();
    let mut file = repo.filename("http-isolation.txt");
    file.set_contents(lines!["AI content that only exists in Git notes".ai()]);
    repo.stage_all_and_commit("feat: isolate HTTP notes")
        .unwrap();
    file.assert_lines_and_blame(lines!["AI content that only exists in Git notes".ai()]);

    let sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("resolve committed SHA")
        .trim()
        .to_string();
    assert!(
        repo.read_authorship_note(&sha).is_some(),
        "precondition failed: legacy authorship note should exist only in refs/notes/ai"
    );

    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: None,
        });
    });

    let notes_db = tempfile::NamedTempFile::new().expect("create isolated empty notes db");
    let notes_db_path = notes_db.path().to_string_lossy().to_string();
    let spawn_args = tempfile::NamedTempFile::new().expect("create internal Git argument log");
    let spawn_args_path = spawn_args.path().to_string_lossy().to_string();
    let env = [
        ("GIT_AI_TEST_NOTES_DB_PATH", notes_db_path.as_str()),
        ("GIT_AI_SPAWN_ARGS_LOG", spawn_args_path.as_str()),
    ];

    let log_output = repo
        .git_ai_with_env(&["log", "--no-pager", "--raw", "-n", "1"], &env)
        .expect("HTTP-backed log should succeed without cached authorship");
    assert!(log_output.contains("feat: isolate HTTP notes"));
    assert!(
        !log_output.contains("schema_version")
            && !log_output.contains("s_")
            && log_output.contains("Authorship note:\n      (none)"),
        "HTTP log must not fall back to the legacy Git note:\n{log_output}"
    );

    let show_output = repo
        .git_ai_with_env(&["show", "HEAD"], &env)
        .expect("HTTP-backed show should succeed without cached authorship");
    assert!(
        show_output.contains("No authorship data found"),
        "HTTP show must not fall back to the legacy Git note:\n{show_output}"
    );

    repo.git_ai_with_env(&["stats", "HEAD", "--json"], &env)
        .expect("HTTP-backed stats should succeed without cached authorship");
    let _ = repo.git_ai_with_env(&["diff", "HEAD", "--json"], &env);
    let _ = repo.git_ai_with_env(&["show-prompt", "s_missing_http_isolation"], &env);

    let spawned_args =
        fs::read_to_string(spawn_args.path()).expect("read internal Git argument log");
    assert!(
        !spawned_args
            .lines()
            .any(|line| line.contains("refs/notes/") || line.contains("--ref=ai")),
        "ordinary HTTP paths must never target a Git notes ref; spawns:\n{spawned_args}"
    );
}
