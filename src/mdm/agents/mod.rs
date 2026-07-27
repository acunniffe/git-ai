mod amp;
mod claude_code;
mod cline;
mod codex;
mod cursor;
mod droid;
mod firebender;
mod gemini;
mod github_copilot;
mod jetbrains;
mod opencode;
mod pi;
#[cfg(windows)]
mod visual_studio;
mod vscode;
mod windsurf;

pub use amp::AmpInstaller;
pub use claude_code::ClaudeCodeInstaller;
pub use cline::ClineInstaller;
pub use codex::CodexInstaller;
pub use cursor::CursorInstaller;
pub use droid::DroidInstaller;
pub use firebender::FirebenderInstaller;
pub use gemini::GeminiInstaller;
pub use github_copilot::GitHubCopilotInstaller;
pub use jetbrains::JetBrainsInstaller;
pub use opencode::OpenCodeInstaller;
pub use pi::PiInstaller;
#[cfg(windows)]
pub use visual_studio::VisualStudioInstaller;
pub use vscode::VSCodeInstaller;
pub use windsurf::WindsurfInstaller;

use super::hook_installer::HookInstaller;

/// Get all available hook installers
pub fn get_all_installers() -> Vec<Box<dyn HookInstaller>> {
    let mut installers: Vec<Box<dyn HookInstaller>> = vec![
        Box::new(ClaudeCodeInstaller),
        Box::new(ClineInstaller),
        Box::new(CodexInstaller),
        Box::new(CursorInstaller),
        Box::new(VSCodeInstaller),
        Box::new(GitHubCopilotInstaller),
        Box::new(AmpInstaller),
        Box::new(OpenCodeInstaller),
        Box::new(PiInstaller),
        Box::new(GeminiInstaller),
        Box::new(DroidInstaller),
        Box::new(FirebenderInstaller),
        Box::new(JetBrainsInstaller),
    ];

    #[cfg(windows)]
    installers.push(Box::new(VisualStudioInstaller));

    installers.push(Box::new(WindsurfInstaller));
    installers
}
