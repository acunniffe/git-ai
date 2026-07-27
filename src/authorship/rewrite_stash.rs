use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::authorship::imara_diff_utils::{DiffOp, capture_diff_slices};
use crate::authorship::working_log::{Checkpoint, CheckpointKind};
use crate::error::GitAiError;
use crate::git::repo_storage::{InitialAttributions, PersistedWorkingLog};
use crate::git::repository::{
    Repository, batch_read_paths_at_treeishes, disable_internal_git_hooks,
    exec_git_allow_nonzero_with_env,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StashMetadata {
    pub base_commit: String,
    pub timestamp: u64,
    #[serde(default)]
    pub pathspecs: Vec<String>,
}

fn stashes_dir(repo: &Repository) -> PathBuf {
    repo.storage.ai_dir.join("stashes")
}

fn stashes_v2_dir(repo: &Repository) -> PathBuf {
    repo.storage.ai_dir.join("stashes_v2")
}

fn cleanup_legacy_stashes_dir(repo: &Repository) {
    let legacy = stashes_dir(repo);
    if legacy.exists() {
        let _ = fs::remove_dir_all(legacy);
    }
}

fn stash_entry_dir(repo: &Repository, stash_sha: &str) -> PathBuf {
    stashes_v2_dir(repo).join(stash_sha)
}

fn stash_metadata_path(repo: &Repository, stash_sha: &str) -> PathBuf {
    stash_entry_dir(repo, stash_sha).join("metadata.json")
}

fn filtered_stash_working_log_base(stash_sha: &str) -> String {
    format!("_stash_filter_{}", stash_sha)
}

fn working_log_for_dir(repo: &Repository, dir: PathBuf, base_commit: &str) -> PersistedWorkingLog {
    let canonical_workdir = repo
        .storage
        .repo_workdir
        .canonicalize()
        .unwrap_or_else(|_| repo.storage.repo_workdir.clone());
    PersistedWorkingLog::new(
        dir,
        base_commit,
        repo.storage.repo_workdir.clone(),
        canonical_workdir,
        None,
    )
}

fn path_matches_any(path: &str, pathspecs: &[String]) -> bool {
    pathspecs.iter().any(|spec| {
        // Trailing-`*` prefix glob (e.g. `src/foo*`, or a bare `*`), matching
        // the pathspec semantics the pre-rewrite stash matcher supported.
        if let Some(prefix) = spec.strip_suffix('*') {
            return path.starts_with(prefix);
        }
        let normalized = spec.trim_end_matches('/');
        path == spec || path == normalized || {
            let prefix = format!("{}/", normalized);
            path.starts_with(&prefix)
        }
    })
}

fn clean_working_log_for_stash(
    repo: &Repository,
    head_sha: &str,
    pathspecs: &[String],
) -> Result<(), GitAiError> {
    if !repo.storage.has_working_log(head_sha) {
        return Ok(());
    }

    let persisted = repo.storage.working_log_for_base_commit(head_sha)?;
    let mut initial = persisted.read_initial_attributions();

    if pathspecs.is_empty() {
        initial.files.clear();
        initial.file_blobs.clear();
    } else {
        initial
            .files
            .retain(|path, _| !path_matches_any(path, pathspecs));
        initial
            .file_blobs
            .retain(|path, _| !path_matches_any(path, pathspecs));
    }

    trim_initial_metadata_to_referenced_authors(&mut initial);
    persisted.write_initial(initial)?;

    if pathspecs.is_empty() {
        return persisted.write_all_checkpoints(&[]);
    }

    let checkpoints = persisted.read_all_checkpoints()?;
    let filtered = checkpoints
        .into_iter()
        .map(|mut checkpoint| {
            checkpoint
                .entries
                .retain(|entry| !path_matches_any(&entry.file, pathspecs));
            checkpoint
        })
        .filter(|checkpoint| !checkpoint.entries.is_empty())
        .collect::<Vec<_>>();
    persisted.write_all_checkpoints(&filtered)?;
    Ok(())
}

pub fn handle_stash_create(
    repo: &Repository,
    stash_sha: &str,
    head_sha: &str,
    pathspecs: Vec<String>,
) -> Result<(), GitAiError> {
    cleanup_legacy_stashes_dir(repo);

    let metadata = StashMetadata {
        base_commit: head_sha.to_string(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        pathspecs: pathspecs.clone(),
    };

    let stash_dir = stash_entry_dir(repo, stash_sha);
    fs::create_dir_all(&stash_dir)?;

    let metadata_path = stash_metadata_path(repo, stash_sha);
    let json = serde_json::to_string_pretty(&metadata)?;
    fs::write(&metadata_path, json)?;

    // Save compact stashed file attributions before cleaning them from the working log.
    save_stash_attributions(repo, stash_sha, head_sha, &pathspecs)?;

    clean_working_log_for_stash(repo, head_sha, &pathspecs)?;

    Ok(())
}

pub fn handle_stash_pop_or_apply_with_head(
    repo: &Repository,
    stash_sha: &str,
    is_pop: bool,
    target_head: Option<&str>,
) -> Result<(), GitAiError> {
    cleanup_legacy_stashes_dir(repo);

    let metadata_path = stash_metadata_path(repo, stash_sha);

    if !metadata_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&metadata_path)?;
    let metadata: StashMetadata = serde_json::from_str(&content)?;

    let Some(current_head) = target_head.filter(|h| !h.is_empty()) else {
        return Ok(());
    };

    if metadata.base_commit != current_head {
        restore_stash_attributions_with_shift(repo, stash_sha, current_head)?;
    } else {
        restore_stash_attributions(repo, stash_sha, current_head)?;
    }

    if is_pop {
        let _ = fs::remove_dir_all(stash_entry_dir(repo, stash_sha));
    }

    Ok(())
}

pub fn handle_stash_drop(repo: &Repository, stash_sha: &str) -> Result<(), GitAiError> {
    cleanup_legacy_stashes_dir(repo);
    let _ = fs::remove_dir_all(stash_entry_dir(repo, stash_sha));
    Ok(())
}

fn save_stash_attributions(
    repo: &Repository,
    stash_sha: &str,
    head_sha: &str,
    pathspecs: &[String],
) -> Result<(), GitAiError> {
    if !repo.storage.has_working_log(head_sha) {
        return Ok(());
    }

    let filtered_base = if pathspecs.is_empty() {
        None
    } else {
        let filtered_base = filtered_stash_working_log_base(stash_sha);
        if let Err(err) = write_path_filtered_working_log(repo, head_sha, &filtered_base, pathspecs)
        {
            let _ = fs::remove_dir_all(repo.storage.working_logs.join(&filtered_base));
            return Err(err);
        }
        Some(filtered_base)
    };

    let base_commit = filtered_base.as_deref().unwrap_or(head_sha);
    let result =
        compact_stash_attributions_from_working_log(repo, stash_sha, head_sha, base_commit);

    if let Some(filtered_base) = filtered_base {
        let _ = fs::remove_dir_all(repo.storage.working_logs.join(filtered_base));
    }

    result
}

fn compact_stash_attributions_from_working_log(
    repo: &Repository,
    stash_sha: &str,
    stash_base_commit: &str,
    working_log_base_commit: &str,
) -> Result<(), GitAiError> {
    use crate::authorship::virtual_attribution::VirtualAttributions;

    let va = VirtualAttributions::from_persisted_working_log(
        repo.clone(),
        working_log_base_commit.to_string(),
        None,
    )?;
    let initial = va.to_initial_working_log_only();

    if initial.files.is_empty() {
        return Ok(());
    }

    let mut file_contents = HashMap::new();
    for file_path in initial.files.keys() {
        if let Some(content) = va.get_file_content(file_path).cloned() {
            file_contents.insert(file_path.clone(), content);
        }
    }

    let stash_log = working_log_for_dir(repo, stash_entry_dir(repo, stash_sha), stash_base_commit);
    stash_log.write_initial_attributions_with_contents(
        initial.files,
        initial.prompts,
        initial.humans,
        file_contents,
        initial.sessions,
    )
}

fn write_path_filtered_working_log(
    repo: &Repository,
    source_base_commit: &str,
    filtered_base_commit: &str,
    pathspecs: &[String],
) -> Result<(), GitAiError> {
    let source_log = repo
        .storage
        .working_log_for_base_commit(source_base_commit)?;
    let filtered_dir = repo.storage.working_logs.join(filtered_base_commit);
    let _ = fs::remove_dir_all(&filtered_dir);
    let filtered_log = repo
        .storage
        .working_log_for_base_commit(filtered_base_commit)?;

    let mut initial = source_log.read_initial_attributions();
    initial
        .files
        .retain(|path, _| path_matches_any(path, pathspecs));
    initial
        .file_blobs
        .retain(|path, _| path_matches_any(path, pathspecs));
    trim_initial_metadata_to_referenced_authors(&mut initial);
    copy_initial_blobs(&source_log, &filtered_log, &initial)?;
    filtered_log.write_initial(initial)?;

    write_path_filtered_checkpoints(&source_log, &filtered_log, pathspecs)
}

fn write_path_filtered_checkpoints(
    source_log: &PersistedWorkingLog,
    filtered_log: &PersistedWorkingLog,
    pathspecs: &[String],
) -> Result<(), GitAiError> {
    let source_checkpoints = source_log.dir.join("checkpoints.jsonl");
    let filtered_checkpoints = filtered_log.dir.join("checkpoints.jsonl");
    source_log.ensure_checkpoints_file_size_limit()?;
    if !source_checkpoints.exists() {
        return filtered_log.write_all_checkpoints(&[]);
    }

    fs::create_dir_all(&filtered_log.dir)?;
    let input = fs::File::open(source_checkpoints)?;
    let mut output = BufWriter::new(fs::File::create(filtered_checkpoints)?);
    let mut copied_blobs = HashSet::new();

    for line in BufReader::new(input).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let mut checkpoint: Checkpoint = serde_json::from_str(&line)?;
        checkpoint
            .entries
            .retain(|entry| path_matches_any(&entry.file, pathspecs));
        if checkpoint.entries.is_empty() {
            continue;
        }

        checkpoint.diff.clear();
        for entry in &checkpoint.entries {
            copy_blob_sha(source_log, filtered_log, &entry.blob_sha, &mut copied_blobs)?;
        }

        serde_json::to_writer(&mut output, &checkpoint)?;
        output.write_all(b"\n")?;
    }

    output.flush()?;
    Ok(())
}

fn copy_blob_sha(
    source_log: &PersistedWorkingLog,
    target_log: &PersistedWorkingLog,
    blob_sha: &str,
    copied_blobs: &mut HashSet<String>,
) -> Result<(), GitAiError> {
    if blob_sha.is_empty() || !copied_blobs.insert(blob_sha.to_string()) {
        return Ok(());
    }

    let source = source_log.dir.join("blobs").join(blob_sha);
    let target_blobs = target_log.dir.join("blobs");
    fs::create_dir_all(&target_blobs)?;
    fs::copy(source, target_blobs.join(blob_sha))?;
    Ok(())
}

fn trim_initial_metadata_to_referenced_authors(initial: &mut InitialAttributions) {
    let human_sentinel = CheckpointKind::Human.to_str();
    let mut referenced_authors = HashSet::new();
    let mut referenced_sessions = HashSet::new();

    for attrs in initial.files.values() {
        for attr in attrs {
            if attr.author_id == human_sentinel {
                continue;
            }

            referenced_authors.insert(attr.author_id.clone());
            if attr.author_id.starts_with("s_") {
                let session_key = attr
                    .author_id
                    .split("::")
                    .next()
                    .unwrap_or(&attr.author_id)
                    .to_string();
                referenced_sessions.insert(session_key);
            }
        }
    }

    initial
        .prompts
        .retain(|author_id, _| referenced_authors.contains(author_id));
    initial
        .humans
        .retain(|author_id, _| referenced_authors.contains(author_id));
    initial
        .sessions
        .retain(|session_id, _| referenced_sessions.contains(session_id));
}

fn restore_stash_attributions(
    repo: &Repository,
    stash_sha: &str,
    current_head: &str,
) -> Result<(), GitAiError> {
    let stash_log = working_log_for_dir(repo, stash_entry_dir(repo, stash_sha), current_head);
    if !stash_log.initial_file.exists() {
        return Ok(());
    }

    let initial = stash_log.read_initial_attributions();
    if initial.files.is_empty() {
        return Ok(());
    }

    let working_log = repo.storage.working_log_for_base_commit(current_head)?;
    copy_initial_blobs(&stash_log, &working_log, &initial)?;
    remove_checkpoint_entries_for_files(&working_log, initial.files.keys().cloned())?;
    merge_initial_replacing_paths(&working_log, initial)?;
    Ok(())
}

fn restore_stash_attributions_with_shift(
    repo: &Repository,
    stash_sha: &str,
    current_head: &str,
) -> Result<(), GitAiError> {
    let stash_log = working_log_for_dir(repo, stash_entry_dir(repo, stash_sha), current_head);
    if !stash_log.initial_file.exists() {
        return Ok(());
    }

    let initial = stash_log.read_initial_attributions();
    if initial.files.is_empty() {
        return Ok(());
    }

    let mut stash_file_contents: HashMap<String, String> = HashMap::new();
    for file_path in initial.files.keys() {
        if let Some(content) = stash_log.stored_initial_file_content_from(&initial, file_path) {
            stash_file_contents.insert(file_path.clone(), content);
        }
    }

    // Reconstruct the applied content from immutable trees.
    let mut files: HashMap<String, Vec<LineAttribution>> = HashMap::new();
    let mut file_contents: HashMap<String, String> = HashMap::new();

    let applied_paths: Vec<String> = initial.files.keys().cloned().collect();
    let applied_contents =
        reconstruct_stash_applied_contents(repo, stash_sha, current_head, &applied_paths)?;

    for (file_path, attrs) in &initial.files {
        let stash_content = stash_file_contents
            .get(file_path)
            .cloned()
            .unwrap_or_default();
        let current_content = applied_contents.get(file_path).cloned().unwrap_or_default();

        if current_content.is_empty() {
            continue;
        }

        if stash_content == current_content {
            files.insert(file_path.clone(), attrs.clone());
            file_contents.insert(file_path.clone(), current_content);
            continue;
        }

        // Content-based shift using Equal regions
        let old_lines: Vec<&str> = stash_content.lines().collect();
        let new_lines: Vec<&str> = current_content.lines().collect();
        let ops = capture_diff_slices(&old_lines, &new_lines);

        let mut line_map: HashMap<u32, u32> = HashMap::new();
        for op in &ops {
            if let DiffOp::Equal {
                old_index,
                new_index,
                len,
            } = op
            {
                for i in 0..*len {
                    line_map.insert((*old_index + i + 1) as u32, (*new_index + i + 1) as u32);
                }
            }
        }

        let shifted: Vec<LineAttribution> = attrs
            .iter()
            .filter_map(|attr| {
                let new_start = line_map.get(&attr.start_line).copied()?;
                let new_end = line_map.get(&attr.end_line).copied()?;
                Some(LineAttribution::new(
                    new_start,
                    new_end,
                    attr.author_id.clone(),
                    attr.overrode.clone(),
                ))
            })
            .collect();

        if !shifted.is_empty() {
            files.insert(file_path.clone(), shifted);
            file_contents.insert(file_path.clone(), current_content);
        }
    }

    if files.is_empty() {
        return Ok(());
    }

    let working_log = repo.storage.working_log_for_base_commit(current_head)?;
    remove_checkpoint_entries_for_files(&working_log, files.keys().cloned())?;
    merge_initial_replacing_paths_with_contents(
        &working_log,
        files,
        initial.prompts,
        initial.humans,
        file_contents,
        initial.sessions,
    )?;

    Ok(())
}

fn copy_initial_blobs(
    src_log: &PersistedWorkingLog,
    dst_log: &PersistedWorkingLog,
    initial: &InitialAttributions,
) -> Result<(), GitAiError> {
    if initial.file_blobs.is_empty() {
        return Ok(());
    }

    let dst_blobs = dst_log.dir.join("blobs");
    fs::create_dir_all(&dst_blobs)?;
    for blob_sha in initial.file_blobs.values() {
        let src = src_log.dir.join("blobs").join(blob_sha);
        let dst = dst_blobs.join(blob_sha);
        if src.exists() && !dst.exists() {
            fs::copy(src, dst)?;
        }
    }
    Ok(())
}

fn remove_checkpoint_entries_for_files<I>(
    working_log: &PersistedWorkingLog,
    files: I,
) -> Result<(), GitAiError>
where
    I: IntoIterator<Item = String>,
{
    let files: HashSet<String> = files.into_iter().collect();
    if files.is_empty() {
        return Ok(());
    }

    let checkpoints = working_log.read_all_checkpoints()?;
    if checkpoints.is_empty() {
        return Ok(());
    }

    let filtered = checkpoints
        .into_iter()
        .map(|mut checkpoint| {
            checkpoint
                .entries
                .retain(|entry| !files.contains(&entry.file));
            checkpoint
        })
        .filter(|checkpoint| !checkpoint.entries.is_empty())
        .collect::<Vec<_>>();
    working_log.write_all_checkpoints(&filtered)?;
    Ok(())
}

fn merge_initial_replacing_paths(
    working_log: &PersistedWorkingLog,
    mut source: InitialAttributions,
) -> Result<(), GitAiError> {
    if source.files.is_empty() {
        return Ok(());
    }

    let restored_paths: HashSet<String> = source.files.keys().cloned().collect();
    let mut target = working_log.read_initial_attributions();
    for path in &restored_paths {
        target.files.remove(path);
        target.file_blobs.remove(path);
    }

    target.files.extend(source.files.drain());
    target.file_blobs.extend(source.file_blobs.drain());
    target.prompts.extend(source.prompts.drain());
    target.humans.extend(source.humans);
    target.sessions.extend(source.sessions);
    working_log.write_initial(target)?;
    Ok(())
}

fn merge_initial_replacing_paths_with_contents(
    working_log: &PersistedWorkingLog,
    files: HashMap<String, Vec<LineAttribution>>,
    prompts: HashMap<String, PromptRecord>,
    humans: BTreeMap<String, HumanRecord>,
    file_contents: HashMap<String, String>,
    sessions: BTreeMap<String, SessionRecord>,
) -> Result<(), GitAiError> {
    let files: HashMap<String, Vec<LineAttribution>> = files
        .into_iter()
        .filter(|(_, attrs)| !attrs.is_empty())
        .collect();
    if files.is_empty() {
        return Ok(());
    }

    let mut file_blobs = HashMap::new();
    for file_path in files.keys() {
        let content = file_contents.get(file_path).ok_or_else(|| {
            GitAiError::Generic(format!(
                "stash restore missing file content snapshot for {}",
                file_path
            ))
        })?;
        let blob_sha = working_log.persist_file_version(content)?;
        file_blobs.insert(file_path.clone(), blob_sha);
    }

    merge_initial_replacing_paths(
        working_log,
        InitialAttributions {
            files,
            prompts,
            file_blobs,
            humans,
            sessions,
        },
    )
}

fn reconstruct_stash_applied_contents(
    repo: &Repository,
    stash_sha: &str,
    target_head: &str,
    file_paths: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if file_paths.is_empty() {
        return Ok(HashMap::new());
    }

    let unique = format!(
        "git-ai-stash-apply-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let temp_dir = std::env::temp_dir().join(unique);
    let index_path = temp_dir.join("index");
    let worktree_path = temp_dir.join("worktree");
    fs::create_dir_all(&worktree_path)?;

    let result = (|| {
        let _guard = disable_internal_git_hooks();
        run_isolated_git(
            repo,
            vec!["read-tree".to_string(), target_head.to_string()],
            &index_path,
            &worktree_path,
            true,
        )?;
        // `-f` (force) is required: on case-insensitive filesystems (macOS/Windows)
        // a tree with case-colliding paths (e.g. `README.md` and `readme.md`) makes
        // `checkout-index -a` fail with "already exists, no checkout" for the second
        // entry. This is a throwaway scratch worktree, so last-casing-wins is harmless.
        run_isolated_git(
            repo,
            vec![
                "checkout-index".to_string(),
                "-a".to_string(),
                "-f".to_string(),
            ],
            &index_path,
            &worktree_path,
            true,
        )?;
        let _ = run_isolated_git(
            repo,
            vec![
                "stash".to_string(),
                "apply".to_string(),
                stash_sha.to_string(),
            ],
            &index_path,
            &worktree_path,
            false,
        )?;
        run_isolated_git(
            repo,
            vec!["add".to_string(), "-A".to_string()],
            &index_path,
            &worktree_path,
            true,
        )?;
        let output = run_isolated_git(
            repo,
            vec!["write-tree".to_string()],
            &index_path,
            &worktree_path,
            true,
        )?;
        let result_tree = String::from_utf8(output.stdout)?.trim().to_string();
        let requests: Vec<(String, String)> = file_paths
            .iter()
            .map(|path| (result_tree.clone(), path.clone()))
            .collect();
        let contents = batch_read_paths_at_treeishes(repo, &requests)?;
        Ok(contents
            .into_iter()
            .map(|((_, path), content)| (path, content))
            .collect())
    })();

    let _ = fs::remove_dir_all(&temp_dir);
    result
}

fn run_isolated_git(
    repo: &Repository,
    args: Vec<String>,
    index_path: &std::path::Path,
    worktree_path: &std::path::Path,
    require_success: bool,
) -> Result<std::process::Output, GitAiError> {
    let mut full_args = repo.global_args_for_exec();
    full_args.extend(args);
    let envs = [
        ("GIT_INDEX_FILE", index_path.as_os_str()),
        ("GIT_WORK_TREE", worktree_path.as_os_str()),
    ];
    let output = exec_git_allow_nonzero_with_env(&full_args, &envs)?;
    if require_success && !output.status.success() {
        return Err(GitAiError::GitCliError {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            args: full_args,
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_matches_any_exact() {
        let specs = vec!["src/main.rs".to_string()];
        assert!(path_matches_any("src/main.rs", &specs));
        assert!(!path_matches_any("src/lib.rs", &specs));
    }

    #[test]
    fn test_path_matches_any_directory_prefix() {
        let specs = vec!["src/".to_string()];
        assert!(path_matches_any("src/main.rs", &specs));
        assert!(path_matches_any("src/lib.rs", &specs));
        assert!(!path_matches_any("tests/main.rs", &specs));
    }

    #[test]
    fn test_path_matches_any_directory_without_slash() {
        let specs = vec!["src".to_string()];
        assert!(path_matches_any("src/main.rs", &specs));
        assert!(!path_matches_any("src2/main.rs", &specs));
    }

    #[test]
    fn test_path_matches_any_trailing_slash_normalized() {
        let specs = vec!["dir/".to_string()];
        assert!(path_matches_any("dir", &specs));
        assert!(path_matches_any("dir/file.txt", &specs));
    }

    #[test]
    fn test_path_matches_any_empty_specs() {
        let specs: Vec<String> = vec![];
        assert!(!path_matches_any("anything", &specs));
    }

    #[test]
    fn test_path_matches_any_trailing_glob() {
        // Regression (#5): the pre-rewrite matcher honored a trailing `*`
        // prefix-glob; path_matches_any dropped it, so `git stash push --
        // 'src/foo*'` no longer matched src/foobar.txt.
        let specs = vec!["src/foo*".to_string()];
        assert!(path_matches_any("src/foobar.txt", &specs));
        assert!(path_matches_any("src/foo.rs", &specs));
        assert!(!path_matches_any("src/bar.rs", &specs));
        // A bare `*` matches anything.
        assert!(path_matches_any("anything/at/all.txt", &["*".to_string()]));
    }

    #[test]
    fn test_stash_metadata_serialization_roundtrip() {
        let metadata = StashMetadata {
            base_commit: "abc123def456".to_string(),
            timestamp: 1700000000,
            pathspecs: vec!["src/".to_string(), "Cargo.toml".to_string()],
        };

        let json = serde_json::to_string_pretty(&metadata).unwrap();
        let deserialized: StashMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.base_commit, "abc123def456");
        assert_eq!(deserialized.timestamp, 1700000000);
        assert_eq!(deserialized.pathspecs.len(), 2);
        assert_eq!(deserialized.pathspecs[0], "src/");
        assert_eq!(deserialized.pathspecs[1], "Cargo.toml");
    }

    #[test]
    fn test_stash_metadata_empty_pathspecs_default() {
        let json = r#"{"base_commit":"abc123","timestamp":100}"#;
        let metadata: StashMetadata = serde_json::from_str(json).unwrap();
        assert!(metadata.pathspecs.is_empty());
    }
}
