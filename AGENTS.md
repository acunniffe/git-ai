## Non-Negotiable Rules

These are hard constraints. Violating any of them will get a PR rejected outright.

1. **All Git integration/processing is trace2-driven -- we do NOT wrap git.** All git command processing is based on trace2, so EVERYTHING we do must be fully async. As a result, we cannot rely on repo/git state at processing time being reflective of the state whenever the given operation actually occurred. We have a highly latency-sensitive trace2 ingestion flow that gathers/estimates the minimal possible state and orders events for later async processing.

2. **No new work on the critical ingestion path.** You CANNOT add git spawns, git object lookups, ref checks, etc. in the critical ingestion path of the daemon. It is EXTREMELY latency-sensitive -- even additional file reads have meaningful overhead on this path, which is sensitive to sub-millisecond latency increases.

3. **No non-constant-time git work anywhere.** You CANNOT add git spawns, git object lookups, ref checks, etc. that are not constant-time (e.g., must be O(2)/O(3), not O(n)). You cannot have any operation that calls git per commit, per file, per object, or per ref -- that is not constant time. You can still call git outside of latency-sensitive paths, but instead of calling it once per item, find a constant-time alternative: a plumbing command, a command that accepts batched inputs, etc., so you avoid the massive overhead of N process spawns. Rule of thumb: if you're calling git or looking up objects/refs "for each `<thing>`," you're doing it wrong.

4. **Unbounded git spawns are an automatic rejection.** Changes/PRs with unbounded git spawns will NOT be accepted.

5. **Reuse existing code.** This is a large codebase. For nearly any operation you need, there is almost certainly a helper function already available and tested. Reuse code as much as practical -- it also keeps diffs small. PRs that fail to reuse existing code where applicable will be rejected immediately.

6. **Follow STRICT TDD.** PRs that are not clearly TDD-driven with high-quality `TestRepo`-based tests will be rejected immediately.

## Build & Test Commands

```bash
# Install a git-ai debug build for local dev on the system so that all git commands will route through it.
# Installs to the same location as real release builds, so it overrides system-wide. It also runs `git-ai install`
# and restarts the daemon to ensure all latest code changes are fully installed and propagated system-wide.
# Use this for trying out changes locally -- do not use any other approaches for runing git-ai locally. They will
# not work, interfere, and break things.
task dev

# Build (only use this for checking that your changes compile)
task build

# Test (use these commands to run the test suite -- these calls are optimized for your system; all flags/args can be combined)
task test # Run the full test suite
task test TEST_FILTER=foo # run specific test
task test NO_CAPTURE=true # Run with Cargo's --no-capture flag
task test EXTRA_TEST_BINARY_ARGS="--ignored" # ignored / exact / other flags
task test CARGO_TEST_ARGS="--lib" # cargo-level flags (rare)

# Lint & Format
task lint
task fmt

# Snapshot management (insta crate)
cargo insta review                       # interactively review snapshot changes
cargo insta accept                       # accept all pending snapshots
```

## PR Workflow

Before opening a PR, make sure to run `task lint` and `task fmt` and resolve any formatting/lint issues as they will fail in CI.

When opening a PR, make sure to monitor the ubuntu-based CI jobs first. They are the fastest (roughly 15mins) and if they fail, you should quickly iterate based on those failures and update the PR -- iterating there until those jobs are all green. Additionally, while you're checking on the ubuntu-based jobs, our automated PR review bot, Devin, should have had time to leave feedback. Make sure to read all of Devin's PR review feedback commits and address them. Address them means review, understand, evaluate, and fix if necessary or comment with your thoughts if you don't the feedback is a real issue. Once the lint, fmt, and Ubuntu-based tests have passed and you have addressed all Devin PR review feedback, you can stop monitoring CI for the Mac (~35mins) and Windows (up to 3.5 hours) checks unless the user has explicitly asked for you to wait for those or you're working on a specific OS-based bug.

## Architecture

### Binary dispatch (src/main.rs)

A single binary serves two roles based on `argv[0]`:
- **`argv[0] == "git"`** --> `commands::git_handlers::handle_git()` -- proxies to real git with pre/post hooks per subcommand
- **`argv[0] == "git-ai"`** --> `commands::git_ai_handlers::handle_git_ai()` -- direct subcommands (checkpoint, blame, diff, status, search, etc.)
- **Debug-only shortcut**: When `cfg!(debug_assertions)` and `GIT_AI=git` env var is set, forces git proxy mode regardless of binary name. Most integration tests no longer rely on this: they run the real git binary with trace2 wired to a per-test daemon (production-like), using the proxy env only in a few special cases.

### Core data flow: checkpoint --> working log --> authorship note

1. **Checkpoint**: An AI coding agent calls `git-ai checkpoint <agent>` with hook input (typically JSON via stdin) before AND after it edits a file. The corresponding agent preset (`src/commands/checkpoint_agent/agent_presets.rs`) extracts edited file paths, transcript, and model info. The checkpoint processor diffs the file against HEAD's version or the last-checkpointed value of that file and compute character-level attributions. The combination of pre and post file edit checkpoints is what allows us to know exactly what the AI changed (since we can compare the before and after). There are 3 main types of checkpoints in git-ai:
    * Plain or legacy `human`: only due to legacy, it's still called `human` as it used to mean "human" edited files, but since we migrated to an explicit Human checkpoint (now called `known_human`), this checkpoint represents 'untracked' changes. This is the checkpoint that AI agent presets invoke to take the before edit snapshots. Changes caught by these checkpoints do get explicit attestations in the final authorship notes (they are basically holes in the data) and stats recognize them as untracked. For testing, invoke by calling `git-ai checkpoint human` (for unscoped) or `git-ai checkpoint human /path/to/file` (for scoped).
    * Known human (`known_human`) checkpoints: this is the 'real' Human checkpoint. These are never called by the AI agent presets and are only invoked by our IDE/editor extensions that recognize when a change has actually been made by the human by typing, etc. For testing, invoke via `git-ai checkpoint mock_known_human` (for unscoped) or `git-ai checkpoint mock_known_human /path/to/file` (for scoped).
    * AI checkpoint (`ai_agent`) checkpoints: this is the AI checkpoint that explicitly associates the captured changes with the particular AI agent and session. This is the checkpoint taht AI agent presets invoke to take the after edit snapshots. For testing, invoke via `git-ai checkpoint mock_ai` (for unscoped) or `git-ai checkpoint mock_ai /path/to/file` (for scoped).

2. **Working log**: Checkpoint data is written to `.git/ai/working_logs/<base_commit>/` as JSON files. Each working log entry records per-file line attributions (which ranges are AI vs known human vs untracked (legacy human)) and session metadata.

3. **Post-commit authorship**: After `git commit`, the daemon reads working logs, generates an `AuthorshipLog` (schema version `authorship/3.0.0`), and stores it as a Git Note under `refs/notes/ai`. The authorship log contains attestation entries (hash --> line ranges) and a metadata section with prompt records.

4. **Rewrite tracking**: The daemon ingests git trace2 event streams to learn which git commands ran, establishes exact ref transitions via a reflog cursor model (`src/daemon/ref_cursor.rs`), and migrates authorship notes/working logs through `src/authorship/rewrite.rs` (`RewriteEvent` + `handle_rewrite_event`) plus the per-operation modules (`rewrite_reset.rs`, `rewrite_stash.rs`, `rewrite_revert.rs`, `rewrite_cherry_pick.rs`). See `docs/rewrite-ops-spec.md` and `docs/daemon-trace2-ingestion-spec.md`.

### Daemon trace2 ingestion (src/daemon.rs, src/daemon/)

The git proxy is a thin passthrough (`src/commands/git_handlers.rs`); all attribution side effects run in the shared daemon, driven by trace2:

- Socket listener receives trace2 JSON frames; definitely-read-only roots are filtered out
- `TraceNormalizer` (src/daemon/trace_normalizer.rs) groups frames by root sid into a `NormalizedCommand`
- A per-repo-family actor (src/daemon/family_actor.rs) sequences commands and checkpoints in order
- `RefCursor::enrich_command` (src/daemon/ref_cursor.rs) consumes cursor-bounded reflog entries to fill exact `ref_changes`; commands without a cursor or immutable argv OIDs fail closed for attribution
- Analyzers (src/daemon/analyzers/history.rs) classify enriched commands into semantic events that drive post-commit authorship and rewrite-note migration

Signal forwarding: On Unix, the git proxy installs signal handlers (SIGTERM, SIGINT, SIGHUP, SIGQUIT) that forward to the child git process group.

### Config singleton

`Config` is a global `OnceLock` singleton accessed via `Config::get()`. It reads from `~/.git-ai/config.json`. In tests, `GIT_AI_TEST_CONFIG_PATCH` env var allows overriding specific config fields without a real config file. Feature flags follow precedence: environment vars (`GIT_AI_*` prefix via `envy`) > config file > defaults.

Feature flags have separate debug/release defaults defined via the `define_feature_flags!` macro in `src/feature_flags.rs`. Currently: `auth_keyring` (false/false), `transcript_streaming` (true/true), `transcript_sweep` (true/true), `checkpoint_debug_log` (false/false), `daemon_log_upload` (true/true).

**Internal directory (`GIT_AI_INTERNAL_DIR`)**: The canonical helper `config::internal_dir_path()` returns git-ai's internal state directory, defaulting to `~/.git-ai/internal`. When `GIT_AI_INTERNAL_DIR` is set to a non-empty value, that value IS the internal directory verbatim (no extra `internal` segment appended); empty or unset preserves the default byte-for-byte. Everything machine-local routes through this helper: the 4 SQLite DBs (`db`, `metrics-db`, `notes-db`, `transcripts-db`), daemon runtime files (`daemon/control.sock`, `daemon/trace2.sock`, `daemon/daemon.lock`, `daemon/daemon.pid.json`, `daemon/logs/`), `distinct_id`, `update_check`, checkpoint-debug-logs, and debug-self-checks. Purpose: machines sharing an NFS home set a unique machine-local `GIT_AI_INTERNAL_DIR` each so they never collide on sockets/locks/DBs. Precedence for the daemon internal dir (`DaemonConfig::from_env_or_default_paths`): `GIT_AI_INTERNAL_DIR` > `GIT_AI_DAEMON_HOME` > default, with the explicit `GIT_AI_DAEMON_CONTROL_SOCKET`/`GIT_AI_DAEMON_TRACE_SOCKET` overrides still applied on top (most specific). The detached daemon child inherits the parent env, so client, git proxy, and daemon all compute identical paths. **Shared (NOT relocated):** `config.json`, `skills/`, and `internal/credentials` (OAuth tokens, so login is shared across machines) all stay on the `~/.git-ai` path regardless of `GIT_AI_INTERNAL_DIR`.

### Error handling

`GitAiError` enum in `src/error.rs` -- not `thiserror`-based, uses manual `Display`/`From` impls. Variants: `GitCliError` (captures exit code + stderr + args), `IoError`, `JsonError`, `SqliteError`, `PresetError`, `Generic`, `GixError`.

## Test Infrastructure

### Integration test framework (tests/integration/repos/)

Tests create real git repositories and run against a shared test daemon pool (trace2-driven, like production). The test framework has three key files:

- **`tests/integration/repos/test_repo.rs`** -- `TestRepo` struct: creates temp git repos, runs git/git-ai commands as subprocesses wired to a per-test daemon (control + trace sockets), and provides explicit daemon sync before assertions. Uses `get_binary_path()` which auto-compiles the binary via a `OnceLock`.

- **`tests/integration/repos/test_file.rs`** -- `TestFile` fluent API for setting file contents with attribution expectations. The `lines!` macro + `.ai()` / `.human()` / `.unattributed_human()` trait methods create `ExpectedLine` vectors. `assert_lines_and_blame()` validates both content and AI/human attribution.

- **`tests/integration/repos/mod.rs`** -- `subdir_test_variants!` macro auto-generates two test variants: one from a subdirectory and one using `-C` flag, to verify repository discovery works from any CWD.

Simple test pattern (using all standard helpers):
```rust
#[test]
fn test_using_test_repo() {
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");
    file.set_contents(lines!["Line 1", "AI line".ai()]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    file.assert_lines_and_blame(lines!["Line 1".human(), "AI line".ai()]);
}
```

For certain test cases, especially where you are focused on testing specific checkpoint or attribution behavior, do NOT use the `file.set_contents` helper as it has a very specific (and unrealistic) ai vs human checkpointing flow that first sets file content to all the human values with explicit placeholders for the lines that are AI, calls a known human checkpoint, and then replaces the AI lines with their real values and calls the AI checkpoint after. As you can imagine, if you really want to test nuances of checkpointing, this is problematic. In those cases, explicitly write the file using standard Rust file write utils and explicitly call the ai vs human checkpoints mocking the real pre/post checkpointing flow using `mock_known_human` for explicit/known human changes, `human` for untracked changes, and `mock_ai` for AI changes. Example with custom writes+checkpointing for when you really care about exact replication of issues or testing checkpointing/attribution internals or any time the exact flow, order, etc. of checkpoints is relevant:

```rust
#[test]
fn test_using_test_repo_with_custom_checkpoints() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.md");

    let initial = "\
Untracked line
";
    fs::write(&file_path, initial).unwrap();
    // Example of a completely untracked edit where we didn't fire a checkpoint call at all
    repo.stage_all_and_commit("Initial commit").unwrap();
    // Assert after every commit
    let mut file = repo.filename("example.md");
    // ALWAYS use the helper to assert the lines post-commit AND make sure to always assert line-level after EVERY commit for EVERY test you EVER right. This is CRUCIAL.
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(), // 'untracked'
    ]);


    let second_edit = "\
Untracked line
Human line
";
    fs::write(&file_path, second_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.md"])
        .unwrap();

    // Explicit add call (very useful to test partial staging scenarios)
    repo.git(&["add", "."]).unwrap();
    // Explicit commit
    repo.commit("Second commit").unwrap();
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(), // still 'untracked'
        "Human line".human(), // known human
    ]);

    let third_edit = "\
Untracked line
Human line
AI line
";
    fs::write(&file_path, third_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.md"])
        .unwrap();
    // Example of a completely untracked edit where we didn't fire a checkpoint call at all
    repo.stage_all_and_commit("Third commit").unwrap();
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(), // 'untracked'
        "Human line".human(), // known human
        "AI line".ai(), // AI line
    ]);

    let fourth_edit = "\
Untracked line
Human line
AI line
Another untracked line
";
    fs::write(&file_path, fourth_edit).unwrap();
    // Mocking an AI agent preset's pre edit checkpoint, which all the AI agent presets do to exclude
    // changes made by something else (impossible to know what) before the AI makes its own edit. We mock
    // that by calling a 'legacy human' (untracked) checkpoint.
    repo.git_ai(&["checkpoint", "human", "example.md"])
        .unwrap();
    
    let fifth_edit = "\
Untracked line
Human line
AI line
Another untracked line
Another AI line
";
    fs::write(&file_path, fifth_edit).unwrap();
    // Mocking an AI agent preset's post edit checkpoint, which all the AI agent presets do to capture the changes made by the AI.
    // We mock that by calling a 'mock_ai' checkpoint.
    repo.git_ai(&["checkpoint", "mock_ai", "example.md"])
        .unwrap();
    repo.stage_all_and_commit("Fourth commit").unwrap();
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(), // 'untracked'
        "Human line".human(), // known human
        "AI line".ai(), // AI line
        "Another untracked line".unattributed_human(), // 'untracked'
        "Another AI line".ai(), // AI line
    ]);
}
```

### Test isolation

- Each `TestRepo` gets a random temp directory and a separate `GIT_AI_TEST_DB_PATH`.
- `GIT_AI_TEST_CONFIG_PATCH` env var passes `ConfigPatch` JSON to override config in subprocess.
- Background flush is skipped when `GIT_AI_TEST_DB_PATH` is set (prevents race conditions on temp dir cleanup).
- Use `#[serial_test::serial]` for tests that conflict on shared env vars. Do your best to avoid needing this though by using the config patch, etc.

### Snapshot tests

Uses `insta` crate. Snapshots live in `tests/integration/snapshots/` and `tests/integration/repos/snapshots/`. Run `cargo insta review` to update.

## Key Conventions

- **Rust 2024 edition** with Rust 1.93.0 -- uses let-chains (`if let Some(x) = foo && condition`), which are stable in edition 2024.
- **Git CLI only**: All git operations use `std::process::Command` to call the real git binary. The `git2`/libgit2 dependency has been fully removed. The binary acts as a transparent git proxy.
- **`debug_log()`** for conditional debug output: prints `[git-ai]` prefixed messages to stderr when `cfg!(debug_assertions)` or `GIT_AI_DEBUG=1`. Set `GIT_AI_DEBUG=0` to suppress in debug builds.
- **`GIT_AI_DEBUG_PERFORMANCE=1`** (or `=2` for JSON) enables performance timing output.
- **Paths are POSIX-normalized**: `normalize_to_posix()` utility converts Windows backslashes. File paths in authorship logs and working logs always use forward slashes.
- **`GIT_AI_VERSION` constant** changes between debug/release/test modes via `cfg` attributes in `authorship_log_serialization.rs`.
- **Cross-platform**: `#[cfg(unix)]` / `#[cfg(windows)]` conditional compilation is used extensively (well over a hundred `#[cfg(windows)]` annotations across ~two dozen files) for signal handling, process creation flags (`CREATE_NO_WINDOW`), path handling, terminal detection, and named-pipe vs unix-socket transport (e.g. the daemon control/trace sockets are named pipes on Windows, so `Path::exists()` checks are gated to non-Windows).

## Optimize for Human Review

- Always write code optimized for human review. No code can be merged without a greenlight from a human, so make it easy for humans to review your code. This means clear naming, clear refactors as needed, and, most importantly, minimal and simple code. Clean, DRY, simple, maintainable code is your true north star.
- Always submit work with Graphite (if `gt` CLI is available) as a stack of pull requests, with each pull request representing a logical, self-contained chunk of the problem. This is how you present your work for human review.
- Before stopping, ensure every submitted pull request passes all CI checks and all Devin review feedback has been addressed and resolved.

## Gotchas

- **Test binary auto-compilation**: Integration tests trigger `cargo build --bin git-ai` on first test run via `OnceLock`. If you change code and run tests, the test harness recompiles. This can cause confusion if you're debugging -- the test binary is always a debug build at `target/debug/git-ai`.

- **argv[0] dispatch is load-bearing**: The binary's behavior is entirely determined by how it's invoked. In production, symlinking as `git` makes it a proxy. The `GIT_AI=git` env var forces proxy mode (debug builds only). Breaking this dispatch breaks everything.

- **Feature flag debug/release divergence**: Some flags have different debug/release defaults (see `define_feature_flags!` macro). Tests run debug builds, so a test passing in debug may behave differently in release if it depends on a flag that diverges.

- **Working log base commit**: Working logs are keyed by the HEAD commit at checkpoint time (`.git/ai/working_logs/<sha>/`). Git AI must ensure that HEAD changes update/copy over the working log accordingly.

- **Large source files**: Several core files exceed 5-10k lines. Navigate with grep, not scrolling.
