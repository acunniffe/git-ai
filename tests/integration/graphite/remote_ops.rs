//! Remote-backed Graphite (`gt`) operations.
//!
//! These tests drive `gt` against a real GitHub repository (`jumboblip/aug-6` by
//! default, override with `GRAPHITE_TEST_REPO`), covering the commands the
//! local-only suite in `super::local_ops` cannot reach — chiefly `gt submit`,
//! which force-pushes the stack and opens a pull request per branch.
//!
//! They are `#[ignore]`d by default because they push branches and open pull
//! requests on a shared remote. Run them with:
//!
//! ```sh
//! export GRAPHITE_TEST_TOKEN=...     # Graphite API token (app.graphite.dev/settings/cli)
//! export GRAPHITE_TEST_GH_TOKEN=...  # GitHub PAT with `repo` scope on the test repo
//! ./tests/integration/graphite/scripts/run-graphite-tests.sh
//! ```
//!
//! Every branch is namespaced under `gtai/<test>-<pid>-<timestamp>` and torn
//! down when the test ends. Set `GIT_AI_TEST_NO_CLEANUP=1` to leave the pushed
//! branches and opened PRs on the remote for inspection; sweep them afterwards
//! with `scripts/cleanup-test-branches.sh`.

use super::graphite_test_harness::GraphiteTestRepo;
use crate::repos::test_file::ExpectedLineExt;

/// `gt submit` pushes the branch and opens a PR for it. It rewrites commits
/// through `commit-tree` + `update-ref` when the stack needs restacking first,
/// so this is the core check that attribution survives a submit.
#[test]
#[ignore] // Remote-backed - run with `cargo test --test integration graphite::remote_ops -- --ignored`
fn test_gt_submit_opens_pr_and_preserves_attribution() {
    let Some(remote) = GraphiteTestRepo::new("test_gt_submit_opens_pr") else {
        return;
    };

    let branch = remote.branch("first");
    let mut file = remote.repo.filename(&remote.scoped_path("first.txt"));
    file.set_contents(crate::lines!["human line", "ai line".ai()]);
    remote
        .gt(&["create", &branch, "-am", "first branch"])
        .expect("gt create should succeed");

    file.assert_committed_lines(crate::lines!["human line".human(), "ai line".ai()]);

    remote.submit().expect("gt submit should succeed");

    assert!(
        remote.pr_number_for_branch(&branch).is_some(),
        "expected an open PR for {branch}"
    );

    // Attribution must survive the restack and force-push that submit performs.
    file.assert_committed_lines(crate::lines!["human line".human(), "ai line".ai()]);
}
