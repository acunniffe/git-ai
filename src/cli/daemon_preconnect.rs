/// Whether a command currently requires the process-wide daemon telemetry
/// connection before command dispatch.
///
/// P0 intentionally preserves the existing `checkpoint` behavior so its
/// failure mode is characterized before the delivery refactor changes it.
pub(crate) fn command_requires_daemon_preconnect(command: &str) -> bool {
    !matches!(
        command,
        "help"
            | "--help"
            | "-h"
            | "version"
            | "--version"
            | "-v"
            | "config"
            | "bg"
            | "d"
            | "daemon"
            | "debug"
            | "upgrade"
            | "install-hooks"
            | "install"
            | "uninstall-hooks"
            | "usage"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_preconnect_characterizes_current_failure_path() {
        assert!(command_requires_daemon_preconnect("checkpoint"));
    }

    #[test]
    fn recovery_commands_do_not_require_preconnect() {
        for command in ["help", "config", "bg", "debug", "upgrade"] {
            assert!(!command_requires_daemon_preconnect(command));
        }
    }

    #[test]
    fn daemon_backed_read_command_requires_preconnect() {
        assert!(command_requires_daemon_preconnect("status"));
    }
}
