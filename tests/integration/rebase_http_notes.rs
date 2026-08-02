//! ENG-212 regression coverage for rewrite source-note recovery under the HTTP backend.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::config::{NotesBackendConfig, NotesBackendKind};
use git_ai::model::repository::notes_db::NotesDatabase;
use git_ai::notes::reference_server::ReferenceServer;
use std::fs;

fn notes_db_path(repo: &TestRepo) -> std::path::PathBuf {
    repo.test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("notes-db")
}

fn switch_to_http_backend(repo: &mut TestRepo, backend_url: String) {
    repo.patch_git_ai_config(|patch| {
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: Some(backend_url),
        });
    });
}

fn commit_untracked_base(repo: &TestRepo) -> String {
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.git(&["add", "base.txt"]).unwrap();
    repo.git(&["commit", "-m", "initial commit"]).unwrap();
    repo.sync_daemon();

    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    repo.current_branch()
}

fn commit_ai_source(repo: &TestRepo) -> String {
    let file_path = repo.path().join("feature.txt");
    repo.human_edit("feature.txt", "Human line\n");
    fs::write(&file_path, "Human line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    repo.git(&["add", "feature.txt"]).unwrap();
    repo.git(&["commit", "-m", "AI source"]).unwrap();
    repo.sync_daemon();

    let mut file = repo.filename("feature.txt");
    file.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai()]);

    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string()
}

fn advance_main(repo: &TestRepo, main_branch: &str) {
    repo.git(&["checkout", main_branch]).unwrap();
    fs::write(repo.path().join("main.txt"), "main change\n").unwrap();
    repo.git(&["add", "main.txt"]).unwrap();
    repo.git(&["commit", "-m", "advance main"]).unwrap();
    repo.sync_daemon();

    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(crate::lines!["base".unattributed_human()]);
    let mut main = repo.filename("main.txt");
    main.assert_committed_lines(crate::lines!["main change".unattributed_human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.sync_daemon();
}

fn add_broken_origin(repo: &TestRepo) {
    let missing_remote = repo.path().join("no-such-remote");
    repo.git(&["remote", "add", "origin", missing_remote.to_str().unwrap()])
        .unwrap();
}

/// ENG-212: when a rewrite source note exists only on the HTTP backend, fetch
/// and cache it before falling back to a Git refs fetch that may require
/// unavailable SSH credentials.
#[test]
fn test_rebase_fetches_http_only_source_note_before_broken_git_remote() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes server");
    let mut repo = TestRepo::new_with_daemon_env(&[("GIT_AI_API_KEY", "eng-212-test-key")]);
    let main_branch = commit_untracked_base(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let source_sha = commit_ai_source(&repo);
    repo.sync_daemon();

    let source_note = repo
        .git(&["notes", "--ref=ai", "show", &source_sha])
        .expect("source note should exist in refs before the backend switch");
    server.store().put(source_sha.clone(), source_note.clone());
    repo.git(&["notes", "--ref=ai", "remove", &source_sha])
        .unwrap();
    assert!(
        repo.git(&["notes", "--ref=ai", "show", &source_sha])
            .is_err(),
        "source note must exist only on the HTTP backend"
    );

    switch_to_http_backend(&mut repo, server.base_url());
    advance_main(&repo, &main_branch);
    add_broken_origin(&repo);

    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    let cached_source = NotesDatabase::open_at_path(&notes_db_path(&repo))
        .expect("open notes db")
        .get_note(&source_sha)
        .expect("read cached source note");
    assert_eq!(
        cached_source,
        Some(source_note),
        "HTTP source note should be cached as part of rewrite preflight"
    );

    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai()]);
}

/// ENG-212: a source with no note anywhere must not abort the whole rewrite
/// when another source already has a local HTTP-cache note that can migrate.
#[test]
fn test_rebase_preserves_local_note_when_sibling_source_fetch_fails() {
    let server = ReferenceServer::start("127.0.0.1:0").expect("start notes server");
    let backend_url = server.base_url();
    let repo = TestRepo::new_with_daemon_env_and_patch(
        &[("GIT_AI_API_KEY", "eng-212-test-key")],
        |patch| {
            patch.notes_backend = Some(NotesBackendConfig {
                kind: NotesBackendKind::Http,
                backend_url: Some(backend_url),
            });
        },
    );
    let main_branch = commit_untracked_base(&repo);

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let noted_source = commit_ai_source(&repo);
    repo.sync_daemon();

    fs::write(
        repo.path().join("plumbing.txt"),
        "unattributed plumbing line\n",
    )
    .unwrap();
    repo.git(&["add", "plumbing.txt"]).unwrap();
    let tree = repo.git(&["write-tree"]).unwrap().trim().to_string();
    let noteless_source = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            &noted_source,
            "-m",
            "noteless plumbing commit",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&[
        "update-ref",
        "refs/heads/feature",
        &noteless_source,
        &noted_source,
    ])
    .unwrap();
    repo.sync_daemon();

    assert!(
        NotesDatabase::open_at_path(&notes_db_path(&repo))
            .expect("open notes db")
            .get_note(&noteless_source)
            .expect("query noteless source")
            .is_none(),
        "plumbing source should have no authorship note"
    );
    let mut feature = repo.filename("feature.txt");
    feature.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai()]);
    let mut plumbing = repo.filename("plumbing.txt");
    plumbing.assert_committed_lines(crate::lines![
        "unattributed plumbing line".unattributed_human()
    ]);

    advance_main(&repo, &main_branch);
    add_broken_origin(&repo);

    repo.git(&["rebase", &main_branch]).unwrap();
    repo.sync_daemon();

    feature.assert_committed_lines(crate::lines!["Human line".human(), "AI line".ai()]);
    plumbing.assert_committed_lines(crate::lines![
        "unattributed plumbing line".unattributed_human()
    ]);
}
