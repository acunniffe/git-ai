---
name: git-ai-search
description: "Inspect AI authorship context using supported git-ai commands"
argument-hint: "[commit, file, line range, or prompt id]"
allowed-tools: ["Bash(git-ai:*)", "Read", "Glob", "Grep"]
---

# Git AI Context Inspection Skill

Use this skill to inspect AI authorship metadata that Git AI has already attached to commits and lines. This version intentionally uses only commands that exist in the current CLI.

The `git-ai-search` skill name is retained for backwards compatibility with existing skill invocations. Despite the name, this skill must not call the unsupported `git-ai search` command.

## Important CLI constraints

Do not run these commands from this skill:

- `git-ai search`
- `git-ai continue`
- `git-ai prompts`

Those commands are not part of the current CLI surface. Prefer the supported commands below.

## Supported command map

| Goal | Supported command |
|---|---|
| Inspect authorship note(s) for a commit or range | `git-ai show <rev-or-range>` |
| Get commit-level AI stats | `git-ai stats <commit> --json` |
| Inspect one commit's diff with stats and prompt metadata | `git-ai diff <commit> --json --include-stats --all-prompts` |
| Inspect line authorship for a file | `git-ai blame <file>` |
| Inspect line authorship and prompt payloads for a range | `git-ai blame <file> -L <start>,<end> --show-prompt \| cat` |
| Display one known prompt record | `git-ai show-prompt <prompt_id>` |
| Explore commit history with notes | `git-ai log --oneline` |

## Workflow patterns

### 1. Investigate a commit

```bash
# Show authorship log data attached to the commit
git-ai show abc1234

# Get machine-readable stats
git-ai stats abc1234 --json

# Inspect the diff with stats and prompt metadata
git-ai diff abc1234 --json --include-stats --all-prompts
```

Use `git-ai diff ... --all-prompts` when you need prompt metadata that is already stored in the authorship note for that commit.

### 2. Understand a code region

```bash
# Blame a whole file with AI authorship overlay
git-ai blame src/main.rs

# Blame a line range and include prompt payloads when stdout is piped
git-ai blame src/main.rs -L 100,150 --show-prompt | cat
```

When the user asks about selected lines, prefer `blame --show-prompt` because it ties the current file range to prompt metadata without relying on a separate search command.

### 3. Review a pull request

```bash
COMMITS=$(gh pr view 123 --json commits -q '.commits[].oid')

for sha in $COMMITS; do
  echo "=== $sha ==="
  git-ai stats "$sha" --json || true
  git-ai diff "$sha" --json --include-stats --all-prompts || true
done
```

Use `|| true` for review loops so one commit without authorship metadata does not stop the entire inspection.

### 4. Inspect a specific prompt id

If a prior command prints a prompt id or prompt hash, drill into it with:

```bash
git-ai show-prompt <prompt_id>
```

If `show-prompt` cannot find the id, say that the prompt may not be present in the local prompt store or may require a commit-scoped lookup.

## Fallback behavior

If no authorship data is found:

- The code may be human-written.
- The code may predate Git AI setup.
- The relevant notes may not have been fetched locally.

Do not invent prompt context. State clearly that no AI conversation history was found and continue with code-only analysis when helpful.
