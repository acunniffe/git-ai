use super::presets::ParsedHookEvent;
use crate::config::Config;
use crate::error::GitAiError;
use crate::model::checkpoint_delivery::CHECKPOINT_DELIVERY_MAX_FILES;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::operations::git::repository::{
    discover_repository_policy_location_no_git_exec, load_repository_policy_context_no_git_exec,
};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAuthorizationDenial {
    NoAllowedRepositories,
    FileLimitExceeded,
    RepositoryUnverifiable,
    RepositoryNotAllowed,
}

impl CheckpointAuthorizationDenial {
    pub fn user_message(self) -> &'static str {
        match self {
            Self::NoAllowedRepositories => {
                "Skipping checkpoint because no repositories are allowed; add one with `git-ai config --add allowed_repositories <path-or-url>`"
            }
            Self::FileLimitExceeded => {
                "Skipping checkpoint because checkpoint file count exceeds the supported limit"
            }
            Self::RepositoryUnverifiable => {
                "Skipping checkpoint because repository authorization could not be verified"
            }
            Self::RepositoryNotAllowed => {
                "Skipping checkpoint because repository is excluded or not in the allowed_repositories list"
            }
        }
    }
}

#[derive(Debug)]
pub enum CheckpointPresetOutcome {
    Authorized(Vec<CheckpointRequest>),
    Denied(CheckpointAuthorizationDenial),
}

pub(super) fn authorize_events(
    events: &mut [ParsedHookEvent],
    config: &Config,
) -> Result<(), CheckpointAuthorizationDenial> {
    for count in events.iter().filter_map(event_file_count) {
        validate_checkpoint_file_count(count)?;
    }
    if !config.has_allowed_repositories() {
        return Err(CheckpointAuthorizationDenial::NoAllowedRepositories);
    }

    let mut checked_repositories = HashSet::new();
    for event in events {
        match event {
            ParsedHookEvent::PreBashCall(event) => {
                authorize_bash_cwd(&mut event.context.cwd, config, &mut checked_repositories)?;
            }
            ParsedHookEvent::PostBashCall(event) => {
                authorize_bash_cwd(&mut event.context.cwd, config, &mut checked_repositories)?;
            }
            ParsedHookEvent::PreFileEdit(event) => {
                authorize_file_paths(
                    &event.context.cwd,
                    &mut event.file_paths,
                    event.dirty_files.as_mut(),
                    config,
                    &mut checked_repositories,
                )?;
            }
            ParsedHookEvent::PostFileEdit(event) => {
                authorize_file_paths(
                    &event.context.cwd,
                    &mut event.file_paths,
                    event.dirty_files.as_mut(),
                    config,
                    &mut checked_repositories,
                )?;
            }
            ParsedHookEvent::KnownHumanEdit(event) => {
                authorize_file_paths(
                    &event.cwd,
                    &mut event.file_paths,
                    event.dirty_files.as_mut(),
                    config,
                    &mut checked_repositories,
                )?;
            }
            ParsedHookEvent::UntrackedEdit(event) => {
                authorize_file_paths(
                    &event.cwd,
                    &mut event.file_paths,
                    None,
                    config,
                    &mut checked_repositories,
                )?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_checkpoint_file_count(
    count: usize,
) -> Result<(), CheckpointAuthorizationDenial> {
    if count > CHECKPOINT_DELIVERY_MAX_FILES {
        Err(CheckpointAuthorizationDenial::FileLimitExceeded)
    } else {
        Ok(())
    }
}

fn event_file_count(event: &ParsedHookEvent) -> Option<usize> {
    match event {
        ParsedHookEvent::PreFileEdit(event) => Some(event.file_paths.len()),
        ParsedHookEvent::PostFileEdit(event) => Some(event.file_paths.len()),
        ParsedHookEvent::KnownHumanEdit(event) => Some(event.file_paths.len()),
        ParsedHookEvent::UntrackedEdit(event) => Some(event.file_paths.len()),
        ParsedHookEvent::PreBashCall(_) | ParsedHookEvent::PostBashCall(_) => None,
    }
}

pub(super) fn authorize_repository_path(
    path: &Path,
    config: &Config,
) -> Result<(), CheckpointAuthorizationDenial> {
    authorize_repository_workdir(path, config).map(|_| ())
}

pub(super) fn authorize_repository_workdir(
    path: &Path,
    config: &Config,
) -> Result<PathBuf, CheckpointAuthorizationDenial> {
    if !config.has_allowed_repositories() {
        return Err(CheckpointAuthorizationDenial::NoAllowedRepositories);
    }
    authorize_path(path, config, &mut HashSet::new())
}

fn authorize_file_paths(
    cwd: &Path,
    file_paths: &mut [PathBuf],
    dirty_files: Option<&mut HashMap<PathBuf, String>>,
    config: &Config,
    checked_repositories: &mut HashSet<PathBuf>,
) -> Result<(), CheckpointAuthorizationDenial> {
    if file_paths.is_empty() {
        return Err(CheckpointAuthorizationDenial::RepositoryUnverifiable);
    }
    let mut dirty_key_updates = Vec::with_capacity(file_paths.len());
    for file_path in file_paths {
        let original_path = file_path.clone();
        let absolute_path = resolve_against_cwd(file_path, cwd);
        let resolved_path = resolve_file_target(&absolute_path)?;
        authorize_path(&resolved_path, config, checked_repositories)?;
        dirty_key_updates.push((original_path, absolute_path, resolved_path.clone()));
        *file_path = resolved_path;
    }
    if let Some(dirty_files) = dirty_files {
        for (original_path, absolute_path, resolved_path) in dirty_key_updates {
            let content = dirty_files
                .get(&original_path)
                .or_else(|| dirty_files.get(&absolute_path))
                .cloned();
            if let Some(content) = content {
                dirty_files.insert(resolved_path, content);
            }
        }
    }
    Ok(())
}

fn resolve_file_target(path: &Path) -> Result<PathBuf, CheckpointAuthorizationDenial> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            resolve_missing_file_target(path)
        }
        Err(_) => Err(CheckpointAuthorizationDenial::RepositoryUnverifiable),
    }
}

fn resolve_missing_file_target(path: &Path) -> Result<PathBuf, CheckpointAuthorizationDenial> {
    let mut ancestor = path.to_path_buf();
    let mut missing_components = Vec::<OsString>::new();
    let canonical_ancestor = loop {
        match ancestor.canonicalize() {
            Ok(canonical) => break canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor
                    .file_name()
                    .filter(|name| *name != "." && *name != "..")
                    .ok_or(CheckpointAuthorizationDenial::RepositoryUnverifiable)?;
                missing_components.push(component.to_os_string());
                if !ancestor.pop() {
                    return Err(CheckpointAuthorizationDenial::RepositoryUnverifiable);
                }
            }
            Err(_) => return Err(CheckpointAuthorizationDenial::RepositoryUnverifiable),
        }
    };

    let mut resolved = canonical_ancestor;
    for component in missing_components.into_iter().rev() {
        resolved.push(component);
    }
    match resolved.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        Err(_) => Err(CheckpointAuthorizationDenial::RepositoryUnverifiable),
    }
}

pub(super) enum CheckpointFileSnapshot {
    Missing,
    Oversized(u64),
    Read(Option<String>),
}

pub(super) fn read_checkpoint_file_snapshot(
    path: &Path,
    max_size: usize,
) -> Result<CheckpointFileSnapshot, GitAiError> {
    read_checkpoint_file_snapshot_with_after_open(path, max_size, || {})
}

fn read_checkpoint_file_snapshot_with_after_open<AfterOpen>(
    path: &Path,
    max_size: usize,
    after_open: AfterOpen,
) -> Result<CheckpointFileSnapshot, GitAiError>
where
    AfterOpen: FnOnce(),
{
    let canonical_before = match path.canonicalize() {
        Ok(canonical) if canonical == path => canonical,
        Ok(_) => return Err(file_identity_error()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckpointFileSnapshot::Missing);
        }
        Err(_) => return Err(file_identity_error()),
    };

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = match options.open(&canonical_before) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckpointFileSnapshot::Missing);
        }
        Err(error) if is_unsafe_open_error(&error) => return Err(file_identity_error()),
        Err(_) => return Ok(CheckpointFileSnapshot::Read(None)),
    };
    let opened_metadata = file.metadata().map_err(|_| file_identity_error())?;
    if !opened_metadata.file_type().is_file() {
        return Ok(CheckpointFileSnapshot::Read(None));
    }
    after_open();
    let canonical_after = path.canonicalize().map_err(|_| file_identity_error())?;
    if canonical_after != canonical_before {
        return Err(file_identity_error());
    }
    if !opened_file_matches_path(&file, &canonical_after).map_err(|_| file_identity_error())? {
        return Err(file_identity_error());
    }
    if opened_metadata.len() as usize > max_size {
        return Ok(CheckpointFileSnapshot::Oversized(opened_metadata.len()));
    }

    let read_limit = (max_size as u64).saturating_add(1);
    let mut bytes = Vec::with_capacity(opened_metadata.len().min(max_size as u64) as usize);
    if file.take(read_limit).read_to_end(&mut bytes).is_err() {
        return Ok(CheckpointFileSnapshot::Read(None));
    }
    if bytes.len() > max_size {
        return Ok(CheckpointFileSnapshot::Oversized(bytes.len() as u64));
    }
    Ok(CheckpointFileSnapshot::Read(String::from_utf8(bytes).ok()))
}

#[cfg(unix)]
fn opened_file_matches_path(file: &fs::File, path: &Path) -> io::Result<bool> {
    let opened = file.metadata()?;
    let current = fs::metadata(path)?;
    Ok(opened.dev() == current.dev() && opened.ino() == current.ino())
}

#[cfg(windows)]
fn opened_file_matches_path(file: &fs::File, path: &Path) -> io::Result<bool> {
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    let current = same_file::Handle::from_path(path)?;
    Ok(opened == current)
}

#[cfg(not(any(unix, windows)))]
fn opened_file_matches_path(_file: &fs::File, _path: &Path) -> io::Result<bool> {
    Ok(true)
}

fn is_unsafe_open_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

fn file_identity_error() -> GitAiError {
    GitAiError::PresetError("checkpoint file identity could not be verified".to_string())
}

fn authorize_path(
    path: &Path,
    config: &Config,
    checked_repositories: &mut HashSet<PathBuf>,
) -> Result<PathBuf, CheckpointAuthorizationDenial> {
    authorize_path_with_policy(
        path,
        checked_repositories,
        &mut |path| {
            let location = discover_repository_policy_location_no_git_exec(path)
                .map_err(|_| CheckpointAuthorizationDenial::RepositoryUnverifiable)?;
            Ok((location.repository_root().to_path_buf(), location))
        },
        &mut |location| {
            load_repository_policy_context_no_git_exec(&location)
                .map(|repository| repository.is_collection_allowed(config))
                .map_err(|_| CheckpointAuthorizationDenial::RepositoryUnverifiable)
        },
    )
}

fn authorize_bash_cwd(
    cwd: &mut PathBuf,
    config: &Config,
    checked_repositories: &mut HashSet<PathBuf>,
) -> Result<(), CheckpointAuthorizationDenial> {
    bind_bash_cwd_with(cwd, &mut |canonical_cwd| {
        authorize_path(canonical_cwd, config, checked_repositories).map(|_| ())
    })
}

fn bind_bash_cwd_with<Authorize>(
    cwd: &mut PathBuf,
    authorize: &mut Authorize,
) -> Result<(), CheckpointAuthorizationDenial>
where
    Authorize: FnMut(&Path) -> Result<(), CheckpointAuthorizationDenial>,
{
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|_| CheckpointAuthorizationDenial::RepositoryUnverifiable)?;
    authorize(&canonical_cwd)?;
    *cwd = canonical_cwd;
    Ok(())
}

fn authorize_path_with_policy<Location, Discover, IsAllowed>(
    path: &Path,
    checked_repositories: &mut HashSet<PathBuf>,
    discover: &mut Discover,
    is_allowed: &mut IsAllowed,
) -> Result<PathBuf, CheckpointAuthorizationDenial>
where
    Discover: FnMut(&Path) -> Result<(PathBuf, Location), CheckpointAuthorizationDenial>,
    IsAllowed: FnMut(Location) -> Result<bool, CheckpointAuthorizationDenial>,
{
    let (repository_root, location) = discover(path)?;
    if !checked_repositories.insert(repository_root.clone()) {
        return Ok(repository_root);
    }
    let allowed = is_allowed(location)?;
    if allowed {
        Ok(repository_root)
    } else {
        Err(CheckpointAuthorizationDenial::RepositoryNotAllowed)
    }
}

fn resolve_against_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn bash_cwd_binding_survives_symlink_retarget_after_authorization() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        let denied = root.join("denied");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&denied).unwrap();
        let cwd_link = root.join("cwd");
        symlink(&allowed, &cwd_link).unwrap();

        let mut cwd = cwd_link.clone();
        bind_bash_cwd_with(&mut cwd, &mut |candidate| {
            if candidate == allowed {
                Ok(())
            } else {
                Err(CheckpointAuthorizationDenial::RepositoryNotAllowed)
            }
        })
        .unwrap();

        fs::remove_file(&cwd_link).unwrap();
        symlink(&denied, &cwd_link).unwrap();

        assert_eq!(cwd, allowed);
        assert_eq!(cwd.canonicalize().unwrap(), allowed);
        assert_eq!(cwd_link.canonicalize().unwrap(), denied);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn snapshot_rejects_path_replacement_after_open() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("checkpoint.txt");
        let displaced = root.join("checkpoint-original.txt");
        let replacement = root.join("replacement.txt");
        fs::write(&path, "authorized bytes").unwrap();
        fs::write(&replacement, "replacement bytes").unwrap();

        let result = read_checkpoint_file_snapshot_with_after_open(&path, 1_024, || {
            fs::rename(&path, &displaced).unwrap();
            fs::rename(&replacement, &path).unwrap();
        });

        let Err(error) = result else {
            panic!("a path replacement must fail closed");
        };
        assert_eq!(error.to_string(), file_identity_error().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_file_growth_after_open_without_reading_past_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap().join("checkpoint.txt");
        fs::write(&path, "small").unwrap();

        let result = read_checkpoint_file_snapshot_with_after_open(&path, 8, || {
            fs::write(&path, "content that grew after the file was opened").unwrap();
        })
        .unwrap();

        let CheckpointFileSnapshot::Oversized(size) = result else {
            panic!("growth beyond the read limit must be rejected");
        };
        assert_eq!(size, 9, "only one byte beyond the limit should be read");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap().join("checkpoint.fifo");
        let native_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(native_path.as_ptr(), 0o600) }, 0);

        let result = read_checkpoint_file_snapshot(&path, 1_024).unwrap();

        assert!(matches!(result, CheckpointFileSnapshot::Read(None)));
    }

    #[test]
    fn same_repository_policy_is_loaded_once_after_location_discovery() {
        let mut checked_repositories = HashSet::new();
        let mut location_discovery_calls = 0;
        let mut policy_load_calls = 0;
        let mut discover = |_path: &Path| {
            location_discovery_calls += 1;
            Ok((PathBuf::from("/repo"), ()))
        };
        let mut is_allowed = |()| {
            policy_load_calls += 1;
            Ok(true)
        };

        for path in [Path::new("/repo/a.rs"), Path::new("/repo/b.rs")] {
            authorize_path_with_policy(
                path,
                &mut checked_repositories,
                &mut discover,
                &mut is_allowed,
            )
            .unwrap();
        }

        assert_eq!(location_discovery_calls, 2);
        assert_eq!(
            policy_load_calls, 1,
            "same-repository paths must not reparse policy config"
        );
    }
}
