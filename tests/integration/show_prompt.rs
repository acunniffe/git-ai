use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::git::notes_api::write_note;
use serde_json::Value;

// Local helper mirroring the CLI arg vector used by main
fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// Reimport the parsing function from the show_prompt command module
use git_ai::commands::show_prompt::parse_args;

#[test]
fn parse_args_requires_prompt_id() {
    let result = parse_args(&args(&[]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "show-prompt requires a prompt ID");
}

#[test]
fn parse_args_parses_basic_id() {
    let result = parse_args(&args(&["my-prompt-id"])).unwrap();
    assert_eq!(result.prompt_id, "my-prompt-id");
    assert!(result.commit.is_none());
    assert_eq!(result.offset, 0);
}

#[test]
fn parse_args_parses_commit_flag() {
    let result = parse_args(&args(&["my-id", "--commit", "HEAD"])).unwrap();
    assert_eq!(result.prompt_id, "my-id");
    assert_eq!(result.commit.as_deref(), Some("HEAD"));
    assert_eq!(result.offset, 0);
}

#[test]
fn parse_args_parses_offset_flag() {
    let result = parse_args(&args(&["my-id", "--offset", "2"])).unwrap();
    assert_eq!(result.prompt_id, "my-id");
    assert!(result.commit.is_none());
    assert_eq!(result.offset, 2);
}

#[test]
fn parse_args_rejects_commit_and_offset_together() {
    let result = parse_args(&args(&["id", "--commit", "HEAD", "--offset", "1"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "--commit and --offset are mutually exclusive"
    );
}

#[test]
fn parse_args_rejects_multiple_prompt_ids() {
    let result = parse_args(&args(&["id1", "id2"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "Only one prompt ID can be specified".to_string()
    );
}

#[test]
fn parse_args_requires_commit_value() {
    let result = parse_args(&args(&["id", "--commit"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "--commit requires a value");
}

#[test]
fn parse_args_requires_offset_value() {
    let result = parse_args(&args(&["id", "--offset"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "--offset requires a value");
}

#[test]
fn parse_args_rejects_invalid_offset() {
    let result = parse_args(&args(&["id", "--offset", "not-a-number"]));
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "--offset must be a non-negative integer"
    );
}

#[test]
fn parse_args_rejects_unknown_flag() {
    let result = parse_args(&args(&["id", "--unknown"]));
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "Unknown option: --unknown");
}

#[test]
fn show_prompt_returns_latest_prompt_by_default() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // First AI session
    file.set_contents(crate::lines!["Base".human(), "AI line 1".ai()]);
    repo.stage_all_and_commit("First commit").unwrap();

    // Second AI session with a different AI line so we have distinct prompts
    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    let second_commit = repo.stage_all_and_commit("Second commit").unwrap();

    // Grab one of the session IDs from the latest commit's authorship log
    // (mock_ai now produces sessions instead of prompts)
    let sessions = &second_commit.authorship_log.metadata.sessions;
    let (prompt_id, _session) = sessions
        .iter()
        .next()
        .expect("expected at least one session");

    // show-prompt should return the latest occurrence by default
    let output = repo
        .git_ai(&["show-prompt", prompt_id])
        .expect("show-prompt should succeed");

    let json: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(json["prompt_id"].as_str(), Some(prompt_id.as_str()));
    assert_eq!(
        json["commit"].as_str(),
        Some(second_commit.commit_sha.as_str())
    );
}

#[test]
fn show_prompt_with_offset_skips_occurrences() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // First AI session
    file.set_contents(crate::lines!["Base".human(), "AI line 1".ai()]);
    let first_commit = repo.stage_all_and_commit("First commit").unwrap();

    // Second AI session with new AI content so we get another prompt occurrence
    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    repo.stage_all_and_commit("Second commit").unwrap();

    // Use a session ID from the first commit (mock_ai now produces sessions)
    let sessions_first = &first_commit.authorship_log.metadata.sessions;
    let (prompt_id, _) = sessions_first
        .iter()
        .next()
        .expect("expected at least one session in first commit");

    // Default (no offset) and explicit offset 0 should both succeed and point to the same commit
    let default_output = repo
        .git_ai(&["show-prompt", prompt_id])
        .expect("show-prompt without offset should succeed");
    let default_json: serde_json::Value = serde_json::from_str(default_output.trim()).unwrap();

    let offset0_output = repo
        .git_ai(&["show-prompt", prompt_id, "--offset", "0"])
        .expect("show-prompt with offset 0 should succeed");
    let offset0_json: serde_json::Value = serde_json::from_str(offset0_output.trim()).unwrap();

    assert_eq!(default_json["commit"], offset0_json["commit"]);

    // Offset that is too large should return a clear error
    let err = repo
        .git_ai(&["show-prompt", prompt_id, "--offset", "1"])
        .expect_err("show-prompt with offset 1 should fail when only one occurrence exists");
    assert!(
        err.contains("found 1 time(s), but offset 1 requested"),
        "unexpected error message: {}",
        err
    );
}

/// Regression test for #861: `show-prompt --commit <rev>` must scope its output
/// to the note attached to that specific commit.
///
/// The same prompt ID can appear in several commits of a single agent session.
/// Each commit's authorship note carries its own per-commit prompt record (stats
/// such as `total_additions`). `--commit` must read the note of the requested
/// commit, so two different commits yield two different records — rather than the
/// original bug where output was identical regardless of `--commit`.
#[test]
fn show_prompt_commit_flag_scopes_to_requested_commit() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["Human line", "AI line".ai()]);
    let first_commit = repo.stage_all_and_commit("First commit").unwrap();

    file.insert_at(2, crate::lines!["AI line 2".ai()]);
    let second_commit = repo.stage_all_and_commit("Second commit").unwrap();

    // Attach a note to each commit for the SAME prompt ID, but with distinct
    // per-commit stats so we can prove the output is scoped to the right note.
    let prompt_id = "16e1bbcfe4838018";
    let note = |sha: &str, additions: u32| {
        format!(
            r#"test.txt
  {prompt_id} 2-2
---
{{
  "schema_version": "authorship/3.0.0",
  "git_ai_version": "1.2.0",
  "base_commit_sha": "{sha}",
  "prompts": {{
    "{prompt_id}": {{
      "agent_id": {{"tool": "claude", "id": "shared_session", "model": "opus"}},
      "human_author": null,
      "total_additions": {additions},
      "total_deletions": 0,
      "accepted_lines": {additions},
      "overriden_lines": 0
    }}
  }}
}}"#
        )
    };

    let git_ai_repo = git_ai::git::find_repository_in_path(repo.path().to_str().unwrap())
        .expect("find repository");
    write_note(
        &git_ai_repo,
        &first_commit.commit_sha,
        &note(&first_commit.commit_sha, 5),
    )
    .expect("attach note to first commit");
    write_note(
        &git_ai_repo,
        &second_commit.commit_sha,
        &note(&second_commit.commit_sha, 99),
    )
    .expect("attach note to second commit");

    let show = |rev: &str| -> Value {
        let out = repo
            .git_ai(&["show-prompt", prompt_id, "--commit", rev])
            .expect("show-prompt --commit should succeed");
        serde_json::from_str(out.trim()).expect("valid show-prompt json")
    };

    let first = show(&first_commit.commit_sha);
    let second = show(&second_commit.commit_sha);

    // The reported commit must match the requested commit.
    assert_eq!(
        first["commit"].as_str(),
        Some(first_commit.commit_sha.as_str()),
        "--commit <first> must report the first commit"
    );
    assert_eq!(
        second["commit"].as_str(),
        Some(second_commit.commit_sha.as_str()),
        "--commit <second> must report the second commit"
    );

    // The per-commit prompt record must come from the requested commit's note.
    assert_eq!(
        first["prompt"]["total_additions"].as_u64(),
        Some(5),
        "first commit note records total_additions=5"
    );
    assert_eq!(
        second["prompt"]["total_additions"].as_u64(),
        Some(99),
        "second commit note records total_additions=99"
    );

    // And the two scoped outputs must genuinely differ (the core of #861).
    assert_ne!(
        first["prompt"], second["prompt"],
        "--commit must scope output to the requested commit, not return identical records"
    );
}

crate::reuse_tests_in_worktree!(
    parse_args_requires_prompt_id,
    parse_args_parses_basic_id,
    parse_args_parses_commit_flag,
    parse_args_parses_offset_flag,
    parse_args_rejects_commit_and_offset_together,
    parse_args_rejects_multiple_prompt_ids,
    parse_args_requires_commit_value,
    parse_args_requires_offset_value,
    parse_args_rejects_invalid_offset,
    parse_args_rejects_unknown_flag,
    show_prompt_returns_latest_prompt_by_default,
    show_prompt_with_offset_skips_occurrences,
    show_prompt_commit_flag_scopes_to_requested_commit,
);
