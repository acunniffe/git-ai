use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::fixture_path;
use serde_json::json;
use std::fs;

fn bash_fixture() -> (tempfile::TempDir, TestRepo, std::path::PathBuf) {
    let db_dir = tempfile::tempdir().unwrap();
    let db_value = db_dir
        .path()
        .join("bash-history.db")
        .to_string_lossy()
        .to_string();
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
        db_value.as_str(),
    )]);
    let transcript = repo.path().join("codex-transcript.jsonl");
    fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript).unwrap();
    (db_dir, repo, transcript)
}

fn bash_hook(repo: &TestRepo, transcript: &std::path::Path, event: &str, command: &str) {
    let input = json!({
        "session_id": "mv-bash-session",
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": event,
        "tool_name": "Bash",
        "tool_use_id": "mv-bash-tool",
        "tool_input": { "command": command },
        "transcript_path": transcript.to_string_lossy().to_string()
    })
    .to_string();
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &input])
        .unwrap();
}

#[test]
fn test_git_mv_directory_preserves_each_files_existing_attribution() {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.path().join("old-dir")).unwrap();
    let mut ai = repo.filename("old-dir/ai.txt");
    ai.set_contents_no_stage(lines!["AI source".ai()]);
    fs::write(repo.path().join("old-dir/human.txt"), "human source\n").unwrap();
    repo.stage_all_and_commit("Initial directory").unwrap();

    repo.git(&["mv", "old-dir", "new-dir"]).unwrap();
    repo.git(&["commit", "-m", "Rename directory"]).unwrap();

    let mut ai = repo.filename("new-dir/ai.txt");
    ai.assert_committed_lines(lines!["AI source".ai()]);
    let mut human = repo.filename("new-dir/human.txt");
    human.assert_committed_lines(lines!["human source".human()]);
}

#[test]
fn test_git_mv_case_only_preserves_existing_attribution() {
    let repo = TestRepo::new();
    let mut file = repo.filename("CaseName.txt");
    file.set_contents_no_stage(lines!["case AI".ai()]);
    repo.stage_all_and_commit("Initial case path").unwrap();

    repo.git(&["mv", "CaseName.txt", "casename.txt"]).unwrap();
    repo.git(&["commit", "-m", "Case-only rename"]).unwrap();

    let mut file = repo.filename("casename.txt");
    file.assert_committed_lines(lines!["case AI".ai()]);
}

#[test]
fn test_git_mv_force_overwrite_uses_source_not_destination_attribution() {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    source.set_contents_no_stage(lines!["source AI".ai()]);
    fs::write(repo.path().join("destination.txt"), "destination human\n").unwrap();
    repo.stage_all_and_commit("Initial paths").unwrap();

    repo.git(&["mv", "--force", "--", "source.txt", "destination.txt"])
        .unwrap();
    repo.git(&["commit", "-m", "Force rename over destination"])
        .unwrap();

    let mut destination = repo.filename("destination.txt");
    destination.assert_committed_lines(lines!["source AI".ai()]);
}

#[test]
fn test_chained_git_mv_then_force_overwrite_composes_source_mapping() {
    let repo = TestRepo::new();
    let mut source = repo.filename("source.txt");
    source.set_contents_no_stage(lines!["chained source AI".ai()]);
    fs::write(repo.path().join("destination.txt"), "destination human\n").unwrap();
    repo.stage_all_and_commit("Initial paths").unwrap();

    repo.git(&["mv", "source.txt", "intermediate.txt"]).unwrap();
    repo.git(&["mv", "-f", "--", "intermediate.txt", "destination.txt"])
        .unwrap();
    repo.git(&["commit", "-m", "Chained force rename"]).unwrap();

    let mut destination = repo.filename("destination.txt");
    destination.assert_committed_lines(lines!["chained source AI".ai()]);
}

/// A rename and edit committed in one AI Bash call must attribute only the new
/// edit to the Bash session. The carried source line and a pre-staged file are
/// outside the command-window delta.
#[test]
fn test_git_mv_and_commit_inside_bash_scopes_only_new_edit_to_ai() {
    let (_db, repo, transcript) = bash_fixture();
    fs::write(repo.path().join("old.txt"), "existing human\n").unwrap();
    repo.stage_all_and_commit("Initial source").unwrap();
    fs::write(repo.path().join("pre-staged.txt"), "pre-staged human\n").unwrap();
    repo.git_og(&["add", "pre-staged.txt"]).unwrap();

    let command = "git mv old.txt new.txt && printf 'AI appended\\n' >> new.txt && git add new.txt && git commit -m rename-edit";
    bash_hook(&repo, &transcript, "PreToolUse", command);
    repo.git(&["mv", "old.txt", "new.txt"]).unwrap();
    fs::write(repo.path().join("new.txt"), "existing human\nAI appended\n").unwrap();
    repo.git(&["add", "new.txt"]).unwrap();
    repo.git(&["commit", "-m", "rename-edit"]).unwrap();
    bash_hook(&repo, &transcript, "PostToolUse", command);
    repo.sync_daemon();

    let mut renamed = repo.filename("new.txt");
    renamed.assert_committed_lines(lines!["existing human".human(), "AI appended".ai()]);
    let mut pre_staged = repo.filename("pre-staged.txt");
    pre_staged.assert_committed_lines(lines!["pre-staged human".unattributed_human()]);
}
