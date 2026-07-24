//! Shared "single-file plugin drop" installer archetype.
//!
//! Several agent integrations (Amp, Pi, OpenCode) install by dropping one
//! generated TypeScript file into a fixed per-tool location, substituting the
//! git-ai binary path into an `include_str!`-embedded template. Detection,
//! up-to-date checking, and install/uninstall all follow the same shape;
//! this module holds that shared logic so each agent module only declares a
//! [`FileDropSpec`] plus whatever it does beyond the archetype (see
//! `agents/opencode.rs` for the legacy-path migration it layers on top).

use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::binary_exists;
use crate::operations::mdm::file_ops::{generate_diff, write_atomic};
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstallerParams};
use std::fs;
use std::path::{Path, PathBuf};

/// Declarative description of a single-file plugin/extension drop installer.
pub struct FileDropSpec {
    /// `HookInstaller::name`.
    pub name: &'static str,
    /// `HookInstaller::id`.
    pub id: &'static str,
    /// Raw template content with a `__GIT_AI_BINARY_PATH__` placeholder.
    pub template: &'static str,
    /// Absolute path the generated file is written to / read from.
    pub dest_path: fn() -> PathBuf,
    /// Absolute path to the tool's global config directory; its existence
    /// counts as evidence the tool is installed.
    pub global_config_dir: fn() -> PathBuf,
    /// Repo-local config directory (relative to cwd), e.g. `.amp`.
    pub local_config_dir: &'static str,
    /// Binary names probed via `binary_exists` for tool detection.
    pub detect_binary_names: &'static [&'static str],
    /// `HookInstaller::process_names`.
    pub process_names: &'static [&'static str],
}

/// Generate the file content with the absolute binary path substituted in.
/// Backslashes are escaped so Windows paths remain valid inside a TS string
/// literal.
pub fn generate_content(spec: &FileDropSpec, binary_path: &Path) -> String {
    let path_str = binary_path.display().to_string().replace('\\', "\\\\");
    spec.template.replace("__GIT_AI_BINARY_PATH__", &path_str)
}

/// Shared `HookInstaller::check_hooks` body for the file-drop archetype.
pub fn file_drop_check_hooks(
    spec: &FileDropSpec,
    params: &HookInstallerParams,
) -> Result<HookCheckResult, GitAiError> {
    let has_binary = spec
        .detect_binary_names
        .iter()
        .any(|name| binary_exists(name));
    let has_global_config = (spec.global_config_dir)().exists();
    let has_local_config = Path::new(spec.local_config_dir).exists();

    if !has_binary && !has_global_config && !has_local_config {
        return Ok(HookCheckResult {
            tool_installed: false,
            hooks_installed: false,
            hooks_up_to_date: false,
        });
    }

    let dest_path = (spec.dest_path)();
    if !dest_path.exists() {
        return Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: false,
            hooks_up_to_date: false,
        });
    }

    let current_content = fs::read_to_string(&dest_path).unwrap_or_default();
    let expected_content = generate_content(spec, &params.binary_path);

    Ok(HookCheckResult {
        tool_installed: true,
        hooks_installed: true,
        hooks_up_to_date: current_content.trim() == expected_content.trim(),
    })
}

/// Shared `HookInstaller::install_hooks` body for the file-drop archetype.
pub fn file_drop_install(
    spec: &FileDropSpec,
    params: &HookInstallerParams,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let dest_path = (spec.dest_path)();

    if let Some(dir) = dest_path.parent()
        && !dry_run
    {
        fs::create_dir_all(dir)?;
    }

    let existing_content = if dest_path.exists() {
        fs::read_to_string(&dest_path)?
    } else {
        String::new()
    };

    let new_content = generate_content(spec, &params.binary_path);
    if existing_content.trim() == new_content.trim() {
        return Ok(None);
    }

    let diff_output = generate_diff(&dest_path, &existing_content, &new_content);

    if !dry_run {
        if let Some(dir) = dest_path.parent() {
            fs::create_dir_all(dir)?;
        }
        write_atomic(&dest_path, new_content.as_bytes())?;
    }

    Ok(Some(diff_output))
}

/// Shared `HookInstaller::uninstall_hooks` body for the file-drop archetype.
pub fn file_drop_uninstall(
    spec: &FileDropSpec,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let dest_path = (spec.dest_path)();

    if !dest_path.exists() {
        return Ok(None);
    }

    let existing_content = fs::read_to_string(&dest_path)?;
    let diff_output = generate_diff(&dest_path, &existing_content, "");

    if !dry_run {
        fs::remove_file(&dest_path)?;
    }

    Ok(Some(diff_output))
}
