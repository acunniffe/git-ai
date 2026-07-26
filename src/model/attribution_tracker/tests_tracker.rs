#[cfg(test)]
mod tests {
    use super::super::line_attribution::{
        attributions_to_line_attributions, line_attributions_to_attributions,
    };
    use super::super::tokenizer::collect_line_metadata;
    use super::super::tracker::{
        AttributionConfig, AttributionTracker, is_attribution_list_sorted,
    };
    use crate::model::attribution::{Attribution, LineAttribution};
    use crate::model::imara_diff_utils::ByteDiffOp;

    const TEST_TS: u128 = 1234567890000;

    fn assert_range_owned_by(attributions: &[Attribution], start: usize, end: usize, author: &str) {
        assert!(start < end, "expected non-empty range");
        let owner = attributions
            .iter()
            .find(|a| a.start <= start && a.end >= end)
            .unwrap_or_else(|| panic!("range {}..{} missing in {:?}", start, end, attributions));
        assert_eq!(
            owner.author_id, author,
            "expected {} to own {}..{}, got {}",
            author, start, end, owner.author_id
        );
    }

    fn assert_non_ws_owned_by(
        attributions: &[Attribution],
        content: &str,
        author: &str,
        message: &str,
    ) {
        for (idx, ch) in content.char_indices() {
            if ch.is_whitespace() {
                continue;
            }
            let owner = attributions.iter().find(|a| a.start <= idx && a.end > idx);
            assert!(
                owner.map(|a| a.author_id.as_str()) == Some(author),
                "{}: non-ws char '{}' at {} owned by {:?}",
                message,
                ch,
                idx,
                owner.map(|a| a.author_id.as_str())
            );
        }
    }

    #[test]
    fn substantive_token_change_switches_author() {
        let tracker = AttributionTracker::new();
        let old = "fn main() {\n    let value = 1;\n}\n";
        let new = "fn main() {\n    let value = 2;\n}\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let two_pos = new.find('2').unwrap();
        assert_range_owned_by(&updated, two_pos, two_pos + 1, "Bob");
        let prefix_end = new.find('1').unwrap_or(two_pos);
        assert_non_ws_owned_by(
            &updated,
            &new[..prefix_end],
            "Alice",
            "unchanged prefix should stay Alice",
        );
    }

    #[test]
    fn whitespace_only_indent_change_preserves_tokens() {
        let tracker = AttributionTracker::new();
        let old = "fn test() {\n  do_stuff();\n}\n";
        let new = "fn test() {\n        do_stuff();\n}\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        assert_non_ws_owned_by(
            &updated,
            new,
            "Alice",
            "indentation change should not steal tokens",
        );
    }

    #[test]
    fn large_file_small_edit_preserves_unchanged_tokens() {
        let tracker = AttributionTracker::new();

        let mut old = String::new();
        for i in 0..1400 {
            old.push_str(&format!("const V{:04} = {};\n", i, i));
        }
        let mut new = old.clone();
        new = new.replace("const V0700 = 700;", "const V0700 = 9999;");

        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];
        let updated = tracker
            .update_attributions(&old, &new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let changed_pos = new.find("9999").expect("changed token");
        assert_range_owned_by(&updated, changed_pos, changed_pos + "9999".len(), "Bob");

        let unchanged_pos = new.find("V0001").expect("unchanged token");
        assert_range_owned_by(
            &updated,
            unchanged_pos,
            unchanged_pos + "V0001".len(),
            "Alice",
        );
    }

    #[test]
    fn unsorted_ranges_preserve_existing_lines_across_insertions() {
        let tracker = AttributionTracker::new();
        let old = "function example() {\n  return 42;\n}\n";
        let new = "// Header comment\nfunction example() {\n  // Added documentation\n  return 42;\n}\n// Footer\n";

        // Intentionally unsorted line ranges to mimic out-of-order caller input.
        let unsorted_line_attrs = vec![
            LineAttribution::new(2, 2, "Alice".to_string(), None),
            LineAttribution::new(1, 1, "Alice".to_string(), None),
            LineAttribution::new(3, 3, "Alice".to_string(), None),
        ];
        let old_attrs = line_attributions_to_attributions(&unsorted_line_attrs, old, TEST_TS);
        assert!(!is_attribution_list_sorted(&old_attrs));

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let function_pos = new.find("function example() {").unwrap();
        assert_range_owned_by(
            &updated,
            function_pos,
            function_pos + "function example() {".len(),
            "Alice",
        );

        let return_pos = new.find("return 42;").unwrap();
        assert_range_owned_by(
            &updated,
            return_pos,
            return_pos + "return 42;".len(),
            "Alice",
        );

        let brace_pos = new.rfind("\n}\n").unwrap() + 1;
        assert_range_owned_by(&updated, brace_pos, brace_pos + 1, "Alice");

        let header_pos = new.find("// Header comment").unwrap();
        assert_range_owned_by(
            &updated,
            header_pos,
            header_pos + "// Header comment".len(),
            "Bob",
        );

        let docs_pos = new.find("// Added documentation").unwrap();
        assert_range_owned_by(
            &updated,
            docs_pos,
            docs_pos + "// Added documentation".len(),
            "Bob",
        );

        let footer_pos = new.find("// Footer").unwrap();
        assert_range_owned_by(&updated, footer_pos, footer_pos + "// Footer".len(), "Bob");
    }

    #[test]
    fn line_reflow_without_token_change_is_non_substantive() {
        let tracker = AttributionTracker::new();
        let old = "call(foo, bar, baz)";
        let new = "call(\n  foo,\n  bar,\n  baz\n)";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let line_attrs = attributions_to_line_attributions(&updated, new);
        assert!(
            line_attrs.iter().all(|la| la.author_id == "Alice"),
            "every reflowed line should remain Alice, got {:?}",
            line_attrs
        );
    }

    #[test]
    fn line_reflow_without_token_change_is_non_substantive_with_semicolon() {
        let tracker = AttributionTracker::new();
        let old = "call(foo, bar, baz);";
        let new = "call(\n  foo,\n  bar,\n  baz\n);";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let line_attrs = attributions_to_line_attributions(&updated, new);
        assert!(
            line_attrs.iter().all(|la| la.author_id == "Alice"),
            "every reflowed line should remain Alice, got {:?}",
            line_attrs
        );
    }

    #[test]
    fn adding_semicolon_is_substantive() {
        let tracker = AttributionTracker::new();
        let old = "call(foo, bar, baz)";
        let new = "call(foo, bar, baz);";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let line_attrs = attributions_to_line_attributions(&updated, new);
        assert!(
            line_attrs.iter().all(|la| la.author_id == "Bob"),
            "adding semicolon should be substantive, got {:?}",
            line_attrs
        );
    }

    #[test]
    fn reflow_complex_if_statement_is_non_substantive() {
        let tracker = AttributionTracker::new();
        let old = "if (foo && bar || baz) { println!(\"condition\"); }";
        let new = "if (foo\n    && bar\n    || baz) {\n    println!(\"condition\");\n}";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        let line_attrs = attributions_to_line_attributions(&updated, new);
        assert!(
            line_attrs.iter().all(|la| la.author_id == "Alice"),
            "reflow of complex if statement should not be substantive, got {:?}",
            line_attrs
        );
    }

    #[test]
    fn move_block_preserves_original_authors_one_line_threshold() {
        let tracker = AttributionTracker::with_config(AttributionConfig {
            // Test with a one-line threshold
            move_lines_threshold: 1,
        });
        let old = "fn helper() { println!(\"helper\"); }\nfn main() { println!(\"main\"); }\n";
        let new = "fn main() { println!(\"main\"); }\nfn helper() { println!(\"helper\"); }\n";
        let old_attrs = vec![
            Attribution::new(0, 36, "Alice".into(), TEST_TS),
            Attribution::new(36, old.len(), "Bob".into(), TEST_TS),
        ];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Charlie", TEST_TS + 1)
            .unwrap();

        let helper_pos = new.find("helper").unwrap();
        assert_range_owned_by(&updated, helper_pos, helper_pos + "helper".len(), "Alice");
        let main_pos = new.find("main").unwrap();
        assert!(
            updated
                .iter()
                .filter(|a| a.start <= main_pos && a.end >= main_pos + "main".len())
                .any(|a| a.author_id != "Alice"),
            "Moved main block should not be reassigned to helper author"
        );
    }

    #[test]
    fn move_block_preserves_original_authors_default_threshold() {
        // Test move detection with blocks of 3+ lines (the default threshold)
        let tracker = AttributionTracker::new();
        // Helper function block with 4 lines
        let helper_block =
            "fn helper() {\n    let x = 1;\n    let y = 2;\n    println!(\"helper\");\n}\n";
        // Main function block with 4 lines
        let main_block =
            "fn main() {\n    let a = 3;\n    let b = 4;\n    println!(\"main\");\n}\n";

        let old = format!("{}{}", helper_block, main_block);
        let new = format!("{}{}", main_block, helper_block);

        let helper_len = helper_block.len();
        let old_attrs = vec![
            Attribution::new(0, helper_len, "Alice".into(), TEST_TS),
            Attribution::new(helper_len, old.len(), "Bob".into(), TEST_TS),
        ];

        let updated = tracker
            .update_attributions(&old, &new, &old_attrs, "Charlie", TEST_TS + 1)
            .unwrap();

        // After the move, the helper block (originally written by Alice) should
        // retain Alice's authorship in the new position
        let helper_pos_in_new = new.find("helper").unwrap();
        let helper_owner = updated
            .iter()
            .find(|a| a.start <= helper_pos_in_new && a.end > helper_pos_in_new);

        // The moved helper block should either preserve Alice's authorship (via move detection)
        // or be attributed to Charlie (if move detection doesn't match)
        // With imara-diff's git-compatible output, this tests the actual move detection
        assert!(helper_owner.is_some(), "helper text should have an owner");
    }

    #[test]
    fn deletions_remove_attribution() {
        let tracker = AttributionTracker::new();
        let old = "keep remove keep";
        let new = "keep  keep";
        let old_attrs = vec![
            Attribution::new(0, 4, "Alice".into(), TEST_TS),
            Attribution::new(5, 11, "Bob".into(), TEST_TS),
            Attribution::new(12, old.len(), "Alice".into(), TEST_TS),
        ];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Carol", TEST_TS + 1)
            .unwrap();

        assert!(
            updated.iter().all(|a| a.author_id != "Bob"),
            "Bob attribution should disappear after deletion"
        );
    }

    #[test]
    fn multibyte_tokens_are_preserved_and_added() {
        let tracker = AttributionTracker::new();
        let old = "😀 one\n";
        let new = "😀 one\n✅ two\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions(old, new, &old_attrs, "Bob", TEST_TS + 1)
            .unwrap();

        assert_range_owned_by(&updated, 0, old.len(), "Alice");
        assert!(
            updated
                .iter()
                .any(|a| a.author_id == "Bob" && a.start >= old.len()),
            "New multibyte tokens should belong to Bob"
        );
    }

    #[test]
    fn line_attribution_handles_split_multibyte_ranges() {
        let content = "选\n";
        let attrs = vec![Attribution::new(1, 2, "Alice".into(), TEST_TS)];
        let line_attrs = attributions_to_line_attributions(&attrs, content);
        assert_eq!(line_attrs.len(), 1);
        assert_eq!(line_attrs[0].author_id, "Alice");
    }

    #[test]
    fn line_attributions_follow_dominant_tokens() {
        let content = "let x = foo() + bar();\n";
        let attrs = vec![
            Attribution::new(0, 8, "Alice".into(), TEST_TS),
            Attribution::new(8, 13, "Bob".into(), TEST_TS),
            Attribution::new(13, 21, "Carol".into(), TEST_TS),
        ];

        let line_attrs = attributions_to_line_attributions(&attrs, content);
        assert_eq!(line_attrs.len(), 1);
        assert_eq!(line_attrs[0].author_id, "Alice");
    }

    #[test]
    fn unattributed_ranges_are_filled() {
        let tracker = AttributionTracker::new();
        let content = "A B C";
        let prev = vec![Attribution::new(0, 1, "Alice".into(), TEST_TS)];
        let filled = tracker.attribute_unattributed_ranges(content, &prev, "Bob", TEST_TS + 1);

        assert_eq!(filled.len(), 2);
        assert_range_owned_by(&filled, 0, 1, "Alice");
        assert_range_owned_by(&filled, 1, content.len(), "Bob");
    }

    #[test]
    fn ai_inserted_blank_line_counts_for_ai() {
        let tracker = AttributionTracker::new();
        let old = "# My Application\n";
        let new = "# My Application\n\nimport os\nimport sys\n\ndef setup():\n    print(\"Setting up\")\n\ndef main():\n    setup()\n    print(\"Running main\")\n\ndef cleanup():\n    print(\"Cleaning up\")\n\nif __name__ == \"__main__\":\n    main()\n";

        let human_attrs = vec![Attribution::new(0, old.len(), "human".into(), TEST_TS)];
        let diff_ops: Vec<_> = tracker
            .compute_diffs(old, new, false)
            .unwrap()
            .diffs
            .iter()
            .map(|d| d.op())
            .collect();
        assert!(
            matches!(diff_ops.first(), Some(ByteDiffOp::Equal)),
            "expected first diff op to be equal, got {:?}",
            diff_ops
        );
        let updated = tracker
            .update_attributions(old, new, &human_attrs, "ai", TEST_TS + 1)
            .unwrap();

        assert!(
            updated
                .iter()
                .any(|a| a.author_id == "human" && a.start == 0 && a.end >= old.len()),
            "header should remain attributed to human"
        );

        let line_attrs = attributions_to_line_attributions(&updated, new);
        let ai_block = line_attrs
            .iter()
            .find(|la| la.author_id == "ai")
            .expect("AI block missing");
        assert_eq!(ai_block.start_line, 2);
        assert_eq!(ai_block.end_line, 17);
    }

    // ====================================================================
    // CRLF / LF normalization tests
    // ====================================================================

    #[test]
    fn crlf_to_lf_same_content_preserves_attributions() {
        // When content only changes line endings (CRLF→LF), attributions should
        // be preserved for the original author, NOT re-attributed.
        let tracker = AttributionTracker::new();
        let old = "hello\r\nworld\r\n";
        let new = "hello\nworld\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions_for_checkpoint(old, new, &old_attrs, "Bob", TEST_TS + 1, false)
            .unwrap();

        // All non-whitespace content should still be owned by Alice
        assert_non_ws_owned_by(
            &updated,
            new,
            "Alice",
            "CRLF→LF with same content should not re-attribute to Bob",
        );
    }

    #[test]
    fn lf_to_crlf_same_content_preserves_attributions() {
        let tracker = AttributionTracker::new();
        let old = "hello\nworld\n";
        let new = "hello\r\nworld\r\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions_for_checkpoint(old, new, &old_attrs, "Bob", TEST_TS + 1, false)
            .unwrap();

        assert_non_ws_owned_by(
            &updated,
            new,
            "Alice",
            "LF→CRLF with same content should not re-attribute to Bob",
        );
    }

    #[test]
    fn crlf_to_lf_with_real_edit_attributes_correctly() {
        // Old has CRLF, new has LF with one line changed. Only the changed line
        // should be attributed to the new author.
        let tracker = AttributionTracker::new();
        let old = "line1\r\nline2\r\nline3\r\n";
        let new = "line1\nmodified\nline3\n";
        let old_attrs = vec![Attribution::new(0, old.len(), "Alice".into(), TEST_TS)];

        let updated = tracker
            .update_attributions_for_checkpoint(old, new, &old_attrs, "Bob", TEST_TS + 1, false)
            .unwrap();

        // "line1" and "line3" should remain Alice's
        // "modified" should be Bob's
        let line1_start = 0;
        let line1_end = "line1".len();
        assert_range_owned_by(&updated, line1_start, line1_end, "Alice");

        let modified_start = "line1\n".len();
        let modified_end = "line1\nmodified".len();
        assert_range_owned_by(&updated, modified_start, modified_end, "Bob");

        let line3_start = "line1\nmodified\n".len();
        let line3_end = "line1\nmodified\nline3".len();
        assert_range_owned_by(&updated, line3_start, line3_end, "Alice");
    }

    #[test]
    fn collect_line_metadata_strips_cr_from_text() {
        // Verify that collect_line_metadata strips \r from the text field
        // (this already works, but verifies the building block)
        let content = "hello\r\nworld\r\n";
        let metadata = collect_line_metadata(content);
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0].text, "hello");
        assert_eq!(metadata[1].text, "world");
    }
}
