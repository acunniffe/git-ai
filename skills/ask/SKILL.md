---
name: ask
description: "Answer questions about AI-authored code by inspecting supported Git AI authorship data for selected lines."
argument-hint: "[a question about selected code or a specific file/range]"
allowed-tools: ["Bash(git-ai:*)", "Read", "Glob", "Grep", "Task"]
---

# Ask Skill

Answer questions about AI-written code by locating the relevant file and line range, reading Git AI authorship metadata, and then explaining the code with the available prompt context.

## Main agent workflow

1. Resolve the file path and line range.

   Prefer these sources in order:

   - editor selection context
   - explicit file and line references from the user
   - a named symbol found by reading or grepping the repository
   - a file path without line specifics

   If no file, symbol, or selected code can be identified, reply:

   > Select some code or mention a specific file/symbol, then `/ask` your question.

2. Spawn one tightly scoped subagent with `max_turns: 4`.

3. Relay the answer to the user.

## Subagent configuration

The subagent gets only `Bash` and `Read`. It does not get `Glob`, `Grep`, or `Task`. Keep the subagent focused on one authorship lookup plus one code read.

```text
Task tool settings:
  subagent_type: "general-purpose"
  max_turns: 4
  allowed tools: Bash, Read
```

## Supported lookup command

Use `git-ai blame --show-prompt`. Do not use `git-ai search`; it is not part of the current CLI.

```bash
# File range with prompt payloads appended when piped
git-ai blame src/commands/blame.rs -L 23,54 --show-prompt | cat

# Whole-file fallback when no exact line range is available
git-ai blame src/commands/blame.rs --show-prompt | cat
```

## Subagent prompt template

When an exact line range is available, fill in `{question}`, `{file_path}`, `{start}`, and `{end}`:

```text
You are answering a question about code by inspecting the available Git AI authorship context.

QUESTION: {question}
FILE: {file_path}
LINES: {start}-{end}

You have exactly 3 steps. Do them in order, then stop.

STEP 1 - Authorship lookup:
  Run: git-ai blame {file_path} -L {start},{end} --show-prompt | cat
  Do not run git-ai search, git-ai prompts, or any transcript-directory search.

STEP 2 - Read the code:
  Read {file_path}, focusing on lines {start}-{end}.

STEP 3 - Answer:
  Use the prompt context from Step 1 when available. If no prompt context is found,
  say that clearly and answer from the code alone.

Format:
- Answer: direct answer
- Original context: what prompt/authorship data was found, if any
- Evidence: file/line references and command output used
```

When no exact line range is available, omit the `LINES:` field and use this lookup step instead:

```text
STEP 1 - Authorship lookup:
  Run: git-ai blame {file_path} --show-prompt | cat
  Do not run git-ai search, git-ai prompts, or any transcript-directory search.
```

## Hard constraints

- Subagents may use only `Bash` and `Read`.
- Do not give subagents `Glob`, `Grep`, or `Task`.
- Do not run more than one `git-ai blame` command unless the first command fails because the line range is invalid.
- Do not use `git-ai search`.
- Do not use `git-ai prompts`.
- Do not read `.claude/`, `.cursor/`, `.agents/`, or agent log directories directly.
- Do not invent prompt context when Git AI metadata is absent.
