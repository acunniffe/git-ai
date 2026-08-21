use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::stats::{
    stats_for_commit_stats_with_parent_and_authorship, write_stats_to_terminal,
};
use crate::config::{Config, NotesBackendKind};
use crate::error::GitAiError;
use crate::git::repository::Repository;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode},
    execute, queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_BATCH_SIZE: usize = 24;
const GIT_LOG_FIELD_COUNT: usize = 8;
const GIT_LOG_FORMAT: &str = "%H%x00%P%x00%an%x00%ae%x00%aI%x00%D%x00%s%x00%b%x00";

/// Extract recognized Git global flags from args so they can be placed
/// before repository discovery. Everything else is interpreted as a log arg.
///
/// We deliberately skip the ambiguous short forms `-p` (paginate vs patch),
/// `-P` (no-pager vs perl-regexp), `-C` (change-dir vs copy-detection),
/// and bare `-c` (config vs combined-diff).
/// Their long-form equivalents (`--paginate`, `--no-pager`, `--git-dir`,
/// `--work-tree`, `-c key=val`) are handled correctly.
fn extract_git_global_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut global_args: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        // `--` marks the end of options; everything after is a pathspec.
        if arg == "--" {
            rest.extend_from_slice(&args[i..]);
            break;
        }

        // --- Global no-value long options (unambiguous with git log) ---
        if matches!(
            arg.as_str(),
            "--paginate"
                | "--no-pager"
                | "--no-replace-objects"
                | "--no-lazy-fetch"
                | "--no-optional-locks"
                | "--no-advice"
                | "--bare"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
        ) {
            global_args.push(arg.clone());
            i += 1;
            continue;
        }

        // --- Global takes-value long options: --opt=val or --opt val ---
        if matches!(
            arg.as_str(),
            "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--config-env"
                | "--list-cmds"
                | "--attr-source"
                | "--super-prefix"
        ) {
            global_args.push(arg.clone());
            if i + 1 < args.len() {
                global_args.push(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // --exec-path can be standalone (query) or --exec-path=<path> (set)
        if arg == "--exec-path" {
            global_args.push(arg.clone());
            i += 1;
            continue;
        }

        // =<value> forms for all long takes-value options
        if arg.starts_with("--git-dir=")
            || arg.starts_with("--work-tree=")
            || arg.starts_with("--namespace=")
            || arg.starts_with("--config-env=")
            || arg.starts_with("--list-cmds=")
            || arg.starts_with("--attr-source=")
            || arg.starts_with("--super-prefix=")
            || arg.starts_with("--exec-path=")
        {
            global_args.push(arg.clone());
            i += 1;
            continue;
        }

        // -C is deliberately NOT extracted:
        //   git global: -C <path> (change directory before doing anything)
        //   git log:    -C (detect copies, no argument)
        // Since all args arrive after the `log` keyword is stripped, a bare
        // `-C` is far more likely to be copy-detection. Users needing the
        // global form should use `--git-dir` or `--work-tree` instead.

        // -c <key>=<value>: git config override.
        // Git config keys are always `section.variable=value`, so a valid
        // assignment contains a '.' before the first '='.  A bare `-c`
        // without such a token is git log's combined-diff flag, and a next
        // token like `--format=%H` is a log option (no dot in key portion).
        if arg == "-c"
            && i + 1 < args.len()
            && args[i + 1]
                .find('=')
                .is_some_and(|eq| args[i + 1][..eq].contains('.'))
        {
            global_args.push(arg.clone());
            global_args.push(args[i + 1].clone());
            i += 2;
            continue;
        }

        // -c<key>=<value> sticky form — apply same dot-check as the spaced form
        if arg.starts_with("-c")
            && arg.len() > 2
            && arg[2..]
                .find('=')
                .is_some_and(|eq| arg[2..2 + eq].contains('.'))
        {
            global_args.push(arg.clone());
            i += 1;
            continue;
        }

        // -p, -P, and -C are deliberately NOT extracted:
        //   -p = git log --patch (not --paginate)
        //   -P = git log --perl-regexp (not --no-pager)
        //   -C = git log copy-detection (not --git-dir/change-dir)

        // Everything else (including --help, --version, -h, -v, -p, -P, -C,
        // and all git-log options) remains a log arg.
        rest.push(arg.clone());
        i += 1;
    }

    (global_args, rest)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLogArgs {
    git_log_args: Vec<String>,
    plain: bool,
    show_raw_notes: bool,
    oneline: bool,
    show_decorations: bool,
    help: bool,
}

impl Default for ParsedLogArgs {
    fn default() -> Self {
        Self {
            git_log_args: Vec::new(),
            plain: false,
            show_raw_notes: false,
            oneline: false,
            show_decorations: true,
            help: false,
        }
    }
}

/// Handle the `git ai log` command.
///
/// Unlike the old implementation, this does not proxy to `git log --notes=ai`.
/// It streams commits from git with a stable machine-readable format, resolves
/// authorship notes through the configured notes backend, and renders Git AI
/// stats by default. Raw note content is shown only with `--raw` or `--notes`.
pub fn handle_log(args: &[String]) -> ExitStatus {
    match run_log(args) {
        Ok(status) => status,
        Err(LogError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            status_from_code(0)
        }
        Err(error) => {
            eprintln!("git-ai log: {}", error);
            status_from_code(1)
        }
    }
}

fn run_log(args: &[String]) -> Result<ExitStatus, LogError> {
    let (global_args, log_args) = extract_git_global_args(args);
    let parsed = parse_log_args(&log_args).map_err(LogError::Message)?;

    if parsed.help {
        print_log_help();
        return Ok(status_from_code(0));
    }

    if parsed.plain {
        return run_plain_log(&global_args, &parsed.git_log_args);
    }

    let repository_global_args = repository_global_args(&global_args);
    let repo =
        crate::git::repository::find_repository(&repository_global_args).map_err(LogError::Git)?;
    let use_pager = should_use_pager(&global_args);
    let renderer = LogRenderer::new(repo, parsed)?;

    if use_pager {
        run_pager(renderer)?;
    } else {
        stream_to_stdout(renderer)?;
    }
    Ok(status_from_code(0))
}

fn run_plain_log(global_args: &[String], git_log_args: &[String]) -> Result<ExitStatus, LogError> {
    if Config::fresh().notes_backend_kind() != NotesBackendKind::GitNotes {
        return Err(LogError::Message(
            "plain git log --notes=ai only supports the git_notes backend".to_string(),
        ));
    }

    let mut command_args = global_args.to_vec();
    command_args.push("log".to_string());
    command_args.push("--notes=ai".to_string());
    command_args.extend(git_log_args.iter().cloned());

    let mut child = crate::git::repository::spawn_git_passthrough(&command_args)?;
    child.wait().map_err(LogError::Io)
}

fn repository_global_args(global_args: &[String]) -> Vec<String> {
    global_args
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "--paginate" | "--no-pager"))
        .cloned()
        .collect()
}

fn should_use_pager(global_args: &[String]) -> bool {
    if global_args.iter().any(|arg| arg == "--no-pager") {
        return false;
    }
    let forced = global_args.iter().any(|arg| arg == "--paginate");
    forced || std::io::stdout().is_terminal()
}

fn parse_log_args(args: &[String]) -> Result<ParsedLogArgs, String> {
    let mut parsed = ParsedLogArgs::default();
    let mut passthrough = Vec::new();
    let mut after_double_dash = false;
    let plain_requested = contains_plain_flag(args);

    for arg in args {
        if after_double_dash {
            passthrough.push(arg.clone());
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            passthrough.push(arg.clone());
            continue;
        }

        if arg == "--plain" {
            parsed.plain = true;
            continue;
        }

        if plain_requested {
            passthrough.push(arg.clone());
            continue;
        }

        match arg.as_str() {
            "--help" | "-h" => {
                parsed.help = true;
            }
            "--raw" | "--notes" | "--show-notes" => {
                parsed.show_raw_notes = true;
            }
            "--oneline" => {
                parsed.oneline = true;
            }
            "--decorate" | "--decorate=short" | "--decorate=full" | "--decorate=auto" => {
                parsed.show_decorations = true;
            }
            "--no-decorate" => {
                parsed.show_decorations = false;
            }
            _ if is_unsupported_render_arg(arg) => {
                return Err(format!(
                    "unsupported git log rendering option '{}'. `git-ai log` owns rendering so it can show authorship stats; use plain `git log` for this option.",
                    arg
                ));
            }
            _ => passthrough.push(arg.clone()),
        }
    }

    parsed.git_log_args = passthrough;
    Ok(parsed)
}

fn contains_plain_flag(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--plain")
}

fn is_unsupported_render_arg(arg: &str) -> bool {
    matches!(
        arg,
        "--format"
            | "--pretty"
            | "--graph"
            | "--patch"
            | "-p"
            | "--stat"
            | "--shortstat"
            | "--numstat"
            | "--name-only"
            | "--name-status"
            | "--check"
            | "--summary"
            | "--show-signature"
            | "--cc"
            | "-c"
    ) || arg.starts_with("--format=")
        || arg.starts_with("--pretty=")
        || arg.starts_with("--notes=")
        || arg.starts_with("--stat=")
        || arg.starts_with("--patch=")
}

fn print_log_help() {
    println!("Usage: git-ai log [--raw|--notes] [--plain] [git log filters] [--] [pathspecs...]");
    println!();
    println!("Shows commit history with Git AI authorship stats.");
    println!();
    println!("Options:");
    println!("  --raw, --notes    Include raw authorship note data after the stats");
    println!("  --show-notes      Alias for --notes");
    println!("  --plain           Run git log --notes=ai directly (git_notes backend only)");
    println!("  --oneline         Compact commit header");
    println!("  --no-decorate     Hide ref decorations");
    println!("  --no-pager        Stream output instead of opening the pager");
    println!();
    println!("Common git log filters such as -n, --max-count, --author, --grep,");
    println!("--since, --until, revisions, and pathspecs are passed through to git.");
}

struct LogRenderer {
    repo: Repository,
    options: ParsedLogArgs,
    stream: CommitStream,
    ignore_patterns: Vec<String>,
    eof: bool,
}

impl LogRenderer {
    fn new(repo: Repository, options: ParsedLogArgs) -> Result<Self, LogError> {
        let ignore_patterns = effective_ignore_patterns(&repo, &[], &[]);
        let stream = CommitStream::spawn(&repo, &options.git_log_args, options.show_decorations)?;
        Ok(Self {
            repo,
            options,
            stream,
            ignore_patterns,
            eof: false,
        })
    }

    fn render_next_batch(&mut self) -> Result<Vec<String>, LogError> {
        if self.eof {
            return Ok(Vec::new());
        }

        let mut commits = Vec::new();
        for _ in 0..LOG_BATCH_SIZE {
            match self.stream.next_commit()? {
                Some(commit) => commits.push(commit),
                None => {
                    self.eof = true;
                    break;
                }
            }
        }

        if commits.is_empty() {
            return Ok(Vec::new());
        }

        let shas: Vec<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
        let notes = crate::git::notes_api::read_notes_batch(&self.repo, &shas)?;

        Ok(commits
            .iter()
            .map(|commit| {
                let note = notes.get(&commit.sha).map(String::as_str);
                render_commit(
                    &self.repo,
                    commit,
                    note,
                    &self.options,
                    &self.ignore_patterns,
                )
            })
            .collect())
    }

    fn is_eof(&self) -> bool {
        self.eof
    }
}

struct CommitStream {
    child: Option<Child>,
    stdout: BufReader<std::process::ChildStdout>,
}

impl CommitStream {
    fn spawn(
        repo: &Repository,
        git_log_args: &[String],
        show_decorations: bool,
    ) -> Result<Self, LogError> {
        let mut command_args = repo.global_args_for_exec();
        command_args.push("log".to_string());
        command_args.push("--no-color".to_string());
        command_args.push("--no-notes".to_string());
        if show_decorations {
            command_args.push("--decorate=short".to_string());
        } else {
            command_args.push("--no-decorate".to_string());
        }
        command_args.push(format!("--format=format:{}", GIT_LOG_FORMAT));
        command_args.extend(git_log_args.iter().cloned());

        let mut child = crate::git::repository::spawn_git_stdout(&command_args)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LogError::Message("failed to capture git log stdout".to_string()))?;

        Ok(Self {
            child: Some(child),
            stdout: BufReader::new(stdout),
        })
    }

    fn next_commit(&mut self) -> Result<Option<CommitRecord>, LogError> {
        let mut fields = Vec::with_capacity(GIT_LOG_FIELD_COUNT);
        for field_index in 0..GIT_LOG_FIELD_COUNT {
            match read_nul_field(&mut self.stdout)? {
                Some(field) => fields.push(field),
                None if field_index == 0 => {
                    self.wait_for_git_log()?;
                    return Ok(None);
                }
                None => {
                    self.wait_for_git_log()?;
                    return Err(LogError::Message(
                        "malformed git log output: truncated commit record".to_string(),
                    ));
                }
            }
        }

        Ok(Some(CommitRecord {
            sha: normalize_commit_sha_field(&fields[0]),
            parents: fields[1]
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            author_name: fields[2].clone(),
            author_email: fields[3].clone(),
            author_date: fields[4].clone(),
            decorations: fields[5].clone(),
            subject: fields[6].clone(),
            body: fields[7].clone(),
        }))
    }

    fn wait_for_git_log(&mut self) -> Result<(), LogError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let status = child.wait().map_err(LogError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(LogError::Message(format!(
                "git log exited with status {}",
                status
            )))
        }
    }
}

impl Drop for CommitStream {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_nul_field<R: BufRead>(reader: &mut R) -> Result<Option<String>, LogError> {
    let mut bytes = Vec::new();
    let read = reader.read_until(0, &mut bytes).map_err(LogError::Io)?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
}

fn normalize_commit_sha_field(field: &str) -> String {
    field.trim_start_matches('\n').to_string()
}

#[derive(Debug, Clone)]
struct CommitRecord {
    sha: String,
    parents: Vec<String>,
    author_name: String,
    author_email: String,
    author_date: String,
    decorations: String,
    subject: String,
    body: String,
}

fn render_commit(
    repo: &Repository,
    commit: &CommitRecord,
    raw_note: Option<&str>,
    options: &ParsedLogArgs,
    ignore_patterns: &[String],
) -> String {
    let authorship_log =
        raw_note.and_then(|note| AuthorshipLog::deserialize_from_string(note).ok());
    let mut out = String::new();

    if options.oneline {
        out.push_str(&short_sha(&commit.sha));
        out.push(' ');
        out.push_str(&commit.subject);
        if options.show_decorations && !commit.decorations.trim().is_empty() {
            out.push(' ');
            out.push('(');
            out.push_str(commit.decorations.trim());
            out.push(')');
        }
        out.push('\n');
    } else {
        out.push_str("commit ");
        out.push_str(&commit.sha);
        if options.show_decorations && !commit.decorations.trim().is_empty() {
            out.push(' ');
            out.push('(');
            out.push_str(commit.decorations.trim());
            out.push(')');
        }
        out.push('\n');

        if commit.parents.len() > 1 {
            out.push_str("Merge: ");
            out.push_str(
                &commit
                    .parents
                    .iter()
                    .map(|sha| short_sha(sha))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            out.push('\n');
        }

        out.push_str("Author: ");
        out.push_str(&commit.author_name);
        if !commit.author_email.is_empty() {
            out.push_str(" <");
            out.push_str(&commit.author_email);
            out.push('>');
        }
        out.push('\n');
        out.push_str("Date:   ");
        out.push_str(&commit.author_date);
        out.push_str("\n\n");

        append_indented_line(&mut out, &commit.subject, 4);
        let trimmed_body = commit.body.trim_end_matches('\n');
        if !trimmed_body.is_empty() {
            out.push('\n');
            for line in trimmed_body.lines() {
                append_indented_line(&mut out, line, 4);
            }
        }
        out.push('\n');
    }

    out.push_str("    Git AI stats:\n");
    match render_stats(
        repo,
        &commit.sha,
        &commit.parents,
        authorship_log.as_ref(),
        ignore_patterns,
    ) {
        Ok(stats) => append_indented_block(&mut out, &stats, 6),
        Err(message) => append_indented_line(&mut out, &message, 6),
    }

    if options.show_raw_notes {
        out.push('\n');
        out.push_str("    Authorship note:\n");
        match raw_note {
            Some(note) if !note.trim().is_empty() => append_indented_block(&mut out, note, 6),
            _ => append_indented_line(&mut out, "(none)", 6),
        }
    }

    out.push('\n');
    out
}

fn render_stats(
    repo: &Repository,
    commit_sha: &str,
    parents: &[String],
    authorship_log: Option<&AuthorshipLog>,
    ignore_patterns: &[String],
) -> Result<String, String> {
    if parents.len() > 1 {
        return Err("stats skipped for merge commit".to_string());
    }

    let parent_sha = parents
        .first()
        .map(String::as_str)
        .unwrap_or("4b825dc642cb6eb9a060e54bf8d69288fbee4904");

    if let Ok(estimate) = crate::authorship::post_commit::estimate_stats_cost_for_commit_range(
        repo,
        parent_sha,
        commit_sha,
        ignore_patterns,
    ) && estimate.should_skip()
    {
        return Err(format!(
            "stats skipped for large commit; run `git-ai stats {}` to compute on demand",
            commit_sha
        ));
    }

    let stats = stats_for_commit_stats_with_parent_and_authorship(
        repo,
        commit_sha,
        parents.first().map(String::as_str),
        ignore_patterns,
        authorship_log,
    )
    .map_err(|e| format!("stats unavailable: {}", e))?;
    Ok(write_stats_to_terminal(&stats, false))
}

fn append_indented_line(out: &mut String, line: &str, spaces: usize) {
    out.push_str(&" ".repeat(spaces));
    out.push_str(line);
    out.push('\n');
}

fn append_indented_block(out: &mut String, block: &str, spaces: usize) {
    for line in block.trim_end_matches('\n').lines() {
        append_indented_line(out, line, spaces);
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn stream_to_stdout(mut renderer: LogRenderer) -> Result<(), LogError> {
    let stdout = io::stdout();
    let mut lock = stdout.lock();

    loop {
        let rendered = renderer.render_next_batch()?;
        if rendered.is_empty() {
            break;
        }

        for commit in rendered {
            match lock.write_all(commit.as_bytes()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                Err(error) => return Err(LogError::Io(error)),
            }
        }
        lock.flush().map_err(LogError::Io)?;
    }

    Ok(())
}

fn run_pager(renderer: LogRenderer) -> Result<(), LogError> {
    let mut pager = LogPager::new(renderer)?;
    let mut stdout = io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;
    let mut scroll = 0usize;
    let mut needs_redraw = true;
    let mut last_size: Option<(u16, u16)> = None;

    loop {
        let (width, height) = terminal::size().map_err(LogError::Io)?;
        let viewport_height = usize::from(height.saturating_sub(1)).max(1);
        if needs_redraw {
            if last_size.is_some_and(|size| size != (width, height)) {
                queue!(stdout, MoveTo(0, 0), Clear(ClearType::All)).map_err(LogError::Io)?;
            }
            pager.ensure_line_loaded(scroll.saturating_add(viewport_height))?;
            draw_pager(
                &mut stdout,
                &mut pager,
                scroll,
                width,
                height,
                viewport_height,
            )?;
            last_size = Some((width, height));
            needs_redraw = false;
        }

        if !event::poll(std::time::Duration::from_millis(250)).map_err(LogError::Io)? {
            continue;
        }

        match event::read().map_err(LogError::Io)? {
            Event::Key(key) => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('j') | KeyCode::Down => {
                    pager.ensure_line_loaded(scroll.saturating_add(viewport_height + 1))?;
                    if scroll + 1 < pager.line_count() {
                        scroll += 1;
                    }
                    needs_redraw = true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    scroll = scroll.saturating_sub(1);
                    needs_redraw = true;
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    let next = scroll.saturating_add(viewport_height);
                    pager.ensure_line_loaded(next.saturating_add(viewport_height))?;
                    scroll = next.min(pager.max_scroll(viewport_height));
                    needs_redraw = true;
                }
                KeyCode::PageUp => {
                    scroll = scroll.saturating_sub(viewport_height);
                    needs_redraw = true;
                }
                KeyCode::Home => {
                    scroll = 0;
                    needs_redraw = true;
                }
                KeyCode::End => {
                    pager.load_all()?;
                    scroll = pager.max_scroll(viewport_height);
                    needs_redraw = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => {
                needs_redraw = true;
            }
            _ => {}
        }
    }

    Ok(())
}

fn draw_pager(
    stdout: &mut io::Stdout,
    pager: &mut LogPager,
    scroll: usize,
    width: u16,
    height: u16,
    viewport_height: usize,
) -> Result<(), LogError> {
    for row in 0..viewport_height {
        queue!(stdout, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))
            .map_err(LogError::Io)?;
        if let Some(line) = pager.read_line(scroll + row)? {
            queue!(
                stdout,
                Print(truncate_for_width(
                    line.trim_end_matches('\n'),
                    width as usize
                ))
            )
            .map_err(LogError::Io)?;
        }
    }

    let status = if pager.is_eof() {
        format!(
            " git-ai log  lines {}  q quit  ↑/↓ scroll  PgUp/PgDn page ",
            pager.line_count()
        )
    } else {
        format!(
            " git-ai log  loaded {} lines  q quit  ↑/↓ scroll  PgUp/PgDn page ",
            pager.line_count()
        )
    };
    queue!(
        stdout,
        MoveTo(0, height.saturating_sub(1)),
        Clear(ClearType::CurrentLine),
        SetAttribute(Attribute::Reverse),
        Print(pad_for_width(&status, width as usize)),
        SetAttribute(Attribute::Reset)
    )
    .map_err(LogError::Io)?;
    stdout.flush().map_err(LogError::Io)?;
    Ok(())
}

fn truncate_for_width(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible_width = 0usize;
    let mut index = 0usize;
    let mut saw_ansi = false;
    let bytes = line.as_bytes();

    while index < bytes.len() && visible_width < width {
        if bytes[index] == 0x1b
            && let Some(end) = ansi_escape_end(bytes, index)
        {
            out.push_str(&line[index..end]);
            index = end;
            saw_ansi = true;
            continue;
        }

        let Some(ch) = line[index..].chars().next() else {
            break;
        };
        out.push(ch);
        visible_width += 1;
        index += ch.len_utf8();
    }

    if saw_ansi {
        out.push_str("\x1b[0m");
    }

    out
}

fn ansi_escape_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&0x1b) || bytes.get(start + 1) != Some(&b'[') {
        return None;
    }

    for (index, byte) in bytes.iter().enumerate().skip(start + 2) {
        if (0x40..=0x7e).contains(byte) {
            return Some(index + 1);
        }
    }

    None
}

fn pad_for_width(line: &str, width: usize) -> String {
    let mut value = truncate_for_width(line, width);
    let current = value.chars().count();
    if current < width {
        value.push_str(&" ".repeat(width - current));
    }
    value
}

struct LogPager {
    renderer: LogRenderer,
    spool: Spool,
}

impl LogPager {
    fn new(renderer: LogRenderer) -> Result<Self, LogError> {
        Ok(Self {
            renderer,
            spool: Spool::new()?,
        })
    }

    fn ensure_line_loaded(&mut self, line_index: usize) -> Result<(), LogError> {
        while self.spool.line_count() <= line_index && !self.renderer.is_eof() {
            let rendered = self.renderer.render_next_batch()?;
            if rendered.is_empty() {
                break;
            }
            for commit in rendered {
                self.spool.append(&commit)?;
            }
        }
        Ok(())
    }

    fn load_all(&mut self) -> Result<(), LogError> {
        while !self.renderer.is_eof() {
            let rendered = self.renderer.render_next_batch()?;
            if rendered.is_empty() {
                break;
            }
            for commit in rendered {
                self.spool.append(&commit)?;
            }
        }
        Ok(())
    }

    fn read_line(&mut self, line_index: usize) -> Result<Option<String>, LogError> {
        self.spool.read_line(line_index)
    }

    fn line_count(&self) -> usize {
        self.spool.line_count()
    }

    fn is_eof(&self) -> bool {
        self.renderer.is_eof()
    }

    fn max_scroll(&self, viewport_height: usize) -> usize {
        self.line_count().saturating_sub(viewport_height)
    }
}

struct Spool {
    path: PathBuf,
    file: File,
    line_offsets: Vec<u64>,
    write_offset: u64,
}

impl Spool {
    fn new() -> Result<Self, LogError> {
        let path = unique_spool_path();
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(LogError::Io)?;
        Ok(Self {
            path,
            file,
            line_offsets: Vec::new(),
            write_offset: 0,
        })
    }

    fn append(&mut self, text: &str) -> Result<(), LogError> {
        self.file
            .seek(SeekFrom::Start(self.write_offset))
            .map_err(LogError::Io)?;

        if text.is_empty() {
            return Ok(());
        }

        for chunk in text.as_bytes().split_inclusive(|byte| *byte == b'\n') {
            self.line_offsets.push(self.write_offset);
            self.file.write_all(chunk).map_err(LogError::Io)?;
            self.write_offset += chunk.len() as u64;
        }

        if !text.as_bytes().ends_with(b"\n") {
            self.file.write_all(b"\n").map_err(LogError::Io)?;
            self.write_offset += 1;
        }

        self.file.flush().map_err(LogError::Io)?;
        Ok(())
    }

    fn read_line(&mut self, line_index: usize) -> Result<Option<String>, LogError> {
        let Some(offset) = self.line_offsets.get(line_index).copied() else {
            return Ok(None);
        };

        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(LogError::Io)?;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            let read = self.file.read(&mut buf).map_err(LogError::Io)?;
            if read == 0 {
                break;
            }
            bytes.push(buf[0]);
            if buf[0] == b'\n' {
                break;
            }
        }

        Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
    }

    fn line_count(&self) -> usize {
        self.line_offsets.len()
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn unique_spool_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("git-ai-log-{}-{}.tmp", std::process::id(), nanos))
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> Result<Self, LogError> {
        enable_raw_mode().map_err(LogError::Io)?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Clear(ClearType::All), Hide) {
            let _ = disable_raw_mode();
            return Err(LogError::Io(error));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
    }
}

#[derive(Debug)]
enum LogError {
    Git(GitAiError),
    Io(io::Error),
    Message(String),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::Git(error) => write!(f, "{}", error),
            LogError::Io(error) => write!(f, "{}", error),
            LogError::Message(message) => write!(f, "{}", message),
        }
    }
}

impl From<GitAiError> for LogError {
    fn from(value: GitAiError) -> Self {
        LogError::Git(value)
    }
}

impl From<io::Error> for LogError {
    fn from(value: io::Error) -> Self {
        LogError::Io(value)
    }
}

#[cfg(unix)]
fn status_from_code(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn status_from_code(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(code as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // -- double-dash separator --

    #[test]
    fn double_dash_stops_extraction() {
        let (global, rest) = extract_git_global_args(&s(&["--", "--bare"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["--", "--bare"]));
    }

    #[test]
    fn double_dash_after_global_arg() {
        let (global, rest) = extract_git_global_args(&s(&["--paginate", "--", "--bare"]));
        assert_eq!(global, s(&["--paginate"]));
        assert_eq!(rest, s(&["--", "--bare"]));
    }

    #[test]
    fn double_dash_alone() {
        let (global, rest) = extract_git_global_args(&s(&["--"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["--"]));
    }

    #[test]
    fn double_dash_as_last_after_log_args() {
        let (global, rest) = extract_git_global_args(&s(&["--oneline", "--"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["--oneline", "--"]));
    }

    // -- no-value global flags --

    #[test]
    fn no_value_global_flags_extracted() {
        let (global, rest) = extract_git_global_args(&s(&["--paginate", "--oneline"]));
        assert_eq!(global, s(&["--paginate"]));
        assert_eq!(rest, s(&["--oneline"]));
    }

    #[test]
    fn bare_flag_extracted() {
        let (global, rest) = extract_git_global_args(&s(&["--bare", "--graph"]));
        assert_eq!(global, s(&["--bare"]));
        assert_eq!(rest, s(&["--graph"]));
    }

    // -- takes-value global options --

    #[test]
    fn git_dir_spaced_form() {
        let (global, rest) = extract_git_global_args(&s(&["--git-dir", "/some/path", "--oneline"]));
        assert_eq!(global, s(&["--git-dir", "/some/path"]));
        assert_eq!(rest, s(&["--oneline"]));
    }

    #[test]
    fn git_dir_equals_form() {
        let (global, rest) = extract_git_global_args(&s(&["--git-dir=/some/path", "--oneline"]));
        assert_eq!(global, s(&["--git-dir=/some/path"]));
        assert_eq!(rest, s(&["--oneline"]));
    }

    #[test]
    fn takes_value_option_at_end_without_value() {
        let (global, rest) = extract_git_global_args(&s(&["--git-dir"]));
        assert_eq!(global, s(&["--git-dir"]));
        assert!(rest.is_empty());
    }

    // -- exec-path --

    #[test]
    fn exec_path_standalone() {
        let (global, rest) = extract_git_global_args(&s(&["--exec-path", "--oneline"]));
        assert_eq!(global, s(&["--exec-path"]));
        assert_eq!(rest, s(&["--oneline"]));
    }

    #[test]
    fn exec_path_equals_form() {
        let (global, rest) = extract_git_global_args(&s(&["--exec-path=/usr/lib/git", "--graph"]));
        assert_eq!(global, s(&["--exec-path=/usr/lib/git"]));
        assert_eq!(rest, s(&["--graph"]));
    }

    // -- -c config override --

    #[test]
    fn dash_c_with_valid_config_key() {
        let (global, rest) = extract_git_global_args(&s(&["-c", "core.pager=cat", "--oneline"]));
        assert_eq!(global, s(&["-c", "core.pager=cat"]));
        assert_eq!(rest, s(&["--oneline"]));
    }

    #[test]
    fn dash_c_without_dot_is_not_extracted() {
        // bare -c followed by something without section.key=val is git log's combined-diff
        let (global, rest) = extract_git_global_args(&s(&["-c", "foo=bar"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-c", "foo=bar"]));
    }

    #[test]
    fn dash_c_followed_by_log_option() {
        let (global, rest) = extract_git_global_args(&s(&["-c", "--format=%H"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-c", "--format=%H"]));
    }

    #[test]
    fn sticky_c_with_valid_config_key() {
        let (global, rest) = extract_git_global_args(&s(&["-ccore.pager=cat"]));
        assert_eq!(global, s(&["-ccore.pager=cat"]));
        assert!(rest.is_empty());
    }

    #[test]
    fn sticky_c_without_dot_is_not_extracted() {
        // -cC=3 should NOT be extracted — no dot in key portion
        let (global, rest) = extract_git_global_args(&s(&["-cC=3"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-cC=3"]));
    }

    // -- ambiguous short flags are NOT extracted --

    #[test]
    fn dash_capital_c_not_extracted() {
        let (global, rest) = extract_git_global_args(&s(&["-C", "--oneline"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-C", "--oneline"]));
    }

    #[test]
    fn dash_p_not_extracted() {
        let (global, rest) = extract_git_global_args(&s(&["-p"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-p"]));
    }

    #[test]
    fn dash_capital_p_not_extracted() {
        let (global, rest) = extract_git_global_args(&s(&["-P"]));
        assert!(global.is_empty());
        assert_eq!(rest, s(&["-P"]));
    }

    // -- empty args --

    #[test]
    fn empty_args() {
        let (global, rest) = extract_git_global_args(&s(&[]));
        assert!(global.is_empty());
        assert!(rest.is_empty());
    }

    // -- mixed scenarios --

    #[test]
    fn multiple_global_args_with_log_args() {
        let (global, rest) = extract_git_global_args(&s(&[
            "--paginate",
            "-c",
            "core.pager=less",
            "--oneline",
            "--graph",
        ]));
        assert_eq!(global, s(&["--paginate", "-c", "core.pager=less"]));
        assert_eq!(rest, s(&["--oneline", "--graph"]));
    }

    #[test]
    fn global_args_then_double_dash_then_pathspecs() {
        let (global, rest) = extract_git_global_args(&s(&[
            "--no-pager",
            "--git-dir=/repo",
            "--oneline",
            "--",
            "src/",
            "--bare",
        ]));
        assert_eq!(global, s(&["--no-pager", "--git-dir=/repo"]));
        assert_eq!(rest, s(&["--oneline", "--", "src/", "--bare"]));
    }

    #[test]
    fn log_raw_flag_is_consumed() {
        let parsed = parse_log_args(&s(&["--raw", "-n", "1"])).unwrap();
        assert!(parsed.show_raw_notes);
        assert_eq!(parsed.git_log_args, s(&["-n", "1"]));
    }

    #[test]
    fn log_notes_flag_is_consumed() {
        let parsed = parse_log_args(&s(&["--notes", "--author=me"])).unwrap();
        assert!(parsed.show_raw_notes);
        assert_eq!(parsed.git_log_args, s(&["--author=me"]));
    }

    #[test]
    fn log_show_notes_alias_is_consumed() {
        let parsed = parse_log_args(&s(&["--show-notes", "--author=me"])).unwrap();
        assert!(parsed.show_raw_notes);
        assert_eq!(parsed.git_log_args, s(&["--author=me"]));
    }

    #[test]
    fn plain_mode_consumes_only_plain_flag() {
        let parsed =
            parse_log_args(&s(&["--plain", "--raw", "--format=%H", "--max-count=2"])).unwrap();
        assert!(parsed.plain);
        assert!(!parsed.show_raw_notes);
        assert_eq!(
            parsed.git_log_args,
            s(&["--raw", "--format=%H", "--max-count=2"])
        );
    }

    #[test]
    fn plain_pathspec_after_double_dash_is_not_interpreted() {
        let parsed = parse_log_args(&s(&["--", "--plain"])).unwrap();
        assert!(!parsed.plain);
        assert_eq!(parsed.git_log_args, s(&["--", "--plain"]));
    }

    #[test]
    fn plain_mode_allows_git_render_flags() {
        let parsed = parse_log_args(&s(&["--plain", "--graph", "--patch"])).unwrap();
        assert!(parsed.plain);
        assert_eq!(parsed.git_log_args, s(&["--graph", "--patch"]));
    }

    #[test]
    fn oneline_is_consumed() {
        let parsed = parse_log_args(&s(&["--oneline", "--max-count=2"])).unwrap();
        assert!(parsed.oneline);
        assert_eq!(parsed.git_log_args, s(&["--max-count=2"]));
    }

    #[test]
    fn unsupported_render_flag_errors() {
        let err = parse_log_args(&s(&["--graph"])).unwrap_err();
        assert!(err.contains("unsupported git log rendering option"));
    }

    #[test]
    fn pathspec_after_double_dash_is_not_interpreted() {
        let parsed = parse_log_args(&s(&["--", "--graph"])).unwrap();
        assert_eq!(parsed.git_log_args, s(&["--", "--graph"]));
    }

    #[test]
    fn pager_globals_are_not_kept_for_repository_commands() {
        assert_eq!(
            repository_global_args(&s(&["--paginate", "--no-pager", "--bare"])),
            s(&["--bare"])
        );
    }

    #[test]
    fn truncate_for_width_preserves_ansi_escape_sequences() {
        let truncated = truncate_for_width("\x1b[90mabcdef\x1b[0m", 3);
        assert_eq!(truncated, "\x1b[90mabc\x1b[0m");
    }

    #[test]
    fn truncate_for_width_does_not_cut_incomplete_ansi_reset() {
        let truncated = truncate_for_width("\x1b[90mabc\x1b[0mdef", 3);
        assert_eq!(truncated, "\x1b[90mabc\x1b[0m");
    }
}
