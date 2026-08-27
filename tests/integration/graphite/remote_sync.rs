//! `gt get` reconciliation when a branch has diverged between two clones.
//!
//! Covers every combination of local × remote divergence:
//!
//! |                     | remote: none | remote: rebased | remote: meaningful |
//! |---------------------|--------------|-----------------|--------------------|
//! | local: none         | — (no-op)    | ✔               | ✔                  |
//! | local: rebased      | ✔            | ✔               | ✔                  |
//! | local: meaningful   | ✔            | ✔               | ✔                  |
//!
//! "Rebased" means the commit was replayed onto a moved trunk — same tree, new
//! SHA — which runs through Graphite's `commit-tree` + `update-ref` plumbing.
//! "Meaningful" means the tree actually changed.
//!
//! The remote side of each case is produced by a real second clone
//! ([`GraphiteTestRepo::peer`]) that submits through Graphite, so the branch
//! under test reaches the first clone only via the remote.
//!
//! ## Which side `gt get` keeps
//!
//! If there are meaningful changes both locally and remotely, then we need to
//! tell graphite which changes to "keep". If the `--force` option is provided
//! to `gt get`, the command will overwrite local changes if there are meaningful
//! remote changes.
//!
//! ## Confirmed gap: attribution does not survive adoption from the remote
//!
//! `gt get` fetches with `git fetch`, and git-ai deliberately does NOT import
//! authorship notes on `fetch` — only `clone`, `pull`, and `git-ai fetch-notes`
//! do. See `notes_sync_fetch_does_not_import_authorship_notes` in
//! `tests/notes_sync_regression.rs`.
//!
//! Running this matrix confirmed the consequence. The two tests that read the
//! *remote's* attribution both fail, and they are the only two that ever do —
//! every other case keeps the local branch, whose notes are already local:
//!
//! - `test_gt_get_adopts_meaningful_remote_when_local_unchanged` — no note
//!   arrives; every AI line falls back to the committer identity.
//! - `test_gt_get_force_adopts_remote_attribution_over_meaningful_local` — worse:
//!   the local note is remapped onto the adopted commit, so it attests the lines
//!   this clone already knew and silently omits the line the peer added. The
//!   attribution looks complete and is wrong.
//!
//! ## Why these are serialized
//!
//! Branch namespacing isolates what each test pushes, but `refs/notes/ai` is a
//! *single* ref shared by the whole repository. Every clone's daemon pushes
//! authorship notes there after a `git push`, so tests running in parallel race
//! to lock it and fail with `cannot lock ref 'refs/notes/ai': is at X but
//! expected Y` — faster than the fetch-merge-retry loop in
//! `push_authorship_notes` can recover. `#[serial(graphite_remote)]` puts every
//! remote-backed Graphite test in one group so only one pushes notes at a time.
//!
//! Run with `./tests/integration/graphite/scripts/run-graphite-tests.sh`; see
//! `super::remote_ops` for the required environment.

use super::graphite_test_harness::GraphiteTestRepo;
use crate::repos::test_file::ExpectedLineExt;

/// How one side of the branch changed before `gt get` reconciles the two.
#[derive(Clone, Copy, PartialEq)]
enum Divergence {
    /// Untouched since it was submitted.
    None,
    /// Replayed onto a moved trunk: same tree, new SHA.
    Rebased,
    /// The tree changed — an AI line was appended.
    Meaningful,
}

const BRANCH_NAME: &str = "feature";

/// The file's content after `Divergence::Meaningful` has been applied to it.
fn meaningful_lines(marker: &str) -> Vec<crate::repos::test_file::ExpectedLine> {
    crate::lines![
        "human line".human(),
        "ai line".ai(),
        format!("{marker} ai line").as_str().ai(),
    ]
}

/// Advance this test's trunk by one commit and publish it.
///
/// Trunk is kept identical between clones on purpose: this matrix is about the
/// *feature* branch diverging. Leaving a trunk commit unpushed instead makes
/// `gt get` refuse with "trunk could not be fast-forwarded" and reconcile
/// nothing, which tests a different — and far less natural — scenario.
fn advance_trunk(clone: &GraphiteTestRepo, marker: &str) {
    clone
        .gt(&["checkout", clone.trunk()])
        .expect("gt checkout trunk should succeed");

    // Pick up trunk movement another clone already published, so this commit
    // lands on top of it and the push below stays a fast-forward. A no-op unless
    // the peer advanced trunk first.
    clone
        .repo
        .git(&["pull", "--ff-only", "origin", clone.trunk()])
        .expect("pulling trunk should succeed");

    let mut trunk_file = clone.repo.filename(&clone.scoped_path("trunk.txt"));
    trunk_file.set_contents(crate::lines![format!("{marker} trunk line").as_str()]);
    clone.repo.git(&["add", "-A"]).unwrap();
    clone
        .repo
        .commit(&format!("{marker} trunk commit"))
        .expect("trunk commit should succeed");
    clone
        .repo
        .git(&["push", "origin", clone.trunk()])
        .expect("pushing the moved trunk should succeed");
}

/// Apply `divergence` to the feature branch in `clone`.
///
/// `marker` distinguishes the local edit from the remote one so the assertion
/// can tell which side `gt get` kept.
fn apply(clone: &GraphiteTestRepo, divergence: Divergence, marker: &str) {
    match divergence {
        Divergence::None => {}

        Divergence::Rebased => {
            // Advance trunk, then restack the feature branch onto it. The commit
            // is replayed with an identical tree but a new SHA.
            advance_trunk(clone, marker);
            clone
                .gt(&["checkout", &clone.branch(BRANCH_NAME)])
                .expect("gt checkout feature should succeed");
            clone.gt(&["restack"]).expect("gt restack should succeed");
        }

        Divergence::Meaningful => {
            clone
                .gt(&["checkout", &clone.branch(BRANCH_NAME)])
                .expect("gt checkout feature should succeed");
            let mut file = clone.repo.filename(&clone.scoped_path("feature.txt"));
            file.set_contents(meaningful_lines(marker));
            clone.repo.git(&["add", "-A"]).unwrap();
            clone
                .gt(&["modify", "-c", "-m", &format!("{marker} change")])
                .expect("gt modify should succeed");
        }
    }
}

/// Drive one cell of the matrix with the default (non-forced) `gt get`.
fn run_get_case(
    test_name: &str,
    local: Divergence,
    remote: Divergence,
) -> Option<(GraphiteTestRepo, Result<String, String>)> {
    run_case(test_name, local, remote, GraphiteTestRepo::get)
}

/// Drive one cell of the matrix with `gt get --force`, which takes the remote as
/// the source of truth even when local changes are discarded.
fn run_forced_get_case(
    test_name: &str,
    local: Divergence,
    remote: Divergence,
) -> Option<(GraphiteTestRepo, Result<String, String>)> {
    run_case(test_name, local, remote, GraphiteTestRepo::get_force)
}

/// Create and submit a branch from clone A, apply `remote` in a peer clone and
/// submit it, apply `local` in A, then reconcile A with `reconcile`.
///
/// Returns `None` when prerequisites are missing, so callers can `return`.
fn run_case(
    test_name: &str,
    local: Divergence,
    remote: Divergence,
    reconcile: fn(&GraphiteTestRepo, &str) -> Result<String, String>,
) -> Option<(GraphiteTestRepo, Result<String, String>)> {
    let test_repo = GraphiteTestRepo::new(test_name)?;
    let feature = test_repo.branch(BRANCH_NAME);

    // Clone A creates the branch and submits it.
    let mut file = test_repo
        .repo
        .filename(&test_repo.scoped_path("feature.txt"));
    file.set_contents(crate::lines!["human line", "ai line".ai()]);
    test_repo.repo.git(&["add", "-A"]).unwrap();
    test_repo
        .gt(&["create", &feature, "-m", "feature branch"])
        .expect("gt create should succeed");
    file.assert_committed_lines(crate::lines!["human line".human(), "ai line".ai()]);
    test_repo.submit().expect("gt submit should succeed");

    // A peer clone picks the branch up and diverges the remote side.
    if remote != Divergence::None {
        let test_repo_peer = test_repo.peer();
        test_repo_peer
            .get(&feature)
            .expect("gt get should succeed in the peer");
        apply(&test_repo_peer, remote, "remote");
        test_repo_peer
            .submit()
            .expect("peer gt submit should succeed");
        // Flush the peer's daemon so the notes it pushed reflect final state.
        test_repo_peer.repo.sync_daemon();
    }

    // Diverge the local side. The feature branch is deliberately never pushed —
    // that divergence from the remote is what `gt get` has to reconcile.
    apply(&test_repo, local, "local");

    let result = reconcile(&test_repo, &feature);
    Some((test_repo, result))
}

/// The feature file as clone A sees it after reconciling.
fn assert_feature_lines(
    test_repo: &GraphiteTestRepo,
    expected: Vec<crate::repos::test_file::ExpectedLine>,
) {
    test_repo
        .repo
        .filename(&test_repo.scoped_path("feature.txt"))
        .assert_committed_lines(expected);
}

// ===== Group 1: no local changes =====

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_adopts_rebased_remote_when_local_unchanged() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_rebased_remote_no_local",
        Divergence::None,
        Divergence::Rebased,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // The remote only replayed the commit; content and attribution are unchanged.
    assert_feature_lines(
        &test_repo,
        crate::lines!["human line".human(), "ai line".ai()],
    );
}

/// KNOWN_ISSUE:FETCH_NOTES — a branch adopted from the remote arrives with no
/// authorship at all.
///
/// `gt get` fetches with `git fetch`, and git-ai does not import notes on
/// `fetch` (only `clone`, `pull`, and `git-ai fetch-notes` do — see
/// `notes_sync_fetch_does_not_import_authorship_notes` in
/// `tests/notes_sync_regression.rs`). Nothing rewrites history in this case
/// either — the local branch is unchanged, so `gt get` just fast-forwards — so
/// the on-demand rescue in `fetch_missing_notes_for_commits`
/// (`src/git/sync_authorship.rs`) never fires.
///
/// Observed: both AI lines land on the peer's commit with the committer identity
/// instead of the agent.
///
/// ```text
/// 0d7cbeb7 (Test User    ... 1) human line
/// 21c2b212 (Graphite Test ... 2) ai line          <- expected mock_ai
/// 21c2b212 (Graphite Test ... 3) remote ai line   <- expected mock_ai
/// ```
#[test]
#[ignore]
#[serial_test::serial(graphite_remote)]
fn test_gt_get_adopts_meaningful_remote_when_local_unchanged() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_meaningful_remote_no_local",
        Divergence::None,
        Divergence::Meaningful,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // The remote's added AI line must arrive attributed to AI, not fall back to
    // untracked-human because its note never came down.
    assert_feature_lines(&test_repo, meaningful_lines("remote"));
}

// ===== Group 2: rebased local changes =====

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_rebased_local_and_unchanged_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_rebased_local_no_remote",
        Divergence::Rebased,
        Divergence::None,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    assert_feature_lines(
        &test_repo,
        crate::lines!["human line".human(), "ai line".ai()],
    );
}

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_rebased_local_and_rebased_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_rebased_local_rebased_remote",
        Divergence::Rebased,
        Divergence::Rebased,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // Both sides replayed the same tree, so whichever side wins the content is
    // identical — only the attribution is at risk.
    assert_feature_lines(
        &test_repo,
        crate::lines!["human line".human(), "ai line".ai()],
    );
}

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_rebased_local_and_meaningful_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_rebased_local_meaningful_remote",
        Divergence::Rebased,
        Divergence::Meaningful,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // Local only replayed; the remote's content is the one with real changes.
    assert_feature_lines(&test_repo, meaningful_lines("remote"));
}

// ===== Group 3: meaningful local changes =====

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_meaningful_local_and_unchanged_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_meaningful_local_no_remote",
        Divergence::Meaningful,
        Divergence::None,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // Nothing on the remote to adopt, so the local AI line must survive.
    assert_feature_lines(&test_repo, meaningful_lines("local"));
}

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_meaningful_local_and_rebased_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_meaningful_local_rebased_remote",
        Divergence::Meaningful,
        Divergence::Rebased,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // The remote only replayed, so the local AI line is the real content.
    assert_feature_lines(&test_repo, meaningful_lines("local"));
}

#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_sync -- --ignored`
#[serial_test::serial(graphite_remote)]
fn test_gt_get_preserves_attribution_for_meaningful_local_and_meaningful_remote() {
    let Some((test_repo, result)) = run_get_case(
        "test_gt_get_meaningful_local_meaningful_remote",
        Divergence::Meaningful,
        Divergence::Meaningful,
    ) else {
        return;
    };
    result.expect("gt get should succeed");

    // Both sides made real changes, so `gt get` keeps the local branch rather
    // than discarding work (adopting the remote requires `--force`). What matters
    // here is that surviving the reconcile attempt leaves the local AI line still
    // attributed to AI rather than degraded to untracked-human.
    assert_feature_lines(&test_repo, meaningful_lines("local"));
}

// ===== Group 4: forced adoption of the remote =====

/// KNOWN_ISSUE:FETCH_NOTES — Unlike the unforced case, a note IS present here:
/// the local note is remapped onto the adopted commit, so it attests the lines
/// this clone already knew about and silently says nothing about the line the
/// peer added. The result is not "attribution missing" but *attribution that
/// looks complete and is wrong*.
///
/// Observed — lines 2 and 3 are the same commit, attributed differently:
///
/// ```text
/// 89b38ac3 (Test User     ... 1) human line
/// 81058709 (mock_ai       ... 2) ai line          <- correct, local note remapped
/// 81058709 (Graphite Test ... 3) remote ai line   <- expected mock_ai
/// ```
#[test]
#[ignore]
#[serial_test::serial(graphite_remote)]
fn test_gt_get_force_adopts_remote_attribution_over_meaningful_local() {
    let Some((test_repo, result)) = run_forced_get_case(
        "test_gt_get_force_meaningful_local_meaningful_remote",
        Divergence::Meaningful,
        Divergence::Meaningful,
    ) else {
        return;
    };
    result.expect("gt get --force should succeed");

    // `--force` makes the remote the source of truth, discarding the local
    // commit — so the remote's AI line must arrive, and must arrive attributed.
    assert_feature_lines(&test_repo, meaningful_lines("remote"));
}
