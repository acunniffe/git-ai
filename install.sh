#!/bin/bash

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

# GitHub repository details
REPO="acunniffe/git-ai"
LATEST_RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"

# Function to print error messages
error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

# Function to print success messages
success() {
    echo -e "${GREEN}$1${NC}"
}

# Function to detect shell and generate alias command
detect_shell_and_alias() {
    local shell_name=""
    local config_file=""
    local alias_command=""
    
    # Check for zsh first (macOS default)
    if [ -f "$HOME/.zshrc" ]; then
        shell_name="zsh"
        config_file="$HOME/.zshrc"
        alias_command="alias git=git-ai"
    # Check for bash
    elif [ -f "$HOME/.bashrc" ] || [ -f "$HOME/.bash_profile" ]; then
        shell_name="bash"
        config_file="$HOME/.bashrc"
        alias_command="alias git=git-ai"
    # Check for PowerShell
    elif [ -n "$PS1" ] && [ -n "$POWERSHELL_DISTRIBUTION_CHANNEL" ]; then
        shell_name="powershell"
        config_file="$PROFILE"
        alias_command="Set-Alias -Name git -Value git-ai"
    # Check for WSL
    elif [ -n "$PS1" ] && [ -n "$WSL_DISTRO_NAME" ]; then
        shell_name="wsl"
        config_file="$HOME/.bashrc"
        alias_command="alias git=git-ai"
    else
        # Fallback - try to detect from environment
        if [ -n "$ZSH_VERSION" ]; then
            shell_name="zsh"
            config_file="$HOME/.zshrc"
            alias_command="alias git=git-ai"
        elif [ -n "$BASH_VERSION" ]; then
            shell_name="bash"
            config_file="$HOME/.bashrc"
            alias_command="alias git=git-ai"
        else
            shell_name="unknown"
            config_file=""
            alias_command="alias git=git-ai"
        fi
    fi
    
    echo "$shell_name|$config_file|$alias_command"
}

# Function to check if git resolves to git-ai
check_git_alias() {
    # Check if git is aliased to git-ai
    local which_output=""
    
    if command -v which >/dev/null 2>&1; then
        which_output=$(which git 2>/dev/null)
    elif command -v whereis >/dev/null 2>&1; then
        which_output=$(whereis git 2>/dev/null | awk '{print $2}')
    fi
    
    # Check if the output contains "aliased to git-ai"
    if [ -n "$which_output" ] && echo "$which_output" | grep -q "aliased to git-ai"; then
        return 0  # Git is aliased to git-ai
    fi
    
    # Also check if git resolves to our binary path
    if [ -n "$which_output" ] && [ "$which_output" = "${INSTALL_DIR}/git-ai" ]; then
        return 0  # Git resolves to git-ai
    fi
    
    # Check if the alias exists in the detected config file
    if [ -n "$CONFIG_FILE" ] && [ -f "$CONFIG_FILE" ]; then
        if grep -q "alias git=git-ai" "$CONFIG_FILE" 2>/dev/null; then
            return 0  # Alias exists in config file
        fi
    fi
    
    return 1  # Git doesn't resolve to git-ai
}

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
    "mingw"*|"msys"*|"cygwin"*)
        OS="windows"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
if [ "$OS" = "windows" ]; then
    BINARY_NAME="git-ai-${OS}-${ARCH}.exe"
else
    BINARY_NAME="git-ai-${OS}-${ARCH}"
fi

# Get the latest release version
VERSION=$(curl -s $LATEST_RELEASE_URL | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
if [ -z "$VERSION" ]; then
    error "Failed to fetch latest release version"
fi

# Download URL
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY_NAME}"

# Determine installation directory
if [ "$OS" = "windows" ]; then
    INSTALL_DIR="$HOME/.local/bin"
else
    # Try to use /usr/local/bin first, fall back to ~/.local/bin if no permission
    if mkdir -p /usr/local/bin 2>/dev/null && [ -w /usr/local/bin ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi

# Create directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

# Download and install
echo "Downloading git-ai ${VERSION}..."
if ! curl -L -o "${INSTALL_DIR}/git-ai" "$DOWNLOAD_URL"; then
    error "Failed to download binary"
fi

# Make executable
chmod +x "${INSTALL_DIR}/git-ai"

# Remove quarantine attribute on macOS
if [ "$OS" = "darwin" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/git-ai" 2>/dev/null || true
fi

# Detect shell and get alias information
SHELL_INFO=$(detect_shell_and_alias)
SHELL_NAME=$(echo "$SHELL_INFO" | cut -d'|' -f1)
CONFIG_FILE=$(echo "$SHELL_INFO" | cut -d'|' -f2)
ALIAS_CMD=$(echo "$SHELL_INFO" | cut -d'|' -f3)

success "Successfully installed git-ai ${VERSION} to ${INSTALL_DIR}/git-ai"
success "You can now run 'git-ai' from your terminal"

# Add to PATH if not already there
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo "To use git-ai, add this to your ${CONFIG_FILE} and restart shell:"
    echo "export PATH=\"\$PATH:$INSTALL_DIR\""
fi

# Check if git alias already exists
if check_git_alias; then
    echo ""
    success "✅ Git alias already configured! You're all set."
else
    # Show alias command based on detected shell
    echo ""
    echo "⚠️  IMPORTANT: You MUST alias 'git' to 'git-ai' for proper functionality!"
    echo ""
    if [ "$SHELL_NAME" = "powershell" ]; then
        echo "Run this command in PowerShell to set up the alias:"
        echo "$ALIAS_CMD"
        echo ""
        echo "Or run this to add it to your PowerShell profile:"
        echo "> Add-Content -Path \"${CONFIG_FILE}\" -Value \"$ALIAS_CMD\""
    elif [ "$SHELL_NAME" != "unknown" ]; then
        echo "Add this line to your ${CONFIG_FILE}:"
        echo "$ALIAS_CMD"
        echo ""
        echo "Or run this to add it automatically:"
        echo "> echo \"$ALIAS_CMD\" >> \"${CONFIG_FILE}\""
        echo ""
        echo "Then restart your shell or run: source ${CONFIG_FILE}"
    else
        echo "Add this line to your shell config file:"
        echo "$ALIAS_CMD"
    fi
fi