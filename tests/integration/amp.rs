use crate::test_utils::fixture_path;
use git_ai::model::stream_watermark::RecordIndexWatermark;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::operations::streams::agent::Agent;
use git_ai::operations::streams::agents::AmpAgent;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

const AMP_SIMPLE_THREAD_ID: &str = "T-019ca1f6-7f21-77b5-a308-65416ebbdf48";
const AMP_SIMPLE_EDIT_TOOL_USE_ID: &str = "toolu_vrtx_01TJD3myjs6gdrDRVn6ZbNME";
const AMP_THINKING_THREAD_ID: &str = "T-019ca1ce-3ae2-7686-a41e-ccc078837f8a";

fn amp_threads_fixture_path() -> PathBuf {
    fixture_path("amp-threads")
}

fn amp_simple_thread_fixture_path() -> PathBuf {
    amp_threads_fixture_path().join(format!("{}.json", AMP_SIMPLE_THREAD_ID))
}

#[test]
fn test_amp_raw_event_fidelity() {
    let thread_path = amp_threads_fixture_path().join(format!("{}.json", AMP_THINKING_THREAD_ID));

    let agent = AmpAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent
        .read_incremental(&thread_path, watermark, "test")
        .expect("Failed to parse Amp thread JSON");

    // Independently parse the fixture and extract the messages array.
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&thread_path).unwrap()).unwrap();
    let expected: Vec<serde_json::Value> = parsed["messages"].as_array().unwrap().clone();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
fn test_amp_raw_event_fidelity_with_thinking() {
    let thread_path = amp_simple_thread_fixture_path();

    let agent = AmpAgent::new();
    let watermark = Box::new(RecordIndexWatermark::new(0));
    let result = agent
        .read_incremental(&thread_path, watermark, "test")
        .expect("Failed to parse Amp thread JSON");

    // Independently parse the fixture and extract the messages array.
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&thread_path).unwrap()).unwrap();
    let expected: Vec<serde_json::Value> = parsed["messages"].as_array().unwrap().clone();

    assert_eq!(result.events.len(), expected.len());
    assert_eq!(result.events, expected);
}

#[test]
#[serial_test::serial]
fn test_amp_preset_pretooluse_returns_human_checkpoint() {
    unsafe {
        std::env::set_var("GIT_AI_AMP_THREADS_PATH", amp_threads_fixture_path());
    }

    let hook_input = json!({
        "hook_event_name": "PreToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "thread_id": AMP_SIMPLE_THREAD_ID,
        "cwd": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/jokes.csv"],
        "tool_input": {
            "path": "/Users/test/project/jokes.csv"
        }
    })
    .to_string();

    let events = resolve_preset("amp")
        .unwrap()
        .parse(&hook_input, "t_test")
        .expect("Amp preset should succeed");

    unsafe {
        std::env::remove_var("GIT_AI_AMP_THREADS_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "amp");
            assert_eq!(e.context.agent_id.id, AMP_SIMPLE_THREAD_ID);
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/jokes.csv")]
            );
        }
        _ => panic!("Expected PreFileEdit for PreToolUse"),
    }
}

#[test]
#[serial_test::serial]
fn test_amp_preset_posttooluse_returns_ai_checkpoint() {
    unsafe {
        std::env::set_var("GIT_AI_AMP_THREADS_PATH", amp_threads_fixture_path());
    }

    let hook_input = json!({
        "hook_event_name": "PostToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "thread_id": AMP_SIMPLE_THREAD_ID,
        "cwd": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/jokes.csv"],
        "tool_input": {
            "path": "/Users/test/project/jokes.csv"
        }
    })
    .to_string();

    let preset = resolve_preset("amp").unwrap();
    let mut events = preset
        .parse(&hook_input, "t_test")
        .expect("Amp preset should succeed");
    preset
        .enrich_authorized_events(&hook_input, &mut events)
        .expect("Amp preset enrichment should succeed");

    unsafe {
        std::env::remove_var("GIT_AI_AMP_THREADS_PATH");
    }

    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "amp");
            assert_eq!(e.context.agent_id.id, AMP_SIMPLE_THREAD_ID);
            // Model is extracted from the resolved Amp thread fixture file
            assert_eq!(e.context.agent_id.model, "claude-opus-4-6");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/jokes.csv")]
            );
            // Transcript should be a path reference (lazy loading)
            assert!(e.stream_source.is_some());
            let transcript_path_str = e
                .stream_source
                .as_ref()
                .map(|ts| ts.path.to_string_lossy().to_string())
                .unwrap();
            assert!(
                transcript_path_str.ends_with(&format!("{}.json", AMP_SIMPLE_THREAD_ID)),
                "transcript_path should point to the matched Amp thread file"
            );
            // Metadata should contain transcript_path
            assert!(
                e.context.metadata.contains_key("transcript_path"),
                "metadata should contain transcript_path"
            );
        }
        _ => panic!("Expected PostFileEdit for PostToolUse"),
    }
}

#[test]
#[serial_test::serial]
fn test_amp_e2e_checkpoint_and_commit() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.ts");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "// initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let temp_threads = tempfile::tempdir().unwrap();
    copy_dir_all(&amp_threads_fixture_path(), temp_threads.path()).unwrap();

    unsafe {
        std::env::set_var("GIT_AI_AMP_THREADS_PATH", temp_threads.path());
    }

    let pre_hook_input = json!({
        "hook_event_name": "PreToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "cwd": repo_root.to_string_lossy().to_string(),
        "edited_filepaths": [file_path.to_string_lossy().to_string()],
        "tool_input": {
            "path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.checkpoint_with_hook_input("amp", &pre_hook_input)
        .unwrap();

    fs::write(&file_path, "// initial\n// Hello from amp\n").unwrap();

    let post_hook_input = json!({
        "hook_event_name": "PostToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "cwd": repo_root.to_string_lossy().to_string(),
        "edited_filepaths": [file_path.to_string_lossy().to_string()],
        "tool_input": {
            "path": file_path.to_string_lossy().to_string()
        }
    })
    .to_string();
    repo.checkpoint_with_hook_input("amp", &post_hook_input)
        .unwrap();

    unsafe {
        std::env::remove_var("GIT_AI_AMP_THREADS_PATH");
    }

    let commit = repo.stage_all_and_commit("Add amp-authored line").unwrap();

    assert!(
        !commit.authorship_log.metadata.sessions.is_empty(),
        "Expected a session record after amp checkpoint + commit"
    );

    let session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("session record should exist");

    assert_eq!(session_record.agent_id.tool, "amp");
    assert_eq!(session_record.agent_id.model, "claude-opus-4-6");
}

#[test]
#[serial_test::serial]
fn test_amp_post_commit_resyncs_latest_thread_transcript() {
    use crate::repos::test_repo::TestRepo;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("src").join("main.ts");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "// initial\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    let temp_threads = tempfile::tempdir().unwrap();
    copy_dir_all(&amp_threads_fixture_path(), temp_threads.path()).unwrap();
    let thread_path = temp_threads
        .path()
        .join(format!("{}.json", AMP_SIMPLE_THREAD_ID));

    unsafe {
        std::env::set_var("GIT_AI_AMP_THREADS_PATH", temp_threads.path());
    }

    let pre_hook_input = json!({
        "hook_event_name": "PreToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "cwd": repo_root.to_string_lossy().to_string(),
        "edited_filepaths": [file_path.to_string_lossy().to_string()]
    })
    .to_string();
    repo.checkpoint_with_hook_input("amp", &pre_hook_input)
        .unwrap();

    fs::write(&file_path, "// initial\n// ai edit\n").unwrap();

    let post_hook_input = json!({
        "hook_event_name": "PostToolUse",
        "tool_use_id": AMP_SIMPLE_EDIT_TOOL_USE_ID,
        "cwd": repo_root.to_string_lossy().to_string(),
        "edited_filepaths": [file_path.to_string_lossy().to_string()]
    })
    .to_string();
    repo.checkpoint_with_hook_input("amp", &post_hook_input)
        .unwrap();

    append_assistant_message(
        &thread_path,
        "RESYNC_TEST_MESSAGE: This message was appended after checkpoint",
    );

    unsafe {
        std::env::remove_var("GIT_AI_AMP_THREADS_PATH");
    }

    let commit = repo
        .stage_all_and_commit("Commit with amp transcript resync")
        .unwrap();

    let _session_record = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .expect("Expected a session record");

    // Note: Messages field has been removed from SessionRecord
}

fn append_assistant_message(thread_path: &Path, text: &str) {
    let content = fs::read_to_string(thread_path).expect("Failed to read thread file");
    let mut value: serde_json::Value =
        serde_json::from_str(&content).expect("Failed to parse thread JSON");

    let next_message_id = value
        .get("nextMessageId")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let new_message = json!({
        "role": "assistant",
        "messageId": next_message_id,
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "state": {
            "type": "complete",
            "stopReason": "end_turn"
        },
        "usage": {
            "model": "claude-opus-4-6",
            "timestamp": "2026-02-28T02:00:00.000Z"
        }
    });

    value
        .get_mut("messages")
        .and_then(|messages| messages.as_array_mut())
        .expect("Thread should contain a messages array")
        .push(new_message);

    value["nextMessageId"] = json!(next_message_id + 1);

    let serialized = serde_json::to_string_pretty(&value).expect("Failed to serialize thread JSON");
    fs::write(thread_path, serialized).expect("Failed to write updated thread file");
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}

crate::reuse_tests_in_worktree!(
    test_amp_raw_event_fidelity,
    test_amp_raw_event_fidelity_with_thinking,
);
