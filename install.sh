#!/usr/bin/env bash
# ==============================================================================
# AnimeZ (zbrr) Installer Script
# Builds and installs the zbrr binary to your system PATH
# ==============================================================================

set -euo pipefail

# Text formatting
BOLD="\033[1m"
GREEN="\033[32m"
YELLOW="\033[33m"
CYAN="\033[36m"
RED="\033[31m"
RESET="\033[0m"

log_info()    { echo -e "${CYAN}[INFO]${RESET} $*"; }
log_success() { echo -e "${GREEN}[OK]${RESET} ${BOLD}$*${RESET}"; }
log_warn()    { echo -e "${YELLOW}[WARN]${RESET} $*"; }
log_error()   { echo -e "${RED}[ERROR]${RESET} $*" >&2; }

# Locate script directory (repository root)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Defaults
NO_BUILD=false
SYSTEM_INSTALL=false
CUSTOM_DIR=""

usage() {
    cat <<EOF
Usage: ./install.sh [OPTIONS]

Options:
  --dir <path>, --prefix <path>   Custom directory to install the binary to (e.g. ~/.local/bin)
  --system                        Install system-wide to /usr/local/bin (uses sudo if needed)
  --no-build                      Skip 'cargo build --release' and install existing target binary
  -h, --help                      Show this help message
EOF
    exit 0
}

# Parse flags
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dir|--prefix)
            CUSTOM_DIR="$2"
            shift 2
            ;;
        --system)
            SYSTEM_INSTALL=true
            shift
            ;;
        --no-build)
            NO_BUILD=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            log_error "Unknown option: $1"
            usage
            ;;
    esac
done

echo -e "${BOLD}=== AnimeZ (zbrr) Installation ===${RESET}\n"

# 1. Check Rust / Cargo if build is needed
if [ "$NO_BUILD" = false ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        log_error "Rust toolchain 'cargo' was not found in PATH."
        echo "Please install Rust from https://rustup.rs/ before running this installer,"
        echo "or run with --no-build if the binary has already been compiled."
        exit 1
    fi

    log_info "Compiling zbrr in release mode ('cargo build --release')..."
    cargo build --release
    log_success "Compilation finished successfully."
fi

# 2. Locate built binary (supports Linux/macOS and Windows/MSYS)
BINARY_SRC=""
if [ -f "$SCRIPT_DIR/target/release/zbrr" ]; then
    BINARY_SRC="$SCRIPT_DIR/target/release/zbrr"
    BINARY_NAME="zbrr"
elif [ -f "$SCRIPT_DIR/target/release/zbrr.exe" ]; then
    BINARY_SRC="$SCRIPT_DIR/target/release/zbrr.exe"
    BINARY_NAME="zbrr.exe"
else
    log_error "Could not find built binary at target/release/zbrr[.exe]."
    log_error "Please run 'cargo build --release' first or omit --no-build."
    exit 1
fi

# 3. Determine target install directory
if [ -n "$CUSTOM_DIR" ]; then
    INSTALL_DIR="$CUSTOM_DIR"
elif [ "$SYSTEM_INSTALL" = true ]; then
    INSTALL_DIR="/usr/local/bin"
else
    # Non-root default: check writable /usr/local/bin, else ~/.local/bin
    if [ -w "/usr/local/bin" ] || [ "$(id -u 2>/dev/null || echo 1000)" -eq 0 ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi

# Expand tilde if present in custom dir
INSTALL_DIR="${INSTALL_DIR/#\~/$HOME}"

# Ensure install directory exists
if [ ! -d "$INSTALL_DIR" ]; then
    log_info "Creating target installation directory: $INSTALL_DIR"
    mkdir -p "$INSTALL_DIR" || {
        if command -v sudo >/dev/null 2>&1; then
            log_info "Elevating with sudo to create $INSTALL_DIR..."
            sudo mkdir -p "$INSTALL_DIR"
        else
            log_error "Failed to create directory $INSTALL_DIR and sudo is unavailable."
            exit 1
        fi
    }
fi

DEST_PATH="$INSTALL_DIR/$BINARY_NAME"

# 4. Copy / Install binary
log_info "Installing $BINARY_NAME to $INSTALL_DIR..."
if [ -w "$INSTALL_DIR" ] || [ "$(id -u 2>/dev/null || echo 1000)" -eq 0 ]; then
    if command -v install >/dev/null 2>&1; then
        install -m 755 "$BINARY_SRC" "$DEST_PATH"
    else
        cp -f "$BINARY_SRC" "$DEST_PATH"
        chmod +x "$DEST_PATH" 2>/dev/null || true
    fi
else
    if command -v sudo >/dev/null 2>&1; then
        log_info "Elevating permissions with sudo to install into $INSTALL_DIR..."
        if command -v install >/dev/null 2>&1; then
            sudo install -m 755 "$BINARY_SRC" "$DEST_PATH"
        else
            sudo cp -f "$BINARY_SRC" "$DEST_PATH"
            sudo chmod +x "$DEST_PATH" 2>/dev/null || true
        fi
    else
        log_error "No write permission for $INSTALL_DIR and sudo is unavailable."
        log_error "Try running: ./install.sh --dir ~/.local/bin"
        exit 1
    fi
fi

log_success "Binary installed at: $DEST_PATH"

# 5. Initialize user configuration directory ~/.zbrr if missing
ZBRR_USER_DIR="$HOME/.zbrr"
if [ ! -d "$ZBRR_USER_DIR" ]; then
    log_info "Initializing zbrr data directory: $ZBRR_USER_DIR"
    mkdir -p "$ZBRR_USER_DIR"
fi

if [ -f "$SCRIPT_DIR/.env" ] && [ ! -f "$ZBRR_USER_DIR/.env" ]; then
    log_info "Copying local .env template to $ZBRR_USER_DIR/.env for persistent service execution..."
    cp -n "$SCRIPT_DIR/.env" "$ZBRR_USER_DIR/.env" 2>/dev/null || true
fi

# 6. Verify PATH accessibility
echo ""
IN_PATH=false
IFS=':' read -ra PATH_DIRS <<< "$PATH"
for p in "${PATH_DIRS[@]}"; do
    if [ "${p%/}" = "${INSTALL_DIR%/}" ]; then
        IN_PATH=true
        break
    fi
done

if [ "$IN_PATH" = true ]; then
    log_success "$INSTALL_DIR is in your PATH."
else
    log_warn "$INSTALL_DIR is NOT in your current PATH!"
    echo -e "To make 'zbrr' accessible from any terminal, add the following line to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
    echo -e "    ${BOLD}export PATH=\"\$PATH:$INSTALL_DIR\"${RESET}\n"
fi

# 7. Test execution
if command -v zbrr >/dev/null 2>&1; then
    log_success "Verified: 'zbrr' is executable directly from PATH:"
    zbrr --version || true
elif [ -x "$DEST_PATH" ]; then
    log_success "Verified: Installed binary is executable directly:"
    "$DEST_PATH" --version || true
fi

echo -e "\n${BOLD}${GREEN}Installation completed!${RESET} Run ${CYAN}zbrr doctor${RESET} to verify external services and tools."
