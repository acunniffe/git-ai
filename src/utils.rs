use crate::error::GitAiError;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

static IS_TERMINAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

#[inline]
pub fn normalize_to_posix(path: &str) -> String {
    path.replace('\\', "/")
}

/// Stable path key for maps that must match before and after a file is created.
///
/// `std::fs::canonicalize` fails for paths that do not exist yet, so a naive
/// `canonicalize(...).unwrap_or(raw)` stores the raw path at AI PreToolUse and a
/// resolved path (symlink-expanded) once the file exists — e.g. macOS
/// `/var` → `/private/var`, or an explicit workdir symlink. Walk up to the
/// longest existing ancestor, canonicalize that, and re-join the missing
/// suffix so both sides share the same key.
pub fn canonicalize_path_key(path: &str) -> String {
    let path = Path::new(path);
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            let mut result = canonical;
            for part in suffix.iter().rev() {
                result.push(part);
            }
            return result.to_string_lossy().into_owned();
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                suffix.push(name.to_owned());
                cursor = parent;
            }
            _ => break,
        }
    }

    path.to_string_lossy().into_owned()
}

fn resolve_git_ai_exe_from_invocation_path(path: PathBuf) -> PathBuf {
    let canonical_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    // Get platform-specific executable names
    let git_name = if cfg!(windows) { "git.exe" } else { "git" };
    let git_ai_name = if cfg!(windows) {
        "git-ai.exe"
    } else {
        "git-ai"
    };

    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return canonical_path;
    };

    if file_name == git_name {
        let sibling = path.with_file_name(git_ai_name);
        if sibling.exists() {
            return sibling;
        }

        let canonical_sibling = canonical_path.with_file_name(git_ai_name);
        if canonical_sibling.exists() {
            return canonical_sibling;
        }

        return PathBuf::from(git_ai_name);
    }

    let hook_candidate = file_name.strip_suffix(".exe").unwrap_or(file_name);
    if crate::commands::git_hook_handlers::is_git_hook_binary_name(hook_candidate) {
        if canonical_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == git_ai_name)
        {
            return canonical_path;
        }

        let sibling = path.with_file_name(git_ai_name);
        if sibling.exists() {
            return sibling;
        }

        let canonical_sibling = canonical_path.with_file_name(git_ai_name);
        if canonical_sibling.exists() {
            return canonical_sibling;
        }

        return PathBuf::from(git_ai_name);
    }

    canonical_path
}

pub(crate) fn current_git_ai_exe() -> Result<PathBuf, GitAiError> {
    let path = std::env::current_exe()?;
    Ok(resolve_git_ai_exe_from_invocation_path(path))
}

fn internal_git_ai_command_with_exe(exe: PathBuf, subcommand: &str) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg(subcommand)
        .env(crate::commands::git_hook_handlers::ENV_SKIP_ALL_HOOKS, "1");
    cmd
}

pub fn spawn_internal_git_ai_subcommand(
    subcommand: &str,
    extra_args: &[&str],
    guard_env: &str,
    extra_env: &[(&str, &str)],
) -> bool {
    if guard_env.is_empty() || std::env::var(guard_env).as_deref() == Ok("1") {
        return false;
    }

    let Ok(exe) = current_git_ai_exe() else {
        return false;
    };
    let mut cmd = internal_git_ai_command_with_exe(exe, subcommand);
    cmd.args(extra_args);

    cmd.env(guard_env, "1");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

pub fn is_interactive_terminal() -> bool {
    *IS_TERMINAL.get_or_init(|| std::io::stdin().is_terminal())
}

/// Returns true if the process is running inside a background AI agent environment.
pub fn is_in_background_agent() -> bool {
    !matches!(
        crate::authorship::background_agent::detect(),
        crate::authorship::background_agent::BackgroundAgent::None
    )
}

/// Returns true if the current process is running with elevated privileges
/// (root on Unix, Administrator on Windows).
#[cfg(unix)]
pub fn is_running_as_superuser() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn is_running_as_superuser() -> bool {
    use std::ffi::c_void;
    use std::mem;

    type Handle = *mut c_void;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(
            process_handle: Handle,
            desired_access: u32,
            token_handle: *mut Handle,
        ) -> i32;
        fn GetTokenInformation(
            token_handle: Handle,
            token_information_class: u32,
            token_information: *mut u8,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    const TOKEN_QUERY: u32 = 0x0008;
    // TokenElevationType (class 18) returns 1/2/3:
    //   1 = Default (no split token — UAC disabled or built-in Admin)
    //   2 = Full (elevated half of split token — "Run as Administrator")
    //   3 = Limited (non-elevated half of split token — normal terminal)
    // Only type 2 is the dangerous case: files will be admin-owned but normal
    // processes won't be, causing permission mismatches.
    const TOKEN_ELEVATION_TYPE_CLASS: u32 = 18;
    const TOKEN_ELEVATION_TYPE_FULL: u32 = 2;

    unsafe {
        let mut token: Handle = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }

        let mut elev_type: u32 = 0;
        let mut size: u32 = 0;
        let result = GetTokenInformation(
            token,
            TOKEN_ELEVATION_TYPE_CLASS,
            &mut elev_type as *mut _ as *mut u8,
            mem::size_of::<u32>() as u32,
            &mut size,
        );
        CloseHandle(token);

        result != 0 && elev_type == TOKEN_ELEVATION_TYPE_FULL
    }
}

/// Returns true if the environment indicates a CI system or automated agent
/// sandbox where running as superuser is expected and acceptable.
pub fn is_superuser_expected_environment() -> bool {
    if std::env::var_os("CI").is_some() {
        return true;
    }
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        return true;
    }
    if std::env::var_os("GITLAB_CI").is_some() {
        return true;
    }
    if std::env::var_os("JENKINS_URL").is_some() {
        return true;
    }
    if std::env::var_os("BUILDKITE").is_some() {
        return true;
    }
    if std::env::var_os("CIRCLECI").is_some() {
        return true;
    }
    if std::env::var_os("CODEBUILD_BUILD_ID").is_some() {
        return true;
    }
    if std::env::var_os("AGENT_OS").is_some() {
        return true;
    }
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
        return true;
    }
    if is_inside_container() {
        return true;
    }
    if std::env::var_os("GIT_AI_DAEMON_UPGRADE").is_some() {
        return true;
    }
    false
}

fn is_inside_container() -> bool {
    // `container` env var is set by podman, systemd-nspawn, and other runtimes
    if std::env::var_os("container").is_some() {
        return true;
    }
    // Docker creates /.dockerenv in every container
    #[cfg(unix)]
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }
    false
}

/// Returns true if the user has explicitly opted in to running as superuser
/// via the `GIT_AI_ALLOW_SUPERUSER` env var or `allow_superuser` config flag.
pub fn superuser_is_allowed() -> bool {
    std::env::var("GIT_AI_ALLOW_SUPERUSER")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        || crate::config::Config::get().allow_superuser()
}

pub enum SuperuserCheckResult {
    Allowed,
    AllowedWithWarning,
    WarnFutureBlock,
}

/// Checks whether the current process is running with elevated privileges.
/// Returns `Allowed` if not superuser or in CI/agent environments.
/// Returns `AllowedWithWarning` if user explicitly opted in.
/// Returns `WarnFutureBlock` if running as superuser without opt-in (warn-only
/// for now; a future version will block).
pub fn check_superuser_guard() -> SuperuserCheckResult {
    if !is_running_as_superuser() {
        return SuperuserCheckResult::Allowed;
    }
    if is_superuser_expected_environment() {
        return SuperuserCheckResult::Allowed;
    }
    if superuser_is_allowed() {
        return SuperuserCheckResult::AllowedWithWarning;
    }
    SuperuserCheckResult::WarnFutureBlock
}

pub fn print_superuser_warning() {
    eprintln!(
        "[git-ai] warning: running as superuser (root/Administrator) is not recommended.\n\
         \n\
         Running with elevated privileges creates files owned by root that become\n\
         inaccessible to your normal user account, causing persistent daemon lock\n\
         failures. A future version may refuse to run in this configuration.\n\
         \n\
         To suppress this warning, either:\n\
         - Run git-ai as your normal user (recommended), or\n\
         - Set GIT_AI_ALLOW_SUPERUSER=1 or add \"allow_superuser\": true to ~/.git-ai/config.json\n\
         \n\
         This warning is automatically suppressed in CI environments."
    );
}

/// A cross-platform exclusive file lock.
///
/// Holds an exclusive advisory lock (Unix) or exclusive-access file handle (Windows)
/// for the lifetime of the struct. The lock is automatically released when dropped
/// or when the process exits.
pub struct LockFile {
    _file: std::fs::File,
}

impl LockFile {
    /// Try to acquire an exclusive lock on the given path.
    /// Returns `Some(LockFile)` if successful, `None` if another process holds the lock.
    pub fn try_acquire(path: &std::path::Path) -> Option<Self> {
        let file = try_lock_exclusive(path)?;
        Some(Self { _file: file })
    }
}

#[cfg(unix)]
impl Drop for LockFile {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(unix)]
#[allow(clippy::suspicious_open_options)]
fn try_lock_exclusive(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .ok()?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return None;
    }
    Some(file)
}

#[cfg(windows)]
#[allow(clippy::suspicious_open_options)]
fn try_lock_exclusive(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .ok()
}

/// Windows-specific flag to prevent console window creation
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;
/// Windows-specific flag to start a new process group
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
/// Windows-specific flag to allow a child process to break away from the current job object
#[cfg(windows)]
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
/// Unescape a git-quoted path that may contain octal escape sequences.
///
/// Git quotes filenames containing non-ASCII characters (and some special characters)
/// using C-style escaping with octal sequences. For example, a Chinese filename like
/// "中文.txt" would appear as `"\344\270\255\346\226\207.txt"` in git output.
///
/// This function handles:
/// - Quoted paths: removes surrounding quotes and unescapes content
/// - Octal escapes: converts `\NNN` sequences back to UTF-8 bytes
/// - Other escapes: `\\`, `\"`, `\n`, `\t`, etc.
/// - Unquoted paths: returned as-is
///
/// # Examples
///
/// ```
/// use git_ai::utils::unescape_git_path;
///
/// // Unquoted path - returned as-is
/// assert_eq!(unescape_git_path("simple.txt"), "simple.txt");
///
/// // Quoted path with spaces
/// assert_eq!(unescape_git_path("\"path with spaces.txt\""), "path with spaces.txt");
///
/// // Chinese characters encoded as octal
/// assert_eq!(unescape_git_path("\"\\344\\270\\255\\346\\226\\207.txt\""), "中文.txt");
/// ```
pub fn unescape_git_path(path: &str) -> String {
    // If not quoted, return as-is
    if !path.starts_with('"') || !path.ends_with('"') {
        return path.to_string();
    }

    // Remove surrounding quotes
    let inner = &path[1..path.len() - 1];

    // Parse escape sequences and collect bytes
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\\') => {
                    chars.next();
                    bytes.push(b'\\');
                }
                Some('"') => {
                    chars.next();
                    bytes.push(b'"');
                }
                Some('n') => {
                    chars.next();
                    bytes.push(b'\n');
                }
                Some('t') => {
                    chars.next();
                    bytes.push(b'\t');
                }
                Some('r') => {
                    chars.next();
                    bytes.push(b'\r');
                }
                Some(d) if d.is_ascii_digit() => {
                    // Octal escape sequence: \NNN (1-3 octal digits)
                    let mut octal = String::new();
                    for _ in 0..3 {
                        if let Some(&d) = chars.peek() {
                            if d.is_ascii_digit() && d <= '7' {
                                octal.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if let Ok(byte_val) = u8::from_str_radix(&octal, 8) {
                        bytes.push(byte_val);
                    }
                }
                _ => {
                    // Unknown escape - keep the backslash
                    bytes.push(b'\\');
                }
            }
        } else {
            // Regular character - encode as UTF-8
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
        }
    }

    // Convert bytes to UTF-8 string
    String::from_utf8(bytes).unwrap_or_else(|e| {
        // If invalid UTF-8, try lossy conversion
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    })
}

/// Current Unix time in seconds.
pub fn unix_timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Read and deserialize a JSON file, returning None on any error (missing
/// file, unreadable, or unparseable). For best-effort local caches.
pub fn read_json_file<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Best-effort atomic JSON write for local cache files: serialize, write to a
/// sibling temp file, then rename over the target so concurrent readers never
/// observe a partial file. Parent directories are created as needed; failures
/// are silently ignored.
pub fn write_json_file<T: serde::Serialize>(path: &std::path::Path, value: &T) {
    let Ok(json) = serde_json::to_vec(value) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // canonicalize_path_key
    // =========================================================================

    #[test]
    fn test_canonicalize_path_key_matches_before_and_after_create() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let before = canonicalize_path_key(file.to_str().unwrap());
        assert!(!file.exists());
        std::fs::write(&file, "hi\n").unwrap();
        let after = canonicalize_path_key(file.to_str().unwrap());
        assert_eq!(
            before, after,
            "pending-AI path keys must match across file creation"
        );
        assert_eq!(after, file.canonicalize().unwrap().to_string_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn test_canonicalize_path_key_resolves_symlink_parent_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let via_link = link.join("created.txt");
        let key_before = canonicalize_path_key(via_link.to_str().unwrap());
        std::fs::write(real.join("created.txt"), "x\n").unwrap();
        let key_after = canonicalize_path_key(via_link.to_str().unwrap());
        let expected = real
            .join("created.txt")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(key_before, expected);
        assert_eq!(key_after, expected);
    }

    // =========================================================================
    // JSON cache file helpers
    // =========================================================================

    #[test]
    fn test_json_file_round_trip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("cache.json");

        write_json_file(&path, &serde_json::json!({"a": 1}));
        assert_eq!(
            read_json_file::<serde_json::Value>(&path),
            Some(serde_json::json!({"a": 1})),
            "write must create parent dirs and round-trip"
        );

        write_json_file(&path, &serde_json::json!({"a": 2}));
        assert_eq!(
            read_json_file::<serde_json::Value>(&path),
            Some(serde_json::json!({"a": 2})),
            "write must atomically replace an existing file"
        );
    }

    #[test]
    fn test_read_json_file_none_for_missing_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        assert_eq!(read_json_file::<serde_json::Value>(&path), None);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_json_file::<serde_json::Value>(&path), None);
    }

    // =========================================================================
    // LockFile Tests
    // =========================================================================

    #[test]
    fn test_lockfile_acquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        let lock = LockFile::try_acquire(&lock_path);
        assert!(lock.is_some(), "should acquire lock on a fresh path");
    }

    #[test]
    fn test_lockfile_second_acquire_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        let _first = LockFile::try_acquire(&lock_path).expect("first acquire should succeed");
        let second = LockFile::try_acquire(&lock_path);
        assert!(second.is_none(), "second acquire should be blocked");
    }

    #[test]
    fn test_lockfile_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        {
            let _lock = LockFile::try_acquire(&lock_path).expect("first acquire should succeed");
            // _lock is dropped here
        }
        let second = LockFile::try_acquire(&lock_path);
        assert!(
            second.is_some(),
            "should acquire lock after previous holder is dropped"
        );
    }

    #[test]
    fn test_lockfile_nonexistent_parent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("no_such_dir").join("test.lock");
        let lock = LockFile::try_acquire(&lock_path);
        assert!(
            lock.is_none(),
            "should return None when parent directory does not exist"
        );
    }

    #[test]
    fn test_resolve_git_ai_exe_from_git_sibling_prefers_git_ai() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir
            .path()
            .join(if cfg!(windows) { "git.exe" } else { "git" });
        let git_ai = dir.path().join(if cfg!(windows) {
            "git-ai.exe"
        } else {
            "git-ai"
        });
        std::fs::write(&git, "").unwrap();
        std::fs::write(&git_ai, "").unwrap();

        let resolved = resolve_git_ai_exe_from_invocation_path(git);
        assert_eq!(resolved, git_ai);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_git_ai_exe_from_hook_symlink_uses_canonical_git_ai() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let git_ai = dir.path().join("git-ai");
        let hook = dir.path().join("pre-push");
        std::fs::write(&git_ai, "").unwrap();
        symlink(&git_ai, &hook).unwrap();

        let resolved = resolve_git_ai_exe_from_invocation_path(hook);
        assert_eq!(
            std::fs::canonicalize(resolved).unwrap(),
            std::fs::canonicalize(git_ai).unwrap()
        );
    }

    #[test]
    fn test_resolve_git_ai_exe_from_hook_without_target_falls_back_to_git_ai_name() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join(if cfg!(windows) {
            "pre-push.exe"
        } else {
            "pre-push"
        });
        std::fs::write(&hook, "").unwrap();

        let resolved = resolve_git_ai_exe_from_invocation_path(hook);
        let expected = if cfg!(windows) {
            "git-ai.exe"
        } else {
            "git-ai"
        };
        assert_eq!(resolved, PathBuf::from(expected));
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_git_ai_exe_from_hook_exe_name_falls_back_to_git_ai_exe() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("pre-push.exe");
        std::fs::write(&hook, "").unwrap();

        let resolved = resolve_git_ai_exe_from_invocation_path(hook);
        assert_eq!(resolved, PathBuf::from("git-ai.exe"));
    }

    #[test]
    fn test_internal_git_ai_command_sets_skip_all_hooks_env() {
        let exe = PathBuf::from("/tmp/git-ai-test");
        let cmd = internal_git_ai_command_with_exe(exe.clone(), "status");

        assert_eq!(cmd.get_program(), exe.as_os_str());
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("status")]
        );
        assert!(
            cmd.get_envs().any(|(k, v)| {
                k == std::ffi::OsStr::new(crate::commands::git_hook_handlers::ENV_SKIP_ALL_HOOKS)
                    && v == Some(std::ffi::OsStr::new("1"))
            }),
            "internal command must always set GIT_AI_SKIP_ALL_HOOKS=1"
        );
    }

    #[test]
    fn test_spawn_internal_git_ai_subcommand_respects_guard_env() {
        let key = "GIT_AI_TEST_WORKER_GUARD";
        unsafe {
            std::env::set_var(key, "1");
        }
        let spawned = spawn_internal_git_ai_subcommand("status", &[], key, &[]);
        unsafe {
            std::env::remove_var(key);
        }
        assert!(
            !spawned,
            "spawn should be skipped when guard env is already set"
        );
    }

    #[test]
    fn test_spawn_internal_git_ai_subcommand_requires_non_empty_guard_env() {
        let spawned = spawn_internal_git_ai_subcommand("status", &[], "", &[]);
        assert!(!spawned, "spawn should be skipped when guard env is empty");
    }

    // =========================================================================
    // unescape_git_path Tests
    // =========================================================================

    #[test]
    fn test_unescape_git_path_simple() {
        // Unquoted path - no change
        assert_eq!(unescape_git_path("simple.txt"), "simple.txt");
        assert_eq!(unescape_git_path("path/to/file.rs"), "path/to/file.rs");
    }

    #[test]
    fn test_unescape_git_path_quoted_with_spaces() {
        // Quoted path with spaces
        assert_eq!(
            unescape_git_path("\"path with spaces.txt\""),
            "path with spaces.txt"
        );
        assert_eq!(
            unescape_git_path("\"dir name/file name.txt\""),
            "dir name/file name.txt"
        );
    }

    #[test]
    fn test_unescape_git_path_chinese_characters() {
        // Chinese characters "中文" encoded as octal: \344\270\255\346\226\207
        assert_eq!(
            unescape_git_path("\"\\344\\270\\255\\346\\226\\207.txt\""),
            "中文.txt"
        );

        // More complex Chinese filename: "中文文件.txt"
        // 中 = \344\270\255, 文 = \346\226\207, 件 = \344\273\266
        assert_eq!(
            unescape_git_path(
                "\"\\344\\270\\255\\346\\226\\207\\346\\226\\207\\344\\273\\266.txt\""
            ),
            "中文文件.txt"
        );
    }

    #[test]
    fn test_unescape_git_path_emoji() {
        // Emoji "🚀" (rocket) = U+1F680 = \360\237\232\200 in octal UTF-8
        assert_eq!(unescape_git_path("\"\\360\\237\\232\\200.txt\""), "🚀.txt");

        // Emoji "😀" (grinning face) = U+1F600 = \360\237\230\200 in octal UTF-8
        assert_eq!(unescape_git_path("\"\\360\\237\\230\\200.txt\""), "😀.txt");

        // Mixed: "test_🎉_file.txt" where 🎉 = \360\237\216\211
        assert_eq!(
            unescape_git_path("\"test_\\360\\237\\216\\211_file.txt\""),
            "test_🎉_file.txt"
        );
    }

    #[test]
    fn test_unescape_git_path_escaped_characters() {
        // Escaped backslash
        assert_eq!(
            unescape_git_path("\"path\\\\with\\\\slashes\""),
            "path\\with\\slashes"
        );

        // Escaped quotes
        assert_eq!(unescape_git_path("\"file\\\"name.txt\""), "file\"name.txt");

        // Escaped newline and tab
        assert_eq!(unescape_git_path("\"line1\\nline2\""), "line1\nline2");
        assert_eq!(unescape_git_path("\"col1\\tcol2\""), "col1\tcol2");
    }

    #[test]
    fn test_unescape_git_path_mixed_content() {
        // Mix of ASCII, Chinese, and escapes
        assert_eq!(
            unescape_git_path("\"src/\\344\\270\\255\\346\\226\\207/file.txt\""),
            "src/中文/file.txt"
        );
    }

    // =========================================================================
    // Phase 1: CJK Extended Coverage Tests
    // =========================================================================

    #[test]
    fn test_unescape_japanese_hiragana() {
        // Japanese Hiragana "ひらがな" = \343\201\262\343\202\211\343\201\214\343\201\252
        assert_eq!(
            unescape_git_path(
                "\"\\343\\201\\262\\343\\202\\211\\343\\201\\214\\343\\201\\252.txt\""
            ),
            "ひらがな.txt"
        );
    }

    #[test]
    fn test_unescape_japanese_katakana() {
        // Japanese Katakana "カタカナ" = \343\202\253\343\202\277\343\202\253\343\203\212
        assert_eq!(
            unescape_git_path(
                "\"\\343\\202\\253\\343\\202\\277\\343\\202\\253\\343\\203\\212.txt\""
            ),
            "カタカナ.txt"
        );
    }

    #[test]
    fn test_unescape_korean_hangul() {
        // Korean Hangul "한글" = \355\225\234\352\270\200
        assert_eq!(
            unescape_git_path("\"\\355\\225\\234\\352\\270\\200.txt\""),
            "한글.txt"
        );
    }

    #[test]
    fn test_unescape_traditional_chinese() {
        // Traditional Chinese "繁體" = \347\271\201\351\253\224
        assert_eq!(
            unescape_git_path("\"\\347\\271\\201\\351\\253\\224.txt\""),
            "繁體.txt"
        );
    }

    #[test]
    fn test_unescape_mixed_cjk() {
        // Mixed CJK: "日中韓" (Japanese, Chinese, Korean characters mixed)
        // 日 = \346\227\245, 中 = \344\270\255, 韓 = \351\237\223
        assert_eq!(
            unescape_git_path("\"\\346\\227\\245\\344\\270\\255\\351\\237\\223.txt\""),
            "日中韓.txt"
        );
    }

    // =========================================================================
    // Phase 2: RTL Scripts Tests (Arabic, Hebrew, Persian, Urdu)
    // =========================================================================

    #[test]
    fn test_unescape_arabic() {
        // Arabic "مرحبا" (marhaba = hello)
        // م = \331\205, ر = \330\261, ح = \330\255, ب = \330\250, ا = \330\247
        assert_eq!(
            unescape_git_path("\"\\331\\205\\330\\261\\330\\255\\330\\250\\330\\247.txt\""),
            "مرحبا.txt"
        );
    }

    #[test]
    fn test_unescape_hebrew() {
        // Hebrew "שלום" (shalom = hello/peace)
        // ש = \327\251, ל = \327\234, ו = \327\225, ם = \327\235
        assert_eq!(
            unescape_git_path("\"\\327\\251\\327\\234\\327\\225\\327\\235.txt\""),
            "שלום.txt"
        );
    }

    #[test]
    fn test_unescape_persian() {
        // Persian "فارسی" (farsi)
        // ف = \331\201, ا = \330\247, ر = \330\261, س = \330\263, ی = \333\214
        assert_eq!(
            unescape_git_path("\"\\331\\201\\330\\247\\330\\261\\330\\263\\333\\214.txt\""),
            "فارسی.txt"
        );
    }

    #[test]
    fn test_unescape_urdu() {
        // Urdu "اردو" (urdu)
        // ا = \330\247, ر = \330\261, د = \330\257, و = \331\210
        assert_eq!(
            unescape_git_path("\"\\330\\247\\330\\261\\330\\257\\331\\210.txt\""),
            "اردو.txt"
        );
    }

    #[test]
    fn test_unescape_mixed_rtl_ltr() {
        // Mixed RTL/LTR: "test_مرحبا_file" (ASCII + Arabic + ASCII)
        assert_eq!(
            unescape_git_path(
                "\"test_\\331\\205\\330\\261\\330\\255\\330\\250\\330\\247_file.txt\""
            ),
            "test_مرحبا_file.txt"
        );
    }

    // =========================================================================
    // Phase 3: Indic Scripts Tests (Hindi, Tamil, Bengali, Telugu, Gujarati)
    // =========================================================================

    #[test]
    fn test_unescape_hindi_devanagari() {
        // Hindi "हिंदी" (Hindi in Devanagari script)
        // ह = \340\244\271, ि = \340\244\277, ं = \340\244\202, द = \340\244\246, ी = \340\245\200
        assert_eq!(
            unescape_git_path(
                "\"\\340\\244\\271\\340\\244\\277\\340\\244\\202\\340\\244\\246\\340\\245\\200.txt\""
            ),
            "हिंदी.txt"
        );
    }

    #[test]
    fn test_unescape_tamil() {
        // Tamil "தமிழ்" (Tamil)
        // த = \340\256\244, ம = \340\256\256, ி = \340\256\277, ழ = \340\256\264, ் = \340\257\215
        assert_eq!(
            unescape_git_path(
                "\"\\340\\256\\244\\340\\256\\256\\340\\256\\277\\340\\256\\264\\340\\257\\215.txt\""
            ),
            "தமிழ்.txt"
        );
    }

    #[test]
    fn test_unescape_bengali() {
        // Bengali "বাংলা" (Bangla)
        // ব = \340\246\254, া = \340\246\276, ং = \340\246\202, ল = \340\246\262, া = \340\246\276
        assert_eq!(
            unescape_git_path(
                "\"\\340\\246\\254\\340\\246\\276\\340\\246\\202\\340\\246\\262\\340\\246\\276.txt\""
            ),
            "বাংলা.txt"
        );
    }

    #[test]
    fn test_unescape_telugu() {
        // Telugu "తెలుగు" (Telugu)
        // త = \340\260\244, ె = \340\261\206, ల = \340\260\262, ు = \340\261\201, గ = \340\260\227, ు = \340\261\201
        assert_eq!(
            unescape_git_path(
                "\"\\340\\260\\244\\340\\261\\206\\340\\260\\262\\340\\261\\201\\340\\260\\227\\340\\261\\201.txt\""
            ),
            "తెలుగు.txt"
        );
    }

    #[test]
    fn test_unescape_gujarati() {
        // Gujarati "ગુજરાતી" (Gujarati)
        // ગ = \340\252\227, ુ = \340\253\201, જ = \340\252\234, ર = \340\252\260, ા = \340\252\276, ત = \340\252\244, ી = \340\253\200
        assert_eq!(
            unescape_git_path(
                "\"\\340\\252\\227\\340\\253\\201\\340\\252\\234\\340\\252\\260\\340\\252\\276\\340\\252\\244\\340\\253\\200.txt\""
            ),
            "ગુજરાતી.txt"
        );
    }

    // =========================================================================
    // Phase 4: Southeast Asian Scripts Tests (Thai, Vietnamese, Khmer, Lao)
    // =========================================================================

    #[test]
    fn test_unescape_thai() {
        // Thai "ไทย" (Thai)
        // ไ = \340\271\204, ท = \340\270\227, ย = \340\270\242
        assert_eq!(
            unescape_git_path("\"\\340\\271\\204\\340\\270\\227\\340\\270\\242.txt\""),
            "ไทย.txt"
        );
    }

    #[test]
    fn test_unescape_vietnamese() {
        // Vietnamese "tiếng" with tone marks
        // t = 't', i = 'i', ế = \341\272\277, n = 'n', g = 'g'
        assert_eq!(
            unescape_git_path("\"ti\\341\\272\\277ng.txt\""),
            "tiếng.txt"
        );
    }

    #[test]
    fn test_unescape_khmer() {
        // Khmer "ខ្មែរ" (Khmer)
        // ខ = \341\236\201, ្ = \341\237\222, ម = \341\236\230, ែ = \341\237\202, រ = \341\236\232
        assert_eq!(
            unescape_git_path(
                "\"\\341\\236\\201\\341\\237\\222\\341\\236\\230\\341\\237\\202\\341\\236\\232.txt\""
            ),
            "ខ្មែរ.txt"
        );
    }

    #[test]
    fn test_unescape_lao() {
        // Lao "ລາວ" (Lao)
        // ລ = \340\272\245, າ = \340\272\262, ວ = \340\272\247
        assert_eq!(
            unescape_git_path("\"\\340\\272\\245\\340\\272\\262\\340\\272\\247.txt\""),
            "ລາວ.txt"
        );
    }

    // =========================================================================
    // Phase 5: Cyrillic and Greek Scripts Tests
    // =========================================================================

    #[test]
    fn test_unescape_russian_cyrillic() {
        // Russian "Русский" (Russian)
        // Р = \320\240, у = \321\203, с = \321\201, к = \320\272, и = \320\270, й = \320\271
        assert_eq!(
            unescape_git_path(
                "\"\\320\\240\\321\\203\\321\\201\\321\\201\\320\\272\\320\\270\\320\\271.txt\""
            ),
            "Русский.txt"
        );
    }

    #[test]
    fn test_unescape_ukrainian_cyrillic() {
        // Ukrainian "Україна" (Ukraine)
        // У = \320\243, к = \320\272, р = \321\200, а = \320\260, ї = \321\227, н = \320\275, а = \320\260
        assert_eq!(
            unescape_git_path(
                "\"\\320\\243\\320\\272\\321\\200\\320\\260\\321\\227\\320\\275\\320\\260.txt\""
            ),
            "Україна.txt"
        );
    }

    #[test]
    fn test_unescape_greek() {
        // Greek "Ελλάδα" (Greece)
        // Ε = \316\225, λ = \316\273, λ = \316\273, ά = \316\254, δ = \316\264, α = \316\261
        assert_eq!(
            unescape_git_path(
                "\"\\316\\225\\316\\273\\316\\273\\316\\254\\316\\264\\316\\261.txt\""
            ),
            "Ελλάδα.txt"
        );
    }

    #[test]
    fn test_unescape_greek_polytonic() {
        // Greek polytonic "Ἑλληνική" (Hellenic with diacritics)
        // Ἑ = \341\274\231, λ = \316\273, λ = \316\273, η = \316\267, ν = \316\275, ι = \316\271, κ = \316\272, ή = \316\256
        assert_eq!(
            unescape_git_path(
                "\"\\341\\274\\231\\316\\273\\316\\273\\316\\267\\316\\275\\316\\271\\316\\272\\316\\256.txt\""
            ),
            "Ἑλληνική.txt"
        );
    }

    // =========================================================================
    // Phase 6: Extended Emoji Tests (ZWJ, skin tones, flags)
    // =========================================================================

    #[test]
    fn test_unescape_emoji_skin_tone() {
        // Emoji with skin tone modifier 👋🏽 = 👋 (U+1F44B) + 🏽 (U+1F3FD)
        // 👋 = \360\237\221\213, 🏽 = \360\237\217\275
        assert_eq!(
            unescape_git_path("\"\\360\\237\\221\\213\\360\\237\\217\\275.txt\""),
            "👋🏽.txt"
        );
    }

    #[test]
    fn test_unescape_emoji_zwj_sequence() {
        // ZWJ emoji sequence: 👨‍💻 (man technologist) = man + ZWJ + laptop
        // 👨 = \360\237\221\250, ZWJ = \342\200\215, 💻 = \360\237\222\273
        assert_eq!(
            unescape_git_path("\"\\360\\237\\221\\250\\342\\200\\215\\360\\237\\222\\273.txt\""),
            "👨‍💻.txt"
        );
    }

    #[test]
    fn test_unescape_emoji_flag() {
        // Flag emoji 🇯🇵 (Japan) = regional indicator J + regional indicator P
        // 🇯 = \360\237\207\257, 🇵 = \360\237\207\265
        assert_eq!(
            unescape_git_path("\"\\360\\237\\207\\257\\360\\237\\207\\265.txt\""),
            "🇯🇵.txt"
        );
    }

    #[test]
    fn test_unescape_multiple_emoji() {
        // Multiple emoji: 🚀🎉 (rocket + party)
        // 🚀 = \360\237\232\200, 🎉 = \360\237\216\211
        assert_eq!(
            unescape_git_path("\"\\360\\237\\232\\200\\360\\237\\216\\211.txt\""),
            "🚀🎉.txt"
        );
    }

    // =========================================================================
    // Phase 7: Special Unicode Characters Tests (math, currency, symbols)
    // =========================================================================

    #[test]
    fn test_unescape_math_symbols() {
        // Math symbols: ∑ (summation) = \342\210\221
        assert_eq!(unescape_git_path("\"\\342\\210\\221.txt\""), "∑.txt");
    }

    #[test]
    fn test_unescape_currency_symbols() {
        // Currency: € (euro) = \342\202\254
        assert_eq!(unescape_git_path("\"\\342\\202\\254.txt\""), "€.txt");
    }

    #[test]
    fn test_unescape_box_drawing() {
        // Box drawing: ┌ (box drawings light down and right) = \342\224\214
        assert_eq!(unescape_git_path("\"\\342\\224\\214.txt\""), "┌.txt");
    }

    #[test]
    fn test_unescape_dingbats() {
        // Dingbats: ✓ (check mark) = \342\234\223
        assert_eq!(unescape_git_path("\"\\342\\234\\223.txt\""), "✓.txt");
    }

    // =========================================================================
    // Phase 8: Unicode Normalization Tests (NFC vs NFD)
    // =========================================================================

    #[test]
    fn test_unescape_nfc_precomposed() {
        // NFC precomposed: é (U+00E9) = \303\251
        assert_eq!(unescape_git_path("\"caf\\303\\251.txt\""), "café.txt");
    }

    #[test]
    fn test_unescape_nfd_decomposed() {
        // NFD decomposed: e + combining acute (U+0065 + U+0301) = e + \314\201
        assert_eq!(
            unescape_git_path("\"cafe\\314\\201.txt\""),
            "cafe\u{0301}.txt"
        );
    }

    #[test]
    fn test_unescape_combining_diaeresis() {
        // Combining diaeresis: i + ̈ (U+0069 + U+0308) = i + \314\210
        assert_eq!(
            unescape_git_path("\"nai\\314\\210ve.txt\""),
            "nai\u{0308}ve.txt"
        );
    }

    #[test]
    fn test_unescape_angstrom() {
        // Å (A with ring above, U+00C5) = \303\205
        assert_eq!(
            unescape_git_path("\"\\303\\205ngstr\\303\\266m.txt\""),
            "Ångström.txt"
        );
    }

    // =========================================================================
    // Phase 9: Escape Sequence Edge Cases
    // =========================================================================

    #[test]
    fn test_unescape_incomplete_octal() {
        // Incomplete octal at end of string
        assert_eq!(unescape_git_path("\"file\\34\""), "file\x1c");
        assert_eq!(unescape_git_path("\"file\\3\""), "file\x03");
    }

    #[test]
    fn test_unescape_invalid_octal() {
        // Invalid octal digit (8 and 9 are not valid octal)
        assert_eq!(
            unescape_git_path("\"file\\389.txt\""),
            "file\x038\u{0039}.txt"
        );
    }

    #[test]
    fn test_unescape_backslash_only() {
        // Backslash at end without following character
        assert_eq!(unescape_git_path("\"file\\\""), "file\\");
    }

    #[test]
    fn test_unescape_mixed_escapes() {
        // Mix of different escape types
        assert_eq!(
            unescape_git_path("\"path\\nwith\\ttab\\\\and\\344\\270\\255.txt\""),
            "path\nwith\ttab\\and中.txt"
        );
    }

    #[test]
    fn test_unescape_empty_quoted() {
        // Empty quoted string
        assert_eq!(unescape_git_path("\"\""), "");
    }

    #[test]
    fn test_unescape_unmatched_quotes() {
        // Unmatched quotes - returned as-is
        assert_eq!(unescape_git_path("\"unmatched"), "\"unmatched");
        assert_eq!(unescape_git_path("unmatched\""), "unmatched\"");
    }

    // =========================================================================
    // normalize_to_posix Tests
    // =========================================================================

    #[test]
    fn test_normalize_to_posix_no_change() {
        // Already POSIX paths
        assert_eq!(normalize_to_posix("path/to/file.txt"), "path/to/file.txt");
        assert_eq!(normalize_to_posix("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn test_normalize_to_posix_windows() {
        // Windows paths
        assert_eq!(normalize_to_posix("path\\to\\file.txt"), "path/to/file.txt");
        assert_eq!(normalize_to_posix("C:\\Users\\file"), "C:/Users/file");
    }

    #[test]
    fn test_normalize_to_posix_mixed() {
        // Mixed separators
        assert_eq!(
            normalize_to_posix("path/to\\some\\file.txt"),
            "path/to/some/file.txt"
        );
    }

    #[test]
    fn test_normalize_to_posix_empty() {
        assert_eq!(normalize_to_posix(""), "");
    }

    // =========================================================================
    // current_git_ai_exe Tests
    // =========================================================================

    #[test]
    fn test_current_git_ai_exe_returns_path() {
        // Should return a path (either current exe or git-ai)
        let result = current_git_ai_exe();
        assert!(result.is_ok(), "current_git_ai_exe should not fail");
        let path = result.unwrap();
        assert!(!path.as_os_str().is_empty(), "path should not be empty");
    }

    // =========================================================================
    // is_interactive_terminal Tests
    // =========================================================================

    #[test]
    fn test_is_interactive_terminal() {
        // Just call it to ensure it doesn't panic
        let _ = is_interactive_terminal();
    }

    // =========================================================================
    // Platform-specific constants
    // =========================================================================

    #[cfg(windows)]
    #[test]
    fn test_create_no_window_constant() {
        // Verify the Windows constant is correct
        assert_eq!(CREATE_NO_WINDOW, 0x08000000);
    }

    #[cfg(windows)]
    #[test]
    fn test_create_new_process_group_constant() {
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x00000200);
    }

    #[cfg(windows)]
    #[test]
    fn test_create_breakaway_from_job_constant() {
        assert_eq!(CREATE_BREAKAWAY_FROM_JOB, 0x01000000);
    }

    // =========================================================================
    // Superuser Guard Tests
    // =========================================================================

    #[test]
    #[serial_test::serial]
    fn test_is_superuser_expected_environment_ci() {
        let had_ci = std::env::var_os("CI");
        unsafe { std::env::set_var("CI", "true") };
        assert!(is_superuser_expected_environment());
        match had_ci {
            Some(v) => unsafe { std::env::set_var("CI", v) },
            None => unsafe { std::env::remove_var("CI") },
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_superuser_is_allowed_env_var() {
        let had_var = std::env::var_os("GIT_AI_ALLOW_SUPERUSER");
        unsafe { std::env::set_var("GIT_AI_ALLOW_SUPERUSER", "1") };
        assert!(superuser_is_allowed());
        unsafe { std::env::set_var("GIT_AI_ALLOW_SUPERUSER", "true") };
        assert!(superuser_is_allowed());
        unsafe { std::env::set_var("GIT_AI_ALLOW_SUPERUSER", "TRUE") };
        assert!(superuser_is_allowed());
        unsafe { std::env::set_var("GIT_AI_ALLOW_SUPERUSER", "0") };
        assert!(!superuser_is_allowed());
        unsafe { std::env::remove_var("GIT_AI_ALLOW_SUPERUSER") };
        assert!(!superuser_is_allowed());
        match had_var {
            Some(v) => unsafe { std::env::set_var("GIT_AI_ALLOW_SUPERUSER", v) },
            None => unsafe { std::env::remove_var("GIT_AI_ALLOW_SUPERUSER") },
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_running_as_superuser_reports_correctly() {
        let euid = unsafe { libc::geteuid() };
        assert_eq!(is_running_as_superuser(), euid == 0);
    }
}
