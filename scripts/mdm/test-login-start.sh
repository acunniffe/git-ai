#!/usr/bin/env bash
# End-to-end test for mdm/{macos,linux}/install-login-start.sh.
#
# Installs git-ai into $HOME, registers the login start, simulates the login
# trigger, and checks that the daemon (a) comes up, (b) survives the launcher
# exiting, (c) restarts itself on schedule while registered, and (d) the login
# mechanism stays healthy throughout. The auto-update scenario additionally
# starts from an older release and waits for the daemon to self-update.
#
# Usage: test-login-start.sh [lifecycle|auto-update]
#
# Environment:
#   BINARY_SOURCE          local (default) | release
#   GIT_AI_LOCAL_BINARY    binary to install when BINARY_SOURCE=local
#   GIT_AI_RELEASE_TAG     release tag to install when BINARY_SOURCE=release
#                          (empty = latest); required for auto-update
#   LATEST_TAG             tag the auto-update scenario must reach
#   MDM_TEST_LOG_DIR       where daemon logs are copied (default $RUNNER_TEMP/mdm-logs)
#   MDM_TEST_ISOLATED_HOME set to 1 when $HOME is a scratch directory: passes
#                          --bin/--env HOME so the launcher does not resolve the
#                          real home. On Linux also export XDG_CONFIG_HOME to the
#                          real ~/.config so the user manager sees the unit.
set -euo pipefail

SCENARIO="${1:-lifecycle}"
BINARY_SOURCE="${BINARY_SOURCE:-local}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_OUT="${MDM_TEST_LOG_DIR:-${RUNNER_TEMP:-/tmp}/mdm-logs}"
BIN="$HOME/.git-ai/bin/git-ai"
DAEMON_DIR="$HOME/.git-ai/internal/daemon"
LABEL="com.usegitai.bg"
UNIT="git-ai-bg.service"

case "$(uname -s)" in
  Darwin) OS=macos ;;
  Linux) OS=linux ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
MDM_SCRIPT="$REPO_ROOT/mdm/$OS/install-login-start.sh"

log() { printf '[mdm-test] %s\n' "$*"; }
fail() { printf '[mdm-test] FAIL: %s\n' "$*" >&2; exit 1; }

# --- helpers -----------------------------------------------------------------

install_binary() {
  case "$BINARY_SOURCE" in
    local)
      [ -x "${GIT_AI_LOCAL_BINARY:-}" ] || fail "GIT_AI_LOCAL_BINARY must point at a built git-ai"
      GIT_AI_LOCAL_BINARY="$GIT_AI_LOCAL_BINARY" bash "$REPO_ROOT/install.sh"
      ;;
    release)
      if [ -n "${GIT_AI_RELEASE_TAG:-}" ]; then
        GIT_AI_RELEASE_TAG="$GIT_AI_RELEASE_TAG" bash "$REPO_ROOT/install.sh"
      else
        bash "$REPO_ROOT/install.sh"
      fi
      ;;
    *) fail "BINARY_SOURCE must be local or release" ;;
  esac
  [ -x "$BIN" ] || fail "$BIN missing after install"
  log "installed $("$BIN" --version | head -1)"
}

installed_version() {
  "$BIN" --version | head -1 | awk '{print $1}' | sed 's/^v//'
}

daemon_pid() {
  jq -r '.pid // empty' "$DAEMON_DIR/daemon.pid.json" 2>/dev/null || true
}

pid_alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }

STATUS_REPO="$(mktemp -d)"
git -C "$STATUS_REPO" init -q

daemon_up() {
  local pid
  pid="$(daemon_pid)"
  pid_alive "$pid" && (cd "$STATUS_REPO" && "$BIN" bg status >/dev/null 2>&1)
}

# wait_for <seconds> <description> <command...>
wait_for() {
  local secs="$1" what="$2"; shift 2
  local deadline=$((SECONDS + secs))
  until "$@"; do
    [ "$SECONDS" -lt "$deadline" ] || fail "timed out after ${secs}s waiting for $what"
    sleep 1
  done
  log "$what"
}

pid_changed_from() { local now; now="$(daemon_pid)"; [ -n "$now" ] && [ "$now" != "$1" ]; }

# Only daemons under this HOME: a developer box may run its own git-ai daemon.
no_daemon_process() { ! pgrep -f "^$HOME/.*git-ai bg run" >/dev/null; }

# `bg shutdown` returns before the old process has released the daemon lock;
# a login start racing that window would have to retry.
stop_daemon() {
  "$BIN" bg shutdown >/dev/null 2>&1 || true
  wait_for 30 "previous daemon exited" no_daemon_process
}

daemon_command_line() { ps -p "$(daemon_pid)" -o args= 2>/dev/null || true; }

mdm_script_args() {
  if [ "${MDM_TEST_ISOLATED_HOME:-0}" = "1" ]; then
    printf '%s\n' --bin "$BIN" --env "HOME=$HOME"
  fi
}

run_mdm_script() {
  local extra=()
  while IFS= read -r line; do extra+=("$line"); done < <(mdm_script_args)
  # Isolation flags go first so a caller's own --bin wins (last one wins).
  sh "$MDM_SCRIPT" "${extra[@]+"${extra[@]}"}" "$@"
}

registered() {
  case "$OS" in
    macos) [ -f "$HOME/Library/LaunchAgents/$LABEL.plist" ] && launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 ;;
    linux) [ -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$UNIT" ] && [ "$(systemctl --user is-enabled "$UNIT" 2>/dev/null)" = "enabled" ] ;;
  esac
}

# Re-fires the login trigger while the daemon is up; must be a no-op.
trigger_login() {
  case "$OS" in
    macos) launchctl kickstart "gui/$(id -u)/$LABEL" ;;
    # `start` is a no-op while the oneshot is active, so also run the launcher
    # directly to prove idempotency.
    linux) systemctl --user start "$UNIT"; "$BIN" bg start ;;
  esac
}

launcher_exited() { launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null | grep -q "state = not running"; }

mechanism_sane() {
  case "$OS" in
    macos)
      launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || fail "launchd job missing"
      # The daemon answers before `bg start` returns and launchd notices; give
      # the launcher a moment to exit rather than asserting instantly.
      wait_for 20 "launcher exited" launcher_exited
      if launchctl print "gui/$(id -u)/$LABEL" | grep -E "last exit code = [1-9]"; then
        fail "launchd job exited non-zero"
      fi
      ;;
    linux)
      [ "$(systemctl --user is-active "$UNIT")" = "active" ] || fail "unit not active"
      ! systemctl --user is-failed --quiet "$UNIT" || fail "unit failed"
      # Self-restarted daemons must stay in the unit's cgroup so logout stops them.
      grep -q "$UNIT" "/proc/$(daemon_pid)/cgroup" || fail "daemon $(daemon_pid) escaped the unit cgroup"
      ;;
  esac
  log "login mechanism healthy"
}

lint_definition() {
  case "$OS" in
    macos) plutil -lint "$HOME/Library/LaunchAgents/$LABEL.plist" ;;
    linux)
      if command -v systemd-analyze >/dev/null 2>&1; then
        systemd-analyze --user verify "$UNIT" || log "WARN: systemd-analyze verify reported issues"
      fi
      ;;
  esac
}

daemon_started_versions() {
  grep -ho 'daemon started .*version="[^"]*"' "$DAEMON_DIR"/logs/*.log 2>/dev/null | sed 's/.*version="//; s/"$//' || true
}

cleanup() {
  local rc=$?
  set +e
  mkdir -p "$LOG_OUT"
  cp -R "$DAEMON_DIR/logs" "$LOG_OUT/daemon-logs" 2>/dev/null
  case "$OS" in
    macos) launchctl print "gui/$(id -u)/$LABEL" >"$LOG_OUT/launchctl-print.txt" 2>&1 ;;
    linux) systemctl --user status "$UNIT" --no-pager >"$LOG_OUT/systemctl-status.txt" 2>&1 ;;
  esac
  run_mdm_script --uninstall >/dev/null 2>&1
  [ -x "$BIN" ] && "$BIN" bg shutdown >/dev/null 2>&1
  rm -rf "$STATUS_REPO"
  exit "$rc"
}
trap cleanup EXIT

# --- scenarios ---------------------------------------------------------------

register_and_wait_for_daemon() {
  run_mdm_script "$@"
  registered || fail "login start not registered after install"
  wait_for 30 "daemon up after login trigger" daemon_up
}

scenario_lifecycle() {
  install_binary
  # Keep the uptime restart deterministic: no network update checks.
  "$BIN" config set disable_auto_updates true
  stop_daemon

  register_and_wait_for_daemon \
    --env GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL=5 \
    --env GIT_AI_DAEMON_MAX_UPTIME_SECS=25
  local pid1; pid1="$(daemon_pid)"

  sleep 10
  pid_alive "$pid1" || fail "daemon $pid1 died after the launcher exited (process group torn down?)"
  daemon_up || fail "daemon not answering after launcher exit"
  log "daemon survived launcher exit"

  # Max uptime (25s) is past the survival check above but well inside this wait.
  wait_for 60 "daemon restarted itself on schedule" pid_changed_from "$pid1"
  local pid2; pid2="$(daemon_pid)"
  wait_for 15 "restarted daemon healthy" daemon_up
  mechanism_sane

  trigger_login
  sleep 3
  daemon_up || fail "daemon unhealthy after re-trigger"
  [ "$(daemon_pid)" = "$pid2" ] || fail "re-triggering login restarted the daemon (pid $pid2 -> $(daemon_pid))"
  mechanism_sane
  log "re-triggered login start was a no-op"

  lint_definition

  run_mdm_script --uninstall
  ! registered || fail "login start still registered after --uninstall"
  log "uninstall clean"

  stop_daemon
  scenario_unusual_binary_path
  scenario_launcher_retry
}

# --bin must cope with every path an admin might install to: spaces, quotes,
# parentheses, percent signs, non-ASCII.
scenario_unusual_binary_path() {
  local dir="$HOME/mdm test (weird) 100% 'quoted' ünïcode"
  mkdir -p "$dir"
  cp "$BIN" "$dir/git-ai"

  run_mdm_script --bin "$dir/git-ai"
  registered || fail "login start not registered with unusual --bin"
  wait_for 30 "daemon up from unusual binary path" daemon_up
  case "$(daemon_command_line)" in
    *"$dir/git-ai"*) log "daemon runs from the unusual path" ;;
    *) fail "daemon command line does not reference $dir: $(daemon_command_line)" ;;
  esac
  mechanism_sane

  run_mdm_script --uninstall
  stop_daemon
}

# Writes a fake git-ai that fails until its Nth invocation, then hands off to
# the real binary. Attempts are counted in $2.
write_fake_binary() {
  local path="$1" attempts="$2" succeed_at="$3"
  cat >"$path" <<EOF
#!/bin/sh
n=\$(cat "$attempts" 2>/dev/null || echo 0); n=\$((n + 1)); echo "\$n" >"$attempts"
[ "\$n" -ge $succeed_at ] || exit 1
exec "$BIN" "\$@"
EOF
  chmod +x "$path"
}

mechanism_failed() {
  case "$OS" in
    macos) launchctl print "gui/$(id -u)/$LABEL" | grep -Eq "last exit code = [1-9]" ;;
    linux) systemctl --user is-failed --quiet "$UNIT" ;;
  esac
}

# Runs the registered launcher once; the login mechanism itself must report failure.
trigger_login_expect_failure() {
  case "$OS" in
    macos) launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/$LABEL.plist" ;;
    linux) ! systemctl --user start "$UNIT" ;;
  esac
}

# The launcher retries bg start a few times: transient failures must end in a
# healthy daemon, and persistent failure must surface as a failed login start.
scenario_launcher_retry() {
  local fake_dir="$HOME/mdm-fake" attempts="$HOME/mdm-fake/attempts"
  mkdir -p "$fake_dir"

  write_fake_binary "$fake_dir/git-ai" "$attempts" 3
  run_mdm_script --bin "$fake_dir/git-ai"
  wait_for 45 "daemon up after transient launcher failures" daemon_up
  [ "$(cat "$attempts")" = 3 ] || fail "expected 3 attempts, got $(cat "$attempts")"
  mechanism_sane
  run_mdm_script --uninstall
  stop_daemon

  rm -f "$attempts"
  write_fake_binary "$fake_dir/git-ai" "$attempts" 99
  run_mdm_script --bin "$fake_dir/git-ai" --no-start
  trigger_login_expect_failure
  wait_for 45 "persistent launcher failure reported by the login mechanism" mechanism_failed
  [ "$(cat "$attempts")" = 5 ] || fail "expected 5 attempts, got $(cat "$attempts")"
  daemon_up && fail "no daemon should be running after persistent failure"
  run_mdm_script --uninstall
  [ "$OS" != linux ] || systemctl --user reset-failed "$UNIT" 2>/dev/null || true
  log "launcher retry semantics verified"
}

scenario_auto_update() {
  [ "$BINARY_SOURCE" = "release" ] || fail "auto-update needs BINARY_SOURCE=release"
  [ -n "${GIT_AI_RELEASE_TAG:-}" ] || fail "auto-update needs GIT_AI_RELEASE_TAG (an older release)"
  [ -n "${LATEST_TAG:-}" ] || fail "auto-update needs LATEST_TAG"
  local latest="${LATEST_TAG#v}"

  install_binary
  local before; before="$(installed_version)"
  [ "$before" != "$latest" ] || fail "GIT_AI_RELEASE_TAG must be older than LATEST_TAG"
  stop_daemon

  register_and_wait_for_daemon --env GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL=10
  local pid1; pid1="$(daemon_pid)"

  version_is_latest() { [ "$(installed_version)" = "$latest" ]; }
  wait_for 240 "binary updated to $latest" version_is_latest
  wait_for 120 "daemon restarted after update" pid_changed_from "$pid1"
  wait_for 30 "updated daemon healthy" daemon_up
  daemon_started_versions | grep -qx "$latest" || fail "no 'daemon started' log line for version $latest"
  mechanism_sane

  run_mdm_script --uninstall
  ! registered || fail "login start still registered after --uninstall"
  log "auto-update $before -> $latest completed under login start"
}

case "$SCENARIO" in
  lifecycle) scenario_lifecycle ;;
  auto-update) scenario_auto_update ;;
  *) fail "unknown scenario: $SCENARIO" ;;
esac
log "PASS $SCENARIO on $OS"
