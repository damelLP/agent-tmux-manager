#!/bin/bash
# ATM Installer
# Downloads and installs atm binaries and configures Claude Code hooks.
#
# Usage: curl -sSL https://raw.githubusercontent.com/damel/agent-tmux-manager/main/scripts/install.sh | sh

set -e

REPO="damel/agent-tmux-manager"
INSTALL_DIR="${ATM_INSTALL_DIR:-$HOME/.local/bin}"

# Colors (if terminal supports them)
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[0;33m'
    NC='\033[0m' # No Color
else
    RED=''
    GREEN=''
    YELLOW=''
    NC=''
fi

info() { echo -e "${GREEN}==>${NC} $1"; }
warn() { echo -e "${YELLOW}warning:${NC} $1"; }
error() { echo -e "${RED}error:${NC} $1" >&2; exit 1; }

# Detect platform
detect_platform() {
    local arch os target

    arch=$(uname -m)
    os=$(uname -s | tr '[:upper:]' '[:lower:]')

    case "$arch" in
        x86_64)        arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) error "Unsupported architecture: $arch" ;;
    esac

    case "$os" in
        linux)  target="${arch}-unknown-linux-gnu" ;;
        darwin) target="${arch}-apple-darwin" ;;
        *) error "Unsupported OS: $os" ;;
    esac

    echo "$target"
}

# Get latest release version
get_latest_version() {
    curl -sSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | cut -d'"' -f4
}

# Main installation
main() {
    info "Detecting platform..."
    local target
    target=$(detect_platform)
    info "Platform: $target"

    info "Fetching latest version..."
    local version
    version=$(get_latest_version)
    if [ -z "$version" ]; then
        error "Could not determine latest version. Check your internet connection."
    fi
    info "Version: $version"

    # Download URL
    local url="https://github.com/${REPO}/releases/download/${version}/atm-${target}.tar.gz"

    # Create temp directory
    local tmpdir
    tmpdir=$(mktemp -d)
    trap "rm -rf '$tmpdir'" EXIT

    info "Downloading $url..."
    if ! curl -sSL "$url" -o "$tmpdir/atm.tar.gz"; then
        error "Download failed. Version $version may not have binaries for $target."
    fi

    info "Extracting..."
    tar -xzf "$tmpdir/atm.tar.gz" -C "$tmpdir"

    info "Installing to $INSTALL_DIR..."
    mkdir -p "$INSTALL_DIR"

    # Find extracted directory (atm-vX.X.X/)
    local extracted_dir
    extracted_dir=$(find "$tmpdir" -maxdepth 1 -type d -name 'atm-*' | head -1)
    if [ -z "$extracted_dir" ]; then
        error "Could not find extracted files"
    fi

    cp "$extracted_dir/atm" "$INSTALL_DIR/"
    cp "$extracted_dir/atmd" "$INSTALL_DIR/"
    cp "$extracted_dir/atm-hook" "$INSTALL_DIR/"
    chmod +x "$INSTALL_DIR/atm" "$INSTALL_DIR/atmd" "$INSTALL_DIR/atm-hook"

    info "Binaries installed!"

    # Check if INSTALL_DIR is in PATH
    if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
        warn "$INSTALL_DIR is not in your PATH"
        echo ""
        echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
        echo ""
    fi

    info "Configuring Claude Code hooks..."
    if "$INSTALL_DIR/atm" setup; then
        echo ""
        info "Installation complete!"
    else
        warn "Hook configuration failed. Run 'atm setup' manually."
    fi

    echo ""
    echo "Quick start:"
    echo "  atmd start -d   # Start daemon in background"
    echo "  atm             # Launch TUI"
}

main "$@"
