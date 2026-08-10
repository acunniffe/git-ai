#!/usr/bin/env bash
set -euo pipefail
IFS=$'\n\t'

info() { printf '%s\n' "$*"; }
warn() { printf 'Warning: %s\n' "$*" >&2; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }

resolve_home() {
    if [ -n "${HOME:-}" ]; then
        printf '%s\n' "$HOME"
        return 0
    fi

    if command -v scutil >/dev/null 2>&1; then
        local current_user home_dir
        current_user=$(/usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true)
        if [ -n "${current_user:-}" ] && [ "$current_user" != "loginwindow" ] && [ "$current_user" != "_mbsetupuser" ]; then
            home_dir=$(/usr/bin/dscl . -read "/Users/$current_user" NFSHomeDirectory | awk '{print $2}' || true)
            if [ -n "${home_dir:-}" ]; then
                printf '%s\n' "$home_dir"
                return 0
            fi
        fi
    fi

    if command -v getent >/dev/null 2>&1; then
        local user home_dir
        user="$(id -un 2>/dev/null || true)"
        if [ -n "${user:-}" ]; then
            home_dir="$(getent passwd "$user" | awk -F: '{print $6}' || true)"
            if [ -n "${home_dir:-}" ]; then
                printf '%s\n' "$home_dir"
                return 0
            fi
        fi
    fi

    printf '%s\n' "${HOME:-/root}"
}

HOME_DIR="$(resolve_home)"
export HOME="$HOME_DIR"

INSTALL_DIR="$HOME/.git-ai/bin"
CONFIG_DIR="$HOME/.git-ai"
LOCAL_BIN_LINK="$HOME/.local/bin/git-ai"

cleanup_shell_config() {
    local rc_file="$1"

    if [ ! -f "$rc_file" ]; then
        return 0
    fi

    local tmp
    tmp="$(mktemp)"

    awk -v install_dir="$INSTALL_DIR" '
        BEGIN { remove_next = 0 }
        {
            if (remove_next) {
                if (index($0, install_dir) > 0) {
                    remove_next = 0
                    next
                }
                print
                remove_next = 0
                next
            }

            if ($0 ~ /^# Added by git-ai installer/) {
                remove_next = 1
                next
            }

            if (index($0, install_dir) > 0) {
                next
            }

            print
        }
    ' "$rc_file" > "$tmp"

    if cmp -s "$rc_file" "$tmp"; then
        rm -f "$tmp"
        return 0
    fi

    if grep -q '[^[:space:]]' "$tmp"; then
        cat "$tmp" > "$rc_file" && rm -f "$tmp"
        info "Updated $rc_file"
    else
        rm -f "$tmp" "$rc_file"
        info "Removed empty shell config: $rc_file"
    fi
}

remove_temp_files() {
    if [ -d "$INSTALL_DIR" ]; then
        for f in "$INSTALL_DIR"/git-ai.tmp.*; do
            [ -e "$f" ] || continue
            rm -f "$f"
        done
    fi

    if [ -d "$CONFIG_DIR" ]; then
        for f in "$CONFIG_DIR"/config.json.tmp.*; do
            [ -e "$f" ] || continue
            rm -f "$f"
        done
    fi
}

uninstall_hooks_if_possible() {
    if [ -x "$INSTALL_DIR/git-ai" ]; then
        if "$INSTALL_DIR/git-ai" uninstall-hooks >/dev/null 2>&1; then
            info "Uninstalled git-ai hooks"
        else
            warn "Could not automatically uninstall hooks; you may need to remove IDE/editor integrations manually"
        fi
    fi
}

remove_local_bin_symlink() {
    if [ -L "$LOCAL_BIN_LINK" ]; then
        local target
        target="$(readlink "$LOCAL_BIN_LINK" 2>/dev/null || true)"
        if [ "$target" = "$INSTALL_DIR/git-ai" ] || [ "$target" = "$INSTALL_DIR/git" ] || [ "$target" = "$INSTALL_DIR/git-og" ]; then
            rm -f "$LOCAL_BIN_LINK"
            info "Removed $LOCAL_BIN_LINK"
        fi
    fi
}

remove_installed_files() {
    for f in "$INSTALL_DIR/git-ai" "$INSTALL_DIR/git" "$INSTALL_DIR/git-og"; do
        if [ -e "$f" ] || [ -L "$f" ]; then
            rm -f "$f"
            info "Removed $f"
        fi
    done

    rmdir "$INSTALL_DIR" 2>/dev/null || true

    if [ -f "$CONFIG_DIR/config.json" ]; then
        rm -f "$CONFIG_DIR/config.json"
        info "Removed $CONFIG_DIR/config.json"
    fi

    rmdir "$CONFIG_DIR" 2>/dev/null || true
}

cleanup_empty_dirs() {
    rmdir "$HOME/.local/bin" 2>/dev/null || true
    rmdir "$HOME/.config/fish" 2>/dev/null || true
    rmdir "$HOME/.git-ai/bin" 2>/dev/null || true
    rmdir "$HOME/.git-ai" 2>/dev/null || true
}

main() {
    info "Removing git-ai installation from $HOME"

    uninstall_hooks_if_possible
    remove_local_bin_symlink
    remove_installed_files
    remove_temp_files

    for rc in "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.zshrc" "$HOME/.config/fish/config.fish"; do
        cleanup_shell_config "$rc"
    done

    cleanup_empty_dirs

    info "git-ai uninstall complete."
    info "Restart your terminal and IDE sessions to clear any cached PATH changes."
}

main "$@"
