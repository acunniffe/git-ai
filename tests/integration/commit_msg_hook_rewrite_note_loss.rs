//! Repro for a customer bug: commits that run a hook suite can complete with
//! an empty `new_head` in the daemon, so no authorship note is written.
//!
//! The repro uses a `commit-msg` hook that rewrites the final commit subject.
//! `git commit -m ...` records the original subject in argv, but git writes the
//! HEAD reflog message from the hook-rewritten final commit message. git-ai
//! currently matches commit reflog entries against an exact message built from
//! the raw argv subject, so the real reflog entry is missed and `ref_changes`
//! remains empty.

#[cfg(unix)]
use crate::repos::test_file::ExpectedLineExt;
#[cfg(unix)]
use crate::repos::test_repo::TestRepo;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn install_git_hook(repo: &TestRepo, name: &str, script: &str) {
    let git_dir = repo.path().join(".git");
    let hooks_dir = if git_dir.is_file() {
        let content = fs::read_to_string(&git_dir).expect("read .git file");
        let real_git_dir = content
            .trim()
            .strip_prefix("gitdir: ")
            .expect("parse gitdir");
        std::path::PathBuf::from(real_git_dir).join("hooks")
    } else {
        git_dir.join("hooks")
    };
    fs::create_dir_all(&hooks_dir).expect("create hooks dir");
    let hook_path = hooks_dir.join(name);
    fs::write(&hook_path, script).unwrap_or_else(|_| panic!("write {name} hook"));
    let mut perms = fs::metadata(&hook_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook_path, perms).unwrap_or_else(|_| panic!("chmod {name} hook"));
}

/// A hook suite can include a `commit-msg` hook that rewrites the final subject.
/// Git writes the reflog message from the final hook-rewritten commit message,
/// but git-ai currently builds an exact expected reflog message from the raw
/// `git commit -m ...` argv. When those differ, the daemon misses the HEAD
/// reflog entry and logs the commit with empty `new_head`.
#[cfg(unix)]
#[test]
fn commit_msg_hook_rewriting_subject_still_writes_authorship_note() {
    let repo = TestRepo::new();
    let app_path = repo.path().join("app.py");

    fs::write(&app_path, "print('start')\n").unwrap();
    repo.stage_all_and_commit("initial commit").unwrap();

    let mut app = repo.filename("app.py");
    app.assert_committed_lines(crate::lines!["print('start')".unattributed_human()]);

    install_git_hook(
        &repo,
        "commit-msg",
        "#!/bin/sh\n\
         msg_file=\"$1\"\n\
         tmp=\"$msg_file.tmp\"\n\
         { printf 'hooked: '; cat \"$msg_file\"; } > \"$tmp\"\n\
         mv \"$tmp\" \"$msg_file\"\n\
         exit 0\n",
    );

    fs::write(&app_path, "print('start')\nprint('ai wrote this')\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "app.py"]).unwrap();
    repo.git(&["add", "app.py"]).unwrap();

    // The actual commit subject becomes "hooked: add AI line", while the raw
    // argv subject remains "add AI line". The daemon must still learn HEAD's
    // new OID and write the authorship note.
    repo.commit("add AI line").unwrap();

    app.assert_committed_lines(crate::lines![
        "print('start')".unattributed_human(),
        "print('ai wrote this')".ai(),
    ]);
}
