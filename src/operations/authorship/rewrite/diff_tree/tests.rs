use super::*;

#[test]
fn test_extract_b_path_simple() {
    assert_eq!(
        extract_b_path("a/src/main.rs b/src/main.rs"),
        Some("src/main.rs".to_string())
    );
}

#[test]
fn test_extract_b_path_rename() {
    assert_eq!(
        extract_b_path("a/src/old.rs b/src/new.rs"),
        Some("src/new.rs".to_string())
    );
}

#[test]
fn test_extract_b_path_with_spaces() {
    assert_eq!(
        extract_b_path("a/path with spaces b/another path"),
        Some("another path".to_string())
    );
}

#[test]
fn test_parse_diff_tree_output_simple() {
    let output = "\
diff --git a/src/foo.rs b/src/foo.rs
index abc123..def456 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,3 +10,5 @@ fn foo()
+added line 1
+added line 2
";
    let result = parse_diff_tree_output(output);
    assert!(result.renames.is_empty());
    assert_eq!(result.hunks_by_file.len(), 1);
    let hunks = &result.hunks_by_file["src/foo.rs"];
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].old_start, 10);
    assert_eq!(hunks[0].old_count, 3);
    assert_eq!(hunks[0].new_start, 10);
    assert_eq!(hunks[0].new_count, 5);
}

#[test]
fn test_parse_diff_tree_output_with_rename() {
    let output = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index abc123..def456 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -5,2 +5,3 @@ fn bar()
+new line
";
    let result = parse_diff_tree_output(output);
    assert_eq!(result.renames.len(), 1);
    assert_eq!(
        result.renames[0],
        ("src/old.rs".to_string(), "src/new.rs".to_string())
    );
    let hunks = &result.hunks_by_file["src/new.rs"];
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].old_start, 5);
    assert_eq!(hunks[0].old_count, 2);
    assert_eq!(hunks[0].new_start, 5);
    assert_eq!(hunks[0].new_count, 3);
}

#[test]
fn test_parse_diff_tree_output_multiple_files() {
    let output = "\
diff --git a/file1.rs b/file1.rs
index aaa..bbb 100644
--- a/file1.rs
+++ b/file1.rs
@@ -1,2 +1,3 @@
+line
diff --git a/file2.rs b/file2.rs
index ccc..ddd 100644
--- a/file2.rs
+++ b/file2.rs
@@ -10,0 +11,2 @@
+line1
+line2
";
    let result = parse_diff_tree_output(output);
    assert_eq!(result.hunks_by_file.len(), 2);
    assert_eq!(result.hunks_by_file["file1.rs"].len(), 1);
    assert_eq!(result.hunks_by_file["file2.rs"].len(), 1);
    assert_eq!(result.hunks_by_file["file2.rs"][0].old_start, 10);
    assert_eq!(result.hunks_by_file["file2.rs"][0].old_count, 0);
    assert_eq!(result.hunks_by_file["file2.rs"][0].new_start, 11);
    assert_eq!(result.hunks_by_file["file2.rs"][0].new_count, 2);
}

#[test]
fn test_parse_diff_tree_output_binary() {
    let output = "\
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ
";
    let result = parse_diff_tree_output(output);
    // No hunks for binary files
    assert!(
        result
            .hunks_by_file
            .get("image.png")
            .is_none_or(|h| h.is_empty())
    );
}

#[test]
fn test_parse_diff_tree_empty_output() {
    let result = parse_diff_tree_output("");
    assert!(result.hunks_by_file.is_empty());
    assert!(result.renames.is_empty());
}

#[test]
fn test_is_tree_pair_separator_valid() {
    let line = "1778ed95466977076f4e5908e6500789be732d2e 471b7bbf5998ffa15a81b17ee9f6854a357a2a6a";
    assert!(is_tree_pair_separator(line));
}

#[test]
fn test_is_tree_pair_separator_invalid() {
    assert!(!is_tree_pair_separator("diff --git a/foo b/foo"));
    assert!(!is_tree_pair_separator("@@ -1,2 +1,3 @@"));
    assert!(!is_tree_pair_separator(""));
    assert!(!is_tree_pair_separator("short"));
    // Missing space
    assert!(!is_tree_pair_separator(
        "1778ed95466977076f4e5908e6500789be732d2e471b7bbf5998ffa15a81b17ee9f6854a357a2a6a"
    ));
}

#[test]
fn test_is_tree_pair_separator_accepts_sha256_pair() {
    // Regression (#10): a SHA-256 tree-pair separator is "64hex 64hex"
    // (129 bytes), not the hard-coded 81-byte SHA-1 shape.
    let a = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let b = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    let line = format!("{} {}", a, b);
    assert_eq!(line.len(), 129);
    assert!(is_tree_pair_separator(&line));
}

#[test]
fn test_parse_batched_diff_tree_output_single_pair() {
    let output = "\
1778ed95466977076f4e5908e6500789be732d2e 471b7bbf5998ffa15a81b17ee9f6854a357a2a6a
diff --git a/f.txt b/f.txt
index a29bdeb..c0d0fb4 100644
--- a/f.txt
+++ b/f.txt
@@ -1,0 +2 @@ line1
+line2
";
    let results = parse_batched_diff_tree_output(output, 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].hunks_by_file.len(), 1);
    assert_eq!(results[0].hunks_by_file["f.txt"][0].new_count, 1);
}

#[test]
fn test_parse_batched_diff_tree_output_multiple_pairs() {
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/f.txt b/f.txt
index a29bdeb..c0d0fb4 100644
--- a/f.txt
+++ b/f.txt
@@ -1,0 +2 @@ line1
+line2
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/g.txt b/g.txt
index eee..fff 100644
--- a/g.txt
+++ b/g.txt
@@ -5,2 +5,3 @@
+new line
";
    let results = parse_batched_diff_tree_output(output, 2);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].hunks_by_file.len(), 1);
    assert!(results[0].hunks_by_file.contains_key("f.txt"));
    assert_eq!(results[1].hunks_by_file.len(), 1);
    assert!(results[1].hunks_by_file.contains_key("g.txt"));
}

#[test]
fn test_parse_batched_diff_tree_output_identical_trees() {
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
";
    let results = parse_batched_diff_tree_output(output, 1);
    assert_eq!(results.len(), 1);
    assert!(results[0].hunks_by_file.is_empty());
    assert!(results[0].renames.is_empty());
}

#[test]
fn test_parse_batched_diff_tree_output_mixed_identical_and_changed() {
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/f.txt b/f.txt
@@ -1,0 +2 @@
+x
cccccccccccccccccccccccccccccccccccccccc cccccccccccccccccccccccccccccccccccccccc
dddddddddddddddddddddddddddddddddddddddd eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
diff --git a/g.txt b/g.txt
@@ -3,1 +3,2 @@
+y
";
    let results = parse_batched_diff_tree_output(output, 3);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].hunks_by_file.len(), 1);
    assert!(results[1].hunks_by_file.is_empty());
    assert_eq!(results[2].hunks_by_file.len(), 1);
}

#[test]
fn test_parse_batched_diff_tree_output_empty() {
    let results = parse_batched_diff_tree_output("", 0);
    assert!(results.is_empty());
}

#[test]
fn test_batched_diff_tree_parser_streams_line_by_line() {
    // The streaming exec path feeds the parser one line at a time (without
    // trailing newlines); the result must match parsing the whole output.
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index abc123..def456 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -5,2 +5,3 @@ fn bar()
+new line
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/g.txt b/g.txt
index eee..fff 100644
--- a/g.txt
+++ b/g.txt
@@ -10,0 +11,2 @@
+line1
+line2
";
    // Pad expected_pairs beyond what git emitted (identical trees case).
    let mut parser = BatchedDiffTreeParser::new(3);
    for line in output.lines() {
        parser.feed_line(line);
    }
    let streamed = parser.finish();

    assert_eq!(streamed, parse_batched_diff_tree_output(output, 3));
    assert_eq!(streamed.len(), 3);
    assert_eq!(
        streamed[0].renames,
        vec![("src/old.rs".to_string(), "src/new.rs".to_string())]
    );
    assert_eq!(streamed[0].added_lines_by_file["src/new.rs"], vec![5]);
    assert_eq!(streamed[1].added_lines_by_file["g.txt"], vec![11, 12]);
    assert_eq!(streamed[2], DiffTreeResult::default());
}

#[test]
fn test_batched_diff_tree_parser_drains_completed_chunks_without_losing_tail() {
    let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/a.txt b/a.txt
@@ -1,0 +1 @@
+a
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/b.txt b/b.txt
@@ -1,0 +1 @@
+b
eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee ffffffffffffffffffffffffffffffffffffffff
diff --git a/c.txt b/c.txt
@@ -1,0 +1 @@
+c
";

    let mut parser = BatchedDiffTreeParser::new(4);
    let mut chunks = Vec::new();
    for line in output.lines() {
        parser.feed_line(line);
        if parser.completed_count() >= 2 {
            chunks.push(parser.take_completed());
        }
    }
    chunks.push(parser.take_all());

    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2]);
    assert!(chunks[0][0].hunks_by_file.contains_key("a.txt"));
    assert!(chunks[0][1].hunks_by_file.contains_key("b.txt"));
    assert!(chunks[1][0].hunks_by_file.contains_key("c.txt"));
    assert_eq!(chunks[1][1], DiffTreeResult::default());
}

#[test]
fn test_batched_diff_tree_parser_drains_at_production_chunk_boundary() {
    let pair_count = DIFF_TREE_STREAM_CHUNK_SIZE + 2;
    let output = (0..pair_count)
        .map(|index| format!("{:040x} {:040x}\n", index + 1, index + pair_count + 1))
        .collect::<String>();
    let mut parser = BatchedDiffTreeParser::new(pair_count);
    let mut chunk_lengths = Vec::new();

    for line in output.lines() {
        parser.feed_line(line);
        if parser.completed_count() >= DIFF_TREE_STREAM_CHUNK_SIZE {
            chunk_lengths.push(parser.take_completed().len());
        }
    }
    chunk_lengths.push(parser.take_all().len());

    assert_eq!(chunk_lengths, vec![DIFF_TREE_STREAM_CHUNK_SIZE, 2]);
}
