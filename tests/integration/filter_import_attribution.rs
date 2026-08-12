use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{checkpoint_codex_bash_hook, setup_codex_bash_repo};
use std::fs;

fn write_ai_edit(repo: &TestRepo, path: &str, contents: &str) {
    repo.git_ai(&["checkpoint", "human", path]).unwrap();
    fs::write(repo.path().join(path), contents).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", path]).unwrap();
}

fn fast_import_stream(repo: &TestRepo, content: &str, message: &str) -> Vec<u8> {
    let old = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let branch = repo.current_branch();
    format!(
        "blob\nmark :1\ndata {}\n{}commit refs/heads/{}\nmark :2\nauthor Importer <importer@example.com> 1700000000 +0000\ncommitter Importer <importer@example.com> 1700000000 +0000\ndata {}\n{}\nfrom {}\nM 100644 :1 fast-import.txt\ndone\n",
        content.len(),
        content,
        branch,
        message.len(),
        message,
        old,
    )
    .into_bytes()
}

fn multi_fast_import_stream(repo: &TestRepo) -> Vec<u8> {
    let old = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let branch = repo.current_branch();
    let first = "first imported AI\n";
    let second = "second imported AI\n";
    let first_message = "first import";
    let second_message = "second import";
    format!(
        "blob\nmark :1\ndata {}\n{}commit refs/heads/{}\nmark :2\nauthor Importer <importer@example.com> 1700000000 +0000\ncommitter Importer <importer@example.com> 1700000000 +0000\ndata {}\n{}\nfrom {}\nM 100644 :1 first-import.txt\nblob\nmark :3\ndata {}\n{}commit refs/heads/{}\nmark :4\nauthor Importer <importer@example.com> 1700000001 +0000\ncommitter Importer <importer@example.com> 1700000001 +0000\ndata {}\n{}\nfrom :2\nM 100644 :3 second-import.txt\ndone\n",
        first.len(),
        first,
        branch,
        first_message.len(),
        first_message,
        old,
        second.len(),
        second,
        branch,
        second_message.len(),
        second_message,
    )
    .into_bytes()
}

#[test]
fn fast_import_inside_codex_bash_attributes_imported_commit() {
    let (_db, repo, transcript) = setup_codex_bash_repo("initial");
    let content = "fast import AI\n";
    let stream = fast_import_stream(&repo, content, "fast import commit");
    let command = "git fast-import --quiet < stream";
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "fast-import-bash-session",
        "fast-import-bash-tool",
        "PreToolUse",
        command,
    );
    repo.git_with_stdin(&["fast-import", "--quiet"], &stream)
        .unwrap();
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "fast-import-bash-session",
        "fast-import-bash-tool",
        "PostToolUse",
        command,
    );
    repo.sync_daemon();
    repo.git_og(&["reset", "--hard", "HEAD"]).unwrap();

    let mut imported = repo.filename("fast-import.txt");
    imported.assert_committed_lines(lines!["fast import AI".ai()]);
}

#[test]
fn fast_import_outside_agent_bash_does_not_invent_ai_attribution() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut base = repo.filename("base.txt");
    base.assert_committed_lines(lines!["base".unattributed_human()]);
    let stream = fast_import_stream(&repo, "external import\n", "external import");
    repo.git_with_stdin(&["fast-import", "--quiet"], &stream)
        .unwrap();
    repo.sync_daemon();
    repo.git_og(&["reset", "--hard", "HEAD"]).unwrap();

    let mut imported = repo.filename("fast-import.txt");
    imported.assert_committed_lines(lines!["external import".unattributed_human()]);
}

#[test]
fn multi_commit_fast_import_inside_bash_notes_every_imported_commit() {
    let (_db, repo, transcript) = setup_codex_bash_repo("initial");
    let base = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let stream = multi_fast_import_stream(&repo);
    let command = "git fast-import --quiet < two-commit-stream";
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "fast-import-bash-session",
        "fast-import-bash-tool",
        "PreToolUse",
        command,
    );
    repo.git_with_stdin(&["fast-import", "--quiet"], &stream)
        .unwrap();
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "fast-import-bash-session",
        "fast-import-bash-tool",
        "PostToolUse",
        command,
    );
    repo.sync_daemon();

    let commits = repo
        .git(&["rev-list", "--reverse", &format!("{base}..HEAD")])
        .unwrap()
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(commits.len(), 2);
    for commit in &commits {
        assert!(
            repo.read_authorship_note(commit).is_some(),
            "imported commit {commit} should have an authorship note"
        );
    }
    repo.git_og(&["reset", "--hard", "HEAD"]).unwrap();
    let mut first = repo.filename("first-import.txt");
    first.assert_committed_lines(lines!["first imported AI".ai()]);
    let mut second = repo.filename("second-import.txt");
    second.assert_committed_lines(lines!["second imported AI".ai()]);
}

#[test]
fn filter_branch_rewrite_preserves_existing_ai_line_attribution() {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    write_ai_edit(&repo, "source.txt", "source AI\n");
    let original = repo.stage_all_and_commit("source").unwrap().commit_sha;
    source.assert_committed_lines(lines!["source AI".ai()]);

    repo.git_with_env(
        &[
            "filter-branch",
            "--force",
            "--env-filter",
            "export GIT_AUTHOR_EMAIL=rewritten@example.com",
            "--",
            "HEAD",
        ],
        &[("FILTER_BRANCH_SQUELCH_WARNING", "1")],
        None,
    )
    .unwrap();
    repo.sync_daemon();
    let rewritten = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rewritten, original);
    source.assert_lines_and_blame(vec!["source AI".ai()]);
}

#[test]
fn filter_branch_multi_commit_rewrite_preserves_every_ai_source_note() {
    let repo = TestRepo::new();
    let mut first = repo.filename("first.txt");
    write_ai_edit(&repo, "first.txt", "first AI\n");
    repo.stage_all_and_commit("first").unwrap();
    first.assert_committed_lines(lines!["first AI".ai()]);
    let mut second = repo.filename("second.txt");
    write_ai_edit(&repo, "second.txt", "second AI\n");
    repo.stage_all_and_commit("second").unwrap();
    second.assert_committed_lines(lines!["second AI".ai()]);

    repo.git_with_env(
        &[
            "filter-branch",
            "--force",
            "--env-filter",
            "export GIT_COMMITTER_EMAIL=rewritten@example.com",
            "--",
            "HEAD",
        ],
        &[("FILTER_BRANCH_SQUELCH_WARNING", "1")],
        None,
    )
    .unwrap();
    repo.sync_daemon();
    first.assert_lines_and_blame(vec!["first AI".ai()]);
    second.assert_lines_and_blame(vec!["second AI".ai()]);
}

#[test]
fn filter_branch_index_filter_prune_preserves_surviving_ai_history() {
    let repo = TestRepo::new();
    let mut keep = repo.filename("keep.txt");
    write_ai_edit(&repo, "keep.txt", "kept AI\n");
    repo.stage_all_and_commit("keep").unwrap();
    keep.assert_committed_lines(lines!["kept AI".ai()]);
    let mut remove = repo.filename("remove.txt");
    write_ai_edit(&repo, "remove.txt", "removed AI\n");
    repo.stage_all_and_commit("remove").unwrap();
    remove.assert_committed_lines(lines!["removed AI".ai()]);

    repo.git_with_env(
        &[
            "filter-branch",
            "--force",
            "--index-filter",
            "git rm --cached --ignore-unmatch remove.txt",
            "--prune-empty",
            "--",
            "HEAD",
        ],
        &[("FILTER_BRANCH_SQUELCH_WARNING", "1")],
        None,
    )
    .unwrap();
    repo.sync_daemon();
    assert!(!repo.path().join("remove.txt").exists());
    keep.assert_lines_and_blame(vec!["kept AI".ai()]);
}
