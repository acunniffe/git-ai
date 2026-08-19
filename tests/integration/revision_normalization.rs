use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use serde_json::Value;
use std::fs;

struct DivergedWorktree {
    repo: TestRepo,
    primary_commit: String,
    linked_commit: String,
    linked_prompt_id: String,
}

fn diverged_worktree_with_lowercase_head() -> DivergedWorktree {
    let repo = TestRepo::new_worktree();
    let mut file = repo.filename("test.txt");

    file.set_contents(crate::lines!["primary AI line".ai()]);
    let primary = repo.stage_all_and_commit("Primary branch commit").unwrap();

    // Keep the primary worktree's branch at the first attributed commit while
    // advancing the linked worktree's branch to a distinct attributed commit.
    repo.git(&["update-ref", "refs/heads/main", &primary.commit_sha])
        .unwrap();
    file.insert_at(1, crate::lines!["linked AI line".ai()]);
    let linked = repo.stage_all_and_commit("Linked worktree commit").unwrap();

    let linked_prompt_id = linked
        .authorship_log
        .metadata
        .sessions
        .keys()
        .next()
        .expect("linked commit should contain an AI session")
        .clone();

    // On a case-insensitive filesystem, `head` can resolve through the common
    // Git directory's HEAD. Create that spelling explicitly so this macOS bug
    // is reproduced on every test platform.
    let common_dir = repo.git(&["rev-parse", "--git-common-dir"]).unwrap();
    let common_dir = repo.path().join(common_dir.trim());
    fs::copy(common_dir.join("HEAD"), common_dir.join("head")).unwrap();

    DivergedWorktree {
        repo,
        primary_commit: primary.commit_sha,
        linked_commit: linked.commit_sha,
        linked_prompt_id,
    }
}

#[test]
fn user_revision_commands_normalize_lowercase_head_in_linked_worktree() {
    let fixture = diverged_worktree_with_lowercase_head();
    let repo = &fixture.repo;

    assert_eq!(
        repo.git_ai(&["show", "head"]).unwrap(),
        repo.git_ai(&["show", "HEAD"]).unwrap()
    );
    assert_eq!(
        repo.git_ai(&["diff", "head"]).unwrap(),
        repo.git_ai(&["diff", "HEAD"]).unwrap()
    );
    assert_eq!(
        repo.git_ai(&["stats", "head", "--json"]).unwrap(),
        repo.git_ai(&["stats", "HEAD", "--json"]).unwrap()
    );

    let show_prompt = |rev: &str| -> Value {
        let output = repo
            .git_ai(&["show-prompt", &fixture.linked_prompt_id, "--commit", rev])
            .unwrap();
        serde_json::from_str(output.trim()).unwrap()
    };
    assert_eq!(show_prompt("head"), show_prompt("HEAD"));
}

#[test]
fn show_normalizes_both_range_endpoints_and_preserves_head_suffixes() {
    let fixture = diverged_worktree_with_lowercase_head();
    let repo = &fixture.repo;

    assert_eq!(
        repo.git_ai(&["show", "head~1..head"]).unwrap(),
        repo.git_ai(&["show", "HEAD~1..HEAD"]).unwrap()
    );
    assert_eq!(
        repo.git_ai(&["diff", "head~1..head"]).unwrap(),
        repo.git_ai(&["diff", "HEAD~1..HEAD"]).unwrap()
    );
    assert_eq!(
        repo.git_ai(&["stats", "head~1..head", "--json"]).unwrap(),
        repo.git_ai(&["stats", "HEAD~1..HEAD", "--json"]).unwrap()
    );

    for (lowercase, uppercase) in [
        ("head~1", "HEAD~1"),
        ("head^1", "HEAD^1"),
        ("head@{0}", "HEAD@{0}"),
    ] {
        assert_eq!(
            repo.git_ai(&["show", lowercase]).unwrap(),
            repo.git_ai(&["show", uppercase]).unwrap(),
            "{lowercase} should preserve its revision suffix"
        );
    }
}

#[test]
fn revision_names_that_merely_begin_with_head_are_not_rewritten() {
    let fixture = diverged_worktree_with_lowercase_head();
    let repo = &fixture.repo;
    repo.git(&["branch", "header", &fixture.primary_commit])
        .unwrap();

    let header = repo.git_ai(&["show", "header"]).unwrap();
    let linked_head = repo.git_ai(&["show", "HEAD"]).unwrap();

    assert_ne!(fixture.primary_commit, fixture.linked_commit);
    assert_ne!(
        header, linked_head,
        "the branch named `header` must remain intact"
    );
}

#[test]
fn non_ascii_revision_names_are_not_rewritten_or_sliced_mid_character() {
    let fixture = diverged_worktree_with_lowercase_head();
    let repo = &fixture.repo;
    repo.git(&["branch", "中文分支", &fixture.primary_commit])
        .unwrap();

    assert_eq!(
        repo.git_ai(&["show", "中文分支"]).unwrap(),
        repo.git_ai(&["show", &fixture.primary_commit]).unwrap()
    );
}
