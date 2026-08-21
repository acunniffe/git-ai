//! Wire-shape DTOs for the daemon control socket protocol.
//!
//! These types are serialized as JSON over the control socket.  Serde
//! attributes and field order are preserved byte-for-byte from their original
//! location in `operations/daemon/control_api.rs`.

use crate::model::checkpoint_delivery::CheckpointDelivery;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::model::stat_snapshot::StatSnapshot;
use crate::model::telemetry::TelemetryEnvelope;
use crate::model::working_log::AgentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A request sent to the daemon over the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum ControlRequest {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "checkpoint.run")]
    CheckpointRun { request: Box<CheckpointRequest> },
    #[serde(rename = "checkpoint.deliver")]
    CheckpointDeliver { delivery: Box<CheckpointDelivery> },
    #[serde(rename = "sync.family")]
    SyncFamily { repo_working_dir: String },
    #[serde(rename = "status.family")]
    StatusFamily { repo_working_dir: String },
    #[serde(rename = "telemetry.submit")]
    SubmitTelemetry { envelopes: Vec<TelemetryEnvelope> },
    #[serde(rename = "cas.submit")]
    SubmitCas { records: Vec<CasSyncPayload> },
    /// Signal the daemon that new notes are pending in notes-db and should be flushed.
    #[serde(rename = "notes.flush")]
    FlushNotes,
    #[serde(rename = "snapshot.watermarks")]
    SnapshotWatermarks { repo_working_dir: String },
    #[serde(rename = "bash_session.start")]
    BashSessionStart {
        repo_work_dir: String,
        original_cwd: Option<String>,
        session_id: String,
        tool_use_id: String,
        agent_id: AgentId,
        metadata: HashMap<String, String>,
        stat_snapshot: Box<StatSnapshot>,
        trace_id: String,
        started_at_ns: u128,
        command: Option<String>,
    },
    #[serde(rename = "bash_session.end")]
    BashSessionEnd {
        repo_work_dir: String,
        original_cwd: Option<String>,
        session_id: String,
        tool_use_id: String,
        agent_id: AgentId,
        metadata: HashMap<String, String>,
        trace_id: String,
        ended_at_ns: u128,
        command: Option<String>,
    },
    #[serde(rename = "bash_session.query")]
    BashSessionQuery { repo_work_dir: String },
    #[serde(rename = "bash_snapshot.query")]
    BashSnapshotQuery {
        session_id: String,
        tool_use_id: String,
    },
    #[serde(rename = "bash_hook_attempt.start")]
    BashHookAttemptStart {
        original_cwd: String,
        discovered_repo_work_dir: Option<String>,
        repo_discovery_error: Option<String>,
        session_id: String,
        tool_use_id: String,
        agent_id: AgentId,
        metadata: HashMap<String, String>,
        trace_id: String,
        started_at_ns: u128,
        command: Option<String>,
    },
    #[serde(rename = "bash_hook_attempt.end")]
    BashHookAttemptEnd {
        original_cwd: String,
        discovered_repo_work_dir: Option<String>,
        repo_discovery_error: Option<String>,
        session_id: String,
        tool_use_id: String,
        agent_id: AgentId,
        metadata: HashMap<String, String>,
        trace_id: String,
        ended_at_ns: u128,
        command: Option<String>,
    },
    /// Wait for the daemon to finish all in-flight work and flush telemetry.
    #[serde(rename = "await")]
    Await { timeout_secs: u64 },
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// Response envelope from the daemon for a control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ControlResponse {
    pub fn ok(seq: Option<u64>, data: Option<Value>) -> Self {
        Self {
            ok: true,
            seq,
            data,
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            seq: None,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Response to a `bash_session.query` control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashSessionQueryResponse {
    pub active: bool,
    pub agent_id: Option<AgentId>,
    pub session_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub metadata: Option<HashMap<String, String>>,
}

/// Response to a `bash_snapshot.query` control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashSnapshotQueryResponse {
    pub found: bool,
    pub stat_snapshot: Option<StatSnapshot>,
}

/// Per-family daemon status returned by `status.family`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FamilyStatus {
    pub family_key: String,
    pub latest_seq: u64,
    pub last_error: Option<String>,
}

/// A CAS object payload sent from client to daemon for background upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasSyncPayload {
    pub hash: String,
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::checkpoint_delivery::CheckpointDelivery;
    use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
    use crate::model::working_log::CheckpointKind;

    #[test]
    fn checkpoint_delivery_uses_versioned_control_method() {
        let delivery = CheckpointDelivery::from_requests_at(
            vec![CheckpointRequest {
                trace_id: "trace-1".to_string(),
                checkpoint_kind: CheckpointKind::Human,
                agent_id: None,
                files: Vec::new(),
                path_role: PreparedPathRole::Edited,
                stream_source: None,
                metadata: HashMap::new(),
                delivery_id: None,
            }],
            42,
        )
        .remove(0);

        let request = ControlRequest::CheckpointDeliver {
            delivery: Box::new(delivery.clone()),
        };
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["method"], "checkpoint.deliver");
        assert_eq!(
            value["params"]["delivery"]["delivery_id"],
            delivery.delivery_id
        );
    }
}
