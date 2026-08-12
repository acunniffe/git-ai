use super::*;

impl TestRepo {
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn canonical_path(&self) -> PathBuf {
        self.path
            .canonicalize()
            .expect("failed to canonicalize test repo path")
    }

    /// Write raw file contents into the repo, creating parent directories as
    /// needed. Plain `fs::write` — no checkpoint side effects.
    pub fn write_file(&self, rel: &str, contents: &str) {
        let abs = self.path.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("parent directory should be creatable");
        }
        fs::write(&abs, contents).expect("file write should succeed");
    }

    /// Scenario builder for the real AI-agent checkpoint flow on a single
    /// file: write `pre_contents`, fire the file-scoped "human" pre-edit
    /// checkpoint (the legacy/untracked checkpoint every AI preset fires
    /// before it edits, so any changes made by something else are excluded),
    /// then write `post_contents` and fire the file-scoped "mock_ai"
    /// post-edit checkpoint. This is exactly the pre/post pattern documented
    /// in CLAUDE.md and used by the real agent presets.
    ///
    /// Checkpoint-NUANCE tests — ordering, partial staging, unscoped
    /// checkpoints, or assertions interleaved between the pre and post
    /// steps — must keep writing the manual `fs::write` + `git_ai(&["checkpoint", ...])`
    /// sequence themselves; this helper intentionally hides those steps.
    pub fn ai_edit(&self, rel: &str, pre_contents: &str, post_contents: &str) {
        self.write_file(rel, pre_contents);
        self.git_ai(&["checkpoint", "human", rel])
            .expect("pre-edit human checkpoint should succeed");
        self.write_file(rel, post_contents);
        self.git_ai(&["checkpoint", "mock_ai", rel])
            .expect("post-edit mock_ai checkpoint should succeed");
    }

    /// Scenario builder for the real known-human checkpoint flow: write
    /// `contents`, then fire the file-scoped "mock_known_human" checkpoint.
    /// Mirrors what our IDE/editor extensions do when they detect an actual
    /// human keystroke, as opposed to the legacy/untracked "human" checkpoint.
    ///
    /// Checkpoint-NUANCE tests must keep writing the manual sequence
    /// themselves; this helper intentionally hides those steps.
    pub fn human_edit(&self, rel: &str, contents: &str) {
        self.write_file(rel, contents);
        self.git_ai(&["checkpoint", "mock_known_human", rel])
            .expect("known-human checkpoint should succeed");
    }

    /// Scenario builder for an untracked edit: write `contents` with no
    /// checkpoint call at all. Identical to `write_file` — this alias exists
    /// so call sites read as the matched `ai_edit`/`human_edit`/`untracked_edit`
    /// trio rather than mixing a differently-named primitive in.
    pub fn untracked_edit(&self, rel: &str, contents: &str) {
        self.write_file(rel, contents);
    }

    pub fn test_db_path(&self) -> &PathBuf {
        &self.test_db_path
    }

    pub fn test_home_path(&self) -> &PathBuf {
        &self.test_home
    }

    pub(super) fn has_active_daemon(&self) -> bool {
        self.daemon_process.is_some()
    }

    pub fn sync_daemon(&self) {
        self.sync_daemon_force();
    }

    pub fn stats(&self) -> Result<CommitStats, String> {
        let output = self.git_ai(&["stats", "--json"])?;
        let start = output
            .find('{')
            .ok_or_else(|| format!("stats output does not contain JSON: {}", output))?;

        let mut depth = 0usize;
        let mut end_index = None;
        for (offset, ch) in output[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    if depth == 0 {
                        return Err(format!("malformed stats JSON output: {}", output));
                    }
                    depth -= 1;
                    if depth == 0 {
                        end_index = Some(start + offset);
                        break;
                    }
                }
                _ => {}
            }
        }

        let end_index =
            end_index.ok_or_else(|| format!("incomplete stats JSON output: {}", output))?;
        let json = &output[start..=end_index];
        let stats: CommitStats =
            serde_json::from_str(json).map_err(|e| format!("invalid stats JSON: {}", e))?;
        Ok(stats)
    }
    pub fn filename(&self, filename: &str) -> TestFile<'_> {
        let file_path = self.path.join(filename);

        // If file exists, populate from existing file with blame
        if file_path.exists() {
            TestFile::from_existing_file(file_path, self)
        } else {
            // New file, start with empty lines
            TestFile::new_with_filename(file_path, vec![], self)
        }
    }

    pub fn current_working_logs(&self) -> PersistedWorkingLog {
        self.sync_daemon_force();

        let repo = GitAiRepository::find_repository_in_path(self.path.to_str().unwrap())
            .expect("Failed to find repository");

        // Get the current HEAD commit SHA, or use "initial" for empty repos
        let commit_sha = repo
            .head()
            .ok()
            .and_then(|head| head.target().ok())
            .unwrap_or_else(|| "initial".to_string());

        // Get the working log for the current HEAD commit
        repo.storage
            .working_log_for_base_commit(&commit_sha)
            .unwrap()
    }

    pub fn read_authorship_note(&self, commit_sha: &str) -> Option<String> {
        self.git(&["notes", "--ref=ai", "show", commit_sha])
            .ok()
            .filter(|note| !note.trim().is_empty())
    }

    pub fn require_authorship_log(&self, commit_sha: &str) -> AuthorshipLog {
        let note = self
            .read_authorship_note(commit_sha)
            .unwrap_or_else(|| panic!("commit {commit_sha} should have an authorship note"));

        AuthorshipLog::deserialize_from_string(&note).unwrap_or_else(|error| {
            panic!("failed to parse authorship note for commit {commit_sha}: {error}")
        })
    }

    pub fn read_authorship_note_in_git_dir(
        &self,
        git_dir: &Path,
        commit_sha: &str,
    ) -> Option<String> {
        self.sync_daemon_force();

        let mut command = Command::new(real_git_executable());
        configure_test_home_env(&mut command, &self.test_home);
        command.args([
            "--git-dir",
            git_dir.to_str().expect("valid git dir"),
            "--no-pager",
            "notes",
            "--ref=ai",
            "show",
            commit_sha,
        ]);

        let output = run_command_output(&mut command, "git notes show in git dir")
            .expect("failed to run git notes show in git dir");

        if !output.status.success() {
            return None;
        }

        let note = String::from_utf8_lossy(&output.stdout).to_string();
        if note.trim().is_empty() {
            None
        } else {
            Some(note)
        }
    }

    pub fn commit(&self, message: &str) -> Result<NewCommit, String> {
        self.commit_with_env(message, &[], None)
    }

    /// Commit from a working directory (without using -C flag)
    /// This tests that git-ai correctly handles commits when run from a subdirectory
    /// The working_dir will be canonicalized to ensure it's an absolute path
    pub fn commit_from_working_dir(
        &self,
        working_dir: &std::path::Path,
        message: &str,
    ) -> Result<NewCommit, String> {
        self.commit_with_env(message, &[], Some(working_dir))
    }

    pub fn stage_all_and_commit(&self, message: &str) -> Result<NewCommit, String> {
        self.git(&["add", "-A"]).expect("add --all should succeed");
        self.commit(message)
    }

    pub fn stage_all_and_commit_with_env(
        &self,
        message: &str,
        envs: &[(&str, &str)],
    ) -> Result<NewCommit, String> {
        self.git(&["add", "-A"]).expect("add --all should succeed");
        self.commit_with_env(message, envs, None)
    }

    /// After `sync_daemon_force()` has drained this commit's completion
    /// session, inspect the completion log entries appended since `baseline`
    /// for the one that classified this `git commit` invocation. If the
    /// daemon's own analyzer recorded no HEAD-transition event for it (the
    /// reflog-cursor race: `RefCursor` enrichment lost the race with git's
    /// own reflog append, so `HistoryAnalyzer` saw empty ref changes and fell
    /// back to `OpaqueCommand`), fail immediately with that classification
    /// instead of falling through to the generic fs-visibility retry below --
    /// no amount of retrying makes a note appear for a commit the daemon
    /// never tried to process. `semantic_events` is only populated by daemon
    /// binaries built after this diagnostic was added (see
    /// `TestCompletionLogEntry` in `daemon_config.rs`); an empty
    /// `semantic_events` on a matched entry means we can't tell either way,
    /// so we conservatively fall through to the retry rather than risk a
    /// false positive against an older daemon in the shared test pool.
    pub(super) fn fail_fast_on_opaque_commit_completion(
        &self,
        head_commit: &str,
        baseline: usize,
    ) -> Result<(), String> {
        commit_completion_diagnostic(&self.daemon_completion_entries(), head_commit, baseline)
            .map_err(|error| {
                format!(
                    "{error} (worktree: {}, daemon family: {})",
                    self.path.display(),
                    self.daemon_family_key()
                )
            })
    }

    /// Preserve the daemon completion log when a commit has no authorship
    /// note. CI can set `GIT_AI_TEST_ARTIFACT_DIR` to retain this evidence;
    /// local runs remain unchanged when the variable is absent.
    pub(super) fn maybe_save_daemon_completion_artifact(&self, head_commit: &str) {
        let Some(artifact_dir) = std::env::var_os("GIT_AI_TEST_ARTIFACT_DIR") else {
            return;
        };
        let artifact_dir = PathBuf::from(artifact_dir);
        if fs::create_dir_all(&artifact_dir).is_err() {
            return;
        }
        let family_key = self.daemon_family_key();
        let source = self.daemon_completion_log_path_for_family(&family_key);
        let destination = artifact_dir.join(format!("daemon-completion-{head_commit}.jsonl"));
        let _ = fs::copy(source, destination);
    }

    pub fn commit_with_env(
        &self,
        message: &str,
        envs: &[(&str, &str)],
        working_dir: Option<&std::path::Path>,
    ) -> Result<NewCommit, String> {
        // A previous raw-git mutation (for example `git commit --amend`) may
        // have already returned while its daemon side effects are still
        // queued. Drain those effects before starting the next commit so its
        // authorship note cannot race the previous note migration.
        self.sync_daemon_force();
        let completion_baseline = self.daemon_completion_entries().len();
        let output = self.git_with_env(&["commit", "-m", message], envs, working_dir);

        // println!("commit output: {:?}", output);
        match output {
            Ok(combined) => {
                // Get the repository and HEAD commit SHA
                let repo = GitAiRepository::find_repository_in_path(self.path.to_str().unwrap())
                    .map_err(|e| format!("Failed to find repository: {}", e))?;

                let head_commit = repo
                    .head()
                    .map_err(|e| format!("Failed to get HEAD: {}", e))?
                    .target()
                    .map_err(|e| format!("Failed to get HEAD target: {}", e))?;

                self.sync_daemon_force();

                if self.has_active_daemon()
                    && let Err(error) = self
                        .fail_fast_on_opaque_commit_completion(&head_commit, completion_baseline)
                {
                    self.maybe_save_daemon_completion_artifact(&head_commit);
                    return Err(error);
                }

                // In daemon mode, the authorship note may not be immediately
                // visible after the session completes due to filesystem flush
                // timing. Use bounded backoff before failing; the completion
                // session has already established that note generation ran.
                let mut content =
                    git_ai::operations::git::notes_api::read_note(&repo, &head_commit);
                if content.is_none() {
                    for delay_ms in [50, 100, 200, 400, 800] {
                        thread::sleep(Duration::from_millis(delay_ms));
                        content =
                            git_ai::operations::git::notes_api::read_note(&repo, &head_commit);
                        if content.is_some() {
                            break;
                        }
                    }
                }
                let content = match content {
                    Some(content) => content,
                    None => {
                        self.maybe_save_daemon_completion_artifact(&head_commit);
                        return Err(format!(
                            "No authorship log found for new commit {} after daemon sync",
                            head_commit
                        ));
                    }
                };
                let authorship_log = AuthorshipLog::deserialize_from_string(&content)
                    .map_err(|e| format!("Failed to parse authorship log: {}", e))?;

                Ok(NewCommit {
                    commit_sha: head_commit,
                    authorship_log,
                    stdout: combined,
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_file(&self, filename: &str) -> Option<String> {
        let file_path = self.path.join(filename);
        fs::read_to_string(&file_path).ok()
    }
}

#[derive(Debug)]
pub struct NewCommit {
    pub authorship_log: AuthorshipLog,
    pub stdout: String,
    pub commit_sha: String,
}

impl NewCommit {
    pub fn assert_authorship_snapshot(&self) {
        assert_debug_snapshot!(self.authorship_log);
    }
    pub fn print_authorship(&self) {
        // Debug method to print authorship log
        println!("{}", self.authorship_log.serialize_to_string().unwrap());
    }
}
