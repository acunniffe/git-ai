use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

use git_ai::error::GitAiError;
use git_ai::operations::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};

fn parse_ai_tab(hook_input: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
    resolve_preset("ai_tab")?.parse(hook_input, "t_test")
}

fn run_ai_tab_checkpoint(repo: &TestRepo, hook_payload: serde_json::Value) {
    let hook_input = hook_payload.to_string();
    match repo.checkpoint_with_hook_input("ai_tab", &hook_input) {
        Ok(output) => {
            println!("git_ai checkpoint output: {}", output);
        }
        Err(err) => {
            panic!("ai_tab checkpoint failed: {}", err);
        }
    }
}

#[test]
fn test_ai_tab_before_edit_checkpoint_includes_dirty_files() {
    let hook_input = json!({
        "hook_event_name": "before_edit",
        "tool": " github-copilot-tab ",
        "model": " default ",
        "repo_working_dir": " /Users/test/project ",
        "will_edit_filepaths": [
            "/Users/test/project/src/main.rs",
            "/Users/test/project/src/lib.rs"
        ],
        "completion_id": "checkpoint-123",
        "dirty_files": {
            "/Users/test/project/src/main.rs": "fn main() {\n    println!(\"hello world\");\n}\n",
            "/Users/test/project/src/lib.rs": "pub fn helper() {}\n"
        }
    })
    .to_string();

    let events = parse_ai_tab(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "github-copilot-tab");
            assert_eq!(e.context.agent_id.model, "default");
            assert_eq!(e.context.external_session_id, "ai_tab-checkpoint-123");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(dirty_files.values().any(|v| v.contains("hello world")));
            assert!(dirty_files.values().any(|v| v.contains("helper")));
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_ai_tab_after_edit_checkpoint_includes_dirty_files_and_paths() {
    let hook_input = json!({
        "hook_event_name": "after_edit",
        "tool": "github-copilot-tab",
        "model": "default",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": [
            "/Users/test/project/src/unused.rs"
        ],
        "edited_filepaths": [
            "/Users/test/project/src/main.rs"
        ],
        "completion_id": "checkpoint-456",
        "dirty_files": {
            "/Users/test/project/src/main.rs": "fn main() {\n    println!(\"goodbye world\");\n}\n"
        }
    })
    .to_string();

    let events = parse_ai_tab(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "github-copilot-tab");
            assert_eq!(e.context.agent_id.model, "default");
            assert_eq!(e.context.external_session_id, "ai_tab-checkpoint-456");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert!(!e.file_paths.is_empty());
            assert!(e.dirty_files.is_some());
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(dirty_files.values().any(|v| v.contains("goodbye world")));
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_ai_tab_rejects_invalid_hook_event() {
    let hook_input = json!({
        "hook_event_name": "during_edit",
        "tool": "github-copilot-tab",
        "model": "default"
    })
    .to_string();

    let result = parse_ai_tab(&hook_input);
    match result {
        Err(GitAiError::PresetError(message)) => {
            assert!(
                message.contains("Unsupported hook_event_name"),
                "unexpected error message: {}",
                message
            );
        }
        other => panic!(
            "expected PresetError for invalid hook_event_name, got {:?}",
            other
        ),
    }
}

#[test]
fn test_ai_tab_requires_non_empty_tool_and_model() {
    // Tool validation
    let hook_input_missing_tool = json!({
        "hook_event_name": "before_edit",
        "tool": "   ",
        "model": "default"
    })
    .to_string();
    let result = parse_ai_tab(&hook_input_missing_tool);
    match result {
        Err(GitAiError::PresetError(message)) => {
            assert!(
                message.contains("tool must be a non-empty string"),
                "unexpected error message: {}",
                message
            );
        }
        other => panic!("expected PresetError for empty tool, got {:?}", other),
    }

    // Model validation
    let hook_input_missing_model = json!({
        "hook_event_name": "after_edit",
        "tool": "github-copilot-tab",
        "model": ""
    })
    .to_string();
    let result = parse_ai_tab(&hook_input_missing_model);
    match result {
        Err(GitAiError::PresetError(message)) => {
            assert!(
                message.contains("model must be a non-empty string"),
                "unexpected error message: {}",
                message
            );
        }
        other => panic!("expected PresetError for empty model, got {:?}", other),
    }
}

#[test]
fn test_ai_tab_e2e_marks_ai_lines() {
    let repo = TestRepo::new();
    let relative_path = "notes_test.ts";
    let file_path = repo.canonical_path().join(relative_path);

    let base_content = "console.log(\"hello world\");\n".to_string();
    fs::write(&file_path, &base_content).unwrap();
    repo.stage_all_and_commit("Initial human commit").unwrap();

    let file_path_str = file_path.to_string_lossy().to_string();

    // Before edit checkpoint captures the pre-edit state
    run_ai_tab_checkpoint(
        &repo,
        json!({
            "hook_event_name": "before_edit",
            "tool": "github-copilot-tab",
            "model": "default",
            "repo_working_dir": repo.canonical_path().to_string_lossy(),
            "will_edit_filepaths": [file_path_str.clone()],
            "dirty_files": {
                file_path_str.clone(): base_content.clone()
            }
        }),
    );

    // AI tab inserts new lines alongside the existing content
    let ai_content =
        "console.log(\"hello world\");\n// Log hello world\nconsole.log(\"hello from ai\");\n"
            .to_string();
    fs::write(&file_path, &ai_content).unwrap();

    run_ai_tab_checkpoint(
        &repo,
        json!({
            "hook_event_name": "after_edit",
            "tool": "github-copilot-tab",
            "model": "default",
            "repo_working_dir": repo.canonical_path().to_string_lossy(),
            "edited_filepaths": [file_path_str.clone()],
            "dirty_files": {
                file_path_str.clone(): ai_content.clone()
            }
        }),
    );

    repo.stage_all_and_commit("Accept AI tab completion")
        .unwrap();

    let mut file = repo.filename(relative_path);
    file.assert_lines_and_blame(crate::lines![
        "console.log(\"hello world\");".human(),
        "// Log hello world".ai(),
        "console.log(\"hello from ai\");".ai(),
    ]);
}

#[test]
fn test_ai_tab_e2e_handles_dirty_files_map() {
    let repo = TestRepo::new();
    let lib_relative_path = std::path::Path::new("src").join("lib.rs");
    let lib_file_path = repo.path().join(lib_relative_path);
    // Create parent directory - handle Windows paths safely
    if let Some(parent) = lib_file_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let readme_relative_path = "README.md";
    let readme_file_path = repo.path().join(readme_relative_path);

    let base_lib_content = "fn greet() {\n    println!(\"hello\");\n}\n".to_string();
    fs::write(&lib_file_path, &base_lib_content).unwrap();
    let base_readme_content = "# Readme\n".to_string();
    fs::write(&readme_file_path, &base_readme_content).unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Human makes unrelated edits that remain dirty while AI tab runs
    let readme_dirty_content = "# Readme\nSome pending human notes.\n".to_string();
    fs::write(&readme_file_path, &readme_dirty_content).unwrap();

    let lib_file_path_str = lib_file_path.to_string_lossy().to_string();
    let readme_file_path_str = readme_file_path.to_string_lossy().to_string();

    let _working_logs = repo.current_working_logs();

    // Before edit snapshot includes all dirty files (AI target plus unrelated human edits)
    run_ai_tab_checkpoint(
        &repo,
        json!({
            "hook_event_name": "before_edit",
            "tool": "github-copilot-tab",
            "model": "default",
            "repo_working_dir": repo.canonical_path().to_string_lossy(),
            "will_edit_filepaths": [lib_file_path_str.clone()],
            "dirty_files": {
                lib_file_path_str.clone(): base_lib_content.clone(),
                readme_file_path_str.clone(): readme_dirty_content.clone()
            }
        }),
    );

    // AI tab updates the lib file contents while other dirty files remain human-authored
    let ai_content =
        "fn greet() {\n    println!(\"hello\");\n}\nfn ai_suggested() {\n    println!(\"from ai\");\n}\n"
            .to_string();
    fs::write(&lib_file_path, &ai_content).unwrap();

    let _working_logs = repo.current_working_logs();

    run_ai_tab_checkpoint(
        &repo,
        json!({
            "hook_event_name": "after_edit",
            "tool": "github-copilot-tab",
            "model": "default",
            "repo_working_dir": repo.canonical_path().to_string_lossy(),
            "edited_filepaths": [lib_file_path_str.clone()],
            "dirty_files": {
                lib_file_path_str.clone(): ai_content.clone(),
                readme_file_path_str.clone(): readme_dirty_content.clone()
            }
        }),
    );

    let _working_logs = repo.current_working_logs();

    let commit_result = repo
        .stage_all_and_commit("Record AI tab completion while other files dirty")
        .unwrap();

    commit_result.print_authorship();

    let mut file = repo.filename("src/lib.rs");
    file.assert_lines_and_blame(crate::lines![
        "fn greet() {".human(),
        "    println!(\"hello\");".human(),
        "}".human(),
        "fn ai_suggested() {".ai(),
        "    println!(\"from ai\");".ai(),
        "}".ai(),
    ]);
}

crate::reuse_tests_in_worktree!(
    test_ai_tab_before_edit_checkpoint_includes_dirty_files,
    test_ai_tab_after_edit_checkpoint_includes_dirty_files_and_paths,
    test_ai_tab_rejects_invalid_hook_event,
    test_ai_tab_requires_non_empty_tool_and_model,
    test_ai_tab_e2e_marks_ai_lines,
    test_ai_tab_e2e_handles_dirty_files_map,
);
