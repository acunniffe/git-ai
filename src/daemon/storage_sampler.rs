//! Best-effort .git/ai directory storage telemetry for daemon heartbeats.
//!
//! Tracks known repo ai_dirs discovered through trace2 ingestion and computes
//! aggregate storage statistics (total bytes, working log counts) on demand.
//!
//! All I/O is bounded: directory traversals cap entries, skip symlinks, and
//! enforce an elapsed-time limit so the heartbeat path stays fast.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Instant;

struct StorageRegistry {
    sender: mpsc::Sender<PathBuf>,
    state: Mutex<StorageRegistryState>,
}

struct StorageRegistryState {
    receiver: mpsc::Receiver<PathBuf>,
    dirs: HashSet<PathBuf>,
}

static STORAGE_REGISTRY: OnceLock<StorageRegistry> = OnceLock::new();

fn storage_registry() -> &'static StorageRegistry {
    STORAGE_REGISTRY.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        StorageRegistry {
            sender,
            state: Mutex::new(StorageRegistryState {
                receiver,
                dirs: HashSet::new(),
            }),
        }
    })
}

/// Maximum number of directory entries to visit per ai_dir traversal.
const MAX_ENTRIES_PER_DIR: usize = 10_000;

/// Maximum wall-clock time for the entire storage scan.
const MAX_SCAN_DURATION: std::time::Duration = std::time::Duration::from_secs(2);

/// Aggregated storage statistics across all known repos.
#[derive(Debug, Clone)]
pub struct StorageStats {
    pub git_ai_dir_bytes: u64,
    pub working_logs_dir_bytes: u64,
    pub working_logs_count: u64,
    pub working_log_largest_bytes: u64,
}

/// Register a .git/ai directory path for storage tracking.
pub fn register_ai_dir(ai_dir: PathBuf) {
    let _ = storage_registry().sender.send(ai_dir);
}

fn known_ai_dirs() -> Option<Vec<PathBuf>> {
    let mut state = storage_registry().state.lock().ok()?;
    while let Ok(ai_dir) = state.receiver.try_recv() {
        state.dirs.insert(ai_dir);
    }
    if state.dirs.is_empty() {
        return None;
    }
    Some(state.dirs.iter().cloned().collect())
}

#[cfg(test)]
pub(crate) fn is_ai_dir_registered(ai_dir: &Path) -> bool {
    known_ai_dirs().is_some_and(|dirs| dirs.iter().any(|dir| dir == ai_dir))
}

/// Compute aggregate storage statistics across all known repo ai_dirs.
/// Returns `None` if no repos are registered or on registry failure.
pub fn scan_storage() -> Option<StorageStats> {
    let dirs = known_ai_dirs()?;
    Some(scan_storage_dirs(&dirs))
}

/// Compute aggregate storage statistics for a given set of ai_dir paths.
/// Bounded: skips symlinks, caps entry count, and enforces a time limit.
fn scan_storage_dirs(dirs: &[PathBuf]) -> StorageStats {
    let deadline = Instant::now() + MAX_SCAN_DURATION;
    let mut total_ai_bytes: u64 = 0;
    let mut total_wl_bytes: u64 = 0;
    let mut total_wl_count: u64 = 0;
    let mut largest_wl_bytes: u64 = 0;

    for ai_dir in dirs {
        if Instant::now() >= deadline {
            break;
        }
        if !ai_dir.is_dir() {
            continue;
        }

        let mut entries_visited: usize = 0;
        let ai_bytes =
            dir_size_bounded(ai_dir, &mut entries_visited, MAX_ENTRIES_PER_DIR, deadline);
        total_ai_bytes = total_ai_bytes.saturating_add(ai_bytes);

        // Collect working_logs dirs: direct ai_dir/working_logs plus
        // ai_dir/worktrees/*/working_logs for linked worktrees.
        let mut wl_dirs = vec![ai_dir.join("working_logs")];
        let worktrees_dir = ai_dir.join("worktrees");
        if worktrees_dir.is_dir()
            && let Ok(rd) = std::fs::read_dir(&worktrees_dir)
        {
            for entry in rd.flatten() {
                if Instant::now() >= deadline {
                    break;
                }
                let wt_wl = entry.path().join("working_logs");
                if wt_wl.is_dir() {
                    wl_dirs.push(wt_wl);
                }
            }
        }

        for wl_dir in &wl_dirs {
            if Instant::now() >= deadline {
                break;
            }
            if !wl_dir.is_dir() {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(wl_dir) else {
                continue;
            };
            for entry in rd.flatten() {
                if Instant::now() >= deadline {
                    break;
                }
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("old-"))
                {
                    continue;
                }

                total_wl_count += 1;
                let mut wl_entries: usize = 0;
                let wl_size =
                    dir_size_bounded(&path, &mut wl_entries, MAX_ENTRIES_PER_DIR, deadline);
                total_wl_bytes = total_wl_bytes.saturating_add(wl_size);
                if wl_size > largest_wl_bytes {
                    largest_wl_bytes = wl_size;
                }
            }
        }
    }

    StorageStats {
        git_ai_dir_bytes: total_ai_bytes,
        working_logs_dir_bytes: total_wl_bytes,
        working_logs_count: total_wl_count,
        working_log_largest_bytes: largest_wl_bytes,
    }
}

/// Recursively compute directory size, bounded by max entries and deadline.
/// Skips symlinks.
fn dir_size_bounded(
    dir: &Path,
    entries_visited: &mut usize,
    max_entries: usize,
    deadline: Instant,
) -> u64 {
    let mut total: u64 = 0;
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    for entry in rd.flatten() {
        if *entries_visited >= max_entries || Instant::now() >= deadline {
            break;
        }
        *entries_visited += 1;

        let path = entry.path();

        // Skip symlinks
        if path
            .symlink_metadata()
            .map_or(true, |m| m.file_type().is_symlink())
        {
            continue;
        }

        if path.is_file() {
            if let Ok(meta) = path.metadata() {
                total = total.saturating_add(meta.len());
            }
        } else if path.is_dir() {
            total = total.saturating_add(dir_size_bounded(
                &path,
                entries_visited,
                max_entries,
                deadline,
            ));
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_storage_dirs_returns_zeros_for_nonexistent_dir() {
        let stats = scan_storage_dirs(&[PathBuf::from("/tmp/nonexistent-ai-dir-test-12345")]);
        assert_eq!(stats.git_ai_dir_bytes, 0);
        assert_eq!(stats.working_logs_count, 0);
    }

    #[test]
    fn scan_storage_dirs_computes_basic_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("ai");
        fs::create_dir_all(ai_dir.join("working_logs").join("abc123")).unwrap();
        fs::write(
            ai_dir
                .join("working_logs")
                .join("abc123")
                .join("checkpoints.jsonl"),
            "test data here!",
        )
        .unwrap();
        fs::write(ai_dir.join("config.json"), "{}").unwrap();

        let stats = scan_storage_dirs(&[ai_dir]);
        assert!(stats.git_ai_dir_bytes > 0);
        assert!(stats.working_logs_dir_bytes > 0);
        assert_eq!(stats.working_logs_count, 1);
        assert!(stats.working_log_largest_bytes > 0);
    }

    #[test]
    fn scan_storage_dirs_skips_old_working_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("ai");
        let wl = ai_dir.join("working_logs");
        fs::create_dir_all(wl.join("abc123")).unwrap();
        fs::write(wl.join("abc123").join("data.json"), "active").unwrap();
        fs::create_dir_all(wl.join("old-def456")).unwrap();
        fs::write(wl.join("old-def456").join("data.json"), "archived").unwrap();

        let stats = scan_storage_dirs(&[ai_dir]);
        assert_eq!(stats.working_logs_count, 1);
    }

    #[test]
    fn scan_storage_dirs_finds_largest_working_log() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("ai");
        let wl = ai_dir.join("working_logs");
        fs::create_dir_all(wl.join("small")).unwrap();
        fs::write(wl.join("small").join("data"), "x").unwrap();
        fs::create_dir_all(wl.join("large")).unwrap();
        fs::write(wl.join("large").join("data"), "x".repeat(1000)).unwrap();

        let stats = scan_storage_dirs(&[ai_dir]);
        assert_eq!(stats.working_logs_count, 2);
        assert!(stats.working_log_largest_bytes >= 1000);
    }

    #[test]
    fn dir_size_bounded_respects_entry_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(tmp.path().join(format!("file_{i}.txt")), "hello").unwrap();
        }

        let mut visited = 0;
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let _ = dir_size_bounded(tmp.path(), &mut visited, 5, deadline);
        assert!(visited <= 5);
    }

    #[test]
    fn scan_storage_dirs_includes_linked_worktree_working_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let ai_dir = tmp.path().join("ai");

        // Direct working_logs
        fs::create_dir_all(ai_dir.join("working_logs").join("main-log")).unwrap();
        fs::write(
            ai_dir.join("working_logs").join("main-log").join("data"),
            "main",
        )
        .unwrap();

        // Linked worktree working_logs at ai/worktrees/feature/working_logs
        let wt_wl = ai_dir
            .join("worktrees")
            .join("feature")
            .join("working_logs");
        fs::create_dir_all(wt_wl.join("wt-log")).unwrap();
        fs::write(wt_wl.join("wt-log").join("data"), "worktree data!").unwrap();

        let stats = scan_storage_dirs(&[ai_dir]);
        // Should find both: 1 from direct + 1 from linked worktree
        assert_eq!(stats.working_logs_count, 2);
        assert!(stats.working_logs_dir_bytes > 0);
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_bounded_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("real.txt"), "data").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real.txt"), tmp.path().join("link.txt"))
            .unwrap();

        let mut visited = 0;
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let size = dir_size_bounded(tmp.path(), &mut visited, 100, deadline);
        // Only the real file's size (4 bytes), not counted twice
        assert_eq!(size, 4);
    }
}
