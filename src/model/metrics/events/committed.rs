use super::pos_event::pos_event;

/// Value positions for "committed" event.
pub mod committed_pos {
    // Scalar fields
    pub const HUMAN_ADDITIONS: usize = 0;
    pub const GIT_DIFF_DELETED_LINES: usize = 1;
    pub const GIT_DIFF_ADDED_LINES: usize = 2;

    // Array fields (parallel arrays, index 0 = "all" aggregate, index 1+ = per tool/model)
    pub const TOOL_MODEL_PAIRS: usize = 3;
    pub const MIXED_ADDITIONS: usize = 4;
    pub const AI_ADDITIONS: usize = 5;
    pub const AI_ACCEPTED: usize = 6;
    pub const TOTAL_AI_ADDITIONS: usize = 7;
    pub const TOTAL_AI_DELETIONS: usize = 8;
    // Position 9 was time_waiting_for_ai (removed)

    // New scalar fields
    pub const FIRST_CHECKPOINT_TS: usize = 10; // u64 (null if no checkpoints)
    pub const COMMIT_SUBJECT: usize = 11; // String
    pub const COMMIT_BODY: usize = 12; // String (null if empty)
    pub const AUTHORSHIP_NOTE: usize = 13; // String (full serialized authorship note)
    pub const HUNKS: usize = 14; // String (JSON array of DiffJsonHunk)
    pub const AUTHOR_TS: usize = 15; // u64 (git author timestamp, %at)
    pub const COMMIT_TS: usize = 16; // u64 (git committer timestamp, %ct)
    pub const PATCH_ID: usize = 17; // String (git patch-id --stable)
}

pos_event! {
    /// Values for Event ID 1: committed
    ///
    /// Recorded when AI-assisted code is committed.
    ///
    /// **Scalar fields:**
    /// | Position | Name | Type |
    /// |----------|------|------|
    /// | 0 | human_additions | u32 |
    /// | 1 | git_diff_deleted_lines | u32 |
    /// | 2 | git_diff_added_lines | u32 |
    ///
    /// **Array fields (parallel arrays, index 0 = "all" for aggregate, index 1+ = per tool/model):**
    /// | Position | Name | Type |
    /// |----------|------|------|
    /// | 3 | tool_model_pairs | `Vec<String>` |
    /// | 4 | (removed) | - |
    /// | 5 | ai_additions | `Vec<u32>` |
    /// | 6 | ai_accepted | `Vec<u32>` |
    /// | 7 | (removed) | - |
    /// | 8 | (removed) | - |
    /// | 9 | (removed) | - |
    /// | 10 | first_checkpoint_ts | u64 |
    /// | 11 | commit_subject | String |
    /// | 12 | commit_body | String |
    /// | 13 | authorship_note | String |
    /// | 14 | hunks | String |
    /// | 15 | author_ts | u64 |
    /// | 16 | commit_ts | u64 |
    /// | 17 | patch_id | String |
    struct CommittedValues uses committed_pos for Committed {
        // Scalar fields
        human_additions: u32 @ HUMAN_ADDITIONS,
        git_diff_deleted_lines: u32 @ GIT_DIFF_DELETED_LINES,
        git_diff_added_lines: u32 @ GIT_DIFF_ADDED_LINES,

        // Array fields (parallel arrays)
        tool_model_pairs: [String] @ TOOL_MODEL_PAIRS,
        ai_additions: [u32] @ AI_ADDITIONS,
        ai_accepted: [u32] @ AI_ACCEPTED,

        // New scalar fields
        first_checkpoint_ts: u64 @ FIRST_CHECKPOINT_TS,
        commit_subject: String @ COMMIT_SUBJECT,
        commit_body: String @ COMMIT_BODY,
        authorship_note: String @ AUTHORSHIP_NOTE,
        hunks: String @ HUNKS,
        author_ts: u64 @ AUTHOR_TS,
        commit_ts: u64 @ COMMIT_TS,
        patch_id: String @ PATCH_ID,
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
    fn test_committed_values_builder() {
        let values = CommittedValues::new()
            .human_additions(50)
            .git_diff_deleted_lines(20)
            .git_diff_added_lines(150)
            .tool_model_pairs(vec!["all".to_string(), "claude-code:claude-3".to_string()])
            .ai_additions(vec![100, 70])
            .ai_accepted(vec![80, 55]);

        assert_eq!(values.human_additions, Some(Some(50)));
        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec![
                "all".to_string(),
                "claude-code:claude-3".to_string()
            ]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![100, 70])));
    }

    #[test]
    fn test_committed_values_to_sparse() {
        let values = CommittedValues::new()
            .human_additions(50)
            .git_diff_deleted_lines(20)
            .git_diff_added_lines(150)
            .tool_model_pairs(vec!["all".to_string(), "cursor:gpt-4".to_string()])
            .ai_additions(vec![100, 30]);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(sparse.get("0"), Some(&Value::Number(50.into())));
        assert_eq!(sparse.get("1"), Some(&Value::Number(20.into())));
        assert_eq!(sparse.get("2"), Some(&Value::Number(150.into())));
        assert_eq!(
            sparse.get("3"),
            Some(&Value::Array(vec![
                Value::String("all".to_string()),
                Value::String("cursor:gpt-4".to_string())
            ]))
        );
        assert_eq!(
            sparse.get("5"),
            Some(&Value::Array(vec![
                Value::Number(100.into()),
                Value::Number(30.into())
            ]))
        );
    }

    #[test]
    fn test_committed_values_with_commit_timestamps_and_patch_id() {
        let values = CommittedValues::new()
            .author_ts(1_704_067_200)
            .commit_ts(1_704_067_260)
            .patch_id("abc123");

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(
            sparse.get("15"),
            Some(&Value::Number(1_704_067_200u64.into()))
        );
        assert_eq!(
            sparse.get("16"),
            Some(&Value::Number(1_704_067_260u64.into()))
        );
        assert_eq!(sparse.get("17"), Some(&Value::String("abc123".to_string())));
    }

    #[test]
    fn test_committed_values_from_sparse() {
        let mut sparse = SparseArray::new();
        sparse.insert("0".to_string(), Value::Number(75.into()));
        sparse.insert(
            "3".to_string(),
            Value::Array(vec![
                Value::String("all".to_string()),
                Value::String("copilot:gpt-4".to_string()),
            ]),
        );
        sparse.insert(
            "5".to_string(),
            Value::Array(vec![Value::Number(200.into()), Value::Number(100.into())]),
        );

        let values = <CommittedValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(values.human_additions, Some(Some(75)));
        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "copilot:gpt-4".to_string()]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![200, 100])));
        assert_eq!(values.git_diff_deleted_lines, None); // not set
    }

    #[test]
    fn test_committed_values_event_id() {
        assert_eq!(CommittedValues::event_id(), MetricEventId::Committed);
        assert_eq!(CommittedValues::event_id() as u16, 1);
    }

    #[test]
    fn test_committed_values_null_fields() {
        let values = CommittedValues::new()
            .human_additions_null()
            .git_diff_deleted_lines_null()
            .tool_model_pairs_null();

        assert_eq!(values.human_additions, Some(None));
        assert_eq!(values.git_diff_deleted_lines, Some(None));
        assert_eq!(values.tool_model_pairs, Some(None));
    }

    #[test]
    fn test_committed_values_with_commit_info() {
        let values = CommittedValues::new()
            .human_additions(10)
            .first_checkpoint_ts(1704067200)
            .commit_subject("Initial commit")
            .commit_body("This is the commit body\n\nWith multiple lines");

        assert_eq!(values.first_checkpoint_ts, Some(Some(1704067200)));
        assert_eq!(
            values.commit_subject,
            Some(Some("Initial commit".to_string()))
        );
        assert_eq!(
            values.commit_body,
            Some(Some(
                "This is the commit body\n\nWith multiple lines".to_string()
            ))
        );
    }

    #[test]
    fn test_committed_values_roundtrip_with_new_fields() {
        let original = CommittedValues::new()
            .human_additions(25)
            .first_checkpoint_ts(1700000000)
            .commit_subject("Test commit")
            .commit_body_null()
            .author_ts(1700000100)
            .commit_ts(1700000200)
            .patch_id("stable-patch-id");

        let sparse = PosEncoded::to_sparse(&original);
        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);

        assert_eq!(restored.human_additions, Some(Some(25)));
        assert_eq!(restored.first_checkpoint_ts, Some(Some(1700000000)));
        assert_eq!(
            restored.commit_subject,
            Some(Some("Test commit".to_string()))
        );
        assert_eq!(restored.commit_body, Some(None));
        assert_eq!(restored.author_ts, Some(Some(1700000100)));
        assert_eq!(restored.commit_ts, Some(Some(1700000200)));
        assert_eq!(restored.patch_id, Some(Some("stable-patch-id".to_string())));
    }

    #[test]
    fn test_committed_values_with_hunks() {
        let hunks_json = r#"[{"commit_sha":"abc123","content_hash":"def456","hunk_kind":"addition","start_line":1,"end_line":5,"file_path":"src/main.rs"}]"#;
        let values = CommittedValues::new().human_additions(10).hunks(hunks_json);

        assert_eq!(values.hunks, Some(Some(hunks_json.to_string())));
    }

    #[test]
    fn test_committed_values_hunks_null() {
        let values = CommittedValues::new().hunks_null();
        assert_eq!(values.hunks, Some(None));
    }

    #[test]
    fn test_committed_values_hunks_roundtrip() {
        let hunks_json = r#"[{"commit_sha":"abc","content_hash":"def","hunk_kind":"addition","start_line":1,"end_line":3,"file_path":"test.rs"}]"#;
        let original = CommittedValues::new().human_additions(5).hunks(hunks_json);

        let sparse = PosEncoded::to_sparse(&original);
        assert_eq!(
            sparse.get("14"),
            Some(&Value::String(hunks_json.to_string()))
        );

        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.hunks, Some(Some(hunks_json.to_string())));
    }

    #[test]
    fn test_committed_values_with_all_arrays() {
        let values = CommittedValues::new()
            .tool_model_pairs(vec!["all".to_string(), "cursor:gpt-4".to_string()])
            .ai_additions(vec![100, 50])
            .ai_accepted(vec![80, 40]);

        assert_eq!(
            values.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "cursor:gpt-4".to_string()]))
        );
        assert_eq!(values.ai_additions, Some(Some(vec![100, 50])));
        assert_eq!(values.ai_accepted, Some(Some(vec![80, 40])));
    }

    #[test]
    fn test_committed_values_array_nulls() {
        let values = CommittedValues::new().ai_accepted_null();

        assert_eq!(values.ai_accepted, Some(None));
    }

    // --- Golden serialization tests -----------------------------------
    //
    // These pin the exact wire representation of `CommittedValues`. They
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
    fn test_committed_values_golden_fully_populated() {
        let values = CommittedValues::new()
            .human_additions(11)
            .git_diff_deleted_lines(22)
            .git_diff_added_lines(33)
            .tool_model_pairs(vec!["all".to_string(), "claude-code:opus".to_string()])
            .ai_additions(vec![40, 41])
            .ai_accepted(vec![42, 43])
            .first_checkpoint_ts(1_700_000_000)
            .commit_subject("Golden subject")
            .commit_body("Golden body\nline2")
            .authorship_note("note-blob")
            .hunks(r#"[{"a":1}]"#)
            .author_ts(1_700_000_100)
            .commit_ts(1_700_000_200)
            .patch_id("patch-abc");

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": 11,
            "1": 22,
            "2": 33,
            "3": ["all", "claude-code:opus"],
            "5": [40, 41],
            "6": [42, 43],
            "10": 1_700_000_000u64,
            "11": "Golden subject",
            "12": "Golden body\nline2",
            "13": "note-blob",
            "14": r#"[{"a":1}]"#,
            "15": 1_700_000_100u64,
            "16": 1_700_000_200u64,
            "17": "patch-abc",
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 14, "exactly the 14 live fields, no more");

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":11,"1":22,"2":33,"3":["all","claude-code:opus"],"5":[40,41],"6":[42,43],"10":1700000000,"11":"Golden subject","12":"Golden body\nline2","13":"note-blob","14":"[{\"a\":1}]","15":1700000100,"16":1700000200,"17":"patch-abc"}"#
        );

        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }

    #[test]
    fn test_committed_values_golden_partially_populated() {
        // Exercises all three `PosField` states (value / explicit null /
        // absent) across every field type in the struct.
        let values = CommittedValues::new()
            .human_additions(5) // value (u32)
            .git_diff_deleted_lines_null() // explicit null (u32)
            // git_diff_added_lines: absent (u32)
            .tool_model_pairs_null() // explicit null (Vec<String>)
            .ai_additions(vec![7, 8]) // value (Vec<u32>)
            // ai_accepted: absent (Vec<u32>)
            .first_checkpoint_ts_null() // explicit null (u64)
            .commit_subject("partial subject") // value (String)
            .commit_body_null() // explicit null (String)
            // authorship_note: absent (String)
            .hunks("[]") // value (String)
            // author_ts: absent (u64)
            .commit_ts(1_650_000_000) // value (u64)
            .patch_id_null(); // explicit null (String)

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": 5,
            "1": null,
            "3": null,
            "5": [7, 8],
            "10": null,
            "11": "partial subject",
            "12": null,
            "14": "[]",
            "16": 1_650_000_000u64,
            "17": null,
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 10);
        for absent in ["2", "6", "13", "15"] {
            assert!(
                !sparse.contains_key(absent),
                "position {absent} must be omitted, not null"
            );
        }

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":5,"1":null,"3":null,"5":[7,8],"10":null,"11":"partial subject","12":null,"14":"[]","16":1650000000,"17":null}"#
        );

        let restored = <CommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.human_additions, Some(Some(5)));
        assert_eq!(restored.git_diff_deleted_lines, Some(None));
        assert_eq!(restored.git_diff_added_lines, None);
        assert_eq!(restored.tool_model_pairs, Some(None));
        assert_eq!(restored.ai_additions, Some(Some(vec![7, 8])));
        assert_eq!(restored.ai_accepted, None);
        assert_eq!(restored.first_checkpoint_ts, Some(None));
        assert_eq!(
            restored.commit_subject,
            Some(Some("partial subject".to_string()))
        );
        assert_eq!(restored.commit_body, Some(None));
        assert_eq!(restored.authorship_note, None);
        assert_eq!(restored.hunks, Some(Some("[]".to_string())));
        assert_eq!(restored.author_ts, None);
        assert_eq!(restored.commit_ts, Some(Some(1_650_000_000)));
        assert_eq!(restored.patch_id, Some(None));

        // Round-trip: re-serializing the restored value reproduces the
        // exact same sparse map, including the omitted/absent positions.
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }
}
