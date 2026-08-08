#[cfg(unix)]
use crate::repos::test_file::ExpectedLineExt;
#[cfg(unix)]
use crate::repos::test_repo::{TestRepo, real_git_executable};
#[cfg(unix)]
use crate::repos::write_executable_script;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn internal_background_subcommands_must_use_spawn_helper() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_root, &mut files);

    let disallowed_patterns = [
        Regex::new(r#"Command::new\([^\)]*\)(?s:.*?)\.arg\("flush-cas"\)"#).unwrap(),
        Regex::new(
            r#"Command::new\([^\)]*\)(?s:.*?)\.arg\("upgrade"\)(?s:.*?)\.arg\("--background"\)"#,
        )
        .unwrap(),
    ];

    for file in files {
        // This module is allowed to own the centralized spawn implementation.
        if file.ends_with("src/cli/git_ai_exe.rs") {
            continue;
        }

        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        for pattern in &disallowed_patterns {
            assert!(
                !pattern.is_match(&content),
                "direct internal background spawn found in {}: must use spawn_internal_git_ai_subcommand()",
                file.display()
            );
        }
    }
}

#[test]
fn critical_background_spawners_call_spawn_helper() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = [root.join("src/operations/commands/upgrade.rs")];

    for file in files {
        let content = fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("spawn_internal_git_ai_subcommand("),
            "{} must call spawn_internal_git_ai_subcommand()",
            file.display()
        );
    }
}

#[test]
fn internal_spawn_helper_calls_must_provide_guard_env() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_root, &mut files);

    let disallowed = Regex::new(
        r#"spawn_internal_git_ai_subcommand\(\s*"[^"]+"\s*,\s*&\[[^\]]*\]\s*,\s*None\s*,"#,
    )
    .unwrap();

    for file in files {
        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        assert!(
            !disallowed.is_match(&content),
            "guardless spawn_internal_git_ai_subcommand call found in {}",
            file.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn internal_git_spawns_disable_trace2_env() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("plain.txt"), "plain log content\n").unwrap();
    repo.stage_all_and_commit("feat: plain log").unwrap();
    let mut file = repo.filename("plain.txt");
    file.assert_committed_lines(lines!["plain log content".unattributed_human()]);

    let wrapper_path = repo.test_home_path().join("recording-git");
    let env_log_path = repo.test_home_path().join("internal-git-env.log");
    write_executable_script(
        &wrapper_path,
        r#"#!/bin/sh
{
  printf 'argv=%s\n' "$*"
  printf 'GIT_TRACE2=%s\n' "${GIT_TRACE2-<unset>}"
  printf 'GIT_TRACE2_EVENT=%s\n' "${GIT_TRACE2_EVENT-<unset>}"
  printf 'GIT_TRACE2_PERF=%s\n' "${GIT_TRACE2_PERF-<unset>}"
} >> "$GIT_AI_INTERNAL_GIT_ENV_LOG"
exec "$GIT_AI_REAL_GIT" "$@"
"#,
    )
    .unwrap();

    let config_path = repo.test_home_path().join(".git-ai").join("config.json");
    fs::write(
        &config_path,
        serde_json::json!({
            "git_path": wrapper_path,
            "prompt_storage": "notes",
            "exclude_prompts_in_repositories": [],
            "disable_version_checks": true
        })
        .to_string(),
    )
    .unwrap();

    let env_log = env_log_path.to_string_lossy().to_string();
    let real_git = real_git_executable().to_string();
    repo.git_ai_with_env(
        &["log", "--no-pager", "--plain", "-n", "1"],
        &[
            ("GIT_AI_INTERNAL_GIT_ENV_LOG", env_log.as_str()),
            ("GIT_AI_REAL_GIT", real_git.as_str()),
            ("GIT_TRACE2", "1"),
            ("GIT_TRACE2_EVENT", "1"),
            ("GIT_TRACE2_PERF", "1"),
        ],
    )
    .expect("git-ai log --plain should succeed through recording git");

    let env_log = fs::read_to_string(&env_log_path).expect("recording git should write env log");
    assert!(
        env_log.contains("argv=") && env_log.contains(" log "),
        "expected recording git to capture a git log invocation:\n{}",
        env_log
    );

    for line in env_log.lines() {
        if line.starts_with("GIT_TRACE2=") {
            assert_eq!(line, "GIT_TRACE2=0", "env log:\n{}", env_log);
        } else if line.starts_with("GIT_TRACE2_EVENT=") {
            assert_eq!(line, "GIT_TRACE2_EVENT=0", "env log:\n{}", env_log);
        } else if line.starts_with("GIT_TRACE2_PERF=") {
            assert_eq!(line, "GIT_TRACE2_PERF=0", "env log:\n{}", env_log);
        }
    }
}

#[test]
fn direct_git_command_spawns_are_centralized() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src_root, &mut files);

    let allowed_suffixes = ["src/clients/git_cli/mod.rs", "src/cli/git_handlers.rs"];
    let pattern =
        Regex::new(r#"Command::new\((?:crate::)?config::Config::get\(\)\.git_cmd\(\)\)"#).unwrap();

    for file in files {
        let file_str = file.to_string_lossy().replace('\\', "/");
        if allowed_suffixes
            .iter()
            .any(|suffix| file_str.ends_with(suffix))
        {
            continue;
        }

        let Ok(content) = fs::read_to_string(&file) else {
            continue;
        };
        assert!(
            !pattern.is_match(&content),
            "direct git command spawn found in {}: route through centralized repository exec helpers",
            file.display()
        );
    }
}

#[test]
fn ref_cursor_does_not_spawn_git_on_trace_ingestion_path() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("operations")
        .join("daemon")
        .join("ref_cursor");
    let mut files = Vec::new();
    collect_rs_files(&dir, &mut files);
    assert!(
        !files.is_empty(),
        "expected ref_cursor module files under {}",
        dir.display()
    );
    for file in files {
        let content = fs::read_to_string(&file).unwrap();
        for disallowed in [
            "Command::new(",
            "exec_git(",
            "exec_git_allow_nonzero(",
            "exec_git_stdin(",
            "exec_git_stdin_with_profile(",
        ] {
            assert!(
                !content.contains(disallowed),
                "{} must not contain `{}`; trace2 ingestion must not spawn git",
                file.display(),
                disallowed
            );
        }
    }
}
