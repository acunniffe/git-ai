use super::raw_json_event::raw_json_event;

raw_json_event! {
    name: OtelTraceValues,
    pos_mod: otel_trace_pos,
    event_variant: OtelTrace,
    event_num: 6,
    event_name: "otel_trace",
    description: "Each event is an OTEL span from a Copilot traces SQLite DB, stored as JSON at position 0.",
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::metrics::pos_encoded::PosEncoded;
    use crate::model::metrics::types::{EventValues, MetricEventId};

    #[test]
    fn test_otel_trace_values_new() {
        let raw = serde_json::json!({"span": {"span_id": "abc", "trace_id": "t1"}});
        let values = OtelTraceValues::new(raw.clone());
        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, None);
        assert_eq!(values.external_parent_event_id, None);
        assert_eq!(values.external_tool_use_id, None);
    }

    #[test]
    fn test_otel_trace_values_with_ids() {
        let raw = serde_json::json!({"span": {"span_id": "s1", "trace_id": "t1"}});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("span-123".to_string()),
            Some("parent-456".to_string()),
            Some("call-789".to_string()),
        );

        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, Some("span-123".to_string()));
        assert_eq!(
            values.external_parent_event_id,
            Some("parent-456".to_string())
        );
        assert_eq!(values.external_tool_use_id, Some("call-789".to_string()));
    }

    #[test]
    fn test_otel_trace_values_sparse_roundtrip_with_ids() {
        let raw = serde_json::json!({"span": {"span_id": "s1", "trace_id": "t1"}, "attributes": {"key": "val"}});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("span-id".to_string()),
            Some("parent-id".to_string()),
            Some("tool-call-id".to_string()),
        );

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("span-id".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&serde_json::Value::String("parent-id".to_string()))
        );
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tool-call-id".to_string()))
        );

        let restored = <OtelTraceValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.raw_json, raw);
        assert_eq!(restored.external_event_id, Some("span-id".to_string()));
        assert_eq!(
            restored.external_parent_event_id,
            Some("parent-id".to_string())
        );
        assert_eq!(
            restored.external_tool_use_id,
            Some("tool-call-id".to_string())
        );
    }

    #[test]
    fn test_otel_trace_values_sparse_none_ids_omitted() {
        let raw = serde_json::json!({"span": {"span_id": "s1"}});
        let values = OtelTraceValues::new(raw.clone());

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(sparse.get("1"), None);
        assert_eq!(sparse.get("2"), None);
        assert_eq!(sparse.get("3"), None);
    }

    #[test]
    fn test_otel_trace_values_into_sparse_with_ids() {
        let raw = serde_json::json!({"data": "test"});
        let values = OtelTraceValues::with_ids(
            raw.clone(),
            Some("eid".to_string()),
            None,
            Some("tid".to_string()),
        );

        let sparse = EventValues::into_sparse(values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("eid".to_string()))
        );
        assert_eq!(sparse.get("2"), None);
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tid".to_string()))
        );
    }

    #[test]
    fn test_otel_trace_values_event_id() {
        assert_eq!(OtelTraceValues::event_id(), MetricEventId::OtelTrace);
        assert_eq!(OtelTraceValues::event_id() as u16, 6);
    }
}
