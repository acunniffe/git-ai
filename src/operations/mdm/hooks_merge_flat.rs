//! Flat camelCase hook-merge helpers shared by the Cursor and Firebender
//! installers: `{version: 1, hooks: {preToolUse: [...], postToolUse: [...]}}`
//! with a per-agent checkpoint predicate. Deliberately performs NO dedup of
//! extra matches — the flat-format agents preserve user duplicates, unlike
//! the catch-all trio's `upsert_singleton_command_hook`.

use serde_json::{Value, json};

/// Ensure a git-ai command exists in each of `hook_names`' arrays under
/// `merged["hooks"]` (setting `merged["version"]` to `1` if absent). Replaces
/// the first `is_checkpoint` match if it differs from `desired_command` or
/// `needs_update_extra` flags it (Firebender's legacy `"matcher"` field);
/// appends if absent. No dedup, unlike `hooks_merge::upsert_singleton_command_hook`.
pub fn upsert_command_hooks_flat(
    merged: &mut Value,
    desired_command: &str,
    hook_names: &[&str],
    is_checkpoint: fn(&str) -> bool,
    needs_update_extra: fn(&Value) -> bool,
) {
    if merged.get("version").is_none()
        && let Some(obj) = merged.as_object_mut()
    {
        obj.insert("version".to_string(), json!(1));
    }

    let mut hooks_obj = merged.get("hooks").cloned().unwrap_or_else(|| json!({}));

    for hook_name in hook_names {
        let mut entries = hooks_obj
            .get(*hook_name)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut found_idx = None;
        let mut needs_update = false;
        for (idx, entry) in entries.iter().enumerate() {
            if let Some(cmd) = entry.get("command").and_then(|c| c.as_str())
                && is_checkpoint(cmd)
            {
                found_idx = Some(idx);
                needs_update = cmd != desired_command || needs_update_extra(entry);
                break;
            }
        }

        match found_idx {
            Some(idx) if needs_update => entries[idx] = json!({ "command": desired_command }),
            Some(_) => {}
            None => entries.push(json!({ "command": desired_command })),
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert(hook_name.to_string(), Value::Array(entries));
        }
    }

    if let Some(root) = merged.as_object_mut() {
        root.insert("hooks".to_string(), hooks_obj);
    }
}

/// Remove entries matched by `is_checkpoint` from each of `hook_names`'
/// arrays under `merged["hooks"]`; paired with [`upsert_command_hooks_flat`].
pub fn remove_command_hooks_flat(
    merged: &mut Value,
    hook_names: &[&str],
    is_checkpoint: fn(&str) -> bool,
) {
    let Some(mut hooks_obj) = merged.get("hooks").cloned() else {
        return;
    };

    for hook_name in hook_names {
        if let Some(entries) = hooks_obj.get_mut(*hook_name).and_then(|v| v.as_array_mut()) {
            entries.retain(|entry| {
                entry
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(|cmd| !is_checkpoint(cmd))
                    .unwrap_or(true)
            });
        }
    }

    if let Some(root) = merged.as_object_mut() {
        root.insert("hooks".to_string(), hooks_obj);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- upsert_command_hooks_flat: deliberately no dedup (unlike singleton) ----

    #[test]
    fn flat_upsert_does_not_dedupe_extra_matches() {
        let mut merged = json!({"hooks": {"preToolUse": [
            {"command": "git-ai checkpoint x"}, {"command": "git-ai checkpoint x"}
        ]}});
        let is_x = |cmd: &str| cmd.contains("git-ai checkpoint x");
        upsert_command_hooks_flat(
            &mut merged,
            "git-ai checkpoint x",
            &["preToolUse"],
            is_x,
            |_| false,
        );
        assert_eq!(merged["hooks"]["preToolUse"].as_array().unwrap().len(), 2);
    }
}
