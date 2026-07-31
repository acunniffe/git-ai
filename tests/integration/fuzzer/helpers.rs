use std::collections::HashSet;

use crate::repos::blame_support::is_ai_blame_author;

pub struct PorcelainLineInfo {
    pub commit_sha: String,
    pub orig_line: u32,
}

pub fn parse_porcelain_line_info(porcelain: &str) -> Vec<PorcelainLineInfo> {
    let mut result = Vec::new();
    for line in porcelain.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && parts[0].len() == 40
            && parts[0].chars().all(|c| c.is_ascii_hexdigit())
            && let Ok(orig_line) = parts[1].parse::<u32>()
        {
            result.push(PorcelainLineInfo {
                commit_sha: parts[0].to_string(),
                orig_line,
            });
        }
    }
    result
}

/// The three attribution classes the fuzzer asserts, derived from a
/// `git-ai blame --show-prompt` author column. That mode prints:
///   - an agent tool name with a session hash for AI lines (e.g. `mock_ai [s_..]`),
///   - an `h_`-prefixed hash for known-human attestations,
///   - a plain git author name for everything else (untracked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlameClass {
    Ai,
    KnownHuman,
    Untracked,
}

/// Classify the author column of one `--show-prompt` blame line into one of the
/// three attribution classes. `author` is the already-extracted author string
/// (see `crate::repos::blame_support::parse_blame_line`).
pub fn classify_show_prompt_author(author: &str) -> BlameClass {
    if is_ai_blame_author(author) {
        return BlameClass::Ai;
    }
    // Known-human attestations surface as the `h_`-prefixed identity hash in
    // --show-prompt mode. Untracked lines surface as a plain git author name.
    if author.split_whitespace().any(|tok| tok.starts_with("h_")) {
        return BlameClass::KnownHuman;
    }
    BlameClass::Untracked
}

pub fn note_covers_line_as_ai(note: &str, filename: &str, line_num: u32) -> bool {
    let valid_sessions = extract_metadata_sessions(note);
    let mut in_target_file = false;

    for raw_line in note.lines() {
        let trimmed = raw_line.trim();

        if trimmed.starts_with('{') || trimmed == "---" {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !raw_line.starts_with(' ') && !raw_line.starts_with('\t') {
            if in_target_file {
                return false;
            }
            in_target_file = trimmed == filename || trimmed.ends_with(&format!("/{}", filename));
            continue;
        }
        if !in_target_file {
            continue;
        }

        if let Some(space_idx) = trimmed.rfind(' ') {
            let author_part = &trimmed[..space_idx];
            let ranges_part = &trimmed[space_idx + 1..];
            if is_valid_line_ranges(ranges_part) && author_part.starts_with("s_") {
                if let Some(ref sessions) = valid_sessions {
                    let session_key = author_part.split("::").next().unwrap_or(author_part);
                    if !sessions.contains(session_key) {
                        continue;
                    }
                }
                let ranges = parse_line_ranges(ranges_part);
                for (start, end) in ranges {
                    if line_num >= start && line_num <= end {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn note_covers_line_as_human(note: &str, filename: &str, line_num: u32) -> bool {
    let mut in_target_file = false;

    for raw_line in note.lines() {
        let trimmed = raw_line.trim();

        if trimmed.starts_with('{') || trimmed == "---" {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !raw_line.starts_with(' ') && !raw_line.starts_with('\t') {
            if in_target_file {
                return false;
            }
            in_target_file = trimmed == filename || trimmed.ends_with(&format!("/{}", filename));
            continue;
        }
        if !in_target_file {
            continue;
        }

        if let Some(space_idx) = trimmed.rfind(' ') {
            let author_part = &trimmed[..space_idx];
            let ranges_part = &trimmed[space_idx + 1..];
            if is_valid_line_ranges(ranges_part) && author_part.starts_with("h_") {
                let ranges = parse_line_ranges(ranges_part);
                for (start, end) in ranges {
                    if line_num >= start && line_num <= end {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn parse_line_ranges(ranges_str: &str) -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    for part in ranges_str.split(',') {
        if let Some(dash_idx) = part.find('-') {
            let start = part[..dash_idx].parse::<u32>().unwrap_or(0);
            let end = part[dash_idx + 1..].parse::<u32>().unwrap_or(0);
            if start > 0 && end > 0 {
                result.push((start, end));
            }
        } else if let Ok(line) = part.parse::<u32>()
            && line > 0
        {
            result.push((line, line));
        }
    }
    result
}

fn is_valid_line_ranges(ranges_str: &str) -> bool {
    if ranges_str.is_empty() {
        return false;
    }
    ranges_str
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == ',')
}

pub fn extract_metadata_sessions(note: &str) -> Option<HashSet<&str>> {
    let json_section = if let Some(idx) = note.find("\n---\n") {
        &note[idx + 5..]
    } else {
        note.strip_prefix("---\n")?
    };

    let sessions_idx = json_section.find("\"sessions\"")?;

    let mut sessions = HashSet::new();
    let after_sessions = &json_section[sessions_idx..];
    if let Some(brace_start) = after_sessions.find('{') {
        let sessions_obj = &after_sessions[brace_start..];
        let mut depth = 0;
        let mut end_idx = sessions_obj.len();
        for (i, ch) in sessions_obj.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let sessions_block = &sessions_obj[..end_idx];
        let mut in_quote = false;
        let mut quote_start = 0;
        for (i, ch) in sessions_block.char_indices() {
            if ch == '"' {
                if in_quote {
                    let segment = &sessions_block[quote_start..i];
                    if segment.starts_with("s_") && segment.len() > 2 {
                        sessions.insert(segment);
                    }
                } else {
                    quote_start = i + 1;
                }
                in_quote = !in_quote;
            }
        }
    }

    Some(sessions)
}
