use super::{
    FileIdentity, SecureRoot, open_record_at, validate_acknowledged_record, validate_record_file,
};
use crate::model::repository::checkpoint_outbox::CheckpointOutboxError;
use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub(super) fn reject_existing_delivery(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &CStr,
    expected_bytes: &[u8],
) -> Result<(), CheckpointOutboxError> {
    match open_record_at(secure_root.as_raw_fd(), ready_name) {
        Ok(mut record) => {
            acknowledge_opened_delivery(
                root,
                secure_root,
                ready_name,
                expected_bytes,
                &mut record,
                SecureRoot::sync_all,
            )?;
            Err(CheckpointOutboxError::AlreadyPublished)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(CheckpointOutboxError::UnsafeReadyRecord),
    }
}

pub(super) fn acknowledge_existing_delivery<F>(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &CStr,
    expected_bytes: &[u8],
    sync_root: F,
) -> Result<(), CheckpointOutboxError>
where
    F: FnOnce(&SecureRoot) -> Result<(), CheckpointOutboxError>,
{
    let mut record = open_record_at(secure_root.as_raw_fd(), ready_name)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
    acknowledge_opened_delivery(
        root,
        secure_root,
        ready_name,
        expected_bytes,
        &mut record,
        sync_root,
    )
}

fn acknowledge_opened_delivery<F>(
    root: &Path,
    secure_root: &SecureRoot,
    ready_name: &CStr,
    expected_bytes: &[u8],
    record: &mut File,
    sync_root: F,
) -> Result<(), CheckpointOutboxError>
where
    F: FnOnce(&SecureRoot) -> Result<(), CheckpointOutboxError>,
{
    validate_record_bytes(record, expected_bytes)?;
    sync_root(secure_root)?;
    validate_acknowledged_record(root, secure_root, ready_name, record)?;
    Ok(())
}

fn validate_record_bytes(
    record: &mut File,
    expected_bytes: &[u8],
) -> Result<(), CheckpointOutboxError> {
    let before = validate_record_file(record)?;
    if before.len() != u64::try_from(expected_bytes.len()).unwrap_or(u64::MAX) {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    let mut buffer = [0u8; 8 * 1024];
    for expected in expected_bytes.chunks(buffer.len()) {
        record
            .read_exact(&mut buffer[..expected.len()])
            .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?;
        if &buffer[..expected.len()] != expected {
            return Err(CheckpointOutboxError::UnsafeReadyRecord);
        }
    }
    let mut trailing = [0u8; 1];
    if record
        .read(&mut trailing)
        .map_err(|_| CheckpointOutboxError::UnsafeReadyRecord)?
        != 0
    {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }

    let after = validate_record_file(record)?;
    if FileIdentity::of(&before) != FileIdentity::of(&after) || before.len() != after.len() {
        return Err(CheckpointOutboxError::UnsafeReadyRecord);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn secure_record(root: &Path, name: &str, bytes: &[u8]) -> SecureRoot {
        let secure_root = SecureRoot::open(root).unwrap();
        secure_root.try_lock().unwrap();
        let path = root.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        secure_root
    }

    #[test]
    fn exact_collision_syncs_directory_before_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let name = c"existing.ready";
        let bytes = b"identical";
        let secure_root = secure_record(&root, name.to_str().unwrap(), bytes);
        let sync_calls = Cell::new(0usize);

        acknowledge_existing_delivery(&root, &secure_root, name, bytes, |_| {
            sync_calls.set(sync_calls.get() + 1);
            Ok(())
        })
        .unwrap();

        assert_eq!(sync_calls.get(), 1);
    }

    #[test]
    fn exact_collision_propagates_directory_sync_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("outbox");
        let name = c"existing.ready";
        let bytes = b"identical";
        let secure_root = secure_record(&root, name.to_str().unwrap(), bytes);

        let error = acknowledge_existing_delivery(&root, &secure_root, name, bytes, |_| {
            Err(CheckpointOutboxError::Io {
                operation: "sync outbox root",
                kind: std::io::ErrorKind::Other,
            })
        })
        .unwrap_err();

        assert!(matches!(
            error,
            CheckpointOutboxError::Io {
                operation: "sync outbox root",
                kind: std::io::ErrorKind::Other,
            }
        ));
    }
}
