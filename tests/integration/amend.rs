use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::collections::HashMap;

/// Test amending a commit by adding AI-authored lines at the top of the file.
#[test]
fn test_amend_add_lines_at_top() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Initial file with human content
    file.set_contents(crate::lines![
        "line 1", "line 2", "line 3", "line 4", "line 5"
    ]);

    repo.git(&["add", "-A"]).unwrap();

    repo.commit("Initial commit").unwrap();

    // AI adds lines at the top
    file.insert_at(
        0,
        crate::lines!["// AI added line 1".ai(), "// AI added line 2".ai()],
    );

    // Amend the commit WITHOUT staging the AI lines
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Now stage and commit the AI lines
    repo.stage_all_and_commit("Add AI lines").unwrap();

    // Verify AI authorship is preserved after the second commit
    file.assert_lines_and_blame(crate::lines![
        "// AI added line 1".ai(),
        "// AI added line 2".ai(),
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "line 4".human(),
        "line 5".human()
    ]);
}

#[test]
fn test_amend_add_lines_in_middle() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Initial file with human content
    file.set_contents(crate::lines![
        "line 1", "line 2", "line 3", "line 4", "line 5"
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds lines in the middle
    file.insert_at(
        2,
        crate::lines!["// AI inserted line 1".ai(), "// AI inserted line 2".ai()],
    );

    // Amend the commit
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Verify AI authorship is preserved
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "line 2".human(),
        "// AI inserted line 1".ai(),
        "// AI inserted line 2".ai(),
        "line 3".human(),
        "line 4".human(),
        "line 5".human()
    ]);
}

#[test]
fn test_amend_add_lines_at_bottom() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Initial file with human content
    file.set_contents(crate::lines![
        "line 1", "line 2", "line 3", "line 4", "line 5"
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds lines at the bottom
    file.insert_at(
        5,
        crate::lines!["// AI appended line 1".ai(), "// AI appended line 2".ai()],
    );

    // Amend the commit
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Verify AI authorship is preserved
    file.assert_lines_and_blame(crate::lines![
        "line 1".human(),
        "line 2".human(),
        "line 3".human(),
        "line 4".human(),
        "line 5".ai(),
        "// AI appended line 1".ai(),
        "// AI appended line 2".ai()
    ]);
}

#[test]
fn test_amend_multiple_changes() {
    let repo = TestRepo::new();
    let mut file = repo.filename("code.js");

    // Initial file with AI content
    file.set_contents(crate::lines![
        "function example() {".ai(),
        "  return 42;".ai(),
        "}".ai()
    ]);
    repo.stage_all_and_commit("Add example function").unwrap();

    // AI adds header comment
    file.insert_at(0, crate::lines!["// Header comment".ai()]);
    // After inserting at 0, the file now has 4 lines

    // AI adds documentation in middle (after line 2: "function example() {")
    file.insert_at(2, crate::lines!["  // Added documentation".ai()]);
    // After inserting at 2, the file now has 5 lines

    // AI adds footer at bottom (at the end after "}")
    file.insert_at(5, crate::lines!["// Footer".ai()]);

    // Amend the commit
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Add example function (amended)"])
        .unwrap();

    // Verify all AI authorship is preserved
    file.assert_lines_and_blame(crate::lines![
        "// Header comment".ai(),
        "function example() {".ai(),
        "  // Added documentation".ai(),
        "  return 42;".ai(),
        "}".ai(),
        "// Footer".ai()
    ]);
}

#[test]
fn test_amend_with_unstaged_ai_code_in_other_file() {
    let repo = TestRepo::new();

    // Create initial commit with fileA
    let mut file_a = repo.filename("fileA.txt");
    file_a.set_contents(crate::lines!["fileA line 1", "fileA line 2"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Create fileB with AI code but DON'T stage it yet
    let mut file_b = repo.filename("fileB.txt");
    file_b.set_contents_no_stage(crate::lines![
        "// AI code in fileB".ai(),
        "function foo() {".ai(),
        "  return 'bar';".ai(),
        "}".ai()
    ]);

    // Modify fileA and amend the previous commit (fileB stays unstaged in working tree)
    file_a.insert_at(2, crate::lines!["fileA line 3"]);
    repo.git(&["add", "fileA.txt"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Now stage and commit fileB in a new commit
    repo.stage_all_and_commit("Add fileB").unwrap();

    // Verify fileB has AI authorship
    file_b.assert_lines_and_blame(crate::lines![
        "// AI code in fileB".ai(),
        "function foo() {".ai(),
        "  return 'bar';".ai(),
        "}".ai()
    ]);
}

/// Test that unstaged AI code in the tree is attributed after amending HEAD with a different file

#[test]
fn test_amend_preserves_unstaged_ai_attribution() {
    let repo = TestRepo::new();

    // Create initial commit with fileA
    let mut file_a = repo.filename("fileA.txt");
    file_a.set_contents(crate::lines!["original content"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Stage changes to fileA
    file_a.insert_at(1, crate::lines!["staged addition"]);
    repo.git(&["add", "fileA.txt"]).unwrap();

    // Create fileB with unstaged AI code
    let mut file_b = repo.filename("fileB.txt");
    file_b.set_contents_no_stage(crate::lines![
        "// Unstaged AI line 1".ai(),
        "// Unstaged AI line 2".ai(),
        "// Unstaged AI line 3".ai()
    ]);

    // Amend HEAD with fileA (fileB remains unstaged)
    repo.git(&["commit", "--amend", "-m", "Amended commit"])
        .unwrap();

    // Verify that fileB's AI attribution was saved in INITIAL attributions
    let initial = repo.current_working_logs().read_initial_attributions();
    assert!(
        initial.files.contains_key("fileB.txt"),
        "fileB.txt should be in initial attributions"
    );
    let file_b_attrs = &initial.files["fileB.txt"];
    assert_eq!(
        file_b_attrs.len(),
        1,
        "fileB should have 1 attribution range"
    );
    assert_eq!(file_b_attrs[0].start_line, 1);
    assert_eq!(file_b_attrs[0].end_line, 3);

    // Now stage and commit fileB
    repo.stage_all_and_commit("Add fileB").unwrap();

    // Verify fileB retains AI authorship
    file_b.assert_lines_and_blame(crate::lines![
        "// Unstaged AI line 1".ai(),
        "// Unstaged AI line 2".ai(),
        "// Unstaged AI line 3".ai()
    ]);
}

/// Test amending with multiple files where some have unstaged AI changes

#[test]
fn test_amend_with_multiple_files_mixed_staging() {
    let repo = TestRepo::new();

    // Initial commit
    let mut file1 = repo.filename("file1.txt");
    file1.set_contents(crate::lines!["file1 original"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Stage changes to file1
    file1.insert_at(1, crate::lines!["file1 staged"]);
    repo.git(&["add", "file1.txt"]).unwrap();

    // Create file2 with AI code (unstaged)
    let mut file2 = repo.filename("file2.txt");
    file2.set_contents_no_stage(crate::lines![
        "// AI file2 line 1".ai(),
        "// AI file2 line 2".ai()
    ]);

    // Create file3 with mixed AI and human code (unstaged)
    let mut file3 = repo.filename("file3.txt");
    file3.set_contents_no_stage(crate::lines![
        "human line".human(),
        "// AI file3 line".ai(),
        "another human line".human()
    ]);

    // Amend with file1
    repo.git(&["commit", "--amend", "-m", "Amended with file1"])
        .unwrap();

    // Stage and commit file2 and file3
    repo.stage_all_and_commit("Add file2 and file3").unwrap();

    // Verify AI authorship is preserved
    file2.assert_lines_and_blame(crate::lines![
        "// AI file2 line 1".ai(),
        "// AI file2 line 2".ai()
    ]);

    file3.assert_lines_and_blame(crate::lines![
        "human line".human(),
        "// AI file3 line".ai(),
        "another human line".human()
    ]);
}

/// Test amending with a partially staged AI file
/// Stage the first half, leave the second half unstaged
#[test]
fn test_amend_with_partially_staged_ai_file() {
    let repo = TestRepo::new();

    // Create initial commit with two lines: the first will stay human throughout,
    // the second (last line) will be pulled into the AI hunk due to the trailing-newline
    // boundary effect and correctly attributed to AI.
    let mut file = repo.filename("code.txt");
    file.set_contents(crate::lines!["// Initial line", "// Human end"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds 6 lines after "// Human end" (the last line)
    file.insert_at(
        2,
        crate::lines![
            "// AI line 1".ai(),
            "// AI line 2".ai(),
            "// AI line 3".ai(),
            "// AI line 4".ai(),
            "// AI line 5".ai(),
            "// AI line 6".ai()
        ],
    );

    // Stage only the first 3 AI lines (using git add with patch would normally do this,
    // but we'll simulate by creating a version with only first 3 lines and staging that)
    let workdir = repo.path();
    let file_path = workdir.join("code.txt");

    // Write partial content (original lines + first 3 AI lines only)
    std::fs::write(
        &file_path,
        "// Initial line\n// Human end\n// AI line 1\n// AI line 2\n// AI line 3\n",
    )
    .unwrap();
    repo.git(&["add", "code.txt"]).unwrap();

    // Restore full content with all 6 AI lines
    std::fs::write(
        &file_path,
        "// Initial line\n// Human end\n// AI line 1\n// AI line 2\n// AI line 3\n// AI line 4\n// AI line 5\n// AI line 6\n"
    ).unwrap();

    // Amend the commit (only first 3 AI lines are staged)
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Now commit the remaining unstaged lines
    repo.stage_all_and_commit("Add remaining AI lines").unwrap();

    // "// Initial line" stays human — it's not in the same hunk as any AI insertion.
    // "// Human end" becomes AI — it was the last line in the original file, so the
    // diff places it in the same 1→N hunk as the AI additions (force_split applies).
    file.assert_lines_and_blame(crate::lines![
        "// Initial line".human(),
        "// Human end".ai(),
        "// AI line 1".ai(),
        "// AI line 2".ai(),
        "// AI line 3".ai(),
        "// AI line 4".ai(),
        "// AI line 5".ai(),
        "// AI line 6".ai(),
    ]);
}

/// Test amending with partially staged mixed AI/human file
#[test]
fn test_amend_with_partially_staged_mixed_content() {
    let repo = TestRepo::new();

    // Create initial file with human content
    let mut file = repo.filename("mixed.txt");
    file.set_contents(crate::lines!["human line 1", "human line 2", "human end"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Stage only the first AI line and first human addition
    let workdir = repo.path();
    let file_path = workdir.join("mixed.txt");
    // add the line
    std::fs::write(
        &file_path,
        "human line 1\nhuman line 2\n// AI addition 1\nhuman end\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    repo.git(&["add", "mixed.txt"]).unwrap();

    std::fs::write(
        &file_path,
        "human line 1\nhuman line 2\n// AI addition 1\n// AI addition 2\nhuman end\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Commit remaining unstaged content
    repo.stage_all_and_commit("Add remaining content").unwrap();

    // Verify all attributions preserved
    file.assert_lines_and_blame(crate::lines![
        "human line 1".human(),
        "human line 2".human(),
        "// AI addition 1".ai(),
        "// AI addition 2".ai(),
        "human end".ai(),
    ]);
}

/// Test amending where middle section of AI file is unstaged
#[test]
fn test_amend_with_unstaged_middle_section() {
    let repo = TestRepo::new();

    // Initial commit with two lines: "// File header" stays human throughout;
    // "// File footer" (last line) gets pulled into the AI hunk and becomes AI.
    let mut file = repo.filename("function.txt");
    file.set_contents(crate::lines!["// File header", "// File footer"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds multiple sections after "// File footer" (the last line)
    file.insert_at(
        2,
        crate::lines![
            "// AI section 1 line 1".ai(),
            "// AI section 1 line 2".ai(),
            "// AI section 2 line 1".ai(),
            "// AI section 2 line 2".ai(),
            "// AI section 3 line 1".ai(),
            "// AI section 3 line 2".ai()
        ],
    );

    // Stage only sections 1 and 3 (leave section 2 unstaged)
    let workdir = repo.path();
    let file_path = workdir.join("function.txt");
    std::fs::write(
        &file_path,
        "// File header\n// File footer\n// AI section 1 line 1\n// AI section 1 line 2\n// AI section 3 line 1\n// AI section 3 line 2"
    ).unwrap();
    repo.git(&["add", "function.txt"]).unwrap();

    // Restore full content with middle section
    std::fs::write(
        &file_path,
        "// File header\n// File footer\n// AI section 1 line 1\n// AI section 1 line 2\n// AI section 2 line 1\n// AI section 2 line 2\n// AI section 3 line 1\n// AI section 3 line 2"
    ).unwrap();

    // Amend
    repo.git(&["commit", "--amend", "-m", "Initial commit (amended)"])
        .unwrap();

    // Commit remaining (middle section)
    repo.stage_all_and_commit("Add middle section").unwrap();

    // "// File header" stays human — not adjacent to any AI hunk boundary.
    // "// File footer" becomes AI — it was the last line, so the diff places it in
    // the same 1→N hunk as the AI additions (force_split applies).
    file.assert_lines_and_blame(crate::lines![
        "// File header".human(),
        "// File footer".ai(),
        "// AI section 1 line 1".ai(),
        "// AI section 1 line 2".ai(),
        "// AI section 2 line 1".ai(),
        "// AI section 2 line 2".ai(),
        "// AI section 3 line 1".ai(),
        "// AI section 3 line 2".ai(),
    ]);
}

#[test]
fn test_amend_repeated_round_trips_preserve_exact_line_authorship() {
    let repo = TestRepo::new();
    let mut file = repo.filename("code.js");

    file.set_contents(crate::lines![
        "function example() {".ai(),
        "  return 42;".ai(),
        "}".ai()
    ]);
    repo.stage_all_and_commit("Add example function").unwrap();

    file.insert_at(0, crate::lines!["// Header comment".ai()]);
    file.insert_at(2, crate::lines!["  // Added documentation".ai()]);
    file.insert_at(5, crate::lines!["// Footer".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Add example function (amended 1)",
    ])
    .unwrap();

    // Re-amend the same commit with mixed authorship changes.
    file.insert_at(0, crate::lines!["// Human TODO".human()]);
    file.insert_at(7, crate::lines!["// AI trailing note".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Add example function (amended 2)",
    ])
    .unwrap();

    file.assert_lines_and_blame(crate::lines![
        "// Human TODO".human(),
        "// Header comment".ai(),
        "function example() {".ai(),
        "  // Added documentation".ai(),
        "  return 42;".ai(),
        "}".ai(),
        "// Footer".ai(),
        "// AI trailing note".ai()
    ]);
}

/// Test that custom attributes set via config are preserved through an amend
/// when the real post-commit pipeline injects them.
#[test]
fn test_amend_preserves_custom_attributes_from_config() {
    let mut repo = TestRepo::new_dedicated_daemon();

    // Configure custom attributes via config patch
    let mut attrs = HashMap::new();
    attrs.insert("employee_id".to_string(), "E202".to_string());
    attrs.insert("team".to_string(), "security".to_string());
    repo.patch_git_ai_config(|patch| {
        patch.custom_attributes = Some(attrs.clone());
    });

    // Create initial commit with AI content
    let mut file = repo.filename("code.txt");
    file.set_contents(crate::lines![
        "// AI generated code".ai(),
        "function init() {}".ai()
    ]);
    repo.stage_all_and_commit("Initial AI commit").unwrap();

    // Verify custom attributes were set on the original commit
    let original_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let original_log = repo.require_authorship_log(&original_sha);
    assert!(
        original_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !original_log.metadata.sessions.is_empty(),
        "precondition: original commit should have session records"
    );
    for session in original_log.metadata.sessions.values() {
        assert_eq!(
            session.custom_attributes.as_ref(),
            Some(&attrs),
            "precondition: original commit should have custom_attributes from config (sessions)"
        );
    }

    // Amend the commit with additional AI lines
    file.insert_at(2, crate::lines!["// More AI code".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Initial AI commit (amended)"])
        .unwrap();

    // Verify custom attributes survived the amend
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let amended_log = repo.require_authorship_log(&amended_sha);
    assert!(
        amended_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !amended_log.metadata.sessions.is_empty(),
        "amended commit should have session records"
    );
    for session in amended_log.metadata.sessions.values() {
        assert_eq!(
            session.custom_attributes.as_ref(),
            Some(&attrs),
            "custom_attributes should be preserved through amend (sessions)"
        );
    }

    // Also verify the AI attribution itself survived
    file.assert_lines_and_blame(crate::lines![
        "// AI generated code".ai(),
        "function init() {}".ai(),
        "// More AI code".ai()
    ]);
}

/// Bug regression: amend a commit and delete the AI-authored line.
/// The amended note should NOT contain a prompt record for the deleted AI line.
///
/// Before the fix, `to_authorship_log_and_initial_working_log` copied ALL prompts from
/// VirtualAttributions upfront without pruning them to only those referenced by
/// actual attestations.  When an AI line was deleted in the amend the attestation
/// was correctly absent, but the orphaned PromptRecord remained in the metadata.
#[test]
fn test_amend_delete_ai_line_removes_prompt_from_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create a commit that contains both human and AI lines.
    file.set_contents(crate::lines![
        "human line 1",
        "// AI authored line".ai(),
        "human line 2"
    ]);
    repo.stage_all_and_commit("Initial commit with AI line")
        .unwrap();

    let original_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let original_log = repo.require_authorship_log(&original_sha);
    assert!(
        original_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !original_log.metadata.sessions.is_empty(),
        "precondition: original commit should have session records"
    );

    // Amend: overwrite the file with only human content, deleting the AI line.
    let file_path = repo.path().join("test.txt");
    std::fs::write(&file_path, "human line 1\nhuman line 2\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Amended - AI line deleted"])
        .unwrap();

    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let amended_log = repo.require_authorship_log(&amended_sha);

    assert!(
        amended_log.metadata.prompts.is_empty(),
        "amended note should have no prompts since the only AI line was deleted, \
         but found orphaned prompts: {:?}",
        amended_log.metadata.prompts.keys().collect::<Vec<_>>()
    );
    assert!(
        amended_log.metadata.sessions.is_empty(),
        "amended note should have no sessions since the only AI line was deleted, \
         but found orphaned sessions: {:?}",
        amended_log.metadata.sessions.keys().collect::<Vec<_>>()
    );
}

/// Bug regression (worse variant): amend a commit and delete an AI-authored line that
/// was originally introduced by an *earlier* commit.
///
/// When the blame on the pre-amend commit surfaces prompt IDs from older commits,
/// those foreign PromptRecords must NOT appear in the amended commit's note.
/// Before the fix the note for the amended commit contained the earlier commit's
/// PromptRecord even though it had no corresponding attestation.
#[test]
fn test_amend_delete_prior_commit_ai_line_no_foreign_prompt_in_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Commit A: introduces an AI line (prompt P1) and a human line.
    file.set_contents(crate::lines![
        "// AI authored line from commit A".ai(),
        "human line from commit A"
    ]);
    repo.stage_all_and_commit("Commit A with AI line").unwrap();

    let commit_a_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let commit_a_log = repo.require_authorship_log(&commit_a_sha);
    assert!(
        commit_a_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    let commit_a_session_ids: Vec<String> =
        commit_a_log.metadata.sessions.keys().cloned().collect();
    assert!(
        !commit_a_session_ids.is_empty(),
        "precondition: commit A should have session records"
    );

    // Commit B: a human-only addition on top of A.
    // We write directly to avoid creating AI checkpoints for B.
    let file_path = repo.path().join("test.txt");
    std::fs::write(
        &file_path,
        "// AI authored line from commit A\nhuman line from commit A\nhuman line from commit B\n",
    )
    .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "Commit B - human addition"])
        .unwrap();

    // Amend commit B: delete the AI line that came from commit A.
    // After the amend, the file contains only human lines.
    std::fs::write(
        &file_path,
        "human line from commit A\nhuman line from commit B\n",
    )
    .unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Commit B amended - also deleted AI from A",
    ])
    .unwrap();

    let amended_b_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let amended_b_log = repo.require_authorship_log(&amended_b_sha);

    // The amended B note must NOT contain any of commit A's session IDs.
    // They are foreign to commit B and have no corresponding attestation.
    assert!(
        amended_b_log.metadata.prompts.is_empty(),
        "amended B should have no prompts"
    );
    for session_id in &commit_a_session_ids {
        assert!(
            !amended_b_log.metadata.sessions.contains_key(session_id),
            "Amended B's note should not contain session '{}' from commit A \
             (foreign-session-leak bug): amended_b sessions = {:?}",
            session_id,
            amended_b_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }
}

/// Amending a commit and deleting a KnownHuman-attributed line must preserve the
/// HumanRecord in the note's `metadata.humans`.
///
/// The note is a historical record of every contributor that touched the commit.
/// Deleting the attributed line removes the *attribution* (line coordinates), but
/// the HumanRecord itself must remain — matching how PromptRecords are preserved
/// via `checkpoint_prompt_ids` even when all attributed AI lines are deleted.
#[test]
fn test_amend_delete_known_human_line_preserves_human_record_in_note() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");

    // Create a commit that contains a mix of human-attributed and plain human lines.
    // Using `.human()` triggers a `checkpoint mock_known_human` which stores an
    // h_-prefixed HumanRecord in the note's metadata.humans.
    file.set_contents(crate::lines![
        "regular human line",
        "// KnownHuman attested line".human(),
        "another regular line"
    ]);
    repo.stage_all_and_commit("Initial commit with KnownHuman line")
        .unwrap();

    let original_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let original_log = repo.require_authorship_log(&original_sha);
    assert!(
        !original_log.metadata.humans.is_empty(),
        "precondition: original commit should have HumanRecord entries"
    );
    let original_human_ids: Vec<String> = original_log.metadata.humans.keys().cloned().collect();

    // Amend: overwrite the file with plain human content only, deleting the KnownHuman line.
    let file_path = repo.path().join("test.txt");
    std::fs::write(&file_path, "regular human line\nanother regular line\n").unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Amended - KnownHuman line deleted",
    ])
    .unwrap();

    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let amended_log = repo.require_authorship_log(&amended_sha);

    // The HumanRecord must survive the amend even though its attributed line was deleted.
    // The note is a commit-level record of contributors; removing a line doesn't erase
    // the contributor's association with the commit.
    assert!(
        !amended_log.metadata.humans.is_empty(),
        "amended note should still contain the HumanRecord(s) from the original commit \
         even though the KnownHuman line was deleted; got: {:?}",
        amended_log.metadata.humans.keys().collect::<Vec<_>>()
    );
    for id in &original_human_ids {
        assert!(
            amended_log.metadata.humans.contains_key(id),
            "HumanRecord '{}' present in original note must be preserved after amend; \
             amended note has: {:?}",
            id,
            amended_log.metadata.humans.keys().collect::<Vec<_>>()
        );
    }
}

crate::reuse_tests_in_worktree!(
    test_amend_add_lines_at_top,
    test_amend_add_lines_in_middle,
    test_amend_add_lines_at_bottom,
    test_amend_multiple_changes,
    test_amend_with_unstaged_ai_code_in_other_file,
    test_amend_preserves_unstaged_ai_attribution,
    test_amend_with_multiple_files_mixed_staging,
    test_amend_with_partially_staged_ai_file,
    test_amend_with_partially_staged_mixed_content,
    test_amend_with_unstaged_middle_section,
    test_amend_repeated_round_trips_preserve_exact_line_authorship,
    test_amend_delete_ai_line_removes_prompt_from_note,
    test_amend_delete_prior_commit_ai_line_no_foreign_prompt_in_note,
    test_amend_delete_known_human_line_preserves_human_record_in_note,
);
