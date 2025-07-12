use crate::error::GitAiError;
use git2::Repository;

pub fn pre_commit(repo: &Repository, default_user_name: String) -> Result<(), GitAiError> {
    // Run checkpoint as human editor.
    let result =
        crate::commands::checkpoint::run(repo, &default_user_name, false, false, true, None, None);
    result.map(|_| ())
}
