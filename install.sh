#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

# ============================================================
# Ensure HOME is set when running via MDMs (e.g. JAMF) or other environments where HOME may be unbound.
# ============================================================
INSTALL_USER=""

if [ -z "${HOME:-}" ]; then
    if command -v scutil >/dev/null 2>&1; then
        CURRENT_USER=$( /usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true )
        if [ -n "${CURRENT_USER:-}" ] && [ "$CURRENT_USER" != "loginwindow" ] && [ "$CURRENT_USER" != "_mbsetupuser" ]; then
            export HOME=$( /usr/bin/dscl . -read "/Users/$CURRENT_USER" NFSHomeDirectory | awk '{print $2}' )
            INSTALL_USER="$CURRENT_USER"
        else
            echo "Error: No console user logged in. Deferring installation." >&2
            exit 1
        fi
    elif id -un >/dev/null 2>&1; then
        INSTALL_USER="$(id -un)"
        export HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
        if [ -z "$HOME" ]; then
            export HOME="/root"
        fi
    else
        export HOME="/root"
    fi
fi

# Ensure SHELL is set (also may be unbound in JAMF)
if [ -z "${SHELL:-}" ]; then
    if command -v zsh >/dev/null 2>&1; then
        SHELL="$(command -v zsh)"
    elif command -v bash >/dev/null 2>&1; then
        SHELL="$(command -v bash)"
    else
        SHELL="/bin/sh"
    fi
    export SHELL
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
REPO="__REPO_PLACEHOLDER__"
if [ "$REPO" = "__REPO_PLACEHOLDER__" ]; then
    REPO="git-ai-project/git-ai"
fi

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
PINNED_VERSION="__VERSION_PLACEHOLDER__"

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"

# Function to print error messages
error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning: $1${NC}" >&2
}

# Function to print success messages
success() {
    echo -e "${GREEN}$1${NC}"
}

# Function to verify checksum of downloaded binary
verify_checksum() {
    local file="$1"
    local binary_name="$2"

    # Skip verification if no checksums are embedded
    if [ "$EMBEDDED_CHECKSUMS" = "__CHECKSUMS_PLACEHOLDER__" ]; then
        return 0
    fi

    # Extract expected checksum for this binary
    local expected=""
    local old_ifs="$IFS"
    IFS='|' read -ra CHECKSUM_ENTRIES <<< "$EMBEDDED_CHECKSUMS"
    IFS="$old_ifs"
    for entry in "${CHECKSUM_ENTRIES[@]}"; do
        if [[ "$entry" =~ ^[[:xdigit:]]+[[:space:]]+$binary_name$ ]]; then
            expected=$(echo "$entry" | awk '{print $1}')
            break
        fi
    done

    if [ -z "$expected" ]; then
        error "No checksum found for $binary_name"
    fi

    # Calculate actual checksum
    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        warn "Neither sha256sum nor shasum available, skipping checksum verification"
        return 0
    fi

    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

# ============================================================
# Warn when installing as root/sudo (not recommended).
# Running as root creates files that normal-user processes
# cannot access, causing persistent daemon lock failures.
# ============================================================
if [ "$(id -u)" = "0" ] && [ "${GIT_AI_ALLOW_SUPERUSER:-}" != "1" ]; then
    # Auto-allow in CI environments, MDM deployments (JAMF, etc.),
    # and daemon-triggered self-updates (GIT_AI_DAEMON_UPGRADE is set internally by the upgrade command)
    IS_CI_OR_MDM=false
    if [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ] || [ -n "${GITLAB_CI:-}" ] \
        || [ -n "${JENKINS_URL:-}" ] || [ -n "${BUILDKITE:-}" ] || [ -n "${CIRCLECI:-}" ] \
        || [ -n "${CODEBUILD_BUILD_ID:-}" ] || [ -n "${AGENT_OS:-}" ] \
        || [ -n "${KUBERNETES_SERVICE_HOST:-}" ] || [ -n "${INSTALL_USER:-}" ] \
        || [ -n "${GIT_AI_DAEMON_UPGRADE:-}" ] \
        || [ -n "${container:-}" ] || [ -f "/.dockerenv" ]; then
        IS_CI_OR_MDM=true
    fi

    if [ "$IS_CI_OR_MDM" = "false" ]; then
        echo ""
        echo -e "${YELLOW}Warning: installing git-ai as root/sudo is not recommended.${NC}"
        echo ""
        echo "Running with elevated privileges creates files owned by root that become"
        echo "inaccessible to your normal user account, causing persistent daemon lock"
        echo "failures. A future version may refuse to install in this configuration."
        echo ""
        echo "To suppress this warning, either:"
        echo "  - Run this installer as your normal user (recommended), or"
        echo "  - Set GIT_AI_ALLOW_SUPERUSER=1"
        echo ""
    fi
    # Propagate to child git-ai invocations (install-hooks, exchange-nonce, login)
    export GIT_AI_ALLOW_SUPERUSER=1
fi

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)


# Map architecture to binary name
case $ARCH in
    "x86_64")
        ARCH="x64"
        ;;
    "aarch64"|"arm64")
        ARCH="arm64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

# Map OS to binary name
case $OS in
    "darwin")
        OS="macos"
        ;;
    "linux")
        OS="linux"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
BINARY_NAME="git-ai-${OS}-${ARCH}"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    RELEASE_TAG="local"
    DOWNLOAD_URL=""
elif [ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ]; then
    # Version-pinned install script from a release
    RELEASE_TAG="$PINNED_VERSION"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
elif [ -n "${GIT_AI_RELEASE_TAG:-}" ] && [ "${GIT_AI_RELEASE_TAG:-}" != "latest" ]; then
    # Environment variable override
    RELEASE_TAG="$GIT_AI_RELEASE_TAG"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
else
    # Default to latest
    RELEASE_TAG="latest"
    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}"
fi

# Install into the user's bin directory ~/.git-ai/bin
INSTALL_DIR="$HOME/.git-ai/bin"

# Create directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

# Download and install
TMP_FILE="${INSTALL_DIR}/git-ai.tmp.$$"
if [ -n "${GIT_AI_LOCAL_BINARY:-}" ]; then
    echo "Using local git-ai binary (release: ${RELEASE_TAG})..."
    if [ ! -f "$GIT_AI_LOCAL_BINARY" ]; then
        error "Local binary not found at $GIT_AI_LOCAL_BINARY"
    fi
    cp "$GIT_AI_LOCAL_BINARY" "$TMP_FILE"
else
    echo "Downloading git-ai (release: ${RELEASE_TAG})..."
    if ! curl --fail --location --silent --show-error -o "$TMP_FILE" "$DOWNLOAD_URL"; then
        rm -f "$TMP_FILE" 2>/dev/null || true
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    rm -f "$TMP_FILE" 2>/dev/null || true
    error "Downloaded file is empty"
fi

# Verify checksum if embedded (release builds only)
verify_checksum "$TMP_FILE" "$BINARY_NAME"

mv -f "$TMP_FILE" "${INSTALL_DIR}/git-ai"

# Make executable
chmod +x "${INSTALL_DIR}/git-ai"

# Remove quarantine attribute on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/git-ai" 2>/dev/null || true
fi

# Create ~/.local/bin/git-ai symlink for systems where ~/.local/bin is already on PATH
LOCAL_BIN_DIR="$HOME/.local/bin"
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -sf "${INSTALL_DIR}/git-ai" "${LOCAL_BIN_DIR}/git-ai" 2>/dev/null; then
    success "Created symlink at ${LOCAL_BIN_DIR}/git-ai"
else
    warn "Failed to create ~/.local/bin/git-ai symlink. This is non-fatal."
fi

success "Successfully installed git-ai into ${INSTALL_DIR}"
success "You can now run 'git-ai' from your terminal"

# Print installed version
INSTALLED_VERSION=$(${INSTALL_DIR}/git-ai --version 2>&1 || echo "unknown")
echo "Installed git-ai ${INSTALLED_VERSION}"

# Login user with install token if provided
NEED_LOGIN=false
if [ -n "${INSTALL_NONCE:-}" ] && [ -n "${API_BASE:-}" ]; then
    if ! ${INSTALL_DIR}/git-ai exchange-nonce; then
        NEED_LOGIN=true
    fi
fi

echo "Setting up IDE/agent hooks..."
# --env also adds git-ai to the PATH in all detected shell configurations;
# GIT_AI_INSTALL_USER lets it hand ownership of any files it creates back to
# the target user in root/MDM installs.
if ! GIT_AI_INSTALL_USER="$INSTALL_USER" "${INSTALL_DIR}/git-ai" install-hooks --env; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.git-ai" 2>/dev/null || true
fi

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use git-ai.${NC}"

# If nonce exchange failed, run interactive login
if [ "$NEED_LOGIN" = true ]; then
    echo ""
    echo "Launching login..."
    ${INSTALL_DIR}/git-ai login
fi
