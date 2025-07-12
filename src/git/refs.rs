use crate::error::GitAiError;
use crate::log_fmt::authorship_log::AuthorshipLog;
use crate::log_fmt::working_log::Checkpoint;
use git2::Repository;
use serde_json;
use std::fs;

pub const AI_AUTHORSHIP_REFSPEC: &str = "+refs/ai/authorship/*:refs/ai/authorship/*";

///
pub fn put_reference(
    repo: &Repository,
    ref_name: &str,
    content: &str,
    message: &str,
) -> Result<(), GitAiError> {
    // Create the AI namespace directory structure
    let git_dir = repo.path();
    let ai_refs_dir = git_dir.join("refs").join("ai");

    // Create the directory if it doesn't exist
    fs::create_dir_all(&ai_refs_dir)?;

    // Create the blob object
    let oid = repo.blob(content.as_bytes())?;

    // Create the reference
    let full_ref_name = format!("refs/{}", ref_name);
    repo.reference(&full_ref_name, oid, true, message)?;

    Ok(())
}

pub fn get_reference(repo: &Repository, ref_name: &str) -> Result<String, GitAiError> {
    let full_ref_name = format!("refs/{}", ref_name);

    // Get the reference
    let reference = repo.find_reference(&full_ref_name)?;

    // Get the object that the reference points to
    let object = reference.peel_to_blob()?;

    // Convert the blob content to a string, handling invalid UTF-8 gracefully
    let content = String::from_utf8_lossy(object.content());

    Ok(content.to_string())
}

#[allow(dead_code)]
pub fn get_reference_as_working_log(
    repo: &Repository,
    ref_name: &str,
) -> Result<Vec<Checkpoint>, GitAiError> {
    let content = get_reference(repo, ref_name)?;
    let working_log = serde_json::from_str(&content)?;
    Ok(working_log)
}

pub fn get_reference_as_authorship_log(
    repo: &Repository,
    ref_name: &str,
) -> Result<AuthorshipLog, GitAiError> {
    let content = get_reference(repo, ref_name)?;
    let authorship_log = serde_json::from_str(&content)?;
    Ok(authorship_log)
}
