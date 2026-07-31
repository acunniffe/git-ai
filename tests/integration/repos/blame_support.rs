#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BlameAuthor<'a> {
    pub(crate) name: &'a str,
    pub(crate) email: Option<&'a str>,
}

pub(crate) fn parse_blame_author(author: &str) -> BlameAuthor<'_> {
    let author = author.trim();
    let Some((name, email_and_suffix)) = author.split_once('<') else {
        return BlameAuthor {
            name: author,
            email: None,
        };
    };

    BlameAuthor {
        name: name.trim(),
        email: email_and_suffix
            .split_once('>')
            .map(|(email, _)| email.trim())
            .filter(|email| !email.is_empty()),
    }
}

pub(crate) fn parse_blame_line(line: &str) -> (String, String) {
    let Some((_, author_and_content)) = line.split_once('(') else {
        return ("unknown".to_string(), line.to_string());
    };
    let Some((author_section, content)) = author_and_content.split_once(')') else {
        return ("unknown".to_string(), line.to_string());
    };

    let author = author_section
        .split_whitespace()
        .take_while(|part| !part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ");

    (author, content.trim().to_string())
}

pub(crate) fn is_ai_blame_author(author: &str) -> bool {
    const AI_AUTHOR_IDENTITIES: &[&str] = &[
        "mock_ai",
        "claude",
        "continue-cli",
        "gpt",
        "copilot",
        "cursor",
        "codex",
        "gemini",
        "amp",
        "windsurf",
        "devin",
        "cloud-agent",
        "codex-cloud",
        "git-ai-cloud-agent",
        "agent-v1",
    ];

    let name = parse_blame_author(author).name.to_lowercase();
    AI_AUTHOR_IDENTITIES
        .iter()
        .any(|identity| name.contains(identity))
}

#[cfg(test)]
mod tests {
    use super::{BlameAuthor, is_ai_blame_author, parse_blame_author, parse_blame_line};

    #[test]
    fn blame_author_support_parses_name_and_email() {
        assert_eq!(
            parse_blame_author("Jane Mary Doe <jane@example.com>"),
            BlameAuthor {
                name: "Jane Mary Doe",
                email: Some("jane@example.com"),
            }
        );
        assert_eq!(
            parse_blame_line(
                "abc123 (Jane Mary Doe <jane@example.com> 2026-07-22 12:00:00 -0400 1) content"
            ),
            (
                "Jane Mary Doe <jane@example.com>".to_string(),
                "content".to_string(),
            )
        );
    }

    #[test]
    fn blame_author_support_preserves_unknown_malformed_lines() {
        let line = "abc123 (Jane Doe 2026-07-22 12:00:00 -0400 1 content";

        assert_eq!(
            parse_blame_line(line),
            ("unknown".to_string(), line.to_string())
        );
    }

    #[test]
    fn blame_author_support_classifies_supported_ai_identities_case_insensitively() {
        for author in [
            "mock_ai [s_123]",
            "GitHub Copilot <copilot@example.com>",
            "AGENT-V1 <agent@example.com>",
            "Codex-Cloud <codex@example.com>",
            "Git-AI-Cloud-Agent <cloud@example.com>",
            "gEmInI <gemini@example.com>",
        ] {
            assert!(is_ai_blame_author(author), "{author} should be AI");
        }
    }

    #[test]
    fn blame_author_support_excludes_email_text_from_ai_classification() {
        for author in [
            "Human Developer <user@example.com>",
            "Human Developer <amp@example.com>",
            "Human Developer <codex@example.com>",
            "Human Developer <agent-v1@example.com>",
            "Human Developer",
        ] {
            assert!(
                !is_ai_blame_author(author),
                "{author} should not be classified as AI"
            );
        }
    }
}
