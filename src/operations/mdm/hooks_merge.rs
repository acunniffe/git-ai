//! Shared merge engines for MDM hook installers that read-modify-write a JSON
//! (or JSONC) settings file to inject a git-ai checkpoint command. Three
//! archetypes were duplicated across `mdm/agents/*.rs` before this module
//! existed:
//!
//! - **Settings-merge envelope** (`edit_settings_json`): read the file, clone
//!   it, let a caller closure mutate the clone, detect "no change" via
//!   equality, then pretty-print + diff + atomically write.
//! - **Catch-all matcher** (`install_catch_all_hooks` /
//!   `uninstall_catch_all_hooks` / `catch_all_hook_status`): settings that
//!   group hook commands under named "matcher" blocks (Claude Code, Gemini,
//!   Droid) migrate git-ai commands into a single `"*"` block, deduplicating.
//! - **Flat command-hooks** (Cursor, Firebender): see the sibling
//!   `hooks_merge_flat` module.
use crate::error::GitAiError;
use crate::operations::mdm::file_ops::{generate_diff, write_atomic};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

/// The `"*"` catch-all matcher for installers grouping hooks under matcher blocks.
const CATCH_ALL_MATCHER: &str = "*";

/// Check if a command is a git-ai checkpoint command
pub fn is_git_ai_checkpoint_command(cmd: &str) -> bool {
    // Must contain "git-ai" and "checkpoint"
    cmd.contains("git-ai") && cmd.contains("checkpoint")
}

/// Controls how [`edit_settings_json`] treats a settings file that doesn't exist.
pub enum MissingBehavior {
    /// Install-style: proceed as if the file contained `{}`, and ensure the
    /// parent directory exists so a write can land there.
    TreatAsEmpty,
    /// Uninstall-style: nothing to remove from a missing file, so return
    /// `Ok(None)` immediately without touching the filesystem.
    NoOp,
}

/// Shared read -> parse -> clone -> mutate -> compare -> serialize -> diff ->
/// write envelope for MDM hook installers backed by a single JSON/JSONC
/// settings file.
///
/// `parse` converts file content into a [`Value`]; pass `serde_json::from_str`
/// wrapped to return [`GitAiError`], or a JSONC-aware parser (see
/// `agents/droid.rs`). `mutate` receives a clone of the parsed value and edits
/// it in place. **`mutate` must leave its argument byte-for-byte unchanged
/// when there is nothing to do** -- "changed" is detected by comparing the
/// mutated value against the value read from disk.
pub fn edit_settings_json(
    settings_path: &Path,
    dry_run: bool,
    on_missing: MissingBehavior,
    parse: impl Fn(&str) -> Result<Value, GitAiError>,
    mutate: impl FnOnce(&mut Value),
) -> Result<Option<String>, GitAiError> {
    if !settings_path.exists() && matches!(on_missing, MissingBehavior::NoOp) {
        return Ok(None);
    }

    if matches!(on_missing, MissingBehavior::TreatAsEmpty)
        && let Some(dir) = settings_path.parent()
    {
        fs::create_dir_all(dir)?;
    }

    let existing_content = if settings_path.exists() {
        fs::read_to_string(settings_path)?
    } else {
        String::new()
    };

    let existing: Value = match on_missing {
        MissingBehavior::TreatAsEmpty if existing_content.trim().is_empty() => json!({}),
        _ => parse(&existing_content)?,
    };

    let mut merged = existing.clone();
    mutate(&mut merged);

    if existing == merged {
        return Ok(None);
    }

    let new_content = serde_json::to_string_pretty(&merged)?;
    let diff_output = generate_diff(settings_path, &existing_content, &new_content);

    if !dry_run {
        write_atomic(settings_path, new_content.as_bytes())?;
    }

    Ok(Some(diff_output))
}

/// Ensure exactly one git-ai checkpoint command (identified via
/// `is_git_ai_checkpoint_command` on each entry's `"command"`) exists in
/// `array`: replace a differing match with `desired_hook`, dedup any further
/// matches down to one, or append `desired_hook` if none exist. Shared by the
/// catch-all matcher installers' dedup step and by Windsurf's flat per-event
/// hook arrays.
pub fn upsert_singleton_command_hook(
    array: &mut Vec<Value>,
    desired_cmd: &str,
    desired_hook: Value,
) {
    let mut found_idx: Option<usize> = None;
    let mut needs_update = false;

    for (idx, item) in array.iter().enumerate() {
        if let Some(cmd) = item.get("command").and_then(|c| c.as_str())
            && is_git_ai_checkpoint_command(cmd)
            && found_idx.is_none()
        {
            found_idx = Some(idx);
            if cmd != desired_cmd {
                needs_update = true;
            }
        }
    }

    match found_idx {
        Some(idx) => {
            if needs_update {
                array[idx] = desired_hook;
            }
            let keep_idx = idx;
            let mut current_idx = 0;
            array.retain(|item| {
                if current_idx == keep_idx {
                    current_idx += 1;
                    true
                } else if let Some(cmd) = item.get("command").and_then(|c| c.as_str()) {
                    let is_dup = is_git_ai_checkpoint_command(cmd);
                    current_idx += 1;
                    !is_dup
                } else {
                    current_idx += 1;
                    true
                }
            });
        }
        None => {
            array.push(desired_hook);
        }
    }
}

/// Ensure exactly one git-ai checkpoint command lives in the `"*"` catch-all
/// matcher block of `hooks_obj[hook_type]`, for each `(hook_type,
/// desired_command)` pair in `desired`. Any git-ai command found in a
/// non-catch-all matcher block is stripped first (migration); a block
/// emptied entirely by that migration is dropped, but pre-existing empty
/// blocks are left alone. Shared by Claude Code, Gemini, and Droid.
pub fn install_catch_all_hooks(hooks_obj: &mut Value, desired: &[(&str, &str)]) {
    for (hook_type, desired_cmd) in desired {
        let mut hook_type_array = hooks_obj
            .get(*hook_type)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Step 1: strip git-ai from every non-catch-all matcher block (migration).
        let mut emptied_by_migration = vec![false; hook_type_array.len()];
        for (i, block) in hook_type_array.iter_mut().enumerate() {
            let is_catch_all = block
                .get("matcher")
                .and_then(|m| m.as_str())
                .map(|m| m == CATCH_ALL_MATCHER)
                .unwrap_or(false);
            if !is_catch_all
                && let Some(hooks) = block.get_mut("hooks").and_then(|h| h.as_array_mut())
            {
                let before = hooks.len();
                hooks.retain(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(|cmd| !is_git_ai_checkpoint_command(cmd))
                        .unwrap_or(true)
                });
                if hooks.is_empty() && before > 0 {
                    emptied_by_migration[i] = true;
                }
            }
        }
        let mut i = 0;
        hook_type_array.retain(|_| {
            let remove = emptied_by_migration[i];
            i += 1;
            !remove
        });

        // Step 2: find or create the "*" catch-all matcher block.
        let catch_all_idx = hook_type_array
            .iter()
            .position(|b| {
                b.get("matcher")
                    .and_then(|m| m.as_str())
                    .map(|m| m == CATCH_ALL_MATCHER)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| {
                hook_type_array.push(json!({
                    "matcher": CATCH_ALL_MATCHER,
                    "hooks": []
                }));
                hook_type_array.len() - 1
            });

        // Step 3: ensure exactly one git-ai command in the catch-all block.
        let mut hooks_array = hook_type_array[catch_all_idx]
            .get("hooks")
            .and_then(|h| h.as_array())
            .cloned()
            .unwrap_or_default();

        upsert_singleton_command_hook(
            &mut hooks_array,
            desired_cmd,
            json!({
                "type": "command",
                "command": desired_cmd
            }),
        );

        if let Some(matcher_block) = hook_type_array[catch_all_idx].as_object_mut() {
            matcher_block.insert("hooks".to_string(), Value::Array(hooks_array));
        }

        if let Some(obj) = hooks_obj.as_object_mut() {
            obj.insert(hook_type.to_string(), Value::Array(hook_type_array));
        }
    }
}

/// Remove git-ai checkpoint commands from every matcher block (catch-all or
/// otherwise) for each hook type in `hook_types`. Returns whether anything was
/// removed, so callers can skip re-inserting an unchanged `hooks_obj`.
pub fn uninstall_catch_all_hooks(hooks_obj: &mut Value, hook_types: &[&str]) -> bool {
    let mut changed = false;
    for hook_type in hook_types {
        if let Some(hook_type_array) = hooks_obj.get_mut(*hook_type).and_then(|v| v.as_array_mut())
        {
            for matcher_block in hook_type_array.iter_mut() {
                if let Some(hooks_array) = matcher_block
                    .get_mut("hooks")
                    .and_then(|h| h.as_array_mut())
                {
                    let original_len = hooks_array.len();
                    hooks_array.retain(|hook| {
                        if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                            !is_git_ai_checkpoint_command(cmd)
                        } else {
                            true
                        }
                    });
                    if hooks_array.len() != original_len {
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

/// Returns `(hooks_installed, hooks_up_to_date)` for a catch-all-matcher-style
/// settings value, inspecting only `pre_hook_type` (matches every pre-existing
/// agent's `hook_status`, which never inspected the "post" key).
/// `hooks_installed` = a git-ai command exists in ANY matcher block;
/// `hooks_up_to_date` = one exists specifically in the `"*"` catch-all block.
pub fn catch_all_hook_status(settings: &Value, pre_hook_type: &str) -> (bool, bool) {
    let Some(blocks) = settings
        .get("hooks")
        .and_then(|h| h.get(pre_hook_type))
        .and_then(|v| v.as_array())
    else {
        return (false, false);
    };

    let mut hooks_installed = false;
    let mut hooks_up_to_date = false;

    for block in blocks {
        let is_catch_all = block
            .get("matcher")
            .and_then(|m| m.as_str())
            .map(|m| m == CATCH_ALL_MATCHER)
            .unwrap_or(false);

        let has_git_ai = block
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("command")
                        .and_then(|c| c.as_str())
                        .map(is_git_ai_checkpoint_command)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if has_git_ai {
            hooks_installed = true;
            if is_catch_all {
                hooks_up_to_date = true;
            }
        }
    }

    (hooks_installed, hooks_up_to_date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse_json(content: &str) -> Result<Value, GitAiError> {
        Ok(serde_json::from_str(content)?)
    }

    #[test]
    fn test_is_git_ai_checkpoint_command() {
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint claude --hook-input stdin"
        ));
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint claude"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint --hook-input"
        ));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint claude --hook-input \"$(cat)\""
        ));
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint gemini"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint gemini --hook-input stdin"
        ));

        // Non-matching commands
        assert!(!is_git_ai_checkpoint_command("echo hello"));
        assert!(!is_git_ai_checkpoint_command("git status"));
        assert!(!is_git_ai_checkpoint_command("checkpoint"));
        assert!(!is_git_ai_checkpoint_command("git-ai"));
    }

    // ---- edit_settings_json envelope ----

    #[test]
    fn envelope_treat_as_empty_creates_parent_dir_and_writes() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested").join("settings.json");
        assert!(!path.parent().unwrap().exists());

        let result = edit_settings_json(
            &path,
            false,
            MissingBehavior::TreatAsEmpty,
            parse_json,
            |v| {
                v.as_object_mut()
                    .unwrap()
                    .insert("added".to_string(), json!(true));
            },
        )
        .unwrap();

        assert!(result.is_some());
        assert!(path.exists());
        let written: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written.get("added"), Some(&json!(true)));
    }

    #[test]
    fn envelope_no_op_missing_file_never_touches_filesystem() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested").join("settings.json");

        let result = edit_settings_json(&path, false, MissingBehavior::NoOp, parse_json, |v| {
            v.as_object_mut()
                .unwrap()
                .insert("added".to_string(), json!(true));
        })
        .unwrap();

        assert!(result.is_none());
        assert!(!path.parent().unwrap().exists(), "must not create dirs");
    }

    #[test]
    fn envelope_noop_mutation_yields_none_and_no_write() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, r#"{"keep": 1}"#).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let result = edit_settings_json(
            &path,
            false,
            MissingBehavior::TreatAsEmpty,
            parse_json,
            |_v| {
                // Intentionally does nothing: mutate must be a true no-op.
            },
        )
        .unwrap();

        assert!(result.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn envelope_dry_run_computes_diff_without_writing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("settings.json");

        let result = edit_settings_json(
            &path,
            true,
            MissingBehavior::TreatAsEmpty,
            parse_json,
            |v| {
                v.as_object_mut()
                    .unwrap()
                    .insert("added".to_string(), json!(true));
            },
        )
        .unwrap();

        assert!(result.is_some(), "dry run should still report a diff");
        assert!(!path.exists(), "dry run must not write");
    }

    // ---- upsert_singleton_command_hook ----

    #[test]
    fn upsert_inserts_when_absent() {
        let mut array = vec![json!({"command": "echo hi"})];
        upsert_singleton_command_hook(
            &mut array,
            "git-ai checkpoint x",
            json!({"command": "git-ai checkpoint x"}),
        );
        assert_eq!(array.len(), 2);
        assert_eq!(array[1]["command"], "git-ai checkpoint x");
    }

    #[test]
    fn upsert_updates_stale_command_in_place() {
        let mut array = vec![json!({"command": "/old/git-ai checkpoint x"})];
        upsert_singleton_command_hook(
            &mut array,
            "git-ai checkpoint x",
            json!({"command": "git-ai checkpoint x"}),
        );
        assert_eq!(array.len(), 1);
        assert_eq!(array[0]["command"], "git-ai checkpoint x");
    }

    #[test]
    fn upsert_dedupes_extra_matches_keeping_first() {
        let mut array = vec![
            json!({"command": "git-ai checkpoint x"}),
            json!({"command": "git-ai checkpoint x"}),
        ];
        upsert_singleton_command_hook(
            &mut array,
            "git-ai checkpoint x",
            json!({"command": "git-ai checkpoint x"}),
        );
        assert_eq!(array.len(), 1);
    }

    // ---- install_catch_all_hooks / uninstall_catch_all_hooks / catch_all_hook_status ----

    #[test]
    fn install_catch_all_creates_block_when_missing() {
        let mut hooks_obj = json!({});
        install_catch_all_hooks(&mut hooks_obj, &[("PreToolUse", "git-ai checkpoint x")]);
        let (installed, up_to_date) =
            catch_all_hook_status(&json!({"hooks": hooks_obj}), "PreToolUse");
        assert!(installed);
        assert!(up_to_date);
    }

    #[test]
    fn install_catch_all_migrates_out_of_named_matcher() {
        let mut hooks_obj = json!({
            "PreToolUse": [{"matcher": "Write", "hooks": [{"command": "git-ai checkpoint x"}]}]
        });
        install_catch_all_hooks(&mut hooks_obj, &[("PreToolUse", "git-ai checkpoint x")]);
        let blocks = hooks_obj["PreToolUse"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "emptied named block should be dropped");
        assert_eq!(blocks[0]["matcher"], "*");
    }

    #[test]
    fn uninstall_catch_all_reports_unchanged_when_nothing_to_remove() {
        let mut hooks_obj =
            json!({"PreToolUse": [{"matcher": "*", "hooks": [{"command": "echo hi"}]}]});
        let changed = uninstall_catch_all_hooks(&mut hooks_obj, &["PreToolUse"]);
        assert!(!changed);
    }

    #[test]
    fn uninstall_catch_all_removes_git_ai_only() {
        let mut hooks_obj = json!({
            "PreToolUse": [{"matcher": "*", "hooks": [
                {"command": "echo hi"},
                {"command": "git-ai checkpoint x"}
            ]}]
        });
        let changed = uninstall_catch_all_hooks(&mut hooks_obj, &["PreToolUse"]);
        assert!(changed);
        let hooks = hooks_obj["PreToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"], "echo hi");
    }

    #[test]
    fn catch_all_hook_status_missing_hook_type_is_not_installed() {
        let (installed, up_to_date) = catch_all_hook_status(&json!({}), "PreToolUse");
        assert!(!installed);
        assert!(!up_to_date);
    }
}
