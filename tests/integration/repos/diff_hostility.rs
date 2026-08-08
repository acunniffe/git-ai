//! Shared helpers for tests that exercise git-ai's diff pipeline against
//! hostile/unusual repo-level diff configuration (custom prefixes, external
//! diff drivers, etc.).
#![allow(dead_code)]

use super::test_repo::TestRepo;
use super::write_executable_script;

/// Configure an external diff helper that git will invoke instead of its
/// built-in diff, writing `marker` to stdout. `helper_filename` names the
/// script written into the repo root. Returns `marker` for convenience.
pub fn configure_repo_external_diff_helper(
    repo: &TestRepo,
    marker: &str,
    helper_filename: &str,
) -> String {
    let helper_path = repo.path().join(helper_filename);
    let helper_path_posix = helper_path
        .to_str()
        .expect("helper path must be valid UTF-8")
        .replace('\\', "/");

    write_executable_script(&helper_path, format!("#!/bin/sh\necho {marker}\nexit 0\n"))
        .expect("should write external diff helper");

    repo.git_og(&["config", "diff.external", &helper_path_posix])
        .expect("configuring diff.external should succeed");

    marker.to_string()
}

/// Configure a repo-level `diff.*`/`color.*` config known to be hostile to
/// naive diff-output parsing (custom prefixes, renames-as-copies, forced
/// color, histogram algorithm, etc.), so tests can verify git-ai's diff
/// handling is robust to it.
pub fn configure_hostile_diff_settings(repo: &TestRepo) {
    let settings = [
        ("diff.noprefix", "true"),
        ("diff.mnemonicprefix", "true"),
        ("diff.srcPrefix", "SRC/"),
        ("diff.dstPrefix", "DST/"),
        ("diff.renames", "copies"),
        ("diff.relative", "true"),
        ("diff.algorithm", "histogram"),
        ("diff.indentHeuristic", "false"),
        ("diff.interHunkContext", "8"),
        ("color.diff", "always"),
        ("color.ui", "always"),
    ];
    for (key, value) in settings {
        repo.git_og(&["config", key, value])
            .unwrap_or_else(|err| panic!("setting {key}={value} should succeed: {err}"));
    }
}
