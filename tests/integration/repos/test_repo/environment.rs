use super::*;

pub(super) fn default_test_config_patch() -> ConfigPatch {
    // Collection is opt-in (empty allowlist = collect nothing), so allow the
    // OS temp root: every TestRepo, worktree variant, and mirror lives under
    // it. Canonicalized because repo roots are matched symlink-resolved
    // (macOS: /var/folders -> /private/var/folders).
    let temp_root = std::env::temp_dir();
    let temp_root = temp_root.canonicalize().unwrap_or(temp_root);

    // Pin the git-notes backend: the production default is sqlite, but the
    // bulk of the suite asserts against refs/notes/ai directly.
    // Sqlite-backend behavior is covered by dedicated tests.
    ConfigPatch {
        allowed_repositories: Some(vec![temp_root.to_string_lossy().replace('\\', "/")]),
        exclude_prompts_in_repositories: Some(vec![]), // No exclusions = share everywhere
        prompt_storage: Some("notes".to_string()),     // Use notes mode for tests
        notes_backend: Some(git_ai::config::NotesBackendConfig {
            kind: git_ai::config::NotesBackendKind::GitNotes,
            backend_url: None,
        }),
        ..ConfigPatch::default()
    }
}

pub(super) fn write_config_patch_to_home(patch: &ConfigPatch, home: &Path) {
    let config = patch
        .to_file_config()
        .expect("failed to project test config patch into file config");

    let config_dir = home.join(".git-ai");
    fs::create_dir_all(&config_dir).expect("failed to create test HOME config directory");
    let config_path = config_dir.join("config.json");
    let serialized = serde_json::to_string(&config).expect("failed to serialize test config");
    fs::write(&config_path, serialized).expect("failed to write test HOME config");
}

pub(super) fn dedicated_test_db_path(test_home: &Path) -> PathBuf {
    test_home.join("dedicated-daemon-db")
}

impl TestRepo {
    pub fn set_feature_flags(&mut self, feature_flags: FeatureFlags) {
        self.feature_flags = feature_flags;
    }

    pub(crate) fn daemon_control_socket_path(&self) -> PathBuf {
        self.daemon_process
            .as_ref()
            .map(|daemon| daemon.control_socket_path.clone())
            .unwrap_or_else(|| DaemonProcess::control_socket_path_for_home(&self.test_home))
    }

    pub(crate) fn daemon_home_path(&self) -> PathBuf {
        self.daemon_process
            .as_ref()
            .map(|daemon| daemon.daemon_home.clone())
            .unwrap_or_else(|| self.test_home.clone())
    }

    pub(crate) fn daemon_trace_socket_path(&self) -> PathBuf {
        self.daemon_process
            .as_ref()
            .map(|daemon| daemon.trace_socket_path.clone())
            .unwrap_or_else(|| DaemonProcess::trace_socket_path_for_home(&self.test_home))
    }

    pub(crate) fn set_daemon_env_for_in_process(&self) {
        unsafe {
            std::env::set_var("GIT_AI_DAEMON_HOME", self.daemon_home_path());
            std::env::set_var(
                "GIT_AI_DAEMON_CONTROL_SOCKET",
                self.daemon_control_socket_path(),
            );
        }
    }

    pub(crate) fn config_patch_json(&self) -> Option<String> {
        self.config_patch
            .as_ref()
            .and_then(|patch| serde_json::to_string(patch).ok())
    }

    pub(super) fn trace2_nesting_value() -> String {
        std::env::var("GIT_AI_TEST_TRACE2_NESTING").unwrap_or_else(|_| "0".to_string())
    }

    pub(super) fn setup_daemon_mode(&mut self) {
        if self.daemon_process.is_some() {
            return;
        }
        let daemon = match self.daemon_scope {
            DaemonTestScope::Shared => shared_daemon_process(&self.path),
            DaemonTestScope::Dedicated => Arc::new(DaemonProcess::start(
                &self.path,
                &self.test_home,
                &self.test_db_path,
            )),
            DaemonTestScope::NoDaemon => return,
        };
        self.test_db_path = daemon.test_db_path.clone();
        self.daemon_process = Some(daemon);
        self.sync_test_home_config();
    }

    pub(crate) fn start_dedicated_daemon_for_test(&mut self) {
        assert!(
            self.daemon_process.is_none(),
            "test repo already has an active daemon"
        );
        self.daemon_scope = DaemonTestScope::Dedicated;
        self.setup_daemon_mode();
    }

    pub(crate) fn restart_dedicated_daemon_for_test(&mut self) {
        assert_eq!(
            self.daemon_scope,
            DaemonTestScope::Dedicated,
            "restart_dedicated_daemon_for_test requires a dedicated daemon repo"
        );
        let family_key = self.daemon_family_key();
        let pending_summary = {
            let registry = daemon_sync_registry()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.pending_work_summary(&family_key)
        };
        assert!(
            pending_summary.is_none(),
            "cannot restart dedicated daemon with pending daemon sync work for family {}: {}",
            family_key,
            pending_summary.unwrap_or_default()
        );
        if let Some(daemon) = self.daemon_process.take() {
            daemon.shutdown();
            #[cfg(windows)]
            daemon.wait_until_stopped();
        }
        self.setup_daemon_mode();
    }

    pub(super) fn configure_command_env(&self, command: &mut Command) {
        // Isolate all git + git-ai config reads from developer machine settings.
        configure_test_home_env(command, &self.test_home);
        self.configure_test_process_env(command);

        if self.has_active_daemon() {
            command.env(
                "GIT_TRACE2_EVENT",
                DaemonConfig::trace2_event_target_for_path(&self.daemon_trace_socket_path()),
            );
            command.env("GIT_TRACE2_EVENT_NESTING", Self::trace2_nesting_value());
        }
    }

    pub(super) fn configure_git_ai_env(&self, command: &mut Command) {
        // Isolate all git + git-ai config reads from developer machine settings.
        configure_test_home_env(command, &self.test_home);
        self.configure_test_process_env(command);
        command.env("GIT_AI_DAEMON_HOME", self.daemon_home_path());
        command.env(
            "GIT_AI_DAEMON_CONTROL_SOCKET",
            self.daemon_control_socket_path(),
        );
        command.env(
            "GIT_AI_DAEMON_TRACE_SOCKET",
            self.daemon_trace_socket_path(),
        );
        if self.has_active_daemon() {
            command.env("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true");
        }
    }

    fn configure_test_process_env(&self, command: &mut Command) {
        if let Some(patch_json) = self.config_patch_json() {
            command.env("GIT_AI_TEST_CONFIG_PATCH", patch_json);
        }
        command.env("GIT_AI_TEST_DB_PATH", self.test_db_path.to_str().unwrap());
        command.env("GITAI_TEST_DB_PATH", self.test_db_path.to_str().unwrap());
    }

    /// Patch the git-ai config for this test repo
    /// Allows overriding specific config properties like ignore_prompts, telemetry settings, etc.
    /// The patch is applied via environment variable when running git-ai commands
    ///
    /// # Example
    /// ```ignore
    /// let mut repo = TestRepo::new();
    /// repo.patch_git_ai_config(|patch| {
    ///     patch.ignore_prompts = Some(true);
    ///     patch.telemetry_oss_disabled = Some(true);
    /// });
    /// ```
    pub fn patch_git_ai_config<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ConfigPatch),
    {
        let starts_dedicated_daemon = self.promote_shared_daemon_for_config_patch();
        let mut patch = self.config_patch.take().unwrap_or_default();
        f(&mut patch);
        self.config_patch = Some(patch);
        self.sync_test_home_config();
        if starts_dedicated_daemon {
            self.setup_daemon_mode();
        }
    }

    /// Restrict attribution collection to this exact repository or worktree.
    /// Requires a dedicated daemon so the exact allowlist cannot affect other tests.
    pub fn allow_only_self_for_collection(&mut self) {
        assert_eq!(
            self.daemon_scope,
            DaemonTestScope::Dedicated,
            "an exact repository allowlist requires a dedicated test daemon"
        );
        let repo_root = normalize_to_posix(&self.canonical_path().to_string_lossy());
        self.patch_git_ai_config(move |patch| {
            patch.allowed_repositories = Some(vec![repo_root]);
        });
    }
}
