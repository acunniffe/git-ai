//! Wire helpers for git-ai's internal machine-JSON commands (e.g.
//! `effective-ignore-patterns`, `blame-analysis`, `fetch-authorship-notes`):
//! parsing the single `--json <payload>` argument shape and emitting
//! single-line JSON success/error output. These commands' output shapes are
//! a public contract — see `docs/contracts/cli-output.md`.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::cli::hook_input::strip_utf8_bom;
use crate::operations::git::find_repository;
use crate::operations::git::repository::Repository;

pub(crate) fn parse_machine_json_arg(args: &[String], command: &str) -> Result<String, String> {
    if args.len() != 2 || args[0] != "--json" {
        return Err(format!("Usage: git-ai {} --json '<json-payload>'", command));
    }

    let payload = strip_utf8_bom(args[1].clone());
    if payload.trim().is_empty() {
        return Err("JSON payload cannot be empty".to_string());
    }

    Ok(payload)
}

pub(crate) fn emit_machine_json_error(message: impl AsRef<str>) -> ! {
    let payload = serde_json::json!({ "error": message.as_ref() });
    if let Ok(json) = serde_json::to_string(&payload) {
        eprintln!("{}", json);
    } else {
        eprintln!(r#"{{"error":"failed to serialize error payload"}}"#);
    }
    std::process::exit(1);
}

pub(crate) fn print_machine_json(value: &serde_json::Value) {
    match serde_json::to_string(value) {
        Ok(json) => println!("{}", json),
        Err(e) => emit_machine_json_error(format!("Failed to serialize JSON output: {}", e)),
    }
}

/// Parse a machine-JSON request payload into `T`, exiting with a
/// single-line JSON error (and non-zero status) on malformed input.
pub(crate) fn parse_machine_request<T: DeserializeOwned>(payload: &str) -> T {
    serde_json::from_str(payload)
        .unwrap_or_else(|e| emit_machine_json_error(format!("Invalid JSON payload: {}", e)))
}

/// Resolve the repository for the current directory, exiting with a
/// single-line JSON error (and non-zero status) if none is found.
pub(crate) fn resolve_repo_or_machine_error() -> Repository {
    find_repository(&Vec::<String>::new())
        .unwrap_or_else(|e| emit_machine_json_error(format!("Failed to find repository: {}", e)))
}

/// Serialize `value` and print it as a single-line machine-JSON response,
/// exiting with a single-line JSON error (and non-zero status) if
/// serialization fails.
pub(crate) fn print_machine_json_serializable<T: Serialize>(value: &T) {
    let response_value = serde_json::to_value(value).unwrap_or_else(|e| {
        emit_machine_json_error(format!("Failed to serialize command response: {}", e))
    });
    print_machine_json(&response_value);
}
