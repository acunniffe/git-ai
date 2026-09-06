//! Static checks on the MDM login-start scripts in `mdm/`.
//!
//! These scripts register `git-ai bg start` to run at login. The daemon
//! supervises itself (uptime restart, self-update, lock-guarded single
//! instance), so the launch definitions must never supervise it and must
//! never tear down the process group the daemon is left in once `bg start`
//! exits. The platform-specific behaviour is exercised in CI by
//! `scripts/mdm/test-login-start.{sh,ps1}`; these tests pin the invariants
//! that make those runs pass.

use std::fs;
use std::path::PathBuf;

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn mdm_file(relative: &str) -> String {
    repo_file(&format!("mdm/{relative}"))
}

fn macos_script() -> String {
    mdm_file("macos/install-login-start.sh")
}

fn linux_script() -> String {
    mdm_file("linux/install-login-start.sh")
}

fn windows_script() -> String {
    mdm_file("windows/install-login-start.ps1")
}

/// Asserts that the plist `<key>` is immediately followed by `<value/>`,
/// ignoring the whitespace between them.
fn plist_key_has_value(plist: &str, key: &str, value: &str) -> bool {
    let key_tag = format!("<key>{key}</key>");
    plist
        .split(&key_tag)
        .skip(1)
        .any(|rest| rest.trim_start().starts_with(&format!("<{value}/>")))
}

#[test]
fn macos_launch_agent_abandons_process_group_and_never_keeps_alive() {
    let script = macos_script();

    assert!(
        plist_key_has_value(&script, "AbandonProcessGroup", "true"),
        "launchd kills the daemon left behind by `bg start` unless AbandonProcessGroup is true"
    );
    assert!(
        plist_key_has_value(&script, "KeepAlive", "false"),
        "KeepAlive would fight the daemon's own lock-guarded self-restart"
    );
    assert!(
        plist_key_has_value(&script, "RunAtLoad", "true"),
        "RunAtLoad is what makes the agent fire at login"
    );
    assert!(
        script.contains("com.usegitai.bg"),
        "launch agent label should be stable so MDM tooling can reference it"
    );
    assert!(
        script.contains("bg start") && !script.contains("bg run"),
        "the agent must invoke the idempotent `bg start`, never the foreground `bg run`"
    );
}

#[test]
fn linux_unit_is_oneshot_that_remains_after_exit_without_restart() {
    let script = linux_script();

    assert!(
        script.contains("Type=oneshot"),
        "unit must be a oneshot: systemd does not own the daemon process"
    );
    assert!(
        script.contains("RemainAfterExit=yes"),
        "without RemainAfterExit systemd tears down the cgroup, killing the daemon"
    );
    assert!(
        script.contains("WantedBy=default.target"),
        "unit must be pulled in by the user session"
    );
    assert!(
        !script.contains("Restart="),
        "Restart= would loop against the daemon's own lock-guarded self-restart"
    );
    assert!(
        script.contains("bg start") && !script.contains("bg run"),
        "the unit must invoke the idempotent `bg start`, never the foreground `bg run`"
    );
}

#[test]
fn windows_task_runs_once_per_logon_as_the_current_user_without_time_limit() {
    let script = windows_script();

    assert!(
        script.contains("-MultipleInstances IgnoreNew"),
        "a second task instance must not race the running daemon"
    );
    assert!(
        script.contains("-LogonType Interactive"),
        "task must run in the logged-on user's own session"
    );
    assert!(
        script.contains("-ExecutionTimeLimit"),
        "task must disable the execution time limit so Task Scheduler never ends the instance"
    );
    assert!(
        !script.contains(r"BUILTIN\Users") && !script.contains("-RunLevel Highest"),
        "task must be per-user and unelevated; group principals require administrator rights"
    );
    assert!(
        script.contains("bg start") && !script.contains("bg run"),
        "the task must invoke the idempotent `bg start`, never the foreground `bg run`"
    );
}

#[test]
fn all_scripts_share_the_same_cli_contract() {
    for (name, script) in [
        ("macos", macos_script()),
        ("linux", linux_script()),
        ("windows", windows_script()),
    ] {
        for flag in ["--uninstall", "--env", "--bin", "--no-start"] {
            assert!(
                script.contains(flag),
                "{name} script must support {flag} like the other platforms"
            );
        }
    }
    for (name, script) in [("macos", macos_script()), ("linux", linux_script())] {
        assert!(
            script.contains("--system"),
            "{name} script must offer an all-users --system mode"
        );
    }
}

#[test]
fn readme_documents_the_launch_invariants() {
    let readme = mdm_file("README.md");

    for keyword in [
        "AbandonProcessGroup",
        "RemainAfterExit",
        "IgnoreNew",
        "lock",
        "--uninstall",
        "--env",
    ] {
        assert!(
            readme.contains(keyword),
            "mdm/README.md must explain {keyword}"
        );
    }
}

#[test]
fn release_workflow_publishes_the_scripts_as_assets() {
    let release = repo_file(".github/workflows/release.yml");
    let readme = mdm_file("README.md");

    for asset in [
        "git-ai-login-start-macos.sh",
        "git-ai-login-start-linux.sh",
        "git-ai-login-start-windows.ps1",
    ] {
        assert!(
            release.contains(asset),
            "release.yml must copy {asset} into the release assets"
        );
        assert!(
            readme.contains(asset),
            "mdm/README.md must document the {asset} download"
        );
    }
    assert!(
        release.contains("sha256sum install.sh install.ps1 git-ai-login-start-*"),
        "login-start assets must be covered by SHA256SUMS"
    );
}
