use crate::error::GitAiError;
use std::path::{Path, PathBuf};

pub(super) fn resolve_transcript_path(
    transcript_path: Option<&str>,
    thread_id: Option<&str>,
    tool_use_id: Option<&str>,
) -> Option<PathBuf> {
    if let Some(path) = transcript_path {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(env_path) = std::env::var("AMP_THREAD_PATH")
        && !env_path.trim().is_empty()
    {
        let path = PathBuf::from(&env_path);
        if path.exists() {
            return Some(path);
        }
    }

    if let Ok(threads_dir) = amp_threads_dir() {
        if threads_dir.is_file()
            && threads_dir
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            return Some(threads_dir);
        }

        if let Some(thread_id) = thread_id {
            let candidate = threads_dir.join(format!("{}.json", thread_id));
            if candidate.exists() {
                return Some(candidate);
            }
        }

        if let Some(tool_use_id) = tool_use_id
            && let Some(path) = find_thread_file_by_tool_use_id(&threads_dir, tool_use_id)
        {
            return Some(path);
        }
    }

    None
}

fn find_thread_file_by_tool_use_id(threads_dir: &Path, tool_use_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(threads_dir).ok()?;
    let mut newest_match: Option<(PathBuf, std::time::SystemTime)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        if !content.contains(tool_use_id) {
            continue;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&content) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let has_match = parsed
            .get("messages")
            .and_then(|value| value.as_array())
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message
                        .get("content")
                        .and_then(|value| value.as_array())
                        .is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block.get("type").and_then(|value| value.as_str())
                                    == Some("tool_use")
                                    && block.get("id").and_then(|value| value.as_str())
                                        == Some(tool_use_id)
                            })
                        })
                })
            });
        if !has_match {
            continue;
        }

        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        match &newest_match {
            Some((_, newest_modified)) if modified <= *newest_modified => {}
            _ => newest_match = Some((path, modified)),
        }
    }

    newest_match.map(|(path, _)| path)
}

fn amp_threads_dir() -> Result<PathBuf, GitAiError> {
    if let Ok(test_path) = std::env::var("GIT_AI_AMP_THREADS_PATH") {
        return Ok(PathBuf::from(test_path));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
            return Ok(PathBuf::from(xdg_data).join("amp").join("threads"));
        }

        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(home
            .join(".local")
            .join("share")
            .join("amp")
            .join("threads"))
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            return Ok(PathBuf::from(local_app_data).join("amp").join("threads"));
        }
        if let Ok(app_data) = std::env::var("APPDATA") {
            return Ok(PathBuf::from(app_data).join("amp").join("threads"));
        }

        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(home
            .join("AppData")
            .join("Local")
            .join("amp")
            .join("threads"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(GitAiError::Generic(
            "Amp threads path not supported on this platform".to_string(),
        ))
    }
}
