use super::*;
use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
use crate::model::working_log::{AgentId, CheckpointKind};
use crate::operations::daemon::checkpoint_stream_authority::authorize_checkpoint_stream_source;
use serial_test::serial;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

struct CheckpointStreamEnvGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl CheckpointStreamEnvGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: callers use serial tests, so process environment mutation is isolated.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for CheckpointStreamEnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => {
                // SAFETY: the serial test is restoring the environment it isolated.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            }
            None => {
                // SAFETY: the serial test is restoring the environment it isolated.
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CheckpointStreamIngress {
    Legacy,
    Delivery,
}

fn init_checkpoint_stream_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("init")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct CheckpointStreamFixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    claude_root: PathBuf,
    _config: CheckpointStreamEnvGuard,
    _claude: CheckpointStreamEnvGuard,
}

impl CheckpointStreamFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        init_checkpoint_stream_repo(&repo);
        std::fs::write(repo.join("edit.txt"), "checkpoint content\n").unwrap();
        let canonical_repo = repo.canonicalize().unwrap();
        let config_patch = serde_json::json!({
            "allowed_repositories": [canonical_repo.to_string_lossy()]
        })
        .to_string();
        let config =
            CheckpointStreamEnvGuard::set("GIT_AI_TEST_CONFIG_PATCH", config_patch.as_ref());
        let claude_root = temp.path().join("trusted-claude-home");
        std::fs::create_dir_all(claude_root.join("projects")).unwrap();
        let claude = CheckpointStreamEnvGuard::set("CLAUDE_CONFIG_DIR", claude_root.as_os_str());
        Self {
            _temp: temp,
            repo,
            claude_root,
            _config: config,
            _claude: claude,
        }
    }

    fn host_path(&self, name: &str) -> PathBuf {
        let path = self._temp.path().join(name);
        std::fs::write(&path, "{\"private\":\"host-only\"}\n").unwrap();
        path
    }

    fn trusted_claude_session(&self, external_session_id: &str) -> PathBuf {
        let project = self.claude_root.join("projects/test-project");
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{external_session_id}.jsonl"));
        std::fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        path
    }
}

fn checkpoint_stream_request(
    repo: &std::path::Path,
    stream_path: PathBuf,
    external_session_id: &str,
    include_file: bool,
) -> CheckpointRequest {
    checkpoint_stream_request_for(
        repo,
        stream_path,
        external_session_id,
        include_file,
        "claude",
        crate::model::checkpoint_request::StreamFormat::ClaudeJsonl,
    )
}

fn checkpoint_stream_request_for(
    repo: &std::path::Path,
    stream_path: PathBuf,
    external_session_id: &str,
    include_file: bool,
    tool: &str,
    format: crate::model::checkpoint_request::StreamFormat,
) -> CheckpointRequest {
    use crate::model::authorship_log_serialization::generate_session_id;
    use crate::model::checkpoint_request::{BaseCommit, CheckpointFile, StreamSource};

    let files = include_file
        .then(|| CheckpointFile {
            path: repo.join("edit.txt"),
            content: Some("checkpoint content\n".to_string()),
            repo_work_dir: repo.to_path_buf(),
            base_commit: BaseCommit::Initial,
        })
        .into_iter()
        .collect();
    CheckpointRequest {
        trace_id: "stream-authority-test".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: Some(AgentId {
            tool: tool.to_string(),
            id: external_session_id.to_string(),
            model: "test".to_string(),
        }),
        files,
        path_role: PreparedPathRole::Edited,
        stream_source: Some(StreamSource {
            path: stream_path,
            format,
            session_id: generate_session_id(external_session_id, tool),
            external_session_id: external_session_id.to_string(),
            external_parent_session_id: None,
        }),
        metadata: HashMap::new(),
    }
}

async fn submit_checkpoint_stream_request(
    coordinator: &ActorDaemonCoordinator,
    ingress: CheckpointStreamIngress,
    request: CheckpointRequest,
) -> crate::model::daemon_control::ControlResponse {
    let control_request = match ingress {
        CheckpointStreamIngress::Legacy => ControlRequest::CheckpointRun {
            request: Box::new(request),
        },
        CheckpointStreamIngress::Delivery => {
            let delivery = crate::model::checkpoint_delivery::CheckpointDelivery::from_requests_at(
                vec![request],
                42,
            )
            .remove(0);
            ControlRequest::CheckpointDeliver {
                delivery: Box::new(delivery),
            }
        }
    };
    coordinator.handle_control_request(control_request).await
}

#[tokio::test]
#[serial]
async fn checkpoint_stream_ingress_omits_untrusted_host_path_but_preserves_checkpoint() {
    for ingress in [
        CheckpointStreamIngress::Legacy,
        CheckpointStreamIngress::Delivery,
    ] {
        let fixture = CheckpointStreamFixture::new();
        let secret = fixture.host_path("host-secret.jsonl");
        let coordinator = ActorDaemonCoordinator::new();
        let response = submit_checkpoint_stream_request(
            &coordinator,
            ingress,
            checkpoint_stream_request(&fixture.repo, secret.clone(), "host-secret", true),
        )
        .await;

        assert!(
            response.ok,
            "an optional untrusted stream source must not discard exact checkpoint data: {response:?}"
        );
        assert!(
            fixture.repo.join(".git/ai").exists(),
            "the authorized checkpoint must still reach repository storage"
        );
    }
}

#[tokio::test]
#[serial]
async fn checkpoint_stream_ingress_rejects_zero_file_authority_without_repository_storage() {
    for ingress in [
        CheckpointStreamIngress::Legacy,
        CheckpointStreamIngress::Delivery,
    ] {
        let fixture = CheckpointStreamFixture::new();
        let secret = fixture.host_path("host-secret.jsonl");
        let coordinator = ActorDaemonCoordinator::new();
        let response = submit_checkpoint_stream_request(
            &coordinator,
            ingress,
            checkpoint_stream_request(&fixture.repo, secret, "host-secret", false),
        )
        .await;

        assert!(!response.ok, "zero-file stream source must fail closed");
        assert_eq!(
            response.error.as_deref(),
            Some("checkpoint stream source authority could not be verified")
        );
        assert!(
            !fixture.repo.join(".git/ai").exists(),
            "zero-file request must not construct repository storage"
        );
    }
}

#[tokio::test]
#[serial]
#[cfg(unix)]
async fn checkpoint_stream_ingress_omits_discovered_symlink_escape() {
    use std::os::unix::fs::symlink;

    for ingress in [
        CheckpointStreamIngress::Legacy,
        CheckpointStreamIngress::Delivery,
    ] {
        let fixture = CheckpointStreamFixture::new();
        let secret = fixture.host_path("host-secret.jsonl");
        let project = fixture.claude_root.join("projects/test-project");
        std::fs::create_dir_all(&project).unwrap();
        let escaped_source = project.join("host-secret.jsonl");
        symlink(&secret, &escaped_source).unwrap();

        let coordinator = ActorDaemonCoordinator::new();
        let response = submit_checkpoint_stream_request(
            &coordinator,
            ingress,
            checkpoint_stream_request(&fixture.repo, escaped_source, "host-secret", true),
        )
        .await;

        assert!(
            response.ok,
            "a stream symlink escape must drop enrichment without dropping the checkpoint: {response:?}"
        );
        assert!(fixture.repo.join(".git/ai").exists());
    }
}

#[tokio::test]
#[serial]
async fn checkpoint_stream_ingress_accepts_exact_host_discovered_session() {
    for ingress in [
        CheckpointStreamIngress::Legacy,
        CheckpointStreamIngress::Delivery,
    ] {
        let fixture = CheckpointStreamFixture::new();
        let transcript = fixture.trusted_claude_session("trusted-session");
        let agent = crate::operations::streams::agent::get_agent("claude").unwrap();
        let discovered = agent.discover_sessions().unwrap();
        assert!(
            discovered.iter().any(|session| {
                session.external_session_id == "trusted-session"
                    && session.stream_path == transcript
            }),
            "trusted fixture must be discoverable from the daemon-owned Claude root: {discovered:?}"
        );
        let policy_location =
            crate::operations::git::repository::discover_repository_policy_location_no_git_exec(
                &fixture.repo,
            )
            .unwrap();
        let policy =
            crate::operations::git::repository::load_repository_policy_context_no_git_exec(
                &policy_location,
            )
            .unwrap();
        assert!(
            policy.is_collection_allowed(&crate::config::Config::fresh()),
            "fresh daemon policy must allow the checkpoint repository"
        );

        let coordinator = ActorDaemonCoordinator::new();
        let response = submit_checkpoint_stream_request(
            &coordinator,
            ingress,
            checkpoint_stream_request(&fixture.repo, transcript, "trusted-session", true),
        )
        .await;

        assert!(
            response.ok,
            "exact host-discovered stream session should remain supported: {response:?}"
        );
        assert!(
            fixture.repo.join(".git/ai").exists(),
            "authorized checkpoint should reach repository storage"
        );
    }
}

#[test]
#[serial]
fn checkpoint_stream_authority_resolves_host_opencode_database_without_sweeping() {
    let fixture = CheckpointStreamFixture::new();
    let storage = fixture._temp.path().join("opencode");
    std::fs::create_dir_all(&storage).unwrap();
    let database = storage.join("opencode.db");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT);
             INSERT INTO session (id, parent_id) VALUES ('oc-session', 'oc-parent');",
        )
        .unwrap();
    drop(connection);
    let _storage =
        CheckpointStreamEnvGuard::set("GIT_AI_OPENCODE_STORAGE_PATH", storage.as_os_str());

    let mut request = checkpoint_stream_request_for(
        &fixture.repo,
        fixture._temp.path().join("sandbox-opencode.db"),
        "oc-session",
        true,
        "opencode",
        crate::model::checkpoint_request::StreamFormat::OpenCodeSqlite,
    );
    authorize_checkpoint_stream_source(&mut request).unwrap();

    let source = request
        .stream_source
        .expect("host OpenCode session should resolve");
    assert_eq!(source.path, database.canonicalize().unwrap());
    assert_eq!(
        source.external_parent_session_id.as_deref(),
        Some("oc-parent")
    );
}

#[test]
#[serial]
fn checkpoint_stream_authority_binds_pi_header_id_under_host_session_root() {
    let fixture = CheckpointStreamFixture::new();
    let sessions = fixture._temp.path().join("pi-sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let transcript = sessions.join("2026-07-25_pi-session.jsonl");
    let large_later_event = format!(
        "{{\"type\":\"message\",\"content\":\"{}\"}}\n",
        "x".repeat(70 * 1024)
    );
    std::fs::write(
        &transcript,
        format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"pi-session\"}}\n{large_later_event}"
        ),
    )
    .unwrap();
    let _sessions =
        CheckpointStreamEnvGuard::set("PI_CODING_AGENT_SESSION_DIR", sessions.as_os_str());

    let mut request = checkpoint_stream_request_for(
        &fixture.repo,
        transcript.clone(),
        "pi-session",
        true,
        "pi",
        crate::model::checkpoint_request::StreamFormat::PiJsonl,
    );
    authorize_checkpoint_stream_source(&mut request).unwrap();

    assert_eq!(
        request.stream_source.unwrap().path,
        transcript.canonicalize().unwrap()
    );
}

#[test]
#[serial]
fn checkpoint_stream_authority_binds_windsurf_id_to_host_transcript_name() {
    let fixture = CheckpointStreamFixture::new();
    let home = fixture._temp.path().join("windsurf-home");
    let _home = CheckpointStreamEnvGuard::set("HOME", home.as_os_str());
    let _profile = CheckpointStreamEnvGuard::set("USERPROFILE", home.as_os_str());
    let transcripts = home.join(".windsurf/transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let transcript = transcripts.join("trajectory-1.jsonl");
    std::fs::write(&transcript, "{\"type\":\"user_input\"}\n").unwrap();

    let mut request = checkpoint_stream_request_for(
        &fixture.repo,
        transcript.clone(),
        "trajectory-1",
        true,
        "windsurf",
        crate::model::checkpoint_request::StreamFormat::WindsurfJsonl,
    );
    authorize_checkpoint_stream_source(&mut request).unwrap();

    assert_eq!(
        request.stream_source.unwrap().path,
        transcript.canonicalize().unwrap()
    );
}

#[test]
#[serial]
fn checkpoint_stream_authority_drops_namespace_format_and_session_id_mismatches() {
    let fixture = CheckpointStreamFixture::new();
    let transcript = fixture.trusted_claude_session("trusted-session");

    let mut wrong_namespace =
        checkpoint_stream_request(&fixture.repo, transcript.clone(), "trusted-session", true);
    wrong_namespace.agent_id.as_mut().unwrap().id = "different-session".to_string();
    authorize_checkpoint_stream_source(&mut wrong_namespace).unwrap();
    assert!(wrong_namespace.stream_source.is_none());

    let mut wrong_format =
        checkpoint_stream_request(&fixture.repo, transcript.clone(), "trusted-session", true);
    wrong_format.stream_source.as_mut().unwrap().format =
        crate::model::checkpoint_request::StreamFormat::PiJsonl;
    authorize_checkpoint_stream_source(&mut wrong_format).unwrap();
    assert!(wrong_format.stream_source.is_none());

    let mut wrong_internal =
        checkpoint_stream_request(&fixture.repo, transcript, "trusted-session", true);
    wrong_internal.stream_source.as_mut().unwrap().session_id = "s_wrong".to_string();
    authorize_checkpoint_stream_source(&mut wrong_internal).unwrap();
    assert!(wrong_internal.stream_source.is_none());

    let wrong_extension_path = fixture
        .claude_root
        .join("projects/test-project/wrong-extension.txt");
    std::fs::write(&wrong_extension_path, "{\"type\":\"user\"}\n").unwrap();
    let mut wrong_extension =
        checkpoint_stream_request(&fixture.repo, wrong_extension_path, "wrong-extension", true);
    authorize_checkpoint_stream_source(&mut wrong_extension).unwrap();
    assert!(wrong_extension.stream_source.is_none());
}
