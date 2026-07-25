# Git AI agent skills

The canonical source for git-ai's shipped skills is this directory. Each skill
has a kebab-case directory containing a `SKILL.md`; optional `agents/`,
`references/`, and `scripts/` resources live beside it.

The Rust installer embeds the `SKILL.md` files from here and publishes them to
the user's agent-specific skill directories. Run the internal skill-review
validator against this directory before changing or releasing a skill.
