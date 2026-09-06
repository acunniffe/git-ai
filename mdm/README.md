# Git AI MDM helpers

Scripts for managed (MDM) rollouts that start the Git AI background service
when a user logs in. Without them, the daemon only starts on the first
checkpoint or `git-ai` subcommand, so trace2 events from git commands run
before that point are lost.

| Platform | Script | Mechanism | Release asset |
|---|---|---|---|
| macOS | `macos/install-login-start.sh` | LaunchAgent `com.usegitai.bg` | `git-ai-login-start-macos.sh` |
| Linux | `linux/install-login-start.sh` | systemd user unit `git-ai-bg.service` | `git-ai-login-start-linux.sh` |
| Windows | `windows/install-login-start.ps1` | Scheduled Task `\GitAI\Start bg at logon` | `git-ai-login-start-windows.ps1` |

Each release publishes the scripts at
`https://github.com/git-ai-project/git-ai/releases/latest/download/<asset>`.
Run them **after** `git-ai` is installed and `git-ai install` has run for the
user (that step writes the per-user trace2 config the daemon depends on).

## Why the scripts only start the daemon

Every script registers the idempotent `git-ai bg start` and nothing else (the
launchers retry it a few times, two seconds apart, in case a previous daemon is
still releasing its lock at login). The daemon supervises itself:

- It exits 0 immediately if a daemon is already up, and refuses to start a
  second instance while the daemon **lock** is held.
- It restarts itself after 24.5 hours of uptime and after installing a
  self-update; the exiting process spawns its successor.

A launchd `KeepAlive`, systemd `Restart=`, or task-scheduler retry would see the
launcher exit, relaunch it, hit the held lock, and loop. So the launch
definitions must never supervise the daemon, and they must not tear down the
processes left behind when `bg start` exits. `tests/mdm_scripts.rs` pins these
invariants:

- **macOS**: `AbandonProcessGroup` is `true` (launchd otherwise kills the
  daemon when `bg start` exits), `KeepAlive` is `false`, `RunAtLoad` is `true`.
- **Linux**: `Type=oneshot` with `RemainAfterExit=yes` keeps the unit and its
  cgroup (where the daemon lives) active until logout; no `Restart=`.
- **Windows**: `-MultipleInstances IgnoreNew` so a second logon trigger cannot
  race the daemon, and the execution time limit is disabled so Task Scheduler
  never ends the instance.

## Usage

All three scripts share one contract:

```
install-login-start [install] [--env KEY=VALUE]... [--bin PATH] [--no-start]
install-login-start --uninstall
```

- `--env KEY=VALUE` (repeatable) is written into the launch definition and
  reaches the daemon, e.g. `HTTPS_PROXY`, `GIT_AI_API_BASE_URL`.
- `--bin PATH` points at a non-default `git-ai` binary. The path is passed to
  the launcher through the `GIT_AI_LOGIN_START_BIN` environment variable, never
  interpolated into a command line, so any path works.
- `--no-start` registers without starting now; the daemon starts at next login.
- `--uninstall` removes the registration. On macOS and Windows the running
  daemon is left alone; on Linux stopping the unit stops its cgroup and thus
  the daemon (git-ai respawns it lazily on the next checkpoint).
- The `.sh` scripts also take `--system` (run as root) to install for all users
  under `/Library/LaunchAgents` or `/etc/systemd/user`. Per-user is the default
  and needs no elevation.

### macOS

Run as the console user, exactly like the install script itself:

```sh
su - "$(/usr/bin/stat -f%Su /dev/console)" -c \
  'curl -fsSL https://github.com/git-ai-project/git-ai/releases/latest/download/git-ai-login-start-macos.sh | sh'
```

The agent's `PATH` is set explicitly (`/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`)
because launchd does not load the user's shell profile; pass `--env PATH=...`
to override. `bg start` output goes to `~/.git-ai/internal/daemon/logs/login-start.log`.

### Windows

Run in the user's context from a non-elevated shell, the same requirement as
the MSI and `install.ps1`:

```powershell
irm https://github.com/git-ai-project/git-ai/releases/latest/download/git-ai-login-start-windows.ps1 | iex
```

Piped installs use the defaults. To pass flags, download the file and run it:
`.\git-ai-login-start-windows.ps1 --env HTTPS_PROXY=http://proxy:3128`. The task
runs a small launcher at `%USERPROFILE%\.git-ai\login\start-bg.ps1` that sets
the requested variables and calls `bg start`.

### Linux

```sh
curl -fsSL https://github.com/git-ai-project/git-ai/releases/latest/download/git-ai-login-start-linux.sh | sh
```

Per-user installs enable and start the unit immediately, which needs a systemd
user manager (a graphical login, or `loginctl enable-linger <user>` on headless
hosts). `--system` writes `/etc/systemd/user/git-ai-bg.service` and enables it
globally for every user's next login.

## Verifying

Log in, then run `git-ai bg status` from inside any git repository before
issuing other git commands. It should report a healthy daemon. Platform state:

- macOS: `launchctl print gui/$(id -u)/com.usegitai.bg`
- Linux: `systemctl --user status git-ai-bg.service` (expect `active (exited)`)
- Windows: `Get-ScheduledTaskInfo -TaskPath '\GitAI\' -TaskName 'Start bg at logon'`

## Tests

- `tests/mdm_scripts.rs`: static invariants above (`task test CARGO_TEST_ARGS="--test mdm_scripts"`).
- `scripts/mdm/test-login-start.{sh,ps1}`: end-to-end on each OS. Registers the
  login start, simulates login, and checks the daemon survives the launcher
  exiting, restarts itself on schedule, and (nightly) self-updates from an older
  release while registered. Run per PR by `.github/workflows/mdm-login-start.yml`
  and nightly against the published release by `mdm-login-start-nightly.yml`.
