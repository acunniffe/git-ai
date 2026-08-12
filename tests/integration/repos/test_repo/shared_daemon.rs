use super::*;

extern "C" fn shutdown_shared_daemon_at_process_exit() {
    if let Some(daemon) = SHARED_DAEMON_PROCESS.get() {
        daemon.shutdown();
    }
    if let Some(pool) = SHARED_DAEMON_POOL.get() {
        let daemons = {
            let mut pool = pool.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pool.drain().map(|(_, daemon)| daemon).collect::<Vec<_>>()
        };
        for daemon in daemons {
            daemon.shutdown();
        }
    }
}

static SHARED_DAEMON_PROCESS: OnceLock<Arc<DaemonProcess>> = OnceLock::new();
static SHARED_DAEMON_POOL: OnceLock<Mutex<HashMap<usize, Arc<DaemonProcess>>>> = OnceLock::new();
static SHARED_DAEMON_EXIT_HOOK: OnceLock<()> = OnceLock::new();
static SHARED_DAEMON_POOL_ASSIGNMENT_COUNTER: AtomicUsize = AtomicUsize::new(0);
/// to even start the process image (see [`is_windows_loader_init_failure`]).
pub(super) fn shared_daemon_pool_size() -> usize {
    std::env::var("GIT_AI_TEST_SHARED_DAEMON_POOL_SIZE")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(8)
}

pub(super) fn register_shared_daemon_exit_hook() {
    SHARED_DAEMON_EXIT_HOOK.get_or_init(|| {
        let rc = unsafe { libc::atexit(shutdown_shared_daemon_at_process_exit) };
        assert_eq!(rc, 0, "failed to register shared daemon exit hook");
    });
}

pub(super) fn shared_daemon_process(repo_path: &Path) -> Arc<DaemonProcess> {
    register_shared_daemon_exit_hook();
    let pool_size = shared_daemon_pool_size();
    if pool_size <= 1 {
        return SHARED_DAEMON_PROCESS
            .get_or_init(|| Arc::new(start_shared_daemon_process(repo_path, None)))
            .clone();
    }

    let shard = shared_daemon_pool_shard(pool_size);
    let pool = SHARED_DAEMON_POOL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pool = pool.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pool.entry(shard)
        .or_insert_with(|| Arc::new(start_shared_daemon_process(repo_path, Some(shard))))
        .clone()
}

pub(super) fn start_shared_daemon_process(repo_path: &Path, shard: Option<usize>) -> DaemonProcess {
    let mut rng = rand::rng();
    let n: u64 = rng.random_range(0..10_000_000_000);
    let base = std::env::temp_dir();
    let shard_suffix = shard
        .map(|shard| format!("-pool-{}", shard))
        .unwrap_or_default();
    let daemon_home = base.join(format!("git-ai-shared-daemon-{}{}-home", n, shard_suffix));
    let test_db_path = base.join(format!("git-ai-shared-daemon-{}{}-db", n, shard_suffix));
    write_config_patch_to_home(&default_test_config_patch(), &daemon_home);
    DaemonProcess::start(repo_path, &daemon_home, &test_db_path)
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

thread_local! {
    pub(super) static WORKTREE_MODE: Cell<bool> = const { Cell::new(false) };
    static SHARED_DAEMON_POOL_SHARD: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(super) fn shared_daemon_pool_shard(pool_size: usize) -> usize {
    if pool_size <= 1 {
        return 0;
    }

    SHARED_DAEMON_POOL_SHARD.with(|slot| match slot.get() {
        Some(shard) if shard < pool_size => shard,
        _ => {
            let shard =
                SHARED_DAEMON_POOL_ASSIGNMENT_COUNTER.fetch_add(1, Ordering::Relaxed) % pool_size;
            slot.set(Some(shard));
            shard
        }
    })
}
