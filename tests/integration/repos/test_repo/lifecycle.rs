use super::*;

impl TestRepo {
    pub fn new_with_daemon_scope(daemon_scope: DaemonTestScope) -> Self {
        if WORKTREE_MODE.with(|flag| flag.get()) {
            return Self::new_worktree_variant_with_daemon_scope(daemon_scope);
        }
        Self::new_with_daemon_scope_inner(daemon_scope)
    }

    pub fn new_dedicated_daemon() -> Self {
        Self::new_with_daemon_scope(DaemonTestScope::Dedicated)
    }

    pub(super) fn write_test_config_to_home(&self, home: &Path) {
        if let Some(patch) = &self.config_patch {
            write_config_patch_to_home(patch, home);
        }
    }

    pub(super) fn sync_test_home_config(&self) {
        self.write_test_config_to_home(&self.test_home);
    }

    pub(super) fn apply_default_config_patch(&mut self) {
        self.config_patch = Some(default_test_config_patch());
        self.sync_test_home_config();
    }

    pub(super) fn promote_shared_daemon_for_config_patch(&mut self) -> bool {
        if self.daemon_scope != DaemonTestScope::Shared {
            return false;
        }

        // Shared daemons intentionally own an immutable baseline config. A
        // custom fixture must use a daemon whose HOME and database belong only
        // to that fixture, otherwise Config::fresh() can read another test's
        // patch while asynchronously processing trace2 side effects.
        let shared_test_db_path = self.test_db_path.clone();
        if self._base_test_db_path.as_ref() == Some(&shared_test_db_path) {
            // Worktree-mode fixtures inherit the shared DB path solely for the
            // shared daemon. Do not remove that pool-owned directory on drop.
            self._base_test_db_path = None;
        }
        self.daemon_process = None;
        self.daemon_scope = DaemonTestScope::Dedicated;
        self.test_db_path = dedicated_test_db_path(&self.test_home);
        true
    }

    pub fn new() -> Self {
        Self::new_with_daemon_scope(DaemonTestScope::Shared)
    }

    /// Create a worktree-backed TestRepo.
    /// This creates a normal base repo and then adds an orphan linked worktree
    /// so tests keep empty-repo semantics (the first real commit is still a root commit).
    pub(super) fn new_worktree_variant() -> Self {
        Self::new_worktree_variant_with_daemon_scope(DaemonTestScope::Shared)
    }

    pub(super) fn new_worktree_variant_with_daemon_scope(daemon_scope: DaemonTestScope) -> Self {
        let mut base = Self::new_with_daemon_scope_inner(daemon_scope);

        let default_branch = default_branchname();
        let base_branch = base.current_branch();
        if base_branch == default_branch {
            let mut rng = rand::rng();
            let n: u64 = rng.random_range(0..10_000_000_000);
            let temp_branch = format!("base-worktree-{}", n);
            let temp_ref = format!("refs/heads/{}", temp_branch);
            let mut command = Command::new(real_git_executable());
            command.args([
                "-C",
                base.path.to_str().unwrap(),
                "symbolic-ref",
                "HEAD",
                &temp_ref,
            ]);
            let switch_output = run_command_output(
                &mut command,
                "move base repo off default branch for worktree variant",
            )
            .expect("failed to move base repo off default branch");
            if !switch_output.status.success() {
                panic!(
                    "failed to move base repo off default branch:\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&switch_output.stdout),
                    String::from_utf8_lossy(&switch_output.stderr)
                );
            }
        }

        let mut rng = rand::rng();
        let wt_n: u64 = rng.random_range(0..10_000_000_000);
        let worktree_path = std::env::temp_dir().join(format!("{}-wt", wt_n));

        let mut command = Command::new(real_git_executable());
        command.args([
            "-C",
            base.path.to_str().unwrap(),
            "worktree",
            "add",
            "--orphan",
            worktree_path.to_str().unwrap(),
        ]);
        let output = run_command_output(&mut command, "add orphan worktree")
            .expect("failed to add worktree");

        if !output.status.success() {
            panic!(
                "failed to create linked worktree:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let mut command = Command::new(real_git_executable());
        command.args([
            "-C",
            worktree_path.to_str().unwrap(),
            "branch",
            "--show-current",
        ]);
        let branch_name_output = run_command_output(&mut command, "inspect worktree branch")
            .expect("failed to inspect worktree branch");
        if !branch_name_output.status.success() {
            panic!(
                "failed to inspect linked worktree branch:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&branch_name_output.stdout),
                String::from_utf8_lossy(&branch_name_output.stderr)
            );
        }
        let current_branch = String::from_utf8_lossy(&branch_name_output.stdout)
            .trim()
            .to_string();
        if current_branch != default_branch {
            let mut command = Command::new(real_git_executable());
            command.args([
                "-C",
                worktree_path.to_str().unwrap(),
                "branch",
                "-m",
                default_branch,
            ]);
            let rename_output = run_command_output(&mut command, "rename worktree branch")
                .expect("failed to rename worktree branch");
            if !rename_output.status.success() {
                panic!(
                    "failed to rename linked worktree branch:\nstdout: {}\nstderr: {}",
                    String::from_utf8_lossy(&rename_output.stdout),
                    String::from_utf8_lossy(&rename_output.stderr)
                );
            }
        }

        let base_path = base.path.clone();
        let base_test_home = base.test_home.clone();
        let base_test_db_path = base.test_db_path.clone();
        let feature_flags = base.feature_flags.clone();
        let config_patch = base.config_patch.clone();
        let daemon_scope = base.daemon_scope;
        let daemon_process = base.daemon_process.take();

        // Prevent base Drop from running - we manage cleanup in the worktree Drop
        std::mem::forget(base);

        // Daemon tests use a single process-scoped internal DB path. Reuse
        // the base DB path for linked worktrees so test expectations and
        // daemon writes align.
        let wt_test_db_path = base_test_db_path.clone();

        let mut repo = Self {
            path: worktree_path,
            feature_flags,
            config_patch,
            test_db_path: wt_test_db_path,
            test_home: base_test_home,
            daemon_scope,
            daemon_process,
            _base_repo_path: Some(base_path),
            _base_test_db_path: Some(base_test_db_path),
            daemon_family_key: OnceLock::new(),
        };

        repo.apply_default_config_patch();
        repo
    }

    pub(super) fn new_with_daemon_scope_inner(daemon_scope: DaemonTestScope) -> Self {
        // Isolate this test binary's HOME before any git or git-ai subprocess is spawned.
        ensure_isolated_process_home();

        let mut rng = rand::rng();
        let n: u64 = rng.random_range(0..10000000000);
        let base = std::env::temp_dir();
        let path = base.join(n.to_string());
        let test_home = base.join(format!("{}-home", n));
        let test_db_path = resolve_test_db_path(&base, n, &test_home);

        // Clone from cached template (git init + config + symbolic-ref already done)
        clone_template_to(&path);

        let mut repo = Self {
            path,
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path,
            test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        repo.apply_default_config_patch();
        repo.setup_daemon_mode();

        repo
    }

    pub fn new_with_daemon_env(daemon_env: &[(&str, &str)]) -> Self {
        Self::new_with_daemon_env_and_patch(daemon_env, |_| {})
    }

    /// Like `new_with_daemon_env`, but applies `extra_patch` to the config **before** the
    /// daemon process starts, so the daemon reads the fully-configured state from its first
    /// tracing event onward (important for features gated by `telemetry_enabled()`).
    pub fn new_with_daemon_env_and_patch<F>(daemon_env: &[(&str, &str)], extra_patch: F) -> Self
    where
        F: FnOnce(&mut git_ai::config::ConfigPatch),
    {
        ensure_isolated_process_home();

        let mut rng = rand::rng();
        let n: u64 = rng.random_range(0..10000000000);
        let base = std::env::temp_dir();
        let path = base.join(n.to_string());
        let test_home = base.join(format!("{}-home", n));
        let test_db_path = resolve_test_db_path(&base, n, &test_home);

        clone_template_to(&path);

        let mut repo = Self {
            path,
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path,
            test_home,
            daemon_scope: DaemonTestScope::Dedicated,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        repo.apply_default_config_patch();
        repo.patch_git_ai_config(extra_patch);

        // Start a dedicated daemon with extra env vars
        let daemon = Arc::new(DaemonProcess::start_with_env(
            &repo.path,
            &repo.test_home,
            &repo.test_db_path,
            daemon_env,
        ));
        repo.test_db_path = daemon.test_db_path.clone();
        repo.daemon_process = Some(daemon);
        repo.sync_test_home_config();

        repo
    }

    pub fn new_worktree() -> Self {
        Self::new_worktree_with_daemon_scope(DaemonTestScope::Shared)
    }

    pub fn new_worktree_with_daemon_scope(daemon_scope: DaemonTestScope) -> Self {
        let mut rng = rand::rng();
        let n: u64 = rng.random_range(0..10000000000);
        let base = std::env::temp_dir();
        let main_path = base.join(format!("{}-main", n));
        let worktree_path = base.join(format!("{}-wt", n));
        let test_home = base.join(format!("{}-home", n));
        let test_db_path = resolve_test_db_path(&base, n, &test_home);

        // Clone from cached template (git init + config + symbolic-ref already done)
        clone_template_to(&main_path);

        let mut command = Command::new(real_git_executable());
        command.args([
            "-C",
            main_path.to_str().unwrap(),
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let initial_commit_output =
            run_command_output(&mut command, "create initial commit for worktree base")
                .expect("failed to create initial commit for worktree base");
        if !initial_commit_output.status.success() {
            panic!(
                "failed to create initial worktree base commit:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&initial_commit_output.stdout),
                String::from_utf8_lossy(&initial_commit_output.stderr)
            );
        }

        let mut command = Command::new(real_git_executable());
        command.args([
            "-C",
            main_path.to_str().unwrap(),
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
        ]);
        let worktree_output = run_command_output(&mut command, "create linked worktree")
            .expect("failed to create linked worktree");

        if !worktree_output.status.success() {
            panic!(
                "failed to create linked worktree:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&worktree_output.stdout),
                String::from_utf8_lossy(&worktree_output.stderr)
            );
        }

        let mut repo = Self {
            path: worktree_path,
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path,
            test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: Some(main_path),
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        repo.apply_default_config_patch();
        repo.setup_daemon_mode();
        repo
    }

    /// Create a standalone bare repository for testing
    pub fn new_bare() -> Self {
        Self::new_bare_with_daemon_scope(DaemonTestScope::Shared)
    }

    pub fn new_bare_with_daemon_scope(daemon_scope: DaemonTestScope) -> Self {
        let mut rng = rand::rng();
        let n: u64 = rng.random_range(0..10000000000);
        let base = std::env::temp_dir();
        let path = base.join(n.to_string());
        let test_home = base.join(format!("{}-home", n));
        let test_db_path = resolve_test_db_path(&base, n, &test_home);

        // Clone from cached bare template
        clone_bare_template_to(&path);

        let repo = Self {
            path,
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path,
            test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        let mut repo = repo;
        repo.setup_daemon_mode();
        repo
    }

    /// Create a pair of test repos: a local mirror and its upstream remote.
    /// The mirror is cloned from the upstream, so "origin" is automatically configured.
    /// Returns (mirror, upstream) tuple.
    ///
    /// # Example
    /// ```ignore
    /// let (mirror, upstream) = TestRepo::new_with_remote();
    ///
    /// // Make changes in mirror
    /// mirror.filename("test.txt").write("hello").stage();
    /// mirror.commit("initial commit");
    ///
    /// // Push to upstream
    /// mirror.git(&["push", "origin", "main"]);
    /// ```
    pub fn new_with_remote() -> (Self, Self) {
        Self::new_with_remote_with_daemon_scope(DaemonTestScope::Shared)
    }

    pub fn new_with_remote_with_daemon_scope(daemon_scope: DaemonTestScope) -> (Self, Self) {
        let mut rng = rand::rng();
        let base = std::env::temp_dir();

        // Create bare upstream repository (acts as the remote server)
        let upstream_n: u64 = rng.random_range(0..10000000000);
        let upstream_path = base.join(upstream_n.to_string());
        let upstream_test_home = base.join(format!("{}-home", upstream_n));
        let upstream_test_db_path = resolve_test_db_path(&base, upstream_n, &upstream_test_home);
        clone_bare_template_to(&upstream_path);

        let mut upstream = Self {
            path: upstream_path.clone(),
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path: upstream_test_db_path,
            test_home: upstream_test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        // Ensure the upstream default branch is named "main" for consistency across Git versions
        let _ = upstream.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);

        // Clone upstream to create mirror with origin configured
        let mirror_n: u64 = rng.random_range(0..10000000000);
        let mirror_path = base.join(mirror_n.to_string());
        let mirror_test_home = base.join(format!("{}-home", mirror_n));
        let mirror_test_db_path = resolve_test_db_path(&base, mirror_n, &mirror_test_home);

        let mut command = Command::new(real_git_executable());
        command.args([
            "clone",
            upstream_path.to_str().unwrap(),
            mirror_path.to_str().unwrap(),
        ]);
        let clone_output = run_command_output(&mut command, "clone upstream repository")
            .expect("failed to clone upstream repository");

        if !clone_output.status.success() {
            panic!(
                "Failed to clone upstream repository:\nstderr: {}",
                String::from_utf8_lossy(&clone_output.stderr)
            );
        }

        // Configure mirror with user credentials
        set_repo_user_config(&mirror_path);

        let mut mirror = Self {
            path: mirror_path,
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path: mirror_test_db_path,
            test_home: mirror_test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        // Ensure the default branch is named "main" for consistency across Git versions
        let _ = mirror.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);

        upstream.apply_default_config_patch();
        mirror.apply_default_config_patch();
        mirror.setup_daemon_mode();
        // The upstream side of new_with_remote() is a bare remote fixture. It is not the repo
        // under test for daemon mode, and bootstrapping the shared daemon against a bare repo
        // breaks the readiness handshake for this test process.

        (mirror, upstream)
    }

    pub fn new_at_path(path: &Path) -> Self {
        Self::new_at_path_with_daemon_scope(path, DaemonTestScope::Shared)
    }

    pub fn new_at_path_with_daemon_scope(path: &Path, daemon_scope: DaemonTestScope) -> Self {
        let mut rng = rand::rng();
        let db_n: u64 = rng.random_range(0..10000000000);
        let test_home = std::env::temp_dir().join(format!("{}-home", db_n));
        let test_db_path = resolve_test_db_path(&std::env::temp_dir(), db_n, &test_home);

        // Clone from cached template (git init + config + symbolic-ref already done).
        // If path already has a .git directory (e.g. a real repo cloned from GitHub),
        // skip the template copy to avoid overwriting its config, HEAD, and refs.
        if path.join(".git").exists() {
            set_repo_user_config(path);
        } else {
            clone_template_to(path);
        }

        let mut repo = Self {
            path: path.to_path_buf(),
            feature_flags: FeatureFlags::default(),
            config_patch: None,
            test_db_path,
            test_home,
            daemon_scope,
            daemon_process: None,
            _base_repo_path: None,
            _base_test_db_path: None,
            daemon_family_key: OnceLock::new(),
        };

        repo.apply_default_config_patch();
        repo.setup_daemon_mode();
        repo
    }
}
