//! Per-agent tool classification for bash-tool vs file-edit dispatch.

use super::types::ToolClass;

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Supported AI agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Gemini,
    ContinueCli,
    Droid,
    Amp,
    OpenCode,
    Firebender,
    Codex,
    Pi,
    Windsurf,
    Cursor,
    Cline,
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Strip the `functions.` namespace prefix that Codex/OpenAI function-call
/// tools add to hook tool names (e.g. `functions.apply_patch` -> `apply_patch`).
fn normalize_tool_name(tool_name: &str) -> &str {
    tool_name.strip_prefix("functions.").unwrap_or(tool_name)
}

/// Classify an optional hook tool name for a given agent. Payloads that
/// predate `tool_name` classify as file edits to preserve legacy behavior.
pub fn classify_optional_tool(agent: Agent, tool_name: Option<&str>) -> ToolClass {
    tool_name.map_or(ToolClass::FileEdit, |name| classify_tool(agent, name))
}

/// Classify a tool name for a given agent.
pub fn classify_tool(agent: Agent, tool_name: &str) -> ToolClass {
    match agent {
        Agent::Claude => match tool_name {
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Gemini => match tool_name {
            "write_file" | "replace" | "WriteFile" => ToolClass::FileEdit,
            "shell" | "run_shell_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::ContinueCli => match tool_name {
            "edit" => ToolClass::FileEdit,
            "terminal" | "local_shell_call" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Droid => match tool_name {
            "ApplyPatch" | "Edit" | "Write" | "Create" => ToolClass::FileEdit,
            "Bash" | "Execute" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Amp => match tool_name {
            "Write" | "Edit" | "create_file" | "edit_file" | "apply_patch" | "undo_edit" => {
                ToolClass::FileEdit
            }
            "Bash" | "shell_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::OpenCode => match tool_name {
            "edit" | "write" => ToolClass::FileEdit,
            "bash" | "shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Firebender => match tool_name {
            "Write" | "Edit" | "Delete" | "RenameSymbol" | "DeleteSymbol" => ToolClass::FileEdit,
            "Bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Codex => {
            // Codex Desktop (OpenAI function-calling) prefixes tool names with
            // "functions."; strip it before matching the bare tool name.
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "apply_patch" => ToolClass::FileEdit,
                "Bash" | "exec_command" | "shell" | "shell_command" => ToolClass::Bash,
                "multi_tool_use.parallel" => ToolClass::Bash,
                _ => ToolClass::Skip,
            }
        }
        Agent::Pi => match tool_name {
            "edit" | "write" | "replace" | "rename" => ToolClass::FileEdit,
            "bash" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Windsurf => match tool_name {
            "code_action" => ToolClass::FileEdit,
            "run_command" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cursor => match tool_name {
            "Write" | "Delete" | "StrReplace" | "ApplyPatch" => ToolClass::FileEdit,
            "Shell" => ToolClass::Bash,
            _ => ToolClass::Skip,
        },
        Agent::Cline => {
            let tool_name = normalize_tool_name(tool_name);
            match tool_name {
                "replace_in_file" | "write_to_file" | "apply_patch" | "editor" | "edit"
                | "write" => ToolClass::FileEdit,
                "execute_command" | "bash" | "shell" | "run_commands" | "run_command" => {
                    ToolClass::Bash
                }
                _ => ToolClass::Skip,
            }
        }
    }
}
