use crate::authorship::authorship_log::{LineRange, SessionRecord};
use crate::authorship::authorship_log_serialization::{
    AttestationEntry, AuthorshipLog, generate_session_id, generate_trace_id,
};
use crate::authorship::working_log::AgentId;
use std::collections::{HashMap, HashSet};

const DEVIN_ID_PATH: &str = "/opt/.devin/devin_id";
const DEVIN_DIR_PATH: &str = "/opt/.devin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundAgent {
    WithHooks { tool: String },
    NoHooks { tool: String, id: String },
    None,
}

pub fn detect() -> BackgroundAgent {
    // With-hooks agents are explicitly declared and take precedence over
    // directory-based no-hooks detection.
    if std::env::var("CLAUDE_CODE_REMOTE")
        .map(|v| v == "true")
        .unwrap_or(false)
    {
        return BackgroundAgent::WithHooks {
            tool: "claude-web".to_string(),
        };
    }

    if std::env::var("HOSTNAME")
        .map(|v| v == "cursor")
        .unwrap_or(false)
        && std::env::var("CURSOR_AGENT")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        return BackgroundAgent::WithHooks {
            tool: "cursor-agent".to_string(),
        };
    }

    // No-hooks background agents declared via environment variables.
    if std::env::vars().any(|(k, _)| k.starts_with("CLOUD_AGENT_")) {
        return BackgroundAgent::NoHooks {
            tool: "cloud-agent".to_string(),
            id: placeholder_id("CLOUD_AGENT"),
        };
    }

    if std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        .map(|v| v == "codex_web_agent")
        .unwrap_or(false)
    {
        let id = std::env::var("CODEX_THREAD_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| placeholder_id("CODEX_CLOUD"));
        return BackgroundAgent::NoHooks {
            tool: "codex-cloud".to_string(),
            id,
        };
    }

    if std::env::var("COPILOT_AGENT_COMMIT_LOGIN")
        .map(|v| v == "copilot-swe-agent[bot]")
        .unwrap_or(false)
        && std::env::var("GITHUB_JOB")
            .map(|v| v == "copilot")
            .unwrap_or(false)
    {
        let id = std::env::var("COPILOT_AGENT_SESSION_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| placeholder_id("GITHUB_COPILOT_AGENT"));
        return BackgroundAgent::NoHooks {
            tool: "github-copilot-agent".to_string(),
            id,
        };
    }

    if std::env::var("GIT_AI_CLOUD_AGENT")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return BackgroundAgent::NoHooks {
            tool: "git-ai-cloud-agent".to_string(),
            id: placeholder_id("GIT_AI_CLOUD_AGENT"),
        };
    }

    // Directory-based Devin detection can only be active when the git-ai daemon
    // is running a real user session (not a test suite). Test suites set
    // GIT_AI_TEST_DB_PATH for spawned commands/daemons, so skip /opt/.devin
    // when that marker is present.
    if std::env::var_os("GIT_AI_TEST_DB_PATH").is_none()
        && std::env::var_os("GITAI_TEST_DB_PATH").is_none()
        && std::path::Path::new(DEVIN_DIR_PATH).is_dir()
    {
        let id = std::fs::read_to_string(DEVIN_ID_PATH)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| placeholder_id("DEVIN"));
        return BackgroundAgent::NoHooks {
            tool: "devin".to_string(),
            id,
        };
    }

    BackgroundAgent::None
}

/// If running in a no-hooks background agent, attribute any committed lines
/// that have no existing attestation ("holes") to the detected agent.
/// Existing attributions (human, other AI) are preserved.
/// Returns true if any attribution was applied.
pub fn fill_unattributed_lines(
    authorship_log: &mut AuthorshipLog,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    human_author: &str,
) -> bool {
    let BackgroundAgent::NoHooks { tool, id } = detect() else {
        return false;
    };

    if committed_hunks.is_empty() {
        return false;
    }

    // Collect already-attributed lines per file
    let mut attributed_lines: HashMap<&str, HashSet<u32>> = HashMap::new();
    for file_attestation in &authorship_log.attestations {
        let lines = attributed_lines
            .entry(&file_attestation.file_path)
            .or_default();
        for entry in &file_attestation.entries {
            for range in &entry.line_ranges {
                for line in range.expand() {
                    lines.insert(line);
                }
            }
        }
    }

    // Find unattributed lines per file
    let mut unattributed_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    for (file_path, line_ranges) in committed_hunks {
        let existing = attributed_lines.get(file_path.as_str());
        let mut unattributed: Vec<u32> = Vec::new();
        for range in line_ranges {
            for line in range.expand() {
                if existing.is_none_or(|set| !set.contains(&line)) {
                    unattributed.push(line);
                }
            }
        }
        if !unattributed.is_empty() {
            unattributed.sort();
            unattributed_hunks.insert(file_path.clone(), LineRange::compress_lines(&unattributed));
        }
    }

    if unattributed_hunks.is_empty() {
        return false;
    }

    let agent_id = AgentId {
        tool: tool.clone(),
        id: id.clone(),
        model: "unknown".to_string(),
    };

    let session_key = generate_session_id(&id, &tool);
    let trace_id = generate_trace_id();
    let attestation_hash = format!("{}::{}", session_key, trace_id);

    authorship_log.metadata.sessions.insert(
        session_key,
        SessionRecord {
            agent_id,
            human_author: Some(human_author.to_string()),
            custom_attributes: None,
        },
    );

    for (file_path, line_ranges) in unattributed_hunks {
        let file_attestation = authorship_log.get_or_create_file(&file_path);
        file_attestation.add_entry(AttestationEntry::new(attestation_hash.clone(), line_ranges));
    }

    true
}

fn placeholder_id(name: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{name}_SESSION{ts}")
}
