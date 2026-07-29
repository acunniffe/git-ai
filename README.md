# git-ai   <a href="https://discord.gg/XJStYvkb5U"><img alt="Discord" src="https://img.shields.io/badge/discord-join-5865F2?logo=discord&logoColor=white" /></a>        

<img src="https://github.com/git-ai-project/git-ai/raw/main/assets/docs/git-ai.png" align="right"
     alt="Git AI Logo" width="200" height="200">

Git AI is an open source git extension that tracks the AI-generated code in your repositories. After installing the extension, every line of AI code is linked to the **agent, model, and prompts** that generated it — so you never lose the intent, requirements, and architecture decisions behind your code.

**Just prompt and commit** — no workflow changes:

`git commit`

```
[hooks-doctor 0afe44b2] wsl compat check
2 files changed, 81 insertions(+), 3 deletions(-)

you  ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ ai
     6%                                  94%
```

`git ai blame /src/log_fmt/authorship_log.rs`

```bash
cb832b7 (Aidan Cunniffe      2025-12-13  133) pub fn execute_diff(
cb832b7 (Aidan Cunniffe      2025-12-13  134)     repo: &Repository,
cb832b7 (Aidan Cunniffe      2025-12-13  135)     spec: DiffSpec,
fe2c4c8 (Aidan Cunniffe      2025-12-02  136)     format: DiffFormat,
fe2c4c8 (Aidan Cunniffe      2025-12-02  137) ) -> Result<…> {
fe2c4c8 (claude              2025-12-02  138)     // Resolve commits
fe2c4c8 (claude              2025-12-02  139)     let (from, to) = match spec {
fe2c4c8 (claude              2025-12-02  140)         DiffSpec::TwoCommit(s, e) => {
fe2c4c8 (claude              2025-12-02  141)             let from = resolve(repo, &s)?;
```


## Install

**Mac, Linux, Windows (WSL)**

```bash
curl -sSL https://usegitai.com/install.sh | bash
```

**Windows**

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://usegitai.com/install.ps1 | iex"
```

**No per-repo setup or git hooks required.** Commit with the Agent, git, or your favorite git client. Attribution will be linked to commits automatically.

**The [Git AI standard](https://github.com/git-ai-project/git-ai/blob/main/specs/git_ai_standard_v3.0.0.md) is supported by:**
<table>
<tr>
<td align="center" width="20%"><img src="assets/docs/agents/gray/claude_code.png" alt="Claude Code" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/codex-black.png" alt="Codex" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/cursor.png" alt="Cursor" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/copilot.png" alt="GitHub Copilot" width="160" /></td>
<td align="center" width="20%"><img src="assets/docs/agents/gray/opencode.png" alt="OpenCode" width="160" /></td>
</tr>
<tr>
<td align="center"><img src="assets/docs/agents/gray/pi.png" alt="Pi" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/windsurf.png" alt="Windsurf" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/droid.png" alt="Droid" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/amp.png" alt="Amp" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/gemini.png" alt="Gemini" width="160" /></td>
</tr>
<tr>
<td align="center"><img src="assets/docs/agents/gray/continue.png" alt="Continue" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/junie_white.png" alt="Junie" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/rovodev.png" alt="Rovo Dev" width="160" /></td>
<td align="center"><img src="assets/docs/agents/gray/firebender.png" alt="Firebender" width="160" /></td>
<td align="center"><a href="https://usegitai.com/docs/cli/add-your-agent">+ Add an Agent</a></td>
</tr>
</table>


## Our Choices

- 🪄 **Transparent** — Git AI requires no workflow changes. Just prompt and commit as you normally would and Git AI automatically attaches attribution metadata to every commit. 
- ⚡ **No performance overhead** — Git AI does not rely on Git Hooks (slow + difficult to set up in every repo) and it does not wrap the Git binary. Your Git operations are just as fast as they were before Git AI was installed. 
- 💻 **Local-first** — Works offline, no login required.
- 🔒 **Secure Prompt Storage** — Git AI links each line of AI-code back to the prompt that generated it. These sessions are scanned and redacted, and saved outside of Git -- keeping repos lean, enabling fine-grained access control, and preventing PII or secrets from leaking. 
- 🌐 **Git native and open standard** — Git AI built the [open standard](https://github.com/git-ai-project/git-ai/blob/main/specs/git_ai_standard_v3.0.0.md) for tracking AI-generated code with Git Notes.

Want to learn more? 

<a href="https://calendly.com/d/cxjh-z79-ktm/meeting-with-git-ai-authors" target="_blank"><img src="assets/docs/buttons/meet-the-maintainers.svg" alt="Meet the maintainers" height="40" /></a>

### Documentation  

- [CI Actions](https://usegitai.com/docs/guides/ci-workflows) preserves attribution through Rebase and Merge & Square and Merge.
- [How Git AI Works](https://usegitai.com/docs/cli/how-git-ai-works) 
- [Stats command](https://usegitai.com/docs/cli/commit-stats) - aggregate % AI stats across commits
- [AI Blame](https://usegitai.com/docs/cli/ai-blame) - 
- [Config](https://usegitai.com/docs/cli/configuration) - 
- [Add support for an agent in Git AI](https://usegitai.com/docs/guides/add-your-agent)
- Install Git AI in Background Agents: [Claude Web](https://usegitai.com/docs/cli/claude-web), [Codex Cloud](https://usegitai.com/docs/cli/codex-cloud), [Cursor Agent](https://usegitai.com/docs/cli/cursor-agent), and [Devin](https://usegitai.com/docs/cli/devin).

## Attribution Stats

Line-level AI-attribution let you track AI-code through the full SDLC. Track how much AI code gets accepted, committed, through code review, and into production — to identify which tools and practices work best.

```bash
git ai stats --json
git ai stats <start_sha>..<end_sha> --json
```

Calculates % AI-code, AI-lines generated vs committed, accepted rates, human overrides broken down by tool and model. Learn more: [Stats command reference docs](https://usegitai.com/docs/cli/reference#stats). 


<details>
<summary>Example JSON output</summary>

```json
{
  "human_additions": 28,
  "ai_additions": 76,
  "ai_accepted": 47,
  "git_diff_deleted_lines": 34,
  "git_diff_added_lines": 104,
  "tool_model_breakdown": {
    "claude_code/claude-sonnet-4-5-20250929": {
      "ai_additions": 76,
      "ai_accepted": 47
    }
  }
}
```

</details>

## AI Blame

Git AI blame is a drop-in replacement for `git blame` that shows AI attribution for each line. It supports [all standard `git blame` flags](https://git-scm.com/docs/git-blame).

```bash
git ai blame /src/log_fmt/authorship_log.rs
```

```bash
cb832b7 (Aidan Cunniffe 2025-12-13 08:16:29 -0500  136)     format: DiffFormat,
cb832b7 (Aidan Cunniffe 2025-12-13 08:16:29 -0500  137) ) -> Result<String, GitAiError> {
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  138)     // Resolve commits to get from/to SHAs
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  139)     let (from_commit, to_commit) = match spec {
fe2c4c8 (claude         2025-12-02 19:25:13 -0500  140)         DiffSpec::TwoCommit(start, end) => {

```

<img align="right" width="350" alt="Git AI VS Code extension showing color-coded AI blame in the gutter" src="https://github.com/user-attachments/assets/94e332e7-5d96-4e5c-8757-63ac0e2f88e0" />

There are community plugins that display AI-attribution in popular IDEs, color-coded by agent session. Hover over a line to see the raw prompt or summary.

**Supported Editors**

- [VS Code](https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode)
- [Cursor](https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode)
- [Windsurf](https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode)
- [Antigravity](https://marketplace.visualstudio.com/items?itemName=git-ai.git-ai-vscode)
- [Emacs magit](https://github.com/jwiegley/magit-ai)
- *Built support for another editor? [Open a PR](https://github.com/git-ai-project/git-ai/pulls)*

<br clear="all" />

## For teams and enterprises

<a href="https://calendly.com/d/cxjh-z79-ktm/meeting-with-git-ai-authors" target="_blank"><img src="assets/docs/buttons/get-early-access.svg" alt="Get early access" height="40" /></a>

We built Git AI for Teams to make it easy to roll out Git AI across your organization. Just connect GitHub, GitLab, Bitbucket, or Azure DevOps and get aggregate insights across all your repositories, plus the full trace of every agent session—from prompt all the way to production.

- See how much AI-code makes it all the way to production
- Measure **% AI** and token cost by Pull Request, Repo, Team, and Contributor
- Measure and improve agent autonomy and token efficiency
- Measure AI-code durability and how much rework AI-code requires before and after deployment
- Tie incidents back to AI-sessions
- Save prompts behind every generated hunk of code for harness engineering and code review

<img width="1229" height="783" alt="Screenshot 2026-07-29 at 3 48 26 PM" src="https://github.com/user-attachments/assets/be4d5c3b-b37f-48d9-9e9e-69808c94a0f8" />


## FAQs

#### How does it work?

1. Coding Agents call `git-ai checkpoint` whenever they write code or modify files with bash scripts. 
1. On commit, Git AI stores line-level attribution data in Git Notes, linking each line of AI-generated code to the agent, model, and session that created it. Run `git log --show-notes="ai"` to see them. 
1. Git AI moves and merges line-level attributions when you `squash`, `merge`, `reset`, `rebase`, `stash`, `cherry-pick`, etc. so your AI code is always accurately tracked.

*Git AI does not use AI or heuristics to "detect" AI code — the Agents report exactly which lines they wrote, providing the most accurate, explicit attribution possible.*

#### Does the agent have to commit for Git AI to attribute the code?
No. Git AI works no matter how you commit — your Git client, the Git CLI, and your own Git aliases are all supported.

#### Git AI notes are attached to commits — how are attributions preserved when I rebase, squash, stash, cherry-pick, etc.?
Git AI analyzes the final state of the code after the operation completes and copies/merges the attributions into a Git Note for any completed commits. It's eventually consistent. The note will be written 5-100ms after the operation completes.

#### Can I use this on my own?
Yes. Git AI is free and open source, works locally, and requires no login or team setup.

#### Is there a performance impact?
No. Git AI does not use Git hooks and it does not wrap Git, so you won't see any overhead on your Git commands.

#### Do I have to set up agent hooks?
Nope — Git AI manages the agent hooks and checks/updates them daily. If you want to trigger this yourself (ie just installed a new agent) run `git ai install-hooks`.

#### Who uses this?
Hundreds of engineering teams (including many in the Fortune 100) use Git AI to understand their AI usage and make agents more effective on their codebase.

#### What's the difference between the open source CLI and the [teams version](https://usegitai.com)?
The CLI accurately attributes AI code on every commit. The teams version adds a secure prompt store and joins in data from across the SDLC — tying token spend to individual Pull Requests, calculating % AI by PR, team, and repo, and connecting signals like amount of rework during code review, and even tying incidents back to the AI session that caused them. Self-host it or run it in our cloud: connect your SCM and get aggregate stats across thousands of repos plus full observability into everything your coding agents do.

#### Who built this?
[Aidan](https://github.com/acunniffe) and [Sasha](https://github.com/svarlamov) — say hi in [Discord](https://discord.gg/XJStYvkb5U) or set up a [Meet the maintainers call](https://calendly.com/d/cxjh-z79-ktm/meeting-with-git-ai-authors).

#### What are the capabilities and known limitations?
Git AI provides line-level attribution for AI-generated code - whether it is written with an edit tool or a bash command. When a Git rewrite operation is run (`rebase`, `stash`, `squash --merge`, etc) Git AI will move and merge attributions so nothing is lost. 

Here is a full breakdown of what is supported today: 

| Capability                                                      | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| Edit / Write / Patch tools                                      | ✅      | Line-level attribution recorded automatically.                               |
| Files created via Bash                                          | ✅      | May not work if the agent is not operating from the repository root.         |
| Git worktrees                                                   | ✅      | Attribution maintained across linked worktrees.                              |
| Background Agents                                               | ✅      | See docs for [Claude Web](https://usegitai.com/docs/cli/claude-web), [Codex Cloud](https://usegitai.com/docs/cli/codex-cloud), [Cursor Agent](https://usegitai.com/docs/cli/cursor-agent), and [Devin](https://usegitai.com/docs/cli/devin). |
| Attribute lines from multiple Agent Sessions in the same commit | ✅      |                                                                              |
| Record which lines a human overrode                             | ✅      |                                                                              |
| Attribute sessions that produced no code                        | ✅      | Records token usage and session activity even when no code is accepted.      |
| Accepted rate per session                                       | ✅      |                                                                              |
| Added and deleted lines per session                             | ✅      |                                                                              |
| Tool-call level attribution                                     | ✅      | Resolves attributed lines to the tool call that generated them.              |
| Tokens and cost per commit and PR                               | ✅      | Aggregates token usage and cost across the sessions behind each commit/PR.   |
| Formatters                                                      | ✅      | Formatting will not change attribution to human.                             |
| Multi-repo root                                                 | ⚠️     | If you run an agent that edits multiple repos, Bash attributions only work when the agent runs each command with its cwd inside that repo. |

Git Rewrite Operations:

| Operation                                                       | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| `git rebase`                                                    | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6578-L6664) |
| `git cherry-pick`                                               | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6675-L6718) |
| `git stash` / `git stash pop`                                  | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6758-L6824) |
| `git merge --squash`                                            | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6729-L6757) |
| `git reset --soft`                                              | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git reset --mixed`                                            | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git reset --hard`                                              | ✅      | Attribution preserved for commits that remain in history. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6504-L6577) |
| `git merge` (merge commit)                                      | ✅      | Attribution preserved. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6475-L6485) |
| `git commit --amend`                                            | ✅      | Attribution preserved, including unstaged and partially staged changes. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6486-L6503) |
| `git checkout` / `git switch` (branches)                       | ✅      | Attribution follows the working tree across branch changes. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L966-L978) |
| `git pull` (fast-forward / `--rebase`)                          | ✅      | Attribution preserved, including autostashed changes. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/daemon.rs#L6825-L6874) |
| `git push` / `git fetch`                                       | ✅      | Attribution notes synced to/from the remote. [View Code](https://github.com/git-ai-project/git-ai/blob/f3da782e93c492303e44d14805179123d1740e7f/src/commands/hooks/push_hooks.rs#L7-L30) |
| `git mv`                                                        | ❌      | Renames are not yet tracked; attribution does not follow the moved file.     |
| `git filter-branch` / `git filter-repo`                        | ❌      | Bulk history rewrites are not tracked.                                        |
| `git replace`                                                  | ❌      | Object replacements are not tracked.                                         |


GitHub, GitLab, Bitbucket, Azure DevOps:

| Capability                                                      | Status | Notes                                                                        |
| --------------------------------------------------------------- | ------ | ---------------------------------------------------------------------------- |
| Squash and Merge                                                | ✅      | Requires [Git AI for Teams](https://calendly.com/d/cxjh-z79-ktm/meeting-with-git-ai-authors) or [Open Source CI Actions](https://usegitai.com/docs/guides/ci-workflows) to preserve attribution. |
| Rebase and Merge                                                | ✅      | Requires [Git AI for Teams](https://calendly.com/d/cxjh-z79-ktm/meeting-with-git-ai-authors) or [Open Source CI Actions](https://usegitai.com/docs/guides/ci-workflows) to preserve attribution. |




## License
Apache 2.0
