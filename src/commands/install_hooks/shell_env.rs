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
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    if is_package_store_install_dir(install_dir) {
        println!(
            "\nSkipping shell PATH updates for package-managed install at {}.",
            install_dir.display()
        );
        return Ok(());
    }

    let home = crate::mdm::utils::home_dir();
    let install_dir = install_dir.to_string_lossy();
    let mut configured = Vec::new();
    let mut already_configured = Vec::new();
    let mut created_paths = Vec::new();
    let login_shell = std::env::var_os("SHELL");

    for (shell_name, config_file) in detect_unix_shells(&home, login_shell.as_deref()) {
        let path_command = if shell_name == "fish" {
            let config_dir = config_file.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    "could not determine parent directory for {}",
                    config_file.display()
                ))
            })?;
            if !config_dir.is_dir() {
                fs::create_dir_all(config_dir)?;
                created_paths.push(config_dir.to_path_buf());
            }
            format!("fish_add_path -g \"{install_dir}\"")
        } else {
            format!("export PATH=\"{install_dir}:$PATH\"")
        };

        if !config_file.is_file() {
            created_paths.push(config_file.clone());
        }

        let existing = fs::read(&config_file).unwrap_or_default();
        let install_dir_bytes = install_dir.as_bytes();
        let contains_install_dir = !install_dir_bytes.is_empty()
            && existing
                .windows(install_dir_bytes.len())
                .any(|window| window == install_dir_bytes);

        if contains_install_dir {
            already_configured.push((shell_name, config_file));
            continue;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&config_file)?;
        writeln!(file)?;
        writeln!(
            file,
            "# Added by git-ai installer on {}",
            Local::now().format("%a %b %e %T %Z %Y")
        )?;
        writeln!(file, "{path_command}")?;
        configured.push((shell_name, config_file));
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
        println!("  export PATH=\"{install_dir}:$PATH\"");
    }

    repair_install_ownership(&home, &created_paths);

    println!("\n\x1b[0;33mClose and reopen your terminal and IDE sessions to use git-ai.\x1b[0m");
    Ok(())
}

#[cfg(not(windows))]
fn is_package_store_install_dir(install_dir: &Path) -> bool {
    install_dir.starts_with("/nix/store")
}

#[cfg(not(windows))]
fn ownership_repair_uid(
    is_superuser: bool,
    home_owner_uid: Option<u32>,
    resolved_user_uid: Option<u32>,
) -> Option<u32> {
    if !is_superuser {
        return None;
    }
    home_owner_uid
        .filter(|uid| *uid != 0)
        .or(resolved_user_uid.filter(|uid| *uid != 0))
}

#[cfg(all(not(windows), any(test, target_os = "macos")))]
fn resolved_user_uid_for_home(
    home: &Path,
    resolved_uid: u32,
    resolved_home: Option<&Path>,
) -> Option<u32> {
    (resolved_uid != 0 && resolved_home == Some(home)).then_some(resolved_uid)
}

#[cfg(not(windows))]
fn repair_install_ownership(home: &Path, created_shell_paths: &[std::path::PathBuf]) {
    use std::os::unix::fs::MetadataExt;

    let home_owner_uid = std::fs::metadata(home).ok().map(|metadata| metadata.uid());
    let resolved_user_uid = resolved_install_user_uid(home);
    let Some(owner_uid) = ownership_repair_uid(
        crate::utils::is_running_as_superuser(),
        home_owner_uid,
        resolved_user_uid,
    ) else {
        return;
    };

    chown_recursively(&home.join(".git-ai"), owner_uid);
    for path in created_shell_paths {
        chown_path(path, owner_uid);
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn resolved_install_user_uid(_home: &Path) -> Option<u32> {
    None
}

#[cfg(target_os = "macos")]
fn resolved_install_user_uid(home: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;

    let console_uid = std::fs::metadata("/dev/console").ok()?.uid();
    let console_home = user_home_for_uid(console_uid);
    resolved_user_uid_for_home(home, console_uid, console_home.as_deref())
}

#[cfg(target_os = "macos")]
fn user_home_for_uid(uid: u32) -> Option<std::path::PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: `sysconf` has no pointer arguments or caller-owned invariants.
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
        // SAFETY: `passwd`, `buffer`, and `result` remain valid for the call, and
        // the supplied buffer length matches the allocation.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
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

        // SAFETY: a successful `getpwuid_r` call initialized `passwd`.
        let passwd = unsafe { passwd.assume_init() };
        if passwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: `pw_dir` is non-null and points into the live `buffer`.
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
        return Some(std::path::PathBuf::from(OsStr::from_bytes(home)));
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
    // SAFETY: `path` is NUL-terminated and remains alive for the duration of the call.
    // Match `chown USER PATH`: update the owner while preserving the group.
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
    fn unix_ownership_repair_falls_back_to_the_resolved_user_for_superuser_installs() {
        assert_eq!(ownership_repair_uid(false, Some(501), Some(502)), None);
        assert_eq!(ownership_repair_uid(true, Some(0), None), None);
        assert_eq!(ownership_repair_uid(true, Some(501), Some(502)), Some(501));
        assert_eq!(ownership_repair_uid(true, Some(0), Some(502)), Some(502));
    }

    #[test]
    fn unix_resolved_user_must_be_non_root_and_match_the_install_home() {
        let home = Path::new("/Users/alice");
        assert_eq!(
            resolved_user_uid_for_home(home, 501, Some(Path::new("/Users/alice"))),
            Some(501)
        );
        assert_eq!(
            resolved_user_uid_for_home(home, 0, Some(Path::new("/Users/alice"))),
            None
        );
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
    let raw = String::from_reg_value(&value).unwrap_or_default();
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
fn configure_git_bash() -> Result<(), GitAiError> {
    use chrono::Local;
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    let git_bash_installed = [
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
    .any(|path| path.exists());

    if !git_bash_installed {
        return Ok(());
    }

    let home = crate::mdm::utils::home_dir();
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");
    let target = if bashrc.exists() {
        bashrc
    } else if bash_profile.exists() {
        bash_profile
    } else {
        bashrc
    };

    let existing = fs::read_to_string(&target).unwrap_or_default();
    if existing.contains(".git-ai/bin") {
        println!(
            "\x1b[0;32mGit Bash already configured ({})\x1b[0m",
            target.display()
        );
        return Ok(());
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&target)?;
    write!(
        file,
        "\n# Added by git-ai installer on {}\nexport PATH=\"$HOME/.git-ai/bin:$PATH\"\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
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

    if let Err(error) = configure_git_bash() {
        eprintln!("Warning: Failed to configure Git Bash: {error}");
    }

    println!("\x1b[0;33mClose and reopen your terminal and IDE sessions to use git-ai.\x1b[0m");
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

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
