use crate::repos::test_repo::{TestRepo, run_raw_git_plumbing};

/// Reproduces a real-world failure from a large monorepo (~1000 engineers) where
/// users with a locally corrupted notes tree are permanently blocked from syncing.
///
/// The failure chain on the main branch is:
///
///   1. `git notes merge -s ours` crashes with:
///      "Assertion failed: (is_null_oid(&mp->base)), function diff_tree_remote,
///      file notes-merge.c, line 173"
///      because the merge-base has mixed fanout (both flat `aabbcc…` and subtree
///      `aa/bbcc…` entries for the same annotated commit), and the fixed remote
///      removed the flat entries. The diff sees a MOD + DEL for the same object.
///
///   2. The crash is caught and the fallback merge runs, but it uses `N` (notemodify)
///      commands, which require the annotated commit to exist locally. On a large
///      monorepo, remote notes reference commits from other developers that this
///      user hasn't fetched — so `N <blob> <missing_commit>` aborts fast-import.
///
///   3. Both errors are silently logged. The local notes ref is unchanged. The user
///      is stuck: every subsequent fetch/push retries the same broken merge.
///
/// The fix on this branch:
///   - Bypasses `git notes merge` entirely (no assertion crash)
///   - Uses `M 100644 <blob> <path>` instead of `N` (no object existence check)
///   - Uses `deleteall` to produce a clean tree (no corruption perpetuation)
///
/// This test sets up the exact scenario and verifies that a note for a remote-only
/// commit (which doesn't exist locally as a git object) is present after the merge.
#[test]
fn test_fetch_notes_with_locally_corrupted_mixed_fanout_tree() {
    let (mirror, upstream) = TestRepo::new_with_remote();

    // -- Step 1: Create commit A on mirror, push to upstream. --
    // Commit A exists locally on the mirror — its notes are on both sides.
    std::fs::write(mirror.path().join("file.txt"), "content\n").expect("write file");
    let commit_a = mirror
        .stage_all_and_commit("commit A")
        .expect("commit A should succeed");
    mirror
        .git(&["push", "origin", "main"])
        .expect("push should succeed");
    let sha_a = commit_a.commit_sha;

    // -- Step 2: Create commit C ONLY on upstream (mirror never has this object). --
    // This simulates another developer's commit on the monorepo that this user
    // hasn't fetched to their working copy. Commit C's note will only exist on
    // the remote side of the merge.
    let upstream_tree = run_raw_git_plumbing(upstream.path(), &["rev-parse", "HEAD^{tree}"], None);
    let sha_c = run_raw_git_plumbing(
        upstream.path(),
        &[
            "commit-tree",
            &upstream_tree,
            "-m",
            "other developer's commit",
        ],
        None,
    );

    let prefix_a = &sha_a[..2];
    let rest_a = &sha_a[2..];
    let prefix_c = &sha_c[..2];
    let rest_c = &sha_c[2..];

    // -- Step 3: Create note blobs on upstream. --
    let blob_base = run_raw_git_plumbing(
        upstream.path(),
        &["hash-object", "-w", "--stdin"],
        Some(b"note-base"),
    );
    let blob_fixed = run_raw_git_plumbing(
        upstream.path(),
        &["hash-object", "-w", "--stdin"],
        Some(b"note-fixed"),
    );
    let blob_local = run_raw_git_plumbing(
        upstream.path(),
        &["hash-object", "-w", "--stdin"],
        Some(b"note-local"),
    );

    // -- Step 4: Build the notes history on upstream via fast-import. --
    //
    // Three commits forming a diamond:
    //
    //   corrupted_base  (mixed fanout for commit A ONLY)
    //        /     \
    //   local       fixed_remote
    //   (still      (clean fanout for A,
    //   corrupted)   plus NEW note for C)
    //
    // Key: commit C's note is ONLY in fixed_remote, not in the base or local.
    // This means if the merge fails silently, C's note won't be in the final tree.
    let fi_stream = format!(
        "\
commit refs/notes/ai-base\n\
committer Test <test@test.com> 1000000000 +0000\n\
data 14\ncorrupted base\n\
M 100644 {blob_base} {sha_a}\n\
M 100644 {blob_base} {prefix_a}/{rest_a}\n\
\n\
commit refs/notes/ai-fixed\n\
committer Test <test@test.com> 1000000001 +0000\n\
data 12\nfixed remote\n\
from refs/notes/ai-base\n\
deleteall\n\
M 100644 {blob_fixed} {prefix_a}/{rest_a}\n\
M 100644 {blob_fixed} {prefix_c}/{rest_c}\n\
\n\
commit refs/notes/ai-local\n\
committer Test <test@test.com> 1000000001 +0000\n\
data 13\nlocal updated\n\
from refs/notes/ai-base\n\
M 100644 {blob_local} {prefix_a}/{rest_a}\n\
\n\
done\n"
    );
    run_raw_git_plumbing(
        upstream.path(),
        &["fast-import", "--quiet", "--done"],
        Some(fi_stream.as_bytes()),
    );

    // -- Step 5: Set up the refs. --
    // Upstream refs/notes/ai = fixed remote (what the remote looks like now).
    let fixed_commit =
        run_raw_git_plumbing(upstream.path(), &["rev-parse", "refs/notes/ai-fixed"], None);
    run_raw_git_plumbing(
        upstream.path(),
        &["update-ref", "refs/notes/ai", &fixed_commit],
        None,
    );

    // Transfer corrupted local notes to mirror via a temp ref.
    let local_commit =
        run_raw_git_plumbing(upstream.path(), &["rev-parse", "refs/notes/ai-local"], None);
    run_raw_git_plumbing(
        upstream.path(),
        &["update-ref", "refs/heads/tmp-local-notes", &local_commit],
        None,
    );
    mirror
        .git_og(&["fetch", "origin", "refs/heads/tmp-local-notes"])
        .expect("fetch local notes objects");
    let fetched_local = run_raw_git_plumbing(mirror.path(), &["rev-parse", "FETCH_HEAD"], None);
    run_raw_git_plumbing(
        mirror.path(),
        &["update-ref", "refs/notes/ai", &fetched_local],
        None,
    );
    run_raw_git_plumbing(
        upstream.path(),
        &["update-ref", "-d", "refs/heads/tmp-local-notes"],
        None,
    );

    // -- Sanity checks --
    // Mirror must NOT have commit C as a git object (simulates unfetched monorepo commit).
    assert!(
        mirror.git_og(&["cat-file", "-t", &sha_c]).is_err(),
        "precondition failed: mirror should NOT have commit C as a git object"
    );
    // Mirror's local notes tree should be corrupted (both flat and fanout for A).
    let tree_listing =
        run_raw_git_plumbing(mirror.path(), &["ls-tree", "-r", "refs/notes/ai"], None);
    assert!(
        tree_listing.contains(&sha_a) && tree_listing.contains(&format!("{}/{}", prefix_a, rest_a)),
        "precondition failed: local notes tree should have mixed fanout for commit A\n\
         tree listing:\n{}",
        tree_listing
    );
    // Mirror's local notes should NOT have commit C's note.
    assert!(
        !tree_listing.contains(&sha_c)
            && !tree_listing.contains(&format!("{}/{}", prefix_c, rest_c)),
        "precondition failed: local notes tree should NOT have commit C's note\n\
         tree listing:\n{}",
        tree_listing
    );

    // -- Step 5b: Fetch the remote notes ref so the objects are available locally. --
    // (fetch-notes does this internally, but we also need the tracking ref for
    // the manual merge attempt below.)
    mirror
        .git_og(&[
            "fetch",
            "origin",
            "refs/notes/ai:refs/notes/ai-remote/origin",
        ])
        .expect("fetch remote notes ref");

    // -- Step 5c: Verify that `git notes merge -s ours` fails on this tree. --
    // This is the bug we're working around. The mixed-fanout merge base causes
    // an assertion failure (or error) in git's notes-merge.c diff_tree_remote.
    // We save the local notes ref before the attempt so we can restore it after,
    // since a failed merge may leave the ref in a dirty state.
    let pre_merge_tip = run_raw_git_plumbing(mirror.path(), &["rev-parse", "refs/notes/ai"], None);
    let native_merge_result = mirror.git_og(&[
        "notes",
        "--ref=ai",
        "merge",
        "-s",
        "ours",
        "--quiet",
        "refs/notes/ai-remote/origin",
    ]);
    assert!(
        native_merge_result.is_err(),
        "precondition: `git notes merge -s ours` should FAIL on a mixed-fanout notes tree, \
         but it succeeded. This means the bug this test guards against may be fixed in this \
         version of git, and the fallback path is no longer exercised."
    );
    // Restore refs/notes/ai to its pre-merge state in case the failed merge
    // left behind a dirty .git/NOTES_MERGE_* worktree.
    run_raw_git_plumbing(
        mirror.path(),
        &["update-ref", "refs/notes/ai", &pre_merge_tip],
        None,
    );
    // Clean up any leftover merge state
    let _ = mirror.git_og(&["notes", "--ref=ai", "merge", "--abort"]);

    // -- Step 6: Run `git ai fetch-notes`. --
    // This fetches the fixed remote and tries to merge with the corrupted local.
    // `git notes merge -s ours` will fail (as verified above), then the fallback
    // merge (fast-import with `M` commands) kicks in and succeeds.
    mirror
        .git_ai(&["fetch-notes"])
        .expect("fetch-notes should succeed despite corrupted local notes tree");

    // -- Step 7: Verify commit C's note was merged from the remote. --
    //
    // On main before this fix, this assertion FAILS because:
    //   - `git notes merge -s ours` crashes (assertion in notes-merge.c)
    //   - The fallback's `N <blob> <sha_c>` fails (commit C doesn't exist locally;
    //     fast-import's `N` command requires the annotated object to be present)
    //   - Both errors are silently swallowed; local notes ref is unchanged
    //   - Commit C's note was never in the local tree, so it's missing
    //
    // With the fix, this assertion PASSES because:
    //   - `git notes merge -s ours` fails (as before)
    //   - The fallback uses `M` (filemodify) instead of `N` (notemodify), so it
    //     doesn't require the annotated object to exist locally
    //   - All notes (including commit C's) are written successfully
    let final_notes = run_raw_git_plumbing(mirror.path(), &["notes", "--ref=ai", "list"], None);
    assert!(
        final_notes.contains(&sha_c),
        "commit C's note should be present after merge (merged from remote).\n\n\
         On the main branch this fails because:\n\
         1. `git notes merge` crashes on the mixed-fanout merge base\n\
         2. The fallback's `N` (notemodify) command fails for commit C \
            because it doesn't exist locally as a git object\n\
         3. Both errors are silently swallowed, leaving local notes unchanged\n\n\
         notes list after merge:\n{}",
        final_notes,
    );
}
