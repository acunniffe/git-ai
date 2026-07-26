use std::fs;

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

#[test]
fn test_empty_nested_git_dir_does_not_shadow_parent_repository() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("nested/example.md");
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(&file_path, "Existing line\n").unwrap();

    repo.git(&["add", "nested/example.md"]).unwrap();
    repo.commit("Initial commit").unwrap();

    let mut file = repo.filename("nested/example.md");
    file.assert_committed_lines(crate::lines!["Existing line".unattributed_human()]);

    fs::create_dir(file_path.parent().unwrap().join(".git")).unwrap();

    repo.git_ai(&["checkpoint", "human", "nested/example.md"])
        .unwrap();
    fs::write(&file_path, "Existing line\nAI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "nested/example.md"])
        .unwrap();

    repo.git(&["add", "nested/example.md"]).unwrap();
    repo.commit("Add AI line").unwrap();

    file.assert_committed_lines(crate::lines![
        "Existing line".unattributed_human(),
        "AI line".ai(),
    ]);
}
