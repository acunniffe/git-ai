//! Parsing of `git diff` header lines: `diff --git` path pairs and
//! `---`/`+++` file-path tokens, normalized to repo-relative POSIX paths.

use crate::operations::git::path_format::{
    normalize_diff_path_token, parse_diff_path_from_header_line,
};

pub(super) fn parse_new_file_path_from_plus_header_line(line: &str) -> Option<Option<String>> {
    parse_diff_path_from_header_line(line, "+++ ")
}

pub(super) fn parse_old_file_path_from_minus_header_line(line: &str) -> Option<Option<String>> {
    parse_diff_path_from_header_line(line, "--- ")
}

pub(super) fn parse_diff_git_header_paths(line: &str) -> Option<(String, String)> {
    let raw = line.strip_prefix("diff --git ")?;
    let (old_raw, new_raw) = parse_two_git_path_tokens(raw)?;
    Some((
        normalize_diff_path_token(&old_raw),
        normalize_diff_path_token(&new_raw),
    ))
}

fn parse_two_git_path_tokens(raw: &str) -> Option<(String, String)> {
    let mut chars = raw.chars().peekable();
    let mut tokens: Vec<String> = Vec::new();

    while tokens.len() < 2 {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let mut token = String::new();
        if chars.peek() == Some(&'"') {
            token.push(chars.next().unwrap_or('"'));
            let mut escaped = false;
            for ch in chars.by_ref() {
                token.push(ch);
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    break;
                }
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                token.push(ch);
                chars.next();
            }
        }

        if token.is_empty() {
            return None;
        }
        tokens.push(token);
    }

    if tokens.len() == 2 {
        Some((tokens[0].clone(), tokens[1].clone()))
    } else {
        None
    }
}
