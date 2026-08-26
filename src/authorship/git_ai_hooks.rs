use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::git::repository::Repository;
#[cfg(windows)]
use crate::utils::CREATE_NO_WINDOW;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

const POST_NOTES_UPDATED_HOOK: &str = "post_notes_updated";
const HOOK_WAIT_TIMEOUT: Duration = Duration::from_secs(3);
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Schema version for the hook payload JSON. Bump on breaking changes.
pub const HOOK_SCHEMA_VERSION: u32 = 2;

struct RepoHookContext {
    repo_url: String,
    repo_name: String,
    repo_path: Option<PathBuf>,
    git_dir: PathBuf,
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

pub(crate) fn post_notes_updated_refs<'a>(
    repo: &Repository,
    notes: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let mut notes = notes.into_iter().peekable();
    if notes.peek().is_none() {
        return;
    }

    let config = Config::get();
    let hook_commands = config
        .git_ai_hook_commands(POST_NOTES_UPDATED_HOOK)
        .cloned()
        .unwrap_or_default();
    let has_sinks = !config.attribution_sinks().is_empty();
    if hook_commands.is_empty() && !has_sinks {
        return;
    }

    let context = build_repo_hook_context(repo);
    let repo_url = context.repo_url;
    let repo_name = context.repo_name;
    let repo_path = context.repo_path.map(|p| p.to_string_lossy().into_owned());
    let git_dir = context.git_dir.to_string_lossy().into_owned();
    let branch = context.branch;
    let is_default_branch = context.is_default_branch;
    let mut payload = notes
        .map(|(commit_sha, note_content)| {
            let mut entry = serde_json::json!({
                "schema_version": HOOK_SCHEMA_VERSION,
                "commit_sha": commit_sha,
                "repo_url": repo_url.as_str(),
                "repo_name": repo_name.as_str(),
                "git_dir": git_dir.as_str(),
                "branch": branch.as_str(),
                "is_default_branch": is_default_branch,
                "note_content": note_content,
            });
            if let Some(ref path) = repo_path {
                entry["repo_path"] = serde_json::Value::String(path.clone());
            }
            entry
        })
        .collect::<Vec<_>>();
    if config.attribution_fingerprints() {
        enrich_payload_with_fingerprints(repo, &mut payload);
    }
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

    if !hook_commands.is_empty() {
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

    if has_sinks {
        let events = super::attribution_sink::events_from_hook_payload(&payload);
        std::thread::spawn(move || {
            super::attribution_sink::dispatch_to_sinks(&events);
        });
    }
}

fn enrich_payload_with_fingerprints(repo: &Repository, payload: &mut [serde_json::Value]) {
    use super::fingerprint::{build_file_attributions, find_working_log_dir, read_checkpoints};

    for entry in payload {
        let Some(commit_sha) = entry.get("commit_sha").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let parent_sha = repo
            .revparse_single(&format!("{commit_sha}^"))
            .map(|object| object.id())
            .unwrap_or_else(|_| "initial".to_string());
        let Some(working_log_dir) = find_working_log_dir(repo.path(), &parent_sha) else {
            continue;
        };
        let checkpoints = match read_checkpoints(&working_log_dir) {
            Ok(checkpoints) => checkpoints,
            Err(error) => {
                tracing::debug!(
                    "[git_ai_hooks] Failed to read checkpoints for {}: {}",
                    commit_sha,
                    error
                );
                continue;
            }
        };
        let attributions = build_file_attributions(&working_log_dir, &checkpoints);
        if !attributions.is_empty() {
            entry["attributions"] = serde_json::json!(attributions);
        }
    }
}

pub fn post_notes_updated_single(repo: &Repository, commit_sha: &str, note_content: &str) {
    let note_batch = vec![(commit_sha.to_string(), note_content.to_string())];
    post_notes_updated(repo, &note_batch);
}

fn collect_stderr(child: &mut Child) -> String {
    child
        .stderr
        .take()
        .and_then(|mut stderr| {
            use std::io::Read;
            let mut buf = String::new();
            stderr.read_to_string(&mut buf).ok()?;
            if buf.is_empty() { None } else { Some(buf) }
        })
        .unwrap_or_default()
}

fn log_hook_failure(command: &str, status: &std::process::ExitStatus, child: &mut Child) {
    let stderr = collect_stderr(child);
    if stderr.is_empty() {
        tracing::debug!(
            "[git_ai_hooks] Hook '{}' exited with status {}",
            command,
            status
        );
    } else {
        tracing::debug!(
            "[git_ai_hooks] Hook '{}' exited with status {}: {}",
            command,
            status,
            stderr.trim()
        );
    }
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
                        log_hook_failure(&command, &status, &mut child);
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
                                log_hook_failure(&command, &status, &mut child);
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
        .stderr(Stdio::piped())
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

    let repo_path = repo.workdir().ok();
    let git_dir = repo.path().to_path_buf();

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
        repo_path,
        git_dir,
        branch: branch.clone(),
        is_default_branch: branch == default_branch,
    }
}

/// Build a v2 hook payload entry from explicit values (for testing and sink construction).
pub fn build_payload_entry(
    commit_sha: &str,
    repo_url: &str,
    repo_name: &str,
    repo_path: Option<&str>,
    git_dir: &str,
    branch: &str,
    is_default_branch: bool,
    note_content: &str,
) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "schema_version": HOOK_SCHEMA_VERSION,
        "commit_sha": commit_sha,
        "repo_url": repo_url,
        "repo_name": repo_name,
        "git_dir": git_dir,
        "branch": branch,
        "is_default_branch": is_default_branch,
        "note_content": note_content,
    });
    if let Some(path) = repo_path {
        entry["repo_path"] = serde_json::Value::String(path.to_string());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payload_entry_contains_v2_fields() {
        let entry = build_payload_entry(
            "abc123",
            "https://github.com/org/repo.git",
            "repo",
            Some("/home/user/repo"),
            "/home/user/repo/.git",
            "main",
            true,
            "note body",
        );
        assert_eq!(entry["schema_version"], HOOK_SCHEMA_VERSION);
        assert_eq!(entry["commit_sha"], "abc123");
        assert_eq!(entry["repo_path"], "/home/user/repo");
        assert_eq!(entry["git_dir"], "/home/user/repo/.git");
        assert_eq!(entry["repo_url"], "https://github.com/org/repo.git");
        assert_eq!(entry["repo_name"], "repo");
        assert_eq!(entry["branch"], "main");
        assert_eq!(entry["is_default_branch"], true);
        assert_eq!(entry["note_content"], "note body");
    }

    #[test]
    fn test_payload_entry_bare_repo_omits_repo_path() {
        let entry = build_payload_entry("abc123", "", "", None, "/srv/repo.git", "main", true, "");
        assert!(entry.get("repo_path").is_none());
        assert_eq!(entry["git_dir"], "/srv/repo.git");
        assert_eq!(entry["schema_version"], HOOK_SCHEMA_VERSION);
    }

    #[test]
    fn test_payload_batch_serialization() {
        let entries: Vec<serde_json::Value> = (0..3)
            .map(|i| {
                build_payload_entry(
                    &format!("sha{}", i),
                    "https://github.com/org/repo.git",
                    "repo",
                    Some("/repo"),
                    "/repo/.git",
                    "main",
                    true,
                    &format!("note {}", i),
                )
            })
            .collect();
        let json = serde_json::to_string(&entries).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[2]["commit_sha"], "sha2");
    }

    #[cfg(windows)]
    #[test]
    fn test_payload_entry_windows_paths() {
        let entry = build_payload_entry(
            "abc123",
            "",
            "",
            Some("C:\\Users\\dev\\repo"),
            "C:\\Users\\dev\\repo\\.git",
            "main",
            true,
            "",
        );
        assert_eq!(entry["repo_path"], "C:\\Users\\dev\\repo");
        assert_eq!(entry["git_dir"], "C:\\Users\\dev\\repo\\.git");
    }
}
