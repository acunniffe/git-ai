#!/bin/sh
# Register a systemd user unit that runs `git-ai bg start` when the user logs
# in, so the Git AI background service is up before the first git command.
#
# The unit only *starts* the daemon. It never supervises it: the daemon
# restarts and updates itself, and a second instance exits because the daemon
# lock is held. See mdm/README.md for the invariants this file must keep.
set -eu

UNIT="git-ai-bg.service"
DEFAULT_BIN='%h/.git-ai/bin/git-ai'

MODE="install"
SYSTEM=0
START=1
BIN=""
ENV_LINES=""

usage() {
  cat <<'USAGE'
Usage:
  install-login-start.sh [install] [--env KEY=VALUE]... [--bin PATH] [--no-start] [--system]
  install-login-start.sh --uninstall [--system]

Registers a systemd user unit (git-ai-bg.service) that runs `git-ai bg start`
at login. Per-user by default (~/.config/systemd/user, enabled and started
now); --system installs to /etc/systemd/user and enables it for every user's
next login.
USAGE
}

fail() {
  echo "git-ai login-start: $*" >&2
  exit 1
}

add_env() {
  if ! printf '%s' "$1" | grep -Eq '^[A-Za-z_][A-Za-z0-9_]*='; then
    fail "--env expects KEY=VALUE, got '$1'"
  fi
  # systemd unquotes Environment= values; wrap in double quotes with escapes so
  # spaces and quotes survive.
  escaped="$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
  ENV_LINES="${ENV_LINES}Environment=\"${escaped}\"
"
}

while [ $# -gt 0 ]; do
  case "$1" in
    install) ;;
    --uninstall) MODE="uninstall" ;;
    --system) SYSTEM=1 ;;
    --no-start) START=0 ;;
    --bin) shift; [ $# -gt 0 ] || fail "--bin requires a path"; BIN="$1" ;;
    --bin=*) BIN="${1#--bin=}" ;;
    --env) shift; [ $# -gt 0 ] || fail "--env requires KEY=VALUE"; add_env "$1" ;;
    --env=*) add_env "${1#--env=}" ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" ;;
  esac
  shift
done

if [ "$SYSTEM" -eq 1 ]; then
  [ "$(id -u)" -eq 0 ] || fail "--system requires root"
  UNIT_DIR="/etc/systemd/user"
  SYSTEMCTL="systemctl --global"
else
  [ "$(id -u)" -ne 0 ] || fail "run this as the logged-in user, or pass --system to install for all users"
  UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  SYSTEMCTL="systemctl --user"
fi
UNIT_PATH="$UNIT_DIR/$UNIT"

user_manager_available() {
  systemctl --user show-environment >/dev/null 2>&1
}

if [ "$MODE" = "uninstall" ]; then
  if [ "$SYSTEM" -eq 1 ]; then
    $SYSTEMCTL disable "$UNIT" 2>/dev/null || true
  elif user_manager_available; then
    # Stopping the unit stops its cgroup, which includes the daemon; git-ai
    # respawns it lazily on the next checkpoint.
    $SYSTEMCTL disable --now "$UNIT" 2>/dev/null || true
  fi
  rm -f "$UNIT_PATH"
  if [ "$SYSTEM" -eq 0 ] && user_manager_available; then
    $SYSTEMCTL daemon-reload
  fi
  echo "git-ai login-start: removed $UNIT_PATH"
  exit 0
fi

if [ -n "$BIN" ]; then
  case "$BIN" in
    /*) ;;
    *) fail "--bin must be an absolute path" ;;
  esac
  # The path is embedded in the unit file; keep shell metacharacters out of it.
  if ! printf '%s' "$BIN" | grep -Eq '^[A-Za-z0-9_./+@ -]+$'; then
    fail "--bin may only contain letters, digits, spaces and _ . / + @ -"
  fi
  [ -x "$BIN" ] || fail "$BIN is not an executable git-ai binary"
  PROGRAM="$BIN"
elif [ "$SYSTEM" -eq 0 ] && [ ! -x "$HOME/.git-ai/bin/git-ai" ]; then
  fail "$HOME/.git-ai/bin/git-ai not found; install git-ai first or pass --bin"
else
  # %h is expanded by systemd to each user's home, so one unit serves everyone.
  PROGRAM="$DEFAULT_BIN"
fi

mkdir -p "$UNIT_DIR"
cat >"$UNIT_PATH" <<EOF
[Unit]
Description=Start the Git AI background service at login
Documentation=https://github.com/git-ai-project/git-ai/tree/main/mdm

[Service]
# oneshot + RemainAfterExit: systemd runs \`bg start\` once and then keeps the
# unit (and its cgroup, where the daemon lives) active until logout. The daemon
# supervises itself, so it is deliberately not restarted by systemd.
Type=oneshot
RemainAfterExit=yes
ExecStart="$PROGRAM" bg start
ExecStop=-"$PROGRAM" bg shutdown
${ENV_LINES}
[Install]
WantedBy=default.target
EOF
chmod 0644 "$UNIT_PATH"

if [ "$SYSTEM" -eq 1 ]; then
  $SYSTEMCTL enable "$UNIT" >/dev/null
  echo "git-ai login-start: installed $UNIT_PATH (starts at each user's next login)"
  exit 0
fi

if ! user_manager_available; then
  if [ "$START" -eq 1 ]; then
    fail "installed $UNIT_PATH but the systemd user manager is not reachable; run 'loginctl enable-linger $(id -un)' or log in graphically, then 'systemctl --user enable --now $UNIT'"
  fi
  echo "git-ai login-start: installed $UNIT_PATH (enable with 'systemctl --user enable $UNIT' once a user session exists)"
  exit 0
fi

$SYSTEMCTL daemon-reload
if [ "$START" -eq 1 ]; then
  $SYSTEMCTL enable --now "$UNIT" >/dev/null
else
  $SYSTEMCTL enable "$UNIT" >/dev/null
fi
echo "git-ai login-start: installed $UNIT_PATH"
