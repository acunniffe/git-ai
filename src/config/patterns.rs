use std::path::Path;

use super::Config;

use glob::Pattern;
use serde::Serializer;

pub(crate) fn serialize_patterns<S>(patterns: &[Pattern], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::Serialize;
    let as_strings: Vec<&str> = patterns.iter().map(Pattern::as_str).collect();
    as_strings.serialize(serializer)
}

/// Match a repository root path against allow/exclude patterns.
/// Both the root and the pattern are POSIX-normalized. A pattern matches when
/// it equals the root, glob-matches it, or names a parent directory of it
/// (an entry `/work/repos` matches every repository beneath `/work/repos`).
pub(crate) fn repo_root_matches_patterns(patterns: &[Pattern], repo_root: &Path) -> bool {
    let root =
        crate::operations::git::path_format::normalize_to_posix(&repo_root.to_string_lossy());
    let root = root.trim_end_matches('/');
    patterns.iter().any(|pattern| {
        let normalized = crate::operations::git::path_format::normalize_to_posix(pattern.as_str());
        let pattern_str = normalized.trim_end_matches('/');
        pattern_str == root
            || Pattern::new(pattern_str).is_ok_and(|glob| glob.matches(root))
            || Pattern::new(&format!("{}/**", pattern_str)).is_ok_and(|glob| glob.matches(root))
    })
}

impl Config {
    /// Delivery-time recheck for queued telemetry from a repository known
    /// only by its remote URL. True when the repository is definitely no
    /// longer collection-eligible: opt-in fully revoked (empty allowlist) or
    /// the remote now excluded. Path-based allowlist entries cannot be
    /// re-verified from a remote alone, so an unlisted or missing remote
    /// keeps its ingestion-time decision.
    pub(crate) fn revokes_queued_remote_collection(&self, remote_url: Option<&str>) -> bool {
        if self.allowed_repositories.is_empty() {
            return true;
        }
        remote_url.is_some_and(|url| remote_matches_patterns(&self.exclude_repositories, url))
    }
}

pub(crate) fn remote_matches_patterns(patterns: &[Pattern], remote_url: &str) -> bool {
    let remote_candidates = repo_remote_match_candidates(remote_url);
    patterns.iter().any(|pattern| {
        repo_pattern_match_candidates(pattern.as_str())
            .iter()
            .filter_map(|candidate| Pattern::new(candidate).ok())
            .any(|candidate_pattern| {
                remote_candidates
                    .iter()
                    .any(|candidate| candidate_pattern.matches(candidate))
            })
    })
}

pub(crate) fn repo_pattern_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

pub(crate) fn repo_remote_match_candidates(value: &str) -> Vec<String> {
    let mut candidates = vec![value.trim().to_string()];

    if let Some((host, path_variants)) = repo_match_parts(value) {
        for path in path_variants {
            candidates.push(format!("{}/{}", host, path));
            candidates.push(path);
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

pub(crate) fn repo_match_parts(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();

    if let Some((_, rest)) = value.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        return Some((
            normalize_repo_authority(authority)?,
            normalize_repo_path_variants(path)?,
        ));
    }

    let (user_host, path) = value.split_once(':')?;
    if value.starts_with('/') || !user_host.contains('@') || path.is_empty() {
        return None;
    }

    let (_, host) = user_host.rsplit_once('@')?;
    Some((
        normalize_repo_host(host)?,
        normalize_repo_path_variants(path)?,
    ))
}

pub(crate) fn normalize_repo_authority(authority: &str) -> Option<String> {
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    normalize_repo_host(host)
}

fn normalize_repo_host(host: &str) -> Option<String> {
    let host = strip_repo_host_port(host.trim());
    if host.is_empty() {
        return None;
    }

    let host = host.to_ascii_lowercase();
    if matches!(host.as_str(), "dev.azure.com" | "ssh.dev.azure.com") {
        Some("azure".to_string())
    } else {
        Some(host)
    }
}

fn strip_repo_host_port(host: &str) -> &str {
    if let Some(stripped) = strip_bracketed_host_port(host) {
        return stripped;
    }

    let Some((host_without_port, port)) = host.rsplit_once(':') else {
        return host;
    };
    if host_without_port.contains(':') || port.is_empty() {
        host
    } else {
        host_without_port
    }
}

fn strip_bracketed_host_port(host: &str) -> Option<&str> {
    let rest = host.strip_prefix('[')?;
    let bracket_index = rest.find(']')?;
    let bracket_end = bracket_index + 2;
    let after_bracket = host.get(bracket_end..)?;

    if after_bracket.is_empty() || after_bracket.starts_with(':') {
        Some(&host[..bracket_end])
    } else {
        None
    }
}

pub(crate) fn normalize_repo_path_variants(path: &str) -> Option<Vec<String>> {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_start_matches('/')
        .trim_end_matches('/')
        .trim_end_matches(".git");

    if path.is_empty() {
        return None;
    }

    let mut variants = vec![path.to_string()];
    let segments: Vec<&str> = path.split('/').collect();
    if segments.first() == Some(&"v3") && segments.len() > 1 {
        variants.push(segments[1..].join("/"));
    }
    if let Some(git_segment_index) = segments.iter().position(|segment| *segment == "_git")
        && git_segment_index > 0
        && git_segment_index + 1 < segments.len()
    {
        let mut without_git_segment = segments.clone();
        without_git_segment.remove(git_segment_index);
        variants.push(without_git_segment.join("/"));
    }

    variants.sort();
    variants.dedup();
    Some(variants)
}
