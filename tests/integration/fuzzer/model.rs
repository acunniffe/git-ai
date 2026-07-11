use std::collections::HashMap;
use std::fmt;
use std::fs;

use crate::repos::test_repo::TestRepo;

use super::helpers::{BlameClass, classify_show_prompt_author, parse_blame_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAttribution {
    Ai,
    KnownHuman,
    Untracked,
}

impl fmt::Display for LineAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineAttribution::Ai => write!(f, "Ai"),
            LineAttribution::KnownHuman => write!(f, "KnownHuman"),
            LineAttribution::Untracked => write!(f, "Untracked"),
        }
    }
}

impl LineAttribution {
    fn expected_blame_class(self) -> BlameClass {
        match self {
            LineAttribution::Ai => BlameClass::Ai,
            LineAttribution::KnownHuman => BlameClass::KnownHuman,
            LineAttribution::Untracked => BlameClass::Untracked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttrRecord {
    pub attr: LineAttribution,
    pub ai_session: Option<u64>,
}

impl AttrRecord {
    pub fn new(attr: LineAttribution) -> Self {
        Self {
            attr,
            ai_session: None,
        }
    }

    pub fn ai(session: u64) -> Self {
        Self {
            attr: LineAttribution::Ai,
            ai_session: Some(session),
        }
    }
}

/// Global registry: maps each unique char to its CHECKPOINT-TIME attribution.
/// This never forgets — once a char is registered, its original attribution is preserved.
/// Reconciliation can downgrade it to Untracked in the FileModel, but the registry
/// always remembers what was checkpointed.
#[derive(Debug, Clone)]
pub struct AttrRegistry {
    map: HashMap<char, AttrRecord>,
    next_ai_session: u64,
}

impl AttrRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_ai_session: 1,
        }
    }

    pub fn register(&mut self, ch: char, attr: LineAttribution) {
        self.map.insert(ch, AttrRecord::new(attr));
    }

    pub fn register_record(&mut self, ch: char, record: AttrRecord) {
        self.map.insert(ch, record);
    }

    pub fn get(&self, ch: char) -> LineAttribution {
        self.get_record(ch).attr
    }

    pub fn get_record(&self, ch: char) -> AttrRecord {
        self.map
            .get(&ch)
            .copied()
            .unwrap_or_else(|| AttrRecord::new(LineAttribution::Untracked))
    }

    pub fn allocate_ai_session(&mut self) -> u64 {
        let session = self.next_ai_session;
        self.next_ai_session += 1;
        session
    }
}

/// The current state of a file as the fuzzer understands it.
/// `lines` contains one char per line — the char identifies the line uniquely.
/// Attribution is looked up from the AttrRegistry + reconciliation state.
#[derive(Debug, Clone)]
pub struct FileModel {
    pub filename: String,
    pub lines: Vec<char>,
    /// Per-line attribution predicted by the model. This is what we assert against.
    /// Reconciliation must not inspect git-ai's actual notes; missing notes are
    /// implementation failures, not new expected behavior.
    pub resolved_attrs: Vec<LineAttribution>,
    pub resolved_ai_sessions: Vec<Option<u64>>,
    pending_attestations: HashMap<char, AttrRecord>,
}

impl FileModel {
    pub fn new(filename: &str) -> Self {
        Self {
            filename: filename.to_string(),
            lines: Vec::new(),
            resolved_attrs: Vec::new(),
            resolved_ai_sessions: Vec::new(),
            pending_attestations: HashMap::new(),
        }
    }

    pub fn write_to_disk(&self, repo: &TestRepo) {
        let content: String = self.lines.iter().map(|ch| format!("{}\n", ch)).collect();
        fs::write(repo.path().join(&self.filename), content).unwrap();
    }

    /// Re-read file content from disk. Updates `lines` to match what's on disk.
    /// Then rebuilds `resolved_attrs` from the registry (before reconciliation).
    pub fn sync_from_disk(&mut self, repo: &TestRepo, registry: &AttrRegistry) {
        let path = repo.path().join(&self.filename);
        if !path.exists() {
            self.lines.clear();
            self.resolved_attrs.clear();
            self.resolved_ai_sessions.clear();
            return;
        }
        let content = fs::read_to_string(&path).unwrap();
        self.lines = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().next().unwrap_or('?'))
            .collect();
        let records = self
            .lines
            .iter()
            .map(|&ch| registry.get_record(ch))
            .collect::<Vec<_>>();
        self.resolved_attrs = records.iter().map(|record| record.attr).collect();
        self.resolved_ai_sessions = records.iter().map(|record| record.ai_session).collect();
    }

    /// Reconcile hook retained for operation flow symmetry. The model is the
    /// oracle, so this intentionally does not read git blame or authorship notes.
    pub fn reconcile(&mut self, _repo: &TestRepo) {
        let records = self
            .lines
            .iter()
            .map(|&ch| self.resolved_record(ch))
            .collect::<Vec<_>>();
        self.resolved_attrs = records.iter().map(|record| record.attr).collect();
        self.resolved_ai_sessions = records.iter().map(|record| record.ai_session).collect();
    }

    fn resolved_record(&self, ch: char) -> AttrRecord {
        self.lines
            .iter()
            .enumerate()
            .find_map(|(idx, &candidate)| {
                (candidate == ch).then_some(AttrRecord {
                    attr: self.resolved_attrs[idx],
                    ai_session: self.resolved_ai_sessions[idx],
                })
            })
            .unwrap_or_else(|| AttrRecord::new(LineAttribution::Untracked))
    }

    pub fn apply_edge_recovery_for_added_lines(
        &mut self,
        registry: &mut AttrRegistry,
        added_lines: &[u32],
    ) {
        const EDGE_EXTENSION_MAX_LINES: usize = 3;

        let mut unknown = added_lines
            .iter()
            .filter_map(|line| line.checked_sub(1).map(|idx| idx as usize))
            .filter(|&idx| {
                idx < self.resolved_attrs.len()
                    && self.resolved_attrs[idx] == LineAttribution::Untracked
                    && !self.pending_attestations.contains_key(&self.lines[idx])
            })
            .collect::<Vec<_>>();
        unknown.sort_unstable();
        unknown.dedup();

        let mut start = 0;
        while start < unknown.len() {
            let mut end = start + 1;
            while end < unknown.len() && unknown[end] == unknown[end - 1] + 1 {
                end += 1;
            }

            let run = &unknown[start..end];
            let first = run[0];
            let last = *run.last().unwrap();
            let prev = first
                .checked_sub(1)
                .and_then(|idx| self.pending_record_at_index(idx));
            let next = self.pending_record_at_index(last + 1);

            let recovery = match (prev, next) {
                (Some(left), Some(right))
                    if left.attr == LineAttribution::Ai
                        && right.attr == LineAttribution::Ai
                        && left.ai_session.is_some()
                        && left.ai_session == right.ai_session =>
                {
                    let mut lines = run
                        .iter()
                        .take(EDGE_EXTENSION_MAX_LINES)
                        .copied()
                        .collect::<Vec<_>>();
                    lines.extend(run.iter().rev().take(EDGE_EXTENSION_MAX_LINES).copied());
                    lines.sort_unstable();
                    lines.dedup();
                    Some((left.ai_session.unwrap(), lines))
                }
                (Some(left), None)
                    if left.attr == LineAttribution::Ai && left.ai_session.is_some() =>
                {
                    Some((
                        left.ai_session.unwrap(),
                        run.iter().take(EDGE_EXTENSION_MAX_LINES).copied().collect(),
                    ))
                }
                (None, Some(right))
                    if right.attr == LineAttribution::Ai && right.ai_session.is_some() =>
                {
                    Some((
                        right.ai_session.unwrap(),
                        run.iter()
                            .rev()
                            .take(EDGE_EXTENSION_MAX_LINES)
                            .copied()
                            .collect(),
                    ))
                }
                _ => None,
            };

            if let Some((session, recovered_indices)) = recovery {
                for idx in recovered_indices {
                    self.resolved_attrs[idx] = LineAttribution::Ai;
                    self.resolved_ai_sessions[idx] = Some(session);
                    registry.register_record(self.lines[idx], AttrRecord::ai(session));
                }
            }

            start = end;
        }
    }

    pub fn apply_known_human_recovery_for_added_lines(
        &mut self,
        registry: &mut AttrRegistry,
        added_lines: &[u32],
    ) {
        let mut added_indices = added_lines
            .iter()
            .filter_map(|line| line.checked_sub(1).map(|idx| idx as usize))
            .filter(|&idx| idx < self.resolved_attrs.len())
            .collect::<Vec<_>>();
        added_indices.sort_unstable();
        added_indices.dedup();

        if !added_indices
            .iter()
            .any(|&idx| self.resolved_attrs[idx] == LineAttribution::KnownHuman)
        {
            return;
        }

        for idx in added_indices {
            if self.resolved_attrs[idx] == LineAttribution::Untracked {
                self.resolved_attrs[idx] = LineAttribution::KnownHuman;
                self.resolved_ai_sessions[idx] = None;
                registry.register_record(
                    self.lines[idx],
                    AttrRecord::new(LineAttribution::KnownHuman),
                );
            }
        }
    }

    pub fn apply_pure_human_recovery_for_added_lines(
        &mut self,
        registry: &mut AttrRegistry,
        added_lines: &[u32],
    ) {
        let mut added_indices = added_lines
            .iter()
            .filter_map(|line| line.checked_sub(1).map(|idx| idx as usize))
            .filter(|&idx| idx < self.resolved_attrs.len())
            .collect::<Vec<_>>();
        added_indices.sort_unstable();
        added_indices.dedup();

        if added_indices.is_empty()
            || added_indices
                .iter()
                .any(|&idx| self.resolved_attrs[idx] == LineAttribution::Ai)
        {
            return;
        }

        for idx in added_indices {
            if self.resolved_attrs[idx] == LineAttribution::Untracked {
                self.resolved_attrs[idx] = LineAttribution::KnownHuman;
                self.resolved_ai_sessions[idx] = None;
                registry.register_record(
                    self.lines[idx],
                    AttrRecord::new(LineAttribution::KnownHuman),
                );
            }
        }
    }

    fn pending_record_at_index(&self, idx: usize) -> Option<AttrRecord> {
        self.lines
            .get(idx)
            .and_then(|ch| self.pending_attestations.get(ch))
            .copied()
    }

    pub fn mark_pending_attestation(&mut self, ch: char, record: AttrRecord) {
        self.pending_attestations.insert(ch, record);
    }

    pub fn clear_pending_attestations(&mut self) {
        self.pending_attestations.clear();
    }

    /// Assert that git-ai blame output matches our model EXACTLY.
    /// Every line. Every time. No exceptions.
    pub fn assert_blame(&self, repo: &TestRepo, op_log: &[String], seed: u64) {
        let path = repo.path().join(&self.filename);
        if !path.exists() || self.lines.is_empty() {
            return;
        }

        // --show-prompt surfaces all three attribution classes in the author
        // column: agent tool name for AI, h_-prefixed hash for known-human, plain
        // git author for untracked. Plain blame collapses the latter two.
        let blame_output = match repo.git_ai(&["blame", "--show-prompt", &self.filename]) {
            Ok(output) => output,
            Err(e) => {
                panic!(
                    "git-ai blame failed for '{}'\nSeed: {}\nError: {}\nOp log:\n{}\nModel:\n{}",
                    self.filename,
                    seed,
                    e,
                    op_log.join("\n"),
                    self.dump()
                );
            }
        };

        let blame_lines: Vec<&str> = blame_output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();

        if blame_lines.len() != self.lines.len() {
            panic!(
                "Line count mismatch for '{}'\nSeed: {}\n\
                 Blame lines: {}\nModel lines: {}\n\
                 Op log:\n{}\nModel:\n{}",
                self.filename,
                seed,
                blame_lines.len(),
                self.lines.len(),
                op_log.join("\n"),
                self.dump()
            );
        }

        for (i, (blame_line, &expected_attr)) in
            blame_lines.iter().zip(&self.resolved_attrs).enumerate()
        {
            let line_num = i + 1;
            let (author, _content) = parse_blame_line(blame_line);
            let actual_class = classify_show_prompt_author(&author);
            let expected_class = expected_attr.expected_blame_class();

            if expected_class != actual_class {
                panic!(
                    "Attribution mismatch on line {} of '{}'\n\
                     Seed: {}\n\
                     Char: '{}'\n\
                     Model says: {:?} (expected class {:?})\n\
                     Blame shows: author='{}' (actual class {:?})\n\
                     Blame line: {}\n\
                     Full blame:\n{}\n\
                     Op log:\n{}\n\
                     Model:\n{}",
                    line_num,
                    self.filename,
                    seed,
                    self.lines[i],
                    expected_attr,
                    expected_class,
                    author,
                    actual_class,
                    blame_line,
                    blame_output,
                    op_log.join("\n"),
                    self.dump()
                );
            }
        }
    }

    pub fn dump(&self) -> String {
        let mut out = format!("File: {} ({} lines)\n", self.filename, self.lines.len());
        for (i, (&ch, &attr)) in self.lines.iter().zip(&self.resolved_attrs).enumerate() {
            let session = self.resolved_ai_sessions[i]
                .map(|session| format!(" #{session}"))
                .unwrap_or_default();
            out.push_str(&format!("  L{}: '{}' -> {}{}\n", i + 1, ch, attr, session));
        }
        out
    }
}
