use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn parse_agent_v1(hook_input: &str) -> Result<Vec<ParsedHookEvent>, git_ai::error::GitAiError> {
    resolve_preset("agent-v1")?.parse(hook_input, "t_test")
}

#[test]
fn test_agent_v1_human_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "human");
            assert_eq!(e.context.agent_id.id, "human");
            assert_eq!(e.context.agent_id.model, "human");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
        }
        _ => panic!("Expected PreFileEdit for human checkpoint"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
            // Inline transcripts removed - should now be None
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostFileEdit for ai_agent checkpoint"),
    }
}

#[test]
fn test_agent_v1_human_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"]
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123"
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_multiple_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file1.ts", "/Users/test/project/file2.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file1.ts": "content1",
            "/Users/test/project/file2.ts": "content2"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 2);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file1.ts"))
                    .unwrap(),
                "content1"
            );
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file2.ts"))
                    .unwrap(),
                "content2"
            );
            assert_eq!(e.file_paths.len(), 2);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_relative_paths_resolved_to_absolute() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["src/main.rs"],
        "dirty_files": {
            "src/main.rs": "fn main() {}"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }

    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["src/main.rs"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-1",
        "dirty_files": {
            "src/main.rs": "fn main() { println!(\"hello\"); }"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_relative_dirty_files_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["original line".unattributed_human(),]);

    let repo_dir = repo.path().to_string_lossy().to_string();

    let pre_edit_content = "original line\n";
    let human_payload = json!({
        "type": "human",
        "repo_working_dir": repo_dir,
        "will_edit_filepaths": ["test.txt"],
        "dirty_files": {
            "test.txt": pre_edit_content
        }
    })
    .to_string();
    repo.checkpoint_with_hook_input("agent-v1", &human_payload)
        .unwrap();

    let post_edit_content = "original line\nAI added line\n";
    fs::write(&file_path, post_edit_content).unwrap();

    let ai_payload = json!({
        "type": "ai_agent",
        "repo_working_dir": repo_dir,
        "edited_filepaths": ["test.txt"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-123",
        "dirty_files": {
            "test.txt": post_edit_content
        }
    })
    .to_string();
    repo.checkpoint_with_hook_input("agent-v1", &ai_payload)
        .unwrap();

    repo.stage_all_and_commit("AI edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn test_agent_v1_shell_command_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("script-output.txt");

    fs::write(&file_path, "base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("script-output.txt");
    file.assert_committed_lines(crate::lines!["base line".unattributed_human(),]);

    let repo_dir = repo.canonical_path().to_string_lossy().to_string();

    let pre_payload = json!({
        "type": "pre_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.checkpoint_with_hook_input("agent-v1", &pre_payload)
        .unwrap();

    fs::write(&file_path, "base line\ncreated by shell\n").unwrap();

    let post_payload = json!({
        "type": "post_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.checkpoint_with_hook_input("agent-v1", &post_payload)
        .unwrap();

    repo.stage_all_and_commit("Agent v1 shell edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "created by shell".ai(),
    ]);
}
