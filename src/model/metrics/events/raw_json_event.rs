//! Macro for metrics events whose entire payload is a single raw JSON blob
//! plus three optional external-correlation ids.
//!
//! Backs `SessionEventValues` (Event ID 5, an agent transcript entry) and
//! `OtelTraceValues` (Event ID 6, an OTEL span) -- both capture an external
//! record verbatim at position 0, tagged with the same three nullable ids
//! (positions 1-3) for cross-referencing. The two types are otherwise
//! byte-identical; this macro generates the boilerplate around that shared
//! schema, not a new schema. Positions, field names, and wire format must
//! stay exactly as each invocation defines them.

/// Defines a raw-JSON-payload metrics event type: the position-constants
/// module, the values struct, its constructors, and the `PosEncoded` /
/// `EventValues` impls.
///
/// Parameters:
/// - `name`: generated struct name (e.g. `SessionEventValues`)
/// - `pos_mod`: generated position-constants module name
/// - `event_variant`: the `MetricEventId` variant for this event
/// - `event_num`: that variant's numeric id, for the doc comment only
/// - `event_name`: the event's snake_case name, for doc comments
/// - `description`: one doc line describing what the raw JSON contains
macro_rules! raw_json_event {
    (
        name: $name:ident,
        pos_mod: $pos_mod:ident,
        event_variant: $event_variant:ident,
        event_num: $event_num:literal,
        event_name: $event_name:literal,
        description: $description:literal $(,)?
    ) => {
        #[doc = concat!("Value positions for \"", $event_name, "\" event.")]
        pub mod $pos_mod {
            pub const RAW_JSON: usize = 0;
            pub const EXTERNAL_EVENT_ID: usize = 1;
            pub const EXTERNAL_PARENT_EVENT_ID: usize = 2;
            pub const EXTERNAL_TOOL_USE_ID: usize = 3;
        }

        #[doc = concat!("Values for Event ID ", $event_num, ": ", $event_name)]
        #[doc = ""]
        #[doc = $description]
        #[doc = "Uses EventAttributes for session_id, trace_id, tool metadata."]
        #[derive(Debug, Clone, Default)]
        pub struct $name {
            pub raw_json: serde_json::Value,
            pub external_event_id: Option<String>,
            pub external_parent_event_id: Option<String>,
            pub external_tool_use_id: Option<String>,
        }

        impl $name {
            pub fn new(raw_json: serde_json::Value) -> Self {
                Self {
                    raw_json,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                }
            }

            pub fn with_ids(
                raw_json: serde_json::Value,
                external_event_id: Option<String>,
                external_parent_event_id: Option<String>,
                external_tool_use_id: Option<String>,
            ) -> Self {
                Self {
                    raw_json,
                    external_event_id,
                    external_parent_event_id,
                    external_tool_use_id,
                }
            }
        }

        impl $crate::model::metrics::pos_encoded::PosEncoded for $name {
            fn to_sparse(&self) -> $crate::model::metrics::types::SparseArray {
                let mut map = $crate::model::metrics::types::SparseArray::new();
                map.insert($pos_mod::RAW_JSON.to_string(), self.raw_json.clone());
                if let Some(ref id) = self.external_event_id {
                    map.insert(
                        $pos_mod::EXTERNAL_EVENT_ID.to_string(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                if let Some(ref id) = self.external_parent_event_id {
                    map.insert(
                        $pos_mod::EXTERNAL_PARENT_EVENT_ID.to_string(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                if let Some(ref id) = self.external_tool_use_id {
                    map.insert(
                        $pos_mod::EXTERNAL_TOOL_USE_ID.to_string(),
                        serde_json::Value::String(id.clone()),
                    );
                }
                map
            }

            fn from_sparse(arr: &$crate::model::metrics::types::SparseArray) -> Self {
                let raw_json = arr
                    .get(&$pos_mod::RAW_JSON.to_string())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let external_event_id = arr
                    .get(&$pos_mod::EXTERNAL_EVENT_ID.to_string())
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let external_parent_event_id = arr
                    .get(&$pos_mod::EXTERNAL_PARENT_EVENT_ID.to_string())
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let external_tool_use_id = arr
                    .get(&$pos_mod::EXTERNAL_TOOL_USE_ID.to_string())
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Self {
                    raw_json,
                    external_event_id,
                    external_parent_event_id,
                    external_tool_use_id,
                }
            }
        }

        impl $crate::model::metrics::types::EventValues for $name {
            fn event_id() -> $crate::model::metrics::types::MetricEventId {
                $crate::model::metrics::types::MetricEventId::$event_variant
            }

            fn to_sparse(&self) -> $crate::model::metrics::types::SparseArray {
                $crate::model::metrics::pos_encoded::PosEncoded::to_sparse(self)
            }

            fn into_sparse(self) -> $crate::model::metrics::types::SparseArray {
                let mut map = $crate::model::metrics::types::SparseArray::new();
                map.insert($pos_mod::RAW_JSON.to_string(), self.raw_json);
                if let Some(id) = self.external_event_id {
                    map.insert(
                        $pos_mod::EXTERNAL_EVENT_ID.to_string(),
                        serde_json::Value::String(id),
                    );
                }
                if let Some(id) = self.external_parent_event_id {
                    map.insert(
                        $pos_mod::EXTERNAL_PARENT_EVENT_ID.to_string(),
                        serde_json::Value::String(id),
                    );
                }
                if let Some(id) = self.external_tool_use_id {
                    map.insert(
                        $pos_mod::EXTERNAL_TOOL_USE_ID.to_string(),
                        serde_json::Value::String(id),
                    );
                }
                map
            }

            fn from_sparse(arr: &$crate::model::metrics::types::SparseArray) -> Self {
                $crate::model::metrics::pos_encoded::PosEncoded::from_sparse(arr)
            }
        }
    };
}

pub(crate) use raw_json_event;
