use super::pos_event::pos_event;

/// Value positions for "rewrite_committed" event.
pub mod rewrite_committed_pos {
    pub const HUMAN_ADDITIONS: usize = 0;
    pub const GIT_DIFF_DELETED_LINES: usize = 1;
    pub const GIT_DIFF_ADDED_LINES: usize = 2;
    pub const TOOL_MODEL_PAIRS: usize = 3;
    // Keep positions 0-14 aligned with committed_pos for ingestion consistency.
    // Position 4 mirrors committed_pos::MIXED_ADDITIONS, which is no longer emitted.
    pub const AI_ADDITIONS: usize = 5;
    pub const AI_ACCEPTED: usize = 6;
    // Positions 7-9 mirror removed committed event fields.
    // Position 10 is intentionally omitted: rewrite events have no first checkpoint timestamp.
    pub const COMMIT_SUBJECT: usize = 11;
    pub const COMMIT_BODY: usize = 12;
    pub const AUTHORSHIP_NOTE: usize = 13;
    pub const HUNKS: usize = 14;
    pub const OPERATION_KIND: usize = 15;
    pub const ORIGINAL_COMMIT_SHAS: usize = 16;
}

pos_event! {
    /// Values for Event ID 7: rewrite_committed.
    ///
    /// Recorded after rewrite operations create new commit SHAs and authorship
    /// notes have been migrated to those post-rewrite commits.
    struct RewriteCommittedValues uses rewrite_committed_pos for RewriteCommitted {
        human_additions: u32 @ HUMAN_ADDITIONS,
        git_diff_deleted_lines: u32 @ GIT_DIFF_DELETED_LINES,
        git_diff_added_lines: u32 @ GIT_DIFF_ADDED_LINES,
        tool_model_pairs: [String] @ TOOL_MODEL_PAIRS,
        ai_additions: [u32] @ AI_ADDITIONS,
        ai_accepted: [u32] @ AI_ACCEPTED,
        commit_subject: String @ COMMIT_SUBJECT,
        commit_body: String @ COMMIT_BODY,
        authorship_note: String @ AUTHORSHIP_NOTE,
        hunks: String @ HUNKS,
        operation_kind: String @ OPERATION_KIND,
        original_commit_shas: [String] @ ORIGINAL_COMMIT_SHAS,
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
    fn test_rewrite_committed_values_event_id() {
        assert_eq!(
            RewriteCommittedValues::event_id(),
            MetricEventId::RewriteCommitted
        );
        assert_eq!(RewriteCommittedValues::event_id() as u16, 7);
    }

    #[test]
    fn test_rewrite_committed_values_sparse_roundtrip() {
        let original = RewriteCommittedValues::new()
            .human_additions(5)
            .git_diff_deleted_lines(2)
            .git_diff_added_lines(7)
            .tool_model_pairs(vec!["all".to_string(), "codex:gpt-5".to_string()])
            .ai_additions(vec![3, 3])
            .ai_accepted(vec![3, 3])
            .commit_subject("rebased commit")
            .commit_body_null()
            .authorship_note("note")
            .hunks("[]")
            .operation_kind("rebase")
            .original_commit_shas(vec!["old1".to_string()]);

        let sparse = PosEncoded::to_sparse(&original);

        assert!(!sparse.contains_key("10"));
        assert_eq!(sparse.get("15"), Some(&Value::String("rebase".to_string())));
        assert_eq!(
            sparse.get("16"),
            Some(&Value::Array(vec![Value::String("old1".to_string())]))
        );

        let restored = <RewriteCommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.human_additions, Some(Some(5)));
        assert_eq!(
            restored.tool_model_pairs,
            Some(Some(vec!["all".to_string(), "codex:gpt-5".to_string()]))
        );
        assert_eq!(restored.operation_kind, Some(Some("rebase".to_string())));
        assert_eq!(
            restored.original_commit_shas,
            Some(Some(vec!["old1".to_string()]))
        );
    }

    // --- Golden serialization tests -----------------------------------
    //
    // These pin the exact wire representation of `RewriteCommittedValues`,
    // including the deliberate committed_pos alignment gap at position 10
    // (rewrite events have no first-checkpoint timestamp). They are
    // independent of how the struct/impls are built (hand-written vs.
    // macro-generated): any future schema drift shows up here as a loud,
    // explicit diff.

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
    fn test_rewrite_committed_values_golden_fully_populated() {
        let values = RewriteCommittedValues::new()
            .human_additions(1)
            .git_diff_deleted_lines(2)
            .git_diff_added_lines(3)
            .tool_model_pairs(vec!["all".to_string(), "codex:gpt-5".to_string()])
            .ai_additions(vec![9, 9])
            .ai_accepted(vec![8, 8])
            .commit_subject("Rewrite subject")
            .commit_body("Rewrite body")
            .authorship_note("rewrite-note")
            .hunks("[]")
            .operation_kind("rebase")
            .original_commit_shas(vec!["sha1".to_string(), "sha2".to_string()]);

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": 1,
            "1": 2,
            "2": 3,
            "3": ["all", "codex:gpt-5"],
            "5": [9, 9],
            "6": [8, 8],
            "11": "Rewrite subject",
            "12": "Rewrite body",
            "13": "rewrite-note",
            "14": "[]",
            "15": "rebase",
            "16": ["sha1", "sha2"],
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 12, "exactly the 12 live fields, no more");
        assert!(
            !sparse.contains_key("10"),
            "position 10 (committed_pos::FIRST_CHECKPOINT_TS) has no rewrite_committed field"
        );

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":1,"1":2,"2":3,"3":["all","codex:gpt-5"],"5":[9,9],"6":[8,8],"11":"Rewrite subject","12":"Rewrite body","13":"rewrite-note","14":"[]","15":"rebase","16":["sha1","sha2"]}"#
        );

        let restored = <RewriteCommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }

    #[test]
    fn test_rewrite_committed_values_golden_partially_populated() {
        // Exercises all three `PosField` states (value / explicit null /
        // absent) across every field type in the struct.
        let values = RewriteCommittedValues::new()
            .human_additions_null() // explicit null (u32)
            .git_diff_deleted_lines(5) // value (u32)
            // git_diff_added_lines: absent (u32)
            // tool_model_pairs: absent (Vec<String>)
            .ai_additions_null() // explicit null (Vec<u32>)
            .ai_accepted(vec![1, 2]) // value (Vec<u32>)
            // commit_subject: absent (String)
            .commit_body_null() // explicit null (String)
            .authorship_note("note-x") // value (String)
            // hunks: absent (String)
            .operation_kind_null() // explicit null (String)
            .original_commit_shas(vec!["only-one".to_string()]); // value (Vec<String>)

        let sparse = PosEncoded::to_sparse(&values);

        let expected: SparseArray = serde_json::json!({
            "0": null,
            "1": 5,
            "5": null,
            "6": [1, 2],
            "12": null,
            "13": "note-x",
            "15": null,
            "16": ["only-one"],
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        assert_eq!(sparse, expected);
        assert_eq!(sparse.len(), 8);
        for absent in ["2", "3", "10", "11", "14"] {
            assert!(
                !sparse.contains_key(absent),
                "position {absent} must be omitted, not null"
            );
        }

        assert_eq!(
            pinned_json(&sparse),
            r#"{"0":null,"1":5,"5":null,"6":[1,2],"12":null,"13":"note-x","15":null,"16":["only-one"]}"#
        );

        let restored = <RewriteCommittedValues as PosEncoded>::from_sparse(&sparse);
        assert_eq!(restored.human_additions, Some(None));
        assert_eq!(restored.git_diff_deleted_lines, Some(Some(5)));
        assert_eq!(restored.git_diff_added_lines, None);
        assert_eq!(restored.tool_model_pairs, None);
        assert_eq!(restored.ai_additions, Some(None));
        assert_eq!(restored.ai_accepted, Some(Some(vec![1, 2])));
        assert_eq!(restored.commit_subject, None);
        assert_eq!(restored.commit_body, Some(None));
        assert_eq!(restored.authorship_note, Some(Some("note-x".to_string())));
        assert_eq!(restored.hunks, None);
        assert_eq!(restored.operation_kind, Some(None));
        assert_eq!(
            restored.original_commit_shas,
            Some(Some(vec!["only-one".to_string()]))
        );

        // Round-trip: re-serializing the restored value reproduces the
        // exact same sparse map, including the omitted/absent positions.
        assert_eq!(PosEncoded::to_sparse(&restored), sparse);
    }
}
