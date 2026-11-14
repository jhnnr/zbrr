#!/usr/bin/env bash
# ==============================================================================
# AnimeZ (zbrr) Installer Script
# Downloads and installs the prebuilt release binary from GitHub
# ==============================================================================

set -euo pipefail

REPO="jhnnr/zbrr"
BINARY_NAME="zbrr"

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

TARGET_VERSION=""
CUSTOM_DIR=""
SYSTEM_INSTALL=false

usage() {
    cat <<EOF
Usage: ./install.sh [OPTIONS]

Options:
  -v, --version <tag>             Target version to install (default: latest release)
  -d, --dir <path>, --prefix <path> Installation directory (default: /usr/bin)
  --system                        Install to /usr/bin (elevates with sudo if needed)
  -h, --help                      Show this help message
EOF
    exit 0
}

# Parse options
while [[ $# -gt 0 ]]; do
    case "$1" in
        -v|--version)
            TARGET_VERSION="$2"
            shift 2
            ;;
        -d|--dir|--prefix)
            CUSTOM_DIR="$2"
            shift 2
            ;;
        --system)
            SYSTEM_INSTALL=true
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

echo -e "${BOLD}=== AnimeZ (zbrr) Binary Installer ===${RESET}\n"

# 1. Detect Architecture
ARCH_UNAME="$(uname -m)"
case "$ARCH_UNAME" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        ;;
    *)
        log_error "Unsupported architecture: $ARCH_UNAME"
        exit 1
        ;;
esac

# 2. Detect OS
OS_UNAME="$(uname -s)"
case "$OS_UNAME" in
    Linux)
        if [ "$TARGET_ARCH" = "x86_64" ] && command -v ldd >/dev/null 2>&1 && ldd /bin/ls 2>&1 | grep -q 'musl'; then
            TARGET_OS="unknown-linux-musl"
        else
            TARGET_OS="unknown-linux-gnu"
        fi
        EXT="tar.gz"
        ;;
    Darwin)
        TARGET_OS="apple-darwin"
        EXT="tar.gz"
        ;;
    MINGW*|MSYS*|CYGWIN*)
        TARGET_OS="pc-windows-msvc"
        BINARY_NAME="zbrr.exe"
        EXT="zip"
        ;;
    *)
        log_error "Unsupported operating system: $OS_UNAME"
        exit 1
        ;;
esac

TARGET_TRIPLE="${TARGET_ARCH}-${TARGET_OS}"
log_info "Detected platform: ${BOLD}${TARGET_TRIPLE}${RESET}"

# 3. Determine release version tag
if [ -n "$TARGET_VERSION" ]; then
    TAG="$TARGET_VERSION"
    if [[ ! "$TAG" =~ ^v ]]; then
        TAG="v$TAG"
    fi
    log_info "Installing specified version: $TAG"
else
    log_info "Fetching latest release tag from GitHub..."
    LATEST_API="https://api.github.com/repos/${REPO}/releases/latest"
    TAG=$(curl -sSL "$LATEST_API" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' || true)

    if [ -z "$TAG" ]; then
        # Fallback to redirect location header if API is rate-limited
        TAG=$(curl -sSI "https://github.com/${REPO}/releases/latest" 2>/dev/null | grep -i '^location:' | sed -E 's/.*\/tag\/([^ \r\n]+).*/\1/' || true)
    fi

    if [ -z "$TAG" ]; then
        log_error "Could not determine the latest release tag for ${REPO}."
        log_error "You can specify a version explicitly using: ./install.sh --version v0.1.0"
        exit 1
    fi
    log_info "Latest release: ${BOLD}${TAG}${RESET}"
fi

# 4. Determine installation directory
if [ -n "$CUSTOM_DIR" ]; then
    INSTALL_DIR="$CUSTOM_DIR"
else
    INSTALL_DIR="/usr/bin"
fi
INSTALL_DIR="${INSTALL_DIR/#\~/$HOME}"

# 5. Download release archive
ARCHIVE_NAME="zbrr-${TAG}-${TARGET_TRIPLE}.${EXT}"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ARCHIVE_NAME}"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'zbrr')"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

log_info "Downloading ${ARCHIVE_NAME}..."
if ! curl -fL --progress-bar "$DOWNLOAD_URL" -o "$TMP_DIR/$ARCHIVE_NAME"; then
    echo ""
    log_error "Failed to download $DOWNLOAD_URL"
    log_error "Verify that this release asset exists on GitHub, or build from source using 'cargo build --release'."
    exit 1
fi

# 6. Extract archive
log_info "Extracting archive..."
if [ "$EXT" = "zip" ]; then
    if ! command -v unzip >/dev/null 2>&1; then
        log_error "'unzip' is required to extract the Windows archive."
        exit 1
    fi
    unzip -q "$TMP_DIR/$ARCHIVE_NAME" -d "$TMP_DIR"
else
    tar -xzf "$TMP_DIR/$ARCHIVE_NAME" -C "$TMP_DIR"
fi

# Locate extracted binary
EXTRACTED_BIN=$(find "$TMP_DIR" -type f \( -name "zbrr" -o -name "zbrr.exe" \) | head -n 1)
if [ -z "$EXTRACTED_BIN" ] || [ ! -f "$EXTRACTED_BIN" ]; then
    log_error "Could not find '${BINARY_NAME}' executable in extracted archive."
    exit 1
fi

# 7. Install to target directory
if [ ! -d "$INSTALL_DIR" ]; then
    log_info "Creating directory: $INSTALL_DIR"
    mkdir -p "$INSTALL_DIR" || {
        if command -v sudo >/dev/null 2>&1; then
            sudo mkdir -p "$INSTALL_DIR"
        else
            log_error "Failed to create directory $INSTALL_DIR and sudo is unavailable."
            exit 1
        fi
    }
fi

DEST_PATH="$INSTALL_DIR/$BINARY_NAME"
log_info "Installing $BINARY_NAME to $INSTALL_DIR..."

if [ -w "$INSTALL_DIR" ] || [ "$(id -u 2>/dev/null || echo 1000)" -eq 0 ]; then
    if command -v install >/dev/null 2>&1; then
        install -m 755 "$EXTRACTED_BIN" "$DEST_PATH"
    else
        cp -f "$EXTRACTED_BIN" "$DEST_PATH"
        chmod +x "$DEST_PATH" 2>/dev/null || true
    fi
else
    if command -v sudo >/dev/null 2>&1; then
        log_info "Elevating with sudo to install into $INSTALL_DIR..."
        if command -v install >/dev/null 2>&1; then
            sudo install -m 755 "$EXTRACTED_BIN" "$DEST_PATH"
        else
            sudo cp -f "$EXTRACTED_BIN" "$DEST_PATH"
            sudo chmod +x "$DEST_PATH" 2>/dev/null || true
        fi
    else
        log_error "No write permission for $INSTALL_DIR and sudo is unavailable."
        log_error "Try running: ./install.sh --dir ~/.local/bin"
        exit 1
    fi
fi

log_success "Binary installed at: $DEST_PATH"

# 8. Ensure ~/.zbrr data directory exists
ZBRR_USER_DIR="$HOME/.zbrr"
if [ ! -d "$ZBRR_USER_DIR" ]; then
    mkdir -p "$ZBRR_USER_DIR"
fi

# 9. Verify PATH
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

# 10. Test execution
if command -v zbrr >/dev/null 2>&1; then
    log_success "Verified: 'zbrr' is executable directly from PATH:"
    zbrr --version || true
elif [ -x "$DEST_PATH" ]; then
    log_success "Verified: Installed binary is executable directly:"
    "$DEST_PATH" --version || true
fi

echo -e "\n${BOLD}${GREEN}Installation completed!${RESET} Run ${CYAN}zbrr doctor${RESET} to check connectivity."
