use crate::cli::fail::resolve_repo_or_fail;
use crate::operations::authorship::prompt_utils::find_prompt;

/// Handle the `show-prompt` command
///
/// Usage: `git-ai show-prompt <prompt_id> [--commit <rev>] [--offset <n>]`
///
/// Returns the prompt object from the authorship note where the given prompt ID is found.
/// By default returns from the most recent commit containing the prompt.
pub fn handle_show_prompt(args: &[String]) {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let repo = resolve_repo_or_fail();

    match find_prompt(
        &repo,
        &parsed.prompt_id,
        parsed.commit.as_deref(),
        parsed.offset,
    ) {
        Ok((commit_sha, prompt_record)) => {
            // Output the prompt as JSON, including the commit SHA for context
            // Note: messages will be empty if they were uploaded to CAS (legacy behavior)
            let output = serde_json::json!({
                "commit": commit_sha,
                "prompt_id": parsed.prompt_id,
                "prompt": prompt_record,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
            );
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

#[derive(Debug)]
pub struct ParsedArgs {
    pub prompt_id: String,
    pub commit: Option<String>,
    pub offset: usize,
}

pub fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut prompt_id: Option<String> = None;
    let mut commit: Option<String> = None;
    let mut offset: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if arg == "--commit" {
            if i + 1 >= args.len() {
                return Err("--commit requires a value".to_string());
            }
            i += 1;
            commit = Some(args[i].clone());
        } else if arg == "--offset" {
            if i + 1 >= args.len() {
                return Err("--offset requires a value".to_string());
            }
            i += 1;
            offset = Some(
                args[i]
                    .parse::<usize>()
                    .map_err(|_| "--offset must be a non-negative integer")?,
            );
        } else if arg.starts_with('-') {
            return Err(format!("Unknown option: {}", arg));
        } else {
            if prompt_id.is_some() {
                return Err("Only one prompt ID can be specified".to_string());
            }
            prompt_id = Some(arg.clone());
        }

        i += 1;
    }

    let prompt_id = prompt_id.ok_or("show-prompt requires a prompt ID")?;

    // Validate mutual exclusivity of --commit and --offset
    if commit.is_some() && offset.is_some() {
        return Err("--commit and --offset are mutually exclusive".to_string());
    }

    Ok(ParsedArgs {
        prompt_id,
        commit,
        offset: offset.unwrap_or(0),
    })
}
