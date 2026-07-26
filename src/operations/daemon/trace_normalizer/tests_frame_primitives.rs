use crate::operations::daemon::trace_helpers::trace_payload_time_ns;

use super::frame_helpers::payload_timestamp_ns;

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
