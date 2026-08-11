use crate::daemon::domain::FamilyKey;
use crate::error::GitAiError;
use crate::git::cli_parser::parse_git_cli_args;
use crate::git::repo_state::{common_dir_for_repo_path, common_dir_for_worktree};
use crate::git::repository::discover_repository_in_path_no_git_exec;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub trait GitBackend: Send + Sync + 'static {
    fn resolve_family(&self, worktree: &Path) -> Result<FamilyKey, GitAiError>;

    fn resolve_primary_command(
        &self,
        worktree: &Path,
        argv: &[String],
    ) -> Result<Option<String>, GitAiError>;

    /// Resolve the fully alias-expanded invocation: the underlying command
    /// token plus its command-line arguments after expanding any user aliases
    /// (e.g. `up` → `pull --rebase`). Git expands aliases before it writes
    /// reflog messages, so downstream analyzers that reconstruct a command's
    /// reflog action from its args (notably the pull span matcher) must see the
    /// expanded flags rather than the literal alias token.
    ///
    /// Returns `None` when no alias expansion applies (the command is a builtin
    /// or unresolvable); callers then fall back to parsing the raw argv, which
    /// is byte-identical to the pre-alias behavior. The default implementation
    /// returns `None` so backends without config access keep current behavior.
    fn resolve_invocation(
        &self,
        _worktree: &Path,
        _argv: &[String],
    ) -> Result<Option<(String, Vec<String>)>, GitAiError> {
        Ok(None)
    }

    fn clone_target(&self, argv: &[String], cwd_hint: Option<&Path>) -> Option<PathBuf>;

    fn init_target(&self, argv: &[String], cwd_hint: Option<&Path>) -> Option<PathBuf>;
}

const ALIAS_CACHE_TTL_SECS: u64 = 60;

struct AliasCacheEntry {
    /// Resolved alias name → expansion value (e.g. "ci" → "commit -v")
    aliases: HashMap<String, String>,
    refreshed_at: Instant,
    /// Set to true while a background thread is refreshing this entry,
    /// preventing thundering-herd spawns when many events arrive after TTL.
    refresh_in_progress: bool,
}

pub struct SystemGitBackend {
    alias_cache: Arc<Mutex<HashMap<String, AliasCacheEntry>>>,
}

impl std::fmt::Debug for SystemGitBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemGitBackend").finish()
    }
}

impl Default for SystemGitBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemGitBackend {
    pub fn new() -> Self {
        Self {
            alias_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Look up a single alias from the per-family cache.
    ///
    /// Uses stale-while-revalidate: if the cache entry is expired, the stale
    /// value is returned immediately and a background thread refreshes it.
    /// This ensures alias resolution is never on the critical path.
    fn resolve_alias_cached(
        &self,
        worktree: &Path,
        alias_name: &str,
    ) -> Result<Option<String>, GitAiError> {
        let family_key = match common_dir_for_worktree(worktree) {
            Some(common_dir) => common_dir
                .canonicalize()
                .unwrap_or(common_dir)
                .to_string_lossy()
                .to_string(),
            None => return self.resolve_alias_uncached(worktree, alias_name),
        };

        let cache = self
            .alias_cache
            .lock()
            .map_err(|_| GitAiError::Generic("alias cache lock poisoned".to_string()))?;

        if let Some(entry) = cache.get(&family_key) {
            let normalized_alias = alias_name.to_ascii_lowercase();
            let result = entry.aliases.get(&normalized_alias).cloned();
            if entry.refreshed_at.elapsed().as_secs() >= ALIAS_CACHE_TTL_SECS
                && !entry.refresh_in_progress
            {
                // Stale — return cached value but kick off background refresh.
                // Mark in-progress to prevent thundering-herd thread spawns.
                drop(cache);
                if let Ok(mut cache) = self.alias_cache.lock()
                    && let Some(entry) = cache.get_mut(&family_key)
                {
                    entry.refresh_in_progress = true;
                }
                let worktree = worktree.to_path_buf();
                let family_key = family_key.clone();
                let alias_cache = self.alias_cache.clone();
                std::thread::spawn(move || {
                    refresh_alias_cache(&worktree, &family_key, &alias_cache);
                });
            }
            return Ok(result);
        }
        drop(cache);

        // Cold miss — must load synchronously for the first call.
        // If the sync refresh fails (e.g. repo discovery error), the cache won't
        // contain the family key. Fall back to uncached resolution which correctly
        // propagates errors.
        self.refresh_alias_cache_sync(worktree, &family_key)?;
        let cache = self
            .alias_cache
            .lock()
            .map_err(|_| GitAiError::Generic("alias cache lock poisoned".to_string()))?;
        match cache.get(&family_key) {
            Some(entry) => {
                let normalized_alias = alias_name.to_ascii_lowercase();
                Ok(entry.aliases.get(&normalized_alias).cloned())
            }
            None => {
                drop(cache);
                self.resolve_alias_uncached(worktree, alias_name)
            }
        }
    }

    /// Synchronously load all aliases for a family into the cache.
    fn refresh_alias_cache_sync(
        &self,
        worktree: &Path,
        family_key: &str,
    ) -> Result<(), GitAiError> {
        refresh_alias_cache(worktree, family_key, &self.alias_cache);
        Ok(())
    }

    /// Fallback when we can't determine a family key for caching.
    fn resolve_alias_uncached(
        &self,
        worktree: &Path,
        alias_name: &str,
    ) -> Result<Option<String>, GitAiError> {
        let repo = discover_repository_in_path_no_git_exec(worktree)?;
        let key = format!("alias.{}", alias_name);
        repo.config_get_str(&key)
    }

    /// Iteratively expand user aliases until reaching a builtin command (or an
    /// unresolvable token / `!`-shell alias / alias cycle), mirroring how git
    /// itself expands `git <alias> ...` before execution. Returns the resolved
    /// command token together with its expanded command-line args, or `None`
    /// when no command can be determined.
    fn expand_alias_invocation(
        &self,
        worktree: &Path,
        argv: &[String],
    ) -> Result<Option<(String, Vec<String>)>, GitAiError> {
        let mut current = parse_git_cli_args(git_invocation_tokens(argv));
        let mut seen = HashSet::new();
        loop {
            let Some(command) = current.command.clone() else {
                return Ok(None);
            };
            if !seen.insert(command.clone()) {
                return Ok(None);
            }
            if is_builtin_primary_command(&command) {
                return Ok(Some((command, current.command_args)));
            }

            let alias_value = match self.resolve_alias_cached(worktree, &command)? {
                Some(value) => value,
                None => return Ok(Some((command, current.command_args))),
            };

            let Some(alias_tokens) = parse_alias_tokens(&alias_value) else {
                return Ok(None);
            };

            let mut expanded_args = Vec::new();
            expanded_args.extend(current.global_args.iter().cloned());
            expanded_args.extend(alias_tokens);
            expanded_args.extend(current.command_args.iter().cloned());
            current = parse_git_cli_args(&expanded_args);
        }
    }
}

/// Load aliases from disk and store them in the cache. Safe to call from any
/// thread — errors are silently swallowed when running as a background refresh.
fn refresh_alias_cache(
    worktree: &Path,
    family_key: &str,
    alias_cache: &Mutex<HashMap<String, AliasCacheEntry>>,
) {
    let aliases = match discover_repository_in_path_no_git_exec(worktree).and_then(|repo| {
        repo.get_git_config_file()
            .map(|cfg| read_all_aliases_from_config(&cfg))
    }) {
        Ok(aliases) => aliases,
        Err(_) => {
            // Clear refresh_in_progress so a future attempt can retry.
            if let Ok(mut cache) = alias_cache.lock()
                && let Some(entry) = cache.get_mut(family_key)
            {
                entry.refresh_in_progress = false;
            }
            return;
        }
    };
    if let Ok(mut cache) = alias_cache.lock() {
        cache.insert(
            family_key.to_string(),
            AliasCacheEntry {
                aliases,
                refreshed_at: Instant::now(),
                refresh_in_progress: false,
            },
        );
    }
}

fn read_all_aliases_from_config(config: &gix_config::File<'_>) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    let Some(sections) = config.sections_by_name("alias") else {
        return aliases;
    };
    for section in sections {
        let body = section.body();
        for key in body.value_names() {
            let key_str = key.to_string();
            if key_str.is_empty() {
                continue;
            }
            if let Some(value) = body.value(&key_str) {
                aliases.insert(key_str.to_ascii_lowercase(), value.to_string());
            }
        }
    }
    aliases
}

fn is_builtin_primary_command(command: &str) -> bool {
    matches!(
        command,
        "add"
            | "blame"
            | "branch"
            | "cat-file"
            | "check-attr"
            | "check-ignore"
            | "check-mailmap"
            | "checkout"
            | "cherry-pick"
            | "clean"
            | "clone"
            | "commit"
            | "config"
            | "count-objects"
            | "describe"
            | "diff"
            | "diff-files"
            | "diff-index"
            | "diff-tree"
            | "fetch"
            | "for-each-ref"
            | "grep"
            | "gc"
            | "hash-object"
            | "help"
            | "init"
            | "log"
            | "ls-files"
            | "ls-tree"
            | "merge"
            | "merge-base"
            | "maintenance"
            | "mktree"
            | "mv"
            | "name-rev"
            | "notes"
            | "pull"
            | "push"
            | "pack-refs"
            | "rebase"
            | "remote"
            | "reflog"
            | "repack"
            | "reset"
            | "restore"
            | "rev-list"
            | "rev-parse"
            | "revert"
            | "rm"
            | "shortlog"
            | "show"
            | "stash"
            | "status"
            | "submodule"
            | "switch"
            | "symbolic-ref"
            | "tag"
            | "update-ref"
            | "var"
            | "verify-commit"
            | "verify-tag"
            | "version"
            | "worktree"
    )
}

impl GitBackend for SystemGitBackend {
    fn resolve_family(&self, worktree: &Path) -> Result<FamilyKey, GitAiError> {
        let common = common_dir_for_repo_path(worktree).ok_or_else(|| {
            GitAiError::Generic(format!(
                "Failed to resolve git common dir for repo path {}",
                worktree.display()
            ))
        })?;
        let common = common.canonicalize().unwrap_or(common);
        Ok(FamilyKey::new(common.to_string_lossy().to_string()))
    }

    fn resolve_primary_command(
        &self,
        worktree: &Path,
        argv: &[String],
    ) -> Result<Option<String>, GitAiError> {
        Ok(self
            .expand_alias_invocation(worktree, argv)?
            .map(|(command, _args)| command))
    }

    fn resolve_invocation(
        &self,
        worktree: &Path,
        argv: &[String],
    ) -> Result<Option<(String, Vec<String>)>, GitAiError> {
        self.expand_alias_invocation(worktree, argv)
    }

    fn clone_target(&self, argv: &[String], cwd_hint: Option<&Path>) -> Option<PathBuf> {
        let args = command_args(argv, "clone");
        let positional = clone_init_positionals(&args);
        if positional.is_empty() {
            return None;
        }
        let target = if positional.len() >= 2 {
            PathBuf::from(&positional[1])
        } else {
            default_clone_target_from_source(&positional[0])?
        };
        resolve_target(target, cwd_hint)
    }

    fn init_target(&self, argv: &[String], cwd_hint: Option<&Path>) -> Option<PathBuf> {
        let args = command_args(argv, "init");
        let positional = clone_init_positionals(&args);
        let target = positional
            .first()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        resolve_target(target, cwd_hint)
    }
}

fn is_git_binary(token: &str) -> bool {
    if token == "git" || token == "git.exe" {
        return true;
    }
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name == "git" || name == "git.exe")
        .unwrap_or(false)
}

fn git_invocation_tokens(argv: &[String]) -> &[String] {
    if argv
        .first()
        .map(|token| is_git_binary(token))
        .unwrap_or(false)
    {
        &argv[1..]
    } else {
        argv
    }
}

fn command_args(argv: &[String], command: &str) -> Vec<String> {
    let slice = git_invocation_tokens(argv);
    let mut seen = false;
    let mut out = Vec::new();
    for token in slice {
        if !seen {
            if token == command {
                seen = true;
            }
            continue;
        }
        out.push(token.clone());
    }
    out
}

fn clone_init_positionals(args: &[String]) -> Vec<String> {
    let mut positionals = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            positionals.extend(args[idx + 1..].iter().cloned());
            break;
        }
        if arg.starts_with('-') {
            if takes_value(arg) && idx + 1 < args.len() {
                idx += 2;
                continue;
            }
            idx += 1;
            continue;
        }
        positionals.push(arg.clone());
        idx += 1;
    }
    positionals
}

fn takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-b" | "--branch"
            | "--origin"
            | "--upload-pack"
            | "--template"
            | "--separate-git-dir"
            | "--reference"
            | "--reference-if-able"
            | "-c"
            | "--config"
            | "--object-format"
            | "--depth"
            | "--shallow-since"
            | "--shallow-exclude"
            | "-j"
            | "--jobs"
            | "--filter"
            | "--bundle-uri"
            | "--server-option"
    )
}

fn default_clone_target_from_source(source: &str) -> Option<PathBuf> {
    let source = source.trim_end_matches(&['/', '\\'] as &[char]);
    let source = source.strip_suffix(".git").unwrap_or(source);
    // Split on both / and \ to handle Windows paths
    let after_last_sep = source.rsplit(&['/', '\\'] as &[char]).next()?;
    // Handle SCP-like syntax (user@host:path), but skip Windows drive letters (C:)
    let name = if after_last_sep.contains(':') && after_last_sep.len() > 2 {
        after_last_sep.rsplit(':').next()?
    } else {
        after_last_sep
    };
    if name.is_empty() {
        return None;
    }
    Some(PathBuf::from(name))
}

fn resolve_target(target: PathBuf, cwd_hint: Option<&Path>) -> Option<PathBuf> {
    if target.is_absolute() {
        return Some(target);
    }
    if let Some(cwd) = cwd_hint {
        return Some(cwd.join(target));
    }
    // Relative target with no cwd_hint: the path cannot be reliably resolved
    // (filesystem checks would run against the daemon's own CWD rather than the
    // git process's working directory).  Return None so the caller skips this
    // candidate rather than logging a misleading error.
    None
}

fn parse_alias_tokens(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim_start();
    if trimmed.starts_with('!') {
        return None;
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in trimmed.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            continue;
        }

        if in_double {
            match ch {
                '"' => in_double = false,
                '\\' => escaped = true,
                _ => current.push(ch),
            }
            continue;
        }

        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => escaped = true,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    if escaped {
        current.push('\\');
    }
    if in_single || in_double {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::{
        GitBackend, SystemGitBackend, clone_init_positionals, default_clone_target_from_source,
    };
    use std::fs;
    use std::path::PathBuf;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    // --- Bug: `takes_value` is incomplete — options like --depth, -j, -c are not listed.
    // When `git clone --depth 1 <url>` is parsed, "1" is treated as a positional arg and
    // the URL ends up as positional[1] (the "target directory"), triggering the error:
    //   "failed to resolve clone/init target family from filesystem: <url>"
    //
    // These tests pin the CORRECT behaviour (URL-derived name as the only positional)
    // and will FAIL until `takes_value` includes those options.

    #[test]
    fn clone_positionals_skips_value_for_depth_flag() {
        let args = argv(&["--depth", "1", "https://example.com/org/test-repo.git"]);
        assert_eq!(
            clone_init_positionals(&args),
            vec!["https://example.com/org/test-repo.git".to_string()],
            "--depth should consume its value, leaving only the URL as a positional"
        );
    }

    #[test]
    fn clone_positionals_skips_value_for_jobs_short_flag() {
        let args = argv(&["-j", "4", "https://example.com/org/test-repo.git"]);
        assert_eq!(
            clone_init_positionals(&args),
            vec!["https://example.com/org/test-repo.git".to_string()],
            "-j should consume its value, leaving only the URL as a positional"
        );
    }

    #[test]
    fn clone_positionals_skips_value_for_jobs_long_flag() {
        let args = argv(&["--jobs", "4", "https://example.com/org/test-repo.git"]);
        assert_eq!(
            clone_init_positionals(&args),
            vec!["https://example.com/org/test-repo.git".to_string()],
            "--jobs should consume its value, leaving only the URL as a positional"
        );
    }

    #[test]
    fn clone_positionals_skips_value_for_config_short_flag() {
        let args = argv(&[
            "-c",
            "http.sslVerify=false",
            "https://example.com/org/test-repo.git",
        ]);
        assert_eq!(
            clone_init_positionals(&args),
            vec!["https://example.com/org/test-repo.git".to_string()],
            "-c should consume its value, leaving only the URL as a positional"
        );
    }

    #[test]
    fn clone_target_derives_name_from_url_with_depth_flag() {
        let backend = SystemGitBackend::new();
        let cwd = PathBuf::from("/home/testuser/projects");
        let args = argv(&[
            "git",
            "clone",
            "--depth",
            "1",
            "https://example.com/org/test-repo.git",
        ]);
        let result = backend.clone_target(&args, Some(&cwd)).unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/testuser/projects/test-repo"),
            "clone target should be derived from the URL, not the depth value"
        );
    }

    // --- Bug: when `cwd_hint` is None and the derived target is relative (e.g. "." for
    // `git init` with no args), the path cannot be resolved and filesystem checks run
    // against the daemon's own CWD rather than the actual target, producing the error:
    //   "failed to resolve clone/init target family from filesystem: ."
    //
    // These tests pin the CORRECT behaviour (return None so that the caller doesn't
    // attempt a meaningless filesystem lookup against an unresolvable relative path).

    #[test]
    fn init_target_returns_none_for_implicit_dot_without_cwd_hint() {
        let backend = SystemGitBackend::new();
        let args = argv(&["git", "init"]);
        assert!(
            backend.init_target(&args, None).is_none(),
            "init with no path and no cwd_hint should return None — \
             relative '.' cannot be reliably resolved"
        );
    }

    #[test]
    fn clone_target_returns_none_for_explicit_dot_without_cwd_hint() {
        let backend = SystemGitBackend::new();
        let args = argv(&["git", "clone", "https://example.com/org/test-repo.git", "."]);
        assert!(
            backend.clone_target(&args, None).is_none(),
            "clone into '.' with no cwd_hint should return None — \
             relative '.' cannot be reliably resolved"
        );
    }

    #[test]
    fn init_target_resolves_dot_when_cwd_hint_is_provided() {
        let backend = SystemGitBackend::new();
        // Use temp_dir() so the base path is absolute on all platforms (Windows
        // does not consider Unix-style paths like "/home/..." absolute).
        let cwd = std::env::temp_dir().join("git-ai-test-my-repo");
        assert!(
            cwd.is_absolute(),
            "temp_dir should be absolute on all platforms"
        );
        let args = argv(&["git", "init"]);
        let result = backend.init_target(&args, Some(&cwd)).unwrap();
        assert!(
            result.is_absolute(),
            "result must be absolute when cwd_hint is provided"
        );
        assert!(
            result.starts_with(&cwd),
            "result should be rooted at the cwd"
        );
    }

    // --- Bug (pre-existing): `--dissociate` is a boolean flag but was listed in
    // `takes_value`, causing the next argument (typically the URL) to be swallowed
    // as its "value".  `git clone --reference /mirror --dissociate <url>` would leave
    // the positionals list empty and `clone_target()` would return None.

    #[test]
    fn clone_positionals_treats_dissociate_as_boolean_not_value_taking() {
        let args = argv(&[
            "--reference",
            "/mirror",
            "--dissociate",
            "https://example.com/org/test-repo.git",
        ]);
        assert_eq!(
            clone_init_positionals(&args),
            vec!["https://example.com/org/test-repo.git".to_string()],
            "--dissociate is boolean and must not consume the following URL"
        );
    }

    #[test]
    fn builtin_primary_command_skips_repository_lookup() {
        let backend = SystemGitBackend::new();
        let missing_worktree = PathBuf::from("/definitely/missing/git-ai-backend-test");
        let argv = vec!["git".to_string(), "commit".to_string()];

        let resolved = backend
            .resolve_primary_command(&missing_worktree, &argv)
            .expect("builtin commands should not require repository discovery");

        assert_eq!(resolved.as_deref(), Some("commit"));
    }

    #[test]
    fn submodule_is_a_builtin_primary_command() {
        let backend = SystemGitBackend::new();
        let missing_worktree = PathBuf::from("/definitely/missing/git-ai-backend-test");
        let argv = vec![
            "git".to_string(),
            "submodule".to_string(),
            "status".to_string(),
        ];

        let resolved = backend
            .resolve_primary_command(&missing_worktree, &argv)
            .expect("builtin commands should not require repository discovery");

        assert_eq!(resolved.as_deref(), Some("submodule"));
    }

    #[test]
    fn repository_admin_commands_are_builtin() {
        let backend = SystemGitBackend::new();
        let missing_worktree = PathBuf::from("/definitely/missing/git-ai-backend-test");
        for command in ["gc", "maintenance", "pack-refs", "reflog", "repack"] {
            let argv = vec!["git".to_string(), command.to_string()];
            let resolved = backend
                .resolve_primary_command(&missing_worktree, &argv)
                .expect("builtin commands should not require repository discovery");
            assert_eq!(resolved.as_deref(), Some(command));
        }
    }

    #[test]
    fn resolve_family_uses_worktree_filesystem_without_git_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git_dir = temp.path().join(".git");
        fs::create_dir_all(&git_dir).expect("create git dir");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

        let family = SystemGitBackend::new()
            .resolve_family(temp.path())
            .expect("resolve family");

        assert_eq!(
            family.0,
            git_dir
                .canonicalize()
                .expect("canonical git dir")
                .to_string_lossy()
        );
    }

    #[test]
    fn resolve_family_accepts_bare_repo_path_without_git_spawn() {
        let bare = tempfile::tempdir().expect("bare tempdir");
        fs::write(bare.path().join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

        let family = SystemGitBackend::new()
            .resolve_family(bare.path())
            .expect("resolve family");

        assert_eq!(
            family.0,
            bare.path()
                .canonicalize()
                .expect("canonical bare dir")
                .to_string_lossy()
        );
    }

    #[test]
    fn default_clone_target_from_url() {
        assert_eq!(
            default_clone_target_from_source("https://github.com/user/repo.git"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            default_clone_target_from_source("git@github.com:user/repo.git"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            default_clone_target_from_source("/local/path/repo"),
            Some(PathBuf::from("repo"))
        );
    }

    #[test]
    fn default_clone_target_from_windows_path() {
        assert_eq!(
            default_clone_target_from_source(r"C:\Users\runner\Temp\repo"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            default_clone_target_from_source(r"C:\Users\runner\Temp\repo.git"),
            Some(PathBuf::from("repo"))
        );
        assert_eq!(
            default_clone_target_from_source(r"\\?\C:\Temp\bare-repo"),
            Some(PathBuf::from("bare-repo"))
        );
    }

    #[test]
    fn unknown_primary_command_still_requires_repository_lookup() {
        let backend = SystemGitBackend::new();
        let missing_worktree = PathBuf::from("/definitely/missing/git-ai-backend-test");
        let argv = vec!["git".to_string(), "ci".to_string()];

        assert!(
            backend
                .resolve_primary_command(&missing_worktree, &argv)
                .is_err(),
            "unknown commands should still consult repository alias config"
        );
    }
}
