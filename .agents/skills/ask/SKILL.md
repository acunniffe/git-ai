---
name: ask
description: "Use this when exploring a specific code file, symbol, or selection and you want the original AI-authorship context behind how or why it was written."
---

# Ask Skill

Answer questions about AI-written code by finding the original prompts and conversations that produced it, then **embodying the author agent's perspective** to answer.

## Overview

Use this skill to explain the AI-authorship context behind a specific file,
symbol, line range, or editor selection. It requires a concrete code reference
and must not guess a location when the user provides none.

## Workflow

## Main Agent's Job (you)

You do the prep work, then hand off to a **fast, tightly scoped subagent**:

1. **Resolve the file path and line range** — check these sources in order:

   **a) Editor selection context (most common).** When the user has lines selected in their editor, a `<system-reminder>` is injected into the conversation like:
   ```
   The user selected the lines 2 to 4 from /path/to/file.rs:
   _flush_logs(args: &[String]) {
       flush::handle_flush_logs(args);
   }
   ```
   Extract the file path and line range directly from this. This is the primary way users will invoke `/ask` — they select code, then type something like "/ask why is this like that" without naming the file or lines.

   **b) Explicit file/line references** — "on line 42", "lines 10-50 of src/main.rs" → use directly.

   **c) Named symbol** — mentions a variable/function/class → Read the file, find where it's defined, extract line numbers.

   **d) File without line specifics** → whole file (omit `--lines`).

   **e) No file, no lines, no selection context, no identifiable code reference** → Do NOT attempt to guess or search. Just reply:
   > Select some code or mention a specific file/symbol, then `/ask` your question.

   Stop here. Do not spawn a subagent.

2. **Spawn one subagent** with the template below. Use `max_turns: 4`.

3. **Relay the answer** to the user. That's it.

## Subagent Configuration

```
Task tool settings:
  subagent_type: "general-purpose"
  max_turns: 4
```

The subagent gets **only** `Bash` and `Read`. It does NOT get Glob, Grep, or Task. It runs at most 4 turns — this is a fast lookup, not a research project.

## Choosing Between `blame --show-prompt` and `search`

**If you want to read an entire file or range of lines AND the corresponding prompts behind them, use `git-ai blame --show-prompt`.** This is better than `search` for this use case — it gives you every line's authorship plus the full prompt JSON in one call.

```
# Get blame + prompts for a line range (pipe to get prompt dump appended):
git-ai blame src/commands/blame.rs -L 23,54 --show-prompt | cat

# Interactive (TTY) mode shows prompt hashes inline:
# 7a4471d (cursor [abc123e] 2026-02-06 14:20:05 -0800   23)     code_here

# Piped mode appends raw prompt messages after a --- separator:
# ---
# Prompt [abc123e]
# [{"type":"user","text":"Write a function..."},{"type":"assistant","text":"Here is..."}]
```

Use `git-ai search` when you need to find prompts by **commit**, **keyword**, or when you don't have a specific file/line range in mind.

## Subagent Prompt Template

Fill in `{question}`, `{file_path}`, and `{start}-{end}` (omit LINES if not applicable):

```
You are answering a question about code by finding the original AI conversation
that produced it. You will embody the author agent's perspective — first person,
as the agent that wrote the code.

QUESTION: {question}
FILE: {file_path}
LINES: {start}-{end}

You have exactly 3 steps. Do them in order, then stop.

STEP 1 — Search (one command):
  Run: git-ai search --file {file_path} --lines {start}-{end} --verbose
  If no results, try ONE fallback: git-ai search --file {file_path} --verbose
  That's it. Do not run more than 2 git-ai commands total.

STEP 2 — Read the code (one Read call):
  Read {file_path} (focus on lines {start}-{end})

STEP 3 — Answer:
  Using the transcript from Step 1 and the code from Step 2, answer the
  question AS THE AUTHOR in first person:
  - "I wrote this because..."
  - "The problem I was solving was..."
  - "I chose X over Y because..."

  Format:
  - **Answer**: Direct answer in the author's voice
  - **Original context**: What the human asked for and why
  - **Date(s)**: Dates, Human Author where this feature was worked on. 

  If no transcript was found, say so clearly: "I couldn't find AI conversation
  history for this code — it may be human-written or predate git-ai setup."
  In that case, analyze the code objectively (not first person).

HARD CONSTRAINTS:
- Do NOT use Glob, Grep, or Task tools. You only have Bash and Read.
- Do NOT run more than 2 git-ai commands.
- Do NOT read .claude/, .cursor/, .agents/, or any agent log directories.
- Do NOT search JSONL transcripts or session logs directly.
- All conversation data comes from `git-ai search` only.
```

When the user's question doesn't reference specific lines, omit `--lines` from Step 1 and the `LINES:` field.

## Fallback Behavior

When no prompt data is found:
- The code might be human-written or predate git-ai
- Answer from the code alone, clearly stating no AI history was found
- Do NOT use first-person author voice in fallback — analyze objectively

## Example Invocations

**User selects lines 10-25 in editor, types: `/ask why is this like that`**
Selection context is in system-reminder → extract file + lines 10-25, spawn subagent. This is the most common usage pattern.

**`/ask why does this function use recursion instead of iteration?`**
Main agent finds the function definition, extracts file/lines, spawns subagent.

**`/ask what problem was being solved on lines 100-150 of src/main.rs?`**
File and lines explicit — spawn subagent directly.

**`/ask why was this approach chosen over using a HashMap?`**
Main agent identifies relevant code from context, spawns subagent.

## Validation

- Confirm the file or symbol and line range before searching.
- Use no more than two `git-ai search` commands for one request.
- If no transcript is found, say so and switch to objective code analysis.

## Anti-Patterns

- Do not guess a file or symbol when the request provides no code reference.
- Do not read raw transcript databases or agent-log directories directly.
- Do not present an inferred explanation as the original author's intent.
