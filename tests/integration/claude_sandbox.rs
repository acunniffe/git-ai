use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::daemon::DaemonConfig;
use serde_json::{Value, json};
use std::fs;

#[test]
#[cfg(unix)]
fn install_hooks_allows_trace2_socket_in_all_claude_sandboxes() {
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
                    "allowUnixSockets": ["/tmp/user-owned.sock"],
                    "allowAllUnixSockets": false
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    repo.git_ai_without_pre_sync_for_test(&["install-hooks"])
        .expect("install Claude hooks");

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
    #[cfg(target_os = "linux")]
    assert_eq!(network["allowAllUnixSockets"], true);

    repo.git_ai_without_pre_sync_for_test(&["install-hooks"])
        .expect("reinstall Claude hooks");
    assert_eq!(fs::read_to_string(settings_path).unwrap(), first_install);
}
