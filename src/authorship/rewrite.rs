use std::collections::{HashMap, HashSet};

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::hunk_shift::{DiffHunk, parse_hunk_header};
use crate::config::Config;
use crate::error::GitAiError;
use crate::git::notes_api;
use crate::git::repo_state::is_valid_git_oid;
use crate::git::repository::{
    Repository, exec_git, exec_git_allow_nonzero, exec_git_stdin_streaming,
};

const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug)]
pub enum RewriteEvent {
    NonFastForward {
        old_tip: String,
        new_tip: String,
        onto: Option<String>,
    },
    CherryPickComplete {
        sources: Vec<String>,
        new_commits: Vec<String>,
    },
    SquashMerge {
        source_head: String,
        squash_commit: String,
        onto: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiffTreeResult {
    pub hunks_by_file: HashMap<String, Vec<DiffHunk>>,
    pub added_lines_by_file: HashMap<String, Vec<u32>>,
    pub renames: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RewriteMetricOperation {
    Rebase,
    SquashMerge,
    CherryPick,
    CherryPickNoCommit,
    Amend,
    Revert,
    UpdateRef,
    NonFastForward,
}

impl RewriteMetricOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::SquashMerge => "squash_merge",
            Self::CherryPick => "cherry_pick",
            Self::CherryPickNoCommit => "cherry_pick_no_commit",
            Self::Amend => "amend",
            Self::Revert => "revert",
            Self::UpdateRef => "update_ref",
            Self::NonFastForward => "non_fast_forward",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RewriteMetricCommit {
    pub new_sha: String,
    pub original_shas: Vec<String>,
    pub operation: RewriteMetricOperation,
    pub branch: Option<String>,
    pub parent_sha: Option<String>,
    pub authorship_note: Option<String>,
    pub parent_diff: Option<DiffTreeResult>,
}

impl RewriteMetricCommit {
    pub(crate) fn new(
        new_sha: impl Into<String>,
        original_shas: Vec<String>,
        operation: RewriteMetricOperation,
    ) -> Self {
        let mut deduped = Vec::with_capacity(original_shas.len());
        for sha in original_shas {
            if !sha.is_empty() && !deduped.contains(&sha) {
                deduped.push(sha);
            }
        }
        Self {
            new_sha: new_sha.into(),
            original_shas: deduped,
            operation,
            branch: None,
            parent_sha: None,
            authorship_note: None,
            parent_diff: None,
        }
    }

    pub(crate) fn with_branch(mut self, branch: impl Into<String>) -> Self {
        let branch = branch.into();
        if !branch.is_empty() {
            self.branch = Some(branch);
        }
        self
    }

    pub(crate) fn with_parent_sha(mut self, parent_sha: impl Into<String>) -> Self {
        let parent_sha = parent_sha.into();
        if !parent_sha.is_empty() {
            self.parent_sha = Some(parent_sha);
        }
        self
    }

    pub(crate) fn with_authorship_note(mut self, authorship_note: impl Into<String>) -> Self {
        self.authorship_note = Some(authorship_note.into());
        self
    }

    pub(crate) fn with_parent_diff(mut self, parent_diff: DiffTreeResult) -> Self {
        self.parent_diff = Some(parent_diff);
        self
    }
}

pub(crate) fn branch_name_from_ref(reference: &str) -> Option<String> {
    reference
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RewriteOutcome {
    pub(crate) metric_commits: Vec<RewriteMetricCommit>,
}

impl RewriteOutcome {
    fn empty() -> Self {
        Self::default()
    }

    fn from_metric_commits(metric_commits: Vec<RewriteMetricCommit>) -> Self {
        Self { metric_commits }
    }
}

pub(crate) fn rewrite_metrics_enabled() -> bool {
    Config::get().get_feature_flags().rewrite_metrics_events
}

pub(crate) fn metric_commits_from_mappings(
    mappings: &[(String, String)],
    operation: RewriteMetricOperation,
) -> Vec<RewriteMetricCommit> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    for (source_sha, new_sha) in mappings {
        if source_sha.is_empty() || new_sha.is_empty() {
            continue;
        }
        if !seen_pairs.insert((new_sha.clone(), source_sha.clone())) {
            continue;
        }
        if !grouped.contains_key(new_sha) {
            order.push(new_sha.clone());
        }
        grouped
            .entry(new_sha.clone())
            .or_default()
            .push(source_sha.clone());
    }

    order
        .into_iter()
        .filter_map(|new_sha| {
            grouped
                .remove(&new_sha)
                .map(|original_shas| RewriteMetricCommit::new(new_sha, original_shas, operation))
        })
        .collect()
}

fn attach_authorship_notes(
    metric_commits: Vec<RewriteMetricCommit>,
    notes: Vec<(String, String)>,
) -> Vec<RewriteMetricCommit> {
    if notes.is_empty() {
        return metric_commits;
    }
    let mut notes_by_commit: HashMap<String, String> = notes.into_iter().collect();
    metric_commits
        .into_iter()
        .map(|mut commit| {
            if let Some(note) = notes_by_commit.remove(&commit.new_sha) {
                commit = commit.with_authorship_note(note);
            }
            commit
        })
        .collect()
}

fn attach_authorship_note(
    mut metric_commit: RewriteMetricCommit,
    note: Option<String>,
) -> RewriteMetricCommit {
    if let Some(note) = note {
        metric_commit = metric_commit.with_authorship_note(note);
    }
    metric_commit
}

fn write_authorship_log_for_metrics(
    repo: &Repository,
    commit_sha: &str,
    log: &AuthorshipLog,
) -> Result<Option<String>, GitAiError> {
    let serialized = write_authorship_log(repo, commit_sha, log)?;
    if rewrite_metrics_enabled() {
        Ok(Some(serialized))
    } else {
        Ok(None)
    }
}

fn post_squash_metric_note_from_result(
    result: crate::authorship::post_commit::PostCommitDetailedResult,
) -> Option<String> {
    if rewrite_metrics_enabled() {
        Some(result.authorship_note)
    } else {
        None
    }
}

fn empty_tree_sha() -> &'static str {
    EMPTY_TREE_SHA
}

fn tree_revision_arg(sha: &str) -> Option<String> {
    if sha == "initial" {
        None
    } else {
        Some(format!("{}^{{tree}}", sha))
    }
}

fn insert_known_tree(sha_to_tree: &mut HashMap<String, String>, sha: &str) -> bool {
    if sha == "initial" {
        sha_to_tree.insert(sha.to_string(), empty_tree_sha().to_string());
        true
    } else {
        false
    }
}

fn unique_pair_shas(pairs: &[(String, String)]) -> Vec<String> {
    let mut unique_shas = Vec::new();
    let mut seen = HashSet::new();
    for (src, dst) in pairs {
        if seen.insert(src.clone()) {
            unique_shas.push(src.clone());
        }
        if seen.insert(dst.clone()) {
            unique_shas.push(dst.clone());
        }
    }
    unique_shas
}

fn resolve_tree_shas(
    repo: &Repository,
    unique_shas: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let mut sha_to_tree = HashMap::new();
    let mut shas_to_resolve = Vec::new();

    for sha in unique_shas {
        if !insert_known_tree(&mut sha_to_tree, sha) {
            shas_to_resolve.push(sha.clone());
        }
    }

    if shas_to_resolve.is_empty() {
        return Ok(sha_to_tree);
    }

    let mut rev_parse_args = repo.global_args_for_exec();
    rev_parse_args.push("rev-parse".to_string());
    for sha in &shas_to_resolve {
        if let Some(arg) = tree_revision_arg(sha) {
            rev_parse_args.push(arg);
        }
    }
    let rev_output = exec_git(&rev_parse_args)?;
    let rev_stdout = String::from_utf8_lossy(&rev_output.stdout);
    let tree_shas: Vec<&str> = rev_stdout.lines().collect();

    if tree_shas.len() != shas_to_resolve.len() {
        return Err(GitAiError::Generic(format!(
            "rev-parse returned {} trees for {} commits",
            tree_shas.len(),
            shas_to_resolve.len()
        )));
    }

    for (commit, tree) in shas_to_resolve.into_iter().zip(tree_shas) {
        sha_to_tree.insert(commit, tree.to_string());
    }

    Ok(sha_to_tree)
}

fn tree_for_commit<'a>(
    sha_to_tree: &'a HashMap<String, String>,
    sha: &str,
) -> Result<&'a str, GitAiError> {
    sha_to_tree
        .get(sha)
        .map(String::as_str)
        .ok_or_else(|| GitAiError::Generic(format!("missing tree for commit {}", sha)))
}

fn build_diff_tree_stdin(
    pairs: &[(String, String)],
    sha_to_tree: &HashMap<String, String>,
) -> Result<String, GitAiError> {
    let mut stdin_data = String::new();
    for (src, dst) in pairs {
        let src_tree = tree_for_commit(sha_to_tree, src)?;
        let dst_tree = tree_for_commit(sha_to_tree, dst)?;
        stdin_data.push_str(src_tree);
        stdin_data.push(' ');
        stdin_data.push_str(dst_tree);
        stdin_data.push('\n');
    }
    Ok(stdin_data)
}

/// Build the arg list for `git diff-tree --stdin -p -U0 -M --no-color -r`.
/// Shared by the chunked streaming path and the non-chunked
/// `compute_diff_tree_stdin` path.
///
/// We intentionally use the General profile (no PatchParse prefix forcing)
/// here: `diff-tree` is plumbing and -- unlike the `git diff` porcelain --
/// ignores the user's diff.{noprefix,mnemonicPrefix,srcPrefix,dstPrefix},
/// diff.external, and per-path textconv attributes. It always emits raw
/// content with default `a/`..`b/` prefixes, which is exactly what
/// extract_b_path / parse_diff_tree_output expect. (Contrast diff_added_lines
/// in repository.rs, which DOES run `git diff` and therefore must force
/// InternalGitProfile::PatchParse.)
fn build_diff_tree_stdin_args(repo: &Repository) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "--stdin".to_string(),
        "-p".to_string(),
        "-U0".to_string(),
        "-M".to_string(),
        "--no-color".to_string(),
        "-r".to_string(),
    ]);
    args
}

fn compute_diff_tree_stdin(
    repo: &Repository,
    stdin_data: String,
    pair_count: usize,
) -> Result<Vec<DiffTreeResult>, GitAiError> {
    // Stream the output line-by-line into the parser instead of buffering it:
    // after a rebase across a large trunk delta, every pair's root-tree diff
    // contains that whole delta, so the batched output is
    // (trunk delta bytes) x (pair count) and buffering it has driven the
    // daemon to multi-GB RSS. The parsed hunk/line structures are a small
    // fraction of the raw patch text.
    let args = build_diff_tree_stdin_args(repo);
    let mut parser = BatchedDiffTreeParser::new(pair_count);
    exec_git_stdin_streaming(&args, stdin_data.as_bytes(), |line| parser.feed_line(line))?;
    Ok(parser.finish())
}

pub fn handle_rewrite_event(repo: &Repository, event: RewriteEvent) -> Result<(), GitAiError> {
    handle_rewrite_event_with_metrics(repo, event).map(|_| ())
}

pub(crate) fn handle_rewrite_event_with_metrics(
    repo: &Repository,
    event: RewriteEvent,
) -> Result<RewriteOutcome, GitAiError> {
    match event {
        RewriteEvent::SquashMerge {
            ref source_head,
            ref squash_commit,
            ref onto,
        } => handle_squash_merge(repo, source_head, squash_commit, onto),
        RewriteEvent::NonFastForward {
            ref old_tip,
            ref new_tip,
            ref onto,
        } => handle_non_fast_forward_rewrite_with_operation(
            repo,
            old_tip,
            new_tip,
            onto.as_deref(),
            RewriteMetricOperation::NonFastForward,
        ),
        RewriteEvent::CherryPickComplete {
            sources,
            new_commits,
        } => {
            let mappings: Vec<(String, String)> = sources.into_iter().zip(new_commits).collect();
            if mappings.is_empty() {
                return Ok(RewriteOutcome::empty());
            }
            let source_shas: Vec<String> = mappings.iter().map(|(src, _)| src.clone()).collect();
            crate::git::sync_authorship::fetch_missing_notes_for_commits_best_effort(
                repo,
                &source_shas,
            );
            let shifted_notes =
                shift_authorship_notes_merging_existing_with_notes(repo, &mappings)?;
            if !rewrite_metrics_enabled() {
                return Ok(RewriteOutcome::empty());
            }
            let metric_commits =
                metric_commits_from_mappings(&mappings, RewriteMetricOperation::CherryPick);
            Ok(RewriteOutcome::from_metric_commits(
                attach_authorship_notes(metric_commits, shifted_notes),
            ))
        }
    }
}

pub fn handle_non_fast_forward_rewrite(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    onto: Option<&str>,
) -> Result<(), GitAiError> {
    handle_non_fast_forward_rewrite_with_operation(
        repo,
        old_tip,
        new_tip,
        onto,
        RewriteMetricOperation::NonFastForward,
    )
    .map(|_| ())
}

pub(crate) fn handle_non_fast_forward_rewrite_with_operation(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    onto: Option<&str>,
    operation: RewriteMetricOperation,
) -> Result<RewriteOutcome, GitAiError> {
    let mappings = derive_mappings_from_range_diff(repo, old_tip, new_tip, onto)?;
    if mappings.is_empty() {
        return Ok(RewriteOutcome::empty());
    }
    let source_shas: Vec<String> = mappings.iter().map(|(src, _)| src.clone()).collect();
    crate::git::sync_authorship::fetch_missing_notes_for_commits_best_effort(repo, &source_shas);
    let shifted_notes = shift_authorship_notes_merging_existing_with_notes(repo, &mappings)?;
    if !rewrite_metrics_enabled() {
        return Ok(RewriteOutcome::empty());
    }
    let metric_commits = metric_commits_from_mappings(&mappings, operation);
    Ok(RewriteOutcome::from_metric_commits(
        attach_authorship_notes(metric_commits, shifted_notes),
    ))
}

fn handle_squash_merge(
    repo: &Repository,
    source_head: &str,
    squash_commit: &str,
    onto: &str,
) -> Result<RewriteOutcome, GitAiError> {
    use crate::authorship::hunk_shift::apply_hunk_shifts_to_file_attestation;

    let target_notes = notes_api::read_notes_batch(repo, &[squash_commit.to_string()])?;
    let existing_target_log = target_notes
        .get(squash_commit)
        .and_then(|raw| AuthorshipLog::deserialize_from_string(raw).ok())
        .filter(|log| !log.attestations.is_empty());

    let base = find_merge_base(repo, source_head, onto).unwrap_or_else(|| onto.to_string());
    let source_commits = list_commits_in_range(repo, &base, source_head);
    let sources = if source_commits.is_empty() {
        vec![source_head.to_string()]
    } else {
        source_commits
    };

    crate::git::sync_authorship::fetch_missing_notes_for_commits_best_effort(repo, &sources);

    // Batch-read all source notes in O(1) git calls
    let source_notes_map = notes_api::read_notes_batch(repo, &sources)?;

    // Collect which source commits have parseable notes and need intermediate diffs
    struct SourceNote {
        log: AuthorshipLog,
        diff_idx: Option<usize>,
    }

    let mut source_notes: Vec<SourceNote> = Vec::new();
    let mut diff_pairs: Vec<(String, String)> = Vec::new();

    for src_sha in &sources {
        let Some(raw) = source_notes_map.get(src_sha) else {
            continue;
        };
        let Ok(log) = AuthorshipLog::deserialize_from_string(raw) else {
            continue;
        };

        let diff_idx = if src_sha.as_str() != source_head {
            let idx = diff_pairs.len();
            diff_pairs.push((src_sha.clone(), source_head.to_string()));
            Some(idx)
        } else {
            None
        };

        source_notes.push(SourceNote { log, diff_idx });
    }

    if source_notes.is_empty() {
        if let Some(existing_log) = existing_target_log.as_ref()
            && !repo.storage.has_working_log(onto)
        {
            let note = write_authorship_log_for_metrics(repo, squash_commit, existing_log)?;
            return Ok(squash_metric_outcome(squash_commit, &sources, onto, note));
        }
        let note =
            post_squash_resolution_working_log(repo, onto, squash_commit, existing_target_log)?;
        return Ok(squash_metric_outcome(squash_commit, &sources, onto, note));
    }

    // Add the final source_head→squash_commit pair
    let final_diff_idx = diff_pairs.len();
    diff_pairs.push((source_head.to_string(), squash_commit.to_string()));

    // Single batched diff-tree call for ALL intermediate shifts + final shift
    let diff_results = compute_diff_trees_batch(repo, &diff_pairs)?;

    // Phase 1: Shift intermediate notes to source_head's coordinate space and merge
    let mut merged_log: Option<AuthorshipLog> = None;

    for note in source_notes {
        let mut log = note.log;

        if let Some(idx) = note.diff_idx {
            let diff_to_tip = &diff_results[idx];
            for (old_path, new_path) in &diff_to_tip.renames {
                for attestation in &mut log.attestations {
                    if attestation.file_path == *old_path {
                        attestation.file_path = new_path.clone();
                    }
                }
            }
            if !diff_to_tip.hunks_by_file.is_empty() {
                log.attestations = log
                    .attestations
                    .iter()
                    .filter_map(|fa| match diff_to_tip.hunks_by_file.get(&fa.file_path) {
                        Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                        None => Some(fa.clone()),
                    })
                    .collect();
            }
        }

        match merged_log.as_mut() {
            Some(existing) => merge_authorship_logs(existing, &log),
            None => merged_log = Some(log),
        }
    }

    let Some(mut final_log) = merged_log else {
        return Ok(RewriteOutcome::empty());
    };

    // Phase 2: Shift merged log from source_head to squash_commit
    let diff_result = &diff_results[final_diff_idx];

    for (old_path, new_path) in &diff_result.renames {
        for attestation in &mut final_log.attestations {
            if attestation.file_path == *old_path {
                attestation.file_path = new_path.clone();
            }
        }
    }

    if !diff_result.hunks_by_file.is_empty() {
        final_log.attestations = final_log
            .attestations
            .iter()
            .filter_map(|fa| match diff_result.hunks_by_file.get(&fa.file_path) {
                Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                None => Some(fa.clone()),
            })
            .collect();
    }

    final_log.metadata.base_commit_sha = squash_commit.to_string();

    let shifted_log = match existing_target_log {
        Some(existing) => {
            crate::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                Some(final_log),
                existing,
                squash_commit,
            )
        }
        None => final_log,
    };

    if repo.storage.has_working_log(onto) {
        let note =
            post_squash_resolution_working_log(repo, onto, squash_commit, Some(shifted_log))?;
        Ok(squash_metric_outcome(squash_commit, &sources, onto, note))
    } else {
        let note = write_authorship_log_for_metrics(repo, squash_commit, &shifted_log)?;
        Ok(squash_metric_outcome(squash_commit, &sources, onto, note))
    }
}

fn squash_metric_outcome(
    squash_commit: &str,
    sources: &[String],
    onto: &str,
    note: Option<String>,
) -> RewriteOutcome {
    if !rewrite_metrics_enabled() {
        return RewriteOutcome::empty();
    }
    let mut metric_commit = RewriteMetricCommit::new(
        squash_commit.to_string(),
        sources.to_vec(),
        RewriteMetricOperation::SquashMerge,
    )
    .with_parent_sha(onto.to_string());
    metric_commit = attach_authorship_note(metric_commit, note);
    RewriteOutcome::from_metric_commits(vec![metric_commit])
}

fn post_squash_resolution_working_log(
    repo: &Repository,
    onto: &str,
    squash_commit: &str,
    existing_shifted_log: Option<AuthorshipLog>,
) -> Result<Option<String>, GitAiError> {
    if !repo.storage.has_working_log(onto) {
        if let Some(log) = existing_shifted_log {
            return write_authorship_log_for_metrics(repo, squash_commit, &log);
        }
        return Ok(None);
    }

    let commit_for_transform = squash_commit.to_string();
    let author = repo.effective_author_identity().formatted_or_unknown();
    let post_commit_result =
        crate::authorship::post_commit::post_commit_from_working_log_with_transform_and_options_detailed(
            repo,
            Some(onto.to_string()),
            squash_commit.to_string(),
            author,
            crate::authorship::post_commit::PostCommitOptions {
                supress_output: true,
                compute_stats: false,
                recover_attribution: false,
            },
            move |resolution_log| {
                Ok(
                    crate::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                        existing_shifted_log,
                        resolution_log,
                        &commit_for_transform,
                    ),
                )
            },
        )?;
    Ok(post_squash_metric_note_from_result(post_commit_result))
}

fn write_authorship_log(
    repo: &Repository,
    commit_sha: &str,
    log: &AuthorshipLog,
) -> Result<String, GitAiError> {
    let serialized = log.serialize_to_string().map_err(|e| {
        GitAiError::Generic(format!("failed to serialize rewrite authorship log: {}", e))
    })?;
    let entries = vec![(commit_sha.to_string(), serialized)];
    notes_api::write_notes_batch(repo, &entries)?;
    Ok(entries
        .into_iter()
        .next()
        .map(|(_, note)| note)
        .unwrap_or_default())
}

pub fn shift_authorship_notes(
    repo: &Repository,
    mappings: &[(String, String)],
) -> Result<(), GitAiError> {
    shift_authorship_notes_with_existing_mode(repo, mappings, false).map(|_| ())
}

pub fn shift_authorship_notes_merging_existing(
    repo: &Repository,
    mappings: &[(String, String)],
) -> Result<(), GitAiError> {
    shift_authorship_notes_with_existing_mode(repo, mappings, true).map(|_| ())
}

pub(crate) fn shift_authorship_notes_merging_existing_with_notes(
    repo: &Repository,
    mappings: &[(String, String)],
) -> Result<Vec<(String, String)>, GitAiError> {
    shift_authorship_notes_with_existing_mode(repo, mappings, true)
}

/// Maximum number of diff-tree results to hold in memory at once before
/// pausing stdout reads (which blocks git's pipe) to apply shifts and
/// drop the parsed structures. Bounds peak RSS to `CHUNK_SIZE` pairs worth
/// of `Vec<u32>` added-line lists + `Vec<DiffHunk>` hunks, without the cost
/// of extra git spawns.
const DIFF_TREE_STREAM_CHUNK_SIZE: usize = 50;

/// A pending authorship-log shift awaiting its diff-tree result. Built 1:1
/// with (and in the same order as) the `diff_pairs` sent to git, so results
/// are zipped with shifts positionally.
struct PendingShift {
    new_sha: String,
    log: AuthorshipLog,
}

fn shift_authorship_notes_with_existing_mode(
    repo: &Repository,
    mappings: &[(String, String)],
    merge_existing_targets: bool,
) -> Result<Vec<(String, String)>, GitAiError> {
    tracing::debug!("shift_authorship_notes: {} mappings", mappings.len());

    if mappings.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-read all notes for source and target commits in O(1) git calls
    let all_shas: Vec<String> = mappings
        .iter()
        .flat_map(|(src, dst)| [src.clone(), dst.clone()])
        .collect();
    let notes_map = notes_api::read_notes_batch(repo, &all_shas)?;

    // Determine which mappings need processing
    let mut pending: Vec<PendingShift> = Vec::new();
    let mut verbatim_writes: Vec<(String, String)> = Vec::new();
    let mut diff_pairs: Vec<(String, String)> = Vec::new();
    let mut existing_by_target: HashMap<String, AuthorshipLog> = HashMap::new();

    for (source_sha, new_sha) in mappings {
        if let Some(existing_raw) = notes_map.get(new_sha) {
            if let Ok(existing_log) = AuthorshipLog::deserialize_from_string(existing_raw) {
                if !existing_log.attestations.is_empty() {
                    if merge_existing_targets {
                        existing_by_target
                            .entry(new_sha.clone())
                            .or_insert(existing_log);
                    } else {
                        continue;
                    }
                }
            } else {
                continue;
            }
        }

        let Some(raw_note) = notes_map.get(source_sha) else {
            continue;
        };

        let Ok(log) = AuthorshipLog::deserialize_from_string(raw_note) else {
            if !merge_existing_targets {
                verbatim_writes.push((new_sha.clone(), raw_note.clone()));
            }
            continue;
        };

        diff_pairs.push((source_sha.clone(), new_sha.clone()));
        pending.push(PendingShift {
            new_sha: new_sha.clone(),
            log,
        });
    }

    if pending.is_empty() && verbatim_writes.is_empty() {
        return Ok(Vec::new());
    }

    // Stream diff-tree output in bounded chunks from a single git spawn.
    //
    // One `git diff-tree --stdin` is fired with all pairs. The
    // `BatchedDiffTreeParser` accumulates results as lines arrive; every
    // `DIFF_TREE_STREAM_CHUNK_SIZE` completed pairs, the callback drains them,
    // applies shifts, and drops the parsed structures — all inline inside the
    // stdout read loop. While we process, we stop reading stdout, the OS pipe
    // buffer fills, and git blocks on write() until we resume. That
    // back-pressure bounds peak RSS without spawning one git process per
    // chunk. Materializing all pairs at once has driven the daemon to
    // multi-GB RSS when a bad mapping set turns every pair into a
    // full-root-tree diff.
    let mut merged_by_target = existing_by_target;

    // `pending` is 1:1 with `diff_pairs`, so results are zipped with shifts
    // positionally as the chunks advance.
    let mut pending_iter = pending.into_iter();

    if !diff_pairs.is_empty() {
        let unique_shas = unique_pair_shas(&diff_pairs);
        let sha_to_tree = resolve_tree_shas(repo, &unique_shas)?;
        let stdin_data = build_diff_tree_stdin(&diff_pairs, &sha_to_tree)?;
        let diff_args = build_diff_tree_stdin_args(repo);
        let mut parser = BatchedDiffTreeParser::new(diff_pairs.len());

        exec_git_stdin_streaming(&diff_args, stdin_data.as_bytes(), |line| {
            parser.feed_line(line);

            // Drain completed results inline while we hold the pipe —
            // this is the back-pressure: git blocks while we process.
            if parser.completed_count() >= DIFF_TREE_STREAM_CHUNK_SIZE {
                let chunk = parser.take_completed();
                apply_chunk_shifts(&mut merged_by_target, chunk, &mut pending_iter);
            }
        })?;

        // Drain the tail: the in-progress result plus any padding needed if
        // git produced fewer result separators than expected.
        let final_chunk = parser.take_all();
        apply_chunk_shifts(&mut merged_by_target, final_chunk, &mut pending_iter);
    }

    let mut all_writes = verbatim_writes;
    for (sha, log) in merged_by_target {
        let serialized = log.serialize_to_string().map_err(|e| {
            GitAiError::Generic(format!("failed to serialize shifted authorship log: {}", e))
        })?;
        all_writes.push((sha, serialized));
    }

    // Single batched write for all notes
    notes_api::write_notes_batch(repo, &all_writes)?;

    Ok(all_writes)
}

fn merge_authorship_logs(target: &mut AuthorshipLog, source: &AuthorshipLog) {
    for src_fa in &source.attestations {
        if let Some(existing_fa) = target
            .attestations
            .iter_mut()
            .find(|a| a.file_path == src_fa.file_path)
        {
            // Merge entries into existing file attestation
            for src_entry in &src_fa.entries {
                if let Some(existing_entry) = existing_fa
                    .entries
                    .iter_mut()
                    .find(|e| e.hash == src_entry.hash)
                {
                    for range in &src_entry.line_ranges {
                        if !existing_entry.line_ranges.contains(range) {
                            existing_entry.line_ranges.push(range.clone());
                        }
                    }
                } else {
                    existing_fa.entries.push(src_entry.clone());
                }
            }
        } else {
            target.attestations.push(src_fa.clone());
        }
    }
    // Merge all metadata maps
    for (key, record) in &source.metadata.prompts {
        target
            .metadata
            .prompts
            .entry(key.clone())
            .or_insert_with(|| record.clone());
    }
    for (key, record) in &source.metadata.sessions {
        target
            .metadata
            .sessions
            .entry(key.clone())
            .or_insert_with(|| record.clone());
    }
    for (key, record) in &source.metadata.humans {
        target
            .metadata
            .humans
            .entry(key.clone())
            .or_insert_with(|| record.clone());
    }
}

/// Apply hunk/rename shifts from a chunk of `DiffTreeResult`s to the
/// corresponding `PendingShift`s, merging shifted logs into
/// `merged_by_target`. Called inline inside the stdout read loop so the
/// pipe blocks git while we process.
///
/// `pending_iter` yields shifts in the same order as `diff_pairs`, so the
/// Nth diff result always corresponds to the Nth pending shift — the two
/// sequences are zipped positionally.
fn apply_chunk_shifts(
    merged_by_target: &mut HashMap<String, AuthorshipLog>,
    chunk: Vec<DiffTreeResult>,
    pending_iter: &mut impl Iterator<Item = PendingShift>,
) {
    use crate::authorship::hunk_shift::apply_hunk_shifts_to_file_attestation;

    for (diff_result, shift) in chunk.iter().zip(pending_iter) {
        let mut log = shift.log;

        for (old_path, new_path) in &diff_result.renames {
            for attestation in &mut log.attestations {
                if attestation.file_path == *old_path {
                    attestation.file_path = new_path.clone();
                }
            }
        }

        if !diff_result.hunks_by_file.is_empty() {
            log.attestations = log
                .attestations
                .iter()
                .filter_map(|fa| match diff_result.hunks_by_file.get(&fa.file_path) {
                    Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                    None => Some(fa.clone()),
                })
                .collect();
        }

        log.metadata.base_commit_sha = shift.new_sha.clone();

        match merged_by_target.get_mut(&shift.new_sha) {
            Some(existing) => merge_authorship_logs(existing, &log),
            None => {
                merged_by_target.insert(shift.new_sha, log);
            }
        }
    }
}

fn derive_mappings_from_range_diff(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    onto_hint: Option<&str>,
) -> Result<Vec<(String, String)>, GitAiError> {
    let Some(base) = find_merge_base(repo, old_tip, new_tip) else {
        return Ok(Vec::new());
    };

    // Rewind: branch moved backward
    if base == new_tip {
        crate::authorship::rewrite_reset::reconstruct_working_log_after_backward_reset(
            repo, old_tip, new_tip,
        )?;
        return Ok(Vec::new());
    }

    // Fast-forward: no rewrite happened
    if base == old_tip {
        return Ok(Vec::new());
    }

    // Validate onto_hint: it must be an ancestor of new_tip and different from new_tip.
    // If the hint is invalid (e.g., from a checkout-then-rebase where first HEAD change
    // is the checkout, not the rebase), fall back to base.
    let onto = match onto_hint {
        Some(hint) if hint != new_tip && hint != old_tip && is_ancestor(repo, hint, new_tip) => {
            hint
        }
        _ => &base,
    };
    let range_diff_output = run_range_diff(repo, &base, old_tip, onto, new_tip)?;
    let mut mappings = parse_range_diff_output(&range_diff_output);

    let merge_mappings = derive_merge_commit_mappings(repo, &base, old_tip, new_tip, &mappings)?;
    mappings.extend(merge_mappings);

    Ok(mappings)
}

fn is_ancestor(repo: &Repository, ancestor: &str, descendant: &str) -> bool {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "merge-base".to_string(),
        "--is-ancestor".to_string(),
        ancestor.to_string(),
        descendant.to_string(),
    ]);
    exec_git_allow_nonzero(&args)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_merge_base(repo: &Repository, a: &str, b: &str) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.extend(["merge-base".to_string(), a.to_string(), b.to_string()]);

    let output = exec_git_allow_nonzero(&args).ok()?;
    if !output.status.success() {
        return None;
    }
    let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if base.is_empty() { None } else { Some(base) }
}

pub(crate) fn list_commits_in_range(repo: &Repository, base: &str, tip: &str) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--reverse".to_string(),
        format!("{}..{}", base, tip),
    ]);
    exec_git_allow_nonzero(&args)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn run_range_diff(
    repo: &Repository,
    old_base: &str,
    old_tip: &str,
    new_base: &str,
    new_tip: &str,
) -> Result<String, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "range-diff".to_string(),
        "--no-color".to_string(),
        "--no-abbrev".to_string(),
        "-s".to_string(),
        "--creation-factor=100".to_string(),
        format!("{}..{}", old_base, old_tip),
        format!("{}..{}", new_base, new_tip),
    ]);
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Maximum number of unmatched old-range (`<`) commits buffered ahead of the
/// first matched pair. A genuine rebase squash absorbs a handful of commits
/// into the first surviving one, so small queues are drained into that match
/// as before. But when the count exceeds this bound the history is divergent
/// — e.g. a restack undo (`git reset --keep <pre-rebase-tip>`), where the old
/// range spans every trunk commit landed since the branch forked — and the
/// whole queue is discarded, permanently. Without this, each bogus mapping
/// whose source commit has an authorship note becomes a full-root-tree diff
/// pair, and daemon memory scales as
/// (trunk commits since fork) × (lines modified on trunk).
const MAX_PENDING_DROPPED_COMMITS: usize = 64;

fn parse_range_diff_output(output: &str) -> Vec<(String, String)> {
    let mut mappings = Vec::new();
    let mut pending_dropped: Vec<String> = Vec::new();
    let mut pending_overflowed = false;
    let mut previous_new_sha: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Find first 40-char hex SHA
        let Some((old_sha, rest)) = find_next_sha(trimmed) else {
            continue;
        };

        // Skip whitespace, read status character
        let rest = rest.trim_start();
        let Some(status_char) = rest.chars().next() else {
            continue;
        };

        match status_char {
            '<' => {
                // Dropped commit (squashed into a later commit)
                if !old_sha.chars().all(|c| c == '0') {
                    if let Some(new_sha) = previous_new_sha.as_ref() {
                        mappings.push((old_sha, new_sha.clone()));
                    } else if !pending_overflowed {
                        pending_dropped.push(old_sha);
                        if pending_dropped.len() > MAX_PENDING_DROPPED_COMMITS {
                            // Divergent history (e.g. restack undo), not a
                            // squash: discard the queue and stop collecting so
                            // the mapping count stays bounded.
                            pending_dropped.clear();
                            pending_dropped.shrink_to_fit();
                            pending_overflowed = true;
                        }
                    }
                }
            }
            '=' | '!' => {
                // Matched pair
                let after_status = &rest[status_char.len_utf8()..];
                let Some((new_sha, _)) = find_next_sha(after_status) else {
                    continue;
                };
                if old_sha.chars().all(|c| c == '0') || new_sha.chars().all(|c| c == '0') {
                    continue;
                }
                // Map any preceding dropped commits to this new commit (squash)
                for dropped in pending_dropped.drain(..) {
                    mappings.push((dropped, new_sha.clone()));
                }
                previous_new_sha = Some(new_sha.clone());
                mappings.push((old_sha, new_sha));
            }
            _ => {
                // '>' (new commit) or other — skip
                continue;
            }
        }
    }

    mappings
}

/// Find the first maximal ASCII-hex run in `s` whose length is a valid git OID
/// length (40 for SHA-1, 64 for SHA-256) and return it with the remainder of
/// the string after the run.
///
/// Scans over bytes rather than chars so a multibyte commit subject (e.g. a
/// range-diff `-s` line like `Café …`) never makes a window boundary land
/// inside a char and panic. Only a matched, all-ASCII window is converted to a
/// `String`. Taking the maximal run (delimited by non-hex on both sides) means
/// a 64-char SHA-256 OID is returned in full instead of truncated to 40.
fn find_next_sha(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
            end += 1;
        }
        let run_len = end - start;
        if run_len == 40 || run_len == 64 {
            // The run is all ASCII hex, so slicing here is always char-safe.
            return Some((s[start..end].to_string(), &s[end..]));
        }
        // Not an OID-length run; skip past it entirely and keep scanning.
        i = end;
    }
    None
}

// DEFERRED (code-review #15): old->new merge commits are paired greedily by
// first parent-set match (the inner loop `break`s on the first new_merge whose
// parents all map). When two sibling merges in the same range share an
// identical parent mapping, the first-match pairing can attach old_merge A's
// note to new_merge B and vice versa. Harmless in the common single-merge case;
// a precise fix would disambiguate ties (e.g. by tree identity or commit order)
// instead of taking the first structural match.
fn derive_merge_commit_mappings(
    repo: &Repository,
    base: &str,
    old_tip: &str,
    new_tip: &str,
    existing_mappings: &[(String, String)],
) -> Result<Vec<(String, String)>, GitAiError> {
    let old_merges = list_merge_commits(repo, base, old_tip)?;
    let new_merges = list_merge_commits(repo, base, new_tip)?;

    if old_merges.is_empty() || new_merges.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-check which old merges have notes
    let commits_with_notes = notes_api::commits_with_notes(repo, &old_merges)?;
    let merge_parent_map = get_commit_parents_batch(
        repo,
        &old_merges
            .iter()
            .chain(new_merges.iter())
            .cloned()
            .collect::<Vec<_>>(),
    );

    let mut merge_mappings: Vec<(String, String)> = Vec::new();

    for old_merge in &old_merges {
        if !commits_with_notes.contains(old_merge) {
            continue;
        }

        let old_parents = merge_parent_map.get(old_merge).cloned().unwrap_or_default();
        if old_parents.is_empty() {
            continue;
        }

        for new_merge in &new_merges {
            if merge_mappings.iter().any(|(_, n)| n == new_merge) {
                continue;
            }

            let new_parents = merge_parent_map.get(new_merge).cloned().unwrap_or_default();
            if new_parents.len() != old_parents.len() {
                continue;
            }

            let all_match = old_parents.iter().zip(new_parents.iter()).all(|(op, np)| {
                if existing_mappings.iter().any(|(o, n)| o == op && n == np) {
                    return true;
                }
                if merge_mappings.iter().any(|(o, n)| o == op && n == np) {
                    return true;
                }
                op == np
            });

            if all_match {
                merge_mappings.push((old_merge.clone(), new_merge.clone()));
                break;
            }
        }
    }

    Ok(merge_mappings)
}

fn list_merge_commits(repo: &Repository, base: &str, tip: &str) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--merges".to_string(),
        "--topo-order".to_string(),
        "--reverse".to_string(),
        format!("{}..{}", base, tip),
    ]);

    let output = exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn get_commit_parents_batch(repo: &Repository, shas: &[String]) -> HashMap<String, Vec<String>> {
    if shas.is_empty() {
        return HashMap::new();
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%H %P".to_string(),
        "--no-walk".to_string(),
    ]);
    args.extend(shas.iter().cloned());

    let Ok(output) = exec_git_allow_nonzero(&args) else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?.to_string();
            let parents = parts.map(ToOwned::to_owned).collect::<Vec<_>>();
            Some((sha, parents))
        })
        .collect()
}

/// Batch-compute diff-trees for multiple commit pairs in a single git process.
/// Resolves commits to tree SHAs, then pipes all pairs into `git diff-tree --stdin`.
pub(crate) fn compute_diff_trees_batch(
    repo: &Repository,
    pairs: &[(String, String)],
) -> Result<Vec<DiffTreeResult>, GitAiError> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }

    let unique_shas = unique_pair_shas(pairs);
    let sha_to_tree = resolve_tree_shas(repo, &unique_shas)?;
    let stdin_data = build_diff_tree_stdin(pairs, &sha_to_tree)?;
    compute_diff_tree_stdin(repo, stdin_data, pairs.len())
}

/// Incremental parser for the output of `git diff-tree --stdin`, which
/// produces one patch per tree pair, each preceded by a "tree1 tree2"
/// separator line. Results are positional: the Nth separator starts the Nth
/// pair's patch. Fed one line at a time so callers can stream arbitrarily
/// large diff output without holding the raw patch text in memory.
struct BatchedDiffTreeParser {
    expected_pairs: usize,
    results: Vec<DiffTreeResult>,
    current: DiffTreeChunkParser,
    seen_first_header: bool,
}

impl BatchedDiffTreeParser {
    fn new(expected_pairs: usize) -> Self {
        Self {
            expected_pairs,
            results: Vec::with_capacity(expected_pairs.min(DIFF_TREE_STREAM_CHUNK_SIZE)),
            current: DiffTreeChunkParser::default(),
            seen_first_header: false,
        }
    }

    fn feed_line(&mut self, line: &str) {
        // Separator lines are exactly "tree_sha1 tree_sha2" (two OIDs separated by a space)
        if is_tree_pair_separator(line) {
            if self.seen_first_header {
                let chunk = std::mem::take(&mut self.current);
                self.results.push(chunk.finish());
            }
            self.seen_first_header = true;
        } else if self.seen_first_header {
            self.current.feed_line(line);
        }
    }

    fn completed_count(&self) -> usize {
        self.results.len()
    }

    /// Drain and return all completed results, leaving the parser ready to
    /// accumulate more. The `expected_pairs` count is adjusted so final
    /// padding still accounts for already-drained results.
    fn take_completed(&mut self) -> Vec<DiffTreeResult> {
        let drained = std::mem::take(&mut self.results);
        self.expected_pairs = self.expected_pairs.saturating_sub(drained.len());
        // Re-allocate at chunk capacity so subsequent push() calls don't
        // re-allocate incrementally.
        self.results = Vec::with_capacity(self.expected_pairs.min(DIFF_TREE_STREAM_CHUNK_SIZE));
        drained
    }

    /// Drain remaining completed results (including the in-progress chunk) and
    /// pad with empty entries if git produced fewer result separators than
    /// expected.
    fn take_all(&mut self) -> Vec<DiffTreeResult> {
        // Push the in-progress chunk if any
        if self.seen_first_header {
            let chunk = std::mem::take(&mut self.current);
            self.results.push(chunk.finish());
            self.seen_first_header = false;
        }

        let mut drained = std::mem::take(&mut self.results);

        // Pad to expected count for identical trees
        while drained.len() < self.expected_pairs {
            drained.push(DiffTreeResult::default());
        }

        self.expected_pairs = 0;
        drained
    }

    /// Consume self and return all results (padded to expected_pairs).
    fn finish(self) -> Vec<DiffTreeResult> {
        let mut parser = self;
        parser.take_all()
    }
}

/// Parse the output of `git diff-tree --stdin` provided as a single string.
/// Thin wrapper over `BatchedDiffTreeParser` (which the streaming path feeds
/// directly).
#[cfg(test)]
fn parse_batched_diff_tree_output(output: &str, expected_pairs: usize) -> Vec<DiffTreeResult> {
    let mut parser = BatchedDiffTreeParser::new(expected_pairs);
    for line in output.lines() {
        parser.feed_line(line);
    }
    parser.finish()
}

fn is_tree_pair_separator(line: &str) -> bool {
    // "tree1 tree2" — two git OIDs separated by a single space. Validate both
    // halves structurally via is_valid_git_oid so this accepts both the 81-byte
    // SHA-1 separator and the 129-byte SHA-256 separator (rather than a
    // hard-coded length).
    let Some((old, new)) = line.split_once(' ') else {
        return false;
    };
    is_valid_git_oid(old) && is_valid_git_oid(new)
}

/// Incremental parser for a single tree pair's diff-tree patch.
#[derive(Default)]
struct DiffTreeChunkParser {
    hunks_by_file: HashMap<String, Vec<DiffHunk>>,
    added_lines_by_file: HashMap<String, Vec<u32>>,
    renames: Vec<(String, String)>,
    current_file: Option<String>,
    current_rename_from: Option<String>,
    active_hunk_new_line: Option<u32>,
}

impl DiffTreeChunkParser {
    fn feed_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // Extract the b/ path from "a/old b/new"
            self.current_file = extract_b_path(rest);
            self.current_rename_from = None;
            self.active_hunk_new_line = None;
        } else if let Some(from_path) = line.strip_prefix("rename from ") {
            self.current_rename_from = Some(from_path.to_string());
            self.active_hunk_new_line = None;
        } else if let Some(to_path) = line.strip_prefix("rename to ") {
            if let Some(from_path) = self.current_rename_from.take() {
                self.renames.push((from_path, to_path.to_string()));
            }
        } else if line.starts_with("@@")
            && let Some(ref file) = self.current_file
            && let Some(hunk) = parse_hunk_header(line)
        {
            self.active_hunk_new_line = Some(hunk.new_start);
            self.hunks_by_file
                .entry(file.clone())
                .or_default()
                .push(hunk);
        } else if let Some(new_line) = self.active_hunk_new_line.as_mut() {
            if line.starts_with('+') {
                if let Some(ref file) = self.current_file {
                    self.added_lines_by_file
                        .entry(file.clone())
                        .or_default()
                        .push(*new_line);
                }
                *new_line += 1;
            } else if line.starts_with('-') || line.starts_with('\\') {
                // Removed lines and "\ No newline at end of file" markers do
                // not advance the new-file line cursor.
            } else {
                *new_line += 1;
            }
        }
    }

    fn finish(mut self) -> DiffTreeResult {
        for lines in self.added_lines_by_file.values_mut() {
            lines.sort_unstable();
            lines.dedup();
        }

        DiffTreeResult {
            hunks_by_file: self.hunks_by_file,
            added_lines_by_file: self.added_lines_by_file,
            renames: self.renames,
        }
    }
}

#[cfg(test)]
fn parse_diff_tree_output(output: &str) -> DiffTreeResult {
    let mut parser = DiffTreeChunkParser::default();
    for line in output.lines() {
        parser.feed_line(line);
    }
    parser.finish()
}

fn extract_b_path(diff_header: &str) -> Option<String> {
    // Format: "a/path b/path" or "a/path with spaces b/path with spaces"
    // The b/ path starts after the last occurrence of " b/"
    let marker = " b/";
    let pos = diff_header.rfind(marker)?;
    Some(diff_header[pos + marker.len()..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_commits_from_mappings_groups_squashed_sources() {
        let mappings = vec![
            ("old1".to_string(), "new".to_string()),
            ("old2".to_string(), "new".to_string()),
            ("old1".to_string(), "new".to_string()),
        ];

        let commits = metric_commits_from_mappings(&mappings, RewriteMetricOperation::Rebase);

        assert_eq!(
            commits,
            vec![RewriteMetricCommit::new(
                "new",
                vec!["old1".to_string(), "old2".to_string()],
                RewriteMetricOperation::Rebase,
            )]
        );
    }

    #[test]
    fn branch_name_from_ref_only_accepts_local_branch_refs() {
        assert_eq!(
            branch_name_from_ref("refs/heads/feature").as_deref(),
            Some("feature")
        );
        assert_eq!(branch_name_from_ref("HEAD"), None);
        assert_eq!(branch_name_from_ref("refs/tags/v1"), None);
    }

    #[test]
    fn rewrite_metric_operation_strings_are_stable() {
        assert_eq!(RewriteMetricOperation::Rebase.as_str(), "rebase");
        assert_eq!(RewriteMetricOperation::SquashMerge.as_str(), "squash_merge");
        assert_eq!(RewriteMetricOperation::CherryPick.as_str(), "cherry_pick");
        assert_eq!(
            RewriteMetricOperation::CherryPickNoCommit.as_str(),
            "cherry_pick_no_commit"
        );
        assert_eq!(RewriteMetricOperation::Amend.as_str(), "amend");
        assert_eq!(RewriteMetricOperation::Revert.as_str(), "revert");
        assert_eq!(RewriteMetricOperation::UpdateRef.as_str(), "update_ref");
        assert_eq!(
            RewriteMetricOperation::NonFastForward.as_str(),
            "non_fast_forward"
        );
    }

    #[test]
    fn test_extract_b_path_simple() {
        assert_eq!(
            extract_b_path("a/src/main.rs b/src/main.rs"),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn test_extract_b_path_rename() {
        assert_eq!(
            extract_b_path("a/src/old.rs b/src/new.rs"),
            Some("src/new.rs".to_string())
        );
    }

    #[test]
    fn test_extract_b_path_with_spaces() {
        assert_eq!(
            extract_b_path("a/path with spaces b/another path"),
            Some("another path".to_string())
        );
    }

    #[test]
    fn test_parse_diff_tree_output_simple() {
        let output = "\
diff --git a/src/foo.rs b/src/foo.rs
index abc123..def456 100644
--- a/src/foo.rs
+++ b/src/foo.rs
@@ -10,3 +10,5 @@ fn foo()
+added line 1
+added line 2
";
        let result = parse_diff_tree_output(output);
        assert!(result.renames.is_empty());
        assert_eq!(result.hunks_by_file.len(), 1);
        let hunks = &result.hunks_by_file["src/foo.rs"];
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 10);
        assert_eq!(hunks[0].old_count, 3);
        assert_eq!(hunks[0].new_start, 10);
        assert_eq!(hunks[0].new_count, 5);
    }

    #[test]
    fn test_parse_diff_tree_output_with_rename() {
        let output = "\
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index abc123..def456 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -5,2 +5,3 @@ fn bar()
+new line
";
        let result = parse_diff_tree_output(output);
        assert_eq!(result.renames.len(), 1);
        assert_eq!(
            result.renames[0],
            ("src/old.rs".to_string(), "src/new.rs".to_string())
        );
        let hunks = &result.hunks_by_file["src/new.rs"];
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 5);
        assert_eq!(hunks[0].old_count, 2);
        assert_eq!(hunks[0].new_start, 5);
        assert_eq!(hunks[0].new_count, 3);
    }

    #[test]
    fn test_parse_diff_tree_output_multiple_files() {
        let output = "\
diff --git a/file1.rs b/file1.rs
index aaa..bbb 100644
--- a/file1.rs
+++ b/file1.rs
@@ -1,2 +1,3 @@
+line
diff --git a/file2.rs b/file2.rs
index ccc..ddd 100644
--- a/file2.rs
+++ b/file2.rs
@@ -10,0 +11,2 @@
+line1
+line2
";
        let result = parse_diff_tree_output(output);
        assert_eq!(result.hunks_by_file.len(), 2);
        assert_eq!(result.hunks_by_file["file1.rs"].len(), 1);
        assert_eq!(result.hunks_by_file["file2.rs"].len(), 1);
        assert_eq!(result.hunks_by_file["file2.rs"][0].old_start, 10);
        assert_eq!(result.hunks_by_file["file2.rs"][0].old_count, 0);
        assert_eq!(result.hunks_by_file["file2.rs"][0].new_start, 11);
        assert_eq!(result.hunks_by_file["file2.rs"][0].new_count, 2);
    }

    #[test]
    fn test_parse_diff_tree_output_binary() {
        let output = "\
diff --git a/image.png b/image.png
Binary files a/image.png and b/image.png differ
";
        let result = parse_diff_tree_output(output);
        // No hunks for binary files
        assert!(
            result
                .hunks_by_file
                .get("image.png")
                .is_none_or(|h| h.is_empty())
        );
    }

    #[test]
    fn test_parse_diff_tree_empty_output() {
        let result = parse_diff_tree_output("");
        assert!(result.hunks_by_file.is_empty());
        assert!(result.renames.is_empty());
    }

    #[test]
    fn test_find_next_sha_rejects_non_oid_length_runs() {
        // A hex run that is neither 40 nor 64 chars is not an OID and must be
        // skipped (e.g. a short abbreviated hash or an index blob fragment).
        assert!(find_next_sha("deadbeef not a full oid").is_none());
        // 39 and 41 chars (off-by-one around SHA-1) are rejected.
        assert!(find_next_sha(&"a".repeat(39)).is_none());
        let nearly = format!("{} x", "a".repeat(41));
        assert!(find_next_sha(&nearly).is_none());
    }

    #[test]
    fn test_parse_range_diff_output_matched_equal() {
        let output = " 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb Some commit subject\n";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(mappings[0].1, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    }

    #[test]
    fn test_parse_range_diff_output_matched_bang() {
        let output = " 2:  1111111111111111111111111111111111111111 ! 3:  2222222222222222222222222222222222222222 Modified commit\n";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, "1111111111111111111111111111111111111111");
        assert_eq!(mappings[0].1, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn test_parse_range_diff_output_dropped_and_new() {
        let output = "\
 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa < -:  0000000000000000000000000000000000000000 Dropped commit
 -:  0000000000000000000000000000000000000000 > 1:  cccccccccccccccccccccccccccccccccccccccc New commit
";
        let mappings = parse_range_diff_output(output);
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_parse_range_diff_output_dropped_then_matched_maps_both_to_destination() {
        let output = "\
1:  1111111111111111111111111111111111111111 < -:  ---------------------------------------- Add Python joke
2:  2222222222222222222222222222222222222222 ! 1:  3333333333333333333333333333333333333333 Add Rust joke
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(
            mappings,
            vec![
                (
                    "1111111111111111111111111111111111111111".to_string(),
                    "3333333333333333333333333333333333333333".to_string()
                ),
                (
                    "2222222222222222222222222222222222222222".to_string(),
                    "3333333333333333333333333333333333333333".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_parse_range_diff_output_null_shas_skipped() {
        let output = " 1:  0000000000000000000000000000000000000000 = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb Subject\n";
        let mappings = parse_range_diff_output(output);
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_parse_range_diff_output_multiple_lines() {
        let output = "\
 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb First commit
 2:  cccccccccccccccccccccccccccccccccccccccc ! 2:  dddddddddddddddddddddddddddddddddddddddd Second commit
 3:  eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee = 3:  ffffffffffffffffffffffffffffffffffffffff Third commit
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 3);
        assert_eq!(
            mappings[0],
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            )
        );
        assert_eq!(
            mappings[1],
            (
                "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                "dddddddddddddddddddddddddddddddddddddddd".to_string()
            )
        );
        assert_eq!(
            mappings[2],
            (
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
                "ffffffffffffffffffffffffffffffffffffffff".to_string()
            )
        );
    }

    #[test]
    fn test_parse_range_diff_output_empty() {
        let mappings = parse_range_diff_output("");
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_parse_range_diff_output_matched_then_dropped_maps_all_to_destination() {
        // Matches AFTER the first `!` match are still mapped — this is the
        // genuine squash path (drops between matched pairs).
        let output = "\
1:  1111111111111111111111111111111111111111 ! 1:  4444444444444444444444444444444444444444 AI commit 1
2:  2222222222222222222222222222222222222222 < -:  ---------------------------------------- AI commit 2
3:  3333333333333333333333333333333333333333 < -:  ---------------------------------------- AI commit 3
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(
            mappings,
            vec![
                (
                    "1111111111111111111111111111111111111111".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
                (
                    "2222222222222222222222222222222222222222".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
                (
                    "3333333333333333333333333333333333333333".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_parse_range_diff_output_divergent_history_discards_pending() {
        // Regression: restack-undo (moving a branch tip backward to undo a
        // rebase) produces a range-diff where the old range spans every trunk
        // commit since the fork point. All those trunk commits appear as `<`
        // before the first stack-commit match. They must be discarded — they
        // were never part of a squash.
        let sha_a = "1".repeat(40);
        let sha_b = "2".repeat(40);
        let mut output = String::new();
        // 200 divergent trunk commits before the first real match
        for i in 0..200 {
            let sha = format!("{:040}", i);
            output.push_str(&format!(
                "{}:  {} < -:  ---------------------------------------- trunk commit {}\n",
                i + 1,
                sha,
                i
            ));
        }
        // One real matched pair
        output.push_str(&format!(
            "201:  {} = 1:  {} AI feature commit\n",
            sha_a, sha_b
        ));
        let mappings = parse_range_diff_output(&output);
        // Only the matched pair, not 200 bogus mappings
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0], (sha_a, sha_b));
    }

    #[test]
    fn test_parse_range_diff_output_small_pending_dropped_still_mapped() {
        // A bounded number of `<` commits ahead of the first match is a
        // genuine squash and must still be drained into it — the overflow
        // guard only fires past MAX_PENDING_DROPPED_COMMITS.
        let dropped = "1".repeat(40);
        let old_matched = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let new_matched = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let output = format!(
            "\
1:  {} < -:  ---------------------------------------- dropped
2:  {} = 1:  {} matched\n",
            dropped, old_matched, new_matched,
        );
        let mappings = parse_range_diff_output(&output);
        assert_eq!(
            mappings,
            vec![(dropped, new_matched.clone()), (old_matched, new_matched),]
        );
    }

    #[test]
    fn test_parse_range_diff_output_pending_after_first_match_still_mapped() {
        // After the first match, `<` commits between matches are genuine
        // squashes and must still be mapped to the most recently seen new sha.
        let old_1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let new_1 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let old_dropped = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let old_2 = "dddddddddddddddddddddddddddddddddddddddd".to_string();
        let new_2 = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string();
        let output = format!(
            "\
1:  {} ! 1:  {} commit a
2:  {} < -:  ---------------------------------------- squashed into previous
3:  {} ! 2:  {} commit c\n",
            old_1, new_1, old_dropped, old_2, new_2,
        );
        let mappings = parse_range_diff_output(&output);
        // old_1→new_1 (first match, seen_first_match set to true),
        // old_dropped→new_1 (maps to previous_new_sha, which is still new_1),
        // old_2→new_2 (second match, updates previous_new_sha)
        assert_eq!(mappings.len(), 3);
        assert_eq!(mappings[0], (old_1, new_1.clone()));
        assert_eq!(mappings[1], (old_dropped, new_1));
        assert_eq!(mappings[2], (old_2, new_2));
    }

    #[test]
    fn test_parse_range_diff_output_exactly_64_leading_drops_still_mapped() {
        // Exactly MAX_PENDING_DROPPED_COMMITS (64) leading `<` commits should
        // all be mapped to the first matched new SHA, plus the match's own old
        // SHA — total of 65 mappings. Start at 1 so the first SHA is not all
        // zeros and is therefore not skipped.
        const MAX_DROPPED: usize = 64;
        let old_matched = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let new_matched = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let mut output = String::new();
        for i in 1..=MAX_DROPPED {
            let sha = format!("{:040}", i);
            output.push_str(&format!(
                "{}:  {} < -:  ---------------------------------------- dropped {}\n",
                i, sha, i
            ));
        }
        output.push_str(&format!(
            "{}:  {} = 1:  {} matched\n",
            MAX_DROPPED + 1,
            old_matched,
            new_matched,
        ));
        let mappings = parse_range_diff_output(&output);
        // 64 dropped + 1 matched
        assert_eq!(mappings.len(), MAX_DROPPED + 1);
        // All dropped commits map to new_matched
        for i in 1..=MAX_DROPPED {
            let sha = format!("{:040}", i);
            assert_eq!(mappings[i - 1], (sha, new_matched.clone()));
        }
        // The matched pair is last
        assert_eq!(mappings[MAX_DROPPED], (old_matched, new_matched));
    }

    #[test]
    fn test_parse_range_diff_output_exactly_65_leading_drops_discards_all() {
        // Exactly MAX_PENDING_DROPPED_COMMITS + 1 (65) leading `<` commits
        // exceeds the limit, so ALL of them are discarded — only the matched
        // pair's own mapping survives. Start at 1 so the first SHA is not all
        // zeros and is therefore not skipped.
        const MAX_DROPPED_PLUS_1: usize = 65;
        let old_matched = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let new_matched = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let mut output = String::new();
        for i in 1..=MAX_DROPPED_PLUS_1 {
            let sha = format!("{:040}", i);
            output.push_str(&format!(
                "{}:  {} < -:  ---------------------------------------- dropped {}\n",
                i, sha, i
            ));
        }
        output.push_str(&format!(
            "{}:  {} = 1:  {} matched\n",
            MAX_DROPPED_PLUS_1 + 1,
            old_matched,
            new_matched,
        ));
        let mappings = parse_range_diff_output(&output);
        // Only the matched pair survived
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0], (old_matched, new_matched));
    }

    #[test]
    fn test_is_tree_pair_separator_valid() {
        let line =
            "1778ed95466977076f4e5908e6500789be732d2e 471b7bbf5998ffa15a81b17ee9f6854a357a2a6a";
        assert!(is_tree_pair_separator(line));
    }

    #[test]
    fn test_is_tree_pair_separator_invalid() {
        assert!(!is_tree_pair_separator("diff --git a/foo b/foo"));
        assert!(!is_tree_pair_separator("@@ -1,2 +1,3 @@"));
        assert!(!is_tree_pair_separator(""));
        assert!(!is_tree_pair_separator("short"));
        // Missing space
        assert!(!is_tree_pair_separator(
            "1778ed95466977076f4e5908e6500789be732d2e471b7bbf5998ffa15a81b17ee9f6854a357a2a6a"
        ));
    }

    #[test]
    fn test_find_next_sha_does_not_panic_on_multibyte_subject() {
        // Regression (#1): find_next_sha sliced `&s[i..i+40]` by byte index. A
        // commit subject with a multibyte char ('é' at bytes 3..5) makes a
        // byte-window boundary land inside the char and panics
        // ("byte index 4 is not a char boundary; inside 'é'"). It must scan
        // safely and still find the trailing SHA.
        let sha = "a".repeat(40);
        let input = format!("Café commit subject {}", sha);
        let (found, rest) = find_next_sha(&input).expect("should find the trailing SHA");
        assert_eq!(found, sha);
        assert_eq!(rest, "");
    }

    #[test]
    fn test_find_next_sha_returns_full_sha256_oid() {
        // Regression (#10): a 64-char SHA-256 OID must be returned in full, not
        // truncated to the first 40 chars.
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(sha256.len(), 64);
        let input = format!("{} trailing", sha256);
        let (found, rest) = find_next_sha(&input).expect("should find the 64-char OID");
        assert_eq!(found, sha256);
        assert_eq!(rest, " trailing");
    }

    #[test]
    fn test_is_tree_pair_separator_accepts_sha256_pair() {
        // Regression (#10): a SHA-256 tree-pair separator is "64hex 64hex"
        // (129 bytes), not the hard-coded 81-byte SHA-1 shape.
        let a = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let line = format!("{} {}", a, b);
        assert_eq!(line.len(), 129);
        assert!(is_tree_pair_separator(&line));
    }

    #[test]
    fn test_parse_range_diff_output_sha256() {
        // Regression (#10): range-diff with 64-char OIDs must map the full OIDs,
        // not 40-char truncations.
        let old = "1111111111111111111111111111111111111111111111111111111111111111";
        let new = "2222222222222222222222222222222222222222222222222222222222222222";
        let output = format!(" 1:  {} = 1:  {} Some subject\n", old, new);
        let mappings = parse_range_diff_output(&output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, old);
        assert_eq!(mappings[0].1, new);
    }

    #[test]
    fn test_parse_batched_diff_tree_output_single_pair() {
        let output = "\
1778ed95466977076f4e5908e6500789be732d2e 471b7bbf5998ffa15a81b17ee9f6854a357a2a6a
diff --git a/f.txt b/f.txt
index a29bdeb..c0d0fb4 100644
--- a/f.txt
+++ b/f.txt
@@ -1,0 +2 @@ line1
+line2
";
        let results = parse_batched_diff_tree_output(output, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hunks_by_file.len(), 1);
        assert_eq!(results[0].hunks_by_file["f.txt"][0].new_count, 1);
    }

    #[test]
    fn test_parse_batched_diff_tree_output_multiple_pairs() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/f.txt b/f.txt
index a29bdeb..c0d0fb4 100644
--- a/f.txt
+++ b/f.txt
@@ -1,0 +2 @@ line1
+line2
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/g.txt b/g.txt
index eee..fff 100644
--- a/g.txt
+++ b/g.txt
@@ -5,2 +5,3 @@
+new line
";
        let results = parse_batched_diff_tree_output(output, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].hunks_by_file.len(), 1);
        assert!(results[0].hunks_by_file.contains_key("f.txt"));
        assert_eq!(results[1].hunks_by_file.len(), 1);
        assert!(results[1].hunks_by_file.contains_key("g.txt"));
    }

    #[test]
    fn test_parse_batched_diff_tree_output_identical_trees() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
";
        let results = parse_batched_diff_tree_output(output, 1);
        assert_eq!(results.len(), 1);
        assert!(results[0].hunks_by_file.is_empty());
        assert!(results[0].renames.is_empty());
    }

    #[test]
    fn test_parse_batched_diff_tree_output_mixed_identical_and_changed() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/f.txt b/f.txt
@@ -1,0 +2 @@
+x
cccccccccccccccccccccccccccccccccccccccc cccccccccccccccccccccccccccccccccccccccc
dddddddddddddddddddddddddddddddddddddddd eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
diff --git a/g.txt b/g.txt
@@ -3,1 +3,2 @@
+y
";
        let results = parse_batched_diff_tree_output(output, 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].hunks_by_file.len(), 1);
        assert!(results[1].hunks_by_file.is_empty());
        assert_eq!(results[2].hunks_by_file.len(), 1);
    }

    #[test]
    fn test_parse_batched_diff_tree_output_empty() {
        let results = parse_batched_diff_tree_output("", 0);
        assert!(results.is_empty());
    }

    #[test]
    fn test_batched_diff_tree_parser_streams_line_by_line() {
        // The streaming exec path feeds the parser one line at a time (without
        // trailing newlines); the result must match parsing the whole output.
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index abc123..def456 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -5,2 +5,3 @@ fn bar()
+new line
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/g.txt b/g.txt
index eee..fff 100644
--- a/g.txt
+++ b/g.txt
@@ -10,0 +11,2 @@
+line1
+line2
";
        // Pad expected_pairs beyond what git emitted (identical trees case).
        let mut parser = BatchedDiffTreeParser::new(3);
        for line in output.lines() {
            parser.feed_line(line);
        }
        let streamed = parser.finish();

        assert_eq!(streamed, parse_batched_diff_tree_output(output, 3));
        assert_eq!(streamed.len(), 3);
        assert_eq!(
            streamed[0].renames,
            vec![("src/old.rs".to_string(), "src/new.rs".to_string())]
        );
        assert_eq!(streamed[0].added_lines_by_file["src/new.rs"], vec![5]);
        assert_eq!(streamed[1].added_lines_by_file["g.txt"], vec![11, 12]);
        assert_eq!(streamed[2], DiffTreeResult::default());
    }

    #[test]
    fn test_batched_diff_tree_parser_can_drain_completed_chunks() {
        let output = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
diff --git a/a.txt b/a.txt
@@ -1,0 +1 @@
+a
cccccccccccccccccccccccccccccccccccccccc dddddddddddddddddddddddddddddddddddddddd
diff --git a/b.txt b/b.txt
@@ -1,0 +1 @@
+b
eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee ffffffffffffffffffffffffffffffffffffffff
diff --git a/c.txt b/c.txt
@@ -1,0 +1 @@
+c
";

        let mut parser = BatchedDiffTreeParser::new(3);
        let mut chunks = Vec::new();
        for line in output.lines() {
            parser.feed_line(line);
            if parser.completed_count() >= 2 {
                chunks.push(parser.take_completed());
            }
        }
        chunks.push(parser.take_all());

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 2);
        assert_eq!(chunks[1].len(), 1);
        assert!(chunks[0][0].hunks_by_file.contains_key("a.txt"));
        assert!(chunks[0][1].hunks_by_file.contains_key("b.txt"));
        assert!(chunks[1][0].hunks_by_file.contains_key("c.txt"));
    }

    fn diff_tree_stream_with_identical_pairs(
        pair_count: usize,
        identical_pair_numbers: &[usize],
    ) -> String {
        let mut output = String::new();
        for pair_number in 1..=pair_count {
            let old_oid = format!("{:040x}", pair_number);
            let new_oid = if identical_pair_numbers.contains(&pair_number) {
                old_oid.clone()
            } else {
                format!("{:040x}", pair_number + 10_000)
            };
            output.push_str(&format!("{old_oid} {new_oid}\n"));
            if !identical_pair_numbers.contains(&pair_number) {
                output.push_str(&format!(
                    "diff --git a/file_{pair_number}.txt b/file_{pair_number}.txt\n\
                     @@ -1,0 +1 @@\n\
                     +line {pair_number}\n"
                ));
            }
        }
        output
    }

    #[test]
    fn test_batched_diff_tree_parser_preserves_identical_pairs_across_drain_boundary() {
        let output = diff_tree_stream_with_identical_pairs(
            DIFF_TREE_STREAM_CHUNK_SIZE + 2,
            &[
                DIFF_TREE_STREAM_CHUNK_SIZE - 1,
                DIFF_TREE_STREAM_CHUNK_SIZE,
                DIFF_TREE_STREAM_CHUNK_SIZE + 1,
            ],
        );

        let mut parser = BatchedDiffTreeParser::new(DIFF_TREE_STREAM_CHUNK_SIZE + 2);
        let mut chunks = Vec::new();
        for line in output.lines() {
            parser.feed_line(line);
            if parser.completed_count() >= DIFF_TREE_STREAM_CHUNK_SIZE {
                chunks.push(parser.take_completed());
            }
        }
        chunks.push(parser.take_all());

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), DIFF_TREE_STREAM_CHUNK_SIZE);
        assert_eq!(chunks[1].len(), 2);

        let results: Vec<_> = chunks.into_iter().flatten().collect();
        assert_eq!(results.len(), DIFF_TREE_STREAM_CHUNK_SIZE + 2);
        assert!(
            results[DIFF_TREE_STREAM_CHUNK_SIZE - 3]
                .hunks_by_file
                .contains_key("file_48.txt")
        );
        assert_eq!(
            results[DIFF_TREE_STREAM_CHUNK_SIZE - 2],
            DiffTreeResult::default()
        );
        assert_eq!(
            results[DIFF_TREE_STREAM_CHUNK_SIZE - 1],
            DiffTreeResult::default()
        );
        assert_eq!(
            results[DIFF_TREE_STREAM_CHUNK_SIZE],
            DiffTreeResult::default()
        );
        assert!(
            results[DIFF_TREE_STREAM_CHUNK_SIZE + 1]
                .hunks_by_file
                .contains_key("file_52.txt")
        );
    }

    fn authorship_log_for_streaming_merge_test(index: usize) -> AuthorshipLog {
        use crate::authorship::authorship_log::{
            HumanRecord, LineRange, PromptRecord, SessionRecord,
        };
        use crate::authorship::authorship_log_serialization::AttestationEntry;
        use crate::authorship::working_log::AgentId;

        let hash = format!("{index:016x}");
        let mut log = AuthorshipLog::new();
        log.get_or_create_file(&format!("file_{index}.txt"))
            .add_entry(AttestationEntry::new(
                hash.clone(),
                vec![LineRange::Single(1)],
            ));

        let agent_id = AgentId {
            tool: "test".to_string(),
            id: format!("agent-{index}"),
            model: "model".to_string(),
        };
        log.metadata.prompts.insert(
            hash.clone(),
            PromptRecord {
                agent_id: agent_id.clone(),
                human_author: None,
                messages_url: Some(format!("https://example.com/{index}")),
                total_additions: 1,
                total_deletions: 0,
                accepted_lines: 1,
                overriden_lines: 0,
                custom_attributes: None,
            },
        );
        log.metadata.sessions.insert(
            format!("s_{index}"),
            SessionRecord {
                agent_id,
                human_author: None,
                custom_attributes: None,
            },
        );
        log.metadata.humans.insert(
            format!("h_{index}"),
            HumanRecord {
                author: format!("Human {index} <human-{index}@example.com>"),
            },
        );
        log
    }

    #[test]
    fn test_apply_chunk_shifts_merges_same_target_across_streaming_chunks() {
        const SOURCE_COUNT: usize = DIFF_TREE_STREAM_CHUNK_SIZE + 5;
        let target_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let pending: Vec<PendingShift> = (1..=SOURCE_COUNT)
            .map(|index| PendingShift {
                new_sha: target_sha.clone(),
                log: authorship_log_for_streaming_merge_test(index),
            })
            .collect();

        let output = diff_tree_stream_with_identical_pairs(
            SOURCE_COUNT,
            &(1..=SOURCE_COUNT).collect::<Vec<_>>(),
        );
        let mut parser = BatchedDiffTreeParser::new(SOURCE_COUNT);
        let mut pending_iter = pending.into_iter();
        let mut merged_by_target = HashMap::new();
        let mut chunk_lengths = Vec::new();

        for line in output.lines() {
            parser.feed_line(line);
            if parser.completed_count() >= DIFF_TREE_STREAM_CHUNK_SIZE {
                let chunk = parser.take_completed();
                chunk_lengths.push(chunk.len());
                apply_chunk_shifts(&mut merged_by_target, chunk, &mut pending_iter);
            }
        }

        let final_chunk = parser.take_all();
        chunk_lengths.push(final_chunk.len());
        apply_chunk_shifts(&mut merged_by_target, final_chunk, &mut pending_iter);

        assert_eq!(chunk_lengths, vec![DIFF_TREE_STREAM_CHUNK_SIZE, 5]);
        assert_eq!(pending_iter.count(), 0);
        let merged = merged_by_target
            .get(&target_sha)
            .expect("all source logs should merge into the shared target SHA");
        assert_eq!(merged.attestations.len(), SOURCE_COUNT);
        assert_eq!(merged.metadata.prompts.len(), SOURCE_COUNT);
        assert_eq!(merged.metadata.sessions.len(), SOURCE_COUNT);
        assert_eq!(merged.metadata.humans.len(), SOURCE_COUNT);
        assert_eq!(merged.metadata.base_commit_sha, target_sha);

        for index in 1..=SOURCE_COUNT {
            assert!(
                merged
                    .attestations
                    .iter()
                    .any(|attestation| attestation.file_path == format!("file_{index}.txt")),
                "merged target should include file_{index}.txt from source commit {index}"
            );
            assert!(
                merged
                    .metadata
                    .prompts
                    .contains_key(&format!("{index:016x}"))
            );
            assert!(merged.metadata.sessions.contains_key(&format!("s_{index}")));
            assert!(merged.metadata.humans.contains_key(&format!("h_{index}")));
        }
    }
}
