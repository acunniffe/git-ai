use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{checkpoint_codex_bash_hook, setup_codex_bash_repo};
use std::fs;

fn write_blob(repo: &TestRepo, content: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob.txt");
    fs::write(&path, content).unwrap();
    let oid = repo
        .git(&["hash-object", "-w", path.to_str().unwrap()])
        .unwrap()
        .trim()
        .to_string();
    (dir, oid)
}

#[test]
fn test_update_index_cacheinfo_then_commit_inside_bash_is_ai_scoped() {
    let (_db, repo, transcript) = setup_codex_bash_repo("Initial commit");
    fs::write(repo.path().join("human-staged.txt"), "human staged\n").unwrap();
    repo.git_og(&["add", "human-staged.txt"]).unwrap();
    let (_blob_dir, blob) = write_blob(&repo, "plumbing AI\n");
    let cacheinfo = format!("100644,{blob},plumbing.txt");
    let command =
        format!("git update-index --add --cacheinfo {cacheinfo} && git commit -m plumbing");
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "plumbing-bash-session",
        "plumbing-bash-tool",
        "PreToolUse",
        &command,
    );
    repo.git(&["update-index", "--add", "--cacheinfo", &cacheinfo])
        .unwrap();
    repo.git(&["commit", "-m", "plumbing"]).unwrap();
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "plumbing-bash-session",
        "plumbing-bash-tool",
        "PostToolUse",
        &command,
    );
    repo.sync_daemon();
    repo.git_og(&["checkout", "HEAD", "--", "plumbing.txt"])
        .unwrap();

    let mut plumbing = repo.filename("plumbing.txt");
    plumbing.assert_committed_lines(lines!["plumbing AI".ai()]);
    let mut human = repo.filename("human-staged.txt");
    human.assert_committed_lines(lines!["human staged".unattributed_human()]);
}

#[test]
fn test_commit_tree_update_ref_inside_bash_attributes_new_index_content() {
    let (_db, repo, transcript) = setup_codex_bash_repo("Initial commit");
    let original_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let branch = repo.current_branch();
    fs::write(
        repo.path().join("pre-staged-human.txt"),
        "pre-staged human\n",
    )
    .unwrap();
    repo.git_og(&["add", "pre-staged-human.txt"]).unwrap();
    let (_blob_dir, blob) = write_blob(&repo, "commit-tree AI\n");
    let cacheinfo = format!("100644,{blob},commit-tree-ai.txt");
    let command = "git update-index --cacheinfo ... && git write-tree && git commit-tree ... && git update-ref ...";
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "plumbing-bash-session",
        "plumbing-bash-tool",
        "PreToolUse",
        command,
    );
    repo.git(&["update-index", "--add", "--cacheinfo", &cacheinfo])
        .unwrap();
    let tree = repo.git(&["write-tree"]).unwrap().trim().to_string();
    let new_head = repo
        .git(&[
            "commit-tree",
            &tree,
            "-p",
            &original_head,
            "-m",
            "plumbing commit",
        ])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&[
        "update-ref",
        &format!("refs/heads/{branch}"),
        &new_head,
        &original_head,
    ])
    .unwrap();
    checkpoint_codex_bash_hook(
        &repo,
        &transcript,
        "plumbing-bash-session",
        "plumbing-bash-tool",
        "PostToolUse",
        command,
    );
    repo.sync_daemon();
    repo.git_og(&["checkout", "HEAD", "--", "commit-tree-ai.txt"])
        .unwrap();

    let mut file = repo.filename("commit-tree-ai.txt");
    file.assert_committed_lines(lines!["commit-tree AI".ai()]);
    let mut human = repo.filename("pre-staged-human.txt");
    human.assert_committed_lines(lines!["pre-staged human".unattributed_human()]);
}
