#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartBlockReason {
    AutoStartDisabled,
    StrongSandboxMarker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStartDecision {
    UseExisting,
    StartDetached,
    PublishOutbox(StartBlockReason),
    Unavailable(StartBlockReason),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxMarkers {
    pub cursor_sandbox: Option<String>,
    pub sandbox_runtime: Option<String>,
    pub codex_sandbox: Option<String>,
    pub codex_network_disabled: Option<String>,
}

impl SandboxMarkers {
    pub const ENV_VARS: [&'static str; 4] = [
        "CURSOR_SANDBOX",
        "SANDBOX_RUNTIME",
        "CODEX_SANDBOX",
        "CODEX_SANDBOX_NETWORK_DISABLED",
    ];

    pub fn from_env() -> Self {
        Self {
            cursor_sandbox: std::env::var("CURSOR_SANDBOX").ok(),
            sandbox_runtime: std::env::var("SANDBOX_RUNTIME").ok(),
            codex_sandbox: std::env::var("CODEX_SANDBOX").ok(),
            codex_network_disabled: std::env::var("CODEX_SANDBOX_NETWORK_DISABLED").ok(),
        }
    }

    pub fn strong_marker_name(&self) -> Option<&'static str> {
        if self.cursor_sandbox.is_some() {
            Some("CURSOR_SANDBOX")
        } else if self.sandbox_runtime.is_some() {
            Some("SANDBOX_RUNTIME")
        } else if self.codex_sandbox.is_some() {
            Some("CODEX_SANDBOX")
        } else {
            None
        }
    }

    pub fn has_strong_marker(&self) -> bool {
        self.strong_marker_name().is_some()
    }
}

pub fn decide_daemon_start(
    daemon_reachable: bool,
    auto_start_disabled: bool,
    outbox_supported: bool,
    markers: &SandboxMarkers,
) -> DaemonStartDecision {
    if daemon_reachable {
        return DaemonStartDecision::UseExisting;
    }
    if auto_start_disabled {
        return if outbox_supported {
            DaemonStartDecision::PublishOutbox(StartBlockReason::AutoStartDisabled)
        } else {
            DaemonStartDecision::Unavailable(StartBlockReason::AutoStartDisabled)
        };
    }
    if markers.has_strong_marker() {
        return if outbox_supported {
            DaemonStartDecision::PublishOutbox(StartBlockReason::StrongSandboxMarker)
        } else {
            DaemonStartDecision::Unavailable(StartBlockReason::StrongSandboxMarker)
        };
    }
    DaemonStartDecision::StartDetached
}

fn blocked_message(reason: StartBlockReason, markers: &SandboxMarkers) -> String {
    match reason {
        StartBlockReason::AutoStartDisabled => {
            "daemon auto-spawn disabled (_GITAI_INTERNAL_DISABLE_WRAPPER_DAEMON_AUTOSPAWN)"
                .to_string()
        }
        StartBlockReason::StrongSandboxMarker => {
            let marker = markers.strong_marker_name().unwrap_or("sandbox marker");
            format!(
                "daemon startup blocked inside a sandbox ({marker} is set); run the daemon outside the sandbox or use checkpoint delivery outbox"
            )
        }
    }
}

pub fn require_detached_start_allowed(markers: &SandboxMarkers) -> Result<(), String> {
    match decide_daemon_start(false, false, false, markers) {
        DaemonStartDecision::StartDetached | DaemonStartDecision::UseExisting => Ok(()),
        DaemonStartDecision::PublishOutbox(reason) | DaemonStartDecision::Unavailable(reason) => {
            Err(blocked_message(reason, markers))
        }
    }
}

pub fn should_auto_start_detached(
    auto_start_disabled: bool,
    markers: &SandboxMarkers,
) -> Result<bool, String> {
    match decide_daemon_start(false, auto_start_disabled, true, markers) {
        DaemonStartDecision::UseExisting => Ok(false),
        DaemonStartDecision::StartDetached => Ok(true),
        DaemonStartDecision::PublishOutbox(reason) | DaemonStartDecision::Unavailable(reason) => {
            Err(blocked_message(reason, markers))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strong_markers() -> SandboxMarkers {
        SandboxMarkers {
            cursor_sandbox: Some("1".to_string()),
            sandbox_runtime: Some("seatbelt".to_string()),
            codex_sandbox: Some("seatbelt".to_string()),
            codex_network_disabled: Some("1".to_string()),
        }
    }

    #[test]
    fn reachable_daemon_always_wins_over_sandbox_markers() {
        assert_eq!(
            decide_daemon_start(true, true, true, &strong_markers()),
            DaemonStartDecision::UseExisting
        );
    }

    #[test]
    fn explicit_auto_start_disable_uses_outbox() {
        assert_eq!(
            decide_daemon_start(false, true, true, &SandboxMarkers::default()),
            DaemonStartDecision::PublishOutbox(StartBlockReason::AutoStartDisabled)
        );
    }

    #[test]
    fn each_strong_marker_uses_outbox() {
        for markers in [
            SandboxMarkers {
                cursor_sandbox: Some("1".to_string()),
                ..Default::default()
            },
            SandboxMarkers {
                sandbox_runtime: Some("sandbox-exec".to_string()),
                ..Default::default()
            },
            SandboxMarkers {
                codex_sandbox: Some("seatbelt".to_string()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                decide_daemon_start(false, false, true, &markers),
                DaemonStartDecision::PublishOutbox(StartBlockReason::StrongSandboxMarker)
            );
        }
    }

    #[test]
    fn network_disabled_alone_is_not_a_spawn_ban() {
        let markers = SandboxMarkers {
            codex_network_disabled: Some("1".to_string()),
            ..Default::default()
        };

        assert_eq!(
            decide_daemon_start(false, false, true, &markers),
            DaemonStartDecision::StartDetached
        );
    }

    #[test]
    fn reports_the_first_strong_marker_without_exposing_its_value() {
        let markers = SandboxMarkers {
            cursor_sandbox: Some("secret-value".to_string()),
            sandbox_runtime: Some("seatbelt".to_string()),
            ..Default::default()
        };

        assert_eq!(markers.strong_marker_name(), Some("CURSOR_SANDBOX"));
    }

    #[test]
    fn guard_stays_disabled_without_outbox_support() {
        assert_eq!(
            decide_daemon_start(false, false, false, &strong_markers()),
            DaemonStartDecision::Unavailable(StartBlockReason::StrongSandboxMarker)
        );
        assert_eq!(
            decide_daemon_start(false, true, false, &SandboxMarkers::default()),
            DaemonStartDecision::Unavailable(StartBlockReason::AutoStartDisabled)
        );
    }
}
