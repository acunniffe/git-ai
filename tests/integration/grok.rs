use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::error::GitAiError;
use git_ai::mdm::utils::grok_home_dir;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn parse_grok(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("grok")?.parse(hook_input, "t_test")
}

fn grok_input(event: &str, tool: &str) -> String {
    json!({
        "hookEventName": event,
        "sessionId": "grok-sess-1",
        "cwd": "/home/user/project",
        "workspaceRoot": "/home/user/project",
        "toolName": tool,
        "toolUseId": "tu-1",
        "toolInput": {"file_path": "src/main.rs", "command": "npm test"},
        "model": "grok-4.6"
    })
    .to_string()
}

#[test]
fn test_grok_pre_file_edit_camel_case() {
    let events = parse_grok(&grok_input("pre_tool_use", "search_replace")).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "grok");
            assert_eq!(e.context.agent_id.model, "grok-4.6");
            assert_eq!(e.context.external_session_id, "grok-sess-1");
            assert_eq!(e.context.cwd, PathBuf::from("/home/user/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/home/user/project/src/main.rs")]
            );
            assert_eq!(e.tool_use_id.as_deref(), Some("tu-1"));
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_grok_write_is_a_file_edit() {
    let events = parse_grok(&grok_input("pre_tool_use", "write")).unwrap();
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "grok");
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/home/user/project/src/main.rs")]
            );
        }
        _ => panic!("Expected PreFileEdit for write"),
    }
}

#[test]
fn test_grok_post_file_edit_attaches_stream_when_transcript_exists() {
    let temp = tempfile::TempDir::new().unwrap();
    let updates_path = temp.path().join("updates.jsonl");
    fs::write(
        &updates_path,
        json!({"timestamp": 1788274022, "params": {"update": {"_meta": {"modelId": "grok-4.6"}}}})
            .to_string(),
    )
    .unwrap();

    let input = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "grok-sess-1",
        "cwd": "/home/user/project",
        "transcriptPath": updates_path,
        "toolName": "search_replace",
        "toolUseId": "tu-1",
        "toolInput": {"file_path": "src/main.rs"}
    })
    .to_string();

    let events = parse_grok(&input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "grok");
            assert_eq!(e.context.agent_id.model, "grok-4.6");
            let stream = e.stream_source.as_ref().expect("stream_source");
            assert_eq!(stream.path, updates_path);
            assert_eq!(stream.external_session_id, "grok-sess-1");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
#[serial_test::serial]
fn test_grok_post_omits_transcript_path_still_attaches_stream() {
    let temp = tempfile::TempDir::new().unwrap();
    let prev = std::env::var_os("GROK_HOME");
    unsafe {
        std::env::set_var("GROK_HOME", temp.path());
    }

    let input = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "grok-sess-1",
        "cwd": "/tmp/proj",
        "toolName": "search_replace",
        "toolInput": {"file_path": "src/main.rs"}
    })
    .to_string();
    let events = parse_grok(&input).unwrap();

    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let stream = e.stream_source.as_ref().expect("stream_source");
            let expected = grok_home_dir()
                .join("sessions")
                .join("%2Ftmp%2Fproj")
                .join("grok-sess-1")
                .join("updates.jsonl");
            assert_eq!(stream.path, expected);
            assert_eq!(stream.external_session_id, "grok-sess-1");
            assert!(!expected.is_file());
        }
        _ => panic!("Expected PostFileEdit"),
    }

    unsafe {
        match prev {
            Some(value) => std::env::set_var("GROK_HOME", value),
            None => std::env::remove_var("GROK_HOME"),
        }
    }
}

#[test]
fn test_grok_pre_bash_call() {
    let events = parse_grok(&grok_input("pre_tool_use", "run_terminal_command")).unwrap();
    match &events[0] {
        ParsedHookEvent::PreBashCall(e) => {
            assert_eq!(e.context.agent_id.tool, "grok");
            assert_eq!(e.tool_use_id, "tu-1");
            assert_eq!(e.command.as_deref(), Some("npm test"));
        }
        _ => panic!("Expected PreBashCall"),
    }
}

#[test]
fn test_grok_skips_read_only_tools() {
    let events = parse_grok(&grok_input("pre_tool_use", "read_file")).unwrap();
    assert!(events.is_empty());
}

#[test]
fn test_grok_uses_workspace_root_when_cwd_missing() {
    let input = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "s1",
        "workspaceRoot": "/ws",
        "toolName": "Write",
        "toolInput": {"path": "lib.rs"}
    })
    .to_string();
    let events = parse_grok(&input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.cwd, PathBuf::from("/ws"));
            assert_eq!(e.file_paths, vec![PathBuf::from("/ws/lib.rs")]);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_grok_invalid_json() {
    assert!(parse_grok("not json").is_err());
}

#[test]
fn test_grok_e2e_file_edit_is_attributed() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("src.txt");
    fs::write(&file_path, "hello\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("src.txt");
    file.assert_committed_lines(crate::lines!["hello".unattributed_human()]);

    let pre = json!({
        "hookEventName": "pre_tool_use",
        "sessionId": "grok-e2e",
        "cwd": repo.canonical_path().to_string_lossy(),
        "toolName": "search_replace",
        "toolUseId": "tu-edit",
        "toolInput": {"file_path": "src.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &pre])
        .unwrap();

    fs::write(&file_path, "hello\nfrom grok\n").unwrap();

    let post = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "grok-e2e",
        "cwd": repo.canonical_path().to_string_lossy(),
        "toolName": "search_replace",
        "toolUseId": "tu-edit",
        "toolInput": {"file_path": "src.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &post])
        .unwrap();

    let commit = repo.stage_all_and_commit("grok edit").unwrap();
    file.assert_lines_and_blame(crate::lines!["hello".human(), "from grok".ai(),]);

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session");
    assert_eq!(session.agent_id.tool, "grok");
    assert_eq!(session.agent_id.model, "grok-4.6");
}

#[test]
fn test_grok_e2e_write_creates_ai_file() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("keep.txt"), "keep\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut keep = repo.filename("keep.txt");
    keep.assert_committed_lines(crate::lines!["keep".unattributed_human()]);

    let new_path = repo.path().join("created.txt");
    let pre = json!({
        "hookEventName": "pre_tool_use",
        "sessionId": "grok-write",
        "cwd": repo.canonical_path().to_string_lossy(),
        "toolName": "write",
        "toolUseId": "tu-write",
        "toolInput": {"file_path": "created.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &pre])
        .unwrap();

    fs::write(&new_path, "brand new\n").unwrap();

    let post = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "grok-write",
        "cwd": repo.canonical_path().to_string_lossy(),
        "toolName": "write",
        "toolUseId": "tu-write",
        "toolInput": {"file_path": "created.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &post])
        .unwrap();

    repo.stage_all_and_commit("grok write").unwrap();
    let mut file = repo.filename("created.txt");
    file.assert_committed_lines(crate::lines!["brand new".ai(),]);
}

#[test]
fn test_grok_e2e_bash_file_edit_is_attributed() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("out.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("initial").unwrap();
    let mut file = repo.filename("out.txt");
    file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    let cwd = repo.canonical_path().to_string_lossy().to_string();
    let pre = json!({
        "hookEventName": "pre_tool_use",
        "sessionId": "grok-bash",
        "cwd": cwd,
        "toolName": "run_terminal_command",
        "toolUseId": "tu-bash",
        "toolInput": {"command": "printf 'from grok bash\\n' >> out.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &pre])
        .unwrap();

    fs::write(&file_path, "base\nfrom grok bash\n").unwrap();

    let post = json!({
        "hookEventName": "post_tool_use",
        "sessionId": "grok-bash",
        "cwd": cwd,
        "toolName": "run_terminal_command",
        "toolUseId": "tu-bash",
        "toolInput": {"command": "printf 'from grok bash\\n' >> out.txt"},
        "model": "grok-4.6"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "grok", "--hook-input", &post])
        .unwrap();

    let commit = repo.stage_all_and_commit("grok bash").unwrap();
    file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "from grok bash".ai(),
    ]);

    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session");
    assert_eq!(session.agent_id.tool, "grok");
    assert_eq!(session.agent_id.model, "grok-4.6");
}
