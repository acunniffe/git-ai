use crate::ci::ci_context::{CiContext, CiEvent};
use crate::error::GitAiError;
use crate::git::repository::exec_git;
use crate::git::repository::find_repository_in_path;
use chrono::{Duration, Utc};
use serde::Deserialize;
use std::path::PathBuf;

const GITLAB_CI_TEMPLATE_YAML: &str = include_str!("workflow_templates/gitlab.yaml");

/// GitLab Merge Request from API response (list endpoint)
#[derive(Debug, Clone, Deserialize)]
struct GitLabMergeRequest {
    iid: u64,
    title: Option<String>,
    source_branch: String,
    target_branch: String,
    sha: String,
    merge_commit_sha: Option<String>,
    squash_commit_sha: Option<String>,
    squash: Option<bool>,
    source_project_id: u64,
    target_project_id: u64,
}

/// GitLab Project API response (minimal fields for fork detection)
#[derive(Debug, Clone, Deserialize)]
struct GitLabProject {
    http_url_to_repo: String,
}

/// Subset of the single-MR endpoint we need. The list endpoint we already
/// hit does NOT include `diff_refs`; this struct deserializes the single-MR
/// response so we can pull the right SHA out for `CiEvent::Merge.base_sha`.
///
/// GitLab notes that `diff_refs` "is empty when the merge request is created,
/// and populates asynchronously," hence the outer `Option`.
#[derive(Debug, Clone, Deserialize)]
struct GitLabMergeRequestDetails {
    diff_refs: Option<GitLabDiffRefs>,
}

/// The `diff_refs` object has three SHAs with subtly different semantics
/// (paraphrased from <https://docs.gitlab.com/api/merge_requests/>):
///
/// - `base_sha`: the **merge-base** of the source and target branches —
///   `git merge-base source target` — the historical fork point. For a
///   long-lived branch on a fast-moving target this is far behind the
///   current target tip.
/// - `start_sha`: the **target branch commit used as the starting point for
///   the diff**. Documented as "usually the same as `base_sha`," but in
///   practice it tracks the target tip at diff render time, not the
///   common ancestor. **This is the semantic match for GitHub's
///   `pull_request.base.sha`** — i.e. "the target tip the MR was opened
///   against."
/// - `head_sha`: the source branch tip. We already get this from `mr.sha`
///   on the list endpoint, so we don't deserialize it here.
///
/// The #1473 retain filter in `CiContext::run_with_options` computes
/// `base_sha..merge_commit_sha` to find commits the MR introduced. For a
/// squash-on-linear-main scenario with `main = B0→B1→B2→B3` and squash
/// commit `S`, the filter needs a range that yields exactly `{S}`. Using
/// `base_sha` (the merge-base `B0`) gives `{B1,B2,B3,S}` and lets the walk
/// match the PR commit count again — recreating the original #1473 bug.
/// Using `start_sha` (the target tip `B3`) gives `{S}` and squash is
/// correctly detected.
///
/// So: we prefer `start_sha`; `base_sha` is a fallback for the edge cases
/// where GitLab returns the former as null but the latter populated.
#[derive(Debug, Clone, Deserialize)]
struct GitLabDiffRefs {
    base_sha: Option<String>,
    start_sha: Option<String>,
}

/// Build and send an authenticated GET to a GitLab REST endpoint.
///
/// Every GitLab API call in this module shares the same shape: 30s timeout,
/// `User-Agent: git-ai/<version>`, one of `PRIVATE-TOKEN` / `JOB-TOKEN`.
/// Centralizing it here keeps the call sites focused on what they're after
/// (URL + parsing) rather than re-stating transport boilerplate.
fn gitlab_api_get(
    endpoint: &str,
    auth_header_name: &str,
    auth_token: &str,
) -> Result<crate::http::Response, String> {
    let agent = crate::http::build_agent(Some(30));
    let request = agent
        .get(endpoint)
        .header(auth_header_name, auth_token)
        .header(
            "User-Agent",
            &format!("git-ai/{}", env!("CARGO_PKG_VERSION")),
        );
    crate::http::send(request)
}

/// Fetch the SHA we want to feed into `CiEvent::Merge.base_sha` (the
/// target-branch starting point of the MR), preferring `diff_refs.start_sha`
/// over `diff_refs.base_sha`. See [`GitLabDiffRefs`] for why.
///
/// Returns `None` whenever anything goes wrong (transport error, non-200,
/// malformed body, missing `diff_refs`, both SHAs null); callers fall back to
/// the legacy empty-string behavior, which keeps GitLab safe but unprotected
/// from the #1473 misclassification for that one MR.
fn fetch_mr_base_sha(
    api_url: &str,
    auth_header_name: &str,
    auth_token: &str,
    project_id: &str,
    iid: u64,
) -> Option<String> {
    let endpoint = format!("{}/projects/{}/merge_requests/{}", api_url, project_id, iid);
    let resp = match gitlab_api_get(&endpoint, auth_header_name, auth_token) {
        Ok(resp) if resp.status_code == 200 => resp,
        _ => return None,
    };
    let body = String::from_utf8_lossy(resp.as_bytes());
    let diff_refs = serde_json::from_str::<GitLabMergeRequestDetails>(&body)
        .ok()?
        .diff_refs?;

    // Prefer start_sha (target tip at diff render = GitHub's pull_request.base.sha).
    // Fall back to base_sha (merge-base) only if start_sha is null/missing; that
    // produces a wider range that weakens but does not invert the retain filter.
    if let Some(sha) = diff_refs.start_sha {
        Some(sha)
    } else if let Some(sha) = diff_refs.base_sha {
        println!(
            "[GitLab CI] Note: diff_refs.start_sha missing for MR !{}; \
             using diff_refs.base_sha (merge-base) as fallback. \
             The #1473 retain filter may be weakened for this MR.",
            iid
        );
        Some(sha)
    } else {
        None
    }
}

/// Query GitLab API for recently merged MRs and find one matching the current commit SHA.
/// Returns None if no matching MR is found (this is not an error - just means this commit
/// wasn't from a merged MR).
pub fn get_gitlab_ci_context() -> Result<Option<CiContext>, GitAiError> {
    // Read required environment variables
    let api_url = std::env::var("CI_API_V4_URL").map_err(|_| {
        GitAiError::Generic("CI_API_V4_URL environment variable not set".to_string())
    })?;
    let project_id = std::env::var("CI_PROJECT_ID").map_err(|_| {
        GitAiError::Generic("CI_PROJECT_ID environment variable not set".to_string())
    })?;
    let commit_sha = std::env::var("CI_COMMIT_SHA").map_err(|_| {
        GitAiError::Generic("CI_COMMIT_SHA environment variable not set".to_string())
    })?;
    let server_url = std::env::var("CI_SERVER_URL").map_err(|_| {
        GitAiError::Generic("CI_SERVER_URL environment variable not set".to_string())
    })?;
    let project_path = std::env::var("CI_PROJECT_PATH").map_err(|_| {
        GitAiError::Generic("CI_PROJECT_PATH environment variable not set".to_string())
    })?;

    println!("[GitLab CI] Environment:");
    println!("  CI_COMMIT_SHA: {}", commit_sha);
    println!("  CI_PROJECT_ID: {}", project_id);
    println!("  CI_PROJECT_PATH: {}", project_path);

    // Get auth token - prefer GITLAB_TOKEN (explicitly configured with proper permissions),
    // fall back to CI_JOB_TOKEN (auto-provided but may lack API permissions)
    let (auth_header_name, auth_token) = if let Ok(gitlab_token) = std::env::var("GITLAB_TOKEN") {
        println!("  Auth: GITLAB_TOKEN");
        ("PRIVATE-TOKEN", gitlab_token)
    } else if let Ok(job_token) = std::env::var("CI_JOB_TOKEN") {
        println!("  Auth: CI_JOB_TOKEN");
        ("JOB-TOKEN", job_token)
    } else {
        return Err(GitAiError::Generic(
            "Neither GITLAB_TOKEN nor CI_JOB_TOKEN environment variable is set".to_string(),
        ));
    };

    // Calculate cutoff time (10 minutes ago) with safety buffer
    let lookback_minutes = std::env::var("GIT_AI_CI_LOOKBACK_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let cutoff = Utc::now() - Duration::minutes(lookback_minutes);

    let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Query GitLab API for recently merged MRs
    let endpoint = format!(
        "{}/projects/{}/merge_requests?state=merged&updated_after={}&order_by=updated_at&sort=desc&per_page=100",
        api_url, project_id, cutoff_str
    );

    println!("[GitLab CI] Querying API: {}", endpoint);

    let response = gitlab_api_get(&endpoint, auth_header_name, &auth_token)
        .map_err(|e| GitAiError::Generic(format!("GitLab API request failed: {}", e)))?;

    if response.status_code != 200 {
        return Err(GitAiError::Generic(format!(
            "GitLab API returned status {}: {}",
            response.status_code,
            response.as_str().unwrap_or("unknown error")
        )));
    }

    let merge_requests: Vec<GitLabMergeRequest> =
        serde_json::from_str(response.as_str().unwrap_or("[]")).map_err(|e| {
            GitAiError::Generic(format!("Failed to parse GitLab API response: {}", e))
        })?;

    println!(
        "[GitLab CI] Found {} recently merged MRs",
        merge_requests.len()
    );

    // Log details of each MR for debugging
    for mr in &merge_requests {
        println!(
            "[GitLab CI] MR !{}: \"{}\"",
            mr.iid,
            mr.title.as_deref().unwrap_or("(no title)")
        );
        println!("    source_branch: {}", mr.source_branch);
        println!("    target_branch: {}", mr.target_branch);
        println!("    sha (head): {}", mr.sha);
        println!(
            "    merge_commit_sha: {}",
            mr.merge_commit_sha.as_deref().unwrap_or("(none)")
        );
        println!(
            "    squash_commit_sha: {}",
            mr.squash_commit_sha.as_deref().unwrap_or("(none)")
        );
        println!("    squash: {:?}", mr.squash);

        // Check which SHA matches
        let merge_matches = mr.merge_commit_sha.as_ref() == Some(&commit_sha);
        let squash_matches = mr.squash_commit_sha.as_ref() == Some(&commit_sha);
        println!(
            "    matches CI_COMMIT_SHA? merge_commit={}, squash_commit={}",
            merge_matches, squash_matches
        );
    }

    // Find MR where merge_commit_sha OR squash_commit_sha matches our commit
    let matching_mr = merge_requests.into_iter().find(|mr| {
        mr.merge_commit_sha.as_ref() == Some(&commit_sha)
            || mr.squash_commit_sha.as_ref() == Some(&commit_sha)
    });

    let mr = match matching_mr {
        Some(mr) => {
            println!("[GitLab CI] Found matching MR !{}", mr.iid);
            mr
        }
        None => {
            println!("[GitLab CI] No recent MR found corresponding to this commit. Skipping...");
            return Ok(None);
        }
    };

    // Determine which commit SHA to use as the "merge commit" for rewriting
    // If this was a squash merge, CI_COMMIT_SHA might be the squash commit
    // (which is what we want to rewrite authorship TO)
    let effective_merge_sha = if mr.squash_commit_sha.as_ref() == Some(&commit_sha) {
        println!("[GitLab CI] CI_COMMIT_SHA matches squash_commit_sha - this is a squash merge");
        commit_sha.clone()
    } else {
        println!(
            "[GitLab CI] CI_COMMIT_SHA matches merge_commit_sha - checking if this is a squash+merge"
        );
        // If squash was used but we matched on merge_commit_sha,
        // the actual squash commit is in squash_commit_sha
        if let Some(squash_sha) = &mr.squash_commit_sha {
            println!(
                "[GitLab CI] MR has squash_commit_sha={}, will use that for rewriting",
                squash_sha
            );
            squash_sha.clone()
        } else {
            commit_sha.clone()
        }
    };

    println!(
        "[GitLab CI] Effective merge/squash SHA for rewriting: {}",
        effective_merge_sha
    );

    // Detect fork: if source_project_id differs from target_project_id, this is a fork MR
    let fork_clone_url = if mr.source_project_id != mr.target_project_id {
        println!(
            "[GitLab CI] Detected fork MR: source project {} differs from target project {}",
            mr.source_project_id, mr.target_project_id
        );
        // Query the source project API to get its clone URL.
        // Use the existing ureq-based HTTP wrapper to match the rest of this file
        // (avoids pulling in the minreq crate the original PR used).
        let source_project_endpoint = format!("{}/projects/{}", api_url, mr.source_project_id);
        let agent = crate::http::build_agent(Some(30));
        let request = agent
            .get(&source_project_endpoint)
            .header(auth_header_name, &auth_token)
            .header(
                "User-Agent",
                &format!("git-ai/{}", env!("CARGO_PKG_VERSION")),
            );
        match crate::http::send(request) {
            Ok(resp) if resp.status_code == 200 => {
                let body = String::from_utf8_lossy(resp.as_bytes());
                match serde_json::from_str::<GitLabProject>(&body) {
                    Ok(project) => {
                        println!("[GitLab CI] Fork clone URL: {}", project.http_url_to_repo);
                        Some(project.http_url_to_repo)
                    }
                    Err(e) => {
                        println!(
                            "[GitLab CI] Warning: Failed to parse source project response: {}",
                            e
                        );
                        None
                    }
                }
            }
            Ok(resp) => {
                println!(
                    "[GitLab CI] Warning: Failed to query source project (status {}), fork notes may be lost",
                    resp.status_code
                );
                None
            }
            Err(e) => {
                println!(
                    "[GitLab CI] Warning: Failed to query source project: {}, fork notes may be lost",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Found a matching MR - clone and fetch
    let clone_dir = "git-ai-ci-clone".to_string();
    let clone_url = format!("{}/{}.git", server_url, project_path);

    // Build authenticated URLs:
    // - clone_auth_url: Use CI_JOB_TOKEN for clone/fetch (read-only is fine)
    // - push_auth_url: Use GITLAB_TOKEN for push (needs write_repository scope)
    let scheme = if server_url.starts_with("https") {
        "https"
    } else {
        "http"
    };
    let server_host = server_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    // Clone URL uses CI_JOB_TOKEN (available by default, read-only)
    let clone_auth_url = if let Ok(job_token) = std::env::var("CI_JOB_TOKEN") {
        println!("[GitLab CI] Using CI_JOB_TOKEN for clone/fetch");
        clone_url.replace(
            &server_url,
            &format!("{}://gitlab-ci-token:{}@{}", scheme, job_token, server_host),
        )
    } else {
        println!("[GitLab CI] Warning: CI_JOB_TOKEN not available, clone may fail");
        clone_url.clone()
    };

    // Push URL uses GITLAB_TOKEN (needs write_repository scope)
    let push_auth_url = if let Ok(gitlab_token) = std::env::var("GITLAB_TOKEN") {
        println!("[GitLab CI] Using GITLAB_TOKEN for push (write_repository scope)");
        clone_url.replace(
            &server_url,
            &format!("{}://oauth2:{}@{}", scheme, gitlab_token, server_host),
        )
    } else {
        println!("[GitLab CI] Warning: GITLAB_TOKEN not set - push will likely fail");
        println!("[GitLab CI] Create a Project Access Token with write_repository scope");
        clone_auth_url.clone()
    };

    // Clone the repo using CI_JOB_TOKEN
    println!("[GitLab CI] Cloning repository...");
    exec_git(&[
        "clone".to_string(),
        "--branch".to_string(),
        mr.target_branch.clone(),
        clone_auth_url.clone(),
        clone_dir.clone(),
    ])?;

    // Set origin URL to GITLAB_TOKEN URL for push
    println!("[GitLab CI] Setting origin URL for push...");
    exec_git(&[
        "-C".to_string(),
        clone_dir.clone(),
        "remote".to_string(),
        "set-url".to_string(),
        "origin".to_string(),
        push_auth_url,
    ])?;

    // Fetch MR commits using GitLab's special MR refs
    // This is necessary because the MR branch may be deleted after merge
    // but GitLab keeps the commits accessible via refs/merge-requests/{iid}/head
    println!(
        "[GitLab CI] Fetching MR commits from refs/merge-requests/{}/head...",
        mr.iid
    );
    exec_git(&[
        "-C".to_string(),
        clone_dir.clone(),
        "fetch".to_string(),
        clone_auth_url,
        format!(
            "refs/merge-requests/{}/head:refs/gitlab/mr/{}",
            mr.iid, mr.iid
        ),
    ])?;

    let repo = find_repository_in_path(&clone_dir)?;

    // Fetch diff_refs.base_sha from the single-MR endpoint. The list endpoint
    // we hit earlier doesn't include diff_refs; without base_sha the #1473
    // retain filter in CiContext::run_with_options skips, so squash merges on
    // a linear target branch can still be misclassified as rebases. None here
    // -> fall back to empty string (legacy behavior, no protection).
    let base_sha = fetch_mr_base_sha(&api_url, auth_header_name, &auth_token, &project_id, mr.iid)
        .unwrap_or_else(|| {
            println!(
                "[GitLab CI] Warning: could not fetch diff_refs.base_sha for MR !{}; \
                     proceeding without the #1473 retain filter (legacy behavior)",
                mr.iid
            );
            String::new()
        });

    println!(
        "[GitLab CI] Created CiContext: merge_commit_sha={}, head_sha={}, head_ref={}, base_ref={}, base_sha={}",
        effective_merge_sha,
        mr.sha,
        mr.source_branch,
        mr.target_branch,
        if base_sha.is_empty() {
            "(unavailable)"
        } else {
            &base_sha
        }
    );

    // Authenticate the fork clone URL for fetching notes
    let authenticated_fork_url = fork_clone_url.map(|fork_url| {
        if let Ok(job_token) = std::env::var("CI_JOB_TOKEN") {
            fork_url.replace(
                &server_url,
                &format!("{}://gitlab-ci-token:{}@{}", scheme, job_token, server_host),
            )
        } else {
            fork_url
        }
    });

    Ok(Some(CiContext {
        repo,
        event: CiEvent::Merge {
            merge_commit_sha: effective_merge_sha,
            head_ref: mr.source_branch.clone(),
            head_sha: mr.sha.clone(),
            base_ref: mr.target_branch.clone(),
            base_sha,
            fork_clone_url: authenticated_fork_url,
        },
        temp_dir: PathBuf::from(clone_dir),
    }))
}

/// Print the GitLab CI YAML snippet to stdout for users to copy into their .gitlab-ci.yml
pub fn print_gitlab_ci_yaml() {
    println!("Add the following to your .gitlab-ci.yml:");
    println!();
    println!("{}", GITLAB_CI_TEMPLATE_YAML);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gitlab_merge_request_deserialization() {
        let json = r#"{
            "iid": 42,
            "title": "Fix bug",
            "source_branch": "feature/fix",
            "target_branch": "main",
            "sha": "abc123",
            "merge_commit_sha": "def456",
            "squash_commit_sha": null,
            "squash": false,
            "source_project_id": 123,
            "target_project_id": 456
        }"#;
        let mr: GitLabMergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 42);
        assert_eq!(mr.title, Some("Fix bug".to_string()));
        assert_eq!(mr.source_branch, "feature/fix");
        assert_eq!(mr.target_branch, "main");
        assert_eq!(mr.sha, "abc123");
        assert_eq!(mr.merge_commit_sha, Some("def456".to_string()));
        assert!(mr.squash_commit_sha.is_none());
        assert_eq!(mr.squash, Some(false));
        assert_eq!(mr.source_project_id, 123);
        assert_eq!(mr.target_project_id, 456);
    }

    #[test]
    fn test_gitlab_merge_request_deserialization_with_squash() {
        let json = r#"{
            "iid": 99,
            "title": "Squash merge",
            "source_branch": "feature/squash",
            "target_branch": "main",
            "sha": "head123",
            "merge_commit_sha": "merge456",
            "squash_commit_sha": "squash789",
            "squash": true,
            "source_project_id": 123,
            "target_project_id": 123
        }"#;
        let mr: GitLabMergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 99);
        assert_eq!(mr.squash_commit_sha, Some("squash789".to_string()));
        assert_eq!(mr.squash, Some(true));
        assert_eq!(mr.source_project_id, 123);
        assert_eq!(mr.target_project_id, 123);
    }

    #[test]
    fn test_gitlab_merge_request_deserialization_minimal() {
        let json = r#"{
            "iid": 1,
            "source_branch": "dev",
            "target_branch": "main",
            "sha": "abc",
            "source_project_id": 999,
            "target_project_id": 999
        }"#;
        let mr: GitLabMergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 1);
        assert!(mr.title.is_none());
        assert!(mr.merge_commit_sha.is_none());
        assert!(mr.squash_commit_sha.is_none());
        assert!(mr.squash.is_none());
        assert_eq!(mr.source_project_id, 999);
        assert_eq!(mr.target_project_id, 999);
    }

    #[test]
    fn test_gitlab_ci_template_yaml_not_empty() {
        assert!(
            !GITLAB_CI_TEMPLATE_YAML.is_empty(),
            "GitLab CI template YAML should not be empty"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_lookback_minutes_defaults_to_15() {
        unsafe { std::env::remove_var("GIT_AI_CI_LOOKBACK_MINUTES") };
        let lookback = std::env::var("GIT_AI_CI_LOOKBACK_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15i64);
        assert_eq!(lookback, 15);
    }

    #[test]
    #[serial_test::serial]
    fn test_lookback_minutes_reads_env_var() {
        unsafe { std::env::set_var("GIT_AI_CI_LOOKBACK_MINUTES", "4320") };
        let lookback = std::env::var("GIT_AI_CI_LOOKBACK_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15i64);
        unsafe { std::env::remove_var("GIT_AI_CI_LOOKBACK_MINUTES") };
        assert_eq!(lookback, 4320);
    }

    #[test]
    #[serial_test::serial]
    fn test_lookback_minutes_falls_back_on_invalid_value() {
        unsafe { std::env::set_var("GIT_AI_CI_LOOKBACK_MINUTES", "not-a-number") };
        let lookback = std::env::var("GIT_AI_CI_LOOKBACK_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15i64);
        unsafe { std::env::remove_var("GIT_AI_CI_LOOKBACK_MINUTES") };
        assert_eq!(lookback, 15);
    }

    // ---- CiEvent::Merge.base_sha derivation from diff_refs ----
    //
    // We prefer `diff_refs.start_sha` (target tip at diff render, semantically
    // equal to GitHub's `pull_request.base.sha`) over `diff_refs.base_sha`
    // (merge-base). See the `GitLabDiffRefs` docstring for why; tests below
    // pin the preference + fallback + every silently-absorbed failure mode.

    #[test]
    fn test_diff_refs_deserialization_happy() {
        let json = r#"{
            "iid": 42,
            "diff_refs": {
                "base_sha": "0000000000000000000000000000000000000000",
                "head_sha": "1111111111111111111111111111111111111111",
                "start_sha": "2222222222222222222222222222222222222222"
            }
        }"#;
        let details: GitLabMergeRequestDetails = serde_json::from_str(json).unwrap();
        let diff_refs = details.diff_refs.unwrap();
        assert_eq!(
            diff_refs.base_sha,
            Some("0000000000000000000000000000000000000000".to_string())
        );
        assert_eq!(
            diff_refs.start_sha,
            Some("2222222222222222222222222222222222222222".to_string())
        );
    }

    #[test]
    fn test_diff_refs_deserialization_missing_diff_refs() {
        // GitLab notes diff_refs "is empty when the merge request is created,
        // and populates asynchronously"; absorb that as None.
        let json = r#"{"iid": 1}"#;
        let details: GitLabMergeRequestDetails = serde_json::from_str(json).unwrap();
        assert!(details.diff_refs.is_none());
    }

    #[test]
    fn test_diff_refs_deserialization_null_shas() {
        // Both SHAs JSON-null — surface as None on both fields.
        let json = r#"{
            "iid": 7,
            "diff_refs": { "base_sha": null, "start_sha": null }
        }"#;
        let details: GitLabMergeRequestDetails = serde_json::from_str(json).unwrap();
        let diff_refs = details.diff_refs.unwrap();
        assert!(diff_refs.base_sha.is_none());
        assert!(diff_refs.start_sha.is_none());
    }

    /// Happy path: both SHAs present, we MUST pick start_sha. This is the
    /// load-bearing test — picking base_sha here recreates the original
    /// #1473 bug for GitLab squash MRs.
    #[test]
    fn test_fetch_mr_base_sha_prefers_start_sha_over_base_sha() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .match_header("PRIVATE-TOKEN", "test-token")
            .with_status(200)
            .with_header("content-type", "application/json")
            // Distinct values so the assertion can't pass by coincidence.
            .with_body(
                r#"{
                "iid": 42,
                "diff_refs": {
                    "base_sha":  "0000000000000000000000000000000000000000",
                    "start_sha": "2222222222222222222222222222222222222222",
                    "head_sha":  "1111111111111111111111111111111111111111"
                }
            }"#,
            )
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "test-token", "123", 42);

        mock.assert();
        assert_eq!(
            result,
            Some("2222222222222222222222222222222222222222".to_string()),
            "must prefer start_sha (target tip) over base_sha (merge-base)"
        );
    }

    /// Fallback: GitLab returns diff_refs with base_sha populated but
    /// start_sha null/missing. Use base_sha and continue; the retain
    /// filter is weakened but not broken.
    #[test]
    fn test_fetch_mr_base_sha_falls_back_to_base_sha_when_start_sha_missing() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .with_status(200)
            .with_body(
                r#"{
                "iid": 42,
                "diff_refs": {
                    "base_sha":  "0000000000000000000000000000000000000000",
                    "start_sha": null
                }
            }"#,
            )
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "tok", "123", 42);
        mock.assert();
        assert_eq!(
            result,
            Some("0000000000000000000000000000000000000000".to_string())
        );
    }

    #[test]
    fn test_fetch_mr_base_sha_returns_none_when_both_shas_missing() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .with_status(200)
            .with_body(
                r#"{
                "iid": 42,
                "diff_refs": { "base_sha": null, "start_sha": null }
            }"#,
            )
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "tok", "123", 42);
        mock.assert();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_mr_base_sha_404_returns_none() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .with_status(404)
            .with_body(r#"{"message": "404 Not Found"}"#)
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "tok", "123", 42);
        mock.assert();
        assert!(
            result.is_none(),
            "404 should fall through to None (caller uses empty string)"
        );
    }

    #[test]
    fn test_fetch_mr_base_sha_malformed_body_returns_none() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .with_status(200)
            .with_body("not json")
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "tok", "123", 42);
        mock.assert();
        assert!(result.is_none(), "non-JSON body should not panic");
    }

    #[test]
    fn test_fetch_mr_base_sha_missing_diff_refs_returns_none() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .with_status(200)
            .with_body(r#"{"iid": 42}"#)
            .create();

        let result = fetch_mr_base_sha(&server.url(), "PRIVATE-TOKEN", "tok", "123", 42);
        mock.assert();
        assert!(result.is_none());
    }

    #[test]
    fn test_fetch_mr_base_sha_with_job_token_header() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("GET", "/projects/123/merge_requests/42")
            .match_header("JOB-TOKEN", "ci-job-token-value")
            .with_status(200)
            // start_sha present so the happy path through JOB-TOKEN auth fires.
            .with_body(
                r#"{"diff_refs": {"start_sha": "abc1234567890abcdef1234567890abcdef12345"}}"#,
            )
            .create();

        let result = fetch_mr_base_sha(&server.url(), "JOB-TOKEN", "ci-job-token-value", "123", 42);
        mock.assert();
        assert_eq!(
            result,
            Some("abc1234567890abcdef1234567890abcdef12345".to_string())
        );
    }
}
