# Git AI agent skills

The canonical source for git-ai's shipped skills is this directory. Each skill
has a kebab-case directory containing a `SKILL.md`; optional `agents/`,
`references/`, and `scripts/` resources live beside it.

The runtime installer lives in `src/operations/mdm/skills_installer.rs`, where
the release binary embeds and publishes each complete skill bundle to
`~/.git-ai/skills/` and the user's agent-specific directories. The top-level
`scripts/` directory is for developer and CI utilities; it is not the runtime
installer.

Run the internal skill-review validator against this directory before changing
or releasing a skill.
