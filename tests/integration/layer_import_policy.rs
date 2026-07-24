//! Layer import-direction policy for `src/**/*.rs`.
//!
//! Enforces the intended dependency direction from
//! `docs/decisions/2026-07-20-layered-architecture-plan.md`: the pure domain
//! (`model`, excluding the persistence adapter `model/repository`) and the
//! network adapters (`clients`) must not depend on layers above them. This is
//! GPT's "dependency enforcement" as a ~100-line policy test, not a framework.
//!
//! The test scans `use crate::…` (and a few infra crates) lines per layer and
//! fails on any forbidden import direction. Inline fully-qualified paths are not
//! scanned; a small number of known residual leaks that cannot be fixed
//! surgically within P9.2 are listed in `ALLOWED_EXCEPTIONS` with a reason.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A forbidden import for a given layer, matched against the module path in a
/// `use crate::<module>::…;` line (or an infra crate in `use <crate>::…;`).
struct Rule {
    /// Human-readable layer name for messages.
    layer: &'static str,
    /// `src`-relative path prefix the rule applies to (POSIX separators).
    applies_to: &'static str,
    /// Sub-prefixes under `applies_to` that are exempt from this rule.
    excluding: &'static [&'static str],
    /// Crate-root module segments forbidden as the first segment after `crate::`.
    forbidden_crate_modules: &'static [&'static str],
    /// External crate roots forbidden as the first path segment (e.g. `tokio`).
    forbidden_extern_crates: &'static [&'static str],
    /// Full multi-segment `use` targets forbidden by prefix (checked with
    /// `starts_with` on the full path after `use ` / `use crate::`).  Each
    /// entry is the literal prefix to deny, e.g. `"std::fs"` or
    /// `"crate::model::repository"`.  Applied to both crate-relative and
    /// extern paths as written in the source.
    forbidden_prefixes: &'static [&'static str],
}

const RULES: &[Rule] = &[
    // Pure core domain: no orchestration/interface/network/config, no async or
    // sqlite infra.
    Rule {
        layer: "model (pure domain)",
        applies_to: "src/model",
        excluding: &["src/model/repository"],
        forbidden_crate_modules: &["operations", "cli", "clients", "config"],
        forbidden_extern_crates: &["tokio", "rusqlite"],
        forbidden_prefixes: &[],
    },
    // Persistence adapter: may import model + config + infra, but not the
    // orchestration or interface layers.
    Rule {
        layer: "model/repository (persistence adapter)",
        applies_to: "src/model/repository",
        excluding: &[],
        forbidden_crate_modules: &["operations", "cli"],
        forbidden_extern_crates: &[],
        forbidden_prefixes: &[],
    },
    // Network adapters: may import model + config, but not operations.
    Rule {
        layer: "clients (network adapter)",
        applies_to: "src/clients",
        excluding: &[],
        forbidden_crate_modules: &["operations"],
        forbidden_extern_crates: &[],
        forbidden_prefixes: &[],
    },
    // Daemon pure core — analyzers: no I/O, no config, no network, no async.
    // Legitimate imports today: crate::error, crate::model::domain,
    // crate::operations::daemon::analyzers, crate::operations::git::{cli_parser,
    // repo_state, command_classification}, std::{path, collections, sync}.
    Rule {
        layer: "daemon analyzers (pure core)",
        applies_to: "src/operations/daemon/analyzers",
        excluding: &[],
        forbidden_crate_modules: &[
            "cli",
            "clients",
            "config",
            "notes",
            "tokio_runtime",
            "observability",
            "metrics",
            "process_timeout",
            "repo_url",
            "feature_flags",
            "diagnostic_sentinels",
        ],
        forbidden_extern_crates: &["tokio", "rusqlite", "ureq", "interprocess", "serde_json"],
        forbidden_prefixes: &[
            "std::fs",
            "std::io",
            "std::process",
            "std::net",
            "std::os",
            "tokio::fs",
            "tokio::net",
            "tokio::process",
            "crate::model::repository",
            "crate::operations::daemon::family_actor",
            "crate::operations::daemon::ref_cursor",
            "crate::operations::daemon::trace_normalizer",
        ],
    },
    // Daemon pure core — reducer: no I/O, no config, no network, no async.
    // Legitimate imports today: crate::error, crate::model::domain,
    // crate::operations::daemon::analyzers, std::path.
    Rule {
        layer: "daemon reducer (pure core)",
        applies_to: "src/operations/daemon/reducer.rs",
        excluding: &[],
        forbidden_crate_modules: &[
            "cli",
            "clients",
            "config",
            "notes",
            "tokio_runtime",
            "observability",
            "metrics",
            "process_timeout",
            "repo_url",
            "feature_flags",
            "diagnostic_sentinels",
        ],
        forbidden_extern_crates: &["tokio", "rusqlite", "ureq", "interprocess", "serde_json"],
        forbidden_prefixes: &[
            "std::fs",
            "std::io",
            "std::process",
            "std::net",
            "std::os",
            "tokio::fs",
            "tokio::net",
            "tokio::process",
            "crate::model::repository",
            "crate::operations::daemon::family_actor",
            "crate::operations::daemon::ref_cursor",
            "crate::operations::daemon::trace_normalizer",
        ],
    },
];

/// Known residual leaks that cannot be fixed surgically without a larger
/// refactor. Each entry is one deliberate escape hatch; the list is empty
/// after P9.3 moved the last exception (`transcript::Message`).
const ALLOWED_EXCEPTIONS: &[(&str, &str)] = &[];

/// Files in the daemon pure core whose non-test content is checked for IO
/// substrings that use-line scanning cannot catch (e.g. `.canonicalize()`,
/// inline `std::process::` paths).  See the two scanner-gap notes
/// in the map for reducer.rs and generic.rs.
const PURE_CORE_IO_CHECK_FILES: &[&str] = &[
    "src/operations/daemon/reducer.rs",
    "src/operations/daemon/analyzers/mod.rs",
    "src/operations/daemon/analyzers/generic.rs",
    "src/operations/daemon/analyzers/history.rs",
    "src/operations/daemon/analyzers/transport.rs",
    "src/operations/daemon/analyzers/workspace.rs",
];

/// Substrings whose presence in non-test code signals filesystem or process IO.
const PURE_CORE_FORBIDDEN_IO_SUBSTRINGS: &[&str] = &[
    ".canonicalize(",
    "std::process::",
    "std::fs::",
    "std::io::",
    "File::open",
];

fn rule_applies(rule: &Rule, rel: &str) -> bool {
    if !rel.starts_with(rule.applies_to) {
        return false;
    }
    !rule.excluding.iter().any(|ex| rel.starts_with(ex))
}

/// Expand a single `use` path that may contain a brace group into its concrete
/// member paths.  Only single-level brace groups are supported (e.g.
/// `std::{fs, io}` → `["std::fs", "std::io"]`).  If a nested brace group is
/// detected, this function panics with a message asking the author to split the
/// import — full recursive expansion is not warranted given this codebase's
/// import style.
///
/// `path` must be the target after `use ` (or after `use crate::` for
/// crate-relative paths), already stripped of trailing `;` / whitespace.
fn expand_use_path(path: &str) -> Vec<String> {
    let Some(brace_start) = path.find('{') else {
        return vec![path.to_string()];
    };
    let prefix = &path[..brace_start];
    let Some(brace_end) = path.find('}') else {
        // Malformed — just return as-is; the outer check will not match.
        return vec![path.to_string()];
    };
    let inner = &path[brace_start + 1..brace_end];
    if inner.contains('{') {
        panic!(
            "layer_import_policy: nested brace group in `use {path}` is not supported by the \
             policy scanner — please split this import into separate `use` lines so each path \
             can be checked individually."
        );
    }
    inner
        .split(',')
        .map(|m| format!("{}{}", prefix, m.trim()))
        .collect()
}

/// Extract the first path segment and expanded full paths from a `use` line.
///
/// Returns `(is_crate_relative, first_segment, expanded_full_paths)` where
/// `expanded_full_paths` are fully-qualified paths starting with either
/// `"crate::"` (for crate-relative imports) or the crate name (for extern
/// imports), suitable for `starts_with` matching against `forbidden_prefixes`.
///
/// Brace groups are expanded so that `use std::{fs, io};` yields two entries
/// `["std::fs", "std::io"]`.
fn parse_use_target(line: &str) -> Option<(bool, &str, Vec<String>)> {
    let line = line.trim();
    // Accept both `use …` and `pub use …` (and `pub(crate) use …`).
    let rest = line
        .strip_prefix("pub(crate) use ")
        .or_else(|| line.strip_prefix("pub use "))
        .or_else(|| line.strip_prefix("use "))?;
    const SEP: [char; 5] = [':', ';', '{', ' ', ','];
    if let Some(after_crate) = rest.strip_prefix("crate::") {
        let seg = after_crate.split(SEP).next()?;
        // Slice full_target from after_crate so it contains only the path
        // after "crate::" (without the prefix itself), then expand brace groups
        // and re-attach the "crate::" prefix for matching.
        // Use ';' as the sole terminator so that spaces inside brace groups
        // (e.g. `{repository, domain}`) are not treated as end-of-path.
        let target_end = after_crate.find(';').unwrap_or(after_crate.len());
        let raw_target = &after_crate[..target_end];
        let expanded = expand_use_path(raw_target)
            .into_iter()
            .map(|m| format!("crate::{m}"))
            .collect();
        return Some((true, seg, expanded));
    }
    let seg = rest.split(SEP).next()?;
    let target_end = rest.find(';').unwrap_or(rest.len());
    let raw_target = &rest[..target_end];
    let expanded = expand_use_path(raw_target);
    Some((false, seg, expanded))
}

fn collect_src_files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut stack = vec![root.join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src dir") {
            let entry = entry.expect("read dir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let content = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
                let rel = path
                    .strip_prefix(root)
                    .expect("src path under repo root")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push((rel, content));
            }
        }
    }
    files
}

/// Returns the portion of `content` before the trailing test module.
///
/// The trailing test module is identified as the last `#[cfg(test)]` that is
/// immediately followed (on the next non-empty line) by `mod `.  If a
/// `#[cfg(test)]` occurs in the file but is NOT followed by `mod ` (e.g. an
/// early `#[cfg(test)] use …`), this function panics with a clear message so
/// that authors cannot silently exempt production code from the IO scan.
fn non_test_content(content: &str) -> &str {
    // Find all occurrences of "#[cfg(test)]" and locate the one that introduces
    // the trailing test module.
    let mut test_module_offset: Option<usize> = None;
    let mut search_start = 0;
    while let Some(rel_offset) = content[search_start..].find("#[cfg(test)]") {
        let offset = search_start + rel_offset;
        // Find the next non-empty line after this marker.
        let after_marker = &content[offset + "#[cfg(test)]".len()..];
        let next_line = after_marker
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        if next_line.trim().starts_with("mod ") {
            test_module_offset = Some(offset);
            // Keep searching: we want the LAST such marker (should be only one,
            // but we want to be safe).
        } else {
            // A #[cfg(test)] that does NOT introduce a mod — it must be an
            // item-level gate. We still need to ensure it's inside the test
            // module we already found, or panic.
            if test_module_offset.is_none() || offset < test_module_offset.unwrap() {
                panic!(
                    "layer_import_policy: found `#[cfg(test)]` at byte offset {offset} that is \
                     NOT followed by `mod …` (it appears to gate an individual item outside the \
                     test module). This would silently exempt production code from the IO scan. \
                     Move this item inside the test module or re-evaluate the policy."
                );
            }
            // It's inside the test module already found — that's fine.
        }
        search_start = offset + 1;
    }
    match test_module_offset {
        Some(idx) => &content[..idx],
        None => content,
    }
}

#[test]
fn src_layers_respect_import_direction() {
    let root = repo_root();
    let files = collect_src_files(&root);
    let mut violations = Vec::new();

    for (rel, content) in &files {
        let excepted = ALLOWED_EXCEPTIONS.iter().any(|(f, _)| f == rel);
        for rule in RULES {
            if !rule_applies(rule, rel) {
                continue;
            }
            for (lineno, line) in content.lines().enumerate() {
                let Some((is_crate, seg, expanded_paths)) = parse_use_target(line) else {
                    continue;
                };
                let forbidden_first_seg = if is_crate {
                    rule.forbidden_crate_modules.contains(&seg)
                } else {
                    rule.forbidden_extern_crates.contains(&seg)
                };
                // Prefix check: match each expanded path against each forbidden
                // prefix. Brace groups are already expanded by parse_use_target.
                let forbidden_prefix = expanded_paths.iter().any(|expanded| {
                    rule.forbidden_prefixes
                        .iter()
                        .any(|p| expanded.starts_with(p))
                });
                if (forbidden_first_seg || forbidden_prefix) && !excepted {
                    violations.push(format!(
                        "{rel}:{}: {} layer must not `use {}{}::…`  ({})",
                        lineno + 1,
                        rule.layer,
                        if is_crate { "crate::" } else { "" },
                        seg,
                        line.trim(),
                    ));
                }
            }
        }
    }

    // Guard against stale exceptions: every listed file must still exist.
    for (f, _) in ALLOWED_EXCEPTIONS {
        assert!(
            files.iter().any(|(rel, _)| rel == f),
            "ALLOWED_EXCEPTIONS lists {f}, which no longer exists — remove its entry"
        );
    }

    assert!(
        violations.is_empty(),
        "layer import-direction violations:\n  {}",
        violations.join("\n  ")
    );
}

/// Textual IO-freedom check for the daemon pure core.
///
/// Closes two scanner gaps noted in the analysis map:
/// (1) inline fully-qualified calls like `crate::ops::foo::bar()` have no `use`
///     statement and are invisible to the import scanner.
/// (2) methods on `std::path::Path` (e.g. `.canonicalize()`) perform real
///     filesystem syscalls with no import signature.
///
/// This test scans the non-test portion of each pure-core file for substrings
/// that indicate filesystem or process IO and fails if any are found.
#[test]
fn daemon_pure_core_is_io_free() {
    let root = repo_root();
    let files = collect_src_files(&root);
    let mut violations = Vec::new();

    for check_rel in PURE_CORE_IO_CHECK_FILES {
        let Some((_, content)) = files.iter().find(|(rel, _)| rel == check_rel) else {
            panic!("pure-core IO check: expected file {check_rel} not found in src/");
        };
        let production = non_test_content(content);
        for forbidden in PURE_CORE_FORBIDDEN_IO_SUBSTRINGS {
            if let Some(offset) = production.find(forbidden) {
                let lineno = production[..offset].bytes().filter(|b| *b == b'\n').count() + 1;
                violations.push(format!(
                    "{check_rel}:{lineno}: pure core must not contain `{forbidden}`"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "daemon pure-core IO violations:\n  {}",
        violations.join("\n  ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_use_target unit tests ---

    #[test]
    fn parse_use_target_crate_relative_forbidden_prefix() {
        // A crate-relative import that should match a prefix rule like
        // "crate::model::repository".
        let result = parse_use_target("use crate::model::repository::x;");
        let (is_crate, seg, expanded) = result.expect("should parse");
        assert!(is_crate);
        assert_eq!(seg, "model");
        assert_eq!(expanded, vec!["crate::model::repository::x"]);
        // Verify it matches the forbidden prefix.
        assert!(
            expanded
                .iter()
                .any(|p| p.starts_with("crate::model::repository"))
        );
    }

    #[test]
    fn parse_use_target_grouped_import_expansion() {
        // `use std::{fs, io};` should expand to two entries and match
        // "std::fs" and "std::io" prefix rules.
        let result = parse_use_target("use std::{fs, io};");
        let (is_crate, seg, expanded) = result.expect("should parse");
        assert!(!is_crate);
        assert_eq!(seg, "std");
        assert!(
            expanded.contains(&"std::fs".to_string()),
            "expanded = {expanded:?}"
        );
        assert!(
            expanded.contains(&"std::io".to_string()),
            "expanded = {expanded:?}"
        );
        assert!(expanded.iter().any(|p| p.starts_with("std::fs")));
        assert!(expanded.iter().any(|p| p.starts_with("std::io")));
    }

    #[test]
    fn parse_use_target_grouped_crate_import_matches_prefix() {
        // `use crate::model::{repository, domain};` should expand so that
        // "crate::model::repository" matches a forbidden prefix for that entry.
        let result = parse_use_target("use crate::model::{repository, domain};");
        let (is_crate, seg, expanded) = result.expect("should parse");
        assert!(is_crate);
        assert_eq!(seg, "model");
        assert!(
            expanded.contains(&"crate::model::repository".to_string()),
            "expanded = {expanded:?}"
        );
        assert!(
            expanded.contains(&"crate::model::domain".to_string()),
            "expanded = {expanded:?}"
        );
        // Prefix match: "crate::model::repository" starts_with "crate::model::repository".
        assert!(
            expanded
                .iter()
                .any(|p| p.starts_with("crate::model::repository"))
        );
    }
}
