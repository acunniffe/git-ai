use super::raw_json_event::raw_json_event;

raw_json_event! {
    name: SessionEventValues,
    pos_mod: session_event_pos,
    event_variant: SessionEvent,
    event_num: 5,
    event_name: "session_event",
    description: "Each event is the raw JSON from the agent's transcript file, stored at position 0.",
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::metrics::pos_encoded::PosEncoded;
    use crate::model::metrics::types::{EventValues, MetricEventId};

    #[test]
    fn test_session_event_values_new() {
        let raw = serde_json::json!({"type": "user", "uuid": "abc"});
        let values = SessionEventValues::new(raw.clone());
        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, None);
        assert_eq!(values.external_parent_event_id, None);
        assert_eq!(values.external_tool_use_id, None);
    }

    #[test]
    fn test_session_event_values_with_ids() {
        let raw = serde_json::json!({"type": "assistant"});
        let values = SessionEventValues::with_ids(
            raw.clone(),
            Some("evt-123".to_string()),
            Some("parent-456".to_string()),
            Some("toolu_789".to_string()),
        );

        assert_eq!(values.raw_json, raw);
        assert_eq!(values.external_event_id, Some("evt-123".to_string()));
        assert_eq!(
            values.external_parent_event_id,
            Some("parent-456".to_string())
        );
        assert_eq!(values.external_tool_use_id, Some("toolu_789".to_string()));
    }

    #[test]
    fn test_session_event_values_sparse_roundtrip_with_ids() {
        let raw = serde_json::json!({"type": "assistant", "data": 42});
        let values = SessionEventValues::with_ids(
            raw.clone(),
            Some("event-id".to_string()),
            Some("parent-id".to_string()),
            Some("tool-use-id".to_string()),
        );

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(
            sparse.get("1"),
            Some(&serde_json::Value::String("event-id".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&serde_json::Value::String("parent-id".to_string()))
        );
        assert_eq!(
            sparse.get("3"),
            Some(&serde_json::Value::String("tool-use-id".to_string()))
        );

        let restored = <SessionEventValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.raw_json, raw);
        assert_eq!(restored.external_event_id, Some("event-id".to_string()));
        assert_eq!(
            restored.external_parent_event_id,
            Some("parent-id".to_string())
        );
        assert_eq!(
            restored.external_tool_use_id,
            Some("tool-use-id".to_string())
        );
    }

    #[test]
    fn test_session_event_values_event_id() {
        assert_eq!(SessionEventValues::event_id(), MetricEventId::SessionEvent);
        assert_eq!(SessionEventValues::event_id() as u16, 5);
    }

    #[test]
    fn test_session_event_values_sparse_none_ids_omitted() {
        let raw = serde_json::json!({"type": "user"});
        let values = SessionEventValues::new(raw.clone());

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(sparse.get("0"), Some(&raw));
        assert_eq!(sparse.get("1"), None);
        assert_eq!(sparse.get("2"), None);
        assert_eq!(sparse.get("3"), None);
    }

    #[test]
    fn test_session_event_values_into_sparse_with_ids() {
        let raw = serde_json::json!({"msg": "hello"});
        let values = SessionEventValues::with_ids(
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
}
