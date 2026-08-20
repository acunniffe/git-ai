//! Consume side of the checkpoint outbox: scanning, reading, removing, and
//! quarantining ready records. Used by the daemon's replay worker.
//!
//! Every operation revalidates the root and record with the same checks the
//! publish path uses, and mutations run under the root's advisory lock so
//! they serialize against concurrent publishers.

use super::unix::{
    DirectoryEntries, SecureRoot, c_filename, open_record_at, rename_replace, validate_record_file,
};
use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use CheckpointOutboxError as E;
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime};

const READY_SUFFIX: &str = ".ready";
const QUARANTINE_SUFFIX: &str = ".quarantined";

/// The ready records visible in one scan, oldest first, plus any entries
/// whose names or file metadata failed validation (for quarantining).
#[derive(Debug, Default)]
pub struct ReadyScan {
    pub ready: Vec<String>,
    pub invalid: Vec<String>,
}

/// Lists up to `limit` ready-record names, oldest first. The zero-padded
/// `captured_at` prefix of `ready_filename` makes lexicographic order the
/// design's `(captured_at, batch_id, batch_ordinal, delivery_id)` order.
/// A missing root is an empty scan, not an error.
pub fn scan_ready_records(root: &Path, limit: usize) -> Result<ReadyScan, CheckpointOutboxError> {
    if !root.exists() {
        return Ok(ReadyScan::default());
    }
    let secure_root = SecureRoot::open(root)?;
    secure_root.try_lock()?;

    let mut scan = ReadyScan::default();
    let mut entries = DirectoryEntries::open(secure_root.as_raw_fd())?;
    while let Some(name) = entries.next_name()? {
        let Ok(name) = name.into_string() else {
            continue;
        };
        if !name.ends_with(READY_SUFFIX) {
            continue;
        }
        if is_safe_record_name(&name) {
            scan.ready.push(name);
        } else {
            scan.invalid.push(name);
        }
    }
    scan.ready.sort();
    scan.ready.truncate(limit);
    Ok(scan)
}

/// Reads and returns one ready record's raw bytes, revalidating the record
/// file exactly like the publish acknowledgement path. The caller decodes.
pub fn read_ready_record(
    root: &Path,
    name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, CheckpointOutboxError> {
    require_record_name(name)?;
    let secure_root = SecureRoot::open(root)?;
    let record = open_record_at(secure_root.as_raw_fd(), c_filename(name)?.as_c_str())
        .map_err(|error| E::from_io("open ready record", error))?;
    let metadata = validate_record_file(&record)?;
    if metadata.len() > max_bytes {
        return Err(CheckpointOutboxError::RecordTooLarge {
            encoded_bytes: metadata.len(),
            max_bytes,
        });
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    record
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| E::from_io("read ready record", error))?;
    Ok(bytes)
}

/// Removes a fully-processed record. Safe to call again after a crash between
/// apply and removal: replay of the surviving record deduplicates by
/// delivery id and lands back here.
pub fn remove_ready_record(root: &Path, name: &str) -> Result<(), CheckpointOutboxError> {
    require_record_name(name)?;
    let secure_root = SecureRoot::open(root)?;
    secure_root.try_lock()?;
    unlink_at(&secure_root, name)?;
    secure_root.sync_all()
}

/// Moves a record out of the replay set (rename `.ready` -> `.quarantined`)
/// while keeping it on disk for inspection until retention expires.
pub fn quarantine_ready_record(root: &Path, name: &str) -> Result<(), CheckpointOutboxError> {
    require_record_name(name)?;
    let quarantined = format!(
        "{}{}",
        name.trim_end_matches(READY_SUFFIX),
        QUARANTINE_SUFFIX
    );
    let secure_root = SecureRoot::open(root)?;
    secure_root.try_lock()?;
    rename_replace(
        secure_root.as_raw_fd(),
        c_filename(name)?.as_c_str(),
        c_filename(&quarantined)?.as_c_str(),
    )
    .map_err(|error| E::from_io("quarantine ready record", error))?;
    secure_root.sync_all()
}

/// Deletes quarantined records older than `retention`, returning how many
/// were removed. Uses file mtime, which the quarantining rename preserves
/// from publication time.
pub fn prune_quarantined_records(
    root: &Path,
    retention: Duration,
) -> Result<usize, CheckpointOutboxError> {
    if !root.exists() {
        return Ok(0);
    }
    let secure_root = SecureRoot::open(root)?;
    secure_root.try_lock()?;

    let mut stale = Vec::new();
    let mut entries = DirectoryEntries::open(secure_root.as_raw_fd())?;
    while let Some(name) = entries.next_name()? {
        let Ok(name) = name.into_string() else {
            continue;
        };
        if !name.ends_with(QUARANTINE_SUFFIX) || !is_safe_record_name(&name) {
            continue;
        }
        let Ok(record) = open_record_at(secure_root.as_raw_fd(), c_filename(&name)?.as_c_str())
        else {
            continue;
        };
        let expired = record
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= retention);
        if expired {
            stale.push(name);
        }
    }
    drop(entries);

    for name in &stale {
        unlink_at(&secure_root, name)?;
    }
    if !stale.is_empty() {
        secure_root.sync_all()?;
    }
    Ok(stale.len())
}

fn unlink_at(secure_root: &SecureRoot, name: &str) -> Result<(), CheckpointOutboxError> {
    let name = c_filename(name)?;
    let result = unsafe { libc::unlinkat(secure_root.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(E::from_io(
            "remove ready record",
            io::Error::last_os_error(),
        ))
    }
}

fn is_safe_record_name(name: &str) -> bool {
    !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn require_record_name(name: &str) -> Result<(), CheckpointOutboxError> {
    if name.ends_with(READY_SUFFIX) && is_safe_record_name(name) {
        Ok(())
    } else {
        Err(CheckpointOutboxError::UnsafeReadyRecord)
    }
}
