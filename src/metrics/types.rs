//! Core metrics types for event tracking.
//! All types are exported for use by external crates (e.g., ingestion server).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Current API version for metrics wire format.
pub const METRICS_API_VERSION: u8 = 1;

/// Sparse position-encoded array (HashMap with string keys for positions).
/// Missing keys = not-set, explicit null = null, otherwise value.
pub type SparseArray = HashMap<String, Value>;

/// Event IDs for different metric types.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricEventId {
    Committed = 1,
    AgentUsage = 2,
    InstallHooks = 3,
    Checkpoint = 4,
    SessionEvent = 5,
    OtelTrace = 6,
    RewriteCommitted = 7,
    DaemonIngestAnomaly = 8,
    TokenUsage = 9,
}

/// Trait for event-specific values.
pub trait EventValues: Sized {
    fn event_id() -> MetricEventId;
    fn to_sparse(&self) -> SparseArray;
    /// Consuming variant of `to_sparse` that moves data instead of cloning.
    fn into_sparse(self) -> SparseArray {
        self.to_sparse()
    }
    #[allow(dead_code)]
    fn from_sparse(arr: &SparseArray) -> Self;
}

/// Generic wrapper for any metric event.
/// JSON keys: t=timestamp, e=event_id, v=values, a=attrs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricEvent {
    #[serde(rename = "t")]
    pub timestamp: u32,
    #[serde(rename = "e")]
    pub event_id: u16,
    #[serde(rename = "v")]
    pub values: SparseArray,
    #[serde(rename = "a")]
    pub attrs: SparseArray,
}

impl MetricEvent {
    /// Create a new metric event with current timestamp.
    pub fn new<V: EventValues>(values: &V, attrs: SparseArray) -> Self {
        Self {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32,
            event_id: V::event_id() as u16,
            values: values.to_sparse(),
            attrs,
        }
    }

    /// Create a new metric event by consuming the values (avoids cloning).
    pub fn from_values<V: EventValues>(values: V, attrs: SparseArray) -> Self {
        Self {
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32,
            event_id: V::event_id() as u16,
            values: values.into_sparse(),
            attrs,
        }
    }

    /// Create a new metric event by consuming the values, with an optional explicit timestamp.
    /// Falls back to SystemTime::now() when event_ts is None.
    pub fn from_values_with_timestamp<V: EventValues>(
        values: V,
        attrs: SparseArray,
        event_ts: Option<u32>,
    ) -> Self {
        Self {
            timestamp: event_ts.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as u32
            }),
            event_id: V::event_id() as u16,
            values: values.into_sparse(),
            attrs,
        }
    }

    /// Create with explicit timestamp (for deserialization/testing).
    #[allow(dead_code)]
    pub fn with_timestamp<V: EventValues>(timestamp: u32, values: &V, attrs: SparseArray) -> Self {
        Self {
            timestamp,
            event_id: V::event_id() as u16,
            values: values.to_sparse(),
            attrs,
        }
    }
}

/// Metrics batch for wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsBatch {
    #[serde(rename = "v")]
    pub version: u8,
    pub events: Vec<MetricEvent>,
}

impl MetricsBatch {
    pub fn new(events: Vec<MetricEvent>) -> Self {
        Self {
            version: METRICS_API_VERSION,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_batch_serialization() {
        let batch = MetricsBatch::new(vec![]);
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("\"v\":1"));
        assert!(json.contains("\"events\":[]"));
    }

    #[test]
    fn test_metric_event_serialization() {
        let mut values = SparseArray::new();
        values.insert("0".to_string(), Value::String("test".to_string()));

        let mut attrs = SparseArray::new();
        attrs.insert("0".to_string(), Value::String("version".to_string()));

        let event = MetricEvent {
            timestamp: 1704067200,
            event_id: MetricEventId::Committed as u16,
            values,
            attrs,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"t\":1704067200"));
        assert!(json.contains("\"e\":1"));
        assert!(json.contains("\"v\":{"));
        assert!(json.contains("\"a\":{"));
    }

    #[test]
    fn test_metric_event_deserialization() {
        let json = r#"{"t":1704067200,"e":2,"v":{"0":"test"},"a":{"0":"1.0.0"}}"#;
        let event: MetricEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.timestamp, 1704067200);
        assert_eq!(event.event_id, 2);
        assert_eq!(
            event.values.get("0"),
            Some(&Value::String("test".to_string()))
        );
        assert_eq!(
            event.attrs.get("0"),
            Some(&Value::String("1.0.0".to_string()))
        );
    }

    #[test]
    fn test_metric_event_with_timestamp() {
        use crate::metrics::events::CommittedValues;

        let values = CommittedValues::new().human_additions(50);
        let mut attrs = SparseArray::new();
        attrs.insert("0".to_string(), Value::String("1.0.0".to_string()));

        let event = MetricEvent::with_timestamp(1700000000, &values, attrs);

        assert_eq!(event.timestamp, 1700000000);
        assert_eq!(event.event_id, 1);
    }

    #[test]
    fn test_metric_event_id_values() {
        assert_eq!(MetricEventId::Committed as u16, 1);
        assert_eq!(MetricEventId::AgentUsage as u16, 2);
        assert_eq!(MetricEventId::InstallHooks as u16, 3);
        assert_eq!(MetricEventId::Checkpoint as u16, 4);
        assert_eq!(MetricEventId::RewriteCommitted as u16, 7);
        assert_eq!(MetricEventId::TokenUsage as u16, 9);
    }

    #[test]
    fn test_metric_event_id_equality() {
        let id1 = MetricEventId::Committed;
        let id2 = MetricEventId::Committed;
        let id3 = MetricEventId::AgentUsage;

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_metrics_batch_with_events() {
        let mut values = SparseArray::new();
        values.insert("0".to_string(), Value::Number(100.into()));

        let mut attrs = SparseArray::new();
        attrs.insert("0".to_string(), Value::String("2.0.0".to_string()));

        let event1 = MetricEvent {
            timestamp: 1704067200,
            event_id: 1,
            values: values.clone(),
            attrs: attrs.clone(),
        };

        let event2 = MetricEvent {
            timestamp: 1704067300,
            event_id: 2,
            values,
            attrs,
        };

        let batch = MetricsBatch::new(vec![event1, event2]);

        assert_eq!(batch.version, METRICS_API_VERSION);
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].timestamp, 1704067200);
        assert_eq!(batch.events[1].timestamp, 1704067300);
    }

    #[test]
    fn test_metrics_batch_deserialization() {
        let json = r#"{"v":1,"events":[{"t":1704067200,"e":1,"v":{},"a":{}}]}"#;
        let batch: MetricsBatch = serde_json::from_str(json).unwrap();

        assert_eq!(batch.version, 1);
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].timestamp, 1704067200);
    }

    #[test]
    fn test_metrics_api_version() {
        assert_eq!(METRICS_API_VERSION, 1);
    }

    #[test]
    fn test_metric_event_new_creates_current_timestamp() {
        use crate::metrics::events::AgentUsageValues;
        use std::time::{SystemTime, UNIX_EPOCH};

        let values = AgentUsageValues::new();
        let attrs = SparseArray::new();

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;

        let event = MetricEvent::new(&values, attrs);

        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;

        // Timestamp should be between before and after (within a few seconds)
        assert!(event.timestamp >= before);
        assert!(event.timestamp <= after + 1);
    }

    #[test]
    fn test_sparse_array_type() {
        let mut arr: SparseArray = HashMap::new();
        arr.insert("0".to_string(), Value::String("test".to_string()));
        arr.insert("1".to_string(), Value::Number(42.into()));
        arr.insert("2".to_string(), Value::Null);

        assert_eq!(arr.len(), 3);
        assert_eq!(arr.get("0"), Some(&Value::String("test".to_string())));
        assert_eq!(arr.get("1"), Some(&Value::Number(42.into())));
        assert_eq!(arr.get("2"), Some(&Value::Null));
    }

    #[test]
    fn test_metric_event_id_debug() {
        let id = MetricEventId::Committed;
        let debug_str = format!("{:?}", id);
        assert_eq!(debug_str, "Committed");
    }

    #[test]
    fn test_metric_event_id_clone() {
        let id1 = MetricEventId::Checkpoint;
        let id2 = id1;
        assert_eq!(id1, id2);
    }
}
