use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::model::authorship_log_serialization::AuthorshipLog;
use std::fs;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the authorship note for HEAD, parse it, and return the full log.
fn head_authorship_log(repo: &TestRepo) -> AuthorshipLog {
    let sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    repo.require_authorship_log(&sha)
}

// ===========================================================================
// Normal commit path — `from_just_working_log` + `to_authorship_log_and_initial_working_log`
// ===========================================================================

/// Core regression test matching the user report:
///
/// 1. Commit A: AI writes code → prompt appears in note (correct)
/// 2. Commit B: 100% human on a different file → stale AI prompt should NOT appear
/// 3. Commit C: 100% human on yet another file → still should NOT appear
///
/// Commit B and C use `fs::write` + `repo.git(["add"/"commit"])` (through the
/// wrapper, so post-commit hooks fire) to faithfully reproduce a human-only
/// commit with zero AI checkpoint involvement.  Earlier versions of this test
/// used `set_contents` which always creates AI checkpoints even for all-human
/// content — that made the test exercise a different (checkpoint-carrying) path
/// rather than the actual user-reported scenario.
#[test]
fn test_stale_prompt_not_carried_to_subsequent_human_commits() {
    let repo = TestRepo::new();

    // Base commit (via git_og — no hooks, clean slate)
    repo.commit_untracked_file("base.txt", "Base content", "Initial commit");

    // Commit A: AI writes code — creates prompts in the note
    let mut ai_file = repo.filename("pi.md");
    ai_file.set_contents(crate::lines![
        "AI line 1".ai(),
        "AI line 2".ai(),
        "AI line 3".ai(),
    ]);
    let ai_commit = repo.stage_all_and_commit("AI commit").unwrap();
    assert!(
        ai_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !ai_commit.authorship_log.metadata.sessions.is_empty(),
        "precondition: AI commit must have session records"
    );
    let ai_session_ids: Vec<String> = ai_commit
        .authorship_log
        .metadata
        .sessions
        .keys()
        .cloned()
        .collect();

    // Commit B: truly human-only on a different file — no set_contents, no AI checkpoints
    let human_path = repo.path().join("canada.md");
    fs::write(
        &human_path,
        "O Canada!\nOur home and native land!\nTrue patriot love in all of us command.\n",
    )
    .unwrap();
    repo.git(&["add", "canada.md"]).unwrap();
    repo.git(&["commit", "-m", "Human-only commit"]).unwrap();
    let human_b_log = head_authorship_log(&repo);

    assert!(
        human_b_log.metadata.prompts.is_empty(),
        "human-only commit B should have no prompts"
    );
    for id in &ai_session_ids {
        assert!(
            !human_b_log.metadata.sessions.contains_key(id),
            "Stale AI session '{}' should NOT appear in human-only commit B.\n\
             Sessions found: {:?}",
            id,
            human_b_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }

    // Commit C: another human-only commit — stale session must still not appear
    let human_path2 = repo.path().join("new-file.md");
    fs::write(&human_path2, "Hello safety\n").unwrap();
    repo.git(&["add", "new-file.md"]).unwrap();
    repo.git(&["commit", "-m", "Another human-only commit"])
        .unwrap();
    let human_c_log = head_authorship_log(&repo);

    assert!(
        human_c_log.metadata.prompts.is_empty(),
        "human-only commit C should have no prompts"
    );
    for id in &ai_session_ids {
        assert!(
            !human_c_log.metadata.sessions.contains_key(id),
            "Stale AI session '{}' should NOT appear in human-only commit C.\n\
             Sessions found: {:?}",
            id,
            human_c_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }
}

/// Complementary test: prompts ARE correctly included when AI lines are committed.
#[test]
fn test_prompt_present_when_ai_lines_committed() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("base.txt", "Base content", "Initial commit");

    let mut ai_file = repo.filename("code.rs");
    ai_file.set_contents(crate::lines![
        "fn hello() {".ai(),
        "    println!(\"hello\");".ai(),
        "}".ai(),
    ]);
    let ai_commit = repo.stage_all_and_commit("AI adds code").unwrap();

    assert!(
        ai_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !ai_commit.authorship_log.metadata.sessions.is_empty(),
        "AI commit should have session records when AI lines are committed"
    );
}

/// Test that unstaged AI lines carry their prompt to INITIAL but don't pollute
/// the committed note of a human-only commit.
#[test]
fn test_unstaged_ai_lines_prompt_not_in_human_commit_note() {
    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["Base content", ""]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds lines to base.txt
    base_file.insert_at(1, crate::lines!["AI line".ai(), "AI line 2".ai()]);

    // Stage only the AI additions
    base_file.stage();

    // Human adds more unstaged content
    base_file.insert_at(3, crate::lines!["unstaged ai".ai()]);

    // Commit only the staged AI lines
    let first_commit = repo.commit("Commit with AI lines").unwrap();
    assert!(
        first_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    let ai_session_ids: Vec<String> = first_commit
        .authorship_log
        .metadata
        .sessions
        .keys()
        .cloned()
        .collect();
    assert!(
        !ai_session_ids.is_empty(),
        "First commit should have session records"
    );

    // Create a human-only file WITHOUT using set_contents (which stages everything
    // via `git add -A`).  Write directly and stage only it to keep base.txt's
    // unstaged AI changes out of this commit.
    let human_file_path = repo.path().join("human.txt");
    fs::write(&human_file_path, "Pure human content\n").unwrap();
    repo.git(&["add", "human.txt"]).unwrap();
    let human_commit = repo
        .commit("Human-only commit while unstaged AI exists")
        .unwrap();

    assert!(
        human_commit.authorship_log.metadata.prompts.is_empty(),
        "human-only commit should have no prompts"
    );
    for id in &ai_session_ids {
        assert!(
            !human_commit
                .authorship_log
                .metadata
                .sessions
                .contains_key(id),
            "AI session '{}' from unstaged lines should NOT appear in human-only commit note.\n\
             Sessions: {:?}",
            id,
            human_commit
                .authorship_log
                .metadata
                .sessions
                .keys()
                .collect::<Vec<_>>()
        );
    }
}

// ===========================================================================
// Amend path — `from_working_log_for_commit` + merge + `to_authorship_log_and_initial_working_log`
// ===========================================================================

/// Regression: INITIAL-only prompts must not leak through the amend merge path.
///
/// The amend path builds `checkpoint_prompt_ids` from `checkpoint_va.prompts`.
/// Before the fix, INITIAL-only prompts were included, making them survive the
/// merge-path retain filter.
///
/// Scenario:
/// 1. Commit A: AI lines committed + some AI lines left unstaged (→ INITIAL)
/// 2. Commit B: human-only on a separate file (INITIAL carries the stale prompt)
/// 3. Amend B with more human content → stale prompt must NOT appear in note
#[test]
fn test_amend_stale_initial_prompt_not_in_amended_human_commit() {
    let repo = TestRepo::new();

    let mut base_file = repo.filename("base.txt");
    base_file.set_contents(crate::lines!["Base content", ""]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // AI adds lines — some will be committed, some left unstaged for INITIAL
    base_file.insert_at(1, crate::lines!["AI line 1".ai(), "AI line 2".ai()]);
    base_file.stage();
    base_file.insert_at(3, crate::lines!["unstaged AI".ai()]);

    let ai_commit = repo.commit("Commit A with AI lines").unwrap();
    assert!(
        ai_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    let ai_session_ids: Vec<String> = ai_commit
        .authorship_log
        .metadata
        .sessions
        .keys()
        .cloned()
        .collect();
    assert!(
        !ai_session_ids.is_empty(),
        "precondition: AI commit must have session records"
    );

    // Commit B: human-only, using fs::write to avoid touching base.txt
    let human_path = repo.path().join("human_notes.txt");
    fs::write(&human_path, "Human note line 1\n").unwrap();
    repo.git(&["add", "human_notes.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit B - human only"])
        .unwrap();

    // Amend commit B — triggers from_working_log_for_commit merge path
    fs::write(&human_path, "Human note line 1\nHuman note line 2\n").unwrap();
    repo.git(&["add", "human_notes.txt"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Commit B amended"])
        .unwrap();

    let amended_log = head_authorship_log(&repo);

    assert!(
        amended_log.metadata.prompts.is_empty(),
        "amended human-only commit should have no prompts"
    );
    for session_id in &ai_session_ids {
        assert!(
            !amended_log.metadata.sessions.contains_key(session_id),
            "INITIAL-only AI session '{}' should NOT appear in amended human-only commit.\n\
             Sessions in amended commit: {:?}",
            session_id,
            amended_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }
}

/// Regression: blame-sourced prompts from a prior commit must not leak into an
/// amended human-only commit.
///
/// When amending, the blame VA picks up all lines in the repo's history.
/// Prompts from an earlier commit's AI lines appear in `blame_va.prompts`.
/// Before the fix, these survived the merge because `referenced_in_merged`
/// included them (the AI lines exist in the tree), but they have no committed
/// lines in the *current* commit's diff and should be excluded.
///
/// Scenario:
/// 1. Commit A: AI writes code (committed — AI lines are in the tree)
/// 2. Commit B: human-only on a different file
/// 3. Amend B with more human content → A's prompt must NOT appear in B's note
#[test]
fn test_amend_stale_blame_prompt_not_in_amended_human_commit() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("base.txt", "Base content", "Initial commit");

    // Commit A: AI lines committed (no unstaged leftovers — all lines land)
    let mut ai_file = repo.filename("ai_module.rs");
    ai_file.set_contents(crate::lines![
        "fn generated() {".ai(),
        "    // auto-generated".ai(),
        "}".ai(),
    ]);
    let ai_commit = repo.stage_all_and_commit("Commit A: AI code").unwrap();
    assert!(
        ai_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    let ai_session_ids: Vec<String> = ai_commit
        .authorship_log
        .metadata
        .sessions
        .keys()
        .cloned()
        .collect();
    assert!(
        !ai_session_ids.is_empty(),
        "precondition: commit A must have session records"
    );

    // Commit B: human-only on a different file
    let human_path = repo.path().join("notes.txt");
    fs::write(&human_path, "Human note 1\n").unwrap();
    repo.git(&["add", "notes.txt"]).unwrap();
    repo.git(&["commit", "-m", "Commit B - human only"])
        .unwrap();

    // Amend B — blame VA will see A's AI lines; they must not leak into B's note
    fs::write(&human_path, "Human note 1\nHuman note 2\n").unwrap();
    repo.git(&["add", "notes.txt"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Commit B amended"])
        .unwrap();

    let amended_log = head_authorship_log(&repo);

    assert!(
        amended_log.metadata.prompts.is_empty(),
        "amended human-only commit should have no prompts"
    );
    for session_id in &ai_session_ids {
        assert!(
            !amended_log.metadata.sessions.contains_key(session_id),
            "Blame-sourced AI session '{}' from commit A should NOT appear in amended B.\n\
             Sessions in amended commit: {:?}",
            session_id,
            amended_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }
}

/// Complementary: amending a commit that actually has AI lines MUST keep those
/// prompts — ensures we don't over-filter in the amend path.
#[test]
fn test_amend_preserves_prompt_when_ai_lines_survive() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("base.txt", "Base content", "Initial commit");

    // Commit with AI lines
    let mut ai_file = repo.filename("code.rs");
    ai_file.set_contents(crate::lines![
        "fn init() {".ai(),
        "    setup();".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("AI commit").unwrap();

    // Amend: add more AI lines (AI lines still present after amend)
    ai_file.insert_at(2, crate::lines!["    extra();".ai()]);
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "AI commit (amended)"])
        .unwrap();

    let amended_log = head_authorship_log(&repo);
    assert!(
        amended_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !amended_log.metadata.sessions.is_empty(),
        "Amended commit with surviving AI lines must still have session records"
    );
}

// ===========================================================================
// Rebase path — rebase replays commits; stale prompts from INITIAL must not
// leak into rebased human-only commits.
// ===========================================================================

/// Regression: after a rebase, human-only commits must not contain stale AI
/// prompts from an earlier AI commit on the same branch.
///
/// During rebase replay, each commit is re-applied and the post-commit hook
/// regenerates its authorship note.  The rebase path rewrites notes via
/// `rewrite_authorship_if_needed` which may use `from_working_log_for_commit`
/// (the merge path) or `post_commit` depending on the event type.
///
/// Scenario:
/// 1. Main: initial commit
/// 2. Feature branch:
///    a. Commit A: AI writes code (prompt P1)
///    b. Commit B: human-only on a different file
/// 3. Main advances
/// 4. Rebase feature onto main → replayed B must NOT have A's prompt
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_stale_prompt_not_in_rebased_human_commit() {
    let repo = TestRepo::new();

    // Initial commit on main (via git_og — clean, no hooks)
    repo.commit_untracked_file("shared.txt", "shared content", "Initial commit");
    let default_branch = repo.current_branch();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Commit A on feature: AI writes code
    let mut ai_file = repo.filename("ai_feature.rs");
    ai_file.set_contents(crate::lines![
        "fn feature() {".ai(),
        "    do_stuff();".ai(),
        "}".ai(),
    ]);
    let ai_commit = repo
        .stage_all_and_commit("Feature commit A: AI code")
        .unwrap();
    assert!(
        ai_commit.authorship_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    let ai_session_ids: Vec<String> = ai_commit
        .authorship_log
        .metadata
        .sessions
        .keys()
        .cloned()
        .collect();
    assert!(
        !ai_session_ids.is_empty(),
        "precondition: feature commit A must have session records"
    );

    // Commit B on feature: human-only on a separate file
    let human_path = repo.path().join("human_feature.txt");
    fs::write(&human_path, "Human feature content\n").unwrap();
    repo.git(&["add", "human_feature.txt"]).unwrap();
    repo.git(&["commit", "-m", "Feature commit B: human-only"])
        .unwrap();

    // Advance main with a non-conflicting raw commit
    repo.git_og(&["checkout", &default_branch]).unwrap();
    repo.commit_untracked_file("main_only.txt", "main-only content", "Main advances");

    // Rebase feature onto main
    repo.git_og(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch])
        .expect("rebase should succeed (no conflicts)");

    // Read the rebased commit B (HEAD) note
    let rebased_b_log = head_authorship_log(&repo);

    assert!(
        rebased_b_log.metadata.prompts.is_empty(),
        "rebased human-only commit B should have no prompts"
    );
    for id in &ai_session_ids {
        assert!(
            !rebased_b_log.metadata.sessions.contains_key(id),
            "Stale AI session '{}' should NOT appear in rebased human-only commit B.\n\
             Sessions in rebased B: {:?}",
            id,
            rebased_b_log.metadata.sessions.keys().collect::<Vec<_>>()
        );
    }
}

/// Complementary: after a rebase, commits that DO have AI lines must keep their
/// prompts — ensures rebase doesn't over-filter.
#[test]
#[cfg(not(target_os = "windows"))]
fn test_rebase_preserves_prompt_when_ai_lines_present() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("shared.txt", "shared content", "Initial commit");
    let default_branch = repo.current_branch();

    // Feature branch with an AI commit
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let mut ai_file = repo.filename("feature_code.rs");
    ai_file.set_contents(crate::lines![
        "fn feature() {".ai(),
        "    do_stuff();".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("Feature: AI code").unwrap();

    // Advance main
    repo.git(&["checkout", &default_branch]).unwrap();
    repo.commit_untracked_file("main_only.txt", "main-only content", "Main advances");

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch])
        .expect("rebase should succeed");

    let rebased_log = head_authorship_log(&repo);
    assert!(
        rebased_log.metadata.prompts.is_empty(),
        "new-format test should produce sessions, not prompts"
    );
    assert!(
        !rebased_log.metadata.sessions.is_empty(),
        "Rebased AI commit must still have session records"
    );
}
