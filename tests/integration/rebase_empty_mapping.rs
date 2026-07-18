use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;

/// Regression for #1476.
///
/// The first rebase drops a commit whose patch is already upstream, producing no
/// commit mapping. A later rebase to the same target must use its own ref span
/// and migrate the amended AI commit's authorship note.
#[test]
fn test_empty_mapping_rebase_does_not_block_later_rebase_to_same_target() {
    let (repo, _remote) = TestRepo::new_with_remote();
    let shared_path = repo.path().join("shared.txt");

    fs::write(&shared_path, "base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let default_branch = repo.current_branch();
    repo.git(&["push", "-u", "origin", &default_branch])
        .unwrap();
    let base_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let mut shared_file = repo.filename("shared.txt");
    shared_file.assert_committed_lines(crate::lines!["base".human()]);

    repo.git(&["checkout", "-b", "upstream"]).unwrap();
    fs::write(&shared_path, "base\nequivalent\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .unwrap();
    let upstream_equivalent = repo
        .stage_all_and_commit("upstream equivalent patch")
        .unwrap();
    repo.git(&["push", "origin", "upstream:main"]).unwrap();
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);

    repo.git(&["checkout", &default_branch]).unwrap();
    assert_eq!(
        repo.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        base_commit
    );
    repo.git(&["checkout", "-b", "feature"]).unwrap();
    fs::write(&shared_path, "base\nequivalent\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .unwrap();
    let feature_equivalent = repo
        .stage_all_and_commit("feature equivalent patch")
        .unwrap();
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);

    repo.git(&["rebase", "origin/main"]).unwrap();
    let first_rebase_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(first_rebase_head, feature_equivalent.commit_sha);
    assert_eq!(first_rebase_head, upstream_equivalent.commit_sha);
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);

    let ai_path = repo.path().join("ai.txt");
    fs::write(&ai_path, "AI line 1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai.txt"]).unwrap();
    let ai_commit = repo.stage_all_and_commit("add AI work").unwrap();
    let mut ai_file = repo.filename("ai.txt");
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);
    ai_file.assert_committed_lines(crate::lines!["AI line 1".ai()]);
    assert!(repo.read_authorship_note(&ai_commit.commit_sha).is_some());

    fs::write(&ai_path, "AI line 1\nAI line 2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai.txt"]).unwrap();
    repo.git(&["add", "ai.txt"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "amend AI work"])
        .unwrap();
    let amended_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(amended_commit, ai_commit.commit_sha);
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);
    ai_file.assert_committed_lines(crate::lines!["AI line 1".ai(), "AI line 2".ai()]);
    assert!(repo.read_authorship_note(&amended_commit).is_some());

    repo.git(&["checkout", "upstream"]).unwrap();
    let upstream_path = repo.path().join("upstream.txt");
    fs::write(&upstream_path, "new upstream line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "upstream.txt"])
        .unwrap();
    repo.stage_all_and_commit("advance upstream").unwrap();
    repo.git(&["push", "origin", "upstream:main"]).unwrap();
    let mut upstream_file = repo.filename("upstream.txt");
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);
    upstream_file.assert_committed_lines(crate::lines!["new upstream line".human()]);

    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", "origin/main"]).unwrap();

    let rebased_commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert_ne!(rebased_commit, amended_commit);
    assert!(repo.read_authorship_note(&rebased_commit).is_some());
    shared_file.assert_committed_lines(crate::lines!["base".human(), "equivalent".human()]);
    upstream_file.assert_committed_lines(crate::lines!["new upstream line".human()]);
    ai_file.assert_committed_lines(crate::lines!["AI line 1".ai(), "AI line 2".ai()]);

    let stats = repo.stats().unwrap();
    assert_eq!(stats.git_diff_added_lines, 2);
    assert_eq!(stats.ai_additions, 2);
    assert_eq!(stats.unknown_additions, 0);
}
