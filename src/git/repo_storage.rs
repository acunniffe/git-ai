use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::generate_short_hash;
use crate::authorship::working_log::{CHECKPOINT_API_VERSION, Checkpoint, CheckpointKind};
use crate::error::GitAiError;
use crate::utils::normalize_to_posix;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const MAX_CHECKPOINTS_JSONL_BYTES: u64 = 1024 * 1024 * 1024;

#[cfg(feature = "test-support")]
const TEST_CHECKPOINTS_JSONL_MAX_BYTES_ENV: &str = "GIT_AI_TEST_CHECKPOINTS_JSONL_MAX_BYTES";

/// Initial attributions data structure stored in the INITIAL file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InitialAttributions {
    /// Map of file path to line attributions
    pub files: HashMap<String, Vec<LineAttribution>>,
    /// Map of author_id (hash) to PromptRecord for prompt tracking
    pub prompts: HashMap<String, PromptRecord>,
    /// Optional blob snapshot of the file content represented by INITIAL.
    #[serde(default)]
    pub file_blobs: HashMap<String, String>,
    /// Known human records: `h_<hash>` -> HumanRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub humans: std::collections::BTreeMap<String, HumanRecord>,
    /// Session records: `s_<session_id>` -> SessionRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sessions: std::collections::BTreeMap<String, SessionRecord>,
}

#[derive(Debug, Clone)]
pub struct RepoStorage {
    pub ai_dir: PathBuf,
    pub repo_workdir: PathBuf,
    pub working_logs: PathBuf,
    pub logs: PathBuf,
}

impl RepoStorage {
    pub fn for_repo_path(repo_path: &Path, repo_workdir: &Path) -> Result<RepoStorage, GitAiError> {
        Self::for_ai_dir(&repo_path.join("ai"), repo_workdir)
    }

    pub fn for_isolated_worktree_storage(
        ai_dir: &Path,
        repo_workdir: &Path,
    ) -> Result<RepoStorage, GitAiError> {
        Self::for_ai_dir(ai_dir, repo_workdir)
    }

    fn for_ai_dir(ai_dir: &Path, repo_workdir: &Path) -> Result<RepoStorage, GitAiError> {
        let working_logs_dir = ai_dir.join("working_logs");
        let logs_dir = ai_dir.join("logs");

        let config = RepoStorage {
            ai_dir: ai_dir.to_path_buf(),
            repo_workdir: repo_workdir.to_path_buf(),
            working_logs: working_logs_dir,
            logs: logs_dir,
        };

        config.ensure_config_directory()?;
        Ok(config)
    }

    #[doc(hidden)]
    pub fn ensure_config_directory(&self) -> Result<(), GitAiError> {
        fs::create_dir_all(&self.ai_dir)?;

        // Create working_logs directory
        fs::create_dir_all(&self.working_logs)?;

        // Create logs directory for Sentry events
        fs::create_dir_all(&self.logs)?;

        Ok(())
    }

    /* Working Log Persistance */

    pub fn has_working_log(&self, sha: &str) -> bool {
        self.working_logs.join(sha).exists()
    }

    pub fn working_log_for_base_commit(
        &self,
        sha: &str,
    ) -> Result<PersistedWorkingLog, GitAiError> {
        let working_log_dir = self.working_logs.join(sha);
        fs::create_dir_all(&working_log_dir)?;
        let canonical_workdir = self
            .repo_workdir
            .canonicalize()
            .unwrap_or_else(|_| self.repo_workdir.clone());
        Ok(PersistedWorkingLog::new(
            working_log_dir,
            sha,
            self.repo_workdir.clone(),
            canonical_workdir,
            None,
        ))
    }

    pub fn delete_working_log_for_base_commit(&self, sha: &str) -> Result<(), GitAiError> {
        let working_log_dir = self.working_logs.join(sha);
        crate::wltrace::wltrace("working_log.delete", &working_log_dir, String::new);
        if working_log_dir.exists() {
            // Both debug and release: move to old-{sha} for retention
            let old_dir = self.working_logs.join(format!("old-{}", sha));
            // If old-{sha} already exists, remove it first
            if old_dir.exists() {
                fs::remove_dir_all(&old_dir)?;
            }
            fs::rename(&working_log_dir, &old_dir)?;

            // Write a timestamp marker so we know when it was archived
            let marker = old_dir.join(".archived_at");
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            // Best-effort; don't fail the commit if we can't write the marker
            let _ = fs::write(&marker, now.to_string());

            tracing::debug!("Moved checkpoint directory from {} to old-{}", sha, sha);

            // In production builds, prune old working logs that have expired.
            // Debug builds never prune so developers can inspect old state.
            if !cfg!(debug_assertions) {
                self.prune_expired_old_working_logs();
            }
        }
        Ok(())
    }

    /// Number of seconds to retain archived working logs in production builds (7 days).
    const OLD_WORKING_LOG_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

    /// Remove archived (`old-*`) working log directories whose `.archived_at`
    /// timestamp is older than `OLD_WORKING_LOG_RETENTION_SECS`.
    /// Errors are intentionally swallowed so pruning never breaks the commit flow.
    #[doc(hidden)]
    pub fn prune_expired_old_working_logs(&self) {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();

        let entries = match fs::read_dir(&self.working_logs) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("old-") {
                continue;
            }

            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }

            let marker = dir_path.join(".archived_at");
            let archived_at = match fs::read_to_string(&marker) {
                Ok(contents) => contents.trim().parse::<u64>().unwrap_or(0),
                // No marker means this was created before the retention feature;
                // treat it as immediately expired so it gets cleaned up.
                Err(_) => 0,
            };

            if now_secs.saturating_sub(archived_at) >= Self::OLD_WORKING_LOG_RETENTION_SECS {
                tracing::debug!("Pruning expired old working log: {}", name_str);
                let _ = fs::remove_dir_all(&dir_path);
            }
        }
    }

    /// Move a working log directory from one commit SHA to another.
    /// If the destination already has checkpoints, preserve the old-base entries first and
    /// append the destination entries after them.
    pub fn rename_working_log(&self, old_sha: &str, new_sha: &str) -> Result<(), GitAiError> {
        if old_sha == new_sha {
            return Ok(());
        }
        let old_dir = self.working_logs.join(old_sha);
        let new_dir = self.working_logs.join(new_sha);
        if !old_dir.exists() {
            return Ok(());
        }
        if !new_dir.exists() {
            crate::wltrace::wltrace("working_log.rename", &old_dir, || {
                format!("to={}", new_dir.display())
            });
            fs::rename(&old_dir, &new_dir)?;
            tracing::debug!("Renamed working log from {} to {}", old_sha, new_sha);
        } else {
            crate::wltrace::wltrace("working_log.merge", &old_dir, || {
                format!("to={}", new_dir.display())
            });
            self.merge_working_log_dirs(old_sha, new_sha, &old_dir, &new_dir)?;
            fs::remove_dir_all(&old_dir)?;
            tracing::debug!("Merged working log from {} into {}", old_sha, new_sha);
        }
        Ok(())
    }

    fn merge_working_log_dirs(
        &self,
        old_sha: &str,
        new_sha: &str,
        old_dir: &Path,
        new_dir: &Path,
    ) -> Result<(), GitAiError> {
        copy_dir_contents(&old_dir.join("blobs"), &new_dir.join("blobs"))?;

        let canonical = self
            .repo_workdir
            .canonicalize()
            .unwrap_or_else(|_| self.repo_workdir.clone());
        let old_log = PersistedWorkingLog::new(
            old_dir.to_path_buf(),
            old_sha,
            self.repo_workdir.clone(),
            canonical.clone(),
            None,
        );
        let new_log = PersistedWorkingLog::new(
            new_dir.to_path_buf(),
            new_sha,
            self.repo_workdir.clone(),
            canonical,
            None,
        );
        let mut blocked_edge_recovery_paths = old_log.edge_recovery_blocked_paths();
        blocked_edge_recovery_paths.extend(new_log.edge_recovery_blocked_paths());

        // Preserve OLD-base entries first (per rename_working_log's contract):
        // start from the old INITIAL and only insert a new-base entry when its
        // key is absent, so old wins on any shared key. HashMap::extend would do
        // the opposite (new clobbers old). The checkpoints Vec below is already
        // old-then-new, so it needs no such guard.
        let mut merged_initial = old_log.read_initial_attributions();
        let new_initial = new_log.read_initial_attributions();
        for (k, v) in new_initial.files {
            merged_initial.files.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.prompts {
            merged_initial.prompts.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.file_blobs {
            merged_initial.file_blobs.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.humans {
            merged_initial.humans.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.sessions {
            merged_initial.sessions.entry(k).or_insert(v);
        }
        new_log.write_initial(merged_initial)?;

        let mut checkpoints = old_log.read_all_checkpoints()?;
        checkpoints.extend(new_log.read_all_checkpoints()?);
        new_log.write_all_checkpoints(&checkpoints)?;
        if !blocked_edge_recovery_paths.is_empty() {
            new_log.block_edge_recovery_for_paths(blocked_edge_recovery_paths)?;
        }
        Ok(())
    }
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<(), GitAiError> {
    if !src.exists() {
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_contents(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct PersistedWorkingLog {
    pub dir: PathBuf,
    #[allow(dead_code)]
    pub base_commit: String,
    pub repo_workdir: PathBuf,
    /// Canonical (absolute, resolved) version of workdir for reliable path comparisons
    /// On Windows, this uses the \\?\ UNC prefix format
    #[allow(dead_code)]
    pub canonical_workdir: PathBuf,
    pub dirty_files: Option<HashMap<String, Arc<str>>>,
    pub initial_file: PathBuf,
}

impl PersistedWorkingLog {
    const EDGE_RECOVERY_BLOCKED_MARKER: &str = ".edge-recovery-blocked";

    pub fn new(
        dir: PathBuf,
        base_commit: &str,
        repo_root: PathBuf,
        canonical_workdir: PathBuf,
        dirty_files: Option<HashMap<String, Arc<str>>>,
    ) -> Self {
        let initial_file = dir.join("INITIAL");
        Self {
            dir,
            base_commit: base_commit.to_string(),
            repo_workdir: repo_root,
            canonical_workdir,
            dirty_files,
            initial_file,
        }
    }

    pub fn set_dirty_files(&mut self, dirty_files: Option<HashMap<String, Arc<str>>>) {
        let normalized_dirty_files = dirty_files.map(|map| {
            map.into_iter()
                .map(|(file_path, content)| {
                    let relative_path = self.to_repo_relative_path(&file_path);
                    let normalized_path = normalize_to_posix(&relative_path);
                    (normalized_path, content)
                })
                .collect::<HashMap<_, _>>()
        });

        self.dirty_files = normalized_dirty_files;
    }

    pub fn block_edge_recovery(&self) -> Result<(), GitAiError> {
        self.block_edge_recovery_for_paths(self.all_touched_files()?)
    }

    pub fn block_edge_recovery_for_paths(
        &self,
        paths: impl IntoIterator<Item = String>,
    ) -> Result<(), GitAiError> {
        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort_unstable();
        paths.dedup();
        let marker = self.dir.join(Self::EDGE_RECOVERY_BLOCKED_MARKER);
        if paths.is_empty() {
            if marker.exists() {
                crate::wltrace::wltrace("working_log.edge_block.clear", &self.dir, String::new);
                fs::remove_file(marker)?;
            }
            return Ok(());
        }
        crate::wltrace::wltrace("working_log.edge_block.write", &self.dir, || {
            format!("paths={}", paths.join(","))
        });
        fs::write(marker, serde_json::to_vec(&paths)?)?;
        Ok(())
    }

    pub fn edge_recovery_blocked(&self) -> bool {
        !self.edge_recovery_blocked_paths().is_empty()
    }

    pub fn edge_recovery_blocked_paths(&self) -> HashSet<String> {
        let marker = self.dir.join(Self::EDGE_RECOVERY_BLOCKED_MARKER);
        let Ok(contents) = fs::read(&marker) else {
            return HashSet::new();
        };
        if contents.is_empty() {
            // Compatibility with working logs written by versions where the
            // marker was an empty, working-log-wide sentinel.
            return self.all_touched_files().unwrap_or_default();
        }
        serde_json::from_slice::<Vec<String>>(&contents)
            .map(|paths| paths.into_iter().collect())
            .unwrap_or_else(|_| self.all_touched_files().unwrap_or_default())
    }

    pub fn reset_working_log(&self) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.reset", &self.dir, String::new);
        // Clear all blobs by removing the blobs directory
        let blobs_dir = self.dir.join("blobs");
        if blobs_dir.exists() {
            fs::remove_dir_all(&blobs_dir)?;
        }

        // Clear checkpoints by truncating the JSONL file
        let checkpoints_file = self.checkpoints_file();
        fs::write(&checkpoints_file, "")?;

        // Clear INITIAL attributions file so stale attributions from a
        // previous working state do not persist across resets
        if self.initial_file.exists() {
            fs::remove_file(&self.initial_file)?;
        }
        let edge_recovery_marker = self.dir.join(Self::EDGE_RECOVERY_BLOCKED_MARKER);
        if edge_recovery_marker.exists() {
            fs::remove_file(edge_recovery_marker)?;
        }

        Ok(())
    }

    pub fn checkpoints_file(&self) -> PathBuf {
        self.dir.join("checkpoints.jsonl")
    }

    /* blob storage */
    pub fn get_file_version(&self, sha: &str) -> Result<String, GitAiError> {
        let blob_path = self.dir.join("blobs").join(sha);
        Ok(fs::read_to_string(blob_path)?)
    }

    #[allow(dead_code)]
    pub fn persist_file_version(&self, content: &str) -> Result<String, GitAiError> {
        // Create SHA256 hash of the content
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let sha = format!("{:x}", hasher.finalize());

        // Ensure blobs directory exists
        let blobs_dir = self.dir.join("blobs");
        fs::create_dir_all(&blobs_dir)?;

        // Write content to blob file
        let blob_path = blobs_dir.join(&sha);
        fs::write(blob_path, content)?;

        Ok(sha)
    }

    pub fn to_repo_absolute_path(&self, file_path: &str) -> String {
        if Path::new(file_path).is_absolute() {
            return file_path.to_string();
        }
        self.repo_workdir
            .join(file_path)
            .to_string_lossy()
            .to_string()
    }

    pub fn to_repo_relative_path(&self, file_path: &str) -> String {
        if !Path::new(file_path).is_absolute() {
            return file_path.to_string();
        }
        let path = Path::new(file_path);

        // Try without canonicalizing first
        if path.starts_with(&self.repo_workdir) {
            return path
                .strip_prefix(&self.repo_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        // If we couldn't match yet, try canonicalizing both repo_workdir and the input path
        // On Windows, this uses the canonical_workdir that was pre-computed
        #[cfg(windows)]
        let canonical_workdir = &self.canonical_workdir;

        #[cfg(not(windows))]
        let canonical_workdir = match self.repo_workdir.canonicalize() {
            Ok(p) => p,
            Err(_) => self.repo_workdir.clone(),
        };

        let canonical_path = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        #[cfg(windows)]
        if canonical_path.starts_with(canonical_workdir) {
            return canonical_path
                .strip_prefix(canonical_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        #[cfg(not(windows))]
        if canonical_path.starts_with(&canonical_workdir) {
            return canonical_path
                .strip_prefix(&canonical_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        file_path.to_string()
    }

    pub fn read_current_file_content(&self, file_path: &str) -> Result<Arc<str>, GitAiError> {
        if let Some(ref dirty_files) = self.dirty_files
            && let Some(content) = dirty_files.get(&file_path.to_string())
        {
            return Ok(content.clone());
        }

        Err(GitAiError::Generic(format!(
            "read_current_file_content: file '{}' not found in dirty_files snapshot (filesystem fallback is not allowed in checkpoint flow)",
            file_path
        )))
    }

    /* append checkpoint */
    pub fn append_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), GitAiError> {
        let mut checkpoints = self.read_all_checkpoints().unwrap_or_default();
        self.append_checkpoint_to(&mut checkpoints, checkpoint.clone())
    }

    /// Append to a checkpoint collection that the caller has already loaded.
    ///
    /// Checkpoint execution needs the prior collection to calculate attribution.
    /// Reusing it here avoids deserializing the entire working log a second time
    /// and avoids cloning the new checkpoint's attribution payload.
    pub fn append_checkpoint_to(
        &self,
        checkpoints: &mut Vec<Checkpoint>,
        checkpoint: Checkpoint,
    ) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.append_checkpoint", &self.dir, String::new);

        checkpoints.push(checkpoint);

        // Prune char-level attributions from older checkpoints for the same files
        // Only the most recent checkpoint per file needs char-level precision
        self.prune_old_char_attributions(checkpoints);

        // Write all checkpoints back
        self.write_all_checkpoints(checkpoints)
    }

    pub fn read_all_checkpoints(&self) -> Result<Vec<Checkpoint>, GitAiError> {
        self.read_all_checkpoints_with_size_limit(Self::checkpoints_file_size_limit_bytes())
    }

    #[cfg(feature = "test-support")]
    pub fn read_all_checkpoints_with_size_limit_for_test(
        &self,
        max_bytes: u64,
    ) -> Result<Vec<Checkpoint>, GitAiError> {
        self.read_all_checkpoints_with_size_limit(max_bytes)
    }

    pub fn ensure_checkpoints_file_size_limit(&self) -> Result<(), GitAiError> {
        self.truncate_oversized_checkpoints_file(Self::checkpoints_file_size_limit_bytes())?;
        Ok(())
    }

    fn read_all_checkpoints_with_size_limit(
        &self,
        max_bytes: u64,
    ) -> Result<Vec<Checkpoint>, GitAiError> {
        crate::wltrace::wltrace("working_log.read_checkpoints", &self.dir, String::new);
        let checkpoints_file = self.checkpoints_file();

        if !checkpoints_file.exists() {
            return Ok(Vec::new());
        }

        if self.truncate_oversized_checkpoints_file(max_bytes)? {
            return Ok(Vec::new());
        }

        let input = fs::File::open(&checkpoints_file)?;
        let mut checkpoints = Vec::new();

        // Parse JSONL file - each line is a separate JSON object
        for line in BufReader::new(input).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let checkpoint: Checkpoint = serde_json::from_str(&line).map_err(|e| {
                crate::wltrace::wltrace("working_log.read_checkpoints.TORN", &self.dir, || {
                    format!("err={e} line_len={}", line.len())
                });
                std::io::Error::new(std::io::ErrorKind::InvalidData, e)
            })?;

            if checkpoint.api_version != CHECKPOINT_API_VERSION {
                tracing::debug!(
                    "unsupported checkpoint api version: {} (silently skipping checkpoint)",
                    checkpoint.api_version
                );
                continue;
            }

            checkpoints.push(checkpoint);
        }

        // Migrate 7-char prompt hashes to 16-char hashes
        // Step 1: Build mapping from old 7-char hash to new 16-char hash
        let mut old_to_new_hash: HashMap<String, String> = HashMap::new();

        for checkpoint in &checkpoints {
            if let Some(agent_id) = &checkpoint.agent_id {
                let new_hash = generate_short_hash(&agent_id.id, &agent_id.tool);
                let old_hash = new_hash[..7].to_string();
                old_to_new_hash.insert(old_hash, new_hash);
            }
        }

        // Step 2: Replace 7-char author_ids in all checkpoints' attributions and line_attributions
        let mut migrated_checkpoints = Vec::new();
        for mut checkpoint in checkpoints {
            for entry in &mut checkpoint.entries {
                // Replace author_ids in attributions
                for attr in &mut entry.attributions {
                    if attr.author_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(&attr.author_id)
                    {
                        attr.author_id = new_hash.clone();
                    }
                }

                // Replace author_ids in line_attributions
                for line_attr in &mut entry.line_attributions {
                    if line_attr.author_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(&line_attr.author_id)
                    {
                        line_attr.author_id = new_hash.clone();
                    }
                    // Also migrate the overrode field if it contains a 7-char hash
                    if let Some(ref overrode_id) = line_attr.overrode
                        && overrode_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(overrode_id)
                    {
                        line_attr.overrode = Some(new_hash.clone());
                    }
                }
            }
            migrated_checkpoints.push(checkpoint);
        }

        Ok(migrated_checkpoints)
    }

    fn checkpoints_file_size_limit_bytes() -> u64 {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var(TEST_CHECKPOINTS_JSONL_MAX_BYTES_ENV)
            && let Ok(value) = raw.parse::<u64>()
            && value > 0
        {
            return value;
        }

        MAX_CHECKPOINTS_JSONL_BYTES
    }

    fn truncate_oversized_checkpoints_file(&self, max_bytes: u64) -> Result<bool, GitAiError> {
        let checkpoints_file = self.checkpoints_file();
        let metadata = match fs::metadata(&checkpoints_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let size_bytes = metadata.len();
        if size_bytes <= max_bytes {
            return Ok(false);
        }

        let message = format!(
            "checkpoints.jsonl exceeded maximum size: {} bytes > {} bytes; deleting and recreating {}",
            size_bytes,
            max_bytes,
            checkpoints_file.display()
        );
        tracing::error!(
            base_commit = %self.base_commit,
            path = %checkpoints_file.display(),
            size_bytes,
            max_bytes,
            "checkpoints.jsonl exceeded maximum size; deleting and recreating empty file"
        );
        crate::observability::log_error(
            &GitAiError::Generic(message),
            Some(serde_json::json!({
                "event": "checkpoints_jsonl_oversized_reset",
                "base_commit": self.base_commit,
                "path": checkpoints_file.to_string_lossy(),
                "size_bytes": size_bytes,
                "max_bytes": max_bytes,
            })),
        );

        match fs::remove_file(&checkpoints_file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::File::create(&checkpoints_file)?;
        Ok(true)
    }

    /// Remove char-level attributions from all but the most recent checkpoint per file.
    /// This reduces storage size while preserving precision for the entries that matter.
    /// Only the most recent checkpoint entry for each file is used when computing new entries.
    fn prune_old_char_attributions(&self, checkpoints: &mut [Checkpoint]) {
        // Track which checkpoint index has the most recent entry for each file
        // Iterate from newest to oldest
        let mut newest_for_file: HashMap<String, usize> = HashMap::new();

        for (checkpoint_idx, checkpoint) in checkpoints.iter().enumerate().rev() {
            for entry in &checkpoint.entries {
                newest_for_file
                    .entry(entry.file.clone())
                    .or_insert(checkpoint_idx);
            }
        }

        // Clear attributions from entries that aren't the most recent for their file
        for (checkpoint_idx, checkpoint) in checkpoints.iter_mut().enumerate() {
            for entry in &mut checkpoint.entries {
                if let Some(&newest_idx) = newest_for_file.get(&entry.file)
                    && checkpoint_idx != newest_idx
                {
                    entry.attributions.clear();
                }
            }
        }
    }

    /// Write all checkpoints to the JSONL file, replacing any existing content
    /// Note: Unlike append_checkpoint(), this preserves transcripts because it's used
    /// by post-commit after transcripts have been refetched and need to be preserved
    /// for from_just_working_log() to read them.
    pub fn write_all_checkpoints(&self, checkpoints: &[Checkpoint]) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.write_all_checkpoints.begin", &self.dir, || {
            format!("count={}", checkpoints.len())
        });
        let checkpoints_file = self.checkpoints_file();
        let mut output = BufWriter::new(fs::File::create(&checkpoints_file)?);

        for checkpoint in checkpoints {
            serde_json::to_writer(&mut output, checkpoint)?;
            output.write_all(b"\n")?;
        }

        output.flush()?;
        crate::wltrace::wltrace(
            "working_log.write_all_checkpoints.end",
            &self.dir,
            String::new,
        );
        Ok(())
    }

    pub fn mutate_all_checkpoints<F>(&self, mutator: F) -> Result<Vec<Checkpoint>, GitAiError>
    where
        F: FnOnce(&mut Vec<Checkpoint>) -> Result<(), GitAiError>,
    {
        let mut checkpoints = self.read_all_checkpoints()?;
        mutator(&mut checkpoints)?;
        self.write_all_checkpoints(&checkpoints)?;
        Ok(checkpoints)
    }

    pub fn all_touched_files(&self) -> Result<HashSet<String>, GitAiError> {
        let checkpoints = self.read_all_checkpoints()?;
        let mut touched_files = self
            .read_initial_attributions()
            .files
            .into_keys()
            .collect::<HashSet<_>>();
        for checkpoint in checkpoints {
            for entry in checkpoint.entries {
                touched_files.insert(entry.file);
            }
        }
        Ok(touched_files)
    }

    pub fn observed_file_snapshot(&self) -> Result<HashMap<String, String>, GitAiError> {
        let initial = self.read_initial_attributions();
        let mut snapshot = HashMap::new();

        for file_path in initial.files.keys() {
            let content = self
                .stored_initial_file_content_from(&initial, file_path)
                .ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "INITIAL missing persisted file snapshot for {}",
                        file_path
                    ))
                })?;
            snapshot.insert(file_path.clone(), content);
        }

        for checkpoint in self.read_all_checkpoints()? {
            for entry in checkpoint.entries {
                let content = self.get_file_version(&entry.blob_sha)?;
                snapshot.insert(entry.file, content);
            }
        }

        Ok(snapshot)
    }

    #[allow(dead_code)]
    pub fn all_ai_touched_files(&self) -> Result<HashSet<String>, GitAiError> {
        let checkpoints = self.read_all_checkpoints()?;
        let mut touched_files = HashSet::new();
        for checkpoint in checkpoints {
            // Only include files from AI checkpoints (AiAgent or AiTab)
            match checkpoint.kind {
                CheckpointKind::AiAgent | CheckpointKind::AiTab => {
                    for entry in checkpoint.entries {
                        touched_files.insert(entry.file);
                    }
                }
                CheckpointKind::Human | CheckpointKind::KnownHuman => {
                    // Skip human checkpoints
                }
            }
        }
        Ok(touched_files)
    }

    /* INITIAL attributions file */

    /// Persist INITIAL attributions plus exact file snapshots for the target working log.
    pub fn write_initial_attributions_with_contents(
        &self,
        attributions: HashMap<String, Vec<LineAttribution>>,
        prompts: HashMap<String, PromptRecord>,
        humans: std::collections::BTreeMap<String, HumanRecord>,
        file_contents: HashMap<String, String>,
        sessions: std::collections::BTreeMap<String, SessionRecord>,
    ) -> Result<(), GitAiError> {
        let filtered: HashMap<String, Vec<LineAttribution>> = attributions
            .into_iter()
            .filter(|(_, attrs)| !attrs.is_empty())
            .collect();
        let mut file_blobs = HashMap::new();
        for file_path in filtered.keys() {
            let content = file_contents.get(file_path).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "INITIAL missing file content snapshot for {}",
                    file_path
                ))
            })?;
            let blob_sha = self.persist_file_version(content)?;
            file_blobs.insert(file_path.clone(), blob_sha);
        }

        self.write_initial(InitialAttributions {
            files: filtered,
            prompts,
            file_blobs,
            humans,
            sessions,
        })
    }

    /// Write a fully-formed INITIAL state, preserving any persisted blob references.
    pub fn write_initial(&self, initial: InitialAttributions) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.write_initial", &self.dir, String::new);
        let filtered_files: HashMap<String, Vec<LineAttribution>> = initial
            .files
            .into_iter()
            .filter(|(_, attrs)| !attrs.is_empty())
            .collect();

        if filtered_files.is_empty() {
            if self.initial_file.exists() {
                fs::remove_file(&self.initial_file)?;
            }
            return Ok(());
        }

        let mut file_blobs = initial.file_blobs;
        file_blobs.retain(|file_path, _| filtered_files.contains_key(file_path));

        let initial_data = InitialAttributions {
            files: filtered_files,
            prompts: initial.prompts,
            file_blobs,
            humans: initial.humans,
            sessions: initial.sessions,
        };

        let json = serde_json::to_string_pretty(&initial_data)?;
        fs::write(&self.initial_file, json)?;

        Ok(())
    }

    pub fn initial_file_content_from(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(content) = self.stored_initial_file_content_from(initial, file_path) {
            return Ok(Some(content));
        }
        if initial.files.contains_key(file_path) {
            return Err(GitAiError::Generic(format!(
                "INITIAL missing persisted file snapshot for {}",
                file_path
            )));
        }
        Ok(None)
    }

    pub fn stored_initial_file_content_from(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Option<String> {
        if let Some(blob_sha) = initial.file_blobs.get(file_path) {
            return self.get_file_version(blob_sha).ok();
        }
        None
    }

    pub fn latest_checkpoint_file_content(&self, file_path: &str) -> Option<String> {
        let checkpoints = self.read_all_checkpoints().ok()?;
        let entry = checkpoints.iter().rev().find_map(|checkpoint| {
            checkpoint
                .entries
                .iter()
                .find(|entry| entry.file == file_path)
        })?;
        self.get_file_version(&entry.blob_sha).ok()
    }

    pub fn effective_tracked_file_content(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(content) = self.latest_checkpoint_file_content(file_path) {
            return Ok(Some(content));
        }
        self.initial_file_content_from(initial, file_path)
    }

    /// Read initial attributions from the INITIAL file.
    /// Returns empty attributions and prompts if the file doesn't exist.
    pub fn read_initial_attributions(&self) -> InitialAttributions {
        if !self.initial_file.exists() {
            return InitialAttributions::default();
        }

        match fs::read_to_string(&self.initial_file) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(initial_data) => initial_data,
                Err(e) => {
                    tracing::debug!("Failed to parse INITIAL file: {}. Returning empty.", e);
                    InitialAttributions::default()
                }
            },
            Err(e) => {
                tracing::debug!("Failed to read INITIAL file: {}. Returning empty.", e);
                InitialAttributions::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn attr(author: &str) -> Vec<LineAttribution> {
        vec![LineAttribution::new(1, 1, author.to_string(), None)]
    }

    /// Regression (#9): merge_working_log_dirs (via rename_working_log when the
    /// destination already exists) must preserve OLD-base INITIAL entries on a
    /// shared key, per the documented "preserve the old-base entries first".
    /// The old code used HashMap::extend(new), so `new` clobbered `old` for any
    /// shared path. Each side's unique entries must also survive.
    #[test]
    fn test_merge_working_log_dirs_old_base_wins_on_conflict() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let ai_dir = tmp.path().join("ai");
        let storage = RepoStorage::for_repo_path(&ai_dir, &workdir).unwrap();

        let old_sha = "1111111111111111111111111111111111111111";
        let new_sha = "2222222222222222222222222222222222222222";

        // OLD base: shared.txt -> old author, plus a unique old-only file.
        let old_log = storage.working_log_for_base_commit(old_sha).unwrap();
        let mut old_initial = InitialAttributions::default();
        old_initial.files.insert("shared.txt".into(), attr("h_OLD"));
        old_initial
            .files
            .insert("old_only.txt".into(), attr("h_OLD"));
        old_initial
            .file_blobs
            .insert("shared.txt".into(), "OLD CONTENT".into());
        old_log.write_initial(old_initial).unwrap();
        old_log.block_edge_recovery().unwrap();

        // NEW base: shared.txt -> new author (conflict), plus a unique new-only file.
        let new_log = storage.working_log_for_base_commit(new_sha).unwrap();
        let mut new_initial = InitialAttributions::default();
        new_initial
            .files
            .insert("shared.txt".into(), attr("ai_NEW"));
        new_initial
            .files
            .insert("new_only.txt".into(), attr("ai_NEW"));
        new_initial
            .file_blobs
            .insert("shared.txt".into(), "NEW CONTENT".into());
        new_log.write_initial(new_initial).unwrap();

        // Merge old into new (destination already exists).
        storage.rename_working_log(old_sha, new_sha).unwrap();

        let merged = storage
            .working_log_for_base_commit(new_sha)
            .unwrap()
            .read_initial_attributions();

        // Shared key: OLD base wins.
        assert_eq!(
            merged
                .files
                .get("shared.txt")
                .map(|a| a[0].author_id.as_str()),
            Some("h_OLD"),
            "old-base attribution must win on a shared path"
        );
        assert_eq!(
            merged.file_blobs.get("shared.txt").map(|s| s.as_str()),
            Some("OLD CONTENT"),
            "old-base blob must win on a shared path (kept consistent with files)"
        );
        // Both sides' unique entries survive.
        assert!(merged.files.contains_key("old_only.txt"));
        assert!(merged.files.contains_key("new_only.txt"));
        assert!(
            storage
                .working_log_for_base_commit(new_sha)
                .unwrap()
                .edge_recovery_blocked(),
            "merged working logs must preserve the source recovery boundary"
        );
        assert_eq!(
            storage
                .working_log_for_base_commit(new_sha)
                .unwrap()
                .edge_recovery_blocked_paths(),
            HashSet::from(["shared.txt".to_string(), "old_only.txt".to_string()]),
            "the boundary must remain scoped to paths carried from the blocked source log"
        );
    }

    #[test]
    fn test_rename_working_log_to_same_sha_preserves_log() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let ai_dir = tmp.path().join("ai");
        let storage = RepoStorage::for_repo_path(&ai_dir, &workdir).unwrap();
        let sha = "1111111111111111111111111111111111111111";

        let log = storage.working_log_for_base_commit(sha).unwrap();
        let mut initial = InitialAttributions::default();
        initial
            .files
            .insert("pending.txt".into(), attr("ai_PENDING"));
        log.write_initial(initial).unwrap();

        storage.rename_working_log(sha, sha).unwrap();

        let preserved = storage
            .working_log_for_base_commit(sha)
            .unwrap()
            .read_initial_attributions();
        assert_eq!(
            preserved
                .files
                .get("pending.txt")
                .map(|attrs| attrs[0].author_id.as_str()),
            Some("ai_PENDING")
        );
    }
}
