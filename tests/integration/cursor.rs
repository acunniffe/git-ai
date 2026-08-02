use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{TestRepo, real_git_executable};
use crate::test_utils::{fixture_path, read_jsonl_fixture};
use git_ai::error::GitAiError;
use git_ai::model::stream_watermark::ByteOffsetWatermark;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::CursorAgent;
use std::path::PathBuf;

const TEST_CONVERSATION_ID: &str = "de751938-f32b-4441-8239-a31d60aa4cf0";

fn parse_cursor(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("cursor")?.parse(hook_input, "t_test")
}

#[test]
fn test_cursor_raw_event_fidelity() {
    let fixture = fixture_path("cursor-session-simple.jsonl");
    let agent = CursorAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(fixture.as_path(), watermark, "test")
        .expect("Should parse cursor JSONL");

    let expected = read_jsonl_fixture(&fixture).unwrap();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
fn test_cursor_jsonl_empty_file() {
    use tempfile::NamedTempFile;

    let temp_file = NamedTempFile::new().expect("Should create temp file");
    let _ = temp_file.as_file().sync_all();

    let agent = CursorAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent
        .read_incremental(temp_file.path(), watermark, "test")
        .expect("Should handle empty file");

    assert!(
        result.events.is_empty(),
        "Empty file should produce empty events"
    );
}

#[test]
fn test_cursor_jsonl_malformed_lines_skipped() {
    use std::io::Write;
    use tempfile::NamedTempFile;

    let mut temp_file = NamedTempFile::new().expect("Should create temp file");
    writeln!(
        temp_file,
        r#"{{"role":"user","message":{{"content":[{{"type":"text","text":"hello"}}]}}}}"#
    )
    .unwrap();
    writeln!(temp_file, "this is not valid json").unwrap();
    writeln!(
        temp_file,
        r#"{{"role":"assistant","message":{{"content":[{{"type":"text","text":"hi there"}}]}}}}"#
    )
    .unwrap();
    temp_file.flush().unwrap();

    let agent = CursorAgent::new();
    let watermark = Box::new(ByteOffsetWatermark::new(0));
    let result = agent.read_incremental(temp_file.path(), watermark, "test");

    // Malformed JSON lines are skipped; valid lines before and after are returned
    let batch = result.expect("malformed lines should be skipped, not cause errors");
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.events[0]["role"].as_str(), Some("user"));
    assert_eq!(batch.events[1]["role"].as_str(), Some("assistant"));
}

#[test]
fn test_cursor_preset_multi_root_workspace_detection() {
    // Helper function to test workspace selection
    let test_workspace_selection =
        |workspace_roots: &[&str], file_path: &str, expected_workspace: &str, description: &str| {
            let workspace_roots_json: Vec<String> = workspace_roots
                .iter()
                .map(|s| format!("\"{}\"", s))
                .collect();

            let tool_input_json = if file_path.is_empty() {
                String::new()
            } else {
                format!(
                    ",\n        \"tool_input\": {{ \"file_path\": \"{}\" }}",
                    file_path
                )
            };

            let hook_input = format!(
                r##"{{
        "conversation_id": "test-conversation-id",
        "workspace_roots": [{}],
        "hook_event_name": "preToolUse",
        "tool_name": "Write"{},
        "model": "model-name-from-hook-test"
    }}"##,
                workspace_roots_json.join(", "),
                tool_input_json
            );

            let events = parse_cursor(&hook_input)
                .unwrap_or_else(|_| panic!("Should succeed for: {}", description));

            assert_eq!(events.len(), 1);
            match &events[0] {
                ParsedHookEvent::PreFileEdit(e) => {
                    assert_eq!(
                        e.context.cwd,
                        PathBuf::from(expected_workspace),
                        "{}",
                        description
                    );
                }
                _ => panic!("Expected PreFileEdit for: {}", description),
            }
        };

    // Test 1: File in second workspace root
    test_workspace_selection(
        &[
            "/Users/test/workspace1",
            "/Users/test/workspace2",
            "/Users/test/workspace3",
        ],
        "/Users/test/workspace2/src/main.rs",
        "/Users/test/workspace2",
        "Should select workspace2 as it contains the file path",
    );

    // Test 2: File in third workspace root
    test_workspace_selection(
        &[
            "/Users/test/workspace1",
            "/Users/test/workspace2",
            "/Users/test/workspace3",
        ],
        "/Users/test/workspace3/lib/utils.rs",
        "/Users/test/workspace3",
        "Should select workspace3 as it contains the file path",
    );

    // Test 3: File path doesn't match any workspace (should fall back to first)
    test_workspace_selection(
        &["/Users/test/workspace1", "/Users/test/workspace2"],
        "/Users/other/project/src/main.rs",
        "/Users/test/workspace1",
        "Should fall back to first workspace when file path doesn't match any workspace",
    );

    // Test 4: No file path provided (should use first workspace)
    test_workspace_selection(
        &["/Users/test/workspace1", "/Users/test/workspace2"],
        "",
        "/Users/test/workspace1",
        "Should use first workspace when no file path is provided",
    );

    // Test 5: Workspace root with trailing slash
    test_workspace_selection(
        &["/Users/test/workspace1/", "/Users/test/workspace2/"],
        "/Users/test/workspace2/src/main.rs",
        "/Users/test/workspace2/",
        "Should handle workspace roots with trailing slashes",
    );

    // Test 6: File path without leading separator after workspace root
    test_workspace_selection(
        &["/Users/test/workspace1", "/Users/test/workspace2"],
        "/Users/test/workspace2/main.rs",
        "/Users/test/workspace2",
        "Should correctly match workspace even with immediate file after root",
    );

    // Test 7: Ambiguous prefix (workspace1 is prefix of workspace10)
    test_workspace_selection(
        &["/Users/test/workspace1", "/Users/test/workspace10"],
        "/Users/test/workspace10/src/main.rs",
        "/Users/test/workspace10",
        "Should correctly distinguish workspace10 from workspace1",
    );
}

#[test]
fn test_cursor_preset_human_checkpoint_no_filepath() {
    let hook_input = r##"{
        "conversation_id": "test-conversation-id",
        "workspace_roots": ["/Users/test/workspace"],
        "hook_event_name": "preToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": "/Users/test/workspace/src/main.rs" },
        "model": "model-name-from-hook-test"
    }"##;

    let events = parse_cursor(hook_input).expect("Should succeed for human checkpoint");

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(_e) => {
            // PreFileEdit is the human checkpoint equivalent
        }
        _ => panic!("Expected PreFileEdit for human checkpoint"),
    }
}

#[test]
fn test_cursor_checkpoint_stdin_with_utf8_bom() {
    let repo = TestRepo::new();
    let hook_input = format!(
        "\u{feff}{}",
        serde_json::json!({
            "conversation_id": "test-conversation-id",
            "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
            "hook_event_name": "preToolUse",
            "tool_name": "Write",
            "model": "model-name-from-hook-test"
        })
    );

    let output = repo
        .git_ai_with_stdin(
            &["checkpoint", "cursor", "--hook-input", "stdin"],
            hook_input.as_bytes(),
        )
        .expect("checkpoint should parse stdin payload with UTF-8 BOM");

    assert!(
        !output.contains("Invalid JSON in hook_input"),
        "Should not fail JSON parsing when stdin has UTF-8 BOM. Output: {output}"
    );
}

fn utf16le_bytes(input: &str) -> Vec<u8> {
    input
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

#[test]
fn test_cursor_checkpoint_stdin_with_utf16le_odd_cjk_content() {
    use std::fs;

    let repo = TestRepo::new();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/文案文.js");
    fs::write(&file_path, "const a = \"base\";\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("src/文案文.js");
    file.assert_committed_lines(crate::lines!["const a = \"base\";".unattributed_human(),]);

    let edited_content = "const a = \"文案文\"; // 3 chars\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = serde_json::json!({
        "conversation_id": TEST_CONVERSATION_ID,
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": {
            "file_path": file_path.to_string_lossy().to_string(),
            "content": edited_content,
        },
        "model": "model-name-from-hook-test"
    })
    .to_string();

    let output = repo
        .git_ai_with_stdin(
            &["checkpoint", "cursor", "--hook-input", "stdin"],
            &utf16le_bytes(&hook_input),
        )
        .expect("checkpoint should parse UTF-16LE stdin payload with odd CJK content");

    assert!(
        !output.contains("Invalid JSON in hook_input"),
        "Should not fail JSON parsing for odd CJK content in UTF-16LE stdin. Output: {output}"
    );

    repo.stage_all_and_commit("Add cursor CJK edit").unwrap();

    file.assert_lines_and_blame(crate::lines!["const a = \"文案文\"; // 3 chars".ai(),]);
}

#[test]
fn test_cursor_e2e_with_attribution() {
    use std::fs;

    let repo = TestRepo::new();
    let jsonl_fixture = fixture_path("cursor-session-simple.jsonl");
    let jsonl_path_str = jsonl_fixture.to_string_lossy().to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/main.rs");
    let base_content = "fn main() {\n    println!(\"Hello, World!\");\n}\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let edited_content = "fn main() {\n    println!(\"Hello, World!\");\n    // This is from Cursor\n    println!(\"Additional line from Cursor\");\n}\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = serde_json::json!({
        "conversation_id": TEST_CONVERSATION_ID,
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": file_path.to_string_lossy().to_string() },
        "model": "model-name-from-hook-test",
        "transcript_path": jsonl_path_str
    })
    .to_string();

    let result = repo
        .checkpoint_with_hook_input("cursor", &hook_input)
        .unwrap();

    println!("Checkpoint output: {}", result);

    let commit = repo.stage_all_and_commit("Add cursor edits").unwrap();

    let mut file = repo.filename("src/main.rs");
    file.assert_lines_and_blame(crate::lines![
        "fn main() {".human(),
        "    println!(\"Hello, World!\");".human(),
        "    // This is from Cursor".ai(),
        "    println!(\"Additional line from Cursor\");".ai(),
        "}".human(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have at least one attestation"
    );

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one session record in metadata"
    );

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have at least one session record");

    assert_eq!(
        session_record.agent_id.model, "model-name-from-hook-test",
        "Model should be 'model-name-from-hook-test' from hook input"
    );
}

#[test]
fn test_cursor_e2e_with_resync() {
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    let repo = TestRepo::new();

    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_jsonl_path = temp_dir.path().join("cursor-session.jsonl");
    let fixture_content = fs::read_to_string(fixture_path("cursor-session-simple.jsonl"))
        .expect("Should read fixture");
    fs::write(&temp_jsonl_path, &fixture_content).expect("Should write temp JSONL");
    let temp_jsonl_str = temp_jsonl_path.to_string_lossy().to_string();

    let src_dir = repo.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_path = repo.path().join("src/main.rs");
    let base_content = "fn main() {\n    println!(\"Hello, World!\");\n}\n";
    fs::write(&file_path, base_content).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    let edited_content = "fn main() {\n    println!(\"Hello, World!\");\n    // This is from Cursor\n    println!(\"Additional line from Cursor\");\n}\n";
    fs::write(&file_path, edited_content).unwrap();

    let hook_input = serde_json::json!({
        "conversation_id": TEST_CONVERSATION_ID,
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": file_path.to_string_lossy().to_string() },
        "model": "model-name-from-hook-test",
        "transcript_path": temp_jsonl_str
    })
    .to_string();

    let result = repo
        .checkpoint_with_hook_input("cursor", &hook_input)
        .unwrap();

    println!("Checkpoint output: {}", result);

    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&temp_jsonl_path)
            .expect("Should open temp JSONL for appending");
        writeln!(file).expect("Should write newline separator");
        writeln!(
            file,
            r#"{{"role":"assistant","message":{{"content":[{{"type":"text","text":"RESYNC_TEST_MESSAGE: This was added after the checkpoint"}}]}}}}"#
        )
        .expect("Should append to JSONL");
    }

    repo.git(&["add", "-A"]).expect("add --all should succeed");
    let commit = repo.commit("Add cursor edits").unwrap();

    let mut file = repo.filename("src/main.rs");
    file.assert_lines_and_blame(crate::lines![
        "fn main() {".human(),
        "    println!(\"Hello, World!\");".human(),
        "    // This is from Cursor".ai(),
        "    println!(\"Additional line from Cursor\");".ai(),
        "}".human(),
    ]);

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have at least one attestation"
    );

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Should have at least one session record in metadata"
    );

    let _session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Should have at least one session record");

    // Note: Messages field has been removed from SessionRecord
}

#[test]
fn test_cursor_checkpoint_routes_nested_worktree_file_to_worktree_repo() {
    use git_ai::operations::git::repository::find_repository_in_path;
    use std::fs;
    use std::process::Command;

    let repo = TestRepo::new();
    let jsonl_fixture = fixture_path("cursor-session-simple.jsonl");
    let jsonl_path_str = jsonl_fixture.to_string_lossy().to_string();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Parent Repo"]);
    repo.stage_all_and_commit("initial commit").unwrap();

    let worktree_path = repo.path().join("hbd-worktree");
    let worktree_output = Command::new(real_git_executable())
        .args([
            "-C",
            repo.path().to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "hbd-cli",
            worktree_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to create nested linked worktree");
    assert!(
        worktree_output.status.success(),
        "failed to create nested linked worktree:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&worktree_output.stdout),
        String::from_utf8_lossy(&worktree_output.stderr)
    );

    let file_path = worktree_path.join("main.go");
    fs::write(
        &file_path,
        "package main\n\nfunc main() {\n\tprintln(\"hbd\")\n}\n",
    )
    .unwrap();

    let hook_input = serde_json::json!({
        "conversation_id": TEST_CONVERSATION_ID,
        "workspace_roots": [repo.canonical_path().to_string_lossy().to_string()],
        "hook_event_name": "postToolUse",
        "tool_name": "Write",
        "tool_input": { "file_path": file_path.to_string_lossy().to_string() },
        "model": "model-name-from-hook-test",
        "transcript_path": jsonl_path_str
    })
    .to_string();

    let output = repo
        .checkpoint_with_hook_input("cursor", &hook_input)
        .expect("cursor checkpoint should succeed");
    println!("Checkpoint output: {}", output);

    repo.sync_daemon_force();

    let parent_repo =
        find_repository_in_path(repo.path().to_str().unwrap()).expect("find parent repo");
    let parent_base = parent_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let parent_working_log = parent_repo
        .storage
        .working_log_for_base_commit(&parent_base)
        .expect("parent working log");

    assert!(
        parent_working_log
            .all_ai_touched_files()
            .unwrap_or_default()
            .is_empty(),
        "checkpoint must not stay on the parent repo when the edited file lives in a nested linked worktree"
    );

    let worktree_repo =
        find_repository_in_path(worktree_path.to_str().unwrap()).expect("find worktree repo");
    let worktree_base = worktree_repo
        .head()
        .ok()
        .and_then(|head| head.target().ok())
        .unwrap_or_else(|| "initial".to_string());
    let worktree_working_log = worktree_repo
        .storage
        .working_log_for_base_commit(&worktree_base)
        .expect("worktree working log");

    let touched_files = worktree_working_log
        .all_ai_touched_files()
        .expect("read worktree touched files");
    assert!(
        touched_files.contains("main.go"),
        "cursor checkpoint should be recorded in the linked worktree working log when only the parent repo is listed in workspace_roots; found {:?}",
        touched_files
    );

    let checkpoints = worktree_working_log
        .read_all_checkpoints()
        .expect("read worktree checkpoints");
    assert!(
        !checkpoints.is_empty(),
        "worktree checkpoint log should not be empty for a nested linked worktree edit"
    );
}

crate::reuse_tests_in_worktree!(
    test_cursor_raw_event_fidelity,
    test_cursor_preset_multi_root_workspace_detection,
    test_cursor_preset_human_checkpoint_no_filepath,
    test_cursor_e2e_with_attribution,
    test_cursor_e2e_with_resync,
);
