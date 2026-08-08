#[cfg(unix)]
use crate::repos::test_file::ExpectedLineExt;
#[cfg(unix)]
use crate::repos::test_repo::TestRepo;
#[cfg(unix)]
use crate::repos::write_executable_script;
#[cfg(unix)]
use std::fs;

#[cfg(unix)]
fn install_subject_rewriting_commit_msg_hook(repo: &TestRepo) {
    let hooks_dir = repo.path().join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).expect("create Git hooks directory");

    let hook_path = hooks_dir.join("commit-msg");
    write_executable_script(
        &hook_path,
        "#!/bin/sh\n\
         message_file=\"$1\"\n\
         rewritten=\"$message_file.rewritten\"\n\
         { printf 'hooked: '; cat \"$message_file\"; } > \"$rewritten\"\n\
         mv \"$rewritten\" \"$message_file\"\n",
    )
    .expect("write commit-msg hook");
}

#[cfg(unix)]
#[test]
fn eng_216_commit_msg_hook_rewriting_subject_still_writes_authorship_note() {
    let repo = TestRepo::new();
    let app_path = repo.path().join("app.py");

    fs::write(&app_path, "print('start')\n").expect("write initial file");
    repo.stage_all_and_commit("initial commit")
        .expect("commit initial file");

    let mut app = repo.filename("app.py");
    app.assert_committed_lines(crate::lines!["print('start')".unattributed_human()]);

    install_subject_rewriting_commit_msg_hook(&repo);

    fs::write(&app_path, "print('start')\nprint('ai wrote this')\n")
        .expect("write AI-attributed edit");
    repo.git_ai(&["checkpoint", "mock_ai", "app.py"])
        .expect("checkpoint AI edit");
    repo.git(&["add", "app.py"]).expect("stage AI edit");

    repo.commit("add AI line")
        .expect("commit whose subject is rewritten by commit-msg hook");

    assert_eq!(
        repo.git(&["log", "-1", "--format=%s"])
            .expect("read committed subject")
            .trim(),
        "hooked: add AI line",
        "the regression requires Git to record the hook-rewritten subject"
    );
    app.assert_committed_lines(crate::lines![
        "print('start')".unattributed_human(),
        "print('ai wrote this')".ai(),
    ]);
}
