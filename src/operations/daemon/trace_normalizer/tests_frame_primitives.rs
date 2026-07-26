use crate::operations::daemon::trace_helpers::{
    trace_payload_time_ns, trace_token_is_git_executable,
    trace_token_is_git_executable_ascii_case_insensitive,
};

use super::frame_helpers::{canonical_invocation, payload_timestamp_ns};

#[test]
fn timestamp_aggregators_retain_distinct_precedence_and_fallback_semantics() {
    let conflicting = serde_json::json!({
        "time": "2026-06-09T22:47:40.822668Z",
        "ts": 7
    });
    assert_eq!(
        trace_payload_time_ns(&conflicting),
        Some(1_781_045_260_822_668_000)
    );
    assert_eq!(payload_timestamp_ns(&conflicting).unwrap(), 7);

    let negative_relative = serde_json::json!({"t_abs": -1.0});
    assert_eq!(trace_payload_time_ns(&negative_relative), None);
    assert_eq!(payload_timestamp_ns(&negative_relative).unwrap(), 0);

    assert_eq!(trace_payload_time_ns(&serde_json::json!({})), None);
    assert!(payload_timestamp_ns(&serde_json::json!({})).unwrap() > 0);
}

#[test]
fn canonical_invocation_keeps_case_insensitive_executable_prefix_policy() {
    let executable = "C:/Program Files/Git/cmd/GIT.EXE";
    assert!(!trace_token_is_git_executable(executable));
    assert!(trace_token_is_git_executable_ascii_case_insensitive(
        executable
    ));

    let raw_argv = vec![
        executable.to_string(),
        "status".to_string(),
        "--short".to_string(),
    ];
    assert_eq!(
        canonical_invocation(&raw_argv, None),
        (Some("status".to_string()), vec!["--short".to_string()])
    );
}
