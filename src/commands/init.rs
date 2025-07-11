// hi from AI
use crate::error::GitAiError;
use crate::git::refs::setup_git_hooks;
use git2::Repository;

pub fn run(repo: &Repository) -> Result<(), GitAiError> {
    // Only set up git hooks - refspecs will be handled by the proxy
    setup_git_hooks(repo)?;
    println!("git-ai initialized successfully!");
    println!("You can now use git-ai as a git proxy:");
    println!("  git-ai pull                    # git pull");
    println!("  git-ai commit -m 'message'     # git commit with AI tracking");
    println!("  git-ai checkpoint              # create checkpoint");
    Ok(())
}
