use crate::error::GitAiError;
use std::path::Path;

pub(super) fn configure(binary_path: &Path, dry_run: bool) -> Result<(), GitAiError> {
    if dry_run {
        return Ok(());
    }

    let install_dir = binary_path.parent().ok_or_else(|| {
        GitAiError::Generic("could not determine git-ai install directory".to_string())
    })?;

    #[cfg(windows)]
    configure_windows(install_dir);

    #[cfg(not(windows))]
    configure_unix(install_dir)?;

    Ok(())
}

#[cfg(not(windows))]
fn detect_unix_shells(
    home: &Path,
    login_shell: Option<&std::ffi::OsStr>,
) -> Vec<(&'static str, std::path::PathBuf)> {
    let mut shells = Vec::new();
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");
    let zshrc = home.join(".zshrc");
    let fish = home.join(".config").join("fish").join("config.fish");

    if bashrc.is_file() {
        shells.push(("bash", bashrc));
    } else if bash_profile.is_file() {
        shells.push(("bash", bash_profile));
    }
    if zshrc.is_file() {
        shells.push(("zsh", zshrc));
    }
    if fish.is_file() {
        shells.push(("fish", fish));
    }

    if shells.is_empty() {
        let login_shell = login_shell
            .and_then(|shell| Path::new(shell).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (name, path) = match login_shell.as_str() {
            "fish" => ("fish", ".config/fish/config.fish"),
            "zsh" => ("zsh", ".zshrc"),
            _ => ("bash", ".bashrc"),
        };
        shells.push((name, home.join(path)));
    }

    shells
}

#[cfg(not(windows))]
fn configure_unix(install_dir: &Path) -> Result<(), GitAiError> {
    use chrono::Local;
    use std::fs;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;

    if is_package_store_install_dir(install_dir) {
        println!(
            "\nSkipping shell PATH updates for package-managed install at {}.",
            install_dir.display()
        );
        return Ok(());
    }

    let home = crate::mdm::utils::home_dir();
    let persisted_install_dir = persisted_unix_install_dir(install_dir, &home);
    let install_dir_bytes = persisted_install_dir.as_os_str().as_bytes();
    let install_dir_display = persisted_install_dir.to_string_lossy();
    let escaped_posix_install_dir = escape_posix_double_quoted_shell_bytes(install_dir_bytes);
    let escaped_fish_install_dir = escape_fish_double_quoted_shell_bytes(install_dir_bytes);
    let mut configured = Vec::new();
    let mut already_configured = Vec::new();
    let mut created_paths = Vec::new();
    let mut first_error = None;
    let login_shell = std::env::var_os("SHELL");

    for (shell_name, config_file) in detect_unix_shells(&home, login_shell.as_deref()) {
        let path_command = if shell_name == "fish" {
            let Some(config_dir) = config_file.parent() else {
                first_error.get_or_insert_with(|| {
                    GitAiError::Generic(format!(
                        "could not determine parent directory for {}",
                        config_file.display()
                    ))
                });
                continue;
            };
            if !crate::utils::is_running_as_superuser() && !config_dir.is_dir() {
                if let Err(error) = fs::create_dir_all(config_dir) {
                    first_error.get_or_insert_with(|| error.into());
                    continue;
                }
                created_paths.push(config_dir.to_path_buf());
            }
            let mut command = b"fish_add_path -g \"".to_vec();
            command.extend_from_slice(&escaped_fish_install_dir);
            command.extend_from_slice(b"\"");
            command
        } else {
            let mut command = b"export PATH=\"".to_vec();
            command.extend_from_slice(&escaped_posix_install_dir);
            command.extend_from_slice(b":$PATH\"");
            command
        };

        let existing = fs::read(&config_file).unwrap_or_default();
        let contains_install_dir = contains_bytes(&existing, install_dir_bytes)
            || contains_bytes(&existing, &path_command);

        if contains_install_dir {
            already_configured.push((shell_name, config_file));
            continue;
        }

        let write_result = (|| -> Result<(), GitAiError> {
            let mut file = open_unix_shell_profile(
                &home,
                &config_file,
                crate::utils::is_running_as_superuser(),
                &mut created_paths,
            )?;
            writeln!(file)?;
            writeln!(
                file,
                "# Added by git-ai installer on {}",
                Local::now().format("%a %b %e %T %Z %Y")
            )?;
            file.write_all(&path_command)?;
            file.write_all(b"\n")?;
            Ok(())
        })();
        match write_result {
            Ok(()) => configured.push((shell_name, config_file)),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }

    if !configured.is_empty() {
        println!("\nUpdated shell configurations:");
        for (_, config_file) in &configured {
            println!("\x1b[0;32m  ✓ {}\x1b[0m", config_file.display());
        }

        println!("\nTo apply changes immediately:");
        for (shell_name, config_file) in &configured {
            println!("  - For {shell_name}: source {}", config_file.display());
        }
    }

    if !already_configured.is_empty() {
        println!("\nAlready configured (no changes needed):");
        for (_, config_file) in &already_configured {
            println!("  ✓ {}", config_file.display());
        }
    }

    if configured.is_empty() && already_configured.is_empty() {
        println!("\nCould not detect any shell config files.");
        println!("Please add the following line to your shell config and restart:");
        println!("  export PATH=\"{install_dir_display}:$PATH\"");
    }

    repair_install_ownership(&home, &created_paths);

    println!("\n\x1b[0;33mClose and reopen your terminal and IDE sessions to use git-ai.\x1b[0m");
    first_error.map_or(Ok(()), Err)
}

#[cfg(not(windows))]
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(not(windows))]
fn escape_posix_double_quoted_shell_bytes(value: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(value.len());
    for byte in value {
        if matches!(byte, b'\\' | b'"' | b'$' | b'`') {
            escaped.push(b'\\');
        }
        escaped.push(*byte);
    }
    escaped
}

#[cfg(not(windows))]
fn escape_fish_double_quoted_shell_bytes(value: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(value.len());
    for byte in value {
        if matches!(byte, b'\\' | b'"' | b'$') {
            escaped.push(b'\\');
        }
        escaped.push(*byte);
    }
    escaped
}

#[cfg(not(windows))]
fn open_unix_shell_profile(
    home: &Path,
    config_file: &Path,
    is_superuser: bool,
    created_paths: &mut Vec<std::path::PathBuf>,
) -> Result<std::fs::File, GitAiError> {
    use std::fs::OpenOptions;
    if !is_superuser {
        let config_was_created = !config_file.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(config_file)?;
        if config_was_created {
            created_paths.push(config_file.to_path_buf());
        }
        return Ok(file);
    }

    open_unix_shell_profile_beneath_home(home, config_file, created_paths)
}

#[cfg(not(windows))]
fn open_unix_shell_profile_beneath_home(
    home: &Path,
    config_file: &Path,
    created_paths: &mut Vec<std::path::PathBuf>,
) -> Result<std::fs::File, GitAiError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let relative = config_file.strip_prefix(home).map_err(|_| {
        GitAiError::Generic(format!(
            "shell profile {} is outside home {}",
            config_file.display(),
            home.display()
        ))
    })?;
    let canonical_home = home.canonicalize()?;
    let mut lexical_parent = canonical_home.clone();
    let home = CString::new(canonical_home.as_os_str().as_bytes())
        .map_err(|_| GitAiError::Generic(format!("home path contains NUL: {}", home.display())))?;
    // SAFETY: `home` is a live, NUL-terminated path. O_NOFOLLOW prevents the
    // canonical home itself from being replaced with a symlink before open.
    let home_fd = unsafe {
        libc::open(
            home.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if home_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `open` returned a new owned file descriptor.
    let mut directory = unsafe { OwnedFd::from_raw_fd(home_fd) };
    let components: Vec<_> = relative.components().collect();
    let (file_name, parents) = components.split_last().ok_or_else(|| {
        GitAiError::Generic(format!("invalid shell profile: {}", config_file.display()))
    })?;
    for component in parents {
        let name = CString::new(component.as_os_str().as_bytes()).map_err(|_| {
            GitAiError::Generic(format!(
                "shell profile contains NUL: {}",
                config_file.display()
            ))
        })?;
        lexical_parent.push(component.as_os_str());
        // SAFETY: the directory fd and component name are valid for the call.
        let mut next = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if next < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            // SAFETY: the directory fd and component name are valid for the call.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o777) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            created_paths.push(lexical_parent.clone());
            // SAFETY: the just-created entry is opened without following links.
            next = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
        if next < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: `openat` returned a new owned file descriptor.
        directory = unsafe { OwnedFd::from_raw_fd(next) };
    }

    let name = CString::new(file_name.as_os_str().as_bytes()).map_err(|_| {
        GitAiError::Generic(format!(
            "shell profile contains NUL: {}",
            config_file.display()
        ))
    })?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the directory fd, component name, and output pointer are valid.
    let metadata_status = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    let config_was_created = if metadata_status == 0 {
        false
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
        true
    } else {
        return Err(std::io::Error::last_os_error().into());
    };
    // SAFETY: the directory fd and component name are valid for the call.
    let file_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666,
        )
    };
    if file_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if config_was_created {
        created_paths.push(config_file.to_path_buf());
    }
    // SAFETY: `openat` returned a new owned file descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(file_fd) })
}

#[cfg(not(windows))]
fn is_package_store_install_dir(install_dir: &Path) -> bool {
    install_dir.starts_with("/nix/store")
}

#[cfg(not(windows))]
fn persisted_unix_install_dir(install_dir: &Path, home: &Path) -> std::path::PathBuf {
    let managed = home.join(".git-ai").join("bin");
    if managed
        .canonicalize()
        .is_ok_and(|canonical| canonical == install_dir)
    {
        managed
    } else {
        install_dir.to_path_buf()
    }
}

#[cfg(not(windows))]
fn ownership_repair_uid(is_superuser: bool, resolved_user_uid: Option<u32>) -> Option<u32> {
    if !is_superuser {
        return None;
    }
    resolved_user_uid.filter(|uid| *uid != 0)
}

#[cfg(not(windows))]
fn resolved_user_uid_for_home(
    home: &Path,
    resolved_uid: u32,
    resolved_home: Option<&Path>,
) -> Option<u32> {
    (resolved_uid != 0 && resolved_home == Some(home)).then_some(resolved_uid)
}

#[cfg(not(windows))]
fn repair_install_ownership(home: &Path, created_shell_paths: &[std::path::PathBuf]) {
    let resolved_user_uid = resolved_install_user_uid(home);
    let Some(owner_uid) =
        ownership_repair_uid(crate::utils::is_running_as_superuser(), resolved_user_uid)
    else {
        return;
    };

    chown_recursively(&home.join(".git-ai"), owner_uid);
    for path in created_shell_paths {
        chown_path(path, owner_uid);
    }
}

#[cfg(not(windows))]
fn resolved_install_user_uid(home: &Path) -> Option<u32> {
    let install_user = std::env::var_os("GIT_AI_INSTALL_USER")?;
    let (uid, user_home) = user_info_for_name(&install_user)?;
    resolved_user_uid_for_home(home, uid, Some(&user_home))
}

#[cfg(not(windows))]
fn user_info_for_name(user: &std::ffi::OsStr) -> Option<(u32, std::path::PathBuf)> {
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let user = CString::new(user.as_bytes()).ok()?;

    let initial_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if initial_size > 0 {
        initial_size as usize
    } else {
        16 * 1024
    };
    loop {
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getpwnam_r(
                user.as_ptr(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer_size < 1024 * 1024 {
            buffer_size *= 2;
            continue;
        }
        if status != 0 || result.is_null() {
            return None;
        }
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return None;
        }
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        return Some((
            passwd.pw_uid,
            std::path::PathBuf::from(OsStr::from_bytes(home)),
        ));
    }
}

#[cfg(not(windows))]
fn chown_recursively(path: &Path, owner_uid: u32) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            chown_recursively(&entry.path(), owner_uid);
        }
    }
    chown_path(path, owner_uid);
}

#[cfg(not(windows))]
fn chown_path(path: &Path, owner_uid: u32) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: `path` is NUL-terminated and remains alive for the call. Passing
    // `gid_t::MAX` preserves the existing group, matching `chown USER PATH`.
    unsafe {
        libc::lchown(path.as_ptr(), owner_uid, libc::gid_t::MAX);
    }
}

#[cfg(all(test, not(windows)))]
mod unix_tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn unix_shell_detection_prefers_bashrc_and_includes_every_existing_shell() {
        let temp = tempdir().unwrap();
        let home = temp.path();
        fs::write(home.join(".bashrc"), "").unwrap();
        fs::write(home.join(".bash_profile"), "").unwrap();
        fs::write(home.join(".zshrc"), "").unwrap();
        fs::create_dir_all(home.join(".config/fish")).unwrap();
        fs::write(home.join(".config/fish/config.fish"), "").unwrap();

        assert_eq!(
            detect_unix_shells(home, Some(OsStr::new("/bin/bash"))),
            vec![
                ("bash", home.join(".bashrc")),
                ("zsh", home.join(".zshrc")),
                ("fish", home.join(".config/fish/config.fish")),
            ]
        );
    }

    #[test]
    fn unix_shell_detection_falls_back_to_login_shell_or_bash() {
        for (login_shell, expected_name, expected_path) in [
            ("/usr/local/bin/fish", "fish", ".config/fish/config.fish"),
            ("/bin/zsh", "zsh", ".zshrc"),
            ("/bin/bash", "bash", ".bashrc"),
            ("/bin/unknown", "bash", ".bashrc"),
            ("", "bash", ".bashrc"),
        ] {
            let temp = tempdir().unwrap();
            assert_eq!(
                detect_unix_shells(temp.path(), Some(OsStr::new(login_shell))),
                vec![(expected_name, temp.path().join(expected_path))]
            );
        }
    }

    #[test]
    fn unix_shell_paths_escape_double_quote_expansions_without_rewriting_newlines() {
        assert_eq!(
            escape_posix_double_quoted_shell_bytes(b"/tmp/a\\b\"$c`d\ne"),
            b"/tmp/a\\\\b\\\"\\$c\\`d\ne"
        );
        assert_eq!(
            escape_fish_double_quoted_shell_bytes(b"/tmp/a\\b\"$c`d\ne"),
            b"/tmp/a\\\\b\\\"\\$c`d\ne"
        );
    }

    #[test]
    fn unix_superuser_profile_open_refuses_symlinked_parent_directories() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        let outside = temp.path().join("outside");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, home.join(".config")).unwrap();
        let profile = home.join(".config/fish/config.fish");

        assert!(open_unix_shell_profile_beneath_home(&home, &profile, &mut Vec::new()).is_err());
        assert!(!outside.join("fish/config.fish").exists());
    }

    #[test]
    fn unix_superuser_profile_open_creates_missing_components_beneath_home() {
        let temp = tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let profile = home.join(".config/fish/config.fish");
        let mut created = Vec::new();

        drop(open_unix_shell_profile_beneath_home(&home, &profile, &mut created).unwrap());

        assert!(profile.is_file());
        assert_eq!(
            created,
            vec![home.join(".config"), home.join(".config/fish"), profile]
        );
    }

    #[test]
    fn unix_ownership_repair_falls_back_to_the_resolved_user_for_superuser_installs() {
        assert_eq!(ownership_repair_uid(false, Some(502)), None);
        assert_eq!(ownership_repair_uid(true, None), None);
        assert_eq!(ownership_repair_uid(true, Some(0)), None);
        assert_eq!(ownership_repair_uid(true, Some(502)), Some(502));
    }

    #[test]
    fn unix_resolved_user_must_be_non_root_and_match_the_install_home() {
        let home = Path::new("/Users/alice");
        assert_eq!(resolved_user_uid_for_home(home, 501, Some(home)), Some(501));
        assert_eq!(resolved_user_uid_for_home(home, 0, Some(home)), None);
        assert_eq!(
            resolved_user_uid_for_home(home, 501, Some(Path::new("/Users/bob"))),
            None
        );
    }

    #[test]
    fn unix_package_store_paths_are_not_persisted_to_shell_profiles() {
        assert!(is_package_store_install_dir(Path::new(
            "/nix/store/abc123-git-ai/bin"
        )));
        assert!(!is_package_store_install_dir(Path::new(
            "/nix/storehouse/git-ai/bin"
        )));
        assert!(!is_package_store_install_dir(Path::new(
            "/home/alice/.git-ai/bin"
        )));
    }

    #[test]
    fn unix_managed_install_preserves_the_home_spelling() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let real_home = temp.path().join("real-home");
        let linked_home = temp.path().join("linked-home");
        fs::create_dir_all(real_home.join(".git-ai/bin")).unwrap();
        symlink(&real_home, &linked_home).unwrap();
        let canonical_install = real_home.join(".git-ai/bin").canonicalize().unwrap();

        assert_eq!(
            persisted_unix_install_dir(&canonical_install, &linked_home),
            linked_home.join(".git-ai/bin")
        );
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserPathStatus {
    Updated,
    AlreadyPresent,
    Error,
    Skipped,
}

#[cfg(windows)]
fn normalize_windows_path(path: &str) -> String {
    let trimmed = path.trim();
    let absolute = std::path::absolute(trimmed)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.to_string());
    absolute.trim_end_matches('\\').to_lowercase()
}

#[cfg(windows)]
fn path_contains_windows_entry(path: &str, path_to_add: &str) -> bool {
    let normalized_add = normalize_windows_path(path_to_add);
    path.split(';')
        .filter(|entry| !entry.trim().is_empty())
        .any(|entry| normalize_windows_path(entry) == normalized_add)
}

#[cfg(windows)]
fn append_windows_path(path: &str, path_to_add: &str) -> String {
    if path.is_empty() {
        path_to_add.to_string()
    } else {
        format!("{path};{path_to_add}")
    }
}

#[cfg(windows)]
fn expand_windows_environment_variables(value: &str) -> std::io::Result<String> {
    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

    let source: Vec<u16> = OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut required =
        unsafe { ExpandEnvironmentStringsW(source.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        return Err(std::io::Error::last_os_error());
    }

    loop {
        let mut expanded = vec![0; required as usize];
        let written =
            unsafe { ExpandEnvironmentStringsW(source.as_ptr(), expanded.as_mut_ptr(), required) };
        if written == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if written > required {
            required = written;
            continue;
        }

        expanded.truncate(written.saturating_sub(1) as usize);
        return Ok(OsString::from_wide(&expanded)
            .to_string_lossy()
            .into_owned());
    }
}

#[cfg(windows)]
struct WindowsUserPath {
    raw: String,
    expanded: String,
    is_expandable: bool,
}

#[cfg(windows)]
fn read_windows_user_path(environment: &winreg::RegKey) -> std::io::Result<WindowsUserPath> {
    use winreg::enums::REG_EXPAND_SZ;
    use winreg::types::FromRegValue;

    let value = match environment.get_raw_value("Path") {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WindowsUserPath {
                raw: String::new(),
                expanded: String::new(),
                is_expandable: false,
            });
        }
        Err(error) => return Err(error),
    };
    let raw = String::from_reg_value(&value).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported user PATH registry value: {error}"),
        )
    })?;
    let is_expandable = value.vtype == REG_EXPAND_SZ;
    let expanded = if is_expandable {
        expand_windows_environment_variables(&raw)?
    } else {
        raw.clone()
    };
    Ok(WindowsUserPath {
        raw,
        expanded,
        is_expandable,
    })
}

#[cfg(windows)]
fn ensure_windows_user_path_in_registry(
    environment: &winreg::RegKey,
    path_to_add: &str,
) -> std::io::Result<UserPathStatus> {
    let user_path = read_windows_user_path(environment)?;
    if path_contains_windows_entry(&user_path.expanded, path_to_add) {
        return Ok(UserPathStatus::AlreadyPresent);
    }

    let new_user_path = append_windows_path(&user_path.raw, path_to_add);
    if user_path.is_expandable {
        use winreg::enums::REG_EXPAND_SZ;
        use winreg::types::ToRegValue;

        let mut value = new_user_path.to_reg_value();
        value.vtype = REG_EXPAND_SZ;
        environment.set_raw_value("Path", &value)?;
    } else {
        environment.set_value("Path", &new_user_path)?;
    }
    Ok(UserPathStatus::Updated)
}

#[cfg(windows)]
fn ensure_windows_user_path(install_dir: &Path) -> UserPathStatus {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    let path_to_add = install_dir.to_string_lossy();
    (|| -> std::io::Result<UserPathStatus> {
        let environment = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)?;
        let status = ensure_windows_user_path_in_registry(&environment, &path_to_add)?;
        if status == UserPathStatus::Updated {
            broadcast_windows_environment_change();
        }
        Ok(status)
    })()
    .unwrap_or(UserPathStatus::Error)
}

#[cfg(windows)]
fn broadcast_windows_environment_change() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    let environment: Vec<u16> = std::ffi::OsStr::new("Environment")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut result = 0;
    // SAFETY: the string remains live and NUL-terminated for the synchronous call.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5_000,
            &mut result,
        );
    }
}

#[cfg(windows)]
fn windows_home_for_shell_profiles(git_root: Option<&Path>) -> std::path::PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| native_windows_home(&home.to_string_lossy(), git_root))
        .unwrap_or_else(crate::mdm::utils::home_dir)
}

#[cfg(windows)]
fn native_windows_home(home: &str, git_root: Option<&Path>) -> std::path::PathBuf {
    let bytes = home.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
        return std::path::PathBuf::from(format!(
            "{}:\\{}",
            (bytes[1] as char).to_ascii_uppercase(),
            home[3..].replace('/', "\\")
        ));
    }
    if let Some(unc) = home.strip_prefix("//") {
        return std::path::PathBuf::from(format!(r"\\{}", unc.replace('/', "\\")));
    }
    if let Some(relative) = home.strip_prefix('/')
        && let Some(git_root) = git_root
    {
        return git_root.join(relative.replace('/', "\\"));
    }
    std::path::PathBuf::from(home)
}

#[cfg(windows)]
fn git_bash_path(install_dir: &Path, home: &Path) -> String {
    let managed_install_dir = home.join(".git-ai").join("bin");
    if normalize_windows_path(&install_dir.to_string_lossy())
        == normalize_windows_path(&managed_install_dir.to_string_lossy())
    {
        return "$HOME/.git-ai/bin".to_string();
    }

    let mut path = install_dir.to_string_lossy().replace('\\', "/");
    if let Some(unc) = path.strip_prefix("//?/UNC/") {
        path = format!("//{unc}");
    } else if let Some(without_verbatim_prefix) = path.strip_prefix("//?/") {
        path = without_verbatim_prefix.to_string();
    }
    if path.as_bytes().get(1) == Some(&b':') && matches!(path.as_bytes().get(2), Some(b'/')) {
        let drive = path[..1].to_lowercase();
        path = format!("/{drive}/{}", &path[3..]);
    }
    path.replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

#[cfg(windows)]
fn windows_profile_contains(path: &Path, marker: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.starts_with(&[0xff, 0xfe]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&units).contains(marker);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        return String::from_utf16_lossy(&units).contains(marker);
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    bytes
        .windows(marker.len())
        .any(|window| window == marker.as_bytes())
}

#[cfg(windows)]
fn append_windows_profile(path: &Path, content: &str) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let existing = std::fs::read(path).unwrap_or_default();
    let bytes = if existing.starts_with(&[0xff, 0xfe]) {
        content
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    } else if existing.starts_with(&[0xfe, 0xff]) {
        content
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>()
    } else {
        content.as_bytes().to_vec()
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&bytes)
}

#[cfg(windows)]
fn configure_git_bash(install_dir: &Path) -> Result<(), GitAiError> {
    use chrono::Local;
    let git_bash = [
        std::env::var_os("ProgramFiles")
            .map(std::path::PathBuf::from)
            .map(|path| path.join(r"Git\bin\bash.exe")),
        std::env::var_os("ProgramFiles(x86)")
            .map(std::path::PathBuf::from)
            .map(|path| path.join(r"Git\bin\bash.exe")),
        std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .map(|path| path.join(r"Programs\Git\bin\bash.exe")),
    ]
    .into_iter()
    .flatten()
    .find(|path| path.exists());

    let Some(git_bash) = git_bash else {
        return Ok(());
    };
    let git_root = git_bash.parent().and_then(Path::parent);

    let home = windows_home_for_shell_profiles(git_root);
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");
    let target = if bashrc.exists() {
        bashrc
    } else if bash_profile.exists() {
        bash_profile
    } else {
        bashrc
    };

    let git_bash_path = git_bash_path(install_dir, &home);
    if windows_profile_contains(&target, &git_bash_path)
        || (git_bash_path == "$HOME/.git-ai/bin"
            && windows_profile_contains(&target, ".git-ai/bin"))
    {
        println!(
            "\x1b[0;32mGit Bash already configured ({})\x1b[0m",
            target.display()
        );
        return Ok(());
    }

    append_windows_profile(
        &target,
        &format!(
            "\n# Added by git-ai installer on {}\nexport PATH=\"{}:$PATH\"\n",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            git_bash_path
        ),
    )?;
    println!(
        "\x1b[0;32mSuccessfully configured Git Bash ({})\x1b[0m",
        target.display()
    );
    Ok(())
}

#[cfg(windows)]
fn configure_windows(install_dir: &Path) {
    let path_status = if std::env::var("GIT_AI_SKIP_PATH_UPDATE").as_deref() == Ok("1") {
        eprintln!("Skipping PATH updates because GIT_AI_SKIP_PATH_UPDATE=1");
        UserPathStatus::Skipped
    } else {
        ensure_windows_user_path(install_dir)
    };

    match path_status {
        UserPathStatus::Updated => {
            println!("\x1b[0;32mSuccessfully added git-ai to the user PATH.\x1b[0m");
        }
        UserPathStatus::AlreadyPresent => {
            println!("\x1b[0;32mgit-ai already present in the user PATH.\x1b[0m");
        }
        UserPathStatus::Error => eprintln!("Failed to update the user PATH."),
        UserPathStatus::Skipped => {}
    }

    if let Err(error) = configure_git_bash(install_dir) {
        eprintln!("Warning: Failed to configure Git Bash: {error}");
    }

    println!("\x1b[0;33mClose and reopen your terminal and IDE sessions to use git-ai.\x1b[0m");
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn git_bash_path_preserves_the_managed_home_expression() {
        assert_eq!(
            git_bash_path(
                Path::new(r"C:\Users\Alice\.git-ai\bin"),
                Path::new(r"C:\Users\Alice")
            ),
            "$HOME/.git-ai/bin"
        );
    }

    #[test]
    fn git_bash_home_converts_msys_drive_and_unc_paths() {
        assert_eq!(
            native_windows_home("/c/Users/Alice", None),
            Path::new(r"C:\Users\Alice")
        );
        assert_eq!(
            native_windows_home("//server/share/Alice", None),
            Path::new(r"\\server\share\Alice")
        );
        assert_eq!(
            native_windows_home(r"D:\Users\Alice", None),
            Path::new(r"D:\Users\Alice")
        );
        assert_eq!(
            native_windows_home("/home/alice", Some(Path::new(r"C:\Program Files\Git"))),
            Path::new(r"C:\Program Files\Git\home\alice")
        );
    }

    #[test]
    fn git_bash_path_converts_custom_drive_and_unc_paths() {
        let home = Path::new(r"C:\Users\Alice");
        assert_eq!(
            git_bash_path(Path::new(r"D:\Tools\git-ai"), home),
            "/d/Tools/git-ai"
        );
        assert_eq!(
            git_bash_path(Path::new(r"\\server\share\git-ai"), home),
            "//server/share/git-ai"
        );
        assert_eq!(
            git_bash_path(Path::new(r"\\?\UNC\server\share\git-ai"), home),
            "//server/share/git-ai"
        );
    }

    #[test]
    fn windows_profile_marker_detection_handles_bom_encodings() {
        let temp = tempfile::tempdir().unwrap();
        let marker = "$HOME/.git-ai/bin";

        let utf8 = temp.path().join("utf8-bom");
        let mut utf8_contents = vec![0xef, 0xbb, 0xbf];
        utf8_contents.extend_from_slice(marker.as_bytes());
        std::fs::write(&utf8, utf8_contents).unwrap();
        assert!(windows_profile_contains(&utf8, marker));

        let utf16 = temp.path().join("utf16-le");
        let mut utf16_contents = vec![0xff, 0xfe];
        for unit in marker.encode_utf16() {
            utf16_contents.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&utf16, utf16_contents).unwrap();
        assert!(windows_profile_contains(&utf16, marker));
    }

    #[test]
    fn windows_profile_append_preserves_utf16_encoding() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("utf16-le");
        let mut contents = vec![0xff, 0xfe];
        for unit in "# existing\r\n".encode_utf16() {
            contents.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&profile, contents).unwrap();

        append_windows_profile(&profile, "export PATH=\"$HOME/.git-ai/bin:$PATH\"\r\n").unwrap();
        let contents = std::fs::read(&profile).unwrap();
        assert!(contents.starts_with(&[0xff, 0xfe]));
        assert!(windows_profile_contains(&profile, "$HOME/.git-ai/bin"));
    }

    #[test]
    fn windows_expandable_registry_path_preserves_raw_value_and_type() {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, REG_EXPAND_SZ};
        use winreg::types::{FromRegValue, ToRegValue};

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key_path = format!(
            r"Software\git-ai-shell-env-test-{}",
            crate::uuid::generate_v4()
        );
        let (environment, _) = hkcu.create_subkey(&key_path).unwrap();
        let mut value = r"%USERPROFILE%\bin".to_reg_value();
        value.vtype = REG_EXPAND_SZ;
        environment.set_raw_value("Path", &value).unwrap();

        let expanded_existing = format!(
            r"{}\bin",
            std::env::var("USERPROFILE").expect("USERPROFILE must be set on Windows")
        );
        assert_eq!(
            ensure_windows_user_path_in_registry(&environment, &expanded_existing).unwrap(),
            UserPathStatus::AlreadyPresent
        );
        let unchanged = environment.get_raw_value("Path").unwrap();
        assert_eq!(unchanged.vtype, REG_EXPAND_SZ);
        assert_eq!(
            String::from_reg_value(&unchanged).unwrap(),
            r"%USERPROFILE%\bin"
        );

        let status = ensure_windows_user_path_in_registry(&environment, r"C:\git-ai\bin").unwrap();
        let stored = environment.get_raw_value("Path").unwrap();
        drop(environment);
        hkcu.delete_subkey_all(&key_path).unwrap();

        assert_eq!(status, UserPathStatus::Updated);
        assert_eq!(stored.vtype, REG_EXPAND_SZ);
        assert_eq!(
            String::from_reg_value(&stored).unwrap(),
            r"%USERPROFILE%\bin;C:\git-ai\bin"
        );
    }

    #[test]
    fn windows_unsupported_registry_path_is_left_unchanged() {
        use winreg::RegKey;
        use winreg::enums::{HKEY_CURRENT_USER, REG_BINARY};
        use winreg::types::ToRegValue;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key_path = format!(
            r"Software\git-ai-shell-env-test-{}",
            crate::uuid::generate_v4()
        );
        let (environment, _) = hkcu.create_subkey(&key_path).unwrap();
        let mut original = "invalid".to_reg_value();
        original.bytes = vec![1, 2, 3, 4];
        original.vtype = REG_BINARY;
        environment.set_raw_value("Path", &original).unwrap();

        let result = ensure_windows_user_path_in_registry(&environment, r"C:\git-ai\bin");
        let unchanged = environment.get_raw_value("Path").unwrap();
        drop(environment);
        hkcu.delete_subkey_all(&key_path).unwrap();

        assert!(result.is_err());
        assert_eq!(unchanged.vtype, original.vtype);
        assert_eq!(unchanged.bytes, original.bytes);
    }

    #[test]
    fn windows_path_matching_normalizes_case_whitespace_and_trailing_slashes() {
        let path = r" C:\Windows ;c:\Users\Alice\.git-ai\bin\;";
        assert!(path_contains_windows_entry(
            path,
            r"C:\Users\Alice\.git-ai\bin"
        ));
    }

    #[test]
    fn windows_path_matching_does_not_accept_parent_or_child_paths() {
        let path = r"C:\Users\Alice\.git-ai;C:\Users\Alice\.git-ai\bin-tools";
        assert!(!path_contains_windows_entry(
            path,
            r"C:\Users\Alice\.git-ai\bin"
        ));
    }

    #[test]
    fn windows_path_append_preserves_the_existing_value_exactly() {
        assert_eq!(
            append_windows_path(r"C:\Windows;", r"C:\Users\Alice\.git-ai\bin"),
            r"C:\Windows;;C:\Users\Alice\.git-ai\bin"
        );
        assert_eq!(
            append_windows_path("", r"C:\Users\Alice\.git-ai\bin"),
            r"C:\Users\Alice\.git-ai\bin"
        );
    }
}
