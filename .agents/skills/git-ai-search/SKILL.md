---
name: git-ai-search
description: "Search git history for AI prompt context by commit, file, line, author, or topic, and restore a prior session when explicitly requested."
---

# Git AI Search Skill

Search and restore AI conversation context from git history using `git-ai search` and `git-ai continue`.

## Overview

Git AI tracks AI-generated code and the conversations that produced it. This skill helps you:
- **Search** - Find AI prompt sessions by commit, file, pattern, or author
- **Continue** - Restore AI session context to continue work where someone left off
- **Show** - View the full transcript of a specific prompt

### When to Use Each Command

| You want to... | Use... |
|---------------|--------|
| See what AI context exists for a commit | `git-ai search --commit <sha>` |
| See AI context for specific lines | `git-ai search --file <path> --lines <range>` |
| Read the full AI conversation | `git-ai search --commit <sha> --verbose` |
| Get machine-readable output | `git-ai search --json` |
| Find prompts mentioning a topic | `git-ai search --pattern "keyword"` |
| Continue someone's AI session | `git-ai continue --commit <sha>` |
| Continue working on a code region | `git-ai continue --file <path> --lines <range>` |
| Launch Claude with restored context | `git-ai continue --commit <sha> --launch` |
| View a specific prompt transcript | `git-ai show-prompt <id>` |

## Workflow Patterns

### 1. Investigate a Commit

Understand what AI assistance was used in a specific commit:

```bash
# See a summary of AI sessions in the commit
git-ai search --commit abc1234

# View the full conversations
git-ai search --commit abc1234 --verbose

# Get JSON for programmatic processing
git-ai search --commit abc1234 --json | jq '.prompts | keys[]'
```

### 2. Understand a Code Region

Find what AI sessions contributed to specific lines:

```bash
# Search by file
git-ai search --file src/main.rs

# Search specific lines
git-ai search --file src/main.rs --lines 100-150

# Multiple line ranges
git-ai search --file src/main.rs --lines 50-75 --lines 200-250
```

### 3. Continue Someone's Work

Pick up where a teammate left off:

```bash
# See what they were working on
git-ai search --commit abc1234 --verbose

# Restore context and continue in your shell
git-ai continue --commit abc1234

# Or launch directly into Claude Code
git-ai continue --commit abc1234 --launch

# Or copy context to clipboard to paste elsewhere
git-ai continue --commit abc1234 --clipboard
```

### 4. Review a Pull Request

Understand AI involvement in a PR:

```bash
# Get all commits in the PR
COMMITS=$(gh pr view 123 --json commits -q '.commits[].oid')

# Search each commit
for sha in $COMMITS; do
  echo "=== $sha ==="
  git-ai search --commit $sha
done

# Or search the full range
BASE=$(gh pr view 123 --json baseRefOid -q '.baseRefOid')
HEAD=$(gh pr view 123 --json headRefOid -q '.headRefOid')
git-ai search --commit $BASE..$HEAD
```

### 5. Audit AI Involvement in a File

See all AI sessions that touched a file:

```bash
# Full file history
git-ai search --file src/auth/login.rs

# Filter by author
git-ai search --file src/auth/login.rs --author "Alice"

# Filter by tool
git-ai search --file src/auth/login.rs --tool claude
```

### 6. Search by Topic

Find prompts discussing specific topics:

```bash
# Search prompt content
git-ai search --pattern "authentication"
git-ai search --pattern "error handling"

# Combine with filters
git-ai search --pattern "refactor" --tool cursor
```

### 7. Pipe to Other Tools

Integrate with scripts and workflows:

```bash
# Get prompt IDs only
git-ai search --commit abc1234 --porcelain

# Count matches
git-ai search --file src/main.rs --count

# JSON for processing
git-ai search --commit abc1234 --json | jq '.prompts[] | {tool, model, author: .human_author}'

# Find commits with AI-authored code
git log --oneline | while read sha msg; do
  count=$(git-ai search --commit $sha --count 2>/dev/null || echo "0")
  if [ "$count" != "0" ]; then
    echo "$sha ($count sessions): $msg"
  fi
done
```

## Command Reference

### git-ai search

Search for AI prompt sessions by various criteria.

```
git-ai search [OPTIONS]
```

**Context Source (pick one):**
- `--commit <rev>` - Search a specific commit (or range with `sha1..sha2`)
- `--file <path>` - Search by file path
- `--lines <range>` - Filter to line range (requires --file, repeatable)
- `--pattern <text>` - Full-text search in prompt content
- `--prompt-id <id>` - Look up specific prompt by ID

**Filters (combinable):**
- `--author <name>` - Filter by human author (substring match)
- `--tool <name>` - Filter by tool (claude, cursor, etc.)
- `--since <date>` - Only prompts after date
- `--until <date>` - Only prompts before date

**Output Format (pick one):**
- (default) - Human-readable summary
- `--json` - Full JSON output
- `--verbose` - Detailed output with full transcripts
- `--porcelain` - Machine-readable prompt IDs only
- `--count` - Just the count of matching prompts

### git-ai continue

Restore AI session context for continuation.

```
git-ai continue [OPTIONS]
```

**Context Source (pick one, or none for TUI):**
- `--commit <rev>` - Continue from a commit
- `--file <path>` - Continue from a file
- `--lines <range>` - Limit to line range (with --file)
- `--prompt-id <id>` - Continue specific prompt

**Agent Selection:**
- `--agent <name>` - Target agent (claude, cursor; default: claude)
- `--tool <name>` - Alias for --agent

**Output Mode (pick one):**
- (default) - Print formatted context to stdout
- `--launch` - Spawn agent CLI with context
- `--clipboard` - Copy context to clipboard
- `--json` - Output as structured JSON

**Options:**
- `--max-messages <n>` - Limit messages per prompt (default: 50)

### git-ai show-prompt

Display the full transcript of a specific prompt.

```
git-ai show-prompt <prompt_id> [OPTIONS]
```

- `--json` - Output as JSON
- `--raw` - Show raw internal format

## Output Formats

### Default Output

Human-readable summary:

```
Found 2 AI prompt session(s) for commit abc1234

[1] Prompt a1b2c3d4 (claude / claude-sonnet-4)
    Author: Alice
    Files: src/main.rs:10-50

[2] Prompt e5f6g7h8 (cursor / gpt-4)
    Author: Bob
    Files: src/lib.rs:100-150, src/utils.rs:20-30
```

### --json Output

Full structured data for programmatic use:

```json
{
  "source": {
    "sha": "abc1234",
    "author": "Alice",
    "date": "2025-01-15",
    "message": "Add authentication"
  },
  "prompts": [
    {
      "id": "a1b2c3d4",
      "tool": "claude",
      "model": "claude-sonnet-4",
      "author": "Alice",
      "messages": [...]
    }
  ]
}
```

### --verbose Output

Full conversations inline:

```
=== Session 1: Prompt a1b2c3d4 ===
Tool: claude (claude-sonnet-4)
Author: Alice

**User**:
Help me implement user authentication...

**Assistant**:
I'll help you implement authentication. Here's my approach...
```

### --porcelain Output

One prompt ID per line (for scripting):

```
a1b2c3d4
e5f6g7h8
```

### --count Output

Just the number:

```
2
```

## Tips

### Combining Filters

Filters use AND semantics - prompts must match ALL specified filters:

```bash
# Claude prompts by Alice in the last 7 days
git-ai search --commit HEAD~10..HEAD --tool claude --author "Alice" --since 7d
```

### Using with Git Commands

Combine with git log to explore history:

```bash
# Find commits with high AI involvement
git log --oneline --since="1 week ago" | while read sha msg; do
  git-ai search --commit $sha --count 2>/dev/null
done

# Search AI context for files changed in a commit
git diff-tree --no-commit-id --name-only -r abc1234 | while read file; do
  git-ai search --file "$file"
done
```

### Restoring Context for Different Agents

The `--agent` flag controls context formatting:

```bash
# For Claude Code
git-ai continue --commit abc1234 --agent claude --launch

# For other tools, copy to clipboard
git-ai continue --commit abc1234 --clipboard
```

### Inspecting Specific Prompts

If search returns many results, drill down:

```bash
# Get the IDs
git-ai search --commit abc1234 --porcelain

# View a specific one
git-ai show-prompt a1b2c3d4
```

### Performance Tips

- Use `--commit` for fastest results (direct note lookup)
- `--file` requires blame computation (slower for large files)
- `--pattern` searches the database (fast substring match)
- Add `--count` first to check result size before verbose output

## Examples

### Quick checks

```bash
# Any AI in this commit?
git-ai search --commit HEAD --count

# What tools were used?
git-ai search --commit abc1234 --json | jq '.prompts[].tool' | sort -u

# Who used AI in this file?
git-ai search --file src/main.rs --json | jq '.prompts[].author' | sort -u
```

### Detailed exploration

```bash
# Full context of the most recent AI session
git-ai search --commit HEAD --verbose | head -100

# Continue the exact session that wrote these lines
git-ai continue --file src/main.rs --lines 50-100 --launch
```

### Integration with CI/CD

```bash
# In a PR check, report AI involvement
PROMPT_COUNT=$(git-ai search --commit $PR_HEAD --count 2>/dev/null || echo "0")
if [ "$PROMPT_COUNT" -gt 0 ]; then
  echo "This PR includes $PROMPT_COUNT AI-assisted session(s)"
  git-ai search --commit $PR_HEAD
fi
```

## Validation

Use `--count` or a human-readable result to confirm the search scope, and report
clearly when no prompt context exists. Only continue a session after the user
explicitly asks to restore or launch it.

## Anti-Patterns

- Do not search raw prompt databases or agent logs directly.
- Do not claim a file was AI-written solely because `git-ai search` found no data.
- Do not launch another agent or continue a session without an explicit request.
