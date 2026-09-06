use crate::git::repository::Repository;
use glob::Pattern;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "*.lock",
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "Gemfile.lock",
    "poetry.lock",
    "composer.lock",
    "Pipfile.lock",
    "shrinkwrap.yaml",
    "*.generated.*",
    "*.min.js",
    "*.min.css",
    "*.map",
    "**/vendor/**",
    "**/node_modules/**",
    "**/__snapshots__/**",
    "**/*.snap",
    "**/*.snap.new",
    "**/drizzle/meta/**",
    // Protobuf generated code
    "*.pbobjc.h",
    "*.pbobjc.m",
    "*.pb.go",
    "*.pb.h",
    "*.pb.cc",
    "*_pb2.py",
    "*_pb2_grpc.py",
    "*.pb.swift",
    "*.pb.dart",
];

#[derive(Clone, Debug)]
enum CompiledPattern {
    Glob { pattern: Pattern, negated: bool },
    Exact { value: String, negated: bool },
}

impl CompiledPattern {
    fn negated(&self) -> bool {
        match self {
            CompiledPattern::Glob { negated, .. } | CompiledPattern::Exact { negated, .. } => {
                *negated
            }
        }
    }

    fn matches(&self, path: &str, filename: &str) -> bool {
        match self {
            CompiledPattern::Glob { pattern, .. } => {
                pattern.matches(path) || pattern.matches(filename)
            }
            CompiledPattern::Exact { value, .. } => value == path || value == filename,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct IgnoreMatcher {
    patterns: Vec<CompiledPattern>,
}

impl IgnoreMatcher {
    pub fn new(patterns: &[String]) -> Self {
        let patterns = patterns
            .iter()
            .map(|raw| {
                let (negated, body) = split_negation(raw);
                match Pattern::new(&body) {
                    Ok(glob) => CompiledPattern::Glob {
                        pattern: glob,
                        negated,
                    },
                    Err(_) => CompiledPattern::Exact {
                        value: body,
                        negated,
                    },
                }
            })
            .collect();

        Self { patterns }
    }

    /// Returns whether `path` should be ignored.
    ///
    /// Patterns are evaluated in order and the **last** matching pattern wins,
    /// mirroring `.gitignore` semantics. A pattern prefixed with `!` re-includes
    /// (un-ignores) paths matched by an earlier pattern, which is how a user can
    /// override a built-in default such as `**/vendor/**`.
    pub fn is_ignored(&self, path: &str) -> bool {
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.matches(path, filename) {
                ignored = !pattern.negated();
            }
        }
        ignored
    }
}

/// Split a leading `!` negation marker from a pattern.
///
/// Returns `(negated, body)` where `body` is the pattern with the marker
/// removed. A literal leading `!` can still be matched by escaping it as `\!`,
/// mirroring `.gitignore` syntax.
fn split_negation(raw: &str) -> (bool, String) {
    if let Some(rest) = raw.strip_prefix('!') {
        (true, rest.to_string())
    } else if let Some(rest) = raw.strip_prefix("\\!") {
        (false, format!("!{rest}"))
    } else {
        (false, raw.to_string())
    }
}

pub fn default_ignore_patterns() -> Vec<String> {
    DEFAULT_IGNORE_PATTERNS
        .iter()
        .map(|pattern| pattern.to_string())
        .collect()
}

pub fn build_ignore_matcher(patterns: &[String]) -> IgnoreMatcher {
    IgnoreMatcher::new(patterns)
}

pub fn should_ignore_file_with_matcher(path: &str, matcher: &IgnoreMatcher) -> bool {
    matcher.is_ignored(path)
}

/// Check if a file path should be ignored based on the provided patterns.
/// Supports both exact matches and glob patterns (e.g., "*.lock", "**/*.generated.js").
#[allow(dead_code)] // Kept for API compatibility; prefer should_ignore_file_with_matcher in hot paths.
pub fn should_ignore_file(path: &str, patterns: &[String]) -> bool {
    should_ignore_file_with_matcher(path, &build_ignore_matcher(patterns))
}

pub fn load_linguist_generated_patterns_from_root_gitattributes(repo: &Repository) -> Vec<String> {
    let Some(contents) = load_root_gitattributes_contents(repo) else {
        return Vec::new();
    };
    parse_linguist_generated_patterns(&contents)
}

pub fn load_linguist_vendored_patterns_from_root_gitattributes(repo: &Repository) -> Vec<String> {
    let Some(contents) = load_root_gitattributes_contents(repo) else {
        return Vec::new();
    };
    parse_linguist_vendored_patterns(&contents)
}

fn parse_linguist_generated_patterns(contents: &str) -> Vec<String> {
    let mut patterns = Vec::new();

    for raw_line in contents.lines() {
        let Some((path_pattern, attrs)) = parse_gitattributes_line(raw_line) else {
            continue;
        };

        if attribute_state(&attrs, "linguist-generated") == Some(true) {
            patterns.push(path_pattern);
        }
    }

    dedupe_patterns(patterns)
}

/// Parse `linguist-vendored` declarations from `.gitattributes` into ignore
/// patterns.
///
/// `linguist-vendored` / `linguist-vendored=true` marks a path as vendored and
/// yields a positive ignore pattern. `-linguist-vendored` / `!linguist-vendored`
/// / `linguist-vendored=false` un-marks it and yields a **negation** pattern
/// (`!path`), so a user can re-include first-party code that the built-in
/// `**/vendor/**` default would otherwise exclude (see issue #1664).
fn parse_linguist_vendored_patterns(contents: &str) -> Vec<String> {
    let mut patterns = Vec::new();

    for raw_line in contents.lines() {
        let Some((path_pattern, attrs)) = parse_gitattributes_line(raw_line) else {
            continue;
        };

        match attribute_state(&attrs, "linguist-vendored") {
            Some(true) => patterns.push(path_pattern),
            Some(false) => patterns.push(format!("!{path_pattern}")),
            None => {}
        }
    }

    dedupe_patterns(patterns)
}

/// Tokenize a single `.gitattributes` line into `(path_pattern, attributes)`.
///
/// Returns `None` for blank lines, comments, macro definitions (`[attr]…`), and
/// lines without at least one attribute.
fn parse_gitattributes_line(raw_line: &str) -> Option<(String, Vec<String>)> {
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let mut tokens = split_gitattributes_tokens(line);
    if tokens.len() < 2 {
        return None;
    }

    let path_pattern = tokens.remove(0);
    if path_pattern.starts_with("[attr]") {
        return None;
    }

    Some((path_pattern, tokens))
}

/// Resolve the tri-state value of a boolean git attribute over a token list.
///
/// Recognizes `attr`, `-attr` / `!attr`, and `attr=true|false|1|0`. The last
/// occurrence wins. Returns `None` when the attribute is absent.
fn attribute_state(attrs: &[String], name: &str) -> Option<bool> {
    let unset = format!("-{name}");
    let unset_bang = format!("!{name}");
    let value_prefix = format!("{name}=");

    let mut state = None;
    for attr in attrs {
        if attr == name {
            state = Some(true);
        } else if attr == &unset || attr == &unset_bang {
            state = Some(false);
        } else if let Some(value) = attr.strip_prefix(&value_prefix) {
            if value.eq_ignore_ascii_case("true") || value == "1" {
                state = Some(true);
            } else if value.eq_ignore_ascii_case("false") || value == "0" {
                state = Some(false);
            }
        }
    }

    state
}

fn load_root_gitattributes_contents(repo: &Repository) -> Option<String> {
    if repo.is_bare_repository().unwrap_or(false) {
        return repo
            .get_file_content(".gitattributes", "HEAD")
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }

    let workdir = repo.workdir().ok()?;
    let gitattributes_path = workdir.join(".gitattributes");
    fs::read_to_string(gitattributes_path).ok()
}

/// Load ignore patterns from a `.git-ai-ignore` file at the repository root.
/// The file follows `.gitignore` syntax: one glob pattern per line, blank lines
/// and lines starting with `#` are skipped. A line prefixed with `!` negates an
/// earlier pattern, e.g. `!src/main/java/com/acme/vendor/**` re-includes a
/// first-party package that the built-in `**/vendor/**` default would exclude.
pub fn load_git_ai_ignore_patterns(repo: &Repository) -> Vec<String> {
    let Some(contents) = load_root_git_ai_ignore_contents(repo) else {
        return Vec::new();
    };

    let mut patterns = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        patterns.push(line.to_string());
    }

    dedupe_patterns(patterns)
}

fn load_root_git_ai_ignore_contents(repo: &Repository) -> Option<String> {
    if repo.is_bare_repository().unwrap_or(false) {
        return repo
            .get_file_content(".git-ai-ignore", "HEAD")
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok());
    }

    let workdir = repo.workdir().ok()?;
    let ignore_path = workdir.join(".git-ai-ignore");
    fs::read_to_string(ignore_path).ok()
}

/// Load `.git-ai-ignore` patterns from a repo root path directly (no Repository object needed).
/// Use this when you have a `&Path` but not a `Repository` (e.g. in snapshot capture code).
pub fn load_git_ai_ignore_patterns_from_path(repo_root: &Path) -> Vec<String> {
    let contents = match fs::read_to_string(repo_root.join(".git-ai-ignore")) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut patterns = Vec::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        patterns.push(line.to_string());
    }
    dedupe_patterns(patterns)
}

/// Load linguist-generated patterns from `.gitattributes` at a repo root path directly.
/// Use this when you have a `&Path` but not a `Repository` (e.g. in snapshot capture code).
/// Uses the same parser as `load_linguist_generated_patterns_from_root_gitattributes`.
pub fn load_linguist_generated_patterns_from_path(repo_root: &Path) -> Vec<String> {
    match fs::read_to_string(repo_root.join(".gitattributes")) {
        Ok(contents) => parse_linguist_generated_patterns(&contents),
        Err(_) => Vec::new(),
    }
}

/// Load linguist-vendored patterns from `.gitattributes` at a repo root path directly.
/// Use this when you have a `&Path` but not a `Repository` (e.g. in snapshot capture code).
/// Uses the same parser as `load_linguist_vendored_patterns_from_root_gitattributes`.
pub fn load_linguist_vendored_patterns_from_path(repo_root: &Path) -> Vec<String> {
    match fs::read_to_string(repo_root.join(".gitattributes")) {
        Ok(contents) => parse_linguist_vendored_patterns(&contents),
        Err(_) => Vec::new(),
    }
}

pub fn effective_ignore_patterns(
    repo: &Repository,
    user_patterns: &[String],
    extra_patterns: &[String],
) -> Vec<String> {
    // Order matters: built-in defaults come first so later sources (linguist
    // attributes, `.git-ai-ignore`, user flags) can re-include paths via `!`
    // negation under the last-match-wins semantics in `IgnoreMatcher`.
    let mut patterns = default_ignore_patterns();
    patterns.extend(load_linguist_generated_patterns_from_root_gitattributes(
        repo,
    ));
    patterns.extend(load_linguist_vendored_patterns_from_root_gitattributes(
        repo,
    ));
    patterns.extend(load_git_ai_ignore_patterns(repo));
    patterns.extend(extra_patterns.iter().cloned());
    patterns.extend(user_patterns.iter().cloned());
    dedupe_patterns(patterns)
}

/// Remove duplicate patterns, keeping the **last** occurrence of each.
///
/// Under the last-match-wins semantics in `IgnoreMatcher`, the final occurrence
/// of a pattern is the one that decides a path's fate. Keeping the last (rather
/// than the first) occurrence preserves that position, so a pattern re-asserted
/// after an intervening negation isn't collapsed onto an earlier duplicate.
fn dedupe_patterns(patterns: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped: Vec<String> = patterns
        .into_iter()
        .rev()
        .filter(|pattern| seen.insert(pattern.clone()))
        .collect();
    deduped.reverse();
    deduped
}

fn split_gitattributes_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escaped {
        current.push('\\');
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_snapshot_and_lock_patterns() {
        let defaults = default_ignore_patterns();
        assert!(defaults.contains(&"**/*.snap".to_string()));
        assert!(defaults.contains(&"Cargo.lock".to_string()));
        assert!(defaults.contains(&"*.generated.*".to_string()));
    }

    #[test]
    fn defaults_ignore_drizzle_meta_files() {
        let defaults = default_ignore_patterns();
        let matcher = build_ignore_matcher(&defaults);

        assert!(should_ignore_file_with_matcher(
            "web/drizzle/meta/_journal.json",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "web/drizzle/meta/0001_snapshot.json",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "drizzle/meta/0032_snapshot.json",
            &matcher
        ));
        // Should not ignore non-meta drizzle files
        assert!(!should_ignore_file_with_matcher(
            "drizzle/0001_initial.sql",
            &matcher
        ));
    }

    #[test]
    fn defaults_do_not_ignore_generic_snapshots_directories() {
        let defaults = default_ignore_patterns();
        let matcher = build_ignore_matcher(&defaults);

        assert!(!should_ignore_file_with_matcher(
            "backups/snapshots/state.json",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "tests/__snapshots__/feature.snap",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "tests/snapshots/feature.snap",
            &matcher
        ));
    }

    #[test]
    fn defaults_ignore_nested_named_lockfiles() {
        let defaults = default_ignore_patterns();
        let matcher = build_ignore_matcher(&defaults);

        assert!(should_ignore_file_with_matcher(
            "apps/web/Gemfile.lock",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "services/api/package-lock.json",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "libs/core/Cargo.lock",
            &matcher
        ));
    }

    #[test]
    fn should_ignore_file_matches_path_and_filename() {
        let patterns = vec!["*.lock".to_string(), "**/node_modules/**".to_string()];
        let matcher = build_ignore_matcher(&patterns);
        assert!(should_ignore_file("Cargo.lock", &patterns));
        assert!(should_ignore_file("backend/Cargo.lock", &patterns));
        assert!(should_ignore_file_with_matcher("Cargo.lock", &matcher));
        assert!(should_ignore_file_with_matcher(
            "backend/Cargo.lock",
            &matcher
        ));
        assert!(should_ignore_file(
            "web/node_modules/lodash/index.js",
            &patterns
        ));
        assert!(should_ignore_file_with_matcher(
            "web/node_modules/lodash/index.js",
            &matcher
        ));
        assert!(!should_ignore_file("src/main.rs", &patterns));
        assert!(!should_ignore_file_with_matcher("src/main.rs", &matcher));
    }

    #[test]
    fn invalid_patterns_fallback_to_exact_path_or_filename() {
        let patterns = vec!["[".to_string(), "docs/[bad".to_string()];
        let matcher = build_ignore_matcher(&patterns);

        assert!(should_ignore_file_with_matcher("[", &matcher));
        assert!(should_ignore_file_with_matcher("docs/[bad", &matcher));
        assert!(!should_ignore_file_with_matcher("docs/good.rs", &matcher));
    }

    #[test]
    fn defaults_include_protobuf_generated_patterns() {
        let defaults = default_ignore_patterns();
        // Objective-C protobuf
        assert!(defaults.contains(&"*.pbobjc.h".to_string()));
        assert!(defaults.contains(&"*.pbobjc.m".to_string()));
        // Go protobuf
        assert!(defaults.contains(&"*.pb.go".to_string()));
        // C++ protobuf
        assert!(defaults.contains(&"*.pb.h".to_string()));
        assert!(defaults.contains(&"*.pb.cc".to_string()));
        // Python protobuf
        assert!(defaults.contains(&"*_pb2.py".to_string()));
        assert!(defaults.contains(&"*_pb2_grpc.py".to_string()));
        // Swift protobuf
        assert!(defaults.contains(&"*.pb.swift".to_string()));
        // Dart protobuf
        assert!(defaults.contains(&"*.pb.dart".to_string()));
    }

    #[test]
    fn defaults_ignore_protobuf_generated_files() {
        let defaults = default_ignore_patterns();
        let matcher = build_ignore_matcher(&defaults);

        // Bare filenames
        assert!(should_ignore_file_with_matcher(
            "Message.pbobjc.h",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "Message.pbobjc.m",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher("service.pb.go", &matcher));
        assert!(should_ignore_file_with_matcher("message.pb.h", &matcher));
        assert!(should_ignore_file_with_matcher("message.pb.cc", &matcher));
        assert!(should_ignore_file_with_matcher("types_pb2.py", &matcher));
        assert!(should_ignore_file_with_matcher(
            "service_pb2_grpc.py",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "message.pb.swift",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher("message.pb.dart", &matcher));

        // Nested paths
        assert!(should_ignore_file_with_matcher(
            "proto/gen/service.pb.go",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "ios/Proto/Message.pbobjc.h",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "backend/api/types_pb2.py",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "cpp/protos/message.pb.cc",
            &matcher
        ));

        // Non-protobuf files should NOT be matched
        assert!(!should_ignore_file_with_matcher("main.go", &matcher));
        assert!(!should_ignore_file_with_matcher("service.py", &matcher));
        assert!(!should_ignore_file_with_matcher("header.h", &matcher));
        assert!(!should_ignore_file_with_matcher("source.cc", &matcher));
        assert!(!should_ignore_file_with_matcher("app.swift", &matcher));
        assert!(!should_ignore_file_with_matcher("widget.dart", &matcher));
        assert!(!should_ignore_file_with_matcher("Objective.m", &matcher));
    }

    #[test]
    fn negation_reincludes_first_party_vendor_package() {
        // Reproduces issue #1664: a first-party package named `vendor` is excluded
        // by the built-in `**/vendor/**` default, and a `!` negation re-includes it.
        let patterns = vec![
            "**/vendor/**".to_string(),
            "!src/main/java/com/acme/vendor/**".to_string(),
        ];
        let matcher = build_ignore_matcher(&patterns);

        // First-party package re-included by the negation.
        assert!(!should_ignore_file_with_matcher(
            "src/main/java/com/acme/vendor/Probe.java",
            &matcher
        ));
        // A genuine third-party vendor directory elsewhere stays ignored.
        assert!(should_ignore_file_with_matcher(
            "third_party/vendor/lib.js",
            &matcher
        ));
    }

    #[test]
    fn negation_is_last_match_wins() {
        // A positive pattern after a negation re-excludes the path.
        let patterns = vec!["!**/vendor/**".to_string(), "**/vendor/**".to_string()];
        let matcher = build_ignore_matcher(&patterns);
        assert!(should_ignore_file_with_matcher(
            "app/vendor/lib.js",
            &matcher
        ));
    }

    #[test]
    fn escaped_bang_matches_literal_filename() {
        // `\!` escapes the negation marker so a literal leading `!` can be matched.
        let patterns = vec!["\\!important.txt".to_string()];
        let matcher = build_ignore_matcher(&patterns);
        assert!(should_ignore_file_with_matcher("!important.txt", &matcher));
        assert!(!should_ignore_file_with_matcher("important.txt", &matcher));
    }

    #[test]
    fn positive_only_patterns_unaffected_by_negation_support() {
        // Regression guard: with no negation, behavior is still "ignored if any match".
        let patterns = vec!["*.lock".to_string(), "**/node_modules/**".to_string()];
        let matcher = build_ignore_matcher(&patterns);
        assert!(should_ignore_file_with_matcher(
            "backend/Cargo.lock",
            &matcher
        ));
        assert!(should_ignore_file_with_matcher(
            "web/node_modules/lodash/index.js",
            &matcher
        ));
        assert!(!should_ignore_file_with_matcher("src/main.rs", &matcher));
    }

    #[test]
    fn parses_linguist_vendored_true_as_positive_pattern() {
        let contents = "\
tools/bundled/** linguist-vendored
libs/external/** linguist-vendored=true
flags/** linguist-vendored=1
";
        let patterns = parse_linguist_vendored_patterns(contents);
        assert!(patterns.contains(&"tools/bundled/**".to_string()));
        assert!(patterns.contains(&"libs/external/**".to_string()));
        assert!(patterns.contains(&"flags/**".to_string()));
    }

    #[test]
    fn parses_linguist_vendored_false_as_negation_pattern() {
        let contents = "\
src/main/java/com/acme/vendor/** -linguist-vendored
other/** linguist-vendored=false
zero/** linguist-vendored=0
bang/** !linguist-vendored
";
        let patterns = parse_linguist_vendored_patterns(contents);
        assert!(patterns.contains(&"!src/main/java/com/acme/vendor/**".to_string()));
        assert!(patterns.contains(&"!other/**".to_string()));
        assert!(patterns.contains(&"!zero/**".to_string()));
        assert!(patterns.contains(&"!bang/**".to_string()));
    }

    #[test]
    fn linguist_vendored_macro_definitions_are_ignored() {
        let contents = "\
[attr]vendored linguist-vendored=true
generated/** linguist-vendored=true
";
        let patterns = parse_linguist_vendored_patterns(contents);
        assert!(patterns.contains(&"generated/**".to_string()));
        assert!(!patterns.iter().any(|p| p.contains("[attr]")));
    }

    #[test]
    fn dedupe_keeps_last_occurrence() {
        let deduped = dedupe_patterns(vec![
            "a/**".to_string(),
            "b/**".to_string(),
            "a/**".to_string(),
            "c/**".to_string(),
        ]);
        // Each pattern appears once, positioned at its last occurrence.
        assert_eq!(deduped, vec!["b/**", "a/**", "c/**"]);
    }

    #[test]
    fn reasserted_positive_after_negation_wins() {
        // With last-match-wins dedupe keeping the last occurrence, a pattern
        // re-asserted after a negation correctly re-excludes the path.
        let patterns = dedupe_patterns(vec![
            "**/vendor/**".to_string(),
            "!**/vendor/**".to_string(),
            "**/vendor/**".to_string(),
        ]);
        let matcher = build_ignore_matcher(&patterns);
        assert!(should_ignore_file_with_matcher(
            "app/vendor/lib.js",
            &matcher
        ));
    }
}
