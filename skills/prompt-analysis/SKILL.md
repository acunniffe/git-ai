---
name: prompt-analysis
description: "Analyze AI prompting and authorship patterns using supported git-ai commands"
argument-hint: "[question about prompts, models, acceptance, or AI authorship]"
allowed-tools: ["Bash(git-ai:*)", "Read", "Glob", "Grep", "Task"]
---

# Prompt Analysis Skill

Analyze AI prompting and authorship patterns using the current Git AI CLI. This skill works from authorship notes and prompt metadata exposed by supported commands.

## Important CLI constraints

Do not run:

- `git-ai prompts`
- `git-ai prompts exec`
- `git-ai prompts next`
- `git-ai prompts reset`
- any workflow that assumes a local `prompts.db`

Those commands are not available in the current CLI. Use note-backed commands instead.

## Supported data sources

| Question type | Preferred command |
|---|---|
| Commit AI percentage / accepted lines | `git-ai stats <commit> --json` |
| Prompt metadata for a commit | `git-ai diff <commit> --json --include-stats --all-prompts` |
| Prompt payloads for selected lines | `git-ai blame <file> -L <start>,<end> --show-prompt \| cat` |
| Authorship logs for a revision or range | `git-ai show <rev-or-range>` |
| Specific prompt id lookup | `git-ai show-prompt <prompt_id>` |

## Analysis workflows

### 1. Summarize AI usage over recent commits

```bash
for sha in $(git log --format=%H --since="30 days ago"); do
  git-ai stats "$sha" --json 2>/dev/null || true
done
```

Collect the JSON results, then summarize totals by model, tool, author, or acceptance fields that are present in the output.

### 2. Compare prompts or models for a commit range

```bash
for sha in $(git log --format=%H main..HEAD); do
  git-ai diff "$sha" --json --include-stats --all-prompts 2>/dev/null || true
done
```

Use the JSON prompt metadata that appears in each commit's authorship note. Do not assume a prompt database exists.

### 3. Analyze selected code and its prompt context

```bash
git-ai blame src/main.rs -L 100,150 --show-prompt | cat
```

Use this when the user selects code or asks why a specific region was written in a certain way. The command can include prompt payloads for the lines when available.

### 4. Inspect one known prompt

```bash
git-ai show-prompt <prompt_id>
```

Only use this when another command or the user provides a prompt id. Do not fabricate ids.

## Subagent guidance

When analysis requires reading many prompt payloads, spawn a small number of subagents and give each subagent a fixed set of commits or files. Each subagent should use only the supported commands listed above.

Subagents must not call `git-ai prompts` or rely on `prompts.db`.

## Data caveats

- Git AI only has metadata for work captured after setup.
- Some commits may have stats but no prompt payloads.
- Some prompt records may be local-only or unavailable on the current machine.
- Absence of prompt data does not prove a commit was human-written; it only means no prompt data was found locally.

## Output expectations

When answering the user:

1. State which commands were used.
2. Summarize the available data.
3. Clearly separate measured values from interpretation.
4. Call out missing or unavailable prompt metadata instead of guessing.
