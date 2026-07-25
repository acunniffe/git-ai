use crate::model::repository::checkpoint_outbox::{OutboxRootStatus, inspect_outbox_root};
use crate::operations::commands::checkpoint_agent::delivery_runtime::checkpoint_outbox_root_selection;
use crate::operations::daemon::DaemonConfig;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(crate) fn append_checkpoint_outbox_debug(out: &mut String) {
    let selection = DaemonConfig::from_env_or_default_paths().map(|config| {
        let selection = checkpoint_outbox_root_selection(&config);
        let statuses = selection
            .roots
            .into_iter()
            .map(|root| {
                let status = inspect_outbox_root(&root);
                (root, status)
            })
            .collect::<Vec<_>>();
        (statuses, selection.invalid_override)
    });

    let _ = writeln!(out);
    let _ = writeln!(out, "== Checkpoint Outbox ==");
    match selection {
        Ok((statuses, invalid_override)) => {
            if invalid_override {
                let _ = writeln!(out, "Configured override: invalid (must be absolute)");
            }
            append_root_statuses(out, &statuses);
        }
        Err(_) => {
            let _ = writeln!(out, "Status: unavailable");
        }
    }
}

fn append_root_statuses(out: &mut String, statuses: &[(PathBuf, OutboxRootStatus)]) {
    if statuses.is_empty() {
        let _ = writeln!(out, "Status: unavailable");
        return;
    }
    for (root, status) in statuses {
        append_root_status(out, root, status);
    }
}

fn append_root_status(out: &mut String, root: &Path, status: &OutboxRootStatus) {
    let _ = writeln!(out, "Root: {}", root.display());
    let _ = writeln!(out, "  State: {}", status.state.as_str());
    let permissions =
        if status.state == crate::model::repository::checkpoint_outbox::OutboxRootState::Ready {
            "private"
        } else {
            "unverified"
        };
    let _ = writeln!(out, "  Permissions: {permissions}");
    let _ = writeln!(
        out,
        "  Pending: {} records, {} bytes",
        status.pending_records, status.pending_bytes
    );
    match status.oldest_ready_age_ms {
        Some(age_ms) => {
            let _ = writeln!(out, "  Oldest ready age: {age_ms} ms");
        }
        None => {
            let _ = writeln!(out, "  Oldest ready age: none");
        }
    }
    match status.last_failure {
        Some(failure) => {
            let _ = writeln!(
                out,
                "  Last failure: {} at {} ms",
                failure.class.as_str(),
                failure.recorded_at_unix_ms
            );
        }
        None => {
            let _ = writeln!(out, "  Last failure: none");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::repository::checkpoint_outbox::{
        OutboxFailureClass, OutboxRootState, RedactedOutboxFailure,
    };

    #[test]
    fn debug_status_is_actionable_without_record_contents_or_paths() {
        let status = OutboxRootStatus {
            state: OutboxRootState::Ready,
            pending_records: 3,
            pending_bytes: 1_024,
            oldest_ready_age_ms: Some(2_500),
            last_failure: Some(RedactedOutboxFailure::new_for_test(
                123,
                OutboxFailureClass::Capacity,
            )),
        };
        let root = PathBuf::from("/safe/checkpoint-outbox");
        let mut out = String::new();

        append_root_statuses(&mut out, &[(root.clone(), status)]);

        assert!(out.contains(&format!("Root: {}", root.display())));
        assert!(out.contains("State: ready"));
        assert!(out.contains("Permissions: private"));
        assert!(out.contains("Pending: 3 records, 1024 bytes"));
        assert!(out.contains("Oldest ready age: 2500 ms"));
        assert!(out.contains("Last failure: capacity at 123 ms"));
        assert!(!out.contains("trace-secret"));
        assert!(!out.contains("repository/file.rs"));
    }
}
