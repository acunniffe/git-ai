use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::operations::git::repository::Repository;
#[cfg(windows)]
use crate::process_spawn::CREATE_NO_WINDOW;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

const POST_NOTES_UPDATED_HOOK: &str = "post_notes_updated";
const HOOK_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct RepoHookContext {
    repo_url: String,
    repo_name: String,
    branch: String,
    is_default_branch: bool,
}

/// Dispatch configured `git_ai_hooks.post_notes_updated` shell commands.
///
/// The hook input is always passed through stdin as a JSON array of 1..N note entries.
/// Commands are started in parallel, and we wait up to 3 seconds for completion before
/// detaching and continuing so git-ai does not block.
pub fn post_notes_updated(repo: &Repository, notes: &[(String, String)]) {
    post_notes_updated_refs(
        repo,
        notes
            .iter()
            .map(|(commit_sha, note_content)| (commit_sha.as_str(), note_content.as_str())),
    );
}

/// Like [`post_notes_updated`], but borrows the note entries so callers with
/// large batches never have to clone contents into owned pairs.
pub(crate) fn post_notes_updated_refs<'a>(
    repo: &Repository,
    notes: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let mut notes = notes.into_iter().peekable();
    if notes.peek().is_none() {
        return;
    }

    let hook_commands = Config::get()
        .git_ai_hook_commands(POST_NOTES_UPDATED_HOOK)
        .cloned()
        .unwrap_or_default();
    if hook_commands.is_empty() {
        return;
    }

    let context = build_repo_hook_context(repo);
    let repo_url = context.repo_url;
    let repo_name = context.repo_name;
    let branch = context.branch;
    let is_default_branch = context.is_default_branch;
    let payload = notes
        .map(|(commit_sha, note_content)| {
            serde_json::json!({
                "commit_sha": commit_sha,
                "repo_url": repo_url.as_str(),
                "repo_name": repo_name.as_str(),
                "branch": branch.as_str(),
                "is_default_branch": is_default_branch,
                "note_content": note_content,
            })
        })
        .collect::<Vec<_>>();
    let payload_json = match serde_json::to_string(&payload) {
        Ok(json) => json,
        Err(e) => {
            tracing::debug!(
                "[git_ai_hooks] Failed to serialize post_notes_updated payload: {}",
                e
            );
            return;
        }
    };

    let mut running_children = Vec::new();
    for hook_command in hook_commands {
        let mut child = match spawn_shell_command(&hook_command) {
            Ok(child) => child,
            Err(e) => {
                tracing::debug!(
                    "[git_ai_hooks] Failed to spawn post_notes_updated hook '{}': {}",
                    hook_command,
                    e
                );
                continue;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            let payload_for_stdin = payload_json.clone();
            let command_for_log = hook_command.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                if let Err(e) = stdin.write_all(payload_for_stdin.as_bytes()) {
                    tracing::debug!(
                        "[git_ai_hooks] Failed to write post_notes_updated stdin for '{}': {}",
                        command_for_log,
                        e
                    );
                }
            });
        } else {
            tracing::debug!(
                "[git_ai_hooks] Hook '{}' was spawned without a stdin pipe",
                hook_command
            );
        }

        running_children.push((hook_command, child));
    }

    wait_for_hooks_or_detach(running_children);
}

pub fn post_notes_updated_single(repo: &Repository, commit_sha: &str, note_content: &str) {
    let note_batch = vec![(commit_sha.to_string(), note_content.to_string())];
    post_notes_updated(repo, &note_batch);
}

fn wait_for_hooks_or_detach(mut children: Vec<(String, Child)>) {
    if children.is_empty() {
        return;
    }

    let deadline = Instant::now() + HOOK_WAIT_TIMEOUT;

    loop {
        let mut still_running = Vec::new();
        for (command, mut child) in children {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        tracing::debug!(
                            "[git_ai_hooks] Hook '{}' exited with status {}",
                            command,
                            status
                        );
                    }
                }
                Ok(None) => still_running.push((command, child)),
                Err(e) => {
                    tracing::debug!("[git_ai_hooks] Failed to poll hook '{}': {}", command, e);
                }
            }
        }

        if still_running.is_empty() {
            return;
        }

        if Instant::now() >= deadline {
            let detached_count = still_running.len();
            tracing::debug!(
                "[git_ai_hooks] Detaching {} unfinished hook command(s) after {}ms",
                detached_count,
                HOOK_WAIT_TIMEOUT.as_millis()
            );
            std::thread::spawn(move || {
                for (command, mut child) in still_running {
                    match child.wait() {
                        Ok(status) => {
                            if !status.success() {
                                tracing::debug!(
                                    "[git_ai_hooks] Detached hook '{}' exited with status {}",
                                    command,
                                    status
                                );
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                "[git_ai_hooks] Failed waiting detached hook '{}': {}",
                                command,
                                e
                            );
                        }
                    }
                }
            });
            return;
        }

        children = still_running;
        std::thread::sleep(HOOK_POLL_INTERVAL);
    }
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

#[cfg(not(windows))]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    process
}

fn spawn_shell_command(command: &str) -> std::io::Result<Child> {
    let mut cmd = shell_command(command);
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn build_repo_hook_context(repo: &Repository) -> RepoHookContext {
    let repo_url = repo
        .get_default_remote()
        .ok()
        .flatten()
        .and_then(|remote_name| {
            repo.remotes_with_urls().ok().and_then(|remotes| {
                remotes
                    .into_iter()
                    .find(|(name, _)| name == &remote_name)
                    .map(|(_, url)| url)
            })
        })
        .unwrap_or_default();

    let repo_name = repo_url
        .rsplit('/')
        .next()
        .unwrap_or(&repo_url)
        .trim_end_matches(".git")
        .to_string();

    let branch = repo
        .head()
        .ok()
        .and_then(|head_ref| head_ref.shorthand().ok())
        .unwrap_or_else(|| "unknown".to_string());

    let default_branch = repo
        .get_default_remote()
        .ok()
        .flatten()
        .and_then(|remote_name| {
            repo.remote_head(&remote_name).ok().map(|full| {
                full.strip_prefix(&format!("{}/", remote_name))
                    .unwrap_or(&full)
                    .to_string()
            })
        })
        .unwrap_or_else(|| "main".to_string());

    RepoHookContext {
        repo_url,
        repo_name,
        branch: branch.clone(),
        is_default_branch: branch == default_branch,
    }
}
