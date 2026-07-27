#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SandboxRestriction {
    pub(crate) env_var: &'static str,
    pub(crate) name: &'static str,
}

pub(crate) fn sandbox_restriction() -> Option<SandboxRestriction> {
    [
        ("CURSOR_SANDBOX", "Cursor"),
        ("SANDBOX_RUNTIME", "Claude Code"),
        ("CODEX_SANDBOX", "Codex"),
        ("CODEX_SANDBOX_NETWORK_DISABLED", "Codex"),
    ]
    .into_iter()
    .find(|(env_var, _)| std::env::var_os(env_var).is_some())
    .map(|(env_var, name)| SandboxRestriction { env_var, name })
}

pub(crate) fn ensure_daemon_start_allowed() -> Result<(), String> {
    let Some(restriction) = sandbox_restriction() else {
        return Ok(());
    };

    Err(format!(
        "cannot start the daemon inside the {} sandbox ({} is set)",
        restriction.name, restriction.env_var
    ))
}
