use rand::{Rng, RngExt};

use crate::repos::test_repo::TestRepo;

use super::model::{AttrRecord, AttrRegistry, FileModel, LineAttribution};

pub struct CharAllocator {
    next: u32,
}

impl CharAllocator {
    pub fn new() -> Self {
        Self { next: 0x4E00 }
    }

    pub fn alloc(&mut self) -> char {
        let ch = char::from_u32(self.next).unwrap_or('?');
        self.next += 1;
        ch
    }
}

/// Edit the file: insert, append, replace, or delete lines.
/// Returns the chars that were written (for checkpointing).
/// All new chars start as Untracked in the registry until checkpointed.
pub fn random_edit(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    alloc: &mut CharAllocator,
    rng: &mut impl Rng,
    max_lines: usize,
) -> Vec<char> {
    let num_lines = rng.random_range(1..=max_lines);
    let new_chars: Vec<char> = (0..num_lines).map(|_| alloc.alloc()).collect();

    // Register all new chars as Untracked initially
    for &ch in &new_chars {
        registry.register(ch, LineAttribution::Untracked);
    }

    if model.lines.is_empty() {
        for &ch in &new_chars {
            model.lines.push(ch);
            model.resolved_attrs.push(LineAttribution::Untracked);
            model.resolved_ai_sessions.push(None);
        }
    } else {
        let strategy = rng.random_range(0..4);
        match strategy {
            0 => {
                let pos = rng.random_range(0..=model.lines.len());
                for (j, &ch) in new_chars.iter().enumerate() {
                    model.lines.insert(pos + j, ch);
                    model
                        .resolved_attrs
                        .insert(pos + j, LineAttribution::Untracked);
                    model.resolved_ai_sessions.insert(pos + j, None);
                }
            }
            1 => {
                let start = rng.random_range(0..model.lines.len());
                let end = (start + num_lines).min(model.lines.len());
                let replace_count = end - start;
                for (j, &ch) in new_chars.iter().take(replace_count).enumerate() {
                    model.lines[start + j] = ch;
                    model.resolved_attrs[start + j] = LineAttribution::Untracked;
                    model.resolved_ai_sessions[start + j] = None;
                }
                for &ch in new_chars.iter().skip(replace_count) {
                    model.lines.insert(end, ch);
                    model.resolved_attrs.insert(end, LineAttribution::Untracked);
                    model.resolved_ai_sessions.insert(end, None);
                }
            }
            2 => {
                for &ch in &new_chars {
                    model.lines.push(ch);
                    model.resolved_attrs.push(LineAttribution::Untracked);
                    model.resolved_ai_sessions.push(None);
                }
            }
            3 => {
                if model.lines.len() > 1 {
                    let del_count = rng.random_range(1..model.lines.len().min(4));
                    let del_start = rng.random_range(0..model.lines.len() - del_count + 1);
                    model.lines.drain(del_start..del_start + del_count);
                    model.resolved_attrs.drain(del_start..del_start + del_count);
                    model
                        .resolved_ai_sessions
                        .drain(del_start..del_start + del_count);
                }
                let pos = if model.lines.is_empty() {
                    0
                } else {
                    rng.random_range(0..=model.lines.len())
                };
                for (j, &ch) in new_chars.iter().enumerate() {
                    model.lines.insert(pos + j, ch);
                    model
                        .resolved_attrs
                        .insert(pos + j, LineAttribution::Untracked);
                    model.resolved_ai_sessions.insert(pos + j, None);
                }
            }
            _ => unreachable!(),
        }
    }

    model.write_to_disk(repo);
    new_chars
}

/// Checkpoint as AI — marks the written chars as Ai in registry and model.
pub fn checkpoint_ai(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    written_chars: &[char],
    op_log: &mut Vec<String>,
) {
    repo.git_ai(&["checkpoint", "mock_ai", &model.filename])
        .unwrap_or_else(|e| panic!("checkpoint mock_ai failed: {}", e));

    let session = registry.allocate_ai_session();
    for &ch in written_chars {
        let record = AttrRecord::ai(session);
        registry.register_record(ch, record);
        model.mark_pending_attestation(ch, record);
    }
    for (i, &ch) in model.lines.iter().enumerate() {
        if written_chars.contains(&ch) {
            model.resolved_attrs[i] = LineAttribution::Ai;
            model.resolved_ai_sessions[i] = Some(session);
        }
    }
    op_log.push(format!("checkpoint_ai({})", model.filename));
}

/// Checkpoint as known human — marks the written chars as KnownHuman.
pub fn checkpoint_human(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    written_chars: &[char],
    op_log: &mut Vec<String>,
) {
    repo.git_ai(&["checkpoint", "mock_known_human", &model.filename])
        .unwrap_or_else(|e| panic!("checkpoint mock_known_human failed: {}", e));

    for &ch in written_chars {
        let record = AttrRecord::new(LineAttribution::KnownHuman);
        registry.register_record(ch, record);
        model.mark_pending_attestation(ch, record);
    }
    for (i, &ch) in model.lines.iter().enumerate() {
        if written_chars.contains(&ch) {
            model.resolved_attrs[i] = LineAttribution::KnownHuman;
            model.resolved_ai_sessions[i] = None;
        }
    }
    op_log.push(format!("checkpoint_human({})", model.filename));
}

/// Checkpoint as untracked (legacy "human" checkpoint).
pub fn checkpoint_untracked(model: &FileModel, repo: &TestRepo, op_log: &mut Vec<String>) {
    repo.git_ai(&["checkpoint", "human", &model.filename])
        .unwrap_or_else(|e| panic!("checkpoint human failed: {}", e));
    op_log.push(format!("checkpoint_untracked({})", model.filename));
}

/// Commit: stage all and commit. Then reconcile and assert.
pub fn commit(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    op_log: &mut Vec<String>,
    seed: u64,
    msg: &str,
) {
    repo.git(&["add", "."]).unwrap();
    let added_lines = staged_added_lines(repo, &model.filename, Some("HEAD"));
    repo.git(&["commit", "-m", msg, "--allow-empty"])
        .unwrap_or_else(|e| panic!("commit '{}' failed: {}", msg, e));

    op_log.push(format!("commit(\"{}\")", msg));
    model.apply_edge_recovery_for_added_lines(registry, &added_lines);
    model.apply_known_human_recovery_for_added_lines(registry, &added_lines);
    model.apply_pure_human_recovery_for_added_lines(registry, &added_lines);
    model.reconcile(repo);
    model.assert_blame(repo, op_log, seed);
    model.clear_pending_attestations();
}

/// Amend the last commit. Then reconcile and assert.
pub fn amend(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    op_log: &mut Vec<String>,
    seed: u64,
) {
    repo.git(&["add", "."]).unwrap();
    let parent = repo
        .git(&["rev-parse", "--verify", "HEAD^"])
        .ok()
        .map(|output| output.trim().to_string());
    let added_lines = staged_added_lines(repo, &model.filename, parent.as_deref());
    repo.git(&["commit", "--amend", "--no-edit"])
        .unwrap_or_else(|e| panic!("amend failed: {}", e));

    op_log.push("amend".to_string());
    model.sync_from_disk(repo, registry);
    model.apply_edge_recovery_for_added_lines(registry, &added_lines);
    model.apply_known_human_recovery_for_added_lines(registry, &added_lines);
    model.apply_pure_human_recovery_for_added_lines(registry, &added_lines);
    model.reconcile(repo);
    model.assert_blame(repo, op_log, seed);
    model.clear_pending_attestations();
}

/// Rebase: creates a side branch with commits, then rebases onto main.
pub fn rebase(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    alloc: &mut CharAllocator,
    rng: &mut impl Rng,
    op_log: &mut Vec<String>,
    seed: u64,
) {
    let main_branch = repo
        .git(&["branch", "--show-current"])
        .unwrap()
        .trim()
        .to_string();

    // Create a commit on main first (so rebase has something to replay onto)
    let chars = random_edit(model, registry, repo, alloc, rng, 2);
    checkpoint_ai(model, registry, repo, &chars, op_log);
    commit(model, registry, repo, op_log, seed, "rebase: main advance");

    // Create side branch from parent
    let parent = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "rebase-side", &parent])
        .unwrap();

    // Sync model to side branch state (parent's file content)
    model.sync_from_disk(repo, registry);

    // Make a commit on the side branch
    let side_chars = random_edit(model, registry, repo, alloc, rng, 2);
    checkpoint_ai(model, registry, repo, &side_chars, op_log);
    repo.git(&["add", "."]).unwrap();
    let side_added_lines = staged_added_lines(repo, &model.filename, Some("HEAD"));
    repo.git(&["commit", "-m", "rebase: side commit"]).unwrap();
    model.apply_edge_recovery_for_added_lines(registry, &side_added_lines);
    model.apply_known_human_recovery_for_added_lines(registry, &side_added_lines);
    model.apply_pure_human_recovery_for_added_lines(registry, &side_added_lines);
    model.clear_pending_attestations();
    op_log.push("commit(\"rebase: side commit\")".to_string());

    // Rebase side onto main
    let result = repo.git(&["rebase", &main_branch]);
    if result.is_err() {
        let _ = repo.git(&["rebase", "--abort"]);
        repo.git(&["checkout", &main_branch]).unwrap();
        let _ = repo.git(&["branch", "-D", "rebase-side"]);
        model.sync_from_disk(repo, registry);
        model.reconcile(repo);
        model.assert_blame(repo, op_log, seed);
        op_log.push("rebase(aborted due to conflict)".to_string());
        return;
    }

    // Merge side back to main (fast-forward)
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.git(&["merge", "rebase-side"]).unwrap();
    let _ = repo.git(&["branch", "-D", "rebase-side"]);

    op_log.push("rebase(success)".to_string());
    model.sync_from_disk(repo, registry);
    model.reconcile(repo);
    model.assert_blame(repo, op_log, seed);
}

/// Cherry-pick: creates a commit on a side branch, then cherry-picks it onto main.
pub fn cherry_pick(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    alloc: &mut CharAllocator,
    rng: &mut impl Rng,
    op_log: &mut Vec<String>,
    seed: u64,
) {
    let main_branch = repo
        .git(&["branch", "--show-current"])
        .unwrap()
        .trim()
        .to_string();

    // Create side branch from current HEAD
    repo.git(&["checkout", "-b", "cherry-side"]).unwrap();

    // Make a commit on side
    let chars = random_edit(model, registry, repo, alloc, rng, 2);
    checkpoint_ai(model, registry, repo, &chars, op_log);
    repo.git(&["add", "."]).unwrap();
    let side_added_lines = staged_added_lines(repo, &model.filename, Some("HEAD"));
    repo.git(&["commit", "-m", "cherry-pick: side commit"])
        .unwrap();
    model.apply_edge_recovery_for_added_lines(registry, &side_added_lines);
    model.apply_known_human_recovery_for_added_lines(registry, &side_added_lines);
    model.apply_pure_human_recovery_for_added_lines(registry, &side_added_lines);
    model.clear_pending_attestations();
    let side_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Go back to main
    repo.git(&["checkout", &main_branch]).unwrap();
    model.sync_from_disk(repo, registry);

    // Cherry-pick the side commit
    let result = repo.git(&["cherry-pick", &side_sha]);
    let _ = repo.git(&["branch", "-D", "cherry-side"]);

    if result.is_err() {
        let _ = repo.git(&["cherry-pick", "--abort"]);
        op_log.push("cherry_pick(aborted due to conflict)".to_string());
        model.sync_from_disk(repo, registry);
        model.reconcile(repo);
        model.assert_blame(repo, op_log, seed);
        return;
    }

    op_log.push("cherry_pick(success)".to_string());
    model.sync_from_disk(repo, registry);
    model.reconcile(repo);
    model.assert_blame(repo, op_log, seed);
}

/// Soft-reset the last commit and immediately re-commit the same content.
///
/// First principles: `reset --soft HEAD~1` un-commits the tip but leaves the
/// tree and index untouched. git-ai must reconstruct the undone commit's
/// attribution into the working log (rewrite_reset.rs), so re-committing the
/// identical tree must reproduce identical blame. The model is unchanged by a
/// soft-reset+recommit round trip, so we assert it is byte-for-byte preserved.
///
/// Skips when HEAD has no parent (nothing to un-commit).
pub fn soft_reset_recommit(
    model: &mut FileModel,
    registry: &AttrRegistry,
    repo: &TestRepo,
    op_log: &mut Vec<String>,
    seed: u64,
) {
    if repo.git(&["rev-parse", "HEAD~1"]).is_err() {
        op_log.push("soft_reset_recommit(skipped: no parent)".to_string());
        return;
    }

    repo.git(&["reset", "--soft", "HEAD~1"])
        .unwrap_or_else(|e| panic!("reset --soft failed: {}", e));
    op_log.push("reset --soft HEAD~1".to_string());

    // Tree/index are intact; re-commit reproduces the same content.
    repo.git(&["commit", "-m", "soft reset recommit", "--allow-empty"])
        .unwrap_or_else(|e| panic!("recommit after soft reset failed: {}", e));
    op_log.push("commit(\"soft reset recommit\")".to_string());

    model.sync_from_disk(repo, registry);
    model.reconcile(repo);
    model.assert_blame(repo, op_log, seed);
}

/// Stash all uncommitted changes, pop them back, then commit and assert.
///
/// First principles: a stash push/pop round trip must preserve the attribution
/// of uncommitted lines without reading the live worktree after the fact
/// (rewrite_stash.rs). git-ai blame only materializes attribution for committed
/// content (uncommitted lines always show "Not Committed Yet"), so the round
/// trip is followed by a commit before asserting — the model is unchanged by
/// push+pop, so committed blame must match it exactly.
pub fn stash_roundtrip(
    model: &mut FileModel,
    registry: &mut AttrRegistry,
    repo: &TestRepo,
    alloc: &mut CharAllocator,
    rng: &mut impl Rng,
    op_log: &mut Vec<String>,
    seed: u64,
) {
    // Produce a checkpointed-AI uncommitted change to stash.
    let chars = random_edit(model, registry, repo, alloc, rng, 2);
    checkpoint_ai(model, registry, repo, &chars, op_log);

    // Nothing to stash if the worktree is clean relative to HEAD.
    let status = repo.git(&["status", "--porcelain"]).unwrap_or_default();
    if status.trim().is_empty() {
        op_log.push("stash_roundtrip(skipped: clean worktree)".to_string());
        return;
    }

    repo.git(&["stash", "push", "-m", "fuzz stash"])
        .unwrap_or_else(|e| panic!("stash push failed: {}", e));
    op_log.push("stash push".to_string());

    repo.git(&["stash", "pop"])
        .unwrap_or_else(|e| panic!("stash pop failed: {}", e));
    op_log.push("stash pop".to_string());

    // Commit so blame can render the restored attribution, then assert.
    model.sync_from_disk(repo, registry);
    commit(
        model,
        registry,
        repo,
        op_log,
        seed,
        "stash roundtrip commit",
    );
}

fn staged_added_lines(repo: &TestRepo, filename: &str, base: Option<&str>) -> Vec<u32> {
    let mut args = vec![
        "diff".to_string(),
        "--cached".to_string(),
        "--unified=0".to_string(),
    ];
    if let Some(base) = base {
        args.push(base.to_string());
    }
    args.push("--".to_string());
    args.push(filename.to_string());

    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    let diff = repo.git(&arg_refs).unwrap_or_default();
    parse_diff_added_lines(&diff)
}

fn parse_diff_added_lines(diff: &str) -> Vec<u32> {
    let mut added = Vec::new();
    let mut next_new_line = None::<u32>;

    for line in diff.lines() {
        if line.starts_with("@@") {
            next_new_line = parse_hunk_new_start(line);
            continue;
        }

        let Some(current) = next_new_line else {
            continue;
        };

        if line.starts_with("+++") || line.starts_with("\\ No newline") {
            continue;
        }

        if line.starts_with('+') {
            added.push(current);
            next_new_line = Some(current + 1);
        } else if line.starts_with('-') {
            next_new_line = Some(current);
        } else {
            next_new_line = Some(current + 1);
        }
    }

    added
}

fn parse_hunk_new_start(header: &str) -> Option<u32> {
    let plus = header.find('+')?;
    let after_plus = &header[plus + 1..];
    let end = after_plus.find([' ', '@']).unwrap_or(after_plus.len());
    let range = &after_plus[..end];
    range.split(',').next()?.parse().ok()
}
