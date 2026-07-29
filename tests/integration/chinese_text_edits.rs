use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;

#[test]
fn test_chinese_simple_additions() {
    let repo = TestRepo::new();
    let mut file = repo
        .set_file_contents_and_commit(
            "chinese.txt",
            crate::lines!["第一行", "第二行", "第三行"],
            "Initial commit",
        )
        .unwrap();

    file.insert_at(3, crate::lines!["新增一行".ai(), "新增二行".ai()]);
    repo.stage_all_and_commit("AI adds chinese lines").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "第一行".human(),
        "第二行".human(),
        "第三行".ai(),
        "新增一行".ai(),
        "新增二行".ai(),
    ]);
}

#[test]
fn test_chinese_ai_then_human_edits() {
    let repo = TestRepo::new();
    let mut file = repo.filename("status.txt");

    file.set_contents(crate::lines!["功能: 初始化", "状态: 正常", "备注: 无"]);
    repo.stage_all_and_commit("Base commit").unwrap();

    file.replace_at(1, "状态: 已更新".ai());
    repo.stage_all_and_commit("AI updates status").unwrap();

    file.replace_at(1, "状态: 已确认".human());
    repo.stage_all_and_commit("Human confirms status").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "功能: 初始化".human(),
        "状态: 已确认".human(),
        "备注: 无".human(),
    ]);
}

#[test]
fn test_chinese_deletions_and_insertions() {
    let repo = TestRepo::new();
    let mut file = repo.filename("notes.txt");

    file.set_contents(crate::lines![
        "计划: 第一阶段",
        "目标: 完成模块A",
        "风险: 依赖延迟",
        "缓解: 提前沟通",
        "负责人: 张三",
    ]);
    repo.stage_all_and_commit("Initial notes").unwrap();

    file.delete_range(1, 3);
    file.insert_at(
        1,
        crate::lines!["目标: 完成模块B".ai(), "风险: 资源紧张".ai()],
    );
    repo.stage_all_and_commit("AI rewrites plan").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "计划: 第一阶段".human(),
        "目标: 完成模块B".ai(),
        "风险: 资源紧张".ai(),
        "缓解: 提前沟通".human(),
        "负责人: 张三".human(),
    ]);
}

#[test]
fn test_chinese_partial_staging() {
    let repo = TestRepo::new();
    let mut file = repo.filename("partial.txt");

    file.set_contents(crate::lines!["甲: 一", "乙: 二", "丙: 三"]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    file.replace_at(0, "甲: 更新".ai());
    file.replace_at(1, "乙: 更新".ai());
    file.stage();

    file.insert_at(3, crate::lines!["新增: 四".ai(), "新增: 五".ai()]);
    let commit = repo.commit("Partial staging").unwrap();
    assert_eq!(commit.authorship_log.attestations.len(), 1);

    file.assert_committed_lines(crate::lines![
        "甲: 更新".ai(),
        "乙: 更新".ai(),
        // "丙: 三" is not committed because the unstaged insert adds a newline.
    ]);
}

// TODO Reflow and move detection tests need a harness for setting the feature flags, but manually tested
#[test]
#[ignore]
fn test_chinese_move_detection_preserves_ai() {
    let repo = TestRepo::new();
    let mut file = repo.filename("move.txt");

    file.set_contents(crate::lines![
        "标题",
        "段落一",
        "AI块一".ai(),
        "AI块二".ai(),
        "AI块三".ai(),
        "AI块四".ai(),
        "结尾",
    ]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Move the AI block to the end (human edit).
    file.delete_range(2, 6);
    file.insert_at(3, crate::lines!["AI块一", "AI块二", "AI块三", "AI块四"]);

    repo.stage_all_and_commit("Human moves AI block").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "标题".human(),
        "段落一".human(),
        "结尾".human(),
        "AI块一".ai(),
        "AI块二".ai(),
        "AI块三".ai(),
        "AI块四".ai(),
    ]);
}

#[test]
#[ignore]
fn test_chinese_reflow_preserves_ai() {
    use std::fs;

    let repo = TestRepo::new();
    let mut file = repo.filename("reflow.txt");

    file.set_contents(crate::lines!["调用(参数一, 参数二, 参数三)".ai()]);
    repo.stage_all_and_commit("Initial commit").unwrap();

    let file_path = repo.path().join("reflow.txt");
    fs::write(&file_path, "调用(\n  参数一,\n  参数二,\n  参数三\n)").unwrap();
    repo.git_ai(&["checkpoint"]).unwrap();
    repo.stage_all_and_commit("Human reflow").unwrap();

    file.assert_lines_and_blame(crate::lines![
        "调用(".ai(),
        "  参数一,".ai(),
        "  参数二,".ai(),
        "  参数三".ai(),
        ")".ai(),
    ]);
}

crate::reuse_tests_in_worktree!(
    test_chinese_simple_additions,
    test_chinese_ai_then_human_edits,
    test_chinese_deletions_and_insertions,
    test_chinese_partial_staging,
);
