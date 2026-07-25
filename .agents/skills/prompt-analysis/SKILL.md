---
name: prompt-analysis
description: "Analyze prompt history, acceptance rates, and AI-assisted coding patterns in a repository when the user asks about prompting quality or outcomes."
allowed-tools: ["Bash(git-ai:*)", "Read", "Glob", "Grep", "Task"]
---

# Prompt Analysis Skill

Analyze AI prompting patterns using the local `prompts.db` SQLite database.

## Overview

Use this skill to inspect the local Git AI prompt database and answer questions
about prompt quality, acceptance, authors, models, and coding patterns. Keep the
default scope conservative: the current repository, current user, and recent
prompts unless the user asks for a broader analysis.

## Workflow

### What is Git AI?

Git AI is a tool that tracks AI-generated code and prompts in git. It stores:
- Every AI conversation (prompts and responses)
- Which lines of code came from AI vs human edits
- Acceptance rates (how much AI code was kept vs modified)
- Associated commits and authors

This skill queries that data to help users understand their AI coding patterns.

## Initialization

First, determine scope from the user's question:

| User mentions | Flags to use |
|---------------|--------------|
| "my prompts" or nothing specified | (default - current user, current repo) |
| "team", "everyone", "all authors" | `--all-authors` |
| specific person's name | `--author "<name>"` |
| specific time range | `--since <days>` (default: 30) |

Discovery is notes-only — `git-ai prompts` always operates on the current repository (the working directory must be inside a git repo). To analyze multiple repos, run the command separately in each.

Run initialization:
```bash
git-ai prompts [flags]
```

This creates/updates `prompts.db` in the current directory.

## Schema Reference

The `prompts` table contains:
- `seq_id` - Auto-increment ID for iteration
- `id` - Unique prompt identifier
- `tool` - Tool used (e.g., "claude-code", "cursor")
- `model` - Model name (e.g., "claude-sonnet-4-20250514")
- `human_author` - Git user who created the prompt
- `commit_sha` - Associated commit (if any)
- `total_additions`, `total_deletions` - Lines of code changed
- `accepted_lines`, `overridden_lines` - Lines kept vs modified by human
- `accepted_rate` - Ratio: accepted / (accepted + overridden)
- `messages` - JSON array of the conversation
- `start_time`, `last_time` - Unix timestamps

## Analysis Approaches

### For aggregate questions (metrics, comparisons)

Use direct SQL queries:
```bash
git-ai prompts exec "SELECT model, AVG(accepted_rate), COUNT(*) FROM prompts GROUP BY model"
```

### For per-prompt analysis (categorization, content analysis)

When questions require examining each prompt's content (messages JSON), use subagents:

1. **Add analysis columns** to the schema:
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN work_type TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN analysis_notes TEXT"
```

2. **Reset the iteration pointer**:
```bash
git-ai prompts reset
```

3. **Iterate with subagents** - Launch parallel subagents using Task tool with `subagent_type: "general-purpose"`. Each subagent:
   - Runs `git-ai prompts next` to get one prompt as JSON
   - Analyzes the `messages` content
   - Updates the database: `git-ai prompts exec "UPDATE prompts SET work_type='...' WHERE id='...'"`

4. **Final synthesis** - Query the enriched data:
```bash
git-ai prompts exec "SELECT work_type, COUNT(*), AVG(accepted_rate) FROM prompts GROUP BY work_type"
```

## Subagent Pattern for Iteration

When processing prompts individually, spawn multiple subagents in parallel. Each subagent prompt should include:

```
Run `git-ai prompts next` to get the next prompt.

Analyze the messages JSON to determine: [specific analysis task]

Then update the database:
git-ai prompts exec "UPDATE prompts SET [column]='[value]' WHERE id='[prompt_id]'"

Return your analysis result.
```

Spawn 3-5 subagents at a time, check results, spawn more until `git-ai prompts next` returns "No more prompts."

**IMPORTANT:** The `git-ai prompts next` command returns ALL the data needed for analysis as JSON, including:
- The full `messages` array with the complete conversation (human prompts and AI responses)
- Metadata like `model`, `tool`, `accepted_rate`, `accepted_lines`, etc.

Subagents should NOT run additional commands like `git show` or `git log` - everything needed is in the JSON output from `git-ai prompts next`. Instruct subagents explicitly:

```
IMPORTANT: All data you need is in the JSON output from `git-ai prompts next`.
Do NOT run git commands. Analyze the `messages` field in the JSON directly.
```

## Iterator Examples

### Example 1: Categorize prompts by work type

**User asks:** "Categorize my prompts by work type (bug fix, feature, refactor, docs)"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN work_type TEXT"
git-ai prompts reset
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

Read the messages JSON and categorize this prompt into ONE of:
- "bug_fix" - fixing broken behavior, errors, or regressions
- "feature" - adding new functionality
- "refactor" - restructuring code without changing behavior
- "docs" - documentation, comments, READMEs
- "test" - adding or modifying tests
- "config" - configuration, build, CI/CD changes
- "other" - doesn't fit above categories

Update the database:
git-ai prompts exec "UPDATE prompts SET work_type='<category>' WHERE id='<prompt_id>'"

Return: the prompt id, your categorization, and a one-sentence reason.
```

**Synthesis query:**
```sql
SELECT work_type, COUNT(*) as count,
       ROUND(AVG(accepted_rate), 3) as avg_acceptance,
       SUM(accepted_lines) as total_lines
FROM prompts
WHERE work_type IS NOT NULL
GROUP BY work_type
ORDER BY count DESC
```

### Example 2: Analyze why prompts had low acceptance

**User asks:** "Why do some of my prompts have low acceptance rates?"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN low_acceptance_reason TEXT"
git-ai prompts exec "UPDATE pointers SET current_seq_id = (SELECT MIN(seq_id) - 1 FROM prompts WHERE accepted_rate < 0.5 AND accepted_rate IS NOT NULL)"
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

This prompt had a low acceptance rate (human modified most of the AI's code).
Analyze the messages JSON and identify the likely reason:
- "vague_request" - the prompt was unclear or underspecified
- "wrong_approach" - AI took a fundamentally wrong approach
- "style_mismatch" - code worked but didn't match project conventions
- "partial_solution" - AI only solved part of the problem
- "overengineered" - AI added unnecessary complexity
- "context_missing" - AI lacked necessary context about the codebase
- "other" - explain briefly

Update the database:
git-ai prompts exec "UPDATE prompts SET low_acceptance_reason='<reason>' WHERE id='<prompt_id>'"

Return: prompt id, the reason, and specific evidence from the conversation.
```

### Example 3: Identify prompts that could be turned into reusable patterns

**User asks:** "Which of my prompts solved problems I might face again?"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN reusable_pattern TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN pattern_description TEXT"
git-ai prompts reset
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

Analyze whether this prompt represents a reusable pattern worth saving:
- Look for: common coding tasks, useful abstractions, clever solutions
- Skip: one-off fixes, highly context-specific changes, trivial edits

If reusable, set reusable_pattern to a short name (e.g., "api_error_handling", "form_validation", "test_mocking")
and pattern_description to a one-sentence description of what it does.

If not reusable, set both to NULL.

git-ai prompts exec "UPDATE prompts SET reusable_pattern='<name>', pattern_description='<desc>' WHERE id='<prompt_id>'"

Return: prompt id and whether you marked it as reusable (with the pattern name if yes).
```

### Example 4: Score prompt quality/clarity

**User asks:** "How clear are my prompts? Which ones could I have written better?"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN clarity_score INTEGER"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN clarity_feedback TEXT"
git-ai prompts reset
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

Score the HUMAN's prompt clarity from 1-5:
5 = Crystal clear: specific goal, context provided, constraints stated
4 = Good: clear intent, minor ambiguities
3 = Adequate: understandable but missing helpful context
2 = Vague: required AI to make significant assumptions
1 = Unclear: AI had to guess what was wanted

Also provide brief feedback on how the prompt could be improved.

git-ai prompts exec "UPDATE prompts SET clarity_score=<1-5>, clarity_feedback='<feedback>' WHERE id='<prompt_id>'"

Return: prompt id, score, and your feedback.
```

**Synthesis query:**
```sql
SELECT clarity_score, COUNT(*) as count,
       ROUND(AVG(accepted_rate), 3) as avg_acceptance
FROM prompts
WHERE clarity_score IS NOT NULL
GROUP BY clarity_score
ORDER BY clarity_score DESC
```

### Example 5: Correlate prompting techniques with acceptance rate

**User asks:** "Correlate my prompting techniques with acceptance rate"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN technique TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN technique_notes TEXT"
git-ai prompts reset
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

Analyze the HUMAN's prompting technique in the messages. Identify which techniques were used:
- "example_driven" - provided examples of desired output or behavior
- "step_by_step" - broke down the request into steps or phases
- "context_heavy" - provided extensive background/context about the codebase
- "minimal" - terse, brief request with little context
- "iterative" - built up solution through back-and-forth refinement
- "constraint_focused" - emphasized what NOT to do or specific requirements
- "reference_based" - pointed to existing code/files to follow as patterns
- "multiple" - combined several techniques (list them in notes)

Also note any specific effective or ineffective patterns in technique_notes.

git-ai prompts exec "UPDATE prompts SET technique='<technique>', technique_notes='<notes>' WHERE id='<prompt_id>'"

Return: prompt id, technique identified, the acceptance_rate, and your observations.
```

**Synthesis query:**
```sql
SELECT technique,
       COUNT(*) as count,
       ROUND(AVG(accepted_rate), 3) as avg_acceptance,
       ROUND(MIN(accepted_rate), 3) as min_acceptance,
       ROUND(MAX(accepted_rate), 3) as max_acceptance
FROM prompts
WHERE technique IS NOT NULL AND accepted_rate IS NOT NULL
GROUP BY technique
ORDER BY avg_acceptance DESC
```

### Example 6: Qualitative analysis of failed prompts

**User asks:** "Do a qualitative analysis of failed prompts (0 accepted)"

**Definition of "failed prompts":**
- `accepted_lines IS NULL` - conversation happened but no code was committed
- `accepted_lines = 0` - code was generated but human rejected/rewrote all of it

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN failure_analysis TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN failure_category TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN improvement_suggestion TEXT"
-- Set pointer to iterate only over failed prompts
git-ai prompts exec "DELETE FROM pointers WHERE name='default'"
git-ai prompts exec "INSERT INTO pointers (name, current_seq_id) VALUES ('default', (SELECT MIN(seq_id) - 1 FROM prompts WHERE accepted_lines IS NULL OR accepted_lines = 0))"
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

This prompt resulted in 0% acceptance - the human rejected or completely rewrote all AI-generated code.

Perform a qualitative analysis by reading the full conversation in messages JSON:

1. **What was requested?** Summarize the human's goal.
2. **What did the AI produce?** Summarize what code/solution was generated.
3. **Why did it fail?** Categorize into:
   - "misunderstood_intent" - AI solved the wrong problem
   - "poor_code_quality" - bugs, errors, or broken code
   - "wrong_technology" - used wrong framework/library/approach
   - "incomplete" - only partially addressed the request
   - "style_violation" - worked but violated project conventions
   - "overcomplicated" - solution far more complex than needed
   - "security_issue" - introduced vulnerabilities
   - "abandoned" - user changed direction mid-conversation

4. **How could the prompt be improved?** Specific, actionable suggestion.

git-ai prompts exec "UPDATE prompts SET failure_category='<category>', failure_analysis='<analysis>', improvement_suggestion='<suggestion>' WHERE id='<prompt_id>'"

Return a detailed analysis including: prompt id, what was requested, what went wrong, and how to prompt better next time.
```

**Synthesis:**
```sql
SELECT failure_category,
       COUNT(*) as count,
       GROUP_CONCAT(id, ', ') as prompt_ids
FROM prompts
WHERE failure_category IS NOT NULL
GROUP BY failure_category
ORDER BY count DESC
```

Then review individual `failure_analysis` and `improvement_suggestion` values to compile lessons learned.

### Example 7: Grade prompts according to best practices

**User asks:** "Grade my prompts according to best practices"

**Setup:**
```bash
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN grade TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN grade_breakdown TEXT"
git-ai prompts exec "ALTER TABLE prompts ADD COLUMN grade_feedback TEXT"
git-ai prompts reset
```

**Subagent prompt:**
```
Run `git-ai prompts next` to get the next prompt as JSON.

IMPORTANT: All data you need is in this JSON output. Do NOT run git commands.
Analyze the `messages` field directly.

Grade the HUMAN's prompt against these best practices for AI-assisted coding:

**Grading Criteria (each worth 0-2 points):**

1. **Specificity (0-2)**: Does the prompt clearly state what needs to be done?
   - 0: Vague or ambiguous ("fix this", "make it better")
   - 1: General direction but missing details ("add validation")
   - 2: Specific outcome described ("add email validation that checks format and shows inline error")

2. **Context (0-2)**: Does the prompt provide necessary background?
   - 0: No context, assumes AI knows everything
   - 1: Some context but missing key details
   - 2: Relevant files, constraints, or project patterns mentioned

3. **Scope (0-2)**: Is the request appropriately sized?
   - 0: Too broad ("rewrite the app") or impossibly vague
   - 1: Large but manageable, could be broken down
   - 2: Focused, single-responsibility task

4. **Constraints (0-2)**: Are requirements and boundaries specified?
   - 0: No constraints given when they'd be helpful
   - 1: Some constraints but missing important ones
   - 2: Clear about what to do AND what not to do

5. **Testability (0-2)**: Can success be verified?
   - 0: No way to know if the result is correct
   - 1: Implicit success criteria
   - 2: Explicit expected behavior or acceptance criteria

**Calculate total (0-10) and assign grade:**
- A (9-10): Excellent prompt, follows best practices
- B (7-8): Good prompt, minor improvements possible
- C (5-6): Adequate prompt, notable gaps
- D (3-4): Poor prompt, significant issues
- F (0-2): Ineffective prompt, needs major revision

Store breakdown as JSON: {"specificity": X, "context": X, "scope": X, "constraints": X, "testability": X}

git-ai prompts exec "UPDATE prompts SET grade='<A-F>', grade_breakdown='<json>', grade_feedback='<specific feedback>' WHERE id='<prompt_id>'"

Return: prompt id, grade, total score, and one key improvement suggestion.
```

**Synthesis query:**
```sql
SELECT grade,
       COUNT(*) as count,
       ROUND(AVG(accepted_rate), 3) as avg_acceptance,
       SUM(accepted_lines) as total_lines_accepted
FROM prompts
WHERE grade IS NOT NULL AND accepted_rate IS NOT NULL
GROUP BY grade
ORDER BY grade
```

**Correlation analysis:**
```sql
-- See if grade correlates with acceptance rate
SELECT
    grade,
    ROUND(AVG(accepted_rate), 3) as avg_acceptance,
    COUNT(*) as n
FROM prompts
WHERE grade IS NOT NULL AND accepted_rate IS NOT NULL
GROUP BY grade
ORDER BY
    CASE grade
        WHEN 'A' THEN 1
        WHEN 'B' THEN 2
        WHEN 'C' THEN 3
        WHEN 'D' THEN 4
        WHEN 'F' THEN 5
    END
```

## Example Queries

**Acceptance rate by model:**
```sql
SELECT model,
       ROUND(AVG(accepted_rate), 3) as avg_rate,
       SUM(accepted_lines) as total_accepted,
       COUNT(*) as prompt_count
FROM prompts
GROUP BY model
ORDER BY avg_rate DESC
```

**Most productive prompts (high acceptance + high LoC):**
```sql
SELECT id, tool, model, accepted_lines, accepted_rate,
       (last_time - start_time) as duration_secs
FROM prompts
WHERE accepted_rate > 0.8 AND accepted_lines > 50
ORDER BY accepted_lines DESC
LIMIT 10
```

**Prompts by time of day:**
```sql
SELECT
  CASE
    WHEN (start_time % 86400) / 3600 < 12 THEN 'morning'
    WHEN (start_time % 86400) / 3600 < 17 THEN 'afternoon'
    ELSE 'evening'
  END as time_of_day,
  COUNT(*) as count,
  AVG(accepted_rate) as avg_rate
FROM prompts
GROUP BY time_of_day
```

## Cleanup

The `prompts.db` file is ephemeral. Delete it anytime to start fresh:
```bash
rm prompts.db
```

## Troubleshooting

**If there are 0 prompts in the database:**
Git AI may not be set up correctly in this repository. Direct the user to:
- Check https://usegitai.com/docs/cli for setup instructions
- Verify `git-ai` is installed and configured
- Ensure they have made commits with AI assistance after setup

**Important: Git AI only tracks data going forward.**
Git AI cannot retroactively categorize or track prompts from before it was installed. Only commits made after Git AI setup will have prompt data. If the user recently installed Git AI, they may have limited data until they accumulate more AI-assisted commits.

## Footguns and Data Caveats

**Not all prompts have stats or commits:**
- `commit_sha` is NULL when the prompt conversation happened but no code was ever committed
- `accepted_lines`, `overridden_lines`, `accepted_rate` may be NULL or 0 for uncommitted work
- `total_additions`, `total_deletions` may be NULL for the same reason

**Why this happens:**
- User had a conversation but didn't accept the code
- User accepted code but never committed it
- User is still working (code not yet committed)

**Definition of "failed prompts":**
When analyzing failed or unsuccessful prompts, use this definition:
- `accepted_lines IS NULL` - conversation happened but no code was committed
- `accepted_lines = 0` - code was generated but human rejected/rewrote all of it

Query: `WHERE accepted_lines IS NULL OR accepted_lines = 0`

**Handle in queries:**
```sql
-- Exclude uncommitted prompts from acceptance rate analysis
SELECT model, AVG(accepted_rate)
FROM prompts
WHERE commit_sha IS NOT NULL AND accepted_rate IS NOT NULL
GROUP BY model

-- Find prompts with conversation but no committed code
SELECT id, tool, start_time
FROM prompts
WHERE commit_sha IS NULL
```

## Important Notes

- The `messages` column contains JSON - parse it to read conversation content
- Use subagents for any analysis requiring reading message content
- SQLite is a scratchpad - add columns freely for analysis
- Default scope is conservative (current user, current repo, 30 days)
- Expand scope only when user explicitly mentions team/all repos

## Permissions Setup

To avoid being prompted for every `git-ai` command, add to project or user settings:

**Project:** `.claude/settings.json`
**User:** `~/.claude/settings.json`

```json
{
  "permissions": {
    "allow": [
      "Bash(git-ai:*)"
    ]
  }
}
```

This is especially important for subagent iteration, as subagents don't inherit skill-level permissions.

## Validation

- Run `git-ai prompts` before querying so the local database reflects the current repository.
- Check the query result count and nullability before drawing conclusions from acceptance rates.
- Keep qualitative message analysis in subagents and use SQL for aggregate summaries.

## Anti-Patterns

- Do not treat missing prompt data as evidence that no AI assistance occurred.
- Do not include uncommitted or null acceptance values in rate comparisons without saying so.
- Do not expand from the current repository or user to team-wide data without an explicit request.
