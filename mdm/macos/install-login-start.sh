#!/bin/sh
# Register a launchd LaunchAgent that runs `git-ai bg start` when the user
# logs in, so the Git AI background service is up before the first git command.
#
# Usage:
#   install-login-start.sh [install] [--env KEY=VALUE]... [--bin PATH] [--no-start] [--system]
#   install-login-start.sh --uninstall [--system]
#
# The agent only *starts* the daemon. It never supervises it: the daemon
# restarts and updates itself, and a second instance exits because the daemon
# lock is held. See mdm/README.md for the invariants this file must keep.
set -eu

LABEL="com.usegitai.bg"
DEFAULT_PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
# The launcher resolves the binary at run time from GIT_AI_LOGIN_START_BIN (set
# by --bin) or the default install location, and retries briefly so a daemon
# that is still releasing its lock (logout/login, self-update) does not make
# the login start give up. Paths never touch the shell command line.
LOG_DIR='$HOME/.git-ai/internal/daemon/logs'
COMMAND="mkdir -p \"$LOG_DIR\" && { n=0; until \"\${GIT_AI_LOGIN_START_BIN:-\$HOME/.git-ai/bin/git-ai}\" bg start; do [ \$((n+=1)) -lt 5 ] || exit 1; sleep 2; done; } >>\"$LOG_DIR/login-start.log\" 2>&1"

MODE="install"
SYSTEM=0
START=1
BIN=""
ENV_VARS=""
HAS_PATH=0

usage() {
  cat <<'USAGE'
Usage:
  install-login-start.sh [install] [--env KEY=VALUE]... [--bin PATH] [--no-start] [--system]
  install-login-start.sh --uninstall [--system]

Registers a launchd LaunchAgent (com.usegitai.bg) that runs `git-ai bg start`
at login. Per-user by default; --system installs to /Library/LaunchAgents.
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
  case "$1" in
    PATH=*) HAS_PATH=1 ;;
  esac
  ENV_VARS="${ENV_VARS}${1}
"
}

xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g'
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
  PLIST="/Library/LaunchAgents/$LABEL.plist"
  # Bootstrap for the console user if one is logged in; other users pick the
  # agent up at their next login.
  TARGET_USER="$(/usr/bin/stat -f%Su /dev/console 2>/dev/null || true)"
  case "$TARGET_USER" in
    ""|root|loginwindow|_mbsetupuser) TARGET_USER="" ;;
  esac
else
  [ "$(id -u)" -ne 0 ] || fail "run this as the logged-in user (e.g. su - \"\$(stat -f%Su /dev/console)\" -c ...), or pass --system to install for all users"
  PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  TARGET_USER="$(id -un)"
fi

DOMAIN=""
if [ -n "$TARGET_USER" ]; then
  DOMAIN="gui/$(id -u "$TARGET_USER")"
fi

if [ "$MODE" = "uninstall" ]; then
  if [ -n "$DOMAIN" ]; then
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  fi
  rm -f "$PLIST"
  echo "git-ai login-start: removed $PLIST"
  exit 0
fi

if [ -n "$BIN" ]; then
  case "$BIN" in
    /*) ;;
    *) fail "--bin must be an absolute path" ;;
  esac
  [ -x "$BIN" ] || fail "$BIN is not an executable git-ai binary"
  case "$BIN" in
    *"
"*|*"$(printf '\r')"*) fail "--bin path must not contain a newline or carriage return" ;;
  esac
  add_env "GIT_AI_LOGIN_START_BIN=$BIN"
elif [ "$SYSTEM" -eq 0 ] && [ ! -x "$HOME/.git-ai/bin/git-ai" ]; then
  fail "$HOME/.git-ai/bin/git-ai not found; install git-ai first or pass --bin"
fi

if [ "$HAS_PATH" -eq 0 ]; then
  ENV_VARS="PATH=${DEFAULT_PATH}
${ENV_VARS}"
fi

ENV_XML=""
OLD_IFS="$IFS"
IFS='
'
for pair in $ENV_VARS; do
  key="${pair%%=*}"
  value="${pair#*=}"
  ENV_XML="${ENV_XML}        <key>$(xml_escape "$key")</key>
        <string>$(xml_escape "$value")</string>
"
done
IFS="$OLD_IFS"

mkdir -p "$(dirname "$PLIST")"
cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>/bin/sh</string>
        <string>-c</string>
        <string>$(xml_escape "$COMMAND")</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>EnvironmentVariables</key>
    <dict>
${ENV_XML}    </dict>
</dict>
</plist>
EOF

if [ "$SYSTEM" -eq 1 ]; then
  chown root:wheel "$PLIST"
fi
chmod 0644 "$PLIST"

if command -v plutil >/dev/null 2>&1; then
  plutil -lint -s "$PLIST" || fail "generated plist failed validation"
fi

if [ -n "$DOMAIN" ]; then
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  if [ "$START" -eq 1 ]; then
    launchctl bootstrap "$DOMAIN" "$PLIST" || fail "launchctl bootstrap $DOMAIN failed"
  fi
fi

echo "git-ai login-start: installed $PLIST"
