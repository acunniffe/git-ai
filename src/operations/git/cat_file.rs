//! Batched reads from Git's object database through `git cat-file --batch`.
//!
//! Keeping the byte framing and missing-object policy here gives every caller
//! the same parser while retaining a single Git process for an arbitrary batch.

use std::collections::HashMap;
use std::time::Duration;

use crate::clients::git_cli::exec_git_stdin;
use crate::error::GitAiError;
use crate::operations::git::repository::Repository;

// Git for Windows can briefly deny access to a repository's config while a
// concurrent Git process is finishing an update. Keep this retry bounded: a
// real config or repository error must still reach the caller promptly.
const CONFIG_ACCESS_RETRY_DELAYS: &[Duration] = &[
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

pub(crate) fn batch_read_blob_contents(
    repo: &Repository,
    blob_oids: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    if blob_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut args = repo.global_args_for_exec();
    args.push("cat-file".to_string());
    args.push("--batch".to_string());

    let stdin_data = blob_oids.join("\n") + "\n";
    let output = exec_cat_file(&args, stdin_data.as_bytes())?;
    let results = parse_batch_output(&output.stdout)?;
    for oid in blob_oids {
        if !results.contains_key(oid) {
            return Err(GitAiError::Generic(format!(
                "missing git blob object referenced by authorship note: {}",
                oid
            )));
        }
    }
    Ok(results)
}

fn exec_cat_file(args: &[String], stdin_data: &[u8]) -> Result<std::process::Output, GitAiError> {
    with_config_access_retry(|| exec_git_stdin(args, stdin_data))
}

fn with_config_access_retry<T>(
    mut operation: impl FnMut() -> Result<T, GitAiError>,
) -> Result<T, GitAiError> {
    for (attempt, delay) in CONFIG_ACCESS_RETRY_DELAYS.iter().enumerate() {
        match operation() {
            Ok(output) => return Ok(output),
            Err(error) if attempt + 1 < CONFIG_ACCESS_RETRY_DELAYS.len() => {
                if is_transient_config_access_error(&error) {
                    std::thread::sleep(*delay);
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }

    // The retry table is non-empty by construction, but keep this branch
    // explicit so a future change cannot turn an exhausted retry into a
    // successful empty read.
    unreachable!("cat-file retry table must contain at least one delay")
}

fn is_transient_config_access_error(error: &GitAiError) -> bool {
    matches!(
        error,
        GitAiError::GitCliError {
            code: Some(128),
            stderr,
            ..
        } if stderr.contains("unable to access '.git/config': Permission denied")
            && stderr.contains("unknown error occurred while reading the configuration files")
    )
}

fn parse_batch_output(data: &[u8]) -> Result<HashMap<String, String>, GitAiError> {
    let mut results = HashMap::new();
    let mut pos = 0usize;

    while pos < data.len() {
        let header_end = match data[pos..].iter().position(|&b| b == b'\n') {
            Some(idx) => pos + idx,
            None => break,
        };

        let header = std::str::from_utf8(&data[pos..header_end])?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() < 2 {
            pos = header_end + 1;
            continue;
        }

        let oid = parts[0].to_string();
        if parts[1] == "missing" {
            pos = header_end + 1;
            continue;
        }

        if parts.len() < 3 {
            pos = header_end + 1;
            continue;
        }

        let size: usize = parts[2]
            .parse()
            .map_err(|e| GitAiError::Generic(format!("Invalid size in cat-file output: {}", e)))?;

        let content_start = header_end + 1;
        let content_end = content_start + size;
        if content_end > data.len() {
            return Err(GitAiError::Generic(
                "Malformed cat-file --batch output: truncated content".to_string(),
            ));
        }

        let content = String::from_utf8_lossy(&data[content_start..content_end]).to_string();
        results.insert(oid, content);

        pos = content_end;
        if pos < data.len() && data[pos] == b'\n' {
            pos += 1;
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::git::test_utils::TmpRepo;

    fn transient_config_access_error() -> GitAiError {
        GitAiError::GitCliError {
            code: Some(128),
            stderr: "warning: unable to access '.git/config': Permission denied\nfatal: unknown error occurred while reading the configuration files".to_string(),
            args: vec!["git".to_string(), "cat-file".to_string(), "--batch".to_string()],
        }
    }

    #[test]
    fn parses_empty_output() {
        assert!(parse_batch_output(b"").unwrap().is_empty());
    }

    #[test]
    fn skips_missing_objects() {
        assert!(parse_batch_output(b"abc123 missing\n").unwrap().is_empty());
    }

    #[test]
    fn parses_single_blob() {
        let result = parse_batch_output(b"abc123 blob 11\nhello world\n").unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("abc123"), Some(&"hello world".to_string()));
    }

    #[test]
    fn parses_multiple_blobs() {
        let data = b"abc123 blob 5\nhello\ndef456 blob 5\nworld\n";
        let result = parse_batch_output(data).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result.get("abc123"), Some(&"hello".to_string()));
        assert_eq!(result.get("def456"), Some(&"world".to_string()));
    }

    #[test]
    fn preserves_embedded_newlines() {
        let result = parse_batch_output(b"abc123 blob 12\nhello\nworld\n\n").unwrap();

        assert_eq!(result.get("abc123"), Some(&"hello\nworld\n".to_string()));
    }

    #[test]
    fn duplicate_oid_keeps_last_record() {
        let data = b"abc123 blob 3\none\nabc123 blob 3\ntwo\n";
        let result = parse_batch_output(data).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("abc123"), Some(&"two".to_string()));
    }

    #[test]
    fn decodes_blob_content_lossily() {
        let result = parse_batch_output(b"abc123 blob 2\n\xffx\n").unwrap();

        assert_eq!(result.get("abc123"), Some(&"\u{fffd}x".to_string()));
    }

    #[test]
    fn preserves_truncated_content_error() {
        let error = parse_batch_output(b"abc123 blob 20\nhello").unwrap_err();

        assert_eq!(
            error.to_string(),
            "Generic error: Malformed cat-file --batch output: truncated content"
        );
    }

    #[test]
    fn rejects_invalid_size() {
        let error = parse_batch_output(b"abc123 blob notanumber\n").unwrap_err();

        assert_eq!(
            error.to_string(),
            "Generic error: Invalid size in cat-file output: invalid digit found in string"
        );
    }

    #[test]
    fn preserves_unvalidated_object_type_and_oid() {
        let result = parse_batch_output(b"not-an-oid tree 3\none\n").unwrap();

        assert_eq!(result.get("not-an-oid"), Some(&"one".to_string()));
    }

    #[test]
    fn skips_malformed_header() {
        assert!(parse_batch_output(b"abc123\n").unwrap().is_empty());
    }

    #[test]
    fn skips_header_without_object_size() {
        assert!(parse_batch_output(b"abc123 blob\n").unwrap().is_empty());
    }

    #[test]
    fn ignores_trailing_header_without_newline() {
        assert!(parse_batch_output(b"abc123 blob 3").unwrap().is_empty());
    }

    #[test]
    fn rejects_non_utf8_header() {
        let error = parse_batch_output(b"\xff blob 1\nx\n").unwrap_err();

        assert!(matches!(error, GitAiError::Utf8Error(_)));
    }

    #[test]
    fn empty_read_does_not_require_objects() {
        let repo = TmpRepo::new().unwrap();

        assert!(
            batch_read_blob_contents(repo.gitai_repo(), &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reads_duplicate_real_git_objects() {
        let repo = TmpRepo::new().unwrap();
        repo.write_file("blob.txt", "hello\nworld\n", false)
            .unwrap();
        let oid = repo
            .git_command(&["hash-object", "-w", "blob.txt"])
            .unwrap()
            .trim()
            .to_string();

        let result =
            batch_read_blob_contents(repo.gitai_repo(), &[oid.clone(), oid.clone()]).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.get(&oid), Some(&"hello\nworld\n".to_string()));
    }

    #[test]
    fn preserves_missing_object_error() {
        let repo = TmpRepo::new().unwrap();
        let missing_oid = "0000000000000000000000000000000000000000".to_string();

        let error = batch_read_blob_contents(repo.gitai_repo(), std::slice::from_ref(&missing_oid))
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "Generic error: missing git blob object referenced by authorship note: {missing_oid}"
            )
        );
    }

    #[test]
    fn identifies_transient_config_access_error() {
        assert!(is_transient_config_access_error(
            &transient_config_access_error()
        ));
    }

    #[test]
    fn does_not_retry_unrelated_git_cli_errors() {
        let error = GitAiError::GitCliError {
            code: Some(128),
            stderr: "fatal: not a git repository".to_string(),
            args: vec![
                "git".to_string(),
                "cat-file".to_string(),
                "--batch".to_string(),
            ],
        };

        assert!(!is_transient_config_access_error(&error));
    }

    #[test]
    fn retries_transient_config_access_before_success() {
        let mut attempts = 0;

        let result = with_config_access_retry(|| {
            attempts += 1;
            if attempts < 3 {
                Err(transient_config_access_error())
            } else {
                Ok(())
            }
        });

        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }
}
