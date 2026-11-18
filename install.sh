#!/usr/bin/env sh
set -eu

REPO="repentandliveholy/resguard"
SITE_RELEASES="https://resguard.repentandliveholy.org/releases"

VERSION="${RESGUARD_VERSION:-latest}"
INSTALL_DIR="${RESGUARD_INSTALL_DIR:-}"
INSTALL_SERVICE="${RESGUARD_INSTALL_SERVICE:-0}"
WITH_GUI="${RESGUARD_WITH_GUI:-0}"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --service) INSTALL_SERVICE=1 ;;
        --with-gui) WITH_GUI=1 ;;
        --dir)
            shift
            INSTALL_DIR="${1:-}"
            ;;
        --version)
            shift
            VERSION="${1:-latest}"
            ;;
        -h|--help)
            cat <<EOF
resguard installer

USAGE:
  curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh
  curl -fsSL https://resguard.repentandliveholy.org/install.sh | sh -s -- --service --with-gui

OPTIONS:
  --service          Install and start resguard as a background service (launchd / systemd)
  --with-gui         Also install RS Dash desktop GUI (rs-dash / RS Dash.app on macOS)
  --dir <PATH>       Custom install directory (default: /usr/local/bin or ~/.local/bin)
  --version <TAG>    Install specific release tag (default: latest)
EOF
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

OS_RAW="$(uname -s)"
ARCH_RAW="$(uname -m)"

case "$OS_RAW" in
    Darwin) OS="macos" ;;
    Linux)  OS="linux" ;;
    *)
        echo "Unsupported OS: $OS_RAW (use install.ps1 on Windows)" >&2
        exit 1
        ;;
esac

case "$ARCH_RAW" in
    arm64|aarch64) ARCH="arm64" ;;
    x86_64|amd64)  ARCH="x86_64" ;;
    *)
        echo "Unsupported architecture: $ARCH_RAW" >&2
        exit 1
        ;;
esac

ASSET="resguard-${OS}-${ARCH}.tar.gz"
GUI_APP_ASSET="RS-Dash-macos-${ARCH}.app.tar.gz"

if [ "$VERSION" = "latest" ]; then
    GH_BASE="https://github.com/${REPO}/releases/latest/download"
else
    GH_BASE="https://github.com/${REPO}/releases/download/${VERSION}"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

download_asset() {
    name="$1"
    dest="$2"
    if curl -fsSL --retry 2 "${GH_BASE}/${name}" -o "$dest" 2>/dev/null; then
        return 0
    fi
    curl -fsSL --retry 2 "${SITE_RELEASES}/${name}" -o "$dest"
}

echo "==> Downloading ${ASSET} (${OS}/${ARCH})..."
download_asset "$ASSET" "${TMP_DIR}/${ASSET}"

if download_asset "SHA256SUMS.txt" "${TMP_DIR}/SHA256SUMS.txt" 2>/dev/null; then
    EXPECTED="$(awk -v f="$ASSET" '$2 == f || $2 == ("*" f) { print $1; exit }' "${TMP_DIR}/SHA256SUMS.txt")"
    if [ -n "$EXPECTED" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL="$(sha256sum "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
            ACTUAL="$(shasum -a 256 "${TMP_DIR}/${ASSET}" | awk '{print $1}')"
        else
            ACTUAL=""
        fi
        if [ -n "$ACTUAL" ] && [ "$ACTUAL" != "$EXPECTED" ]; then
            echo "Checksum mismatch for ${ASSET}!" >&2
            echo "  expected: $EXPECTED" >&2
            echo "  actual:   $ACTUAL" >&2
            exit 1
        fi
        echo "==> SHA256 verified."
    fi
fi

mkdir -p "${TMP_DIR}/extract"
tar -xzf "${TMP_DIR}/${ASSET}" -C "${TMP_DIR}/extract"

if [ -z "$INSTALL_DIR" ]; then
    if [ -w "/usr/local/bin" ]; then
        INSTALL_DIR="/usr/local/bin"
    elif [ "$(id -u)" -eq 0 ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        INSTALL_DIR="${HOME}/.local/bin"
    fi
fi

mkdir -p "$INSTALL_DIR"

install_bin() {
    bin="$1"
    src="${TMP_DIR}/extract/${bin}"
    if [ ! -f "$src" ]; then
        src="$(find "${TMP_DIR}/extract" -type f -name "$bin" | head -n 1 || true)"
    fi
    if [ -n "$src" ] && [ -f "$src" ]; then
        cp "$src" "${INSTALL_DIR}/${bin}"
        chmod 755 "${INSTALL_DIR}/${bin}"
        echo "==> Installed ${INSTALL_DIR}/${bin}"
    fi
}

install_bin "resguard"
install_bin "rs-tui"
if [ "$WITH_GUI" = "1" ]; then
    install_bin "rs-dash"
    if [ "$OS" = "macos" ]; then
        if download_asset "$GUI_APP_ASSET" "${TMP_DIR}/${GUI_APP_ASSET}" 2>/dev/null; then
            APP_DIR="${HOME}/Applications"
            if [ -w "/Applications" ]; then
                APP_DIR="/Applications"
            fi
            mkdir -p "$APP_DIR"
            tar -xzf "${TMP_DIR}/${GUI_APP_ASSET}" -C "$APP_DIR"
            echo "==> Installed ${APP_DIR}/RS Dash.app"
        fi
    fi
fi

if [ "$INSTALL_SERVICE" = "1" ]; then
    echo "==> Installing background service..."
    "${INSTALL_DIR}/resguard" --install-service
fi

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo ""
        echo "Note: ${INSTALL_DIR} is not in your PATH. Add it with:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac

echo ""
echo "Done! Try:"
echo "  resguard --once --dry-run    # single telemetry snapshot"
echo "  resguard --tui               # interactive terminal dashboard"
echo "  resguard --install-service   # run continuously in background"
