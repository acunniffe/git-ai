pub mod agents;
pub mod editor_cli;
pub(crate) mod editor_extension;
#[cfg(test)]
mod editor_extension_tests;
pub(crate) mod extension_manifests;
pub mod file_ops;
pub mod hook_installer;
pub mod hooks_merge;
pub mod hooks_merge_flat;
pub mod jetbrains;
pub mod paths;
pub mod plugin_drop;
pub mod skills_installer;
pub mod spinner;
#[cfg(test)]
pub(crate) mod test_env;
pub mod version;
pub mod vscode_settings;
