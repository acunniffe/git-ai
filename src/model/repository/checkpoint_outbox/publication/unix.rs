use super::platform_acl;
pub(super) use super::unix_durability::validate_root_metadata;
use super::unix_durability::{open_directory_path, open_secure_root_directory};
use super::{OutboxLimits, PublishedRecord};
use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, Metadata};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const RECORD_MODE: u32 = 0o600;
const TEMP_CREATE_ATTEMPTS: usize = 16;

mod collision;

pub(super) fn publish_encoded(
    root: &Path,
    ready_name: &str,
    bytes: &[u8],
    limits: OutboxLimits,
) -> Result<PublishedRecord, CheckpointOutboxError> {
    let secure_root = SecureRoot::open(root)?;
    secure_root.try_lock()?;
    let ready_name = c_filename(ready_name)?;
    collision::reject_existing_delivery(root, &secure_root, ready_name.as_c_str(), bytes)?;
    enforce_capacity(&secure_root, bytes.len(), limits)?;

    let mut temporary = write_synced_temporary_record(secure_root.directory(), bytes)?;

    let rename_result = rename_no_replace(
        secure_root.as_raw_fd(),
        temporary.name.as_c_str(),
        ready_name.as_c_str(),
    );
    if let Err(error) = rename_result {
        if error.kind() == io::ErrorKind::AlreadyExists {
            collision::acknowledge_existing_delivery(
                root,
                &secure_root,
                ready_name.as_c_str(),
                bytes,
                SecureRoot::sync_all,
            )?;
            return Err(CheckpointOutboxError::AlreadyPublished);
        }
        return Err(io_error("publish ready record", error));
    }
    temporary.published = true;
    secure_root.sync_all()?;

    Ok(PublishedRecord {
        path: validate_acknowledged_path(root, &secure_root, ready_name.as_c_str())?,
        encoded_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

pub(super) fn write_private_marker(
    root: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<(), CheckpointOutboxError> {
    let secure_root = SecureRoot::open(root)?;
    // Failure markers must remain publishable when a ready-record writer has
    // exhausted the bounded wait for this root's advisory lock. Unique,
    // private temporary files plus atomic replacement keep concurrent marker
    // writes complete without participating in ready-record serialization.
    let mut temporary = write_synced_temporary_record(secure_root.directory(), bytes)?;
    let marker_name = c_filename(name)?;
    rename_replace(
        secure_root.as_raw_fd(),
        temporary.name.as_c_str(),
        marker_name.as_c_str(),
    )
    .map_err(|error| io_error("publish private marker", error))?;
    temporary.published = true;
    secure_root.sync_all()?;
    validate_acknowledged_path(root, &secure_root, marker_name.as_c_str())?;
    Ok(())
}

pub(super) struct SecureRoot {
    directory: File,
    identity: FileIdentity,
}

impl SecureRoot {
    pub(super) fn open(root: &Path) -> Result<Self, CheckpointOutboxError> {
        let directory = open_secure_root_directory(root)?;
        let metadata = directory
            .metadata()
            .map_err(|error| io_error("inspect root", error))?;
        Ok(Self {
            identity: FileIdentity::of(&metadata),
            directory,
        })
    }

    pub(super) fn try_lock(&self) -> Result<(), CheckpointOutboxError> {
        let result = unsafe { libc::flock(self.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(())
        } else {
            Err(io_error("lock root", io::Error::last_os_error()))
        }
    }

    pub(super) fn sync_all(&self) -> Result<(), CheckpointOutboxError> {
        self.directory
            .sync_all()
            .map_err(|error| io_error("sync outbox root", error))
    }

    fn directory(&self) -> &File {
        &self.directory
    }

    fn as_raw_fd(&self) -> RawFd {
        self.directory.as_raw_fd()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

struct DirectoryEntries {
    stream: *mut libc::DIR,
}

impl DirectoryEntries {
    fn open(directory_fd: RawFd) -> Result<Self, CheckpointOutboxError> {
        let dot = c".";
        let descriptor = unsafe {
            libc::openat(
                directory_fd,
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io_error("scan root", io::Error::last_os_error()));
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(descriptor);
            }
            return Err(io_error("scan root", error));
        }
        Ok(Self { stream })
    }

    fn next_name(&mut self) -> Result<Option<CString>, CheckpointOutboxError> {
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(self.stream) };
            if entry.is_null() {
                let errno = current_errno();
                return if errno == 0 {
                    Ok(None)
                } else {
                    Err(io_error("scan root", io::Error::from_raw_os_error(errno)))
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            return Ok(Some(name.to_owned()));
        }
    }
}

impl Drop for DirectoryEntries {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.stream);
        }
    }
}

#[cfg(target_os = "linux")]
fn capacity_record_len(directory_fd: RawFd, name: &CStr) -> Result<u64, CheckpointOutboxError> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory_fd,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io_error("inspect ready record", io::Error::last_os_error()));
    }
    let metadata = unsafe { metadata.assume_init() };
    let mode = u32::from(metadata.st_mode);
    if mode & u32::from(libc::S_IFMT) != u32::from(libc::S_IFREG)
        || metadata.st_uid != unsafe { libc::geteuid() }
        || mode & 0o777 != RECORD_MODE
        || metadata.st_nlink as u64 != 1
    {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    u64::try_from(metadata.st_size).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)
}

#[cfg(target_os = "macos")]
fn capacity_record_len(directory_fd: RawFd, name: &CStr) -> Result<u64, CheckpointOutboxError> {
    let record =
        open_record_at(directory_fd, name).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    Ok(validate_record_file(&record)?.len())
}

fn enforce_capacity(
    secure_root: &SecureRoot,
    new_record_bytes: usize,
    limits: OutboxLimits,
) -> Result<(), CheckpointOutboxError> {
    let mut ready_records = 0usize;
    let mut ready_bytes = 0u64;
    let mut entries = DirectoryEntries::open(secure_root.as_raw_fd())?;
    while let Some(name) = entries.next_name()? {
        let name_bytes = name.to_bytes();
        let is_ready = name_bytes.ends_with(b".ready");
        let is_temporary = name_bytes.starts_with(b".") && name_bytes.ends_with(b".tmp");
        if !is_ready && !is_temporary {
            continue;
        }
        let record_len = capacity_record_len(secure_root.as_raw_fd(), name.as_c_str())?;
        ready_records = ready_records.saturating_add(1);
        ready_bytes = ready_bytes.saturating_add(record_len);
    }

    let new_record_bytes = u64::try_from(new_record_bytes).unwrap_or(u64::MAX);
    if ready_records >= limits.max_ready_records
        || ready_bytes.saturating_add(new_record_bytes) > limits.max_ready_bytes
    {
        return Err(CheckpointOutboxError::ReadyCapacityExceeded {
            ready_records,
            max_records: limits.max_ready_records,
            ready_bytes,
            max_bytes: limits.max_ready_bytes,
        });
    }
    Ok(())
}

fn validate_acknowledged_path(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &CStr,
) -> Result<PathBuf, CheckpointOutboxError> {
    let published_record = open_record_at(secure_root.as_raw_fd(), ready_name)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    validate_acknowledged_record(root, secure_root, ready_name, &published_record)
}

fn validate_acknowledged_record(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &CStr,
    published_record: &File,
) -> Result<PathBuf, CheckpointOutboxError> {
    let published_metadata = validate_record_file(published_record)?;
    let published_identity = FileIdentity::of(&published_metadata);

    let acknowledged_root =
        open_directory_path(root).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    let acknowledged_root_metadata = acknowledged_root
        .metadata()
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    validate_root_metadata(&acknowledged_root_metadata, unsafe { libc::geteuid() })
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    platform_acl::reject_unsafe(&acknowledged_root)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    if FileIdentity::of(&acknowledged_root_metadata) != secure_root.identity {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    let acknowledged_record = open_record_at(acknowledged_root.as_raw_fd(), ready_name)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    let acknowledged_metadata = validate_record_file(&acknowledged_record)?;
    if FileIdentity::of(&acknowledged_metadata) != published_identity {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    Ok(root.join(OsStr::from_bytes(ready_name.to_bytes())))
}

fn open_record_at(directory_fd: RawFd, name: &CStr) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            directory_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

pub(in crate::model::repository::checkpoint_outbox) fn open_private_record_at(
    directory: &File,
    name: &OsStr,
) -> Result<File, CheckpointOutboxError> {
    let name =
        CString::new(name.as_bytes()).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    let record = open_record_at(directory.as_raw_fd(), name.as_c_str())
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    validate_record_file(&record)?;
    Ok(record)
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(target_os = "macos")]
fn current_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "linux")]
fn clear_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(target_os = "linux")]
fn current_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(test)]
pub(super) fn test_open_secure_root(root: &Path) -> Result<SecureRoot, CheckpointOutboxError> {
    SecureRoot::open(root)
}

#[cfg(test)]
pub(super) fn test_enforce_capacity_for_open_root(
    secure_root: &SecureRoot,
    _root: &Path,
    new_record_bytes: usize,
    limits: OutboxLimits,
) -> Result<(), CheckpointOutboxError> {
    enforce_capacity(secure_root, new_record_bytes, limits)
}

#[cfg(test)]
pub(super) fn test_validate_acknowledged_path(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &str,
) -> Result<PathBuf, CheckpointOutboxError> {
    validate_acknowledged_path(root, secure_root, c_filename(ready_name)?.as_c_str())
}

fn create_temporary_record(
    directory: &File,
) -> Result<(File, TemporaryRecord), CheckpointOutboxError> {
    for _ in 0..TEMP_CREATE_ATTEMPTS {
        let name = c_filename(&format!(".{}.tmp", crate::uuid::generate_v4()))?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                RECORD_MODE as libc::c_uint,
            )
        };
        if descriptor >= 0 {
            let file = unsafe { File::from_raw_fd(descriptor) };
            return Ok((
                file,
                TemporaryRecord {
                    directory_fd: directory.as_raw_fd(),
                    name,
                    published: false,
                },
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(io_error("create temporary record", error));
        }
    }
    Err(io_error(
        "create temporary record",
        io::Error::new(io::ErrorKind::AlreadyExists, "temporary name collisions"),
    ))
}

fn write_synced_temporary_record(
    directory: &File,
    bytes: &[u8],
) -> Result<TemporaryRecord, CheckpointOutboxError> {
    let (mut file, temporary) = create_temporary_record(directory)?;
    file.set_permissions(fs::Permissions::from_mode(RECORD_MODE))
        .map_err(|error| io_error("set record permissions", error))?;
    platform_acl::clear_inherited(&file)?;
    validate_record_file(&file)?;
    file.write_all(bytes)
        .map_err(|error| io_error("write temporary record", error))?;
    file.flush()
        .map_err(|error| io_error("flush temporary record", error))?;
    file.sync_all()
        .map_err(|error| io_error("sync temporary record", error))?;
    Ok(temporary)
}

fn validate_record_metadata(metadata: &Metadata) -> Result<(), CheckpointOutboxError> {
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != RECORD_MODE
        || metadata.nlink() != 1
    {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(())
}

fn validate_record_file(file: &File) -> Result<Metadata, CheckpointOutboxError> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect ready record", error))?;
    validate_record_metadata(&metadata)?;
    platform_acl::reject_unsafe(file)?;
    Ok(metadata)
}

struct TemporaryRecord {
    directory_fd: RawFd,
    name: CString,
    published: bool,
}

impl Drop for TemporaryRecord {
    fn drop(&mut self) {
        if !self.published {
            unsafe {
                libc::unlinkat(self.directory_fd, self.name.as_ptr(), 0);
            }
        }
    }
}

fn c_filename(name: &str) -> Result<CString, CheckpointOutboxError> {
    CString::new(name).map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)
}

fn rename_replace(directory_fd: RawFd, source: &CStr, destination: &CStr) -> io::Result<()> {
    let result = unsafe {
        libc::renameat(
            directory_fd,
            source.as_ptr(),
            directory_fd,
            destination.as_ptr(),
        )
    };
    syscall_result(result)
}

#[cfg(target_os = "macos")]
fn rename_no_replace(
    directory_fd: RawFd,
    source: &std::ffi::CStr,
    destination: &std::ffi::CStr,
) -> io::Result<()> {
    let result = unsafe {
        libc::renameatx_np(
            directory_fd,
            source.as_ptr(),
            directory_fd,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    syscall_result(result)
}

#[cfg(target_os = "linux")]
fn rename_no_replace(
    directory_fd: RawFd,
    source: &std::ffi::CStr,
    destination: &std::ffi::CStr,
) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory_fd,
            source.as_ptr(),
            directory_fd,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    syscall_result(result as libc::c_int)
}

fn syscall_result(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn io_error(operation: &'static str, error: io::Error) -> CheckpointOutboxError {
    CheckpointOutboxError::Io {
        operation,
        kind: error.kind(),
    }
}
