use crate::error::GitAiError;
use crate::git::find_repository;
use crate::git::notes_api::{CommitAuthorship, filter_commits_with_notes};
use crate::git::repository::{CommitRange, Repository};

const NO_AUTHORSHIP_DATA_MESSAGE: &str = "No authorship data found for this revision";

pub fn handle_show(args: &[String]) {
    if args.is_empty() {
        eprintln!("Error: show requires a revision or range");
        std::process::exit(1);
    }

    if args.len() > 1 {
        eprintln!("Error: show accepts exactly one revision or range");
        std::process::exit(1);
    }

    let repo = match find_repository(&Vec::<String>::new()) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("Failed to find repository: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = show_authorship(&repo, &args[0]) {
        eprintln!("Failed to show authorship: {}", e);
        std::process::exit(1);
    }
}

fn show_authorship(repo: &Repository, spec: &str) -> Result<(), GitAiError> {
    let commits = resolve_commits(repo, spec)?;
    if commits.is_empty() {
        println!("{}", NO_AUTHORSHIP_DATA_MESSAGE);
        return Ok(());
    }

    let entries = filter_commits_with_notes(repo, &commits)?;

    let multiple_commits = entries.len() > 1;
    for (index, entry) in entries.iter().enumerate() {
        if multiple_commits && index > 0 {
            println!();
        }

        match entry {
            CommitAuthorship::Log {
                sha,
                authorship_log,
                ..
            } => {
                if multiple_commits {
                    println!("{}", sha);
                }
                let serialized = authorship_log.serialize_to_string().map_err(|_| {
                    GitAiError::Generic("Failed to serialize authorship log".to_string())
                })?;
                println!("{}", serialized);
            }
            CommitAuthorship::NoLog { sha, .. } => {
                if multiple_commits {
                    println!("{}", sha);
                }
                println!("{}", NO_AUTHORSHIP_DATA_MESSAGE);
            }
        }
    }

    Ok(())
}

fn resolve_commits(repo: &Repository, spec: &str) -> Result<Vec<String>, GitAiError> {
    if let Some((start, end)) = spec.split_once("..") {
        if start.is_empty() || end.is_empty() {
            return Err(GitAiError::Generic(
                "Invalid commit range format. Expected <start>..<end>".to_string(),
            ));
        }

        let range = CommitRange::new_infer_refname(repo, start.to_string(), end.to_string(), None)?;

        let mut commits: Vec<String> = range.into_iter().map(|commit| commit.id()).collect();

        if commits.is_empty() {
            let end_commit = repo.revparse_single(end)?;
            commits.push(end_commit.id());
        }

        Ok(commits)
    } else {
        let commit = repo.revparse_single(spec)?;
        Ok(vec![commit.id()])
    }
}
