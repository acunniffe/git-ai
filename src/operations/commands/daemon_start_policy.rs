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
    pub fn has_strong_marker(&self) -> bool {
        self.cursor_sandbox.is_some()
            || self.sandbox_runtime.is_some()
            || self.codex_sandbox.is_some()
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
    if markers.has_strong_marker() && outbox_supported {
        return DaemonStartDecision::PublishOutbox(StartBlockReason::StrongSandboxMarker);
    }
    DaemonStartDecision::StartDetached
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
    fn guard_stays_disabled_without_outbox_support() {
        assert_eq!(
            decide_daemon_start(false, false, false, &strong_markers()),
            DaemonStartDecision::StartDetached
        );
        assert_eq!(
            decide_daemon_start(false, true, false, &SandboxMarkers::default()),
            DaemonStartDecision::Unavailable(StartBlockReason::AutoStartDisabled)
        );
    }
}
