use crate::error::GitAiError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn required_str<'a>(data: &'a Value, key: &str) -> Result<&'a str, GitAiError> {
    data.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| GitAiError::PresetError(format!("{} not found in hook_input", key)))
}

pub fn required_str_multi<'a>(data: &'a Value, keys: &[&str]) -> Result<&'a str, GitAiError> {
    optional_str_multi(data, keys).ok_or_else(|| {
        GitAiError::PresetError(format!("{} not found in hook_input", keys.join(" or ")))
    })
}

pub fn is_pre_hook_event(name: Option<&str>) -> bool {
    matches!(
        name,
        Some("PreToolUse") | Some("preToolUse") | Some("pre_tool_use") | Some("BeforeTool")
    )
}

pub fn optional_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(|v| v.as_str())
}

pub fn optional_str_multi<'a>(data: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| data.get(*key).and_then(|v| v.as_str()))
}

pub fn str_or_default<'a>(data: &'a Value, key: &str, default: &'a str) -> &'a str {
    data.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

pub fn str_or_default_multi<'a>(data: &'a Value, keys: &[&str], default: &'a str) -> &'a str {
    keys.iter()
        .find_map(|key| data.get(*key).and_then(|v| v.as_str()))
        .unwrap_or(default)
}

pub fn required_file_stem(data: &Value, path_key: &str) -> Result<String, GitAiError> {
    let path_str = required_str(data, path_key)?;
    Path::new(path_str)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            GitAiError::PresetError(format!("Could not extract file stem from {}", path_key))
        })
}

pub fn resolve_absolute(path: &str, cwd: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

pub fn file_paths_from_tool_input(data: &Value, cwd: &str) -> Vec<PathBuf> {
    let tool_input = match data.get("tool_input").or_else(|| data.get("toolInput")) {
        Some(ti) => ti,
        None => return vec![],
    };

    // Try single file_path field
    if let Some(path) = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("filepath"))
        .or_else(|| tool_input.get("path"))
        .and_then(|v| v.as_str())
        && !path.is_empty()
    {
        return vec![resolve_absolute(path, cwd)];
    }

    // Try array fields
    if let Some(arr) = tool_input
        .get("file_paths")
        .or_else(|| tool_input.get("filepaths"))
        .or_else(|| tool_input.get("files"))
        .and_then(|v| v.as_array())
    {
        let paths: Vec<PathBuf> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|p| resolve_absolute(p, cwd))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }

    vec![]
}

pub fn bash_command_from_hook_input(data: &Value) -> Option<String> {
    for (container, key) in [
        ("tool_input", "command"),
        ("toolInput", "command"),
        ("tool_input", "cmd"),
        ("toolInput", "cmd"),
    ] {
        if let Some(command) = data
            .get(container)
            .and_then(|v| v.get(key))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(command.to_string());
        }
    }

    optional_str_multi(data, &["command", "cmd"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Extract file paths from apply_patch / `ApplyPatch` tool text format.
///
/// Several presets (Codex/OpenAI-style) embed edited paths in the patch text
/// rather than in JSON keys. Parses `*** Update File:`, `*** Add File:`,
/// `*** Delete File:`, and `*** Move to:` prefixes, trimming surrounding
/// whitespace and any wrapping quotes, and appends each unique path to `out`.
pub fn collect_apply_patch_paths_from_text(raw: &str, out: &mut Vec<String>) {
    for line in raw.lines() {
        let trimmed = line.trim();
        let maybe_path = trimmed
            .strip_prefix("*** Update File: ")
            .or_else(|| trimmed.strip_prefix("*** Add File: "))
            .or_else(|| trimmed.strip_prefix("*** Delete File: "))
            .or_else(|| trimmed.strip_prefix("*** Move to: "));

        if let Some(path) = maybe_path {
            let path = path.trim().trim_matches('"').trim_matches('\'');
            if !path.is_empty() && !out.iter().any(|existing| existing == path) {
                out.push(path.to_string());
            }
        }
    }
}

pub fn dirty_files_from_value(data: &Value, cwd: &str) -> Option<HashMap<PathBuf, String>> {
    let df = data.get("dirty_files")?;
    let obj = df.as_object()?;
    let mut result = HashMap::new();
    for (key, value) in obj {
        if let Some(content) = value.as_str() {
            let path = resolve_absolute(key, cwd);
            result.insert(path, content.to_string());
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

pub fn string_array(data: &Value, key: &str) -> Option<Vec<String>> {
    let arr = data.get(key)?.as_array()?;
    let strings: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.to_string())
        .collect();
    if strings.is_empty() {
        None
    } else {
        Some(strings)
    }
}

pub fn pathbuf_array(data: &Value, key: &str, cwd: &str) -> Vec<PathBuf> {
    string_array(data, key)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|s| resolve_absolute(&s, cwd))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_required_str_present() {
        let data = json!({"cwd": "/home/user/project"});
        assert_eq!(required_str(&data, "cwd").unwrap(), "/home/user/project");
    }

    #[test]
    fn test_required_str_missing() {
        let data = json!({"other": "value"});
        assert!(required_str(&data, "cwd").is_err());
    }

    #[test]
    fn test_optional_str_present() {
        let data = json!({"tool_name": "Write"});
        assert_eq!(optional_str(&data, "tool_name"), Some("Write"));
    }

    #[test]
    fn test_optional_str_missing() {
        let data = json!({"other": "value"});
        assert_eq!(optional_str(&data, "tool_name"), None);
    }

    #[test]
    fn test_str_or_default_present() {
        let data = json!({"tool_use_id": "abc123"});
        assert_eq!(str_or_default(&data, "tool_use_id", "bash"), "abc123");
    }

    #[test]
    fn test_str_or_default_missing() {
        let data = json!({"other": "value"});
        assert_eq!(str_or_default(&data, "tool_use_id", "bash"), "bash");
    }

    #[test]
    fn test_required_file_stem() {
        let data = json!({"transcript_path": "/home/user/.claude/projects/abc123.jsonl"});
        assert_eq!(
            required_file_stem(&data, "transcript_path").unwrap(),
            "abc123"
        );
    }

    #[test]
    fn test_resolve_absolute_already_absolute() {
        let result = resolve_absolute("/home/user/file.txt", "/some/cwd");
        assert_eq!(result, PathBuf::from("/home/user/file.txt"));
    }

    #[test]
    fn test_resolve_absolute_relative() {
        let result = resolve_absolute("src/main.rs", "/home/user/project");
        assert_eq!(result, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_file_paths_from_tool_input_single() {
        let data = json!({
            "tool_input": {"file_path": "src/main.rs"}
        });
        let paths = file_paths_from_tool_input(&data, "/home/user/project");
        assert_eq!(paths, vec![PathBuf::from("/home/user/project/src/main.rs")]);
    }

    #[test]
    fn test_file_paths_from_tool_input_missing() {
        let data = json!({"tool_input": {"command": "ls"}});
        let paths = file_paths_from_tool_input(&data, "/home/user/project");
        assert!(paths.is_empty());
    }

    #[test]
    fn test_bash_command_from_tool_input_command() {
        let data = json!({"tool_input": {"command": "echo hello"}});
        assert_eq!(
            bash_command_from_hook_input(&data).as_deref(),
            Some("echo hello")
        );
    }

    #[test]
    fn test_bash_command_from_tool_input_cmd_camel_case() {
        let data = json!({"toolInput": {"cmd": "npm test"}});
        assert_eq!(
            bash_command_from_hook_input(&data).as_deref(),
            Some("npm test")
        );
    }

    #[test]
    fn test_bash_command_from_top_level_command() {
        let data = json!({"command": "cargo check"});
        assert_eq!(
            bash_command_from_hook_input(&data).as_deref(),
            Some("cargo check")
        );
    }

    #[test]
    fn test_bash_command_missing_or_empty() {
        let data = json!({"tool_input": {"command": "   "}});
        assert_eq!(bash_command_from_hook_input(&data), None);
    }

    #[test]
    fn test_required_str_multi_falls_back() {
        let data = json!({"workspaceRoot": "/tmp/proj"});
        assert_eq!(
            required_str_multi(&data, &["cwd", "workspaceRoot"]).unwrap(),
            "/tmp/proj"
        );
    }

    #[test]
    fn test_required_str_multi_missing() {
        let data = json!({"other": "value"});
        let err = required_str_multi(&data, &["cwd", "workspaceRoot"]).unwrap_err();
        assert!(err.to_string().contains("cwd or workspaceRoot"));
    }

    #[test]
    fn test_is_pre_hook_event() {
        assert!(is_pre_hook_event(Some("PreToolUse")));
        assert!(is_pre_hook_event(Some("pre_tool_use")));
        assert!(is_pre_hook_event(Some("preToolUse")));
        assert!(is_pre_hook_event(Some("BeforeTool")));
        assert!(!is_pre_hook_event(Some("PostToolUse")));
        assert!(!is_pre_hook_event(Some("post_tool_use")));
        assert!(!is_pre_hook_event(None));
    }

    #[test]
    fn test_optional_str_multi_key() {
        let data = json!({"hookEventName": "PreToolUse"});
        let result = optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        assert_eq!(result, Some("PreToolUse"));
    }

    #[test]
    fn test_optional_str_multi_key_missing() {
        let data = json!({"other": "value"});
        let result = optional_str_multi(&data, &["hook_event_name", "hookEventName"]);
        assert_eq!(result, None);
    }

    #[test]
    fn test_collect_apply_patch_paths_from_text() {
        let text = "\
*** Begin Patch
*** Update File: src/main.rs
@@
-old line
+new line
*** Add File: src/new.rs
@@
+content
*** Delete File: src/gone.rs
*** Move to: src/moved.rs
*** End Patch
";
        let mut paths = Vec::new();
        collect_apply_patch_paths_from_text(text, &mut paths);
        assert_eq!(
            paths,
            vec![
                "src/main.rs".to_string(),
                "src/new.rs".to_string(),
                "src/gone.rs".to_string(),
                "src/moved.rs".to_string(),
            ]
        );
    }

    #[test]
    fn test_collect_apply_patch_paths_dedupes_and_trims_quotes() {
        let text = "\
*** Update File: \"src/main.rs\"
*** Update File: src/main.rs
*** Add File: 'src/other.rs'
";
        let mut paths = Vec::new();
        collect_apply_patch_paths_from_text(text, &mut paths);
        assert_eq!(
            paths,
            vec!["src/main.rs".to_string(), "src/other.rs".to_string()]
        );
    }

    #[test]
    fn test_dirty_files_from_hook_data() {
        let data = json!({
            "dirty_files": {
                "/home/user/file.txt": "old content"
            }
        });
        let result = dirty_files_from_value(&data, "/home/user");
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(
            map.get(&PathBuf::from("/home/user/file.txt")).unwrap(),
            "old content"
        );
    }
}
