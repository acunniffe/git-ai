use super::platform_acl;
use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use CheckpointOutboxError as E;
use std::ffi::CString;
use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

mod ancestor;

use ancestor::{resolve_stable_root, validate_parent_edge};

pub(super) const ROOT_MODE: u32 = 0o700;

pub(super) fn open_secure_root_directory(root: &Path) -> Result<File, CheckpointOutboxError> {
    open_secure_root_directory_with_parent_sync(root, |directory| directory.sync_all())
}

#[cfg(test)]
pub(super) fn validate_existing_root_path(root: &Path) -> Result<(), CheckpointOutboxError> {
    open_existing_root_path(root).map(drop)
}

pub(in crate::model::repository::checkpoint_outbox) fn open_existing_root_path(
    root: &Path,
) -> Result<File, CheckpointOutboxError> {
    traverse_secure_root(root, false, |_| Ok(()))
}

fn open_secure_root_directory_with_parent_sync<F>(
    root: &Path,
    sync_parent: F,
) -> Result<File, CheckpointOutboxError>
where
    F: FnMut(&File) -> io::Result<()>,
{
    traverse_secure_root(root, true, sync_parent)
}

fn traverse_secure_root<F>(
    root: &Path,
    create_missing: bool,
    mut sync_parent: F,
) -> Result<File, CheckpointOutboxError>
where
    F: FnMut(&File) -> io::Result<()>,
{
    let effective_uid = unsafe { libc::geteuid() };
    let resolved_root = resolve_stable_root(root, effective_uid)?;
    let mut components = Vec::new();
    for component in resolved_root.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => components.push(name),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(CheckpointOutboxError::UnsafeReadyRecord);
            }
        }
    }

    let mut directory = open_directory_path(Path::new("/"))
        .map_err(|error| E::from_io("open root ancestor", error))?;
    platform_acl::reject_unsafe(&directory)?;
    for (index, component) in components.iter().enumerate() {
        let is_final = index + 1 == components.len();
        let name = CString::new(component.as_bytes())
            .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
        let parent_metadata = directory
            .metadata()
            .map_err(|error| E::from_io("inspect root ancestor", error))?;
        match open_directory_at(directory.as_raw_fd(), name.as_c_str()) {
            Ok(child) => {
                let child_metadata = child
                    .metadata()
                    .map_err(|error| E::from_io("inspect root component", error))?;
                validate_parent_edge(&parent_metadata, Some(child_metadata.uid()), effective_uid)?;
                platform_acl::reject_unsafe(&child)?;
                sync_parent(&directory).map_err(|error| E::from_io("sync root parent", error))?;
                directory = child;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !create_missing {
                    return Err(E::from_io("open root component", error));
                }
                validate_parent_edge(&parent_metadata, None, effective_uid)?;
                let created = create_component(&directory, name.as_c_str())?;
                let child =
                    open_directory_at(directory.as_raw_fd(), name.as_c_str()).map_err(|error| {
                        classify_component_error(&directory, &name, is_final, error)
                    })?;
                if created {
                    platform_acl::clear_inherited(&child)?;
                }
                let metadata = child
                    .metadata()
                    .map_err(|error| E::from_io("inspect created root component", error))?;
                validate_root_metadata(&metadata, effective_uid)?;
                validate_parent_edge(&parent_metadata, Some(metadata.uid()), effective_uid)?;
                platform_acl::reject_unsafe(&child)?;
                sync_parent(&directory).map_err(|error| E::from_io("sync root parent", error))?;
                directory = child;
            }
            Err(error) => {
                return Err(classify_component_error(&directory, &name, is_final, error));
            }
        }
    }

    let metadata = directory
        .metadata()
        .map_err(|error| E::from_io("inspect root", error))?;
    validate_root_metadata(&metadata, effective_uid)?;
    platform_acl::reject_unsafe(&directory)?;

    let acknowledged = open_and_validate_existing_root(root)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    let acknowledged_metadata = acknowledged
        .metadata()
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    if metadata.dev() != acknowledged_metadata.dev()
        || metadata.ino() != acknowledged_metadata.ino()
    {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(directory)
}

fn create_component(parent: &File, name: &std::ffi::CStr) -> Result<bool, CheckpointOutboxError> {
    let result =
        unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), ROOT_MODE as libc::mode_t) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::AlreadyExists {
        Ok(false)
    } else {
        Err(E::from_io("create root component", error))
    }
}

fn classify_component_error(
    parent: &File,
    name: &std::ffi::CStr,
    is_final: bool,
    error: io::Error,
) -> CheckpointOutboxError {
    match component_file_type(parent.as_raw_fd(), name) {
        Ok(file_type) if file_type == libc::S_IFLNK && is_final => {
            CheckpointOutboxError::RootIsSymlink
        }
        Ok(_) if is_final => CheckpointOutboxError::RootIsNotDirectory,
        Ok(_) => CheckpointOutboxError::UnsafeReadyRecord,
        Err(_) => E::from_io("open root component", error),
    }
}

fn component_file_type(parent_fd: RawFd, name: &std::ffi::CStr) -> Result<libc::mode_t, io::Error> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mode = unsafe { metadata.assume_init() }.st_mode;
    Ok(mode & libc::S_IFMT)
}

fn open_and_validate_existing_root(root: &Path) -> Result<File, CheckpointOutboxError> {
    let directory = open_directory_path(root).map_err(|error| E::from_io("open root", error))?;
    let metadata = directory
        .metadata()
        .map_err(|error| E::from_io("inspect root", error))?;
    validate_root_metadata(&metadata, unsafe { libc::geteuid() })?;
    platform_acl::reject_unsafe(&directory)?;
    Ok(directory)
}

pub(super) fn validate_root_metadata(
    metadata: &Metadata,
    expected_uid: u32,
) -> Result<(), CheckpointOutboxError> {
    if !metadata.file_type().is_dir() {
        return Err(CheckpointOutboxError::RootIsNotDirectory);
    }
    if metadata.uid() != expected_uid {
        return Err(CheckpointOutboxError::RootOwnerMismatch {
            expected: expected_uid,
            actual: metadata.uid(),
        });
    }
    let actual_mode = metadata.mode() & 0o777;
    if actual_mode != ROOT_MODE {
        return Err(CheckpointOutboxError::RootModeMismatch {
            expected: ROOT_MODE,
            actual: actual_mode,
        });
    }
    Ok(())
}

pub(super) fn open_directory_path(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn open_directory_at(directory_fd: RawFd, name: &std::ffi::CStr) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            directory_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;

    #[test]
    fn secure_open_syncs_every_traversed_parent_edge() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("parent").join("outbox");
        let resolved_root = resolve_stable_root(&root, unsafe { libc::geteuid() }).unwrap();
        let expected_sync_calls = resolved_root
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count();
        let sync_calls = Cell::new(0usize);

        open_secure_root_directory_with_parent_sync(&root, |_| {
            sync_calls.set(sync_calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(sync_calls.get(), expected_sync_calls);
    }

    #[test]
    fn parent_sync_failure_prevents_durable_root_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("parent").join("outbox");

        let error = open_secure_root_directory_with_parent_sync(&root, |_| {
            Err(io::Error::other("injected parent fsync failure"))
        })
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointOutboxError::Io {
                operation: "sync root parent",
                kind: io::ErrorKind::Other,
            }
        ));
    }

    #[test]
    fn retry_resyncs_parent_after_final_root_creation_sync_failure() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let root = parent.join("outbox");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(ROOT_MODE)).unwrap();
        let expected_parent = fs::metadata(&parent).unwrap();

        let error = open_secure_root_directory_with_parent_sync(&root, |directory| {
            let metadata = directory.metadata()?;
            if metadata.dev() == expected_parent.dev()
                && metadata.ino() == expected_parent.ino()
                && root.exists()
            {
                return Err(io::Error::other("injected final-parent fsync failure"));
            }
            Ok(())
        })
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointOutboxError::Io {
                operation: "sync root parent",
                kind: io::ErrorKind::Other,
            }
        ));
        assert!(
            root.is_dir(),
            "mkdir succeeds before the injected parent fsync failure"
        );

        let final_parent_syncs = Cell::new(0usize);
        open_secure_root_directory_with_parent_sync(&root, |directory| {
            let metadata = directory.metadata()?;
            if metadata.dev() == expected_parent.dev() && metadata.ino() == expected_parent.ino() {
                final_parent_syncs.set(final_parent_syncs.get() + 1);
            }
            Ok(())
        })
        .unwrap();

        assert_eq!(
            final_parent_syncs.get(),
            1,
            "retry must durably acknowledge the parent edge left by the failed attempt"
        );
    }

    #[test]
    fn replacing_parent_path_during_sync_fails_root_acknowledgement() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let parked = temp.path().join("parked-parent");
        let root = parent.join("outbox");

        let error = open_secure_root_directory_with_parent_sync(&root, |_| {
            if root.exists() {
                fs::rename(&parent, &parked)?;
                fs::create_dir(&parent)?;
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
                fs::create_dir(parent.join("outbox"))?;
                fs::set_permissions(parent.join("outbox"), fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        })
        .unwrap_err();

        assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    }

    #[test]
    fn stable_user_owned_ancestor_symlink_is_canonicalized() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let parent = temp.path().join("parent-link");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &parent).unwrap();

        open_secure_root_directory(&parent.join("outbox")).unwrap();

        assert!(target.join("outbox").is_dir());
    }

    #[test]
    fn ancestor_symlink_in_non_sticky_shared_directory_is_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let target = temp.path().join("target");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, shared.join("parent-link")).unwrap();

        assert!(matches!(
            open_secure_root_directory(&shared.join("parent-link").join("outbox")),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        assert!(fs::read_dir(&target).unwrap().next().is_none());
    }

    #[test]
    fn non_sticky_world_writable_ancestor_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let unsafe_parent = temp.path().join("unsafe-parent");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();

        assert!(matches!(
            open_secure_root_directory(&unsafe_parent.join("outbox")),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
    }

    #[test]
    fn sticky_shared_ancestor_accepts_a_user_owned_child() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();

        open_secure_root_directory(&shared.join("managed").join("outbox")).unwrap();
    }

    #[test]
    fn foreign_owned_parent_policy_fails_closed() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let foreign_effective_uid = metadata.uid().saturating_add(1);

        assert!(matches!(
            validate_parent_edge(
                &metadata,
                Some(foreign_effective_uid),
                foreign_effective_uid
            ),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
    }

    #[test]
    fn read_only_validation_rejects_unsafe_ancestor_without_creating() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let unsafe_parent = temp.path().join("unsafe-parent");
        let root = unsafe_parent.join("outbox");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(
            validate_existing_root_path(&root),
            Err(CheckpointOutboxError::UnsafeReadyRecord)
        ));
        let missing = unsafe_parent.join("missing");
        assert!(validate_existing_root_path(&missing).is_err());
        assert!(!missing.exists());
    }
}
