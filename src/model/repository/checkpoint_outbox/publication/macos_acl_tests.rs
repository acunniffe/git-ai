use super::super::{encode_delivery, publish_delivery};
use super::{CheckpointOutboxError, write_private_marker};
use crate::model::checkpoint_delivery::CheckpointDelivery;
use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
use crate::model::working_log::CheckpointKind;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

fn delivery(trace_id: &str) -> CheckpointDelivery {
    CheckpointDelivery::from_requests_at(
        vec![CheckpointRequest {
            trace_id: trace_id.to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: Vec::new(),
            path_role: PreparedPathRole::Edited,
            stream_source: None,
            metadata: HashMap::new(),
            delivery_id: None,
        }],
        42,
    )
    .remove(0)
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn add_acl(path: &Path, ace: &str) {
    let output = Command::new("/bin/chmod")
        .args(["+a", ace])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "chmod +a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn acl_entries(path: &Path) -> Vec<String> {
    let output = Command::new("/bin/ls")
        .arg("-lde")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ls -lde failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(index, _)| index.parse::<usize>().is_ok())
        })
        .map(str::to_owned)
        .collect()
}

fn assert_mode(path: &Path, expected: u32) {
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, expected);
}

#[test]
fn allow_acl_on_existing_ancestor_is_rejected_without_creating_root() {
    let temp = tempfile::tempdir().unwrap();
    let ancestor = temp.path().join("managed");
    let root = ancestor.join("outbox");
    fs::create_dir(&ancestor).unwrap();
    set_mode(&ancestor, 0o700);
    add_acl(&ancestor, "everyone allow search");
    assert_mode(&ancestor, 0o700);

    let error = publish_delivery(&root, &delivery("ancestor-acl")).unwrap_err();

    assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    assert!(!root.exists());
}

#[test]
fn allow_acl_on_ancestor_symlink_container_is_rejected_before_target_creation() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let managed = temp.path().join("managed");
    let target = temp.path().join("private-target");
    fs::create_dir(&managed).unwrap();
    fs::create_dir(&target).unwrap();
    set_mode(&managed, 0o700);
    set_mode(&target, 0o700);
    add_acl(&managed, "everyone allow add_file,delete_child");
    symlink(&target, managed.join("link")).unwrap();
    assert_mode(&managed, 0o700);

    let error = publish_delivery(
        &managed.join("link").join("outbox"),
        &delivery("symlink-acl"),
    )
    .unwrap_err();

    assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    assert!(!target.join("outbox").exists());
}

#[test]
fn allow_acl_on_existing_root_is_rejected_with_private_posix_mode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    fs::create_dir(&root).unwrap();
    set_mode(&root, 0o700);
    add_acl(&root, "everyone allow list,search");
    assert_mode(&root, 0o700);

    let error = publish_delivery(&root, &delivery("root-acl")).unwrap_err();

    assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    assert!(fs::read_dir(&root).unwrap().next().is_none());
}

#[test]
fn allow_acl_on_existing_ready_record_is_rejected_with_private_posix_mode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let item = delivery("record-acl");
    let published = publish_delivery(&root, &item).unwrap();
    let original = fs::read(&published.path).unwrap();
    add_acl(&published.path, "everyone allow read");
    assert_mode(&published.path, 0o600);

    let error = publish_delivery(&root, &item).unwrap_err();

    assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
    assert_eq!(fs::read(&published.path).unwrap(), original);
}

#[test]
fn allow_acl_on_queued_record_is_rejected_during_capacity_scan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    let published = publish_delivery(&root, &delivery("queued-record-acl")).unwrap();
    add_acl(&published.path, "everyone allow read");
    assert_mode(&published.path, 0o600);

    let error = publish_delivery(&root, &delivery("next-record")).unwrap_err();

    assert!(matches!(error, CheckpointOutboxError::UnsafeReadyRecord));
}

#[test]
fn newly_created_root_components_clear_inherited_acl_entries() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("parent");
    let root = parent.join("nested").join("outbox");
    fs::create_dir(&parent).unwrap();
    set_mode(&parent, 0o700);
    add_acl(
        &parent,
        "everyone deny delete,file_inherit,directory_inherit,only_inherit",
    );
    assert_eq!(acl_entries(&parent).len(), 1);

    let published = publish_delivery(&root, &delivery("inherited-root-acl")).unwrap();

    assert!(acl_entries(&parent.join("nested")).is_empty());
    assert!(acl_entries(&root).is_empty());
    assert!(acl_entries(&published.path).is_empty());
}

#[test]
fn new_ready_records_and_markers_clear_inherited_acl_entries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("outbox");
    fs::create_dir(&root).unwrap();
    set_mode(&root, 0o700);
    add_acl(&root, "everyone deny delete,file_inherit,only_inherit");
    assert_eq!(acl_entries(&root).len(), 1);

    let item = delivery("inherited-record-acl");
    let published = publish_delivery(&root, &item).unwrap();
    write_private_marker(&root, "last-failure.cbor", b"redacted").unwrap();

    assert_eq!(
        fs::read(&published.path).unwrap(),
        encode_delivery(&item).unwrap()
    );
    assert!(acl_entries(&published.path).is_empty());
    assert!(acl_entries(&root.join("last-failure.cbor")).is_empty());
}
