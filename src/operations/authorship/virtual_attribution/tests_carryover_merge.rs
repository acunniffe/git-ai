use super::carryover_merge::{
    carryover_merge_content, checkout_merge_rebased_content, diff_hunks_between_contents,
    merged_carryover_content_pure,
};
use crate::model::imara_diff_utils::split_lines_with_terminators;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn checkout_merge_rebased_content_preserves_clean_local_hunk_on_target_edit() {
    let base = "one\ntwo\n";
    let target = "one feature\ntwo\n";
    let observed = "one\ntwo ai\n";

    assert_eq!(
        checkout_merge_rebased_content(base, target, observed),
        "one feature\ntwo ai\n"
    );
}

#[test]
fn checkout_merge_rebased_content_maps_eof_newline_only_target_line() {
    let base = "one\ntwo";
    let target = "one feature\ntwo\n";
    let observed = "one\ntwo ai\n";

    assert_eq!(
        checkout_merge_rebased_content(base, target, observed),
        "one feature\ntwo ai\n"
    );
}

#[test]
fn checkout_merge_rebased_content_uses_observed_when_target_unchanged() {
    assert_eq!(
        checkout_merge_rebased_content("base\n", "base\n", "ai\n"),
        "ai\n"
    );
}

/// Characterization: the in-memory 3-way merge used to build the carryover
/// snapshot must produce the same result the previous `git merge-file
/// --theirs -p <committed> <parent> <observed>` spawn produced, so that the
/// per-file `git merge-file` process can be eliminated. Roles:
/// base = parent, "ours/current" = committed, "theirs" (favored) = observed.
#[test]
fn carryover_merge_non_overlapping_changes_combines_both_sides() {
    // parent has 3 lines; committed edits line 1; observed edits line 3.
    // Non-overlapping edits on each side both survive.
    let parent = "a\nb\nc\n";
    let committed = "A\nb\nc\n";
    let observed = "a\nb\nC\n";
    assert_eq!(
        carryover_merge_content(parent, committed, observed),
        "A\nb\nC\n"
    );
}

#[test]
fn carryover_merge_overlapping_conflict_favors_observed() {
    // Both sides edit the same line differently → `--theirs` keeps observed.
    let parent = "shared\n";
    let committed = "COMMITTED\n";
    let observed = "OBSERVED\n";
    assert_eq!(
        carryover_merge_content(parent, committed, observed),
        "OBSERVED\n"
    );
}

#[test]
fn carryover_merge_committed_only_change_keeps_committed() {
    // observed == parent (no working-tree change) → committed side wins.
    let parent = "a\nb\n";
    let committed = "a\nB\n";
    let observed = "a\nb\n";
    assert_eq!(
        carryover_merge_content(parent, committed, observed),
        "a\nB\n"
    );
}

#[test]
fn carryover_merge_observed_only_change_keeps_observed() {
    // committed == parent (commit didn't touch file) → observed side wins.
    let parent = "a\nb\n";
    let committed = "a\nb\n";
    let observed = "a\nB\n";
    assert_eq!(
        carryover_merge_content(parent, committed, observed),
        "a\nB\n"
    );
}

/// Differential test: the in-memory carryover merge must agree with real
/// `git merge-file --theirs -p <committed> <parent> <observed>` across many
/// pseudo-random 3-way inputs that produce a clean (non-conflicting) merge.
/// (When git emits conflict markers the two are allowed to differ, since the
/// in-memory version deterministically favors observed; we focus on the
/// clean cases that dominate real carryover snapshots and assert exact
/// agreement there.)
#[test]
fn carryover_merge_matches_git_merge_file_on_random_clean_merges() {
    fn run_git_merge_file(parent: &str, committed: &str, observed: &str) -> Option<String> {
        let dir = std::env::temp_dir().join(format!(
            "git-ai-mf-difftest-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let cp = dir.join("committed");
        let pp = dir.join("parent");
        let op = dir.join("observed");
        std::fs::write(&cp, committed).ok()?;
        std::fs::write(&pp, parent).ok()?;
        std::fs::write(&op, observed).ok()?;
        let output = std::process::Command::new("git")
            .args([
                "merge-file",
                "--theirs",
                "-p",
                &cp.to_string_lossy(),
                &pp.to_string_lossy(),
                &op.to_string_lossy(),
            ])
            .output()
            .ok()?;
        let _ = std::fs::remove_dir_all(&dir);
        // Non-zero with conflict markers → skip (clean merges return 0).
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    }

    // Deterministic LCG so the test is reproducible without rand.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };

    let mut compared = 0;
    for _ in 0..600 {
        let n = (next() % 6) as usize + 1; // 1..=6 base lines
        let base: Vec<String> = (0..n).map(|i| format!("line{i}\n")).collect();
        // Each side independently keeps / edits / deletes each base line, and
        // may append a tail line.
        let mutate = |seed: &mut dyn FnMut() -> u32| -> String {
            let mut out = String::new();
            for (i, line) in base.iter().enumerate() {
                match seed() % 4 {
                    0 => out.push_str(line),                                 // keep
                    1 => out.push_str(&format!("edit{i}_{}\n", seed() % 3)), // edit
                    2 => {}                                                  // delete
                    _ => out.push_str(line),                                 // keep
                }
            }
            if seed().is_multiple_of(3) {
                out.push_str("tail\n");
            }
            out
        };
        let parent: String = base.concat();
        let committed = mutate(&mut next);
        let observed = mutate(&mut next);

        if let Some(git_result) = run_git_merge_file(&parent, &committed, &observed) {
            let ours = carryover_merge_content(&parent, &committed, &observed);
            assert_eq!(
                ours, git_result,
                "in-memory carryover merge diverged from git merge-file (clean merge)\nparent={parent:?}\ncommitted={committed:?}\nobserved={observed:?}"
            );
            compared += 1;
        }
    }
    assert!(
        compared > 50,
        "expected to compare a meaningful number of clean merges, got {compared}"
    );
}

#[test]
fn checkout_merge_rebased_content_preserves_local_side_for_overlapping_conflict() {
    assert_eq!(
        checkout_merge_rebased_content("shared\n", "THEIRS\n", "AI_CONTENT\n"),
        "AI_CONTENT\n"
    );
}

#[test]
fn carryover_merge_ignores_mixed_eol_when_content_matches_committed() {
    let parent = "base one\nbase two\n";
    let committed = "base one\nbase two\nai line\n";
    let observed = "base one\r\nbase two\r\nai line\n";

    assert_eq!(
        merged_carryover_content_pure(parent, committed, observed),
        committed
    );
    assert!(diff_hunks_between_contents(observed, committed).is_empty());
}

#[test]
fn carryover_merge_does_not_treat_crlf_only_observed_chunk_as_change() {
    let parent = "a\nb\nc\n";
    let committed = "A\nb\nC\n";
    let observed = "a\r\nb\r\nD\r\n";

    assert_eq!(
        carryover_merge_content(parent, committed, observed),
        "A\nb\nD\r\n"
    );
}

#[test]
fn shared_line_splitter_preserves_empty_and_crlf_input() {
    assert_eq!(split_lines_with_terminators(""), Vec::<&str>::new());
    assert_eq!(
        split_lines_with_terminators("line1\r\nline2"),
        vec!["line1\r\n", "line2"]
    );
}
