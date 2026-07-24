use super::pos_event::pos_event;

/// Value positions for "checkpoint" event.
/// One event per file in the checkpoint.
pub mod checkpoint_pos {
    pub const CHECKPOINT_TS: usize = 0; // u64 - checkpoint timestamp
    pub const KIND: usize = 1; // String ("human", "ai_agent", "ai_tab")
    pub const FILE_PATH: usize = 2; // String - full relative file path
    pub const LINES_ADDED: usize = 3; // u32 - for this file
    pub const LINES_DELETED: usize = 4; // u32 - for this file
    pub const LINES_ADDED_SLOC: usize = 5; // u32 - for this file
    pub const LINES_DELETED_SLOC: usize = 6; // u32 - for this file
    pub const TOOL_USE_ID: usize = 7; // String - nullable
    pub const EDIT_KIND: usize = 8; // String - nullable ("file_edit" | "bash")
    pub const CHECKPOINT_TYPE: usize = 9; // String - nullable ("recovered_bash", etc.)
    pub const ATTRIBUTION_RECOVERY_METADATA: usize = 10; // String - nullable JSON
}

pos_event! {
    /// Values for Event ID 4: checkpoint
    ///
    /// Recorded for each file in a checkpoint.
    /// Uses EventAttributes for standard metadata (repo_url, author, tool, model, etc.)
    ///
    /// **Fields:**
    /// | Position | Name | Type |
    /// |----------|------|------|
    /// | 0 | checkpoint_ts | u64 |
    /// | 1 | kind | String |
    /// | 2 | file_path | String |
    /// | 3 | lines_added | u32 |
    /// | 4 | lines_deleted | u32 |
    /// | 5 | lines_added_sloc | u32 |
    /// | 6 | lines_deleted_sloc | u32 |
    /// | 7 | external_tool_use_id | String (nullable) |
    /// | 8 | edit_kind | String (nullable) |
    /// | 9 | checkpoint_type | String (nullable) |
    /// | 10 | attribution_recovery_metadata | String (nullable JSON) |
    struct CheckpointValues uses checkpoint_pos for Checkpoint {
        checkpoint_ts: u64 @ CHECKPOINT_TS,
        kind: String @ KIND,
        file_path: String @ FILE_PATH,
        lines_added: u32 @ LINES_ADDED,
        lines_deleted: u32 @ LINES_DELETED,
        lines_added_sloc: u32 @ LINES_ADDED_SLOC,
        lines_deleted_sloc: u32 @ LINES_DELETED_SLOC,
        external_tool_use_id: String @ TOOL_USE_ID,
        edit_kind: String @ EDIT_KIND,
        checkpoint_type: String @ CHECKPOINT_TYPE,
        attribution_recovery_metadata: String @ ATTRIBUTION_RECOVERY_METADATA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::metrics::pos_encoded::PosEncoded;
    use crate::model::metrics::types::{EventValues, MetricEventId, SparseArray};
    use serde_json::Value;
    use std::collections::BTreeMap;

    #[test]
    fn test_checkpoint_values_builder() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .lines_added(50)
            .lines_deleted(10)
            .lines_added_sloc(45)
            .lines_deleted_sloc(8);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(values.file_path, Some(Some("src/main.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(50)));
        assert_eq!(values.lines_deleted, Some(Some(10)));
        assert_eq!(values.lines_added_sloc, Some(Some(45)));
        assert_eq!(values.lines_deleted_sloc, Some(Some(8)));
    }

    #[test]
    fn test_checkpoint_values_with_nulls() {
        let values = CheckpointValues::new()
            .checkpoint_ts_null()
            .kind_null()
            .file_path_null()
            .lines_added_null();

        assert_eq!(values.checkpoint_ts, Some(None));
        assert_eq!(values.kind, Some(None));
        assert_eq!(values.file_path, Some(None));
        assert_eq!(values.lines_added, Some(None));
    }

    #[test]
    fn test_checkpoint_values_to_sparse() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("human")
            .file_path("tests/test.rs")
            .lines_added(100)
            .lines_deleted(20);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(sparse.get("1"), Some(&Value::String("human".to_string())));
        assert_eq!(
            sparse.get("2"),
            Some(&Value::String("tests/test.rs".to_string()))
        );
        assert_eq!(sparse.get("3"), Some(&Value::Number(100.into())));
        assert_eq!(sparse.get("4"), Some(&Value::Number(20.into())));
    }

    #[test]
    fn test_checkpoint_values_from_sparse() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_tab".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("3".to_string(), Value::Number(75.into()));
        sparse.insert("4".to_string(), Value::Number(15.into()));
        sparse.insert("5".to_string(), Value::Number(70.into()));
        sparse.insert("6".to_string(), Value::Number(12.into()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_tab".to_string())));
        assert_eq!(values.file_path, Some(Some("lib.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(75)));
        assert_eq!(values.lines_deleted, Some(Some(15)));
        assert_eq!(values.lines_added_sloc, Some(Some(70)));
        assert_eq!(values.lines_deleted_sloc, Some(Some(12)));
    }

    #[test]
    fn test_checkpoint_event_id() {
        assert_eq!(CheckpointValues::event_id(), MetricEventId::Checkpoint);
        assert_eq!(CheckpointValues::event_id() as u16, 4);
    }

    #[test]
    fn test_checkpoint_values_with_external_tool_use_id() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .lines_added(50)
            .external_tool_use_id("tool-use-123");

        assert_eq!(
            values.external_tool_use_id,
            Some(Some("tool-use-123".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_external_tool_use_id_null() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("human")
            .external_tool_use_id_null();

        assert_eq!(values.external_tool_use_id, Some(None));
    }

    #[test]
    fn test_checkpoint_values_to_sparse_with_external_tool_use_id() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("tests/test.rs")
            .lines_added(100)
            .external_tool_use_id("tool-xyz");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(
            sparse.get("1"),
            Some(&Value::String("ai_agent".to_string()))
        );
        assert_eq!(
            sparse.get("2"),
            Some(&Value::String("tests/test.rs".to_string()))
        );
        assert_eq!(sparse.get("3"), Some(&Value::Number(100.into())));
        assert_eq!(
            sparse.get("7"),
            Some(&Value::String("tool-xyz".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_from_sparse_with_external_tool_use_id() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_tab".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("3".to_string(), Value::Number(75.into()));
        sparse.insert("7".to_string(), Value::String("tool-abc".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_tab".to_string())));
        assert_eq!(values.file_path, Some(Some("lib.rs".to_string())));
        assert_eq!(values.lines_added, Some(Some(75)));
        assert_eq!(
            values.external_tool_use_id,
            Some(Some("tool-abc".to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_roundtrip_with_external_tool_use_id() {
        let original = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("src/lib.rs")
            .lines_added(50)
            .external_tool_use_id_null();

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(restored.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(restored.file_path, Some(Some("src/lib.rs".to_string())));
        assert_eq!(restored.lines_added, Some(Some(50)));
        assert_eq!(restored.external_tool_use_id, Some(None)); // explicitly null
    }

    #[test]
    fn test_checkpoint_values_external_tool_use_id_not_set() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1700000000.into()));
        sparse.insert("1".to_string(), Value::String("human".to_string()));
        // external_tool_use_id not included

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.external_tool_use_id, None); // not set
    }

    #[test]
    fn test_checkpoint_values_with_edit_kind() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .file_path("src/main.rs")
            .edit_kind("file_edit");

        assert_eq!(values.edit_kind, Some(Some("file_edit".to_string())));
    }

    #[test]
    fn test_checkpoint_values_edit_kind_null() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1704067200)
            .kind("ai_agent")
            .edit_kind_null();

        assert_eq!(values.edit_kind, Some(None));
    }

    #[test]
    fn test_checkpoint_values_with_recovery_metadata() {
        let values = CheckpointValues::new()
            .checkpoint_type("recovered_bash")
            .attribution_recovery_metadata(r#"{"solver":"bash_mtime"}"#);

        let sparse = PosEncoded::to_sparse(&values);
        assert_eq!(
            sparse.get("9"),
            Some(&Value::String("recovered_bash".to_string()))
        );
        assert_eq!(
            sparse.get("10"),
            Some(&Value::String(r#"{"solver":"bash_mtime"}"#.to_string()))
        );

        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(
            restored.checkpoint_type,
            Some(Some("recovered_bash".to_string()))
        );
        assert_eq!(
            restored.attribution_recovery_metadata,
            Some(Some(r#"{"solver":"bash_mtime"}"#.to_string()))
        );
    }

    #[test]
    fn test_checkpoint_values_to_sparse_with_edit_kind() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("tests/test.rs")
            .edit_kind("bash");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(1700000000.into())));
        assert_eq!(
            sparse.get("1"),
            Some(&Value::String("ai_agent".to_string()))
        );
        assert_eq!(sparse.get("8"), Some(&Value::String("bash".to_string())));
    }

    #[test]
    fn test_checkpoint_values_from_sparse_with_edit_kind() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1704067200.into()));
        sparse.insert("1".to_string(), Value::String("ai_agent".to_string()));
        sparse.insert("2".to_string(), Value::String("lib.rs".to_string()));
        sparse.insert("8".to_string(), Value::String("file_edit".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(values.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(values.edit_kind, Some(Some("file_edit".to_string())));
    }

    #[test]
    fn test_checkpoint_values_roundtrip_with_edit_kind() {
        let original = CheckpointValues::new()
            .checkpoint_ts(1700000000)
            .kind("ai_agent")
            .file_path("src/lib.rs")
            .lines_added(50)
            .edit_kind("bash");

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(restored.kind, Some(Some("ai_agent".to_string())));
        assert_eq!(restored.file_path, Some(Some("src/lib.rs".to_string())));
        assert_eq!(restored.lines_added, Some(Some(50)));
        assert_eq!(restored.edit_kind, Some(Some("bash".to_string())));
    }

    #[test]
    fn test_checkpoint_values_edit_kind_not_set() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(1700000000.into()));
        sparse.insert("1".to_string(), Value::String("human".to_string()));

        let values = <CheckpointValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.edit_kind, None);
    }

    // --- Golden serialization tests -----------------------------------
    //
    // These pin the exact wire representation of `CheckpointValues`. They
    // are intentionally independent of how the struct/impls are built
    // (hand-written vs. macro-generated): any future schema drift --
    // renamed field, moved position, changed null/absent handling --
    // shows up here as a loud, explicit diff.

    /// Renders a `SparseArray` as a byte-exact JSON string with position
    /// keys in numeric (not lexical) order, for byte-level pinning.
    fn pinned_json(sparse: &SparseArray) -> String {
        let ordered: BTreeMap<usize, Value> = sparse
            .iter()
            .map(|(k, v)| (k.parse::<usize>().unwrap(), v.clone()))
            .collect();
        serde_json::to_string(&ordered).unwrap()
    }

    #[test]
    fn test_checkpoint_values_golden_fully_populated() {
        let values = CheckpointValues::new()
            .checkpoint_ts(1_700_000_000)
            .kind("ai_agent")
            .file_path("src/lib.rs")
            .lines_added(10)
            .lines_deleted(2)
            .lines_added_sloc(8)
            .lines_deleted_sloc(1)
            .external_tool_use_id("tool-99")
            .edit_kind("file_edit")
            .checkpoint_type("recovered_bash")
            .attribution_recovery_metadata(r#"{"solver":"x"}"#);

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": 1_700_000_000u64,
            "1": "ai_agent",
            "2": "src/lib.rs",
            "3": 10,
            "4": 2,
            "5": 8,
            "6": 1,
            "7": "tool-99",
            "8": "file_edit",
            "9": "recovered_bash",
            "10": r#"{"solver":"x"}"#,
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 11, "exactly the 11 live fields, no more");

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":1700000000,"1":"ai_agent","2":"src/lib.rs","3":10,"4":2,"5":8,"6":1,"7":"tool-99","8":"file_edit","9":"recovered_bash","10":"{\"solver\":\"x\"}"}"#
        );

        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }

    #[test]
    fn test_checkpoint_values_golden_partially_populated() {
        // Exercises all three `PosField` states (value / explicit null /
        // absent) across every field type in the struct.
        let values = CheckpointValues::new()
            .checkpoint_ts_null() // explicit null (u64)
            .kind("human") // value (String)
            // file_path: absent (String)
            .lines_added(3) // value (u32)
            // lines_deleted: absent (u32)
            .lines_added_sloc_null() // explicit null (u32)
            // lines_deleted_sloc: absent (u32)
            .external_tool_use_id_null() // explicit null (String)
            .edit_kind("bash") // value (String)
            // checkpoint_type: absent (String)
            .attribution_recovery_metadata_null(); // explicit null (String)

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": null,
            "1": "human",
            "3": 3,
            "5": null,
            "7": null,
            "8": "bash",
            "10": null,
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 7);
        for absent in ["2", "4", "6", "9"] {
            assert!(
                !sparse.contains_key(absent),
                "position {absent} must be omitted, not null"
            );
        }

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":null,"1":"human","3":3,"5":null,"7":null,"8":"bash","10":null}"#
        );

        let restored = <CheckpointValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.checkpoint_ts, Some(None));
        assert_eq!(restored.kind, Some(Some("human".to_string())));
        assert_eq!(restored.file_path, None);
        assert_eq!(restored.lines_added, Some(Some(3)));
        assert_eq!(restored.lines_deleted, None);
        assert_eq!(restored.lines_added_sloc, Some(None));
        assert_eq!(restored.lines_deleted_sloc, None);
        assert_eq!(restored.external_tool_use_id, Some(None));
        assert_eq!(restored.edit_kind, Some(Some("bash".to_string())));
        assert_eq!(restored.checkpoint_type, None);
        assert_eq!(restored.attribution_recovery_metadata, Some(None));

        // Round-trip: re-serializing the restored value reproduces the
        // exact same sparse map, including the omitted/absent positions.
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }
}
