use super::*;
use crate::model::checkpoint_request::StreamSource as CheckpointStreamSource;
use std::ffi::OsString;
use std::io::Write;
use tempfile::TempDir;

struct EnvRestore {
    key: &'static str,
    value: Option<OsString>,
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        unsafe {
            match &self.value {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn make_worker(db: Arc<StreamsDatabase>) -> StreamWorker {
    let (_checkpoint_tx, checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_sweep_tx, sweep_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_drain_tx, drain_rx) = tokio::sync::mpsc::unbounded_channel();
    StreamWorker::new(
        db,
        DaemonTelemetryWorkerHandle::new_noop(),
        Arc::new(Notify::new()),
        Arc::new(AtomicBool::new(false)),
        checkpoint_rx,
        sweep_rx,
        drain_rx,
        SweepTriggerGate::new(),
    )
}

#[tokio::test]
async fn test_handle_checkpoint_skips_subagent_sweep_for_non_claude() {
    let tmp = TempDir::new().unwrap();
    let main_transcript = tmp.path().join("sess-abc.jsonl");
    std::fs::File::create(&main_transcript).unwrap();

    let subagents_dir = tmp.path().join("sess-abc").join("subagents");
    std::fs::create_dir_all(&subagents_dir).unwrap();
    let sub = subagents_dir.join("agent-sub1.jsonl");
    let mut f = std::fs::File::create(&sub).unwrap();
    writeln!(f, r#"{{"type":"message"}}"#).unwrap();

    let db = Arc::new(StreamsDatabase::open(tmp.path().join("test.db")).unwrap());
    let mut worker = make_worker(db);
    let notification = CheckpointNotification {
        session_id: "internal-sess-abc".to_string(),
        tool: "copilot".to_string(),
        trace_id: "trace-3".to_string(),
        tool_use_id: None,
        stream_path: main_transcript.clone(),
        stream_format: CheckpointStreamFormat::CopilotEventStreamJsonl,
        repo_work_dir: None,
        external_session_id: "sess-abc".to_string(),
        external_parent_session_id: None,
    };

    worker.handle_validated_checkpoint_notification(
        notification,
        crate::operations::streams::sweep::DiscoveredSession {
            session_id: "internal-sess-abc".to_string(),
            tool: "github-copilot".to_string(),
            stream_path: std::fs::canonicalize(&main_transcript).unwrap(),
            external_session_id: "sess-abc".to_string(),
            external_parent_session_id: None,
        },
    );

    assert_eq!(worker.priority_queue.len(), 1);
    let task = worker.priority_queue.pop().unwrap();
    assert_eq!(task.session_id, "internal-sess-abc");
    assert_eq!(task.tool, "copilot");
}

#[tokio::test]
#[serial_test::serial]
async fn checkpoint_notification_revalidates_session_binding_before_enqueue() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("pi-sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let transcript = sessions.join("2026-07-25_pi-session.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"session\",\"version\":3,\"id\":\"pi-session\"}\n",
    )
    .unwrap();
    let restore = EnvRestore {
        key: "PI_CODING_AGENT_SESSION_DIR",
        value: std::env::var_os("PI_CODING_AGENT_SESSION_DIR"),
    };
    unsafe {
        std::env::set_var("PI_CODING_AGENT_SESSION_DIR", &sessions);
    }

    let session_id = generate_session_id("pi-session", "pi");
    let source = CheckpointStreamSource {
        path: transcript.clone(),
        format: CheckpointStreamFormat::PiJsonl,
        session_id: session_id.clone(),
        external_session_id: "pi-session".to_string(),
        external_parent_session_id: None,
    };
    crate::operations::streams::agent::get_agent("pi")
        .unwrap()
        .validate_checkpoint_stream(&source)
        .expect("candidate should be valid at ingress");

    std::fs::write(
        &transcript,
        "{\"type\":\"session\",\"version\":3,\"id\":\"replaced-session\"}\n",
    )
    .unwrap();
    let db = Arc::new(StreamsDatabase::open(tmp.path().join("test.db")).unwrap());
    let mut worker = make_worker(db);
    worker
        .handle_checkpoint_notification(CheckpointNotification {
            session_id,
            tool: "pi".to_string(),
            trace_id: "trace-pi".to_string(),
            tool_use_id: None,
            stream_path: transcript,
            stream_format: CheckpointStreamFormat::PiJsonl,
            repo_work_dir: Some(tmp.path().to_path_buf()),
            external_session_id: "pi-session".to_string(),
            external_parent_session_id: None,
        })
        .await;

    assert!(
        worker.priority_queue.is_empty(),
        "a candidate changed after ingress validation must not be enqueued"
    );
    drop(restore);
}

#[tokio::test]
async fn test_copilot_checkpoint_enqueues_shared_otel_stream_immediately() {
    let tmp = TempDir::new().unwrap();
    let user_dir = tmp.path().join("User");
    let transcript_dir = user_dir
        .join("workspaceStorage")
        .join("workspace-hash")
        .join("GitHub.copilot-chat")
        .join("transcripts");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join("sess-otel.jsonl");
    let mut f = std::fs::File::create(&transcript).unwrap();
    writeln!(f, r#"{{"type":"session.start"}}"#).unwrap();

    let otel_dir = user_dir.join("globalStorage").join("github.copilot-chat");
    std::fs::create_dir_all(&otel_dir).unwrap();
    let otel_db = otel_dir.join("agent-traces.db");
    std::fs::File::create(&otel_db).unwrap();
    let canonical_otel_db = std::fs::canonicalize(&otel_db).unwrap();

    let repo_work_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_work_dir).unwrap();

    let db = Arc::new(StreamsDatabase::open(tmp.path().join("test.db")).unwrap());
    let mut worker = make_worker(db.clone());
    let notification = CheckpointNotification {
        session_id: "internal-sess-otel".to_string(),
        tool: "github-copilot".to_string(),
        trace_id: "trace-otel".to_string(),
        tool_use_id: Some("tool-1".to_string()),
        stream_path: transcript.clone(),
        stream_format: CheckpointStreamFormat::CopilotEventStreamJsonl,
        repo_work_dir: Some(repo_work_dir),
        external_session_id: "sess-otel".to_string(),
        external_parent_session_id: None,
    };

    worker.handle_validated_checkpoint_notification(
        notification,
        crate::operations::streams::sweep::DiscoveredSession {
            session_id: "internal-sess-otel".to_string(),
            tool: "github-copilot".to_string(),
            stream_path: std::fs::canonicalize(&transcript).unwrap(),
            external_session_id: "sess-otel".to_string(),
            external_parent_session_id: None,
        },
    );

    let tasks: Vec<_> = worker.priority_queue.iter().collect();
    assert_eq!(tasks.len(), 2);
    let transcript_task = tasks
        .iter()
        .find(|task| task.stream_kind == "transcript")
        .unwrap();
    let otel_task = tasks
        .iter()
        .find(|task| task.stream_kind == "otel_traces")
        .unwrap();

    assert_eq!(transcript_task.priority, Priority::Immediate);
    assert_eq!(transcript_task.session_id, "internal-sess-otel");
    assert_eq!(otel_task.priority, Priority::Immediate);
    assert_eq!(
        otel_task.session_id,
        crate::operations::streams::agent::SHARED_STREAM_SESSION_ID
    );
    assert_eq!(otel_task.canonical_path, canonical_otel_db);

    let otel_record = db
        .get_stream(
            crate::operations::streams::agent::SHARED_STREAM_SESSION_ID,
            "otel_traces",
            &canonical_otel_db.display().to_string(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(otel_record.tool, "github-copilot");
    assert_eq!(otel_record.stream_format, StreamFormat::CopilotOtelSqlite);
    assert_eq!(otel_record.watermark_type, WatermarkType::TimestampCursor);
    assert_eq!(otel_record.external_session_id, "");
    assert_eq!(otel_record.external_parent_session_id, None);
    assert_eq!(otel_record.repo_work_dir, None);
}

#[tokio::test]
#[serial_test::serial]
async fn validated_opencode_checkpoint_reads_external_session_rows() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let git_status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo_dir)
        .status()
        .expect("git init should run");
    assert!(git_status.success());
    let canonical_repo = std::fs::canonicalize(&repo_dir).unwrap();

    let storage_dir = tmp.path().join("opencode");
    std::fs::create_dir_all(&storage_dir).unwrap();
    let database_path = storage_dir.join("opencode.db");
    let connection =
        crate::model::repository::sqlite::open_with_memory_limits(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            INSERT INTO session (id, parent_id)
                VALUES ('external-opencode-session', NULL);
            INSERT INTO message (id, session_id, time_created, time_updated, data)
                VALUES (
                    'message-1',
                    'external-opencode-session',
                    1000,
                    1000,
                    '{\"role\":\"user\",\"content\":\"hello\"}'
                );",
        )
        .unwrap();
    drop(connection);

    let storage_restore = EnvRestore {
        key: "GIT_AI_OPENCODE_STORAGE_PATH",
        value: std::env::var_os("GIT_AI_OPENCODE_STORAGE_PATH"),
    };
    let config_restore = EnvRestore {
        key: "GIT_AI_TEST_CONFIG_PATCH",
        value: std::env::var_os("GIT_AI_TEST_CONFIG_PATCH"),
    };
    unsafe {
        std::env::set_var("GIT_AI_OPENCODE_STORAGE_PATH", &storage_dir);
        std::env::set_var(
            "GIT_AI_TEST_CONFIG_PATCH",
            serde_json::json!({
                "allowed_repositories": [canonical_repo.to_string_lossy()]
            })
            .to_string(),
        );
    }

    let external_session_id = "external-opencode-session";
    let internal_session_id = generate_session_id(external_session_id, "opencode");
    let db = Arc::new(StreamsDatabase::open(tmp.path().join("streams.db")).unwrap());
    let mut worker = make_worker(db.clone());
    worker
        .handle_checkpoint_notification(CheckpointNotification {
            session_id: internal_session_id.clone(),
            tool: "opencode".to_string(),
            trace_id: "trace-opencode".to_string(),
            tool_use_id: None,
            stream_path: database_path,
            stream_format: CheckpointStreamFormat::OpenCodeSqlite,
            repo_work_dir: Some(canonical_repo),
            external_session_id: external_session_id.to_string(),
            external_parent_session_id: None,
        })
        .await;

    let task = worker
        .priority_queue
        .pop()
        .expect("validated checkpoint should enqueue its transcript");
    assert_eq!(task.session_id, internal_session_id);
    let path = task.canonical_path.display().to_string();
    let initial_watermark = db
        .get_stream(&task.session_id, &task.stream_kind, &path)
        .unwrap()
        .unwrap()
        .watermark_value;

    StreamWorker::process_session_blocking(
        &db,
        &DaemonTelemetryWorkerHandle::new_noop(),
        &task,
        &AtomicBool::new(false),
    )
    .expect("worker should read the external OpenCode session row");

    let processed = db
        .get_stream(&task.session_id, &task.stream_kind, &path)
        .unwrap()
        .unwrap();
    assert_eq!(processed.session_id, internal_session_id);
    assert_eq!(processed.external_session_id, external_session_id);
    assert_ne!(
        processed.watermark_value, initial_watermark,
        "reading the external-keyed row must advance the internal stream watermark"
    );

    drop(config_restore);
    drop(storage_restore);
}
