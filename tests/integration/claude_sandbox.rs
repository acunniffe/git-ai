#![cfg(unix)]

use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::daemon::DaemonConfig;
use serde_json::{Value, json};
use std::fs;

fn enable_agent_sandbox_whitelisting(repo: &TestRepo) {
    repo.git_ai_without_pre_sync_for_test(&[
        "config",
        "set",
        "feature_flags.whitelist_agent_sandboxes",
        "true",
    ])
    .expect("enable agent sandbox whitelisting");
}

fn install_hooks(repo: &TestRepo) {
    repo.git_ai_without_pre_sync_for_test(&["install-hooks"])
        .expect("install Claude hooks");
}

#[test]
fn install_hooks_does_not_configure_claude_sandbox_by_default() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings_path = repo.test_home_path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    fs::write(&settings_path, "{}\n").unwrap();

    repo.git_ai_without_pre_sync_for_test(&["install-hooks"])
        .expect("install Claude hooks");

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
    assert!(settings["sandbox"].is_null());
    assert!(settings["hooks"]["PreToolUse"].is_array());
    assert!(settings["hooks"]["PostToolUse"].is_array());
}

#[test]
fn configured_install_hooks_allows_trace2_socket_in_claude_sandbox() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings_path = repo.test_home_path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({
            "theme": "dark",
            "sandbox": {
                "enabled": true,
                "network": {
                    "allowedDomains": ["example.com"],
                    "allowUnixSockets": ["/tmp/user-owned.sock"]
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    enable_agent_sandbox_whitelisting(&repo);
    install_hooks(&repo);

    let first_install = fs::read_to_string(&settings_path).unwrap();
    let settings: Value = serde_json::from_str(&first_install).unwrap();
    let network = &settings["sandbox"]["network"];
    let allowed_sockets = network["allowUnixSockets"].as_array().unwrap();
    let expected_trace_socket = DaemonConfig::from_home(repo.test_home_path())
        .trace_socket_path
        .to_string_lossy()
        .into_owned();

    assert!(allowed_sockets.contains(&json!("/tmp/user-owned.sock")));
    assert!(allowed_sockets.contains(&json!(expected_trace_socket)));
    assert_eq!(network["allowedDomains"], json!(["example.com"]));
    assert_eq!(settings["theme"], "dark");
    assert!(network["allowAllUnixSockets"].is_null());

    install_hooks(&repo);
    assert_eq!(fs::read_to_string(settings_path).unwrap(), first_install);
}

#[test]
fn install_hooks_preserves_explicit_sandbox_restrictions() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings_path = repo.test_home_path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({
            "sandbox": {
                "network": {
                    "allowAllUnixSockets": false
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    enable_agent_sandbox_whitelisting(&repo);
    install_hooks(&repo);

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
    assert_eq!(settings["sandbox"]["network"]["allowAllUnixSockets"], false);
    assert!(settings["hooks"]["PreToolUse"].is_array());
    assert!(settings["hooks"]["PostToolUse"].is_array());
}

#[test]
fn uninstall_hooks_removes_only_the_trace2_socket_allowance() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let settings_path = repo.test_home_path().join(".claude/settings.json");
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let original_sandbox = json!({
        "enabled": true,
        "network": {
            "allowedDomains": ["example.com"],
            "allowUnixSockets": ["/tmp/user-owned.sock"]
        }
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&json!({"sandbox": original_sandbox})).unwrap(),
    )
    .unwrap();

    enable_agent_sandbox_whitelisting(&repo);
    install_hooks(&repo);
    repo.git_ai_without_pre_sync_for_test(&["uninstall-hooks"])
        .expect("uninstall Claude hooks");

    let settings: Value =
        serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
    assert_eq!(settings["sandbox"], original_sandbox);
}

#[test]
fn install_hooks_ignores_unexpected_sandbox_shapes() {
    for sandbox in [
        json!(true),
        json!({"network": true}),
        json!({"network": {"allowUnixSockets": true}}),
    ] {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
        let settings_path = repo.test_home_path().join(".claude/settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&json!({"sandbox": sandbox})).unwrap(),
        )
        .unwrap();

        enable_agent_sandbox_whitelisting(&repo);
        install_hooks(&repo);

        let settings: Value =
            serde_json::from_str(&fs::read_to_string(settings_path).unwrap()).unwrap();
        assert_eq!(settings["sandbox"], sandbox);
        assert!(settings["hooks"]["PreToolUse"].is_array());
        assert!(settings["hooks"]["PostToolUse"].is_array());
    }
}
