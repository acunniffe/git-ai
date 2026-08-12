use super::*;

static DAEMON_SYNC_REGISTRY: OnceLock<Mutex<DaemonSyncRegistry>> = OnceLock::new();

static TEST_SYNC_SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn new_daemon_test_sync_session_id() -> String {
    let id = TEST_SYNC_SESSION_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("test-sync-{}-{}", std::process::id(), id)
}

#[derive(Debug, Default)]
pub(super) struct DaemonSyncRegistry {
    last_synced_completion_count: HashMap<String, u64>,
    pending_sessions: HashMap<String, Vec<String>>,
    /// Number of checkpoint completions we expect the daemon to have processed.
    /// Unlike session tracking (which uses session IDs), checkpoint completions
    /// are tracked by counting entries with `kind == "checkpoint"` in the
    /// completion log.
    expected_checkpoint_count: HashMap<String, u64>,
    last_synced_checkpoint_count: HashMap<String, u64>,
}

fn count_for(map: &HashMap<String, u64>, family_key: &str) -> u64 {
    map.get(family_key).copied().unwrap_or_default()
}

fn advance_to(map: &mut HashMap<String, u64>, family_key: &str, count: u64) {
    let entry = map.entry(family_key.to_string()).or_default();
    *entry = (*entry).max(count);
}

impl DaemonSyncRegistry {
    pub(super) fn pending_sessions(&self, family_key: &str) -> Vec<String> {
        self.pending_sessions
            .get(family_key)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn expected_checkpoint_count(&self, family_key: &str) -> u64 {
        count_for(&self.expected_checkpoint_count, family_key)
    }

    pub(super) fn last_synced_checkpoint_count(&self, family_key: &str) -> u64 {
        count_for(&self.last_synced_checkpoint_count, family_key)
    }

    pub(super) fn last_synced_completion_count(&self, family_key: &str) -> u64 {
        count_for(&self.last_synced_completion_count, family_key)
    }

    pub(super) fn record_expected_completion_session(&mut self, family_key: &str, session: &str) {
        self.pending_sessions
            .entry(family_key.to_string())
            .or_default()
            .push(session.to_string());
    }

    pub(super) fn raise_expected_checkpoint_count(&mut self, family_key: &str, count: u64) {
        *self
            .expected_checkpoint_count
            .entry(family_key.to_string())
            .or_default() += count;
    }

    pub(super) fn advance_last_synced_checkpoint_count(
        &mut self,
        family_key: &str,
        checkpoint_count: u64,
    ) {
        advance_to(
            &mut self.last_synced_checkpoint_count,
            family_key,
            checkpoint_count,
        );
    }

    pub(super) fn advance_last_synced_completion_count(
        &mut self,
        family_key: &str,
        completion_count: u64,
    ) {
        advance_to(
            &mut self.last_synced_completion_count,
            family_key,
            completion_count,
        );
    }

    pub(super) fn pending_work_summary(&self, family_key: &str) -> Option<String> {
        let pending_sessions = self.pending_sessions(family_key);
        let expected_checkpoints = self.expected_checkpoint_count(family_key);
        let last_synced_checkpoints = self.last_synced_checkpoint_count(family_key);
        let pending_checkpoints = expected_checkpoints.saturating_sub(last_synced_checkpoints);

        if pending_sessions.is_empty() && pending_checkpoints == 0 {
            return None;
        }

        Some(format!(
            "{} pending command session(s), {} pending checkpoint completion(s)",
            pending_sessions.len(),
            pending_checkpoints
        ))
    }
}

pub(super) fn daemon_sync_registry() -> &'static Mutex<DaemonSyncRegistry> {
    DAEMON_SYNC_REGISTRY.get_or_init(|| Mutex::new(DaemonSyncRegistry::default()))
}

pub(super) fn git_ai_primary_command<'a>(args: &'a [&'a str]) -> Option<&'a str> {
    args.iter().copied().find(|arg| !arg.starts_with('-'))
}

pub(super) fn is_known_checkpoint_preset(arg: &str) -> bool {
    matches!(
        arg,
        "claude"
            | "codex"
            | "continue-cli"
            | "cursor"
            | "gemini"
            | "github-copilot"
            | "amp"
            | "windsurf"
            | "opencode"
            | "pi"
            | "ai_tab"
            | "firebender"
            | "mock_ai"
            | "mock_known_human"
            | "known_human"
            | "droid"
            | "agent-v1"
    )
}

pub(super) fn normalize_test_git_ai_checkpoint_args(args: &[&str]) -> Vec<String> {
    let original = args
        .iter()
        .map(|arg| (*arg).to_string())
        .collect::<Vec<_>>();
    if git_ai_primary_command(args) != Some("checkpoint") || args.len() <= 1 {
        return original;
    }

    if args.contains(&"--") {
        return original;
    }

    let mut normalized = vec![args[0].to_string()];
    let mut i = 1usize;
    while i < args.len() {
        match args[i] {
            "--hook-input" => {
                normalized.push(args[i].to_string());
                if let Some(value) = args.get(i + 1) {
                    normalized.push((*value).to_string());
                }
                i += 2;
            }
            arg if arg.starts_with("--hook-input=") || arg.starts_with('-') => {
                normalized.push(arg.to_string());
                i += 1;
            }
            arg if is_known_checkpoint_preset(arg) => return original,
            _ => {
                normalized.push("--".to_string());
                normalized.extend(args[i..].iter().map(|arg| (*arg).to_string()));
                return normalized;
            }
        }
    }

    normalized
}

pub(super) fn parse_checkpoint_request_count(stdout: &str) -> u64 {
    for line in stdout.lines() {
        if let Some(val) = line.strip_prefix("checkpoint_requests=") {
            return val.trim().parse().unwrap_or(0);
        }
    }
    0
}

pub(super) fn git_ai_command_requires_daemon_sync(args: &[&str]) -> bool {
    matches!(
        git_ai_primary_command(args),
        Some(
            "blame"
                | "blame-analysis"
                | "diff"
                | "log"
                | "show"
                | "show-prompt"
                | "stats"
                | "status"
                | "await"
        )
    )
}

pub(super) fn git_invocation_requires_daemon_sync(invocation: &ParsedGitInvocation) -> bool {
    matches!(invocation.command.as_deref(), Some("notes"))
}

pub(super) fn git_invocation_routes_to_clone_target(invocation: &ParsedGitInvocation) -> bool {
    invocation.command.as_deref() == Some("clone")
}

pub(super) fn clone_target_path(invocation: &ParsedGitInvocation, cwd: &Path) -> Option<PathBuf> {
    if invocation.command.as_deref() != Some("clone") {
        return None;
    }
    let target = extract_clone_target_directory(&invocation.command_args)?;
    let target_path = PathBuf::from(target);
    let resolved = if target_path.is_absolute() {
        target_path
    } else {
        cwd.join(target_path)
    };
    Some(resolved.canonicalize().unwrap_or(resolved))
}

pub(super) fn env_explicitly_enables_trace2(envs: &[(&str, &str)]) -> bool {
    envs.iter().any(|(key, value)| {
        matches!(*key, "GIT_TRACE2" | "GIT_TRACE2_EVENT" | "GIT_TRACE2_PERF")
            && !matches!(*value, "" | "0")
    })
}

impl TestRepo {
    pub(super) fn daemon_family_key_for_repo_path(&self, repo_path: &Path) -> String {
        let repo = GitAiRepository::find_repository_in_path(repo_path.to_str().unwrap())
            .unwrap_or_else(|e| {
                panic!(
                    "failed to resolve daemon family key for {}: {}",
                    repo_path.display(),
                    e
                )
            });
        let common_dir = repo
            .common_dir()
            .canonicalize()
            .unwrap_or_else(|_| repo.common_dir().to_path_buf());
        common_dir.to_string_lossy().to_string()
    }

    pub(super) fn maybe_daemon_family_key_for_repo_path(&self, repo_path: &Path) -> Option<String> {
        let lookup_path = if repo_path.is_dir() {
            repo_path.to_path_buf()
        } else {
            repo_path.parent()?.to_path_buf()
        };
        let repo = GitAiRepository::find_repository_in_path(lookup_path.to_str()?).ok()?;
        let common_dir = repo
            .common_dir()
            .canonicalize()
            .unwrap_or_else(|_| repo.common_dir().to_path_buf());
        Some(common_dir.to_string_lossy().to_string())
    }

    pub(super) fn daemon_family_key(&self) -> String {
        self.daemon_family_key
            .get_or_init(|| self.daemon_family_key_for_repo_path(&self.path))
            .clone()
    }

    pub(super) fn resolve_checkpoint_family_keys_from_args(
        &self,
        args: &[&str],
    ) -> HashMap<String, u64> {
        // checkpoint args: ["checkpoint", "<preset>", "<file_path>", ...]
        // Group file paths by their repo family key. The orchestrator creates
        // one CheckpointRequest per distinct repo, so each family gets count=1.
        let mut families: HashMap<String, u64> = HashMap::new();
        if args.len() >= 3 {
            for arg in &args[2..] {
                let candidate = std::path::Path::new(arg);
                if candidate.is_absolute()
                    && let Some(key) = self.maybe_daemon_family_key_for_repo_path(candidate)
                {
                    families.entry(key).or_insert(0);
                    continue;
                }
            }
        }
        if families.is_empty() {
            families.insert(self.daemon_family_key(), 0);
        }
        for val in families.values_mut() {
            *val = 1;
        }
        families
    }

    pub(super) fn record_daemon_family_expected_completion_session(&self, session: &str) {
        if !self.has_active_daemon() {
            return;
        }

        let family_key = self.daemon_family_key();
        let mut registry = daemon_sync_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.record_expected_completion_session(&family_key, session);
    }

    pub(super) fn record_pending_checkpoint_completions(&self, count: u64) {
        let family_key = self.daemon_family_key();
        let mut registry = daemon_sync_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.raise_expected_checkpoint_count(&family_key, count);
    }

    pub(super) fn append_daemon_test_sync_session_args(
        &self,
        args: &mut Vec<String>,
        session: &str,
    ) {
        if !self.has_active_daemon() {
            return;
        }

        args.push("-c".to_string());
        args.push(format!(
            "{}={}",
            git_ai::operations::daemon::test_sync::TEST_SYNC_SESSION_CONFIG_KEY,
            session
        ));
    }

    pub(super) fn checkpoint_path_args<'a>(&self, args: &'a [&'a str]) -> Vec<&'a str> {
        if git_ai_primary_command(args) != Some("checkpoint") {
            return Vec::new();
        }

        let mut candidates = Vec::new();
        let mut i = 1usize;
        let mut seen_separator = false;
        while i < args.len() {
            let arg = args[i];
            if seen_separator {
                candidates.push(arg);
                i += 1;
                continue;
            }

            match arg {
                "--" => {
                    seen_separator = true;
                    i += 1;
                }
                "--hook-input" => {
                    i += 2;
                }
                _ if arg.starts_with("--hook-input=") || arg.starts_with('-') => {
                    i += 1;
                }
                _ if i == 1 && is_known_checkpoint_preset(arg) => {
                    i += 1;
                }
                _ => {
                    candidates.push(arg);
                    i += 1;
                }
            }
        }

        candidates
    }

    pub(crate) fn sync_daemon_force(&self) {
        if !self.has_active_daemon() {
            return;
        }

        let family_key = self.daemon_family_key();
        self.sync_daemon_family(&self.path);
        self.sync_pending_daemon_sessions(&family_key);
        self.sync_daemon_family(&self.path);
    }

    pub(crate) fn sync_daemon_external_completion_sessions(&self, sessions: &[String]) {
        if !self.has_active_daemon() || sessions.is_empty() {
            return;
        }

        for session in sessions {
            self.record_daemon_family_expected_completion_session(session);
        }
        self.sync_daemon_force();
    }

    pub(super) fn sync_daemon_clone_target(&self, target_repo_path: &Path) {
        if !self.has_active_daemon() {
            return;
        }

        let family_key = self.daemon_family_key_for_repo_path(target_repo_path);
        let baseline_count = {
            let registry = daemon_sync_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.last_synced_completion_count(&family_key)
        };
        let observed_count = self.wait_for_daemon_completion_count(
            &family_key,
            baseline_count,
            baseline_count.saturating_add(1),
        );
        let mut registry = daemon_sync_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.advance_last_synced_completion_count(&family_key, observed_count);
        self.sync_daemon_family(target_repo_path);
    }

    pub(super) fn sync_daemon_family(&self, repo_path: &Path) {
        let repo_working_dir = repo_path
            .canonicalize()
            .unwrap_or_else(|_| repo_path.to_path_buf())
            .to_string_lossy()
            .to_string();
        let start = Instant::now();
        loop {
            match send_control_request(
                &self.daemon_control_socket_path(),
                &ControlRequest::SyncFamily {
                    repo_working_dir: repo_working_dir.clone(),
                },
            ) {
                Ok(response) if response.ok => return,
                Ok(response) => {
                    panic!(
                        "daemon sync.family failed: {}",
                        response
                            .error
                            .unwrap_or_else(|| "unknown daemon error".to_string())
                    );
                }
                Err(error) if start.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(25));
                    let _ = error;
                }
                Err(error) => panic!("daemon sync.family failed: {}", error),
            }
        }
    }

    pub(super) fn sync_pending_daemon_sessions(&self, family_key: &str) {
        let (pending_sessions, expected_checkpoints) = {
            let registry = daemon_sync_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                registry.pending_sessions(family_key),
                registry.expected_checkpoint_count(family_key),
            )
        };

        if !pending_sessions.is_empty() {
            let observed_count =
                self.wait_for_daemon_completion_sessions(family_key, &pending_sessions);
            let mut registry = daemon_sync_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.pending_sessions.remove(family_key);
            registry.advance_last_synced_completion_count(family_key, observed_count);
        }

        if expected_checkpoints > 0 {
            let last_synced = {
                let registry = daemon_sync_registry()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry.last_synced_checkpoint_count(family_key)
            };
            if expected_checkpoints > last_synced {
                let observed_checkpoint_count =
                    self.wait_for_daemon_checkpoint_count(family_key, expected_checkpoints);
                let mut registry = daemon_sync_registry()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry
                    .advance_last_synced_checkpoint_count(family_key, observed_checkpoint_count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_target_path_uses_resolved_clone_invocation() {
        let repo = TestRepo::new();
        repo.git(&["config", "alias.copy", "clone"]).unwrap();
        let invocation = repo.parsed_git_invocation_for_tracking(
            &["copy", "source", "nested/target"],
            Some(repo.path().as_path()),
        );

        assert_eq!(
            clone_target_path(&invocation, repo.path().as_path()),
            Some(repo.path().join("nested/target"))
        );
    }

    #[test]
    fn test_normalize_test_git_ai_checkpoint_args_inserts_separator_for_direct_file() {
        assert_eq!(
            normalize_test_git_ai_checkpoint_args(&["checkpoint", "src/lib.rs"]),
            vec!["checkpoint", "--", "src/lib.rs"]
        );
    }

    #[test]
    fn test_normalize_test_git_ai_checkpoint_args_preserves_known_presets_and_separator() {
        assert_eq!(
            normalize_test_git_ai_checkpoint_args(&["checkpoint", "mock_ai", "src/lib.rs"]),
            vec!["checkpoint", "mock_ai", "src/lib.rs"]
        );
        assert_eq!(
            normalize_test_git_ai_checkpoint_args(&["checkpoint", "--", "src/lib.rs"]),
            vec!["checkpoint", "--", "src/lib.rs"]
        );
    }

    #[test]
    fn test_normalize_test_git_ai_checkpoint_args_handles_hook_input_before_pathspecs() {
        assert_eq!(
            normalize_test_git_ai_checkpoint_args(&[
                "checkpoint",
                "--hook-input",
                "{\"cwd\":\"/tmp/repo\"}",
                "src/lib.rs",
                "src/main.rs",
            ]),
            vec![
                "checkpoint",
                "--hook-input",
                "{\"cwd\":\"/tmp/repo\"}",
                "--",
                "src/lib.rs",
                "src/main.rs",
            ]
        );
    }
}
