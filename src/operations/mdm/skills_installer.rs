use crate::config::skills_dir_path;
use crate::error::GitAiError;
use crate::operations::mdm::file_ops::write_atomic;
use crate::operations::mdm::paths::claude_config_dir;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Embedded skill - each skill has a name and its SKILL.md content
struct EmbeddedSkill {
    name: &'static str,
    skill_md: &'static str,
}

/// All embedded skills - add new skills here
const EMBEDDED_SKILLS: &[EmbeddedSkill] = &[
    EmbeddedSkill {
        name: "prompt-analysis",
        skill_md: include_str!("../../../skills/prompt-analysis/SKILL.md"),
    },
    EmbeddedSkill {
        name: "git-ai-search",
        skill_md: include_str!("../../../skills/git-ai-search/SKILL.md"),
    },
    EmbeddedSkill {
        name: "ask",
        skill_md: include_str!("../../../skills/ask/SKILL.md"),
    },
];

/// Result of installing skills
pub struct SkillsInstallResult {
    /// Whether any changes were made
    pub changed: bool,
    /// Number of skills installed
    #[allow(dead_code)]
    pub installed_count: usize,
}

/// Get the ~/.agents/skills directory path
fn agents_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".agents").join("skills"))
}

fn claude_skills_dir() -> Option<PathBuf> {
    Some(claude_config_dir().join("skills"))
}

/// Get the ~/.cursor/skills directory path
fn cursor_skills_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cursor").join("skills"))
}

/// Link a skill directory to the target location.
/// On Unix, creates a symlink. On Windows, copies the directory to avoid requiring
/// Administrator privileges (which symlink creation requires on Windows).
fn link_skill_dir(target: &PathBuf, link_path: &PathBuf) -> Result<(), GitAiError> {
    // Create parent directory if needed
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Remove existing file/symlink/directory if present
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        if link_path.is_dir()
            && !link_path
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        {
            fs::remove_dir_all(link_path)?;
        } else {
            fs::remove_file(link_path)?;
        }
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link_path)?;

    #[cfg(windows)]
    copy_dir_recursive(target, link_path)?;

    Ok(())
}

/// Recursively copy a directory and its contents from src to dst.
#[cfg(windows)]
fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<(), GitAiError> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if entry_path.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Remove a skill link (symlink on Unix, copied directory on Windows) if it exists.
fn remove_skill_link(link_path: &PathBuf) -> Result<(), GitAiError> {
    if link_path.symlink_metadata().is_ok() {
        let is_symlink = link_path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_symlink {
            fs::remove_file(link_path)?;
        } else if link_path.is_dir() {
            fs::remove_dir_all(link_path)?;
        }
    }
    Ok(())
}

/// Install all embedded skills to ~/.git-ai/skills/
/// This nukes the entire skills directory and recreates it fresh each time.
///
/// Creates the standard skills structure:
/// ~/.git-ai/skills/
/// └── prompt-analysis/
///     └── SKILL.md
///
/// Then links each skill to:
/// - ~/.agents/skills/{skill-name} (symlink on Unix, copy on Windows)
/// - ~/.claude/skills/{skill-name} (symlink on Unix, copy on Windows)
pub fn install_skills(
    dry_run: bool,
    _verbose: bool,
    installed_tools: &HashSet<String>,
) -> Result<SkillsInstallResult, GitAiError> {
    let skills_base = skills_dir_path().ok_or_else(|| {
        GitAiError::Generic("Could not determine skills directory path".to_string())
    })?;

    if dry_run {
        return Ok(SkillsInstallResult {
            changed: true,
            installed_count: EMBEDDED_SKILLS.len(),
        });
    }

    // Nuke the skills directory if it exists
    if skills_base.exists() {
        fs::remove_dir_all(&skills_base)?;
    }

    // Create fresh skills directory
    fs::create_dir_all(&skills_base)?;

    // Install each skill
    for skill in EMBEDDED_SKILLS {
        // Create skill directory: ~/.git-ai/skills/{skill-name}/
        let skill_dir = skills_base.join(skill.name);
        fs::create_dir_all(&skill_dir)?;

        // Write SKILL.md
        let skill_md_path = skill_dir.join("SKILL.md");
        write_atomic(&skill_md_path, skill.skill_md.as_bytes())?;

        // Link this skill to agent directories
        // ~/.agents/skills/{skill-name} -> ~/.git-ai/skills/{skill-name}
        if let Some(agents_dir) = agents_skills_dir() {
            let agents_link = agents_dir.join(skill.name);
            if let Err(e) = link_skill_dir(&skill_dir, &agents_link) {
                eprintln!("Warning: Failed to link skill at {:?}: {}", agents_link, e);
            }
        }

        // ~/.claude/skills/{skill-name} -> ~/.git-ai/skills/{skill-name}
        if installed_tools.contains("claude-code")
            && let Some(claude_dir) = claude_skills_dir()
        {
            let claude_link = claude_dir.join(skill.name);
            if let Err(e) = link_skill_dir(&skill_dir, &claude_link) {
                eprintln!("Warning: Failed to link skill at {:?}: {}", claude_link, e);
            }
        }

        // ~/.cursor/skills/{skill-name} -> ~/.git-ai/skills/{skill-name}
        if installed_tools.contains("cursor")
            && let Some(cursor_dir) = cursor_skills_dir()
        {
            let cursor_link = cursor_dir.join(skill.name);
            if let Err(e) = link_skill_dir(&skill_dir, &cursor_link) {
                eprintln!("Warning: Failed to link skill at {:?}: {}", cursor_link, e);
            }
        }
    }

    Ok(SkillsInstallResult {
        changed: true,
        installed_count: EMBEDDED_SKILLS.len(),
    })
}

/// Uninstall all skills by removing ~/.git-ai/skills/ and linked skill directories
pub fn uninstall_skills(dry_run: bool, _verbose: bool) -> Result<SkillsInstallResult, GitAiError> {
    let skills_base = skills_dir_path().ok_or_else(|| {
        GitAiError::Generic("Could not determine skills directory path".to_string())
    })?;

    if !skills_base.exists() {
        return Ok(SkillsInstallResult {
            changed: false,
            installed_count: 0,
        });
    }

    if dry_run {
        return Ok(SkillsInstallResult {
            changed: true,
            installed_count: EMBEDDED_SKILLS.len(),
        });
    }

    // Remove linked skill directories first
    for skill in EMBEDDED_SKILLS {
        // ~/.agents/skills/{skill-name}
        if let Some(agents_dir) = agents_skills_dir() {
            let agents_link = agents_dir.join(skill.name);
            if let Err(e) = remove_skill_link(&agents_link) {
                eprintln!(
                    "Warning: Failed to remove skill link at {:?}: {}",
                    agents_link, e
                );
            }
        }

        // ~/.claude/skills/{skill-name}
        if let Some(claude_dir) = claude_skills_dir() {
            let claude_link = claude_dir.join(skill.name);
            if let Err(e) = remove_skill_link(&claude_link) {
                eprintln!(
                    "Warning: Failed to remove skill link at {:?}: {}",
                    claude_link, e
                );
            }
        }

        // ~/.cursor/skills/{skill-name}
        if let Some(cursor_dir) = cursor_skills_dir() {
            let cursor_link = cursor_dir.join(skill.name);
            if let Err(e) = remove_skill_link(&cursor_link) {
                eprintln!(
                    "Warning: Failed to remove skill link at {:?}: {}",
                    cursor_link, e
                );
            }
        }
    }

    // Nuke the entire skills directory
    fs::remove_dir_all(&skills_base)?;

    Ok(SkillsInstallResult {
        changed: true,
        installed_count: EMBEDDED_SKILLS.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::mdm::test_env::with_temp_home;
    use serial_test::serial;

    #[test]
    fn test_embedded_skills_are_loaded() {
        for skill in EMBEDDED_SKILLS {
            assert!(!skill.name.is_empty(), "Skill name should not be empty");
            assert!(
                !skill.skill_md.is_empty(),
                "Skill {} SKILL.md should not be empty",
                skill.name
            );
            assert!(
                skill.skill_md.contains("---"),
                "Skill {} should have frontmatter",
                skill.name
            );
        }
    }

    #[test]
    fn test_skills_dir_path_is_under_git_ai() {
        if let Some(path) = skills_dir_path() {
            assert!(path.ends_with("skills"));
            let parent = path.parent().unwrap();
            assert!(parent.ends_with(".git-ai"));
        }
    }

    #[test]
    fn test_link_skill_dir_creates_link_and_content_is_accessible() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "test content").unwrap();

        let link = tmp.path().join("linked-skill");
        link_skill_dir(&source, &link).unwrap();

        assert!(link.exists());
        assert!(link.join("SKILL.md").exists());
        assert_eq!(
            fs::read_to_string(link.join("SKILL.md")).unwrap(),
            "test content"
        );
    }

    #[test]
    fn test_link_skill_dir_replaces_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "new content").unwrap();

        let link = tmp.path().join("linked-skill");
        fs::create_dir_all(&link).unwrap();
        fs::write(link.join("SKILL.md"), "old content").unwrap();

        link_skill_dir(&source, &link).unwrap();

        assert_eq!(
            fs::read_to_string(link.join("SKILL.md")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn test_link_skill_dir_replaces_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "content").unwrap();

        let link = tmp.path().join("linked-skill");
        fs::write(&link, "i am a file").unwrap();

        link_skill_dir(&source, &link).unwrap();

        assert!(link.is_dir() || link.is_symlink());
        assert!(link.join("SKILL.md").exists());
    }

    #[test]
    fn test_link_skill_dir_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "content").unwrap();

        let link = tmp.path().join("deep").join("nested").join("linked-skill");
        link_skill_dir(&source, &link).unwrap();

        assert!(link.exists());
        assert!(link.join("SKILL.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_link_skill_dir_creates_symlink_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source-skill");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("SKILL.md"), "content").unwrap();

        let link = tmp.path().join("linked-skill");
        link_skill_dir(&source, &link).unwrap();

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), source);
    }

    #[test]
    fn test_remove_skill_link_removes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "content").unwrap();

        remove_skill_link(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_remove_skill_link_removes_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir_all(&target).unwrap();

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_skill_link(&link).unwrap();
        assert!(link.symlink_metadata().is_err());
        assert!(target.exists(), "original target should not be removed");
    }

    #[test]
    fn test_remove_skill_link_noop_on_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        remove_skill_link(&nonexistent).unwrap();
    }

    #[test]
    #[serial]
    fn test_install_and_uninstall_skills_lifecycle() {
        // Use an isolated temp HOME so we don't pollute the real home directory
        // and don't race with other tests that mutate HOME (e.g. codex tests).
        with_temp_home(|_home| {
            let skills_base = skills_dir_path().unwrap();
            let all_tools: HashSet<String> = ["claude-code", "cursor"]
                .iter()
                .map(|s| s.to_string())
                .collect();

            // Dry run should not create anything
            let dry_result = install_skills(true, false, &all_tools).unwrap();
            assert!(dry_result.changed);
            assert_eq!(dry_result.installed_count, EMBEDDED_SKILLS.len());

            // Install creates skill files with correct content
            let result = install_skills(false, false, &all_tools).unwrap();
            assert!(result.changed);
            assert_eq!(result.installed_count, EMBEDDED_SKILLS.len());
            assert!(skills_base.exists());
            for skill in EMBEDDED_SKILLS {
                let skill_md = skills_base.join(skill.name).join("SKILL.md");
                assert!(skill_md.exists(), "SKILL.md missing for {}", skill.name);
                let content = fs::read_to_string(&skill_md).unwrap();
                assert_eq!(content, skill.skill_md);
            }

            // Install again is idempotent
            let result2 = install_skills(false, false, &all_tools).unwrap();
            assert!(result2.changed);
            for skill in EMBEDDED_SKILLS {
                let skill_md = skills_base.join(skill.name).join("SKILL.md");
                assert!(
                    skill_md.exists(),
                    "SKILL.md missing after re-install for {}",
                    skill.name
                );
            }

            // Uninstall removes skills directory
            let uninstall_result = uninstall_skills(false, false).unwrap();
            assert!(uninstall_result.changed);
            assert!(!skills_base.exists());

            // Uninstall again is a no-op
            let noop_result = uninstall_skills(false, false).unwrap();
            assert!(!noop_result.changed);
            assert_eq!(noop_result.installed_count, 0);
        });
    }
}
