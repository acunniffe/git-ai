# git-ai Hook: `post_notes_updated`

## Overview

When git-ai seals attribution notes on one or more commits, it fires the
`post_notes_updated` hook. This is the primary integration point for
exporting attribution data to external systems.

## Configuration

Register shell commands via `git-ai config`:

```bash
git-ai config --add git_ai_hooks.post_notes_updated "./my-hook.sh"
```

Multiple commands can be registered; they run in parallel.

## Payload Schema (v2)

The hook receives a **JSON array** of 1..N note entries on **stdin**.
Each entry has the following fields:

| Field | Type | Description |
|---|---|---|
| `schema_version` | `u32` | Schema version. Currently `2`. |
| `commit_sha` | `string` | The commit SHA the note was sealed for. |
| `repo_url` | `string` | Remote URL of the repository (from the default remote). |
| `repo_name` | `string` | Short name derived from `repo_url` (last path segment, `.git` stripped). |
| `repo_path` | `string?` | Absolute path to the working directory. Omitted for bare repos. |
| `git_dir` | `string` | Absolute path to the `.git` directory. |
| `branch` | `string` | Current branch name (or `"unknown"`). |
| `is_default_branch` | `bool` | Whether `branch` matches the remote's default branch. |
| `note_content` | `string` | The full content of the sealed attribution note. |

**Example payload:**

```json
[
  {
    "schema_version": 2,
    "commit_sha": "abc123def456",
    "repo_url": "https://github.com/org/repo.git",
    "repo_name": "repo",
    "repo_path": "/home/user/projects/repo",
    "git_dir": "/home/user/projects/repo/.git",
    "branch": "main",
    "is_default_branch": true,
    "note_content": "..."
  }
]
```

## Execution Semantics

- **Parallelism:** All registered commands start simultaneously.
- **Timeout:** git-ai waits up to **3 seconds** for each command to
  complete, then detaches any still-running commands into a background
  thread. git-ai never blocks the commit path.
- **Best-effort:** Hook failures are logged via `tracing::debug!` but
  never propagated. Hooks are not retried.
- **stdin:** The JSON payload is written to the command's stdin.
- **stdout:** Discarded (`/dev/null`). Hooks are not a data channel back
  into git-ai.
- **stderr:** Captured and logged on non-zero exit for diagnostics.

## Versioning Policy

- **Additive changes** (new fields) keep the same `schema_version`.
  Consumers should ignore unknown fields.
- **Breaking changes** (field removal, type changes, semantic changes)
  bump `schema_version`.

## Attribution Sinks

In addition to shell hooks, git-ai supports structured **attribution
sinks** configured in `~/.git-ai/config.json`:

```json
{
  "attribution_sinks": [
    {"type": "stdout"},
    {"type": "file", "path": "/var/log/git-ai/attributions.jsonl"},
    {"type": "http", "url": "https://collector.internal/attribution", "headers": {"X-Team": "dx"}}
  ]
}
```

Sinks receive the same `AttributionEvent` payload as shell hooks, in
structured form. Failures are logged, never blocking.

| Sink type | Behavior |
|---|---|
| `stdout` | Prints the JSON array to stdout. |
| `file` | Appends one JSON line per event to the configured path. |
| `http` | POSTs the JSON array to the configured HTTPS URL with optional custom headers. |

HTTP sinks require HTTPS by default. For local development only, set
`"allow_insecure": true` on the sink to permit an `http://` URL.
