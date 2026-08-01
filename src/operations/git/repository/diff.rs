//! `impl Repository` diff methods plus the unified-diff parsing helpers they
//! rely on. Also hosts Git version and numstat parsing shared by Git-facing
//! callers, and the public `parse_diff_added_lines_with_insertions` entry point.

use super::core::Repository;
use crate::clients::git_cli::{InternalGitProfile, exec_git, exec_git_with_profile};
use crate::error::GitAiError;
use crate::operations::git::status::MAX_PATHSPEC_ARGS;
use std::collections::{HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

impl Repository {
    /// Get added line ranges from git diff between two commits
    /// Returns a HashMap of file paths to vectors of added line numbers
    ///
    /// Uses `git diff -U0` to get unified diff with zero context lines,
    /// then parses the hunk headers to extract line numbers directly.
    /// This is much faster than fetching blobs and running TextDiff manually.
    pub fn diff_added_lines(
        &self,
        from_ref: &str,
        to_ref: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<HashMap<String, Vec<u32>>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string()); // Zero context lines
        args.push("--no-color".to_string());
        // Use permissive rename detection to properly handle renames
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        // Add pathspecs if provided (only as CLI args when under threshold).
        // Force post-filtering when any pathspec contains non-ASCII characters,
        // because NFC-normalised pathspecs may not match NFD entries in git's
        // index on macOS when core.precomposeunicode is false.
        let needs_post_filter = if let Some(paths) = pathspecs {
            if paths.is_empty() {
                return Ok(HashMap::new());
            }
            if paths.len() > MAX_PATHSPEC_ARGS || has_non_ascii_pathspec(paths) {
                true
            } else {
                args.push("--".to_string());
                for path in paths {
                    args.push(path.clone());
                }
                false
            }
        } else {
            false
        };

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        let (mut result, _deleted_count) = parse_diff_added_lines(&diff_output)?;

        if needs_post_filter && let Some(paths) = pathspecs {
            let nfc_paths: HashSet<String> = paths.iter().map(|s| s.nfc().collect()).collect();
            result.retain(|path, _| nfc_paths.contains(path));
        }

        Ok(result)
    }

    /// Like `diff_added_lines` but also returns the total number of deleted
    /// lines across all hunks in the diff.  Used by the post-commit stats-cost
    /// estimator to detect deletion-heavy commits without a second git invocation.
    pub fn diff_added_lines_with_deleted_count(
        &self,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<(HashMap<String, Vec<u32>>, usize), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string());
        args.push("--no-color".to_string());
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        parse_diff_added_lines(&diff_output)
    }

    /// Get list of changed files between two refs using `git diff --name-only`
    /// Returns a Vec of file paths that differ between the two refs
    pub fn diff_changed_files(
        &self,
        from_ref: &str,
        to_ref: &str,
    ) -> Result<Vec<String>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("--name-only".to_string());
        args.push("-z".to_string()); // NUL-separated output for proper UTF-8 handling
        // Use permissive rename detection to properly handle renames
        args.push("--find-renames=1%".to_string());
        args.push(from_ref.to_string());
        args.push(to_ref.to_string());

        let output = exec_git_with_profile(&args, InternalGitProfile::RawDiffParse)?;

        // With -z, output is NUL-separated. The output may contain a trailing NUL.
        let files: Vec<String> = output
            .stdout
            .split(|&b| b == 0)
            .filter(|bytes| !bytes.is_empty())
            .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .collect();

        Ok(files)
    }

    /// Get added line ranges from git diff between a commit and the working directory
    /// Returns a HashMap of file paths to vectors of added line numbers
    ///
    /// Get added line ranges from git diff between a commit and the working directory,
    /// along with information about which lines are pure insertions (old_count=0).
    ///
    /// Returns (all_added_lines, pure_insertion_lines)
    /// Pure insertions are lines that were added without modifying existing lines at that position.
    #[allow(clippy::type_complexity)]
    pub fn diff_workdir_added_lines_with_insertions(
        &self,
        from_ref: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<(HashMap<String, Vec<u32>>, HashMap<String, Vec<u32>>), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("diff".to_string());
        args.push("-U0".to_string()); // Zero context lines
        args.push("--no-color".to_string());
        args.push("--no-renames".to_string());
        args.push(from_ref.to_string());

        // See diff_added_lines for why non-ASCII pathspecs need post-filtering.
        let needs_post_filter = if let Some(paths) = pathspecs {
            if paths.is_empty() {
                return Ok((HashMap::new(), HashMap::new()));
            }
            if paths.len() > MAX_PATHSPEC_ARGS || has_non_ascii_pathspec(paths) {
                true
            } else {
                args.push("--".to_string());
                for path in paths {
                    args.push(path.clone());
                }
                false
            }
        } else {
            false
        };

        let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
        let diff_output = String::from_utf8_lossy(&output.stdout);

        let (mut all_added, mut pure_insertions) =
            parse_diff_added_lines_with_insertions(&diff_output)?;

        if needs_post_filter && let Some(paths) = pathspecs {
            let nfc_paths: HashSet<String> = paths.iter().map(|s| s.nfc().collect()).collect();
            all_added.retain(|path, _| nfc_paths.contains(path));
            pure_insertions.retain(|path, _| nfc_paths.contains(path));
        }

        Ok((all_added, pure_insertions))
    }

    pub fn fetch_branch(&self, branch_name: &str, remote_name: &str) -> Result<(), GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("fetch".to_string());
        args.push(remote_name.to_string());
        args.push(branch_name.to_string());
        exec_git(&args)?;
        Ok(())
    }
}

/// Parse git version string (e.g., "git version 2.39.3 (Apple Git-146)") to extract major, minor, patch.
/// Returns None if the version cannot be parsed.
#[doc(hidden)]
pub fn parse_git_version(version_str: &str) -> Option<(u32, u32, u32)> {
    // Expected format: "git version X.Y.Z" or "git version X.Y.Z.windows.N" etc.
    let version_str = version_str.trim();
    let parts: Vec<&str> = version_str.split_whitespace().collect();

    // Find the version number part (usually the 3rd element)
    let version_part = parts.get(2)?;

    // Parse version like "2.39.3" or "2.39.3.windows.1"
    let version_nums: Vec<&str> = version_part.split('.').collect();
    if version_nums.len() < 2 {
        return None;
    }

    let major = version_nums.first()?.parse::<u32>().ok()?;
    let minor = version_nums.get(1)?.parse::<u32>().ok()?;
    let patch = version_nums
        .get(2)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Probe the configured Git executable for its version.
pub(crate) fn probe_configured_git_version() -> Option<(u32, u32, u32)> {
    let output = exec_git(&["--version".to_string()]).ok()?;
    let version_str = String::from_utf8(output.stdout).ok()?;
    parse_git_version(&version_str)
}

/// One parsed row from `git diff --numstat` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NumstatLine<'a> {
    pub(crate) added: Option<u32>,
    pub(crate) deleted: Option<u32>,
    pub(crate) path: &'a str,
}

/// Parse one `git diff --numstat` row while preserving callers' binary and
/// malformed-count behavior.
pub(crate) fn parse_numstat_line(line: &str) -> Option<NumstatLine<'_>> {
    if line.trim().is_empty() {
        return None;
    }

    let mut parts = line.split('\t');
    let added = parts.next()?;
    let deleted = parts.next()?;
    let path = parts.next()?;

    Some(NumstatLine {
        added: added.parse().ok(),
        deleted: deleted.parse().ok(),
        path,
    })
}

/// Parse git diff output to extract added line numbers per file
///
/// Parses unified diff format hunk headers like:
/// @@ -10,2 +15,5 @@
///
/// This means: old file line 10 (2 lines), new file line 15 (5 lines)
/// We extract the "new file" line numbers to know which lines were added.
///
/// Also returns the total number of deleted lines across all hunks so that
/// callers can estimate the cost of a deletion-heavy commit without a second
/// git invocation.
fn parse_diff_added_lines(
    diff_output: &str,
) -> Result<(HashMap<String, Vec<u32>>, usize), GitAiError> {
    let parsed = parse_diff_added_lines_internal(diff_output);
    Ok((parsed.all_lines, parsed.total_deleted))
}

struct ParsedDiffAddedLines {
    all_lines: HashMap<String, Vec<u32>>,
    insertion_lines: HashMap<String, Vec<u32>>,
    total_deleted: usize,
}

struct ActiveDiffHunk {
    new_line: u32,
    is_pure_insertion: bool,
}

fn parse_diff_added_lines_internal(diff_output: &str) -> ParsedDiffAddedLines {
    let mut result: HashMap<String, Vec<u32>> = HashMap::new();
    let mut insertion_lines: HashMap<String, Vec<u32>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut current_hunk: Option<ActiveDiffHunk> = None;
    let mut total_deleted: usize = 0;

    for line in diff_output.lines() {
        if let Some(path_opt) = parse_new_file_path_from_plus_header_line(line) {
            current_file = path_opt;
            current_hunk = None;
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some((new_start, _new_count, old_count)) = parse_hunk_header_counts(line) {
                // Count deleted lines for ALL hunks, including those from purely
                // deleted files (where current_file is None because +++ /dev/null).
                total_deleted += old_count as usize;
                current_hunk = Some(ActiveDiffHunk {
                    new_line: new_start,
                    is_pure_insertion: old_count == 0,
                });
            }
        } else if let Some(hunk) = current_hunk.as_mut() {
            if line.starts_with('+') {
                if let Some(ref file) = current_file {
                    result.entry(file.clone()).or_default().push(hunk.new_line);
                    if hunk.is_pure_insertion {
                        insertion_lines
                            .entry(file.clone())
                            .or_default()
                            .push(hunk.new_line);
                    }
                }
                hunk.new_line += 1;
            } else if line.starts_with('-') || line.starts_with('\\') {
                // Removed lines and "\ No newline at end of file" markers do
                // not advance the new-file line cursor.
            } else {
                hunk.new_line += 1;
            }
        }
    }

    // Sort and deduplicate line numbers for each file
    for lines in result.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }
    for lines in insertion_lines.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    ParsedDiffAddedLines {
        all_lines: result,
        insertion_lines,
        total_deleted,
    }
}

/// Parses the unified diff output to extract line numbers of added lines,
/// along with information about which are pure insertions (old_count=0).
///
/// Returns (all_added_lines, pure_insertion_lines)
#[allow(clippy::type_complexity)]
#[doc(hidden)]
pub fn parse_diff_added_lines_with_insertions(
    diff_output: &str,
) -> Result<(HashMap<String, Vec<u32>>, HashMap<String, Vec<u32>>), GitAiError> {
    let parsed = parse_diff_added_lines_internal(diff_output);
    Ok((parsed.all_lines, parsed.insertion_lines))
}

/// Returns true if any path in the set contains non-ASCII characters.
/// Used to decide whether git pathspecs need post-filtering instead of CLI args,
/// since NFC-normalised pathspecs may not match NFD entries in git's index.
fn has_non_ascii_pathspec(paths: &HashSet<String>) -> bool {
    paths.iter().any(|s| !s.is_ascii())
}

fn normalize_diff_path_token(path: &str) -> String {
    let unescaped = crate::operations::git::path_format::unescape_git_path(path.trim_end());
    let prefixes = ["a/", "b/", "c/", "w/", "i/", "o/"];
    let stripped = prefixes
        .iter()
        .find_map(|prefix| unescaped.strip_prefix(prefix))
        .unwrap_or(&unescaped);
    // Apply NFC normalization so decomposed (NFD) paths from git diff match
    // NFC paths used internally (see normalize_to_posix).
    stripped.nfc().collect()
}

fn parse_new_file_path_from_plus_header_line(line: &str) -> Option<Option<String>> {
    let raw = line.strip_prefix("+++ ")?;
    if raw.trim_end() == "/dev/null" {
        return Some(None);
    }
    Some(Some(normalize_diff_path_token(raw)))
}

fn parse_hunk_header_counts(line: &str) -> Option<(u32, u32, u32)> {
    // Find the part between @@ and @@
    let parts: Vec<&str> = line.split("@@").collect();
    if parts.len() < 2 {
        return None;
    }

    let hunk_info = parts[1].trim();

    // Split by space to get old and new ranges
    let ranges: Vec<&str> = hunk_info.split_whitespace().collect();
    if ranges.len() < 2 {
        return None;
    }

    // Parse the old file range (starts with '-')
    let old_range = ranges
        .iter()
        .find(|r| r.starts_with('-'))?
        .trim_start_matches('-');

    // Parse "start,count" or just "start" for old range
    let old_parts: Vec<&str> = old_range.split(',').collect();
    let old_count: u32 = if old_parts.len() > 1 {
        old_parts[1].parse().ok()?
    } else {
        1 // If no count specified, it's 1 line
    };

    // Parse the new file range (starts with '+')
    let new_range = ranges
        .iter()
        .find(|r| r.starts_with('+'))?
        .trim_start_matches('+');

    // Parse "start,count" or just "start"
    let new_parts: Vec<&str> = new_range.split(',').collect();
    let start: u32 = new_parts[0].parse().ok()?;
    let count: u32 = if new_parts.len() > 1 {
        new_parts[1].parse().ok()?
    } else {
        1 // If no count specified, it's 1 line
    };

    Some((start, count, old_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_version_standard() {
        // Standard git version format
        assert_eq!(parse_git_version("git version 2.39.3"), Some((2, 39, 3)));
        assert_eq!(parse_git_version("git version 2.23.0"), Some((2, 23, 0)));
        assert_eq!(parse_git_version("git version 1.8.5"), Some((1, 8, 5)));
    }

    #[test]
    fn test_parse_git_version_apple_git() {
        // macOS Apple Git format
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn test_parse_git_version_windows() {
        // Windows git format
        assert_eq!(
            parse_git_version("git version 2.42.0.windows.2"),
            Some((2, 42, 0))
        );
    }

    #[test]
    fn test_parse_git_version_no_patch() {
        // Version without patch number
        assert_eq!(parse_git_version("git version 2.39"), Some((2, 39, 0)));
    }

    #[test]
    fn test_parse_git_version_with_newline() {
        // Version string with trailing newline
        assert_eq!(parse_git_version("git version 2.39.3\n"), Some((2, 39, 3)));
    }

    #[test]
    fn test_parse_git_version_invalid() {
        // Invalid formats should return None
        assert_eq!(parse_git_version(""), None);
        assert_eq!(parse_git_version("not a version"), None);
        assert_eq!(parse_git_version("git version"), None);
        assert_eq!(parse_git_version("git version x.y.z"), None);
    }

    #[test]
    fn test_parse_numstat_line_preserves_binary_and_malformed_count_semantics() {
        assert_eq!(
            parse_numstat_line("12\t7\tsrc/lib.rs"),
            Some(NumstatLine {
                added: Some(12),
                deleted: Some(7),
                path: "src/lib.rs",
            })
        );
        assert_eq!(
            parse_numstat_line("-\t-\tassets/logo.png"),
            Some(NumstatLine {
                added: None,
                deleted: None,
                path: "assets/logo.png",
            })
        );
        assert_eq!(
            parse_numstat_line("not-a-number\t3\tbroken-added.rs"),
            Some(NumstatLine {
                added: None,
                deleted: Some(3),
                path: "broken-added.rs",
            })
        );
        assert_eq!(parse_numstat_line("\t\t"), None);
        assert_eq!(parse_numstat_line("1\t2"), None);
    }
}
