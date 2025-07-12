use git_ai::tmp_repo::TmpRepo;
use insta::assert_debug_snapshot;
use tempfile::tempdir;

#[test]
fn test_blame_after_merge_with_ai_contributions() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    // Create initial repository with base commit
    let (tmp_repo, mut lines, _) = TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

    // Create a feature branch
    tmp_repo.create_branch("feature").unwrap();

    // Make changes on feature branch
    lines.append("FEATURE LINE 1\nFEATURE LINE 2\n").unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo
        .commit_with_message("feature branch changes")
        .unwrap();

    // Switch back to the default branch and make different changes
    let default_branch = tmp_repo.get_default_branch().unwrap();
    tmp_repo.switch_branch(&default_branch).unwrap();
    lines.append("MAIN LINE 1\nMAIN LINE 2\n").unwrap();
    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    tmp_repo.commit_with_message("main branch changes").unwrap();

    // Merge feature branch into main
    tmp_repo
        .merge_branch("feature", "merge feature into main")
        .unwrap();

    // Test blame after merge
    let blame = tmp_repo.blame_for_file(&lines, Some((30, 35))).unwrap();
    assert_debug_snapshot!(blame);
}

// #[test]
// fn test_blame_after_rebase_with_ai_contributions() {
//     let tmp_dir = tempdir().unwrap();
//     let repo_path = tmp_dir.path().to_path_buf();

//     // Create initial repository with base commit
//     let (mut tmp_repo, mut lines, mut alphabet) =
//         TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

//     // Create a feature branch
//     tmp_repo.create_branch("feature").unwrap();

//     // Make changes on feature branch (add lines at the end)
//     lines
//         .append("REBASE FEATURE LINE 1\nREBASE FEATURE LINE 2\n")
//         .unwrap();
//     tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
//     tmp_repo
//         .commit_with_message("feature branch changes")
//         .unwrap();

//     // Switch back to the default branch and make different changes (insert in middle)
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.switch_branch(&default_branch).unwrap();
//     lines
//         .insert_at(15 * 2, "REBASE MAIN LINE 1\nREBASE MAIN LINE 2\n")
//         .unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main branch changes").unwrap();

//     // Switch back to feature and rebase onto the default branch
//     tmp_repo.switch_branch("feature").unwrap();
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.rebase_onto("feature", &default_branch).unwrap();

//     // Test blame after rebase
//     let blame = tmp_repo.blame_for_file(&lines, Some((30, 36))).unwrap();
//     assert_debug_snapshot!(blame);
// }

#[test]
fn test_blame_after_complex_merge_scenario() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    // Create initial repository with base commit
    let (tmp_repo, mut lines, _) = TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

    // Create multiple branches
    tmp_repo.create_branch("feature-a").unwrap();
    lines
        .append("\nFEATURE A LINE 1\nFEATURE A LINE 2\n")
        .unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo.commit_with_message("feature a changes").unwrap();

    tmp_repo.create_branch("feature-b").unwrap();
    lines
        .append("FEATURE B LINE 1\nFEATURE B LINE 2\n")
        .unwrap();
    tmp_repo.trigger_checkpoint_with_author("GPT-4").unwrap();
    tmp_repo.commit_with_message("feature b changes").unwrap();

    // Switch back to the default branch and make changes
    let default_branch = tmp_repo.get_default_branch().unwrap();
    tmp_repo.switch_branch(&default_branch).unwrap();
    lines
        .append("MAIN COMPLEX LINE 1\nMAIN COMPLEX LINE 2\n")
        .unwrap();
    tmp_repo
        .trigger_checkpoint_with_author("test_user")
        .unwrap();
    tmp_repo
        .commit_with_message("main complex changes")
        .unwrap();

    // Merge feature-a into main
    tmp_repo
        .merge_branch("feature-a", "merge feature-a into main")
        .unwrap();

    // Merge feature-b into main
    tmp_repo
        .merge_branch("feature-b", "merge feature-b into main")
        .unwrap();

    // Test blame after complex merge
    let blame = tmp_repo.blame_for_file(&lines, None).unwrap();
    assert_debug_snapshot!(blame);
}

// #[test]
// fn test_blame_after_rebase_chain() {
//     let tmp_dir = tempdir().unwrap();
//     let repo_path = tmp_dir.path().to_path_buf();

//     // Create initial repository with base commit
//     let (mut tmp_repo, mut lines, mut alphabet) =
//         TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

//     // Create a feature branch
//     tmp_repo.create_branch("feature").unwrap();

//     // Make multiple commits on feature branch
//     lines.append("REBASE CHAIN 1\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
//     tmp_repo.commit_with_message("feature commit 1").unwrap();

//     lines.append("REBASE CHAIN 2\n").unwrap();
//     tmp_repo.trigger_checkpoint_with_author("GPT-4").unwrap();
//     tmp_repo.commit_with_message("feature commit 2").unwrap();

//     // Switch back to the default branch and make changes
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.switch_branch(&default_branch).unwrap();
//     lines.append("MAIN CHAIN 1\n").unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main commit 1").unwrap();

//     lines.append("MAIN CHAIN 2\n").unwrap();
//     tmp_repo
//         .trigger_checkpoint_with_author("test_user")
//         .unwrap();
//     tmp_repo.commit_with_message("main commit 2").unwrap();

//     // Switch back to feature and rebase onto the default branch
//     tmp_repo.switch_branch("feature").unwrap();
//     let default_branch = tmp_repo.get_default_branch().unwrap();
//     tmp_repo.rebase_onto("feature", &default_branch).unwrap();

//     // Test blame after rebase chain
//     let blame = tmp_repo.blame_for_file(&lines, None).unwrap();
//     println!("blame: {:?}", blame);
//     assert_debug_snapshot!(blame);
// }

#[test]
fn test_blame_after_merge_conflict_resolution() {
    let tmp_dir = tempdir().unwrap();
    let repo_path = tmp_dir.path().to_path_buf();

    // Create initial repository with base commit
    let (tmp_repo, mut lines, _) = TmpRepo::new_with_base_commit(repo_path.clone()).unwrap();

    // Create a feature branch
    tmp_repo.create_branch("feature").unwrap();

    // Make changes on feature branch
    lines
        .replace_range(15, 16, "CONFLICT FEATURE VERSION\n")
        .unwrap();
    tmp_repo.trigger_checkpoint_with_author("Claude").unwrap();
    tmp_repo
        .commit_with_message("feature conflict changes")
        .unwrap();

    // Switch back to the default branch and make conflicting changes
    let default_branch = tmp_repo.get_default_branch().unwrap();
    tmp_repo.switch_branch(&default_branch).unwrap();
    lines
        .replace_range(15, 16, "CONFLICT MAIN VERSION\n")
        .unwrap();
    tmp_repo.trigger_checkpoint_with_author("new-user").unwrap();
    tmp_repo
        .commit_with_message("main conflict changes")
        .unwrap();

    // Merge feature branch into main (our simplified merge will take main's version)
    tmp_repo
        .merge_branch("feature", "merge feature with conflict resolution")
        .unwrap();

    // Test blame after conflict resolution
    let blame = tmp_repo.blame_for_file(&lines, Some((10, 20))).unwrap();
    assert_debug_snapshot!(blame);
}
