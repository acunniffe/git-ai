use super::{StreamFormat, StreamSource};
use crate::error::GitAiError;
use crate::model::authorship_log_serialization::generate_session_id;
use std::path::{Path, PathBuf};

pub(super) fn stream_source_and_model(session_id: &str) -> Option<(StreamSource, String)> {
    let opencode_path = if let Ok(test_path) = std::env::var("GIT_AI_OPENCODE_STORAGE_PATH") {
        PathBuf::from(test_path)
    } else {
        opencode_data_path().ok()?
    };
    let db_path = resolve_sqlite_db_path(&opencode_path)?;
    let parent_id = lookup_parent_session(&db_path, session_id);
    let model = crate::operations::streams::model_extraction::extract_model(
        &db_path,
        crate::operations::streams::sweep::StreamFormat::OpenCodeSqlite,
        Some(session_id),
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| "unknown".to_string());

    Some((
        StreamSource {
            path: db_path,
            format: StreamFormat::OpenCodeSqlite,
            session_id: generate_session_id(session_id, "opencode"),
            external_session_id: session_id.to_string(),
            external_parent_session_id: parent_id,
        },
        model,
    ))
}

fn lookup_parent_session(db_path: &Path, session_id: &str) -> Option<String> {
    let connection =
        crate::operations::streams::agents::opencode::open_sqlite_readonly(db_path).ok()?;
    connection
        .query_row(
            "SELECT parent_id FROM session WHERE id = ?",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
}

fn opencode_data_path() -> Result<PathBuf, GitAiError> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(home.join(".local").join("share").join("opencode"))
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
            Ok(PathBuf::from(xdg_data).join("opencode"))
        } else {
            let home = dirs::home_dir().ok_or_else(|| {
                GitAiError::Generic("Could not determine home directory".to_string())
            })?;
            Ok(home.join(".local").join("share").join("opencode"))
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(app_data) = std::env::var("APPDATA") {
            Ok(PathBuf::from(app_data).join("opencode"))
        } else if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            Ok(PathBuf::from(local_app_data).join("opencode"))
        } else {
            Err(GitAiError::Generic(
                "Neither APPDATA nor LOCALAPPDATA is set".to_string(),
            ))
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(GitAiError::PresetError(
            "OpenCode storage path not supported on this platform".to_string(),
        ))
    }
}

fn resolve_sqlite_db_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name == "opencode.db")
            .map(|_| path.to_path_buf());
    }

    if !path.is_dir() {
        return None;
    }

    let direct_db = path.join("opencode.db");
    if direct_db.exists() {
        return Some(direct_db);
    }

    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "storage")
    {
        let sibling_db = path.parent()?.join("opencode.db");
        if sibling_db.exists() {
            return Some(sibling_db);
        }
    }

    None
}
