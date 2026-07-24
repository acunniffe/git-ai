//! Trace2 self-checks: verifies that global git config points trace2 events
//! at the git-ai daemon, and that a real git command actually emits the
//! expected trace2 event sequence to a file.

use crate::operations::daemon::self_check::{
    CommandRecord, DEBUG_CHECK_TIMEOUT, DiagnosticCheckResult, GitDiagnosticTarget, format_status,
    run_logged_command, run_required_until, sanitize_label,
};
use serde_json::Value;
use std::fs;
use std::time::Instant;

const TRACE2_EVENT_TARGET_KEY: &str = "trace2.eventTarget";
const TRACE2_EVENT_NESTING_KEY: &str = "trace2.eventNesting";
const TRACE2_EVENT_NESTING_VALUE: &str = "0";

pub fn check_trace2_global_config(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let expected_target = match crate::operations::daemon::DaemonConfig::from_env_or_default_paths()
    {
        Ok(config) => config.trace2_event_target(),
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &format!("failed to determine expected trace2 target: {}", err),
                    None,
                    None,
                    None,
                ),
                commands,
            );
        }
    };

    let event_targets =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_TARGET_KEY);
    let event_nesting =
        read_global_git_config_values(&mut commands, &target.program, TRACE2_EVENT_NESTING_KEY);

    let event_targets = match event_targets {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(&err, Some(&expected_target), None, None),
                commands,
            );
        }
    };
    let event_nesting = match event_nesting {
        Ok(values) => values,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 global config could not be inspected",
                trace2_config_failure_details(
                    &err,
                    Some(&expected_target),
                    Some(&event_targets),
                    None,
                ),
                commands,
            );
        }
    };

    let target_matches = event_targets.iter().any(|value| value == &expected_target);
    let nesting_matches = event_nesting
        .iter()
        .any(|value| value == TRACE2_EVENT_NESTING_VALUE);

    if target_matches && nesting_matches {
        return DiagnosticCheckResult::passed(
            "trace2 global config is configured",
            vec![
                format!("{}: {}", TRACE2_EVENT_TARGET_KEY, expected_target),
                format!(
                    "{}: {}",
                    TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
                ),
            ],
            commands,
        );
    }

    DiagnosticCheckResult::failed(
        "trace2 global config is not configured",
        trace2_config_failure_details(
            "trace2 is not configured for git-ai daemon mode",
            Some(&expected_target),
            Some(&event_targets),
            Some(&event_nesting),
        ),
        commands,
    )
}

pub fn run_trace2_file_self_check(target: &GitDiagnosticTarget) -> DiagnosticCheckResult {
    let mut commands = Vec::new();
    let deadline = Instant::now() + DEBUG_CHECK_TIMEOUT;
    let trace_dir = crate::operations::mdm::paths::home_dir()
        .join(".git-ai")
        .join("internal")
        .join("daemon");
    let trace_path = trace_dir.join(format!(
        "trace2-debug-check-{}-{}.json",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));
    let trace_command_dir = crate::diagnostic_sentinels::debug_self_check_root().join(format!(
        "trace2-{}-{}",
        sanitize_label(&target.label),
        crate::uuid::generate_v4()
    ));

    let snapshot = match snapshot_global_trace2_event_target(&mut commands, &target.program) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            return DiagnosticCheckResult::failed(
                "trace2 file self-check failed",
                vec![err],
                commands,
            );
        }
    };

    let mut changed_global_event_target = false;
    let result = (|| -> Result<(Vec<String>, String), String> {
        fs::create_dir_all(&trace_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_dir.display(), e))?;
        fs::create_dir_all(&trace_command_dir)
            .map_err(|e| format!("failed to create {}: {}", trace_command_dir.display(), e))?;
        let _ = fs::remove_file(&trace_path);
        let trace_path_string = trace_path.to_string_lossy().to_string();

        // This intentionally uses global git config rather than a process-local
        // GIT_TRACE2_EVENT override so the diagnostic exercises the install path.
        run_required_until(
            &mut commands,
            &target.program,
            &[
                "config",
                "--global",
                "--replace-all",
                TRACE2_EVENT_TARGET_KEY,
                trace_path_string.as_str(),
            ],
            None,
            deadline,
        )?;
        changed_global_event_target = true;

        // Use init rather than version: when terminal git is the git-ai proxy,
        // read-only commands intentionally suppress trace2 before invoking real git.
        run_required_until(
            &mut commands,
            &target.program,
            &["init", "."],
            Some(&trace_command_dir),
            deadline,
        )?;

        let trace2_json = fs::read_to_string(&trace_path)
            .map_err(|e| format!("failed to read {}: {}", trace_path.display(), e))?;
        let details = validate_trace2_command_events(&trace2_json, "init")?;
        Ok((details, trace2_json))
    })();

    let restore_result = if changed_global_event_target {
        restore_global_trace2_event_target(&mut commands, &target.program, &snapshot)
    } else {
        Ok(())
    };
    let _ = fs::remove_file(&trace_path);
    let _ = fs::remove_dir_all(&trace_command_dir);

    match (result, restore_result) {
        (Ok((mut details, trace2_json)), Ok(())) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            DiagnosticCheckResult::passed("trace2 file self-check completed", details, commands)
                .with_trace2_json(Some(trace2_json))
        }
        (Ok((mut details, trace2_json)), Err(restore_err)) => {
            details.insert(0, format!("trace2 file: {}", trace_path.display()));
            details.insert(1, format!("command dir: {}", trace_command_dir.display()));
            details.push(format!("restore failed: {}", restore_err));
            DiagnosticCheckResult::failed("trace2 file self-check failed", details, commands)
                .with_trace2_json(Some(trace2_json))
        }
        (Err(err), Ok(())) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
            ],
            commands,
        ),
        (Err(err), Err(restore_err)) => DiagnosticCheckResult::failed(
            "trace2 file self-check failed",
            vec![
                format!("trace2 file: {}", trace_path.display()),
                format!("command dir: {}", trace_command_dir.display()),
                err,
                format!("restore failed: {}", restore_err),
            ],
            commands,
        ),
    }
}

#[derive(Debug, Clone, Default)]
struct Trace2EventTargetSnapshot {
    values: Vec<String>,
}

fn snapshot_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
) -> Result<Trace2EventTargetSnapshot, String> {
    let record = run_logged_command(
        git_program,
        &[
            "config",
            "--global",
            "--no-includes",
            "--get-all",
            TRACE2_EVENT_TARGET_KEY,
        ],
        None,
    );
    let snapshot = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to snapshot global {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(Trace2EventTargetSnapshot { values: snapshot })
}

fn restore_global_trace2_event_target(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    snapshot: &Trace2EventTargetSnapshot,
) -> Result<(), String> {
    let remove = run_logged_command(
        git_program,
        &["config", "--global", "--unset-all", TRACE2_EVENT_TARGET_KEY],
        None,
    );
    let remove_ok = remove.success() || remove.status == Some(5);
    let remove_error = if remove_ok {
        None
    } else {
        Some(format!(
            "failed to remove temporary {}: status={}, stderr={}",
            TRACE2_EVENT_TARGET_KEY,
            format_status(remove.status),
            remove.stderr
        ))
    };
    commands.push(remove);
    if let Some(error) = remove_error {
        return Err(error);
    }

    for value in &snapshot.values {
        let record = run_logged_command(
            git_program,
            &[
                "config",
                "--global",
                "--add",
                TRACE2_EVENT_TARGET_KEY,
                value,
            ],
            None,
        );
        let error = if record.success() {
            None
        } else {
            Some(format!(
                "failed to restore {}: status={}, stderr={}",
                TRACE2_EVENT_TARGET_KEY,
                format_status(record.status),
                record.stderr
            ))
        };
        commands.push(record);
        if let Some(error) = error {
            return Err(error);
        }
    }

    Ok(())
}

fn validate_trace2_command_events(
    trace2_json: &str,
    expected_command: &str,
) -> Result<Vec<String>, String> {
    let mut events = Vec::new();
    for (idx, line) in trace2_json.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("invalid trace2 JSON on line {}: {}", idx + 1, e))?;
        events.push(value);
    }

    if events.is_empty() {
        return Err("trace2 file was empty".to_string());
    }

    let has_version = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("version"));
    let has_start = events
        .iter()
        .any(|event| event.get("event").and_then(Value::as_str) == Some("start"));
    let has_cmd_name_expected = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("cmd_name")
            && event.get("name").and_then(Value::as_str) == Some(expected_command)
    });
    let has_exit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("exit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });
    let has_atexit_zero = events.iter().any(|event| {
        event.get("event").and_then(Value::as_str) == Some("atexit")
            && event.get("code").and_then(Value::as_i64) == Some(0)
    });

    let failures = [
        (has_version, "missing version event"),
        (has_start, "missing start event"),
        (
            has_cmd_name_expected,
            "missing cmd_name event for expected command",
        ),
        (has_exit_zero, "missing exit event with code 0"),
        (has_atexit_zero, "missing atexit event with code 0"),
    ]
    .into_iter()
    .filter_map(|(ok, msg)| (!ok).then_some(msg))
    .collect::<Vec<_>>();

    if !failures.is_empty() {
        return Err(format!("unexpected trace2 events: {}", failures.join(", ")));
    }

    Ok(vec![
        format!("events: {}", events.len()),
        format!(
            "validated: version/start/cmd_name({})/exit(0)/atexit(0)",
            expected_command
        ),
    ])
}

fn read_global_git_config_values(
    commands: &mut Vec<CommandRecord>,
    git_program: &str,
    key: &str,
) -> Result<Vec<String>, String> {
    let record = run_logged_command(git_program, &["config", "--global", "--get-all", key], None);
    let values = if record.success() {
        record
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    } else if record.status == Some(1) {
        Vec::new()
    } else {
        let err = format!(
            "failed to read global {}: status={}, stderr={}",
            key,
            format_status(record.status),
            record.stderr
        );
        commands.push(record);
        return Err(err);
    };
    commands.push(record);
    Ok(values)
}

fn trace2_config_failure_details(
    reason: &str,
    expected_target: Option<&str>,
    actual_targets: Option<&[String]>,
    actual_nesting: Option<&[String]>,
) -> Vec<String> {
    let mut details = vec![
        format!("ERROR: {}", reason),
        "Why this matters: git-ai daemon mode relies on Git trace2 events to match real Git commands to checkpoint and authorship state; without this config, commit/rebase/merge attribution can be missed or delayed.".to_string(),
    ];

    if let Some(expected_target) = expected_target {
        details.push(format!(
            "Expected {}: {}",
            TRACE2_EVENT_TARGET_KEY, expected_target
        ));
    }
    if let Some(actual_targets) = actual_targets {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_TARGET_KEY,
            format_config_values(actual_targets)
        ));
    }
    details.push(format!(
        "Expected {}: {}",
        TRACE2_EVENT_NESTING_KEY, TRACE2_EVENT_NESTING_VALUE
    ));
    if let Some(actual_nesting) = actual_nesting {
        details.push(format!(
            "Actual {}: {}",
            TRACE2_EVENT_NESTING_KEY,
            format_config_values(actual_nesting)
        ));
    }

    details.push("Common causes: `git-ai install-hooks` has not run, was run with `--dry-run`, or failed while writing global Git config.".to_string());
    details.push("Common causes: git-ai cannot edit the same global Git config Git reads because HOME/USERPROFILE/XDG_CONFIG_HOME/GIT_CONFIG_GLOBAL points somewhere different, the global config file or parent directory is read-only or locked, permissions or ownership are wrong, or the configured git and terminal git use different config locations.".to_string());
    details
}

fn format_config_values(values: &[String]) -> String {
    if values.is_empty() {
        "<missing>".to_string()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_trace2_command_events_accepts_expected_events() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"cmd_name","name":"init"}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let details = validate_trace2_command_events(trace, "init").unwrap();
        assert!(details.iter().any(|detail| detail == "events: 5"));
    }

    #[test]
    fn test_validate_trace2_command_events_rejects_missing_cmd_name() {
        let trace = r#"{"event":"version"}
{"event":"start","argv":["git","init","."]}
{"event":"exit","code":0}
{"event":"atexit","code":0}
"#;

        let err = validate_trace2_command_events(trace, "init").unwrap_err();
        assert!(err.contains("missing cmd_name event for expected command"));
    }

    #[test]
    fn test_trace2_config_failure_details_explains_missing_config() {
        let empty = Vec::new();
        let details = trace2_config_failure_details(
            "trace2 is not configured for git-ai daemon mode",
            Some("af_unix:stream:/tmp/git-ai-trace2.sock"),
            Some(&empty),
            Some(&empty),
        );

        assert!(details[0].contains("ERROR: trace2 is not configured"));
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Why this matters"))
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventTarget: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail == "Actual trace2.eventNesting: <missing>")
        );
        assert!(
            details
                .iter()
                .any(|detail| detail.contains("Common causes"))
        );
    }
}
