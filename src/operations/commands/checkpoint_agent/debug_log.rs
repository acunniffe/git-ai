use super::orchestrator::CheckpointRequest;
use crate::config;
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Serialize)]
struct CheckpointDebugLogEntry<'a> {
    timestamp: String,
    preset_name: &'a str,
    hook_input: &'a str,
    trace_id: &'a str,
    event_count: usize,
    requests: &'a [CheckpointRequest],
}

pub(super) fn write_checkpoint_debug_log(
    preset_name: &str,
    hook_input: &str,
    trace_id: &str,
    event_count: usize,
    requests: &[CheckpointRequest],
) {
    let Some(internal_dir) = config::internal_dir_path() else {
        return;
    };

    let log_dir = internal_dir.join("checkpoint-debug-logs");
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let log_path = log_dir.join(format!("{}.log", date));

    if let Err(e) = create_private_log_dir(&log_dir) {
        eprintln!("[checkpoint_debug_log] failed to create dir: {}", e);
        return;
    }

    cleanup_old_debug_logs(&log_dir);

    let entry = CheckpointDebugLogEntry {
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        preset_name,
        hook_input,
        trace_id,
        event_count,
        requests,
    };

    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };

    let Ok(mut file) = open_private_log_file(&log_path) else {
        return;
    };

    let _ = file
        .write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush());
}

fn create_private_log_dir(log_dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(log_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::File::open(log_dir)?.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn open_private_log_file(log_path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(log_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn cleanup_old_debug_logs(log_dir: &Path) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::days(14);

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(file_date) = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d")
            && file_date < cutoff.date_naive()
        {
            let _ = fs::remove_file(&path);
        }
    }
}
