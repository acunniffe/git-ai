use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "macos")]
use super::super::platform_acl;
#[cfg(target_os = "macos")]
use std::collections::VecDeque;
#[cfg(target_os = "macos")]
use std::ffi::{CStr, CString};
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

const GROUP_OR_OTHER_WRITABLE: u32 = 0o022;
const STICKY: u32 = 0o1000;

pub(super) fn validate_parent_edge(
    parent: &Metadata,
    child_uid: Option<u32>,
    effective_uid: u32,
) -> Result<(), CheckpointOutboxError> {
    if !parent.file_type().is_dir() {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    let parent_uid = parent.uid();
    if parent_uid != effective_uid && parent_uid != 0 {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    let mode = parent.mode();
    if mode & GROUP_OR_OTHER_WRITABLE == 0 {
        return Ok(());
    }
    if mode & STICKY == 0 || child_uid.is_some_and(|uid| uid != effective_uid) {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(())
}

pub(super) fn resolve_stable_root(
    root: &Path,
    effective_uid: u32,
) -> Result<PathBuf, CheckpointOutboxError> {
    if !root.is_absolute() {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    #[cfg(target_os = "macos")]
    validate_lexical_symlink_chain(root, effective_uid)?;

    let mut components = Vec::new();
    for component in root.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => components.push(name),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CheckpointOutboxError::UnsafeReadyRecord);
            }
        }
    }

    let mut existing_prefix = PathBuf::from("/");
    let mut missing_suffix: Vec<OsString> = Vec::new();
    let component_count = components.len();
    for (index, name) in components.into_iter().enumerate() {
        if !missing_suffix.is_empty() {
            missing_suffix.push(name.to_os_string());
            continue;
        }

        let candidate = existing_prefix.join(name);
        match fs::symlink_metadata(&candidate) {
            Ok(child) => {
                if child.file_type().is_symlink() && index + 1 == component_count {
                    return Err(CheckpointOutboxError::RootIsSymlink);
                }
                let parent = fs::metadata(&existing_prefix)
                    .map_err(|error| path_io_error("inspect root ancestor", error))?;
                validate_parent_edge(&parent, Some(child.uid()), effective_uid)?;
                existing_prefix = candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing_suffix.push(name.to_os_string());
            }
            Err(error) => return Err(path_io_error("inspect root component", error)),
        }
    }

    let mut resolved = fs::canonicalize(&existing_prefix)
        .map_err(|error| path_io_error("resolve root ancestor", error))?;
    for component in missing_suffix {
        resolved.push(component);
    }
    Ok(resolved)
}

#[cfg(target_os = "macos")]
fn validate_lexical_symlink_chain(
    root: &Path,
    effective_uid: u32,
) -> Result<(), CheckpointOutboxError> {
    const MAX_SYMLINKS: usize = 40;

    let mut pending = normalized_components(root)?;
    let mut directory = super::open_directory_path(Path::new("/"))
        .map_err(|error| path_io_error("open lexical root ancestor", error))?;
    let mut resolved_parent = PathBuf::from("/");
    let mut followed_symlinks = 0usize;

    while let Some(component) = pending.pop_front() {
        platform_acl::reject_unsafe(&directory)?;
        let name = CString::new(component.as_bytes())
            .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
        let file_type = match super::component_file_type(directory.as_raw_fd(), name.as_c_str()) {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(path_io_error("inspect lexical root component", error)),
        };

        if file_type == libc::S_IFLNK {
            followed_symlinks = followed_symlinks.saturating_add(1);
            if followed_symlinks > MAX_SYMLINKS {
                return Err(CheckpointOutboxError::UnsafeReadyRecord);
            }
            let target = read_link_at(directory.as_raw_fd(), name.as_c_str())?;
            let mut redirected = if target.is_absolute() {
                target
            } else {
                resolved_parent.join(target)
            };
            for remaining in &pending {
                redirected.push(remaining);
            }
            pending = normalized_components(&redirected)?;
            directory = super::open_directory_path(Path::new("/"))
                .map_err(|error| path_io_error("open lexical root ancestor", error))?;
            resolved_parent = PathBuf::from("/");
            continue;
        }

        if file_type != libc::S_IFDIR {
            return Ok(());
        }
        let parent_metadata = directory
            .metadata()
            .map_err(|error| path_io_error("inspect lexical root ancestor", error))?;
        let child = super::open_directory_at(directory.as_raw_fd(), name.as_c_str())
            .map_err(|error| path_io_error("open lexical root component", error))?;
        let child_metadata = child
            .metadata()
            .map_err(|error| path_io_error("inspect lexical root component", error))?;
        validate_parent_edge(&parent_metadata, Some(child_metadata.uid()), effective_uid)?;
        resolved_parent.push(&component);
        directory = child;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn normalized_components(path: &Path) -> Result<VecDeque<OsString>, CheckpointOutboxError> {
    if !path.is_absolute() {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => components.clear(),
            Component::CurDir => {}
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::ParentDir => {
                components.pop();
            }
            Component::Prefix(_) => return Err(CheckpointOutboxError::UnsafeReadyRecord),
        }
    }
    Ok(components.into())
}

#[cfg(target_os = "macos")]
fn read_link_at(
    directory_fd: std::os::fd::RawFd,
    name: &CStr,
) -> Result<PathBuf, CheckpointOutboxError> {
    const MAX_SYMLINK_BYTES: usize = 64 * 1024;

    let mut buffer = vec![0u8; 1_024];
    loop {
        let length = unsafe {
            libc::readlinkat(
                directory_fd,
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(path_io_error(
                "read lexical root symlink",
                io::Error::last_os_error(),
            ));
        }
        let length =
            usize::try_from(length).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_vec(buffer)));
        }
        if buffer.len() >= MAX_SYMLINK_BYTES {
            return Err(CheckpointOutboxError::UnsafeReadyRecord);
        }
        buffer.resize(buffer.len().saturating_mul(2).min(MAX_SYMLINK_BYTES), 0);
    }
}

fn path_io_error(operation: &'static str, error: io::Error) -> CheckpointOutboxError {
    CheckpointOutboxError::Io {
        operation,
        kind: error.kind(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn user_owned_private_parent_is_safe() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();

        validate_parent_edge(&metadata, Some(metadata.uid()), metadata.uid()).unwrap();
    }

    #[test]
    fn non_sticky_shared_parent_is_unsafe() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();

        assert!(matches!(
            validate_parent_edge(&metadata, Some(metadata.uid()), metadata.uid()),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
    }

    #[test]
    fn sticky_shared_parent_requires_user_owned_child() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let effective_uid = metadata.uid();

        validate_parent_edge(&metadata, Some(effective_uid), effective_uid).unwrap();
        assert!(matches!(
            validate_parent_edge(
                &metadata,
                Some(effective_uid.saturating_add(1)),
                effective_uid
            ),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
    }

    #[test]
    fn unprivileged_foreign_owner_is_unsafe() {
        let temp = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let foreign_effective_uid = metadata.uid().saturating_add(1);

        assert!(matches!(
            validate_parent_edge(&metadata, None, foreign_effective_uid),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn system_owned_var_alias_resolves_to_stable_private_ancestor() {
        let requested =
            Path::new("/var/tmp").join(format!("git-ai-outbox-{}", crate::uuid::generate_v4()));

        let resolved = resolve_stable_root(&requested, unsafe { libc::geteuid() }).unwrap();

        assert!(resolved.starts_with("/private/var/tmp"));
        assert!(!requested.exists());
    }
}
