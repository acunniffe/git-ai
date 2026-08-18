use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::extract_json_object;
use git_ai::authorship::stats::CommitStats;
use std::fs;

fn gbk_hello_world() -> Vec<u8> {
    // "你好世界" in GBK encoding (4 characters, 8 bytes)
    // 你=0xC4E3, 好=0xBAC3, 世=0xCAC0, 界=0xBDE7
    vec![0xC4, 0xE3, 0xBA, 0xC3, 0xCA, 0xC0, 0xBD, 0xE7, b'\n']
}

fn gbk_multiline() -> Vec<u8> {
    // Three lines of GBK text
    let mut bytes = Vec::new();
    // Line 1: 你好 (hello) = C4E3 BAC3
    bytes.extend_from_slice(&[0xC4, 0xE3, 0xBA, 0xC3]);
    bytes.push(b'\n');
    // Line 2: 世界 (world) = CAC0 BDE7
    bytes.extend_from_slice(&[0xCA, 0xC0, 0xBD, 0xE7]);
    bytes.push(b'\n');
    // Line 3: 测试 (test) = B2E2 CAD4
    bytes.extend_from_slice(&[0xB2, 0xE2, 0xCA, 0xD4]);
    bytes.push(b'\n');
    bytes
}

fn latin1_bytes() -> Vec<u8> {
    // Latin-1 text with characters outside UTF-8
    // "café résumé" with Latin-1 accented chars (0xe9 = é in Latin-1, invalid as standalone UTF-8)
    b"caf\xe9 r\xe9sum\xe9\n".to_vec()
}

fn shift_jis_bytes() -> Vec<u8> {
    // Shift-JIS encoded Japanese text
    // こんにちは (konnichiwa) in Shift-JIS
    vec![
        0x82, 0xB1, 0x82, 0xF1, 0x82, 0xC9, 0x82, 0xBF, 0x82, 0xCD, b'\n',
    ]
}

fn mixed_valid_invalid_utf8() -> Vec<u8> {
    // Mix of valid UTF-8 ASCII lines and invalid UTF-8 lines
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"line one is ascii\n");
    // Invalid UTF-8 byte sequence
    bytes.extend_from_slice(&[0x80, 0x81, 0x82, b'\n']);
    bytes.extend_from_slice(b"line three is ascii\n");
    // Another invalid sequence
    bytes.extend_from_slice(&[0xFE, 0xFF, b'\n']);
    bytes.extend_from_slice(b"line five is ascii\n");
    bytes
}

// =============================================================================
// Core: Commit flow with non-UTF-8 files
// =============================================================================

#[test]
fn test_commit_gbk_encoded_file_succeeds() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_hello_world()).unwrap();

    let result = repo.stage_all_and_commit("Add GBK file");
    assert!(
        result.is_ok(),
        "Committing a GBK-encoded file should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_commit_latin1_encoded_file_succeeds() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("latin1_file.txt");
    fs::write(&file_path, latin1_bytes()).unwrap();

    let result = repo.stage_all_and_commit("Add Latin-1 file");
    assert!(
        result.is_ok(),
        "Committing a Latin-1 encoded file should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_commit_shift_jis_encoded_file_succeeds() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("shiftjis_file.txt");
    fs::write(&file_path, shift_jis_bytes()).unwrap();

    let result = repo.stage_all_and_commit("Add Shift-JIS file");
    assert!(
        result.is_ok(),
        "Committing a Shift-JIS encoded file should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_commit_mixed_valid_invalid_utf8_file_succeeds() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("mixed_encoding.txt");
    fs::write(&file_path, mixed_valid_invalid_utf8()).unwrap();

    let result = repo.stage_all_and_commit("Add mixed encoding file");
    assert!(
        result.is_ok(),
        "Committing a file with mixed valid/invalid UTF-8 should not fail, got: {:?}",
        result.err()
    );
}

// =============================================================================
// Stats: Ensure stats work with non-UTF-8 files present
// =============================================================================

#[test]
fn test_stats_with_non_utf8_file_only() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_data.txt");
    fs::write(&file_path, gbk_multiline()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();

    assert!(
        stats.git_diff_added_lines >= 3,
        "Git should count added lines even for non-UTF-8 files, got: {}",
        stats.git_diff_added_lines
    );
}

#[test]
fn test_stats_with_non_utf8_and_utf8_files_mixed() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut normal_file = repo.filename("normal.txt");
    normal_file.set_contents(crate::lines!["line one".ai(), "line two".ai()]);

    let gbk_path = repo.path().join("gbk_data.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    repo.stage_all_and_commit("Add mixed files").unwrap();

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();

    assert_eq!(
        stats.ai_additions, 2,
        "AI additions from UTF-8 file should be counted correctly"
    );
    assert!(
        stats.git_diff_added_lines >= 5,
        "Git should count added lines from both files, got: {}",
        stats.git_diff_added_lines
    );
}

#[test]
fn test_stats_json_output_valid_with_non_utf8_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("latin1_data.txt");
    fs::write(&file_path, latin1_bytes()).unwrap();
    repo.stage_all_and_commit("Add Latin-1 file").unwrap();

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let result: Result<CommitStats, _> = serde_json::from_str(&json);
    assert!(
        result.is_ok(),
        "Stats JSON should be valid even with non-UTF-8 files"
    );
}

// =============================================================================
// Blame: Ensure blame works (or gracefully degrades) with non-UTF-8 files
// =============================================================================

#[test]
fn test_blame_non_utf8_file_does_not_crash() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_multiline()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    let result = repo.git_ai(&["blame", "gbk_file.txt"]);
    assert!(
        result.is_ok(),
        "Blame on a non-UTF-8 file should not crash, got: {:?}",
        result.err()
    );
}

#[test]
fn test_blame_utf8_file_unaffected_by_non_utf8_neighbor() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut normal_file = repo.filename("normal.rs");
    normal_file.set_contents(crate::lines![
        "fn main() {".ai(),
        "    println!(\"hello\");".ai(),
        "}".ai(),
    ]);

    let gbk_path = repo.path().join("gbk_data.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    repo.stage_all_and_commit("Add both files").unwrap();

    normal_file.assert_lines_and_blame(crate::lines![
        "fn main() {".ai(),
        "    println!(\"hello\");".ai(),
        "}".ai(),
    ]);
}

// =============================================================================
// Edit flow: Modifying non-UTF-8 files across commits
// =============================================================================

#[test]
fn test_edit_non_utf8_file_second_commit() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_hello_world()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    fs::write(&file_path, gbk_multiline()).unwrap();
    let result = repo.stage_all_and_commit("Edit GBK file");
    assert!(
        result.is_ok(),
        "Editing a non-UTF-8 file should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_delete_non_utf8_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_hello_world()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    fs::remove_file(&file_path).unwrap();
    let result = repo.stage_all_and_commit("Delete GBK file");
    assert!(
        result.is_ok(),
        "Deleting a non-UTF-8 file should not fail, got: {:?}",
        result.err()
    );
}

// =============================================================================
// AI attribution: UTF-8 files get correct attribution even with non-UTF-8 neighbors
// =============================================================================

#[test]
fn test_ai_attribution_preserved_with_non_utf8_in_same_commit() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let mut ai_file = repo.filename("ai_output.py");
    ai_file.set_contents(crate::lines![
        "def hello():".ai(),
        "    return 'world'".ai(),
    ]);

    let gbk_path = repo.path().join("legacy_gbk.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    let commit = repo.stage_all_and_commit("Mixed commit").unwrap();

    assert!(
        !commit.authorship_log.attestations.is_empty(),
        "Should have attestation for the AI-written file"
    );

    let ai_attestation = commit
        .authorship_log
        .attestations
        .iter()
        .find(|a| a.file_path == "ai_output.py");
    assert!(
        ai_attestation.is_some(),
        "Should have attestation specifically for ai_output.py"
    );

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();

    assert_eq!(
        stats.ai_additions, 2,
        "AI additions should be correctly counted for the UTF-8 file"
    );
}

#[test]
fn test_human_and_ai_edits_with_non_utf8_file_present() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let gbk_path = repo.path().join("legacy.txt");
    fs::write(&gbk_path, gbk_hello_world()).unwrap();
    repo.stage_all_and_commit("Add legacy GBK file").unwrap();

    let mut code_file = repo.filename("app.js");
    code_file.set_contents(crate::lines![
        "const a = 1;".human(),
        "const b = generateCode();".ai(),
        "const c = 3;".human(),
    ]);
    repo.stage_all_and_commit("Add code file").unwrap();

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();

    assert_eq!(stats.ai_additions, 1, "1 AI line should be counted");
    assert_eq!(stats.human_additions, 2, "2 human lines should be counted");
}

// =============================================================================
// Multiple non-UTF-8 files in a single commit
// =============================================================================

#[test]
fn test_multiple_non_utf8_encodings_in_one_commit() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let gbk_path = repo.path().join("gbk_file.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    let latin1_path = repo.path().join("latin1_file.txt");
    fs::write(&latin1_path, latin1_bytes()).unwrap();

    let sjis_path = repo.path().join("shiftjis_file.txt");
    fs::write(&sjis_path, shift_jis_bytes()).unwrap();

    let result = repo.stage_all_and_commit("Add files with various encodings");
    assert!(
        result.is_ok(),
        "Committing multiple non-UTF-8 files should not fail, got: {:?}",
        result.err()
    );

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();
    assert!(
        stats.git_diff_added_lines >= 5,
        "Should count lines across all files, got: {}",
        stats.git_diff_added_lines
    );
}

// =============================================================================
// Non-UTF-8 file in subdirectory
// =============================================================================

#[test]
fn test_non_utf8_file_in_subdirectory() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let subdir = repo.path().join("src").join("legacy");
    fs::create_dir_all(&subdir).unwrap();
    let file_path = subdir.join("data.txt");
    fs::write(&file_path, gbk_multiline()).unwrap();

    let result = repo.stage_all_and_commit("Add non-UTF-8 file in subdirectory");
    assert!(
        result.is_ok(),
        "Committing non-UTF-8 file in subdirectory should not fail, got: {:?}",
        result.err()
    );
}

// =============================================================================
// Checkpoint: Non-UTF-8 files during checkpoint
// =============================================================================

#[test]
fn test_checkpoint_with_non_utf8_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_multiline()).unwrap();

    let result = repo.git_ai(&["checkpoint"]);
    assert!(
        result.is_ok(),
        "Checkpoint with non-UTF-8 file should not crash, got: {:?}",
        result.err()
    );
}

#[test]
fn test_checkpoint_ai_with_non_utf8_file_present() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let gbk_path = repo.path().join("legacy.txt");
    fs::write(&gbk_path, gbk_hello_world()).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git_ai(&["checkpoint"]).unwrap();

    let mut ai_file = repo.filename("generated.py");
    ai_file.set_contents(crate::lines!["print('hello')".ai(), "print('world')".ai()]);

    let commit = repo
        .stage_all_and_commit("Commit with AI file and GBK file")
        .unwrap();

    let ai_attestation = commit
        .authorship_log
        .attestations
        .iter()
        .find(|a| a.file_path == "generated.py");
    assert!(
        ai_attestation.is_some(),
        "AI file should still get proper attestation"
    );
}

// =============================================================================
// Binary files: Should be handled gracefully (related edge case)
// =============================================================================

#[test]
fn test_binary_file_does_not_crash_commit() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let binary_path = repo.path().join("image.png");
    let binary_content: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG header
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1 pixel
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE,
    ];
    fs::write(&binary_path, binary_content).unwrap();

    let result = repo.stage_all_and_commit("Add binary file");
    assert!(
        result.is_ok(),
        "Committing a binary file should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_binary_and_non_utf8_with_ai_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let binary_path = repo.path().join("data.bin");
    fs::write(&binary_path, vec![0x00, 0x01, 0x02, 0xFF, 0xFE]).unwrap();

    let gbk_path = repo.path().join("chinese.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    let mut ai_file = repo.filename("output.rs");
    ai_file.set_contents(crate::lines![
        "fn process() -> bool {".ai(),
        "    true".ai(),
        "}".ai(),
    ]);

    let commit = repo
        .stage_all_and_commit("Add binary, GBK, and AI files")
        .unwrap();

    let ai_attestation = commit
        .authorship_log
        .attestations
        .iter()
        .find(|a| a.file_path == "output.rs");
    assert!(
        ai_attestation.is_some(),
        "AI file should get attestation even with binary and non-UTF-8 neighbors"
    );

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();

    assert!(
        stats.ai_additions >= 3,
        "AI additions should include at least the 3 AI-attributed lines, got: {}",
        stats.ai_additions
    );
}

// =============================================================================
// Line-level attribution: Prove per-line AI/human attribution is correct
// with non-UTF-8 files present
// =============================================================================

#[test]
fn test_line_attribution_ai_file_with_gbk_neighbor() {
    let repo = TestRepo::new();
    let mut ai_file = repo.filename("code.py");

    ai_file.set_contents(crate::lines![
        "def greet():".ai(),
        "    return 'hello'".ai(),
        "# human comment",
    ]);

    let gbk_path = repo.path().join("chinese.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    repo.stage_all_and_commit("Add AI code and GBK file")
        .unwrap();

    ai_file.assert_lines_and_blame(crate::lines![
        "def greet():".ai(),
        "    return 'hello'".ai(),
        "# human comment".human(),
    ]);
}

#[test]
fn test_line_attribution_multi_commit_with_non_utf8_neighbor() {
    let repo = TestRepo::new();
    let mut file = repo.filename("app.ts");

    file.set_contents(crate::lines!["const x = 1;", "const y = 2;"]);

    let gbk_path = repo.path().join("legacy.txt");
    fs::write(&gbk_path, gbk_hello_world()).unwrap();

    repo.stage_all_and_commit("Base commit with GBK neighbor")
        .unwrap();

    file.insert_at(
        2,
        crate::lines![
            "const ai_z = compute();".ai(),
            "const ai_w = transform();".ai()
        ],
    );

    fs::write(&gbk_path, gbk_multiline()).unwrap();

    repo.stage_all_and_commit("AI additions alongside GBK edit")
        .unwrap();

    file.assert_lines_and_blame(crate::lines![
        "const x = 1;".human(),
        "const y = 2;".ai(),
        "const ai_z = compute();".ai(),
        "const ai_w = transform();".ai(),
    ]);
}

#[test]
fn test_line_attribution_interleaved_ai_human_with_non_utf8() {
    let repo = TestRepo::new();
    let mut file = repo.filename("mixed.rs");

    file.set_contents(crate::lines!["fn main() {"]);

    let latin1_path = repo.path().join("notes.txt");
    fs::write(&latin1_path, latin1_bytes()).unwrap();

    repo.stage_all_and_commit("Base commit").unwrap();

    file.insert_at(
        1,
        crate::lines![
            "    let a = ai_gen();".ai(),
            "    let b = human_wrote();".human(),
            "    let c = ai_gen_2();".ai(),
        ],
    );

    repo.stage_all_and_commit("Interleaved additions").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "fn main() {".ai(),
        "    let a = ai_gen();".ai(),
        "    let b = human_wrote();".ai(),
        "    let c = ai_gen_2();".ai(),
    ]);
}

#[test]
fn test_line_attribution_ai_replaces_lines_with_non_utf8_present() {
    let repo = TestRepo::new();
    let mut file = repo.filename("config.js");

    file.set_contents(crate::lines![
        "const a = 1;",
        "const b = 2;",
        "const c = 3;",
        "const d = 4;",
    ]);

    let sjis_path = repo.path().join("japanese.txt");
    fs::write(&sjis_path, shift_jis_bytes()).unwrap();

    repo.stage_all_and_commit("Initial commit").unwrap();

    file.replace_at(1, "const b = ai_replacement();".ai());
    file.replace_at(2, "const c = ai_replacement_2();".ai());

    repo.stage_all_and_commit("AI replaces middle lines")
        .unwrap();

    file.assert_lines_and_blame(crate::lines![
        "const a = 1;".human(),
        "const b = ai_replacement();".ai(),
        "const c = ai_replacement_2();".ai(),
        "const d = 4;".human(),
    ]);
}

#[test]
fn test_line_attribution_multiple_utf8_files_with_non_utf8_neighbors() {
    let repo = TestRepo::new();
    let mut file_a = repo.filename("module_a.py");
    let mut file_b = repo.filename("module_b.py");

    file_a.set_contents(crate::lines![
        "def func_a():".ai(),
        "    pass".ai(),
        "# end of a",
    ]);

    file_b.set_contents(crate::lines![
        "def func_b():".human(),
        "    return ai_result()".ai(),
    ]);

    let gbk_path = repo.path().join("data_gbk.txt");
    fs::write(&gbk_path, gbk_multiline()).unwrap();

    let latin1_path = repo.path().join("data_latin1.txt");
    fs::write(&latin1_path, latin1_bytes()).unwrap();

    repo.stage_all_and_commit("Add multiple files with non-UTF-8 neighbors")
        .unwrap();

    file_a.assert_lines_and_blame(crate::lines![
        "def func_a():".ai(),
        "    pass".ai(),
        "# end of a".human(),
    ]);

    file_b.assert_lines_and_blame(crate::lines![
        "def func_b():".human(),
        "    return ai_result()".ai(),
    ]);
}

#[test]
fn test_line_attribution_ai_across_multiple_commits_with_non_utf8() {
    let repo = TestRepo::new();
    let mut file = repo.filename("evolving.ts");

    file.set_contents(crate::lines!["const base = true;", ""]);

    let gbk_path = repo.path().join("persistent_gbk.txt");
    fs::write(&gbk_path, gbk_hello_world()).unwrap();

    repo.stage_all_and_commit("Base commit").unwrap();

    file.insert_at(
        1,
        crate::lines!["const first_ai = 1;".ai(), "const second_ai = 2;".ai()],
    );

    fs::write(&gbk_path, gbk_multiline()).unwrap();

    repo.stage_all_and_commit("First AI batch").unwrap();

    file.insert_at(
        3,
        crate::lines!["const third_ai = 3;".ai(), "const fourth_ai = 4;".ai()],
    );

    repo.stage_all_and_commit("Second AI batch").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "const base = true;".human(),
        "const first_ai = 1;".ai(),
        "const second_ai = 2;".ai(),
        "const third_ai = 3;".ai(),
        "const fourth_ai = 4;".ai(),
    ]);
}

// =============================================================================
// Edge case: File that starts as UTF-8 and becomes non-UTF-8
// =============================================================================

#[test]
fn test_file_changes_from_utf8_to_non_utf8() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("data.txt");
    fs::write(&file_path, "Hello UTF-8 world\n").unwrap();
    repo.stage_all_and_commit("Add UTF-8 file").unwrap();

    fs::write(&file_path, gbk_multiline()).unwrap();
    let result = repo.stage_all_and_commit("Replace with GBK content");
    assert!(
        result.is_ok(),
        "Changing a file from UTF-8 to non-UTF-8 should not fail, got: {:?}",
        result.err()
    );
}

#[test]
fn test_file_changes_from_non_utf8_to_utf8() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("data.txt");
    fs::write(&file_path, gbk_multiline()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    fs::write(&file_path, "Now this is UTF-8\n").unwrap();
    let result = repo.stage_all_and_commit("Replace with UTF-8 content");
    assert!(
        result.is_ok(),
        "Changing a file from non-UTF-8 to UTF-8 should not fail, got: {:?}",
        result.err()
    );
}

// =============================================================================
// Stats on non-UTF-8 file edits
// =============================================================================

#[test]
fn test_stats_after_editing_non_utf8_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("gbk_file.txt");
    fs::write(&file_path, gbk_hello_world()).unwrap();
    repo.stage_all_and_commit("Add GBK file").unwrap();

    fs::write(&file_path, gbk_multiline()).unwrap();
    repo.stage_all_and_commit("Edit GBK file").unwrap();

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let result: Result<CommitStats, _> = serde_json::from_str(&json);
    assert!(
        result.is_ok(),
        "Stats should produce valid JSON after editing non-UTF-8 files"
    );
}

// =============================================================================
// Large non-UTF-8 file
// =============================================================================

#[test]
fn test_large_non_utf8_file() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("large_gbk.txt");
    let mut content = Vec::new();
    for i in 0..500 {
        // Each line: GBK bytes + line number + newline
        content.extend_from_slice(&[0xC4, 0xE3, 0xBA, 0xC3]);
        content.extend_from_slice(format!(" line {}", i).as_bytes());
        content.push(b'\n');
    }
    fs::write(&file_path, content).unwrap();

    let result = repo.stage_all_and_commit("Add large GBK file");
    assert!(
        result.is_ok(),
        "Committing a large non-UTF-8 file should not fail, got: {:?}",
        result.err()
    );

    let raw = repo.git_ai(&["stats", "--json"]).unwrap();
    let json = extract_json_object(&raw);
    let stats: CommitStats = serde_json::from_str(&json).unwrap();
    assert!(
        stats.git_diff_added_lines >= 500,
        "Should count all 500 lines, got: {}",
        stats.git_diff_added_lines
    );
}

// =============================================================================
// Non-UTF-8 content with null bytes (edge case between binary and text)
// =============================================================================

#[test]
fn test_file_with_null_bytes_in_content() {
    let repo = TestRepo::new();

    let mut readme = repo.filename("README.md");
    readme.set_contents(crate::lines!["# Project"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("nulls.dat");
    let content: Vec<u8> = vec![
        b'h', b'e', b'l', b'l', b'o', 0x00, b'w', b'o', b'r', b'l', b'd', b'\n',
    ];
    fs::write(&file_path, content).unwrap();

    let result = repo.stage_all_and_commit("Add file with null bytes");
    assert!(
        result.is_ok(),
        "Committing a file with null bytes should not fail, got: {:?}",
        result.err()
    );
}

crate::reuse_tests_in_worktree!(
    test_commit_gbk_encoded_file_succeeds,
    test_commit_latin1_encoded_file_succeeds,
    test_commit_shift_jis_encoded_file_succeeds,
    test_commit_mixed_valid_invalid_utf8_file_succeeds,
    test_stats_with_non_utf8_file_only,
    test_stats_with_non_utf8_and_utf8_files_mixed,
    test_stats_json_output_valid_with_non_utf8_file,
    test_blame_non_utf8_file_does_not_crash,
    test_blame_utf8_file_unaffected_by_non_utf8_neighbor,
    test_edit_non_utf8_file_second_commit,
    test_delete_non_utf8_file,
    test_ai_attribution_preserved_with_non_utf8_in_same_commit,
    test_human_and_ai_edits_with_non_utf8_file_present,
    test_multiple_non_utf8_encodings_in_one_commit,
    test_non_utf8_file_in_subdirectory,
    test_checkpoint_with_non_utf8_file,
    test_checkpoint_ai_with_non_utf8_file_present,
    test_binary_file_does_not_crash_commit,
    test_binary_and_non_utf8_with_ai_file,
    test_line_attribution_ai_file_with_gbk_neighbor,
    test_line_attribution_multi_commit_with_non_utf8_neighbor,
    test_line_attribution_interleaved_ai_human_with_non_utf8,
    test_line_attribution_ai_replaces_lines_with_non_utf8_present,
    test_line_attribution_multiple_utf8_files_with_non_utf8_neighbors,
    test_line_attribution_ai_across_multiple_commits_with_non_utf8,
    test_file_changes_from_utf8_to_non_utf8,
    test_file_changes_from_non_utf8_to_utf8,
    test_stats_after_editing_non_utf8_file,
    test_large_non_utf8_file,
    test_file_with_null_bytes_in_content,
);
