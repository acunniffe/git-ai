//! Path formatting helpers for git-produced paths: POSIX separator
//! normalization and git's C-style quoted-path unescaping.

use unicode_normalization::UnicodeNormalization;

#[inline]
pub fn normalize_to_posix(path: &str) -> String {
    path.replace('\\', "/")
}

pub(crate) fn normalize_diff_path_token(path: &str) -> String {
    let unescaped = unescape_git_path(path.trim_end());
    let prefixes = ["a/", "b/", "c/", "w/", "i/", "o/"];
    let stripped = prefixes
        .iter()
        .find_map(|prefix| unescaped.strip_prefix(prefix))
        .unwrap_or(&unescaped);
    stripped.nfc().collect()
}

pub(crate) fn parse_diff_path_from_header_line(line: &str, prefix: &str) -> Option<Option<String>> {
    let raw = line.strip_prefix(prefix)?;
    if raw.trim_end() == "/dev/null" {
        return Some(None);
    }
    Some(Some(normalize_diff_path_token(raw)))
}

/// Unescape a git-quoted path that may contain octal escape sequences.
///
/// Git quotes filenames containing non-ASCII characters (and some special characters)
/// using C-style escaping with octal sequences. For example, a Chinese filename like
/// "中文.txt" would appear as `"\344\270\255\346\226\207.txt"` in git output.
///
/// This function handles:
/// - Quoted paths: removes surrounding quotes and unescapes content
/// - Octal escapes: converts `\NNN` sequences back to UTF-8 bytes
/// - Other escapes: `\\`, `\"`, `\n`, `\t`, etc.
/// - Unquoted paths: returned as-is
///
/// # Examples
///
/// ```
/// use git_ai::operations::git::path_format::unescape_git_path;
///
/// // Unquoted path - returned as-is
/// assert_eq!(unescape_git_path("simple.txt"), "simple.txt");
///
/// // Quoted path with spaces
/// assert_eq!(unescape_git_path("\"path with spaces.txt\""), "path with spaces.txt");
///
/// // Chinese characters encoded as octal
/// assert_eq!(unescape_git_path("\"\\344\\270\\255\\346\\226\\207.txt\""), "中文.txt");
/// ```
pub fn unescape_git_path(path: &str) -> String {
    // If not quoted, return as-is
    if !path.starts_with('"') || !path.ends_with('"') {
        return path.to_string();
    }

    // Remove surrounding quotes
    let inner = &path[1..path.len() - 1];

    // Parse escape sequences and collect bytes
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\\') => {
                    chars.next();
                    bytes.push(b'\\');
                }
                Some('"') => {
                    chars.next();
                    bytes.push(b'"');
                }
                Some('n') => {
                    chars.next();
                    bytes.push(b'\n');
                }
                Some('t') => {
                    chars.next();
                    bytes.push(b'\t');
                }
                Some('r') => {
                    chars.next();
                    bytes.push(b'\r');
                }
                Some(d) if d.is_ascii_digit() => {
                    // Octal escape sequence: \NNN (1-3 octal digits)
                    let mut octal = String::new();
                    for _ in 0..3 {
                        if let Some(&d) = chars.peek() {
                            if d.is_ascii_digit() && d <= '7' {
                                octal.push(chars.next().unwrap());
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if let Ok(byte_val) = u8::from_str_radix(&octal, 8) {
                        bytes.push(byte_val);
                    }
                }
                _ => {
                    // Unknown escape - keep the backslash
                    bytes.push(b'\\');
                }
            }
        } else {
            // Regular character - encode as UTF-8
            let mut buf = [0u8; 4];
            let encoded = c.encode_utf8(&mut buf);
            bytes.extend_from_slice(encoded.as_bytes());
        }
    }

    // Convert bytes to UTF-8 string
    String::from_utf8(bytes).unwrap_or_else(|e| {
        // If invalid UTF-8, try lossy conversion
        String::from_utf8_lossy(e.as_bytes()).into_owned()
    })
}

#[cfg(test)]
#[path = "path_format_tests.rs"]
mod tests;
