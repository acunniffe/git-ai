use crate::authorship::attribution_tracker::{
    Attribution, LineAttribution, attributions_to_line_attributions,
    line_attributions_to_attributions,
};
use crate::authorship::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::authorship::hunk_shift::{DiffHunk, apply_hunk_shifts_to_line_attributions};
use crate::authorship::imara_diff_utils::{
    content_eq_ignoring_line_endings, normalize_line_endings,
};
use crate::authorship::working_log::CheckpointKind;
use crate::commands::blame::{GitAiBlameOptions, OLDEST_AI_BLAME_DATE};
use crate::error::GitAiError;
use crate::git::repository::{Repository, batch_read_paths_at_treeishes};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_normalization::UnicodeNormalization;

pub struct VirtualAttributions {
    repo: Repository,
    base_commit: String,
    // Maps file path -> (char attributions, line attributions)
    pub attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>,
    // Maps file path -> file content
    file_contents: HashMap<String, String>,
    // Prompt records mapping prompt_id -> (commit_sha -> PromptRecord)
    // Same prompt can appear in multiple commits, allowing us to track and sort them
    pub prompts: BTreeMap<String, BTreeMap<String, PromptRecord>>,
    // Timestamp to use for attributions
    ts: u128,
    pub blame_start_commit: Option<String>,
    pub humans: BTreeMap<String, HumanRecord>,
    // Prompt IDs that came from INITIAL attributions only (no matching checkpoint).
    // These are stale prompts from prior commits and should only appear in the
    // authorship note if they have committed lines in the current commit.
    initial_only_prompt_ids: HashSet<String>,
    pub sessions: BTreeMap<String, SessionRecord>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct AuthorshipLogDiffContext<'a> {
    pub precomputed_parent_diff: Option<&'a crate::authorship::rewrite::DiffTreeResult>,
    pub fallback_committed_diff_base: Option<&'a str>,
}

impl VirtualAttributions {
    /// Create a new VirtualAttributions for the given base commit with initial pathspecs
    pub async fn new_for_base_commit(
        repo: Repository,
        base_commit: String,
        pathspecs: &[String],
        blame_start_commit: Option<String>,
    ) -> Result<Self, GitAiError> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        let mut virtual_attrs = VirtualAttributions {
            repo,
            base_commit,
            attributions: HashMap::new(),
            file_contents: HashMap::new(),
            prompts: BTreeMap::new(),
            ts,
            blame_start_commit,
            humans: BTreeMap::new(),
            initial_only_prompt_ids: HashSet::new(),
            sessions: BTreeMap::new(),
        };

        // Process all pathspecs concurrently
        if !pathspecs.is_empty() {
            virtual_attrs.add_pathspecs_concurrent(pathspecs).await?;
        }

        // After running blame, discover and load any missing prompts from blamed commits
        virtual_attrs.discover_and_load_foreign_prompts().await?;

        Ok(virtual_attrs)
    }

    /// Discover and load prompts/sessions from blamed commits that aren't in our maps
    async fn discover_and_load_foreign_prompts(&mut self) -> Result<(), GitAiError> {
        use std::collections::HashSet;

        // Collect all unique author_ids from attributions
        let mut all_author_ids: HashSet<String> = HashSet::new();
        for (char_attrs, _line_attrs) in self.attributions.values() {
            for attr in char_attrs {
                all_author_ids.insert(attr.author_id.clone());
            }
        }

        // Separate session IDs from prompt/human IDs
        let mut missing_session_ids: HashSet<String> = HashSet::new();
        let mut missing_prompt_ids: Vec<String> = Vec::new();

        for id in all_author_ids {
            if id.starts_with("s_") {
                let session_key = id.split("::").next().unwrap_or(&id).to_string();
                if !self.sessions.contains_key(&session_key) {
                    missing_session_ids.insert(session_key);
                }
            } else if !self.prompts.contains_key(&id) && !self.humans.contains_key(&id) {
                missing_prompt_ids.push(id);
            }
        }

        // Load missing prompts in parallel
        if !missing_prompt_ids.is_empty() {
            let prompts = self.load_prompts_concurrent(&missing_prompt_ids).await?;
            for (id, commit_sha, prompt) in prompts {
                self.prompts
                    .entry(id)
                    .or_default()
                    .insert(commit_sha, prompt);
            }
        }

        // Load missing sessions from history
        if !missing_session_ids.is_empty() {
            let sessions = self
                .load_sessions_concurrent(&missing_session_ids.into_iter().collect::<Vec<_>>())
                .await?;
            for (session_id, session_record) in sessions {
                self.sessions.entry(session_id).or_insert(session_record);
            }
        }

        Ok(())
    }

    /// Load multiple prompts concurrently using MAX_CONCURRENT limit
    async fn load_prompts_concurrent(
        &self,
        missing_ids: &[String],
    ) -> Result<Vec<(String, String, PromptRecord)>, GitAiError> {
        const MAX_CONCURRENT: usize = 30;

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let mut tasks = Vec::new();

        for missing_id in missing_ids {
            let missing_id = missing_id.clone();
            let repo = self.repo.clone();
            let semaphore = Arc::clone(&semaphore);

            let task = async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("prompt lookup semaphore was closed");

                crate::tokio_runtime::spawn_blocking_result(move || {
                    Self::find_prompt_in_history_static(&repo, &missing_id)
                        .map(|(commit_sha, prompt)| (missing_id, commit_sha, prompt))
                })
                .await
            };

            tasks.push(task);
        }

        // Await all tasks concurrently
        let results = futures::future::join_all(tasks).await;

        // Process results and collect successful prompts
        let mut prompts = Vec::new();
        for result in results {
            match result {
                Ok((id, commit_sha, prompt)) => prompts.push((id, commit_sha, prompt)),
                Err(_) => {
                    // Error finding prompt, skip it
                }
            }
        }

        Ok(prompts)
    }

    /// Static version of find_prompt_in_history for use in async context
    /// Returns (commit_sha, PromptRecord) for the most recent commit containing this prompt
    fn find_prompt_in_history_static(
        repo: &Repository,
        prompt_id: &str,
    ) -> Result<(String, crate::authorship::authorship_log::PromptRecord), GitAiError> {
        // Use git grep to search for the prompt ID in authorship notes
        let shas = crate::git::notes_api::search_notes(repo, &format!("\"{}\"", prompt_id))
            .unwrap_or_default();

        // Check the most recent commit with this prompt ID
        if let Some(latest_sha) = shas.first()
            && let Ok(log) = crate::git::notes_api::read_authorship_v3(repo, latest_sha)
            && let Some(prompt) = log.metadata.prompts.get(prompt_id)
        {
            return Ok((latest_sha.clone(), prompt.clone()));
        }

        Err(GitAiError::Generic(format!(
            "Prompt not found in history: {}",
            prompt_id
        )))
    }

    /// Load multiple sessions concurrently from git note history
    async fn load_sessions_concurrent(
        &self,
        missing_ids: &[String],
    ) -> Result<Vec<(String, SessionRecord)>, GitAiError> {
        const MAX_CONCURRENT: usize = 30;

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let mut tasks = Vec::new();

        for missing_id in missing_ids {
            let missing_id = missing_id.clone();
            let repo = self.repo.clone();
            let semaphore = Arc::clone(&semaphore);

            let task = async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("session lookup semaphore was closed");
                crate::tokio_runtime::spawn_blocking_result(move || {
                    Self::find_session_in_history_static(&repo, &missing_id)
                        .map(|record| (missing_id, record))
                })
                .await
            };

            tasks.push(task);
        }

        let results = futures::future::join_all(tasks).await;
        let sessions: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
        Ok(sessions)
    }

    fn find_session_in_history_static(
        repo: &Repository,
        session_id: &str,
    ) -> Result<SessionRecord, GitAiError> {
        // Go through notes_api (not refs::) so the HTTP notes backend is honored.
        let shas = crate::git::notes_api::search_notes(repo, &format!("\"{}\"", session_id))
            .unwrap_or_default();

        if let Some(latest_sha) = shas.first()
            && let Ok(log) = crate::git::notes_api::read_authorship_v3(repo, latest_sha)
            && let Some(session) = log.metadata.sessions.get(session_id)
        {
            return Ok(session.clone());
        }

        Err(GitAiError::Generic(format!(
            "Session not found in history: {}",
            session_id
        )))
    }

    /// Add a single pathspec to the virtual attributions
    #[allow(dead_code)]
    pub async fn add_pathspec(&mut self, pathspec: &str) -> Result<(), GitAiError> {
        self.add_pathspecs_concurrent(&[pathspec.to_string()]).await
    }

    /// Add multiple pathspecs concurrently
    async fn add_pathspecs_concurrent(&mut self, pathspecs: &[String]) -> Result<(), GitAiError> {
        const MAX_CONCURRENT: usize = 30;

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));
        let mut tasks = Vec::new();

        for pathspec in pathspecs {
            let pathspec = pathspec.clone();
            let repo = self.repo.clone();
            let base_commit = self.base_commit.clone();
            let ts = self.ts;
            let blame_start_commit = self.blame_start_commit.clone();
            let semaphore = Arc::clone(&semaphore);

            let task = async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("virtual attribution semaphore was closed");

                crate::tokio_runtime::spawn_blocking_result(move || {
                    compute_attributions_for_file(
                        &repo,
                        &base_commit,
                        &pathspec,
                        ts,
                        blame_start_commit,
                    )
                })
                .await
            };

            tasks.push(task);
        }

        // Await all tasks
        let results = futures::future::join_all(tasks).await;

        // Process results and store in HashMap
        for result in results {
            match result {
                Ok(Some((file_path, content, char_attrs, line_attrs))) => {
                    self.attributions
                        .insert(file_path.clone(), (char_attrs, line_attrs));
                    self.file_contents.insert(file_path, content);
                }
                Ok(None) => {
                    // File had no changes or couldn't be processed, skip
                }
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Get both character and line attributions for a file
    #[allow(dead_code)]
    pub fn get_attributions(
        &self,
        file_path: &str,
    ) -> Option<&(Vec<Attribution>, Vec<LineAttribution>)> {
        self.attributions.get(file_path)
    }

    /// Get just character-level attributions for a file
    pub fn get_char_attributions(&self, file_path: &str) -> Option<&Vec<Attribution>> {
        self.attributions
            .get(file_path)
            .map(|(char_attrs, _)| char_attrs)
    }

    /// Get just line-level attributions for a file
    pub fn get_line_attributions(&self, file_path: &str) -> Option<&Vec<LineAttribution>> {
        self.attributions
            .get(file_path)
            .map(|(_, line_attrs)| line_attrs)
    }

    /// List all tracked files
    pub fn files(&self) -> Vec<String> {
        self.attributions.keys().cloned().collect()
    }

    /// Get the base commit SHA
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// Get the timestamp used for attributions
    pub fn timestamp(&self) -> u128 {
        self.ts
    }

    /// Get the prompts metadata (prompt_id -> commit_sha -> PromptRecord)
    pub fn prompts(&self) -> &BTreeMap<String, BTreeMap<String, PromptRecord>> {
        &self.prompts
    }

    /// Get the file content for a tracked file
    pub fn get_file_content(&self, file_path: &str) -> Option<&String> {
        self.file_contents.get(file_path)
    }

    /// Get a reference to the repository
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Create VirtualAttributions from just the working log (no blame)
    ///
    /// This is a fast path that skips the expensive blame operation.
    /// Use this when you only care about working log data and don't need historical blame.
    ///
    /// This function:
    /// 1. Loads INITIAL attributions (unstaged AI code from previous working state)
    /// 2. Applies working log checkpoints on top
    /// 3. Returns VirtualAttributions with just the working log data
    pub fn from_just_working_log(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
    ) -> Result<Self, GitAiError> {
        let working_log = repo.storage.working_log_for_base_commit(&base_commit)?;
        let initial_attributions = working_log.read_initial_attributions();
        let checkpoints = working_log.read_all_checkpoints().unwrap_or_default();

        let mut attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)> =
            HashMap::new();
        let mut prompts = BTreeMap::new();
        let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        let mut file_contents: HashMap<String, String> = HashMap::new();
        // Prompt IDs that originate from INITIAL attributions (prior commits).
        // If a checkpoint later references the same prompt_id, it is removed from
        // this set because the prompt was actively used in this commit's session.
        let mut initial_only_prompt_ids: HashSet<String> = HashSet::new();
        let mut sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();

        // Track additions and deletions per session_id for metrics
        let mut session_additions: HashMap<String, u32> = HashMap::new();
        let mut session_deletions: HashMap<String, u32> = HashMap::new();

        // Add prompts from INITIAL attributions
        // These are uncommitted prompts, so we use an empty string as the commit_sha
        for (prompt_id, prompt_record) in &initial_attributions.prompts {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(BTreeMap::new)
                .insert(String::new(), prompt_record.clone());
            initial_only_prompt_ids.insert(prompt_id.clone());
        }

        // Load known human records from INITIAL attributions
        for (hash, human_record) in &initial_attributions.humans {
            humans
                .entry(hash.clone())
                .or_insert_with(|| human_record.clone());
        }

        // Load session records from INITIAL attributions
        for (session_id, session_record) in &initial_attributions.sessions {
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| session_record.clone());
        }

        // Process INITIAL attributions
        for (file_path, line_attrs) in &initial_attributions.files {
            // Get the latest file content from working directory
            if let Ok(workdir) = repo.workdir() {
                let abs_path = workdir.join(file_path);
                let file_content = if abs_path.exists() {
                    std::fs::read_to_string(&abs_path).unwrap_or_default()
                } else {
                    String::new()
                };
                file_contents.insert(file_path.clone(), file_content.clone());

                // Convert line attributions to character attributions
                let char_attrs = line_attributions_to_attributions(line_attrs, &file_content, 0);
                attributions.insert(file_path.clone(), (char_attrs, line_attrs.clone()));
            }
        }

        // Collect attributions from all checkpoints (later checkpoints override earlier ones)
        for checkpoint in &checkpoints {
            // Add prompts or sessions from checkpoint
            if let Some(agent_id) = &checkpoint.agent_id {
                let is_session_format = checkpoint.trace_id.is_some();

                if is_session_format {
                    // New format: derive session_id from this checkpoint's own agent_id
                    let session_id =
                        crate::authorship::authorship_log_serialization::generate_session_id(
                            &agent_id.id,
                            &agent_id.tool,
                        );

                    let session_record = SessionRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),
                        custom_attributes: None,
                    };

                    sessions.insert(session_id.clone(), session_record);

                    // Track additions/deletions keyed by session_id
                    *session_additions.entry(session_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(session_id).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                } else {
                    // Old format: use existing prompts logic
                    let author_id =
                        crate::authorship::authorship_log_serialization::generate_short_hash(
                            &agent_id.id,
                            &agent_id.tool,
                        );
                    // For working log checkpoints, use empty string as commit_sha since they're uncommitted
                    // Always overwrite with the latest checkpoint for this agent so refreshed
                    // transcripts/models from post-commit aren't lost.
                    let prompt_record = crate::authorship::authorship_log::PromptRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),
                        total_additions: 0,
                        total_deletions: 0,
                        accepted_lines: 0,
                        overriden_lines: 0,
                        custom_attributes: None,
                        messages_url: None,
                    };

                    prompts
                        .entry(author_id.clone())
                        .or_insert_with(BTreeMap::new)
                        .insert(String::new(), prompt_record);
                    // This prompt was actively used in a checkpoint, so it's not
                    // INITIAL-only (even if it was also in INITIAL).
                    initial_only_prompt_ids.remove(&author_id);

                    // Track additions and deletions from checkpoint line_stats
                    *session_additions.entry(author_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(author_id).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                }
            }

            if checkpoint.kind == CheckpointKind::KnownHuman {
                let hash =
                    crate::authorship::authorship_log_serialization::generate_human_short_hash(
                        &checkpoint.author,
                    );
                humans.entry(hash).or_insert_with(|| HumanRecord {
                    author: checkpoint.author.clone(),
                });
            }

            // Collect attributions from checkpoint entries
            for entry in &checkpoint.entries {
                // Most human-only pre-commit entries carry no attribution data and can be skipped.
                // This keeps post-commit work proportional to AI-relevant files.
                if entry.line_attributions.is_empty() && entry.attributions.is_empty() {
                    continue;
                }

                // Get the latest file content from working directory
                if let Ok(workdir) = repo.workdir() {
                    let abs_path = workdir.join(&entry.file);
                    let file_content = if abs_path.exists() {
                        std::fs::read_to_string(&abs_path).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    file_contents.insert(entry.file.clone(), file_content);
                }

                // Prefer persisted line attributions. Fall back to converting char attributions
                // for compatibility with older checkpoint data.
                let file_content = file_contents.get(&entry.file).cloned().unwrap_or_default();
                let line_attrs = if entry.line_attributions.is_empty() {
                    crate::authorship::attribution_tracker::attributions_to_line_attributions(
                        &entry.attributions,
                        &file_content,
                    )
                } else {
                    entry.line_attributions.clone()
                };

                if line_attrs.is_empty() {
                    // The entry had attribution data but no AI lines remain after
                    // filtering (e.g. human rewrote the entire file).  Clear any
                    // stale AI attributions from earlier checkpoints for this file.
                    attributions.remove(&entry.file);
                    continue;
                }

                let char_attrs = line_attributions_to_attributions(&line_attrs, &file_content, 0);

                attributions.insert(entry.file.clone(), (char_attrs, line_attrs));
            }
        }

        // Calculate final metrics for each prompt
        Self::calculate_and_update_prompt_metrics(
            &mut prompts,
            &attributions,
            &session_additions,
            &session_deletions,
        );

        Ok(VirtualAttributions {
            repo,
            base_commit,
            attributions,
            file_contents,
            prompts,
            ts: 0,
            blame_start_commit: None,
            humans,
            initial_only_prompt_ids,
            sessions,
        })
    }

    /// Create VirtualAttributions from working-log state using an exact captured snapshot
    /// instead of the live worktree.
    pub fn from_working_log_snapshot(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
        final_state_snapshot: &HashMap<String, String>,
    ) -> Result<Self, GitAiError> {
        let working_log = repo.storage.working_log_for_base_commit(&base_commit)?;
        let initial_attributions = working_log.read_initial_attributions();
        let checkpoints = working_log.read_all_checkpoints().unwrap_or_default();

        let mut attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)> =
            HashMap::new();
        let mut prompts = BTreeMap::new();
        let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        let mut file_contents: HashMap<String, String> = HashMap::new();
        let mut initial_only_prompt_ids: HashSet<String> = HashSet::new();
        let mut sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();

        let mut session_additions: HashMap<String, u32> = HashMap::new();
        let mut session_deletions: HashMap<String, u32> = HashMap::new();

        for (prompt_id, prompt_record) in &initial_attributions.prompts {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(BTreeMap::new)
                .insert(String::new(), prompt_record.clone());
            initial_only_prompt_ids.insert(prompt_id.clone());
        }

        // Load known human records from INITIAL attributions
        for (hash, human_record) in &initial_attributions.humans {
            humans
                .entry(hash.clone())
                .or_insert_with(|| human_record.clone());
        }

        // Load session records from INITIAL attributions
        for (session_id, session_record) in &initial_attributions.sessions {
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| session_record.clone());
        }

        for (file_path, line_attrs) in &initial_attributions.files {
            // Use stored content for INITIAL since line_attrs reference that file version.
            // Fall back to final_state_snapshot only if no stored content exists.
            let file_content = working_log
                .stored_initial_file_content_from(&initial_attributions, file_path)
                .or_else(|| final_state_snapshot.get(file_path).cloned())
                .unwrap_or_default();
            file_contents.insert(file_path.clone(), file_content.clone());

            let char_attrs = line_attributions_to_attributions(line_attrs, &file_content, 0);
            attributions.insert(file_path.clone(), (char_attrs, line_attrs.clone()));
        }

        for checkpoint in &checkpoints {
            if let Some(agent_id) = &checkpoint.agent_id {
                let is_session_format = checkpoint.trace_id.is_some();

                if is_session_format {
                    // New format: derive session_id from this checkpoint's own agent_id
                    let session_id =
                        crate::authorship::authorship_log_serialization::generate_session_id(
                            &agent_id.id,
                            &agent_id.tool,
                        );

                    let session_record = SessionRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),
                        custom_attributes: None,
                    };

                    sessions.insert(session_id.clone(), session_record);

                    // Track additions/deletions keyed by session_id
                    *session_additions.entry(session_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(session_id).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                } else {
                    // Old format: use existing prompts logic
                    let author_id =
                        crate::authorship::authorship_log_serialization::generate_short_hash(
                            &agent_id.id,
                            &agent_id.tool,
                        );
                    let prompt_record = crate::authorship::authorship_log::PromptRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),

                        total_additions: 0,
                        total_deletions: 0,
                        accepted_lines: 0,
                        overriden_lines: 0,

                        custom_attributes: None,
                        messages_url: None,
                    };

                    prompts
                        .entry(author_id.clone())
                        .or_insert_with(BTreeMap::new)
                        .insert(String::new(), prompt_record);
                    initial_only_prompt_ids.remove(&author_id);

                    *session_additions.entry(author_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(author_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                }
            }

            if checkpoint.kind == CheckpointKind::KnownHuman {
                let hash =
                    crate::authorship::authorship_log_serialization::generate_human_short_hash(
                        &checkpoint.author,
                    );
                humans.entry(hash).or_insert_with(|| HumanRecord {
                    author: checkpoint.author.clone(),
                });
            }

            for entry in &checkpoint.entries {
                if entry.line_attributions.is_empty() && entry.attributions.is_empty() {
                    continue;
                }

                let file_content = final_state_snapshot
                    .get(&entry.file)
                    .cloned()
                    .unwrap_or_else(|| {
                        working_log
                            .get_file_version(&entry.blob_sha)
                            .unwrap_or_default()
                    });
                file_contents.insert(entry.file.clone(), file_content.clone());

                let line_attrs = if entry.line_attributions.is_empty() {
                    crate::authorship::attribution_tracker::attributions_to_line_attributions(
                        &entry.attributions,
                        &file_content,
                    )
                } else {
                    entry.line_attributions.clone()
                };

                if line_attrs.is_empty() {
                    // The entry had attribution data but no AI lines remain after
                    // filtering (e.g. human rewrote the entire file).  Clear any
                    // stale AI attributions from earlier checkpoints for this file.
                    attributions.remove(&entry.file);
                    continue;
                }

                let char_attrs = line_attributions_to_attributions(&line_attrs, &file_content, 0);
                attributions.insert(entry.file.clone(), (char_attrs, line_attrs));
            }
        }

        Self::calculate_and_update_prompt_metrics(
            &mut prompts,
            &attributions,
            &session_additions,
            &session_deletions,
        );

        Ok(VirtualAttributions {
            repo,
            base_commit,
            attributions,
            file_contents,
            prompts,
            ts: 0,
            blame_start_commit: None,
            humans,
            initial_only_prompt_ids,
            sessions,
        })
    }

    /// Create VirtualAttributions from only the persisted working-log state.
    ///
    /// Unlike `from_just_working_log`, this never reads the live worktree. It is intended for
    /// daemon-side async reconstruction where the command's final state has already been captured.
    pub fn from_persisted_working_log(
        repo: Repository,
        base_commit: String,
        human_author: Option<String>,
    ) -> Result<Self, GitAiError> {
        let working_log = repo.storage.working_log_for_base_commit(&base_commit)?;
        let initial_attributions = working_log.read_initial_attributions();
        let checkpoints = working_log.read_all_checkpoints().unwrap_or_default();

        let mut attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)> =
            HashMap::new();
        let mut prompts = BTreeMap::new();
        let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        let mut file_contents: HashMap<String, String> = HashMap::new();
        let mut initial_only_prompt_ids: HashSet<String> = HashSet::new();
        let mut sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();

        let mut session_additions: HashMap<String, u32> = HashMap::new();
        let mut session_deletions: HashMap<String, u32> = HashMap::new();

        for (prompt_id, prompt_record) in &initial_attributions.prompts {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(BTreeMap::new)
                .insert(String::new(), prompt_record.clone());
            initial_only_prompt_ids.insert(prompt_id.clone());
        }

        // Load known human records from INITIAL attributions
        for (hash, human_record) in &initial_attributions.humans {
            humans
                .entry(hash.clone())
                .or_insert_with(|| human_record.clone());
        }

        // Load session records from INITIAL attributions
        for (session_id, session_record) in &initial_attributions.sessions {
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| session_record.clone());
        }

        for (file_path, line_attrs) in &initial_attributions.files {
            let file_content = working_log
                .stored_initial_file_content_from(&initial_attributions, file_path)
                .ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "INITIAL missing persisted file snapshot for {}",
                        file_path
                    ))
                })?;
            file_contents.insert(file_path.clone(), file_content.clone());
            let char_attrs = line_attributions_to_attributions(line_attrs, &file_content, 0);
            attributions.insert(file_path.clone(), (char_attrs, line_attrs.clone()));
        }

        for checkpoint in &checkpoints {
            if let Some(agent_id) = &checkpoint.agent_id {
                let is_session_format = checkpoint.trace_id.is_some();

                if is_session_format {
                    // New format: derive session_id from this checkpoint's own agent_id
                    let session_id =
                        crate::authorship::authorship_log_serialization::generate_session_id(
                            &agent_id.id,
                            &agent_id.tool,
                        );

                    let session_record = SessionRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),
                        custom_attributes: None,
                    };

                    sessions.insert(session_id.clone(), session_record);

                    // Track additions/deletions keyed by session_id
                    *session_additions.entry(session_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(session_id).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                } else {
                    // Old format: use existing prompts logic
                    let author_id =
                        crate::authorship::authorship_log_serialization::generate_short_hash(
                            &agent_id.id,
                            &agent_id.tool,
                        );
                    let prompt_record = crate::authorship::authorship_log::PromptRecord {
                        agent_id: agent_id.clone(),
                        human_author: human_author.clone(),

                        total_additions: 0,
                        total_deletions: 0,
                        accepted_lines: 0,
                        overriden_lines: 0,

                        custom_attributes: None,
                        messages_url: None,
                    };

                    prompts
                        .entry(author_id.clone())
                        .or_insert_with(BTreeMap::new)
                        .insert(String::new(), prompt_record);
                    initial_only_prompt_ids.remove(&author_id);

                    *session_additions.entry(author_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.additions;
                    *session_deletions.entry(author_id.clone()).or_insert(0) +=
                        checkpoint.line_stats.deletions;
                }
            }

            if checkpoint.kind == CheckpointKind::KnownHuman {
                let hash =
                    crate::authorship::authorship_log_serialization::generate_human_short_hash(
                        &checkpoint.author,
                    );
                humans.entry(hash).or_insert_with(|| HumanRecord {
                    author: checkpoint.author.clone(),
                });
            }

            for entry in &checkpoint.entries {
                if entry.line_attributions.is_empty() && entry.attributions.is_empty() {
                    continue;
                }

                let file_content = working_log.get_file_version(&entry.blob_sha)?;
                file_contents.insert(entry.file.clone(), file_content.clone());

                let line_attrs = if entry.line_attributions.is_empty() {
                    attributions_to_line_attributions(&entry.attributions, &file_content)
                } else {
                    entry.line_attributions.clone()
                };
                if line_attrs.is_empty() {
                    // The entry had attribution data but no AI lines remain after
                    // filtering (e.g. human rewrote the entire file).  Clear any
                    // stale AI attributions from earlier checkpoints for this file.
                    attributions.remove(&entry.file);
                    continue;
                }

                let char_attrs = line_attributions_to_attributions(&line_attrs, &file_content, 0);
                attributions.insert(entry.file.clone(), (char_attrs, line_attrs));
            }
        }

        Self::calculate_and_update_prompt_metrics(
            &mut prompts,
            &attributions,
            &session_additions,
            &session_deletions,
        );

        Ok(VirtualAttributions {
            repo,
            base_commit,
            attributions,
            file_contents,
            prompts,
            ts: 0,
            blame_start_commit: None,
            humans,
            initial_only_prompt_ids,
            sessions,
        })
    }

    /// Build amend attributions from the original commit's blame data, persisted
    /// working-log checkpoints, and an explicit final-state snapshot.
    pub async fn from_working_log_for_commit_snapshot(
        repo: Repository,
        base_commit: String,
        pathspecs: &[String],
        human_author: Option<String>,
        blame_start_commit: Option<String>,
        final_state_snapshot: &HashMap<String, String>,
    ) -> Result<Self, GitAiError> {
        let blame_va = Self::new_for_base_commit(
            repo.clone(),
            base_commit.clone(),
            pathspecs,
            blame_start_commit,
        )
        .await?;

        let checkpoint_va =
            Self::from_persisted_working_log(repo.clone(), base_commit.clone(), human_author)?;

        // Save session prompt IDs before the merge consumes checkpoint_va.
        // Exclude INITIAL-only prompts from prior commits.
        let checkpoint_prompt_ids: std::collections::HashSet<String> = checkpoint_va
            .prompts
            .keys()
            .filter(|id| !checkpoint_va.initial_only_prompt_ids.contains(*id))
            .cloned()
            .collect();

        let final_state = final_state_snapshot.clone();
        let mut merged_va =
            merge_attributions_favoring_first(checkpoint_va, blame_va, final_state)?;

        // Mark all non-session prompts (same logic as `from_working_log_for_commit`).
        merged_va.initial_only_prompt_ids = merged_va
            .prompts
            .keys()
            .filter(|id| !checkpoint_prompt_ids.contains(*id))
            .cloned()
            .collect();

        // Prune blame-history prompts whose lines were deleted.  Same logic as
        // `from_working_log_for_commit`.
        let referenced_in_merged: std::collections::HashSet<String> = merged_va
            .attributions
            .values()
            .flat_map(|(_, line_attrs)| line_attrs.iter())
            .map(|la| la.author_id.clone())
            .collect();
        merged_va.prompts.retain(|id, _| {
            checkpoint_prompt_ids.contains(id) || referenced_in_merged.contains(id)
        });
        merged_va
            .humans
            .retain(|id, _| referenced_in_merged.contains(id));
        let referenced_session_ids: std::collections::HashSet<String> = referenced_in_merged
            .iter()
            .filter(|id| id.starts_with("s_"))
            .map(|id| id.split("::").next().unwrap_or(id).to_string())
            .collect();
        merged_va
            .sessions
            .retain(|id, _| referenced_session_ids.contains(id));

        Ok(merged_va)
    }

    /// Create VirtualAttributions from raw components (used for transformations)
    pub fn new(
        repo: Repository,
        base_commit: String,
        attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>,
        file_contents: HashMap<String, String>,
        ts: u128,
    ) -> Self {
        VirtualAttributions {
            repo,
            base_commit,
            attributions,
            file_contents,
            prompts: BTreeMap::new(),
            ts,
            blame_start_commit: None,
            humans: BTreeMap::new(),
            initial_only_prompt_ids: HashSet::new(),
            sessions: BTreeMap::new(),
        }
    }

    pub fn new_with_prompts(
        repo: Repository,
        base_commit: String,
        attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>,
        file_contents: HashMap<String, String>,
        prompts: BTreeMap<String, BTreeMap<String, PromptRecord>>,
        ts: u128,
    ) -> Self {
        VirtualAttributions {
            repo,
            base_commit,
            attributions,
            file_contents,
            prompts,
            ts,
            blame_start_commit: None,
            humans: BTreeMap::new(), // TODO(known-human): propagate humans from caller when rebase path is wired (Task 12)
            initial_only_prompt_ids: HashSet::new(),
            sessions: BTreeMap::new(),
        }
    }

    /// Get sessions map
    pub fn sessions(&self) -> &BTreeMap<String, SessionRecord> {
        &self.sessions
    }

    /// Convert this VirtualAttributions to an AuthorshipLog
    pub fn to_authorship_log(
        &self,
    ) -> Result<crate::authorship::authorship_log_serialization::AuthorshipLog, GitAiError> {
        use crate::authorship::authorship_log_serialization::AuthorshipLog;

        let mut authorship_log = AuthorshipLog::new();
        authorship_log.metadata.base_commit_sha = self.base_commit.clone();
        // Flatten the nested prompts map: take the most recent (first) prompt for each prompt_id
        authorship_log.metadata.prompts = self
            .prompts
            .iter()
            .filter_map(|(prompt_id, commits)| {
                // Get the first (most recent) commit's PromptRecord
                commits
                    .values()
                    .next()
                    .map(|record| (prompt_id.clone(), record.clone()))
            })
            .collect();
        authorship_log.metadata.humans = self.humans.clone();
        authorship_log.metadata.sessions = self.sessions.clone();

        authorship_log.attestations = build_attestations_from_attributions(&self.attributions);

        Ok(authorship_log)
    }
}

/// Build the deterministically-ordered attestation list for an authorship log
/// from the per-file (char, line) attribution map.
///
/// `self.attributions` is a `HashMap`, and entries within a file are grouped by
/// a `HashMap<author_id, ranges>`; iterating either directly yields a
/// process-randomised order, which would make byte-identical commits produce
/// different note bytes (breaking idempotent note sync / dedup). We therefore
/// sort files by path and entries by hash so the output is stable. Ranges
/// within an entry are already sorted+merged.
fn build_attestations_from_attributions(
    attributions: &HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>,
) -> Vec<crate::authorship::authorship_log_serialization::FileAttestation> {
    use crate::authorship::authorship_log_serialization::{AttestationEntry, FileAttestation};

    let mut files: Vec<FileAttestation> = Vec::new();

    for (file_path, (_, line_attrs)) in attributions {
        if line_attrs.is_empty() {
            continue;
        }

        // Group line attributions by author as intervals.
        // This avoids expanding every range to individual line numbers.
        let mut author_ranges: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for line_attr in line_attrs {
            // Skip the legacy "human" sentinel (CheckpointKind::Human checkpoints that were
            // never attested). KnownHuman lines use h_-prefixed author IDs and pass through.
            if line_attr.author_id == CheckpointKind::Human.to_str() {
                continue;
            }

            author_ranges
                .entry(line_attr.author_id.clone())
                .or_default()
                .push((line_attr.start_line, line_attr.end_line));
        }

        // NFC-normalise the path so that attestation file_path is consistent
        // with NFC paths emitted by git diff parsing.
        let nfc_fp: String = file_path.nfc().collect();
        let mut file_attestation = FileAttestation::new(nfc_fp);

        // Create attestation entries for each author.
        for (author_id, mut ranges) in author_ranges {
            if ranges.is_empty() {
                continue;
            }
            ranges.sort_by_key(|(start, end)| (*start, *end));

            let mut merged: Vec<(u32, u32)> = Vec::new();
            for (start, end) in ranges {
                match merged.last_mut() {
                    Some((_, last_end)) if start <= last_end.saturating_add(1) => {
                        *last_end = (*last_end).max(end);
                    }
                    _ => merged.push((start, end)),
                }
            }

            let line_ranges = merged
                .into_iter()
                .map(|(start, end)| {
                    if start == end {
                        LineRange::Single(start)
                    } else {
                        LineRange::Range(start, end)
                    }
                })
                .collect();

            file_attestation.add_entry(AttestationEntry::new(author_id, line_ranges));
        }

        if file_attestation.entries.is_empty() {
            continue;
        }

        // Deterministic entry order within the file: sort by hash (author_id).
        file_attestation.entries.sort_by(|a, b| a.hash.cmp(&b.hash));
        files.push(file_attestation);
    }

    // Deterministic file order: sort by NFC-normalised path.
    files.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    files
}

/// Derive committed (added) line ranges per file from a pre-computed
/// parent→commit `DiffTreeResult`, equivalent to what `collect_committed_hunks`
/// would return for the same pair. The new-side hunk ranges are the lines added
/// by the commit. Filtered by `pathspecs` when provided.
pub(crate) fn committed_hunks_from_diff_result(
    diff: &crate::authorship::rewrite::DiffTreeResult,
    pathspecs: Option<&HashSet<String>>,
) -> HashMap<String, Vec<LineRange>> {
    let mut committed_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    for (file_path, added_lines) in &diff.added_lines_by_file {
        if let Some(paths) = pathspecs
            && !paths.contains(file_path)
        {
            continue;
        }
        let lines = added_lines
            .iter()
            .copied()
            .filter(|line| *line > 0)
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            committed_hunks.insert(file_path.clone(), LineRange::compress_lines(&lines));
        }
    }
    committed_hunks
}

/// Helper function to collect committed line ranges from git diff
fn collect_committed_hunks(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
) -> Result<HashMap<String, Vec<LineRange>>, GitAiError> {
    let mut committed_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    // Handle initial commit (no parent)
    if parent_sha == "initial" {
        // For initial commit, use git diff against the empty tree
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"; // Git's empty tree hash
        let added_lines = repo.diff_added_lines(empty_tree, commit_sha, pathspecs)?;

        for (file_path, lines) in added_lines {
            if !lines.is_empty() {
                committed_hunks.insert(file_path, LineRange::compress_lines(&lines));
            }
        }
        return Ok(committed_hunks);
    }

    // Use git diff to get added lines directly
    let added_lines = repo.diff_added_lines(parent_sha, commit_sha, pathspecs)?;

    for (file_path, lines) in added_lines {
        if !lines.is_empty() {
            committed_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    Ok(committed_hunks)
}

/// Detect file renames between parent and commit. Returns a map of old_path → new_path.
fn detect_renames_in_commit(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
) -> Result<HashMap<String, String>, GitAiError> {
    use crate::git::repository::exec_git_allow_nonzero;

    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "-r".to_string(),
        "-M".to_string(),
        "--diff-filter=R".to_string(),
        parent_sha.to_string(),
        commit_sha.to_string(),
    ]);
    let output = exec_git_allow_nonzero(&args)?;
    let mut renames = HashMap::new();
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // Format: :old_mode new_mode old_hash new_hash Rxx\told_path\tnew_path
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 3 {
                renames.insert(parts[1].to_string(), parts[2].to_string());
            }
        }
    }
    Ok(renames)
}

/// Helper function to collect unstaged line ranges (lines in working directory but not in commit)
/// Returns (unstaged_hunks, pure_insertion_hunks)
/// pure_insertion_hunks contains lines that were purely inserted (old_count=0), not modifications
#[allow(clippy::type_complexity)]
fn collect_unstaged_hunks(
    repo: &Repository,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
) -> Result<
    (
        HashMap<String, Vec<LineRange>>,
        HashMap<String, Vec<LineRange>>,
    ),
    GitAiError,
> {
    let mut unstaged_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    let mut pure_insertion_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    // Use git diff to get added lines in working directory vs commit, with insertion tracking
    let (added_lines, insertion_lines) =
        repo.diff_workdir_added_lines_with_insertions(commit_sha, pathspecs)?;

    for (file_path, lines) in added_lines {
        if !lines.is_empty() {
            unstaged_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    for (file_path, lines) in insertion_lines {
        if !lines.is_empty() {
            pure_insertion_hunks.insert(file_path, LineRange::compress_lines(&lines));
        }
    }

    // Check for untracked files in pathspecs that git diff didn't find
    // These are files that exist in the working directory but aren't tracked by git
    if let Some(paths) = pathspecs
        && let Ok(workdir) = repo.workdir()
    {
        for pathspec in paths {
            // Skip if we already found this file in git diff
            if unstaged_hunks.contains_key(pathspec) {
                continue;
            }

            // Check if file exists in the commit - if it does, it's tracked and git diff should handle it
            // Only process truly untracked files (files that don't exist in the commit tree)
            if file_exists_in_commit(repo, commit_sha, pathspec).unwrap_or(false) {
                continue;
            }

            // Check if file exists in working directory
            let file_path = workdir.join(pathspec);
            if file_path.exists() && file_path.is_file() {
                // Try to read the file
                if let Ok(content) = std::fs::read_to_string(&file_path) {
                    // Count the lines - all lines are "unstaged" since the file is untracked
                    let line_count = content.lines().count() as u32;
                    if line_count > 0 {
                        // Create a range covering all lines (1-indexed)
                        let range = vec![LineRange::Range(1, line_count)];
                        unstaged_hunks.insert(pathspec.clone(), range.clone());
                        // Untracked files are pure insertions (the entire file is new)
                        pure_insertion_hunks.insert(pathspec.clone(), range);
                    }
                }
            }
        }
    }

    Ok((unstaged_hunks, pure_insertion_hunks))
}

#[allow(clippy::type_complexity)]
fn collect_unstaged_hunks_from_snapshot(
    repo: &Repository,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
    final_state_snapshot: &HashMap<String, String>,
) -> Result<
    (
        HashMap<String, Vec<LineRange>>,
        HashMap<String, Vec<LineRange>>,
    ),
    GitAiError,
> {
    let mut unstaged_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();
    let mut pure_insertion_hunks: HashMap<String, Vec<LineRange>> = HashMap::new();

    let file_paths: HashSet<String> = match pathspecs {
        Some(paths) => paths.iter().cloned().collect(),
        None => final_state_snapshot.keys().cloned().collect(),
    };

    // Batch-read committed content for every file in two git spawns instead of
    // one (fast-reader-miss) spawn per file.
    let requests: Vec<(String, String)> = file_paths
        .iter()
        .map(|file_path| (commit_sha.to_string(), file_path.clone()))
        .collect();
    let committed_contents = batch_file_contents(repo, &requests)?;

    for file_path in file_paths {
        let committed_content = committed_contents
            .get(&(commit_sha.to_string(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        let final_content = final_state_snapshot
            .get(&file_path)
            .cloned()
            .unwrap_or_else(|| committed_content.clone());

        if content_eq_ignoring_line_endings(&committed_content, &final_content) {
            continue;
        }

        let normalized_committed = normalize_line_endings(&committed_content);
        let normalized_final = normalize_line_endings(&final_content);
        let committed_lines = split_lines_preserving_terminators(&normalized_committed);
        let final_lines = split_lines_preserving_terminators(&normalized_final);
        let diff_ops = crate::authorship::imara_diff_utils::capture_diff_slices(
            &committed_lines,
            &final_lines,
        );

        let mut all_added_lines = Vec::new();
        let mut pure_insertion_lines = Vec::new();

        for op in diff_ops {
            match op {
                crate::authorship::imara_diff_utils::DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    let start = new_index as u32 + 1;
                    let end = start + new_len as u32;
                    for line in start..end {
                        all_added_lines.push(line);
                        pure_insertion_lines.push(line);
                    }
                }
                crate::authorship::imara_diff_utils::DiffOp::Replace {
                    new_index, new_len, ..
                } => {
                    let start = new_index as u32 + 1;
                    let end = start + new_len as u32;
                    for line in start..end {
                        all_added_lines.push(line);
                    }
                }
                crate::authorship::imara_diff_utils::DiffOp::Equal { .. }
                | crate::authorship::imara_diff_utils::DiffOp::Delete { .. } => {}
            }
        }

        if !all_added_lines.is_empty() {
            unstaged_hunks.insert(
                file_path.clone(),
                LineRange::compress_lines(&all_added_lines),
            );
        }
        if !pure_insertion_lines.is_empty() {
            pure_insertion_hunks
                .insert(file_path, LineRange::compress_lines(&pure_insertion_lines));
        }
    }

    Ok((unstaged_hunks, pure_insertion_hunks))
}

fn split_lines_preserving_terminators(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;

    for (idx, ch) in s.char_indices() {
        if ch == '\n' {
            lines.push(&s[start..idx + 1]);
            start = idx + 1;
        }
    }

    if start < s.len() {
        lines.push(&s[start..]);
    }

    lines
}

fn diff_hunks_between_contents(old_content: &str, new_content: &str) -> Vec<DiffHunk> {
    let normalized_old = normalize_line_endings(old_content);
    let normalized_new = normalize_line_endings(new_content);
    let old_lines = split_lines_preserving_terminators(&normalized_old);
    let new_lines = split_lines_preserving_terminators(&normalized_new);
    crate::authorship::imara_diff_utils::capture_diff_slices(&old_lines, &new_lines)
        .into_iter()
        .filter_map(|op| match op {
            crate::authorship::imara_diff_utils::DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Some(DiffHunk {
                old_start: old_index as u32,
                old_count: 0,
                new_start: new_index as u32 + 1,
                new_count: new_len as u32,
            }),
            crate::authorship::imara_diff_utils::DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => Some(DiffHunk {
                old_start: old_index as u32 + 1,
                old_count: old_len as u32,
                new_start: new_index as u32 + 1,
                new_count: 0,
            }),
            crate::authorship::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => Some(DiffHunk {
                old_start: old_index as u32 + 1,
                old_count: old_len as u32,
                new_start: new_index as u32 + 1,
                new_count: new_len as u32,
            }),
            crate::authorship::imara_diff_utils::DiffOp::Equal { .. } => None,
        })
        .collect()
}

fn line_sequence_contains(needle: &str, haystack: &str) -> bool {
    let normalized_needle = normalize_line_endings(needle);
    let normalized_haystack = normalize_line_endings(haystack);
    let needle_lines = split_lines_preserving_terminators(&normalized_needle);
    if needle_lines.is_empty() {
        return true;
    }

    let mut next_needle = 0;
    for haystack_line in split_lines_preserving_terminators(&normalized_haystack) {
        if haystack_line == needle_lines[next_needle] {
            next_needle += 1;
            if next_needle == needle_lines.len() {
                return true;
            }
        }
    }
    false
}

/// Pure carryover reconciliation given already-fetched contents (no git ops).
/// `parent_content` is the file at the parent commit ("" if absent / initial).
fn merged_carryover_content_pure(
    parent_content: &str,
    committed_content: &str,
    observed_content: &str,
) -> String {
    if content_eq_ignoring_line_endings(committed_content, observed_content) {
        return committed_content.to_string();
    }
    if line_sequence_contains(committed_content, observed_content) {
        return observed_content.to_string();
    }
    if line_sequence_contains(observed_content, committed_content) {
        return committed_content.to_string();
    }
    if content_eq_ignoring_line_endings(committed_content, parent_content) {
        return observed_content.to_string();
    }
    if content_eq_ignoring_line_endings(observed_content, parent_content) {
        return committed_content.to_string();
    }
    carryover_merge_content(parent_content, committed_content, observed_content)
}

/// In-memory 3-way line merge replacing a per-file `git merge-file --theirs -p
/// <committed> <parent> <observed>` spawn (base = `parent`, "ours" =
/// `committed`, favored "theirs" = `observed`). Implements the standard diff3
/// chunk algorithm: align both sides to the base, walk base regions, take the
/// changed side for one-sided changes, and resolve two-sided (conflicting)
/// changes to the observed side. The result feeds an in-memory diff for line
/// bucketing (not stored as an authoritative blob), so byte-exact parity with
/// git's conflict formatting is not required — only a faithful clean-merge
/// reconstruction. Keeps the carryover snapshot build free of per-file spawns.
fn carryover_merge_content(parent: &str, committed: &str, observed: &str) -> String {
    use crate::authorship::imara_diff_utils::{DiffOp, capture_diff_slices};

    if content_eq_ignoring_line_endings(committed, observed) {
        return committed.to_string();
    }
    if content_eq_ignoring_line_endings(parent, committed) {
        return observed.to_string();
    }
    if content_eq_ignoring_line_endings(parent, observed) {
        return committed.to_string();
    }

    let base_lines = split_lines_preserving_terminators(parent);
    let committed_lines = split_lines_preserving_terminators(committed);
    let observed_lines = split_lines_preserving_terminators(observed);
    let normalized_parent = normalize_line_endings(parent);
    let normalized_committed = normalize_line_endings(committed);
    let normalized_observed = normalize_line_endings(observed);
    let base_cmp_lines = split_lines_preserving_terminators(&normalized_parent);
    let committed_cmp_lines = split_lines_preserving_terminators(&normalized_committed);
    let observed_cmp_lines = split_lines_preserving_terminators(&normalized_observed);

    // For each side, map every base line index to its aligned index on that
    // side (None if the base line was changed/deleted on that side). Also record
    // each side's content so we can emit it for changed chunks.
    fn align_to_base(base: &[&str], side: &[&str]) -> Vec<Option<usize>> {
        let base_len = base.len();
        let mut map = vec![None; base_len];
        for op in capture_diff_slices(base, side) {
            if let DiffOp::Equal {
                old_index,
                new_index,
                len,
            } = op
            {
                for k in 0..len {
                    map[old_index + k] = Some(new_index + k);
                }
            }
        }
        map
    }

    let committed_map = align_to_base(&base_cmp_lines, &committed_cmp_lines);
    let observed_map = align_to_base(&base_cmp_lines, &observed_cmp_lines);

    // A base line is "stable" when both sides keep it aligned (unchanged on
    // both). We walk base lines; runs of stable lines are emitted verbatim,
    // and the gaps between them are chunks where at least one side changed.
    // Within each chunk we also consume the corresponding side lines (between
    // the surrounding stable anchors) so inserts/edits are captured.
    let mut result: Vec<String> = Vec::new();
    let mut base_i = 0usize;
    let mut committed_i = 0usize; // next unconsumed committed line
    let mut observed_i = 0usize; // next unconsumed observed line

    // Helper: is base line `i` stable (aligned on both sides)?
    let is_stable = |i: usize| committed_map[i].is_some() && observed_map[i].is_some();

    while base_i < base_lines.len() {
        if is_stable(base_i) {
            // Emit any side-only insertions that occur before this anchor, then
            // the stable line itself. The anchor's side positions:
            let c_anchor = committed_map[base_i].unwrap();
            let o_anchor = observed_map[base_i].unwrap();

            // Lines inserted on each side before the anchor (relative to last
            // consumed position) belong to the preceding chunk; but if we reach
            // a stable line directly we still must flush pending inserts.
            // committed pending inserts:
            let c_pending: Vec<String> = if committed_i < c_anchor {
                committed_lines[committed_i..c_anchor]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            };
            let o_pending: Vec<String> = if observed_i < o_anchor {
                observed_lines[observed_i..o_anchor]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            };
            // Resolve pending region: if both sides inserted differing content,
            // favor observed; else take whichever inserted.
            if !c_pending.is_empty() && !o_pending.is_empty() {
                if committed_cmp_lines[committed_i..c_anchor]
                    == observed_cmp_lines[observed_i..o_anchor]
                {
                    result.extend(c_pending);
                } else {
                    result.extend(o_pending);
                }
            } else if !c_pending.is_empty() {
                result.extend(c_pending);
            } else if !o_pending.is_empty() {
                result.extend(o_pending);
            }

            result.push(base_lines[base_i].to_string());
            committed_i = c_anchor + 1;
            observed_i = o_anchor + 1;
            base_i += 1;
        } else {
            // Start of a change chunk: advance base over all non-stable lines.
            let chunk_base_start = base_i;
            while base_i < base_lines.len() && !is_stable(base_i) {
                base_i += 1;
            }
            // The next stable anchor (or end) bounds how far each side consumes.
            let (c_anchor, o_anchor) = if base_i < base_lines.len() {
                (
                    committed_map[base_i].unwrap(),
                    observed_map[base_i].unwrap(),
                )
            } else {
                (committed_lines.len(), observed_lines.len())
            };

            let committed_chunk: Vec<String> = committed_lines[committed_i..c_anchor]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            let observed_chunk: Vec<String> = observed_lines[observed_i..o_anchor]
                .iter()
                .map(|s| (*s).to_string())
                .collect();

            // Determine which sides changed this base region relative to base.
            let base_chunk: Vec<String> = base_lines[chunk_base_start..base_i]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            let base_cmp_chunk = &base_cmp_lines[chunk_base_start..base_i];
            let committed_cmp_chunk = &committed_cmp_lines[committed_i..c_anchor];
            let observed_cmp_chunk = &observed_cmp_lines[observed_i..o_anchor];
            let committed_changed = committed_cmp_chunk != base_cmp_chunk;
            let observed_changed = observed_cmp_chunk != base_cmp_chunk;

            match (committed_changed, observed_changed) {
                (true, false) => result.extend(committed_chunk),
                (false, true) => result.extend(observed_chunk),
                (true, true) => {
                    // Two-sided change → favor observed (matches --theirs),
                    // unless both produced identical content.
                    if committed_cmp_chunk == observed_cmp_chunk {
                        result.extend(committed_chunk);
                    } else {
                        result.extend(observed_chunk);
                    }
                }
                (false, false) => result.extend(base_chunk),
            }

            committed_i = c_anchor;
            observed_i = o_anchor;
        }
    }

    // Flush any trailing inserts past the last base line on each side.
    let c_tail: Vec<String> = committed_lines[committed_i..]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let o_tail: Vec<String> = observed_lines[observed_i..]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if !c_tail.is_empty() && !o_tail.is_empty() {
        if committed_cmp_lines[committed_i..] == observed_cmp_lines[observed_i..] {
            result.extend(c_tail);
        } else {
            result.extend(o_tail);
        }
    } else if !c_tail.is_empty() {
        result.extend(c_tail);
    } else if !o_tail.is_empty() {
        result.extend(o_tail);
    }

    result.concat()
}

fn mapped_line_range(
    base_to_target: &[Option<usize>],
    old_index: usize,
    old_len: usize,
) -> Option<(usize, usize)> {
    if old_len == 0 {
        return mapped_insertion_point(base_to_target, old_index);
    }
    let first = base_to_target.get(old_index).copied().flatten()?;
    for offset in 0..old_len {
        if base_to_target.get(old_index + offset).copied().flatten()? != first + offset {
            return None;
        }
    }
    Some((first, first + old_len))
}

fn mapped_conflict_range(
    target_changes: &[(usize, usize, usize, usize)],
    old_index: usize,
    old_len: usize,
) -> Option<(usize, usize)> {
    if old_len == 0 {
        return None;
    }
    let old_end = old_index.saturating_add(old_len);
    let mut target_start = usize::MAX;
    let mut target_end = 0usize;
    for (change_old_start, change_old_end, change_target_start, change_target_end) in target_changes
    {
        if *change_old_start < old_end && old_index < *change_old_end {
            target_start = target_start.min(*change_target_start);
            target_end = target_end.max(*change_target_end);
        }
    }
    if target_start == usize::MAX {
        None
    } else {
        Some((target_start, target_end))
    }
}

fn mapped_insertion_point(
    base_to_target: &[Option<usize>],
    old_index: usize,
) -> Option<(usize, usize)> {
    if base_to_target.is_empty() {
        return Some((0, 0));
    }
    if old_index > 0
        && let Some(Some(previous)) = base_to_target.get(old_index - 1)
    {
        let point = previous + 1;
        return Some((point, point));
    }
    if let Some(Some(next)) = base_to_target.get(old_index) {
        return Some((*next, *next));
    }
    None
}

fn line_without_terminator(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn fill_line_ending_only_mappings(
    base_lines: &[&str],
    target_lines: &[&str],
    base_to_target: &mut [Option<usize>],
) {
    let mut used_targets = vec![false; target_lines.len()];
    for target_index in base_to_target.iter().flatten() {
        if let Some(used) = used_targets.get_mut(*target_index) {
            *used = true;
        }
    }

    let mut search_start = 0usize;
    for (base_index, base_line) in base_lines.iter().enumerate() {
        if let Some(target_index) = base_to_target[base_index] {
            search_start = search_start.max(target_index.saturating_add(1));
            continue;
        }

        let base_text = line_without_terminator(base_line);
        if let Some(target_index) = (search_start..target_lines.len()).find(|target_index| {
            !used_targets[*target_index]
                && line_without_terminator(target_lines[*target_index]) == base_text
        }) {
            base_to_target[base_index] = Some(target_index);
            used_targets[target_index] = true;
            search_start = target_index.saturating_add(1);
        }
    }
}

fn checkout_merge_rebased_content(
    base_content: &str,
    target_content: &str,
    observed_content: &str,
) -> String {
    if base_content == target_content {
        return observed_content.to_string();
    }
    if base_content == observed_content {
        return target_content.to_string();
    }

    let base_lines = split_lines_preserving_terminators(base_content);
    let target_lines = split_lines_preserving_terminators(target_content);
    let observed_lines = split_lines_preserving_terminators(observed_content);

    let mut base_to_target = vec![None; base_lines.len()];
    let mut target_changes = Vec::new();
    for op in crate::authorship::imara_diff_utils::capture_diff_slices(&base_lines, &target_lines) {
        match op {
            crate::authorship::imara_diff_utils::DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for offset in 0..len {
                    base_to_target[old_index + offset] = Some(new_index + offset);
                }
            }
            crate::authorship::imara_diff_utils::DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                target_changes.push((old_index, old_index + old_len, new_index, new_index));
            }
            crate::authorship::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                target_changes.push((
                    old_index,
                    old_index + old_len,
                    new_index,
                    new_index + new_len,
                ));
            }
            crate::authorship::imara_diff_utils::DiffOp::Insert { .. } => {}
        }
    }
    fill_line_ending_only_mappings(&base_lines, &target_lines, &mut base_to_target);

    let mut edits = Vec::<(usize, usize, Vec<String>)>::new();
    for op in crate::authorship::imara_diff_utils::capture_diff_slices(&base_lines, &observed_lines)
    {
        match op {
            crate::authorship::imara_diff_utils::DiffOp::Equal { .. } => {}
            crate::authorship::imara_diff_utils::DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, 0) {
                    edits.push((
                        start,
                        end,
                        observed_lines[new_index..new_index + new_len]
                            .iter()
                            .map(|line| (*line).to_string())
                            .collect(),
                    ));
                }
            }
            crate::authorship::imara_diff_utils::DiffOp::Delete {
                old_index, old_len, ..
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, old_len)
                    .or_else(|| mapped_conflict_range(&target_changes, old_index, old_len))
                {
                    edits.push((start, end, Vec::new()));
                }
            }
            crate::authorship::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, old_len)
                    .or_else(|| mapped_conflict_range(&target_changes, old_index, old_len))
                {
                    edits.push((
                        start,
                        end,
                        observed_lines[new_index..new_index + new_len]
                            .iter()
                            .map(|line| (*line).to_string())
                            .collect(),
                    ));
                }
            }
        }
    }

    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut rebased = target_lines
        .iter()
        .map(|line| (*line).to_string())
        .collect::<Vec<_>>();
    for (start, end, replacement) in edits {
        if start <= end && end <= rebased.len() {
            rebased.splice(start..end, replacement);
        }
    }
    rebased.concat()
}

/// Batch-read the content of many `(treeish, path)` pairs in a CONSTANT number
/// of git spawns (one `cat-file --batch-check` + one `cat-file --batch`),
/// regardless of how many files. Missing paths map to an empty string (the same
/// degradation `get_file_content_at_commit` produces for an absent path).
fn batch_file_contents(
    repo: &Repository,
    requests: &[(String, String)],
) -> Result<HashMap<(String, String), String>, GitAiError> {
    if requests.is_empty() {
        return Ok(HashMap::new());
    }
    let mut map = batch_read_paths_at_treeishes(repo, requests)?;
    // Ensure every requested pair has an entry (absent paths → "").
    for req in requests {
        map.entry(req.clone()).or_default();
    }
    Ok(map)
}

pub fn checkout_merge_final_state_snapshot(
    repo: &Repository,
    old_head: &str,
    new_head: &str,
) -> Result<HashMap<String, String>, GitAiError> {
    if old_head.is_empty() || new_head.is_empty() || old_head == new_head {
        return Ok(HashMap::new());
    }
    if !repo.storage.has_working_log(old_head) {
        return Ok(HashMap::new());
    }

    let working_log = repo.storage.working_log_for_base_commit(old_head)?;
    let observed_snapshot = working_log.observed_file_snapshot()?;

    // Batch-read base (old_head) + target (new_head) content for every observed
    // file in two git spawns instead of two spawns PER file.
    let mut requests: Vec<(String, String)> = Vec::with_capacity(observed_snapshot.len() * 2);
    for file_path in observed_snapshot.keys() {
        requests.push((old_head.to_string(), file_path.clone()));
        requests.push((new_head.to_string(), file_path.clone()));
    }
    let contents = batch_file_contents(repo, &requests)?;

    let mut final_state = HashMap::new();
    for (file_path, observed_content) in observed_snapshot {
        let base_content = contents
            .get(&(old_head.to_string(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        let target_content = contents
            .get(&(new_head.to_string(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        let content =
            checkout_merge_rebased_content(&base_content, &target_content, &observed_content);
        final_state.insert(file_path, content);
    }
    Ok(final_state)
}

fn build_carryover_snapshot(
    repo: &Repository,
    parent_sha: &str,
    commit_sha: &str,
    pathspecs: Option<&HashSet<String>>,
    observed_snapshot: &HashMap<String, String>,
) -> Result<HashMap<String, String>, GitAiError> {
    let file_paths: HashSet<String> = match pathspecs {
        Some(paths) => paths.iter().cloned().collect(),
        None => observed_snapshot.keys().cloned().collect(),
    };

    // Batch-read committed (commit_sha) content for every file, plus parent
    // (parent_sha) content for the files we may need to 3-way reconcile. Two
    // git spawns total instead of up to ~2 per file.
    let mut requests: Vec<(String, String)> = Vec::new();
    for file_path in &file_paths {
        requests.push((commit_sha.to_string(), file_path.clone()));
        if parent_sha != "initial" && observed_snapshot.contains_key(file_path) {
            requests.push((parent_sha.to_string(), file_path.clone()));
        }
    }
    let contents = batch_file_contents(repo, &requests)?;

    let mut carryover_snapshot = HashMap::new();
    for file_path in file_paths {
        let committed_content = contents
            .get(&(commit_sha.to_string(), file_path.clone()))
            .cloned()
            .unwrap_or_default();
        let content = if let Some(observed_content) = observed_snapshot.get(&file_path) {
            let parent_content = if parent_sha == "initial" {
                String::new()
            } else {
                contents
                    .get(&(parent_sha.to_string(), file_path.clone()))
                    .cloned()
                    .unwrap_or_default()
            };
            merged_carryover_content_pure(&parent_content, &committed_content, observed_content)
        } else {
            committed_content
        };
        carryover_snapshot.insert(file_path, content);
    }

    Ok(carryover_snapshot)
}

impl VirtualAttributions {
    /// Split VirtualAttributions into committed and uncommitted buckets
    ///
    /// This method uses git diff to determine which line attributions belong in:
    /// - Bucket 1 (committed): Lines added in this commit → AuthorshipLog
    /// - Bucket 2 (uncommitted): Lines NOT added in this commit → InitialAttributions
    pub fn to_authorship_log_and_initial_working_log(
        &self,
        repo: &Repository,
        parent_sha: &str,
        commit_sha: &str,
        pathspecs: Option<&HashSet<String>>,
        final_state_snapshot: Option<&HashMap<String, String>>,
    ) -> Result<
        (
            crate::authorship::authorship_log_serialization::AuthorshipLog,
            crate::git::repo_storage::InitialAttributions,
            HashMap<String, String>,
        ),
        GitAiError,
    > {
        self.to_authorship_log_and_initial_working_log_with_precomputed_diff(
            repo,
            parent_sha,
            commit_sha,
            pathspecs,
            final_state_snapshot,
            AuthorshipLogDiffContext::default(),
        )
    }

    /// As [`Self::to_authorship_log_and_initial_working_log`], but accepts a
    /// pre-computed parent→commit `DiffTreeResult` so a batched caller (e.g. the
    /// rebase conflict-resolution driver) can supply renames + committed hunks
    /// from a single batched `diff-tree` instead of this method spawning its own
    /// per-commit `git diff` / `git diff-tree`.
    ///
    /// When the context has no precomputed diff, it may override the diff base
    /// used only for committed-hunk and rename classification. Other
    /// reconciliation still uses `parent_sha`.
    pub(crate) fn to_authorship_log_and_initial_working_log_with_precomputed_diff(
        &self,
        repo: &Repository,
        parent_sha: &str,
        commit_sha: &str,
        pathspecs: Option<&HashSet<String>>,
        final_state_snapshot: Option<&HashMap<String, String>>,
        diff_context: AuthorshipLogDiffContext<'_>,
    ) -> Result<
        (
            crate::authorship::authorship_log_serialization::AuthorshipLog,
            crate::git::repo_storage::InitialAttributions,
            HashMap<String, String>,
        ),
        GitAiError,
    > {
        use crate::authorship::authorship_log_serialization::AuthorshipLog;
        use crate::git::repo_storage::InitialAttributions;
        use std::collections::{HashMap as StdHashMap, HashSet};

        let mut authorship_log = AuthorshipLog::new();
        authorship_log.metadata.base_commit_sha = self.base_commit.clone();
        // Flatten the nested prompts map: take the most recent (first) prompt for each prompt_id
        authorship_log.metadata.prompts = self
            .prompts
            .iter()
            .filter_map(|(prompt_id, commits)| {
                // Get the first (most recent) commit's PromptRecord
                commits
                    .values()
                    .next()
                    .map(|record| (prompt_id.clone(), record.clone()))
            })
            .collect();
        authorship_log.metadata.humans = self.humans.clone();
        authorship_log.metadata.sessions = self.sessions.clone();

        let mut initial_files: StdHashMap<String, Vec<LineAttribution>> = StdHashMap::new();
        let mut referenced_prompts: HashSet<String> = HashSet::new();
        let mut initial_humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        let mut initial_sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
        let mut initial_file_contents: StdHashMap<String, String> = StdHashMap::new();

        // Detect renames so we can look up committed hunks by new path when
        // the working log references the old path. A batched caller may supply
        // the parent→commit diff (renames included); otherwise spawn per-commit.
        //
        // The persisted working-log base can be an older ref tip on daemon
        // fast-forward paths. Diff classification must still be bounded to the
        // finalized commit, while carryover reconciliation below continues to
        // use the original working-log base.
        let precomputed_parent_diff = diff_context.precomputed_parent_diff;
        let fallback_diff_base = diff_context
            .fallback_committed_diff_base
            .unwrap_or(parent_sha);
        let rename_map = if let Some(diff) = precomputed_parent_diff {
            diff.renames.iter().cloned().collect()
        } else if parent_sha != "initial" {
            detect_renames_in_commit(repo, fallback_diff_base, commit_sha).unwrap_or_default()
        } else {
            HashMap::new()
        };

        // Extend pathspecs with renamed-to paths so diff_added_lines doesn't filter them out.
        let extended_pathspecs;
        let effective_pathspecs = if !rename_map.is_empty()
            && let Some(ps_ref) = pathspecs
        {
            let mut ps = ps_ref.clone();
            for (old_path, new_path) in &rename_map {
                if ps.contains(old_path) {
                    ps.insert(new_path.clone());
                }
            }
            extended_pathspecs = ps;
            Some(&extended_pathspecs)
        } else {
            pathspecs
        };

        // Get committed hunks (in commit coordinates) and unstaged hunks (in working directory coordinates)
        let committed_hunks = if let Some(diff) = precomputed_parent_diff {
            committed_hunks_from_diff_result(diff, effective_pathspecs)
        } else {
            collect_committed_hunks(repo, fallback_diff_base, commit_sha, effective_pathspecs)?
        };
        let carryover_snapshot = if let Some(snapshot) = final_state_snapshot {
            Some(build_carryover_snapshot(
                repo,
                parent_sha,
                commit_sha,
                effective_pathspecs,
                snapshot,
            )?)
        } else {
            None
        };
        let (mut unstaged_hunks, pure_insertion_hunks) = if let Some(snapshot) = &carryover_snapshot
        {
            collect_unstaged_hunks_from_snapshot(repo, commit_sha, effective_pathspecs, snapshot)?
        } else {
            collect_unstaged_hunks(repo, commit_sha, effective_pathspecs)?
        };

        // IMPORTANT: If a line appears in both committed_hunks and unstaged_hunks, it means:
        // - The line was committed in this commit (in commit coordinates)
        // - The line was then modified again in the working directory (in workdir coordinates)
        // Since both use the same line numbering after the commit (workdir coordinates = commit coordinates
        // for the committed state), we can directly compare line numbers.
        // We should treat these lines as committed, not unstaged, because the attribution belongs
        // to the commit even if there's a subsequent unstaged modification.
        //
        // HOWEVER: If a line is a PURE INSERTION (old_count=0), it means a new line was inserted
        // at that position, pushing existing lines down. In this case, the line number overlap
        // doesn't mean the same line - it's a different line at the same position!
        // We should NOT filter out pure insertions even if they overlap with committed line numbers.
        for (file_path, committed_ranges) in &committed_hunks {
            if let Some(unstaged_ranges) = unstaged_hunks.get_mut(file_path) {
                // Expand both to line numbers for comparison
                let committed_lines: std::collections::HashSet<u32> =
                    committed_ranges.iter().flat_map(|r| r.expand()).collect();

                // Get pure insertion lines for this file (these should NOT be filtered out)
                let pure_insertion_lines: std::collections::HashSet<u32> = pure_insertion_hunks
                    .get(file_path)
                    .map(|ranges| ranges.iter().flat_map(|r| r.expand()).collect())
                    .unwrap_or_default();

                // Filter out any unstaged lines that were also committed
                // (these are lines that were committed, then modified again in workdir)
                // BUT keep pure insertions even if they overlap with committed line numbers
                let mut filtered_unstaged_lines: Vec<u32> = unstaged_ranges
                    .iter()
                    .flat_map(|r| r.expand())
                    .filter(|line| {
                        // Keep the line if it's NOT in committed, OR if it's a pure insertion
                        !committed_lines.contains(line) || pure_insertion_lines.contains(line)
                    })
                    .collect();

                if filtered_unstaged_lines.is_empty() {
                    unstaged_ranges.clear();
                } else {
                    filtered_unstaged_lines.sort_unstable();
                    filtered_unstaged_lines.dedup();
                    *unstaged_ranges = LineRange::compress_lines(&filtered_unstaged_lines);
                }
            }
        }

        // Remove files with no unstaged hunks
        unstaged_hunks.retain(|_, ranges| !ranges.is_empty());

        // Process each file
        for (file_path, (_, line_attrs)) in &self.attributions {
            if line_attrs.is_empty() {
                continue;
            }

            // Diff output keys are NFC-normalised, but working-log paths may be
            // NFD.  Compute the NFC form once for all lookups in this iteration.
            let nfc_file_path: String = file_path.nfc().collect();

            let rebased_line_attrs;
            let line_attrs = if let Some(snapshot) = &carryover_snapshot {
                let carryover_content = snapshot
                    .get(&nfc_file_path)
                    .or_else(|| snapshot.get(file_path))
                    .ok_or_else(|| {
                        GitAiError::Generic(format!(
                            "carryover snapshot missing content for {}",
                            file_path
                        ))
                    })?;
                let observed_content = self
                    .file_contents
                    .get(file_path)
                    .or_else(|| self.file_contents.get(&nfc_file_path))
                    .ok_or_else(|| {
                        GitAiError::Generic(format!(
                            "virtual attribution missing content for {}",
                            file_path
                        ))
                    })?;
                let shift_hunks = diff_hunks_between_contents(observed_content, carryover_content);
                rebased_line_attrs =
                    apply_hunk_shifts_to_line_attributions(line_attrs, &shift_hunks);
                &rebased_line_attrs
            } else {
                line_attrs
            };

            // Get unstaged lines for this file (in working directory coordinates).
            let mut unstaged_lines: Vec<u32> = Vec::new();
            let unstaged_lookup = unstaged_hunks.get(&nfc_file_path).or_else(|| {
                rename_map
                    .get(&nfc_file_path)
                    .and_then(|np| unstaged_hunks.get(np))
            });
            if let Some(unstaged_ranges) = unstaged_lookup {
                for range in unstaged_ranges {
                    unstaged_lines.extend(range.expand());
                }
                unstaged_lines.sort_unstable();
            }

            // Split line attributions into committed and uncommitted
            // VirtualAttributions has line numbers in working directory coordinates,
            // so we need to convert to commit coordinates before comparing with committed hunks
            let mut committed_lines_map: StdHashMap<String, Vec<u32>> = StdHashMap::new();
            let mut uncommitted_lines_map: StdHashMap<String, Vec<u32>> = StdHashMap::new();

            // Get the committed hunks for this file (if any) - these are in commit coordinates.
            // If the file was renamed, committed_hunks is keyed by the new path.
            let file_committed_hunks = committed_hunks.get(&nfc_file_path).or_else(|| {
                rename_map
                    .get(&nfc_file_path)
                    .and_then(|np| committed_hunks.get(np))
            });

            for line_attr in line_attrs {
                // Check each line individually
                for workdir_line_num in line_attr.start_line..=line_attr.end_line {
                    // Check if this line is unstaged (in working directory but not in commit)
                    let is_unstaged = unstaged_lines.binary_search(&workdir_line_num).is_ok();

                    if is_unstaged {
                        // Line is unstaged, mark as uncommitted
                        uncommitted_lines_map
                            .entry(line_attr.author_id.clone())
                            .or_default()
                            .push(workdir_line_num);
                        referenced_prompts.insert(line_attr.author_id.clone());
                    } else {
                        // Convert working directory line number to commit line number
                        // by subtracting the count of unstaged lines before this line
                        let adjustment = unstaged_lines
                            .iter()
                            .filter(|&&l| l < workdir_line_num)
                            .count() as u32;
                        let commit_line_num = workdir_line_num - adjustment;

                        // Check if this commit line number is in any committed hunk
                        let is_committed = if let Some(hunks) = file_committed_hunks {
                            hunks.iter().any(|hunk| hunk.contains(commit_line_num))
                        } else {
                            false
                        };

                        let is_renamed_file = rename_map.contains_key(&nfc_file_path);

                        if is_committed {
                            // Line was committed in this commit (use commit coordinates)
                            committed_lines_map
                                .entry(line_attr.author_id.clone())
                                .or_default()
                                .push(commit_line_num);
                        } else if is_renamed_file
                            && line_attr.author_id != CheckpointKind::Human.to_str()
                            && !line_attr.author_id.starts_with("h_")
                        {
                            // For renamed files, git blame attributes ALL lines to
                            // this commit. Include AI lines in the note even if they're
                            // not in committed_hunks — without this, they'd have no
                            // attestation and blame would fall back to the git committer.
                            committed_lines_map
                                .entry(line_attr.author_id.clone())
                                .or_default()
                                .push(commit_line_num);
                        }
                    }
                }
            }

            // Fill gaps in committed hunks caused by imara_diff Equal matching.
            //
            // When AI rewrites a region, imara_diff can match byte-for-byte
            // identical lines (e.g. empty lines between code blocks) as "Equal",
            // preserving the old human attribution. Those lines get stripped from
            // the checkpoint's line_attributions and never make it here. This
            // leaves gaps in committed_hunks that show as [no-data] in `git ai diff`.
            //
            // Fix: for each gap line in a committed hunk, check the nearest
            // attributed line before and after it. If both neighbors have the
            // same AI author (not human/h_), fill the gap with that author.
            if let Some(hunks) = file_committed_hunks {
                // Build a sorted map of committed line → author_id for neighbor lookups
                let mut line_to_author: Vec<(u32, &str)> = Vec::new();
                for (author_id, lines) in &committed_lines_map {
                    for &line in lines {
                        line_to_author.push((line, author_id.as_str()));
                    }
                }
                line_to_author.sort_by_key(|(line, _)| *line);

                let mut gap_fills: Vec<(String, u32)> = Vec::new();

                // Read file content for content-based gap matching
                let gap_file_content = self
                    .file_contents
                    .get(file_path)
                    .or_else(|| self.file_contents.get(&nfc_file_path));
                let gap_file_lines: Vec<&str> = gap_file_content
                    .map(|c| c.lines().collect())
                    .unwrap_or_default();

                // Build content→author map from AI-attributed lines
                let mut content_to_ai_author: StdHashMap<&str, &str> = StdHashMap::new();
                if !gap_file_lines.is_empty() {
                    for &(line_num, author) in &line_to_author {
                        if !author.starts_with("h_")
                            && author != CheckpointKind::Human.to_str()
                            && let Some(&content) = gap_file_lines.get((line_num - 1) as usize)
                            && !content.trim().is_empty()
                        {
                            content_to_ai_author.insert(content, author);
                        }
                    }
                }

                for hunk in hunks {
                    for line in hunk.expand() {
                        // Skip lines that already have attribution
                        if line_to_author
                            .binary_search_by_key(&line, |(l, _)| *l)
                            .is_ok()
                        {
                            continue;
                        }

                        // Find nearest attributed neighbor before this line
                        let prev = line_to_author.iter().rev().find(|(l, _)| *l < line);

                        // Find nearest attributed neighbor after this line
                        let next = line_to_author.iter().find(|(l, _)| *l > line);

                        // Fill if both neighbors exist and are the same AI author
                        if let (Some((_, prev_author)), Some((_, next_author))) = (prev, next)
                            && prev_author == next_author
                            && !prev_author.starts_with("h_")
                        {
                            gap_fills.push((prev_author.to_string(), line));
                        } else if let Some(&content) = gap_file_lines.get((line - 1) as usize) {
                            // Content-based fallback: if the gap line has the same
                            // content as an AI-attributed line in this file, it's
                            // likely part of the same AI edit (imara_diff matched it
                            // as Equal against old content by mistake).
                            if let Some(&author) = content_to_ai_author.get(content) {
                                gap_fills.push((author.to_string(), line));
                            }
                        }
                    }
                }

                for (author_id, line) in gap_fills {
                    committed_lines_map.entry(author_id).or_default().push(line);
                }
            }

            // Add committed attributions to authorship log
            if !committed_lines_map.is_empty() {
                // Create attestation entries from committed lines
                for (author_id, mut lines) in committed_lines_map {
                    // Skip the legacy "human" sentinel (CheckpointKind::Human checkpoints that were
                    // never attested). KnownHuman lines use h_-prefixed author IDs and pass through.
                    if author_id == CheckpointKind::Human.to_str() {
                        continue;
                    }

                    lines.sort();
                    lines.dedup();

                    if lines.is_empty() {
                        continue;
                    }

                    // Create line ranges
                    let mut ranges = Vec::new();
                    let mut range_start = lines[0];
                    let mut range_end = lines[0];

                    for &line in &lines[1..] {
                        if line == range_end + 1 {
                            range_end = line;
                        } else {
                            if range_start == range_end {
                                ranges.push(crate::authorship::authorship_log::LineRange::Single(
                                    range_start,
                                ));
                            } else {
                                ranges.push(crate::authorship::authorship_log::LineRange::Range(
                                    range_start,
                                    range_end,
                                ));
                            }
                            range_start = line;
                            range_end = line;
                        }
                    }

                    // Add the last range
                    if range_start == range_end {
                        ranges.push(crate::authorship::authorship_log::LineRange::Single(
                            range_start,
                        ));
                    } else {
                        ranges.push(crate::authorship::authorship_log::LineRange::Range(
                            range_start,
                            range_end,
                        ));
                    }

                    let entry =
                        crate::authorship::authorship_log_serialization::AttestationEntry::new(
                            author_id, ranges,
                        );

                    let attestation_path = rename_map.get(&nfc_file_path).unwrap_or(&nfc_file_path);
                    let file_attestation = authorship_log.get_or_create_file(attestation_path);
                    file_attestation.add_entry(entry);
                }
            }

            // Add uncommitted attributions to INITIAL
            if !uncommitted_lines_map.is_empty() {
                // Convert the map into line attributions
                let mut uncommitted_line_attrs = Vec::new();
                for (author_id, mut lines) in uncommitted_lines_map {
                    // Skip the legacy "human" sentinel (CheckpointKind::Human checkpoints that were
                    // never attested). KnownHuman lines use h_-prefixed author IDs and pass through.
                    if author_id == CheckpointKind::Human.to_str() {
                        continue;
                    }

                    lines.sort();
                    lines.dedup();

                    if lines.is_empty() {
                        continue;
                    }

                    // Track h_ hashes for INITIAL humans map
                    if author_id.starts_with("h_") {
                        // h_ hash absent from self.humans — foreign cherry-pick or pre-existing
                        // INITIAL attribution. Intentionally skip: the record is not needed locally.
                        if let Some(record) = self.humans.get(&author_id) {
                            initial_humans.insert(author_id.clone(), record.clone());
                        }
                    }

                    // Track s_ sessions for INITIAL sessions map
                    if author_id.starts_with("s_") {
                        let session_key = author_id
                            .split("::")
                            .next()
                            .unwrap_or(&author_id)
                            .to_string();
                        if let Some(record) = self.sessions.get(&session_key) {
                            initial_sessions.insert(session_key, record.clone());
                        }
                    }

                    // Create ranges from individual lines
                    let mut range_start = lines[0];
                    let mut range_end = lines[0];

                    for &line in &lines[1..] {
                        if line == range_end + 1 {
                            range_end = line;
                        } else {
                            // End current range and start new one
                            uncommitted_line_attrs.push(LineAttribution {
                                start_line: range_start,
                                end_line: range_end,
                                author_id: author_id.clone(),
                                overrode: None,
                            });
                            range_start = line;
                            range_end = line;
                        }
                    }

                    // Add the last range
                    uncommitted_line_attrs.push(LineAttribution {
                        start_line: range_start,
                        end_line: range_end,
                        author_id: author_id.clone(),
                        overrode: None,
                    });
                }

                let initial_path = rename_map.get(file_path).unwrap_or(file_path);
                initial_files.insert(initial_path.clone(), uncommitted_line_attrs);
                if let Some(snapshot) = &carryover_snapshot {
                    if let Some(content) = snapshot
                        .get(initial_path)
                        .or_else(|| snapshot.get(file_path))
                    {
                        initial_file_contents.insert(initial_path.clone(), content.clone());
                    }
                } else if let Some(content) = self
                    .file_contents
                    .get(file_path)
                    .or_else(|| self.file_contents.get(&nfc_file_path))
                {
                    initial_file_contents.insert(initial_path.clone(), content.clone());
                }
            }
        }

        // Remove INITIAL-only prompts that have no committed lines in the
        // attestations.  Prompts originating from current-session checkpoints are
        // kept unconditionally (they represent AI tools used during development,
        // even if their lines didn't land — the "non-landing prompt" feature).
        // Only INITIAL-carried prompts (from prior commits' uncommitted AI lines)
        // are filtered out when they have no committed lines.
        if !self.initial_only_prompt_ids.is_empty() {
            let committed_prompt_ids: HashSet<&String> = authorship_log
                .attestations
                .iter()
                .flat_map(|file_att| file_att.entries.iter())
                .map(|entry| &entry.hash)
                .collect();
            authorship_log.metadata.prompts.retain(|prompt_id, _| {
                // Keep if: not INITIAL-only, OR has committed lines
                !self.initial_only_prompt_ids.contains(prompt_id)
                    || committed_prompt_ids.contains(prompt_id)
            });
        }

        // Prune sessions that have no corresponding attestation entries.
        // Unlike prompts (which keep "non-landing" records for historical reasons),
        // sessions are only retained if at least one attestation references them.
        {
            let committed_session_ids: HashSet<String> = authorship_log
                .attestations
                .iter()
                .flat_map(|file_att| file_att.entries.iter())
                .filter_map(|entry| {
                    if entry.hash.starts_with("s_") {
                        Some(
                            entry
                                .hash
                                .split("::")
                                .next()
                                .unwrap_or(&entry.hash)
                                .to_string(),
                        )
                    } else {
                        None
                    }
                })
                .collect();

            authorship_log
                .metadata
                .sessions
                .retain(|session_id, _| committed_session_ids.contains(session_id));
        }

        // Build prompts map for INITIAL (only prompts referenced by uncommitted lines)
        let mut initial_prompts = StdHashMap::new();
        for prompt_id in referenced_prompts {
            if let Some(commits) = self.prompts.get(&prompt_id) {
                // Get the most recent (first) prompt for this prompt_id
                if let Some(prompt) = commits.values().next() {
                    initial_prompts.insert(prompt_id, prompt.clone());
                }
            }
        }

        let initial_attributions = InitialAttributions {
            files: initial_files,
            prompts: initial_prompts,
            file_blobs: HashMap::new(),
            humans: initial_humans,
            sessions: initial_sessions,
        };

        Ok((authorship_log, initial_attributions, initial_file_contents))
    }

    /// Convert VirtualAttributions to AuthorshipLog only (index-only mode)
    ///
    /// This is a simplified version of `to_authorship_log_and_initial_working_log` that:
    /// - Only returns an AuthorshipLog (no InitialAttributions)
    /// - Doesn't check the working copy or unstaged hunks
    /// - Is used for commits that have already landed
    ///
    /// This is useful for retroactively generating authorship logs from working logs
    /// where we know the commit has landed and don't care about uncommitted work.
    // only being used by stats-delta in a fork
    #[allow(dead_code)]
    pub fn to_authorship_log_index_only(
        &self,
        repo: &Repository,
        parent_sha: &str,
        commit_sha: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<crate::authorship::authorship_log_serialization::AuthorshipLog, GitAiError> {
        use crate::authorship::authorship_log_serialization::AuthorshipLog;
        use std::collections::HashMap as StdHashMap;

        let mut authorship_log = AuthorshipLog::new();
        authorship_log.metadata.base_commit_sha = self.base_commit.clone();
        // Flatten the nested prompts map: take the most recent (first) prompt for each prompt_id
        authorship_log.metadata.prompts = self
            .prompts
            .iter()
            .filter_map(|(prompt_id, commits)| {
                // Get the first (most recent) commit's PromptRecord
                commits
                    .values()
                    .next()
                    .map(|record| (prompt_id.clone(), record.clone()))
            })
            .collect();
        authorship_log.metadata.humans = self.humans.clone();
        authorship_log.metadata.sessions = self.sessions.clone();

        // Get committed hunks only (no need to check working copy)
        let committed_hunks = collect_committed_hunks(repo, parent_sha, commit_sha, pathspecs)?;

        // Process each file
        for (file_path, (_, line_attrs)) in &self.attributions {
            if line_attrs.is_empty() {
                continue;
            }

            // Get the committed hunks for this file (if any).
            // NFC-normalise the key (see first loop's comment for rationale).
            let nfc_file_path: String = file_path.nfc().collect();
            let file_committed_hunks = match committed_hunks.get(&nfc_file_path) {
                Some(hunks) => hunks,
                None => continue, // No committed hunks for this file, skip
            };

            // Map author_id -> line numbers (in commit coordinates)
            let mut committed_lines_map: StdHashMap<String, Vec<u32>> = StdHashMap::new();

            for line_attr in line_attrs {
                // Since we're not dealing with unstaged hunks, the line numbers in VirtualAttributions
                // are already in the right coordinates (working log coordinates = commit coordinates)
                for line_num in line_attr.start_line..=line_attr.end_line {
                    // Check if this line is in any committed hunk
                    let is_committed = file_committed_hunks
                        .iter()
                        .any(|hunk| hunk.contains(line_num));

                    if is_committed {
                        committed_lines_map
                            .entry(line_attr.author_id.clone())
                            .or_default()
                            .push(line_num);
                    }
                }
            }

            // Fill attribution gaps for lines in committed hunks that weren't
            // directly attributed (e.g. empty lines between AI-authored blocks).
            // Only fill if both nearest neighbors share the same AI author.
            {
                let mut line_to_author: Vec<(u32, &str)> = Vec::new();
                for (author_id, lines) in &committed_lines_map {
                    for &line in lines {
                        line_to_author.push((line, author_id.as_str()));
                    }
                }
                line_to_author.sort_by_key(|(line, _)| *line);

                let mut gap_fills: Vec<(String, u32)> = Vec::new();

                for hunk in file_committed_hunks {
                    for line in hunk.expand() {
                        if line_to_author
                            .binary_search_by_key(&line, |(l, _)| *l)
                            .is_ok()
                        {
                            continue;
                        }
                        let prev = line_to_author.iter().rev().find(|(l, _)| *l < line);
                        let next = line_to_author.iter().find(|(l, _)| *l > line);
                        if let (Some((_, prev_author)), Some((_, next_author))) = (prev, next)
                            && prev_author == next_author
                            && !prev_author.starts_with("h_")
                        {
                            gap_fills.push((prev_author.to_string(), line));
                        }
                    }
                }

                for (author_id, line) in gap_fills {
                    committed_lines_map.entry(author_id).or_default().push(line);
                }
            }

            // Add committed attributions to authorship log
            if !committed_lines_map.is_empty() {
                // Create attestation entries from committed lines
                for (author_id, mut lines) in committed_lines_map {
                    // Skip the legacy "human" sentinel (CheckpointKind::Human checkpoints that were
                    // never attested). KnownHuman lines use h_-prefixed author IDs and pass through.
                    if author_id == CheckpointKind::Human.to_str() {
                        continue;
                    }

                    lines.sort();
                    lines.dedup();

                    if lines.is_empty() {
                        continue;
                    }

                    // Create line ranges
                    let mut ranges = Vec::new();
                    let mut range_start = lines[0];
                    let mut range_end = lines[0];

                    for &line in &lines[1..] {
                        if line == range_end + 1 {
                            range_end = line;
                        } else {
                            if range_start == range_end {
                                ranges.push(crate::authorship::authorship_log::LineRange::Single(
                                    range_start,
                                ));
                            } else {
                                ranges.push(crate::authorship::authorship_log::LineRange::Range(
                                    range_start,
                                    range_end,
                                ));
                            }
                            range_start = line;
                            range_end = line;
                        }
                    }

                    // Add the last range
                    if range_start == range_end {
                        ranges.push(crate::authorship::authorship_log::LineRange::Single(
                            range_start,
                        ));
                    } else {
                        ranges.push(crate::authorship::authorship_log::LineRange::Range(
                            range_start,
                            range_end,
                        ));
                    }

                    let entry =
                        crate::authorship::authorship_log_serialization::AttestationEntry::new(
                            author_id, ranges,
                        );

                    let file_attestation = authorship_log.get_or_create_file(&nfc_file_path);
                    file_attestation.add_entry(entry);
                }
            }
        }

        // Remove INITIAL-only prompts without committed lines (same logic as the
        // primary method — see comment there).
        if !self.initial_only_prompt_ids.is_empty() {
            let committed_prompt_ids: std::collections::HashSet<&String> = authorship_log
                .attestations
                .iter()
                .flat_map(|file_att| file_att.entries.iter())
                .map(|entry| &entry.hash)
                .collect();
            authorship_log.metadata.prompts.retain(|prompt_id, _| {
                !self.initial_only_prompt_ids.contains(prompt_id)
                    || committed_prompt_ids.contains(prompt_id)
            });
        }

        Ok(authorship_log)
    }

    /// Convert all current AI attributions into INITIAL without consulting the live worktree.
    pub fn to_initial_working_log_only(&self) -> crate::git::repo_storage::InitialAttributions {
        let mut initial_files: HashMap<String, Vec<LineAttribution>> = HashMap::new();
        let mut referenced_prompts = HashSet::new();

        for (file_path, (_, line_attrs)) in &self.attributions {
            let filtered: Vec<LineAttribution> = line_attrs
                .iter()
                .filter(|attr| attr.author_id != CheckpointKind::Human.to_str())
                .cloned()
                .collect();
            if filtered.is_empty() {
                continue;
            }
            for attr in &filtered {
                referenced_prompts.insert(attr.author_id.clone());
            }
            initial_files.insert(file_path.clone(), filtered);
        }

        let mut initial_prompts = HashMap::new();
        for prompt_id in &referenced_prompts {
            if let Some(commits) = self.prompts.get(prompt_id)
                && let Some(prompt) = commits.values().next()
            {
                initial_prompts.insert(prompt_id.clone(), prompt.clone());
            }
        }

        // Collect h_ human records referenced by retained attributions
        let mut initial_humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        for author_id in &referenced_prompts {
            if author_id.starts_with("h_")
                && let Some(record) = self.humans.get(author_id)
            {
                initial_humans.insert(author_id.clone(), record.clone());
            }
        }

        // Collect s_ session records referenced by retained attributions
        let mut initial_sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
        for author_id in &referenced_prompts {
            if author_id.starts_with("s_") {
                let session_key = author_id
                    .split("::")
                    .next()
                    .unwrap_or(author_id)
                    .to_string();
                if let Some(record) = self.sessions.get(&session_key) {
                    initial_sessions.insert(session_key, record.clone());
                }
            }
        }

        crate::git::repo_storage::InitialAttributions {
            files: initial_files,
            prompts: initial_prompts,
            file_blobs: HashMap::new(),
            humans: initial_humans,
            sessions: initial_sessions,
        }
    }

    /// Union-merge two human records maps.
    /// Because records are keyed by content-hash of the author identity, any value
    /// for a given key is semantically equivalent. Simple `b`-wins extension is safe.
    fn merge_humans(
        a: &BTreeMap<String, HumanRecord>,
        b: &BTreeMap<String, HumanRecord>,
    ) -> BTreeMap<String, HumanRecord> {
        let mut result = a.clone();
        result.extend(b.iter().map(|(k, v)| (k.clone(), v.clone())));
        result
    }

    /// Calculate and update prompt metrics (accepted_lines, overridden_lines, total_additions, total_deletions)
    pub fn calculate_and_update_prompt_metrics(
        prompts: &mut BTreeMap<String, BTreeMap<String, PromptRecord>>,
        attributions: &HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)>,
        session_additions: &HashMap<String, u32>,
        session_deletions: &HashMap<String, u32>,
    ) {
        use std::collections::HashSet;

        // Collect all line attributions
        let all_line_attributions: Vec<&LineAttribution> = attributions
            .values()
            .flat_map(|(_, line_attrs)| line_attrs.iter())
            .collect();

        // Calculate accepted_lines: count lines in final attributions per session
        let mut session_accepted_lines: HashMap<String, u32> = HashMap::new();
        for (_char_attrs, line_attrs) in attributions.values() {
            for line_attr in line_attrs {
                // Skip human attributions - we only track AI prompt metrics
                if line_attr.author_id == CheckpointKind::Human.to_str() {
                    continue;
                }

                let line_count = line_attr.end_line - line_attr.start_line + 1;
                *session_accepted_lines
                    .entry(line_attr.author_id.clone())
                    .or_insert(0) += line_count;
            }
        }

        // Calculate overridden_lines: count lines where overrode field matches session_id
        // NOTE: We intentionally include human attributions here because when a human
        // overrides an AI line, the attribution has author_id="human" and overrode="ai_prompt_id"
        let mut session_overridden_lines: HashMap<String, u32> = HashMap::new();
        for line_attr in &all_line_attributions {
            if let Some(overrode_id) = &line_attr.overrode {
                let mut overridden_lines: HashSet<u32> = HashSet::new();
                for line in line_attr.start_line..=line_attr.end_line {
                    overridden_lines.insert(line);
                }
                *session_overridden_lines
                    .entry(overrode_id.clone())
                    .or_insert(0) += overridden_lines.len() as u32;
            }
        }

        // Update all prompt records with calculated metrics
        for (session_id, commits) in prompts.iter_mut() {
            for prompt_record in commits.values_mut() {
                prompt_record.total_additions = *session_additions.get(session_id).unwrap_or(&0);
                prompt_record.total_deletions = *session_deletions.get(session_id).unwrap_or(&0);
                prompt_record.accepted_lines =
                    *session_accepted_lines.get(session_id).unwrap_or(&0);
                prompt_record.overriden_lines =
                    *session_overridden_lines.get(session_id).unwrap_or(&0);
            }
        }
    }

    /// Filter prompts and attributions to only include those from specific commits
    /// This is useful for range analysis where we only want to count AI contributions
    /// from commits within the range, not from before
    pub fn filter_to_commits(&mut self, commit_shas: &HashSet<String>) {
        // Capture original AI prompt IDs before filtering
        let original_prompt_ids: HashSet<String> = self.prompts.keys().cloned().collect();

        // Filter prompts to only include those from the specified commits
        let mut filtered_prompts = BTreeMap::new();

        for (prompt_id, commits_map) in &self.prompts {
            let filtered_commits: BTreeMap<String, PromptRecord> = commits_map
                .iter()
                .filter(|(commit_sha, _)| commit_shas.contains(*commit_sha))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            if !filtered_commits.is_empty() {
                filtered_prompts.insert(prompt_id.clone(), filtered_commits);
            }
        }

        self.prompts = filtered_prompts;

        // Get set of valid prompt IDs after filtering
        let valid_prompt_ids: HashSet<String> = self.prompts.keys().cloned().collect();

        // Remove attributions that reference filtered-out prompts
        for (_file_path, (char_attrs, _line_attrs)) in self.attributions.iter_mut() {
            char_attrs.retain(|attr| {
                // Keep human attributions (not in original prompts at all)
                // OR keep AI attributions that are still valid after filtering
                !original_prompt_ids.contains(&attr.author_id)
                    || valid_prompt_ids.contains(&attr.author_id)
            });
        }

        // Recalculate line attributions for all files
        for (file_path, (char_attrs, line_attrs)) in self.attributions.iter_mut() {
            let file_content = self
                .file_contents
                .get(file_path)
                .cloned()
                .unwrap_or_default();
            *line_attrs = crate::authorship::attribution_tracker::attributions_to_line_attributions(
                char_attrs,
                &file_content,
            );
        }
    }
}
/// Merge two VirtualAttributions, favoring the primary for overlaps
pub fn merge_attributions_favoring_first(
    primary: VirtualAttributions,
    secondary: VirtualAttributions,
    final_state: HashMap<String, String>,
) -> Result<VirtualAttributions, GitAiError> {
    use crate::authorship::attribution_tracker::AttributionTracker;

    let tracker = AttributionTracker::new();
    let ts = primary.ts;
    let repo = primary.repo.clone();
    let base_commit = primary.base_commit.clone();

    // Merge prompts from both VAs (primary wins on conflict)
    let mut merged_prompts = secondary.prompts.clone();
    for (id, commits) in &primary.prompts {
        merged_prompts.insert(id.clone(), commits.clone());
    }

    // Merge humans from both VAs
    let merged_humans = VirtualAttributions::merge_humans(&primary.humans, &secondary.humans);

    // Merge sessions from both VAs (primary wins on conflict)
    let mut merged_sessions = secondary.sessions.clone();
    for (id, record) in &primary.sessions {
        merged_sessions.insert(id.clone(), record.clone());
    }

    let mut merged = VirtualAttributions {
        repo,
        base_commit,
        attributions: HashMap::new(),
        file_contents: HashMap::new(),
        prompts: merged_prompts,
        ts,
        blame_start_commit: None,
        humans: merged_humans,
        initial_only_prompt_ids: HashSet::new(),
        sessions: merged_sessions,
    };

    // Get union of all files
    let mut all_files: std::collections::HashSet<String> =
        primary.attributions.keys().cloned().collect();
    all_files.extend(secondary.attributions.keys().cloned());
    all_files.extend(final_state.keys().cloned());

    for file_path in all_files {
        let final_content = match final_state.get(&file_path) {
            Some(content) => content,
            None => continue, // Skip files not in final state
        };

        // Get attributions from both sources
        let primary_attrs = primary.get_char_attributions(&file_path);
        let secondary_attrs = secondary.get_char_attributions(&file_path);

        // Get source content from both
        let primary_content = primary.get_file_content(&file_path);
        let secondary_content = secondary.get_file_content(&file_path);

        // Transform both to final state
        let transformed_primary =
            if let (Some(attrs), Some(content)) = (primary_attrs, primary_content) {
                transform_attributions_to_final(&tracker, content, attrs, final_content, ts)?
            } else {
                Vec::new()
            };

        let transformed_secondary =
            if let (Some(attrs), Some(content)) = (secondary_attrs, secondary_content) {
                transform_attributions_to_final(&tracker, content, attrs, final_content, ts)?
            } else {
                Vec::new()
            };

        // Merge: primary wins overlaps, secondary fills gaps
        let merged_char_attrs =
            merge_char_attributions(&transformed_primary, &transformed_secondary, final_content);

        // Convert to line attributions
        let merged_line_attrs =
            crate::authorship::attribution_tracker::attributions_to_line_attributions(
                &merged_char_attrs,
                final_content,
            );

        merged
            .attributions
            .insert(file_path.clone(), (merged_char_attrs, merged_line_attrs));
        merged
            .file_contents
            .insert(file_path, final_content.clone());
    }

    // Save total_additions and total_deletions by summing across sources so squash/rebase preserves totals.
    let mut saved_totals: HashMap<String, (u32, u32)> = HashMap::new();
    for source in [&primary.prompts, &secondary.prompts] {
        for (prompt_id, commits) in source {
            for prompt_record in commits.values() {
                let entry = saved_totals.entry(prompt_id.clone()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(prompt_record.total_additions);
                entry.1 = entry.1.saturating_add(prompt_record.total_deletions);
            }
        }
    }

    // Calculate and update prompt metrics (will set accepted_lines and overridden_lines)
    VirtualAttributions::calculate_and_update_prompt_metrics(
        &mut merged.prompts,
        &merged.attributions,
        &HashMap::new(), // Empty - will result in total_additions = 0
        &HashMap::new(), // Empty - will result in total_deletions = 0
    );

    // Restore the saved total_additions and total_deletions
    for (prompt_id, commits) in merged.prompts.iter_mut() {
        if let Some(&(additions, deletions)) = saved_totals.get(prompt_id) {
            for prompt_record in commits.values_mut() {
                prompt_record.total_additions = additions;
                prompt_record.total_deletions = deletions;
            }
        }
    }

    Ok(merged)
}

/// Check whether a file's content contains git conflict markers.
///
/// Requires both an opening `<<<<<<<` and a closing `>>>>>>>` marker to avoid
/// false positives on files that happen to contain `=======` (e.g. Markdown
/// setext headings).
pub fn content_has_conflict_markers(content: &str) -> bool {
    let mut has_open = false;
    let mut has_close = false;
    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            has_open = true;
        } else if line.starts_with(">>>>>>>") {
            has_close = true;
        }
        if has_open && has_close {
            return true;
        }
    }
    false
}

/// Strip conflict markers from content, keeping the "ours" (local) side.
///
/// For `git checkout --merge` and `git switch --merge`, conflicts are written
/// with the **target branch** content first and the **local working tree** content
/// second:
///
/// ```text
/// <<<<<<< feature       ← theirs (target branch)
/// THEIRS
/// =======
/// AI_CONTENT            ← ours (local working tree / stashed VA)
/// >>>>>>> local
/// ```
///
/// We therefore keep the section **between `=======` and `>>>>>>>`** — that is
/// the local ("ours") content the stashed VA was built from.
///
/// Handles both the standard two-way conflict style and the diff3/zdiff3 style
/// which inserts a `|||||||` base section between the target and `=======`:
///
/// ```text
/// <<<<<<< feature
/// THEIRS
/// ||||||| original      ← base (diff3)
/// SHARED
/// =======
/// AI_CONTENT            ← ours (kept)
/// >>>>>>> local
/// ```
///
/// Also preserves the trailing newline of the original content so byte-level
/// attribution diffing sees the same length as the actual on-disk file.
pub fn strip_conflict_markers_keep_ours(content: &str) -> String {
    let mut result = Vec::new();
    let mut in_conflict = false;
    let mut in_ours = false; // true only while inside the ======= … >>>>>>> section

    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            in_conflict = true;
            in_ours = false; // theirs section starts — skip it
        } else if in_conflict && line.starts_with("|||||||") {
            // diff3: base section — skip
            in_ours = false;
        } else if in_conflict && line.starts_with("=======") {
            // ours (local) section starts — keep from here
            in_ours = true;
        } else if in_conflict && line.starts_with(">>>>>>>") {
            in_conflict = false;
            in_ours = false; // back to normal content
        } else if !in_conflict || in_ours {
            result.push(line);
        }
    }
    let mut out = result.join("\n");
    // Preserve the trailing newline that std::fs::read_to_string typically returns,
    // so the cleaned content has the same byte length as the actual file.
    if content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Transform attributions from old content to new content
fn transform_attributions_to_final(
    tracker: &crate::authorship::attribution_tracker::AttributionTracker,
    old_content: &str,
    old_attributions: &[Attribution],
    new_content: &str,
    ts: u128,
) -> Result<Vec<Attribution>, GitAiError> {
    // Use a dummy author for new insertions (we'll discard them anyway)
    let dummy_author = "__DUMMY__";

    let transformed = tracker.update_attributions(
        old_content,
        new_content,
        old_attributions,
        dummy_author,
        ts,
    )?;

    // Filter out dummy attributions (new insertions)
    let filtered: Vec<Attribution> = transformed
        .into_iter()
        .filter(|attr| attr.author_id != dummy_author)
        .collect();

    Ok(filtered)
}

/// Merge character-level attributions, with primary winning overlaps
fn merge_char_attributions(
    primary: &[Attribution],
    secondary: &[Attribution],
    content: &str,
) -> Vec<Attribution> {
    let content_len = content.len();
    if content_len == 0 {
        return primary.to_vec();
    }

    // Create coverage map for primary (byte-based).
    let mut covered = vec![false; content_len];
    #[allow(clippy::needless_range_loop)]
    for attr in primary {
        for i in attr.start..attr.end.min(content_len) {
            covered[i] = true;
        }
    }

    let mut result = Vec::new();

    // Add all primary attributions.
    result.extend(primary.iter().cloned());

    // Add secondary attributions only where primary doesn't cover, on UTF-8 boundaries.
    for attr in secondary {
        let mut range_start: Option<usize> = None;
        let safe_start = floor_char_boundary(content, attr.start);
        let safe_end = ceil_char_boundary(content, attr.end);

        if safe_start >= safe_end {
            continue;
        }

        let slice = &content[safe_start..safe_end];
        for (rel_idx, ch) in slice.char_indices() {
            let start = safe_start + rel_idx;
            let end = start + ch.len_utf8();
            let mut is_covered = false;
            #[allow(clippy::needless_range_loop)]
            for i in start..end.min(content_len) {
                if covered[i] {
                    is_covered = true;
                    break;
                }
            }

            if is_covered {
                if let Some(range_start_idx) = range_start.take()
                    && range_start_idx < start
                {
                    result.push(Attribution::new(
                        range_start_idx,
                        start,
                        attr.author_id.clone(),
                        attr.ts,
                    ));
                }
            } else if range_start.is_none() {
                range_start = Some(start);
            }
        }

        if let Some(range_start_idx) = range_start.take()
            && range_start_idx < safe_end
        {
            result.push(Attribution::new(
                range_start_idx,
                safe_end,
                attr.author_id.clone(),
                attr.ts,
            ));
        }
    }

    // Sort by start position.
    result.sort_by_key(|a| (a.start, a.end));
    result
}

fn floor_char_boundary(content: &str, idx: usize) -> usize {
    let mut i = idx.min(content.len());
    while i > 0 && !content.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(content: &str, idx: usize) -> usize {
    let mut i = idx.min(content.len());
    while i < content.len() && !content.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Compute attributions for a single file at a specific commit
#[allow(clippy::type_complexity)]
fn compute_attributions_for_file(
    repo: &Repository,
    base_commit: &str,
    file_path: &str,
    ts: u128,
    blame_start_commit: Option<String>,
) -> Result<Option<(String, String, Vec<Attribution>, Vec<LineAttribution>)>, GitAiError> {
    // Set up blame options
    let mut ai_blame_opts = GitAiBlameOptions::default();
    #[allow(clippy::field_reassign_with_default)]
    {
        ai_blame_opts.no_output = true;
        ai_blame_opts.return_human_authors_as_human = true;
        ai_blame_opts.use_prompt_hashes_as_names = true;
        ai_blame_opts.newest_commit = Some(base_commit.to_string());
        ai_blame_opts.oldest_commit = blame_start_commit;
        ai_blame_opts.oldest_date = Some(*OLDEST_AI_BLAME_DATE);
    }

    // Run blame at the base commit
    let ai_blame = repo.blame(file_path, &ai_blame_opts);

    match ai_blame {
        Ok((blames, _)) => {
            // Convert blame results to line attributions
            let mut line_attributions = Vec::new();
            for (line, author) in blames {
                // Skip human-only lines as they don't need tracking
                if author == CheckpointKind::Human.to_str() {
                    continue;
                }
                line_attributions.push(LineAttribution {
                    start_line: line,
                    end_line: line,
                    author_id: author.clone(),
                    overrode: None,
                });
            }

            // Get the file content at this commit to convert to character attributions
            // We need to read the file content that blame operated on
            let file_content = get_file_content_at_commit(repo, base_commit, file_path)?;

            // Convert line attributions to character attributions
            let char_attributions =
                line_attributions_to_attributions(&line_attributions, &file_content, ts);

            Ok(Some((
                file_path.to_string(),
                file_content,
                char_attributions,
                line_attributions,
            )))
        }
        Err(_) => {
            // File doesn't exist at this commit or can't be blamed, skip it
            Ok(None)
        }
    }
}

fn get_file_content_at_commit(
    repo: &Repository,
    commit_sha: &str,
    file_path: &str,
) -> Result<String, GitAiError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let tree = commit.tree()?;

    match tree.get_path(std::path::Path::new(file_path)) {
        Ok(entry) => {
            if let Ok(blob) = repo.find_blob(entry.id()) {
                let blob_content = blob.content().unwrap_or_default();
                Ok(String::from_utf8_lossy(&blob_content).to_string())
            } else {
                Ok(String::new())
            }
        }
        Err(_) => Ok(String::new()),
    }
}

/// Check if a file exists in a commit's tree
fn file_exists_in_commit(
    repo: &Repository,
    commit_sha: &str,
    file_path: &str,
) -> Result<bool, GitAiError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let tree = commit.tree()?;
    if tree.get_path(std::path::Path::new(file_path)).is_ok() {
        return Ok(true);
    }
    // The caller's path may be NFC or NFD while the tree stores the opposite
    // form.  Try both normalisations before giving up.
    if !file_path.is_ascii() {
        let nfc_path: String = file_path.nfc().collect();
        if nfc_path != file_path && tree.get_path(std::path::Path::new(&nfc_path)).is_ok() {
            return Ok(true);
        }
        let nfd_path: String = file_path.nfd().collect();
        if nfd_path != file_path && tree.get_path(std::path::Path::new(&nfd_path)).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn restore_working_log_carryover(
    repo: &Repository,
    old_head: &str,
    new_head: &str,
    final_state: HashMap<String, String>,
    human_author: Option<String>,
) -> Result<(), GitAiError> {
    if old_head.is_empty() || new_head.is_empty() || final_state.is_empty() {
        return Ok(());
    }

    let old_va = VirtualAttributions::from_persisted_working_log(
        repo.clone(),
        old_head.to_string(),
        human_author,
    )?;
    restore_virtual_attribution_carryover(repo, new_head, old_va, final_state)
}

pub fn restore_virtual_attribution_carryover(
    repo: &Repository,
    new_head: &str,
    carried_va: VirtualAttributions,
    final_state: HashMap<String, String>,
) -> Result<(), GitAiError> {
    if new_head.is_empty() || final_state.is_empty() || carried_va.attributions.is_empty() {
        return Ok(());
    }

    let new_va =
        VirtualAttributions::from_persisted_working_log(repo.clone(), new_head.to_string(), None)
            .unwrap_or_else(|_| {
                VirtualAttributions::new(
                    repo.clone(),
                    new_head.to_string(),
                    HashMap::new(),
                    HashMap::new(),
                    0,
                )
            });

    let merged_va = merge_attributions_favoring_first(carried_va, new_va, final_state.clone())?;
    let initial_attributions = merged_va.to_initial_working_log_only();
    if initial_attributions.files.is_empty()
        && initial_attributions.prompts.is_empty()
        && initial_attributions.sessions.is_empty()
    {
        return Ok(());
    }

    let working_log = repo.storage.working_log_for_base_commit(new_head)?;
    working_log.write_initial_attributions_with_contents(
        initial_attributions.files,
        initial_attributions.prompts,
        initial_attributions.humans,
        final_state,
        initial_attributions.sessions,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_merge_rebased_content_preserves_clean_local_hunk_on_target_edit() {
        let base = "one\ntwo\n";
        let target = "one feature\ntwo\n";
        let observed = "one\ntwo ai\n";

        assert_eq!(
            checkout_merge_rebased_content(base, target, observed),
            "one feature\ntwo ai\n"
        );
    }

    #[test]
    fn checkout_merge_rebased_content_maps_eof_newline_only_target_line() {
        let base = "one\ntwo";
        let target = "one feature\ntwo\n";
        let observed = "one\ntwo ai\n";

        assert_eq!(
            checkout_merge_rebased_content(base, target, observed),
            "one feature\ntwo ai\n"
        );
    }

    #[test]
    fn checkout_merge_rebased_content_uses_observed_when_target_unchanged() {
        assert_eq!(
            checkout_merge_rebased_content("base\n", "base\n", "ai\n"),
            "ai\n"
        );
    }

    /// Characterization: the in-memory 3-way merge used to build the carryover
    /// snapshot must produce the same result the previous `git merge-file
    /// --theirs -p <committed> <parent> <observed>` spawn produced, so that the
    /// per-file `git merge-file` process can be eliminated. Roles:
    /// base = parent, "ours/current" = committed, "theirs" (favored) = observed.
    #[test]
    fn carryover_merge_non_overlapping_changes_combines_both_sides() {
        // parent has 3 lines; committed edits line 1; observed edits line 3.
        // Non-overlapping edits on each side both survive.
        let parent = "a\nb\nc\n";
        let committed = "A\nb\nc\n";
        let observed = "a\nb\nC\n";
        assert_eq!(
            carryover_merge_content(parent, committed, observed),
            "A\nb\nC\n"
        );
    }

    #[test]
    fn carryover_merge_overlapping_conflict_favors_observed() {
        // Both sides edit the same line differently → `--theirs` keeps observed.
        let parent = "shared\n";
        let committed = "COMMITTED\n";
        let observed = "OBSERVED\n";
        assert_eq!(
            carryover_merge_content(parent, committed, observed),
            "OBSERVED\n"
        );
    }

    #[test]
    fn carryover_merge_committed_only_change_keeps_committed() {
        // observed == parent (no working-tree change) → committed side wins.
        let parent = "a\nb\n";
        let committed = "a\nB\n";
        let observed = "a\nb\n";
        assert_eq!(
            carryover_merge_content(parent, committed, observed),
            "a\nB\n"
        );
    }

    #[test]
    fn carryover_merge_observed_only_change_keeps_observed() {
        // committed == parent (commit didn't touch file) → observed side wins.
        let parent = "a\nb\n";
        let committed = "a\nb\n";
        let observed = "a\nB\n";
        assert_eq!(
            carryover_merge_content(parent, committed, observed),
            "a\nB\n"
        );
    }

    #[test]
    fn carryover_merge_ignores_mixed_eol_when_content_matches_committed() {
        let parent = "base one\nbase two\n";
        let committed = "base one\nbase two\nai line\n";
        let observed = "base one\r\nbase two\r\nai line\n";

        assert_eq!(
            merged_carryover_content_pure(parent, committed, observed),
            committed
        );
        assert!(diff_hunks_between_contents(observed, committed).is_empty());
    }

    #[test]
    fn carryover_merge_does_not_treat_crlf_only_observed_chunk_as_change() {
        let parent = "a\nb\nc\n";
        let committed = "A\nb\nC\n";
        let observed = "a\r\nb\r\nD\r\n";

        assert_eq!(
            carryover_merge_content(parent, committed, observed),
            "A\nb\nD\r\n"
        );
    }

    /// Differential test: the in-memory carryover merge must agree with real
    /// `git merge-file --theirs -p <committed> <parent> <observed>` across many
    /// pseudo-random 3-way inputs that produce a clean (non-conflicting) merge.
    /// (When git emits conflict markers the two are allowed to differ, since the
    /// in-memory version deterministically favors observed; we focus on the
    /// clean cases that dominate real carryover snapshots and assert exact
    /// agreement there.)
    #[test]
    fn carryover_merge_matches_git_merge_file_on_random_clean_merges() {
        fn run_git_merge_file(parent: &str, committed: &str, observed: &str) -> Option<String> {
            let dir = std::env::temp_dir().join(format!(
                "git-ai-mf-difftest-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).ok()?;
            let cp = dir.join("committed");
            let pp = dir.join("parent");
            let op = dir.join("observed");
            std::fs::write(&cp, committed).ok()?;
            std::fs::write(&pp, parent).ok()?;
            std::fs::write(&op, observed).ok()?;
            let output = std::process::Command::new("git")
                .args([
                    "merge-file",
                    "--theirs",
                    "-p",
                    &cp.to_string_lossy(),
                    &pp.to_string_lossy(),
                    &op.to_string_lossy(),
                ])
                .output()
                .ok()?;
            let _ = std::fs::remove_dir_all(&dir);
            // Non-zero with conflict markers → skip (clean merges return 0).
            if !output.status.success() {
                return None;
            }
            Some(String::from_utf8_lossy(&output.stdout).to_string())
        }

        // Deterministic LCG so the test is reproducible without rand.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };

        let mut compared = 0;
        for _ in 0..600 {
            let n = (next() % 6) as usize + 1; // 1..=6 base lines
            let base: Vec<String> = (0..n).map(|i| format!("line{i}\n")).collect();
            // Each side independently keeps / edits / deletes each base line, and
            // may append a tail line.
            let mutate = |seed: &mut dyn FnMut() -> u32| -> String {
                let mut out = String::new();
                for (i, line) in base.iter().enumerate() {
                    match seed() % 4 {
                        0 => out.push_str(line),                                 // keep
                        1 => out.push_str(&format!("edit{i}_{}\n", seed() % 3)), // edit
                        2 => {}                                                  // delete
                        _ => out.push_str(line),                                 // keep
                    }
                }
                if seed().is_multiple_of(3) {
                    out.push_str("tail\n");
                }
                out
            };
            let parent: String = base.concat();
            let committed = mutate(&mut next);
            let observed = mutate(&mut next);

            if let Some(git_result) = run_git_merge_file(&parent, &committed, &observed) {
                let ours = carryover_merge_content(&parent, &committed, &observed);
                assert_eq!(
                    ours, git_result,
                    "in-memory carryover merge diverged from git merge-file (clean merge)\nparent={parent:?}\ncommitted={committed:?}\nobserved={observed:?}"
                );
                compared += 1;
            }
        }
        assert!(
            compared > 50,
            "expected to compare a meaningful number of clean merges, got {compared}"
        );
    }

    #[test]
    fn checkout_merge_rebased_content_preserves_local_side_for_overlapping_conflict() {
        assert_eq!(
            checkout_merge_rebased_content("shared\n", "THEIRS\n", "AI_CONTENT\n"),
            "AI_CONTENT\n"
        );
    }

    /// Regression (#11): the attestation emit order must be deterministic.
    /// `attributions` is a HashMap and per-file entries are grouped in a
    /// HashMap<author_id, ...>, so naive iteration emits files and entries in a
    /// process-randomised order, making byte-identical commits produce
    /// different note bytes. build_attestations_from_attributions must sort
    /// files by path and entries by hash.
    #[test]
    fn test_build_attestations_is_deterministically_sorted() {
        // Many files + many authors per file so that, were the order taken from
        // HashMap iteration, it would be astronomically unlikely to already be
        // sorted at both levels.
        let mut attributions: HashMap<String, (Vec<Attribution>, Vec<LineAttribution>)> =
            HashMap::new();
        let files = [
            "zeta.rs", "mid.rs", "alpha.rs", "beta.rs", "yarn.rs", "delta.rs", "gamma.rs",
            "omega.rs",
        ];
        let authors = [
            "s_zzz", "h_aaa", "s_mmm", "h_qqq", "s_bbb", "h_ttt", "s_ddd",
        ];
        for (fi, file) in files.iter().enumerate() {
            let mut line_attrs = Vec::new();
            for (ai, author) in authors.iter().enumerate() {
                let line = (fi * authors.len() + ai + 1) as u32;
                line_attrs.push(LineAttribution::new(line, line, author.to_string(), None));
            }
            attributions.insert(file.to_string(), (Vec::new(), line_attrs));
        }

        let result = build_attestations_from_attributions(&attributions);

        // Files are sorted by path.
        let got_files: Vec<&str> = result.iter().map(|f| f.file_path.as_str()).collect();
        let mut want_files = got_files.clone();
        want_files.sort_unstable();
        assert_eq!(got_files, want_files, "files must be sorted by path");

        // Entries within each file are sorted by hash.
        for fa in &result {
            let got: Vec<&str> = fa.entries.iter().map(|e| e.hash.as_str()).collect();
            let mut want = got.clone();
            want.sort_unstable();
            assert_eq!(
                got, want,
                "entries in {} must be sorted by hash",
                fa.file_path
            );
        }

        // And the whole thing is stable across repeated builds.
        let again = build_attestations_from_attributions(&attributions);
        assert_eq!(result, again, "output must be stable across builds");
    }
}
