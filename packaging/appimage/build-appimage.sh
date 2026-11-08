#!/usr/bin/env bash
set -euo pipefail

APP_NAME="oxidezap"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
APPIMAGE_RUNTIME_VERSION="20251108"
APPIMAGE_RUNTIME_SHA256="2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d"
APPIMAGE_RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/${APPIMAGE_RUNTIME_VERSION}/runtime-x86_64"

OUTPUT_DIR="$ROOT/dist"
BINARY=""
SKIP_BUILD=0
KEEP_APPDIR=0

usage() {
    cat <<EOF
Build an OxideZap AppImage.

Usage: packaging/appimage/build-appimage.sh [OPTIONS]

Options:
  --skip-build          Use an existing target/release/oxidezap
  --binary PATH         Use this oxidezap binary instead of the release build
  --output-dir DIR      Write the .AppImage here (default: dist)
  --keep-appdir         Keep the temporary AppDir for inspection
  -h, --help            Show this help

Environment:
  VERSION       Artifact version (default: v + Cargo.toml version)
  APPIMAGETOOL  Use this appimagetool instead of downloading version 1.9.1
  APPIMAGE_RUNTIME
                Use this type 2 runtime instead of downloading the pinned one
  TMPDIR        Directory for the temporary AppDir

The AppImage bundles OxideZap, its desktop entry, icons, and AppStream
metadata. It does not bundle GTK 3 or WebKitGTK: the host must provide
WebKitGTK 4.1 >= 2.46.1, GTK 3, Vulkan, and X11/XWayland. Building on Ubuntu
24.04 means hosts need glibc 2.39 or newer.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        --binary)
            if [ "$#" -lt 2 ]; then
                echo "error: --binary needs a path" >&2
                exit 2
            fi
            BINARY="$2"
            shift 2
            ;;
        --output-dir)
            if [ "$#" -lt 2 ]; then
                echo "error: --output-dir needs a directory" >&2
                exit 2
            fi
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --keep-appdir)
            KEEP_APPDIR=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

case "$(uname -m)" in
    x86_64|amd64)
        ARCH="x86_64"
        ;;
    *)
        echo "error: only x86_64 AppImages are built" >&2
        exit 1
        ;;
esac

CARGO_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
if [ -z "${VERSION:-}" ]; then
    if [ -z "$CARGO_VERSION" ]; then
        echo "error: could not determine a version; set VERSION" >&2
        exit 1
    fi
    VERSION="v$CARGO_VERSION"
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "error: cargo was not found; use --skip-build with --binary" >&2
        exit 1
    fi
    (cd "$ROOT" && cargo build --release --locked)
fi

if [ -z "$BINARY" ]; then
    BINARY="$ROOT/target/release/oxidezap"
fi
if [ ! -x "$BINARY" ]; then
    echo "error: oxidezap binary not found: $BINARY" >&2
    exit 1
fi

for asset in \
    "$ROOT/packaging/io.github.artur282.OxideZap.desktop" \
    "$ROOT/packaging/oxidezap.metainfo.xml" \
    "$ROOT/packaging/appimage/AppRun" \
    "$ROOT/assets/oxidezap.svg" \
    "$ROOT/assets/oxidezap-64.png" \
    "$ROOT/assets/oxidezap-128.png" \
    "$ROOT/assets/oxidezap-256.png" \
    "$ROOT/assets/oxidezap-512.png" \
    "$ROOT/LICENSE"; do
    if [ ! -f "$asset" ]; then
        echo "error: missing asset: $asset" >&2
        exit 1
    fi
done

if ! command -v sha256sum >/dev/null 2>&1; then
    echo "error: sha256sum was not found" >&2
    exit 1
fi

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/oxidezap/appimage"
APPIMAGETOOL_PATH="$CACHE_DIR/appimagetool-${APPIMAGETOOL_VERSION}-x86_64.AppImage"
APPIMAGE_RUNTIME_PATH="$CACHE_DIR/runtime-${APPIMAGE_RUNTIME_VERSION}-x86_64"

download_file() {
    local url="$1"
    local destination="$2"
    local expected_sha256="$3"
    local description="$4"
    local partial="${destination}.part"

    mkdir -p "$(dirname "$destination")"
    echo "Downloading $description..."
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --connect-timeout 20 -o "$partial" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$partial" "$url"
    else
        echo "error: curl or wget is required to download $description" >&2
        exit 1
    fi
    printf '%s  %s\n' "$expected_sha256" "$partial" | sha256sum -c -
    chmod +x "$partial"
    mv -f "$partial" "$destination"
}

checksum_matches() {
    printf '%s  %s\n' "$2" "$1" | sha256sum -c - >/dev/null 2>&1
}

if [ -n "${APPIMAGETOOL:-}" ]; then
    if [ ! -x "$APPIMAGETOOL" ]; then
        echo "error: APPIMAGETOOL is not executable: $APPIMAGETOOL" >&2
        exit 1
    fi
    TOOL="$APPIMAGETOOL"
else
    if [ ! -x "$APPIMAGETOOL_PATH" ] ||
       ! checksum_matches "$APPIMAGETOOL_PATH" "$APPIMAGETOOL_SHA256"; then
        download_file "$APPIMAGETOOL_URL" "$APPIMAGETOOL_PATH" \
            "$APPIMAGETOOL_SHA256" "appimagetool ${APPIMAGETOOL_VERSION}"
    fi
    TOOL="$APPIMAGETOOL_PATH"
fi

if [ -n "${APPIMAGE_RUNTIME:-}" ]; then
    if [ ! -f "$APPIMAGE_RUNTIME" ]; then
        echo "error: APPIMAGE_RUNTIME does not exist: $APPIMAGE_RUNTIME" >&2
        exit 1
    fi
    RUNTIME="$APPIMAGE_RUNTIME"
else
    if [ ! -f "$APPIMAGE_RUNTIME_PATH" ] ||
       ! checksum_matches "$APPIMAGE_RUNTIME_PATH" "$APPIMAGE_RUNTIME_SHA256"; then
        download_file "$APPIMAGE_RUNTIME_URL" "$APPIMAGE_RUNTIME_PATH" \
            "$APPIMAGE_RUNTIME_SHA256" "AppImage runtime ${APPIMAGE_RUNTIME_VERSION}"
    fi
    RUNTIME="$APPIMAGE_RUNTIME_PATH"
fi

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    if git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
        SOURCE_DATE_EPOCH=$(git -C "$ROOT" log -1 --format=%ct)
    else
        SOURCE_DATE_EPOCH=0
    fi
fi
export SOURCE_DATE_EPOCH

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/oxidezap-appimage.XXXXXXXX")
cleanup() {
    if [ "$KEEP_APPDIR" -eq 1 ]; then
        echo "AppDir kept at $WORKDIR/OxideZap.AppDir" >&2
    else
        rm -rf "$WORKDIR"
    fi
}
trap cleanup EXIT

APPDIR="$WORKDIR/OxideZap.AppDir"
mkdir -p \
    "$APPDIR/usr/bin" \
    "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
    "$APPDIR/usr/share/metainfo" \
    "$APPDIR/usr/share/licenses/oxidezap"

install -m755 "$BINARY" "$APPDIR/usr/bin/oxidezap"
install -m755 "$ROOT/packaging/appimage/AppRun" "$APPDIR/AppRun"
install -m644 "$ROOT/packaging/io.github.artur282.OxideZap.desktop" \
    "$APPDIR/io.github.artur282.OxideZap.desktop"
install -m644 "$ROOT/packaging/io.github.artur282.OxideZap.desktop" \
    "$APPDIR/usr/share/applications/io.github.artur282.OxideZap.desktop"
install -m644 "$ROOT/assets/oxidezap.svg" \
    "$APPDIR/io.github.artur282.OxideZap.svg"
install -m644 "$ROOT/assets/oxidezap.svg" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.artur282.OxideZap.svg"
for size in 64 128 256 512; do
    install -Dm644 "$ROOT/assets/oxidezap-${size}.png" \
        "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/io.github.artur282.OxideZap.png"
done
install -Dm644 "$ROOT/packaging/oxidezap.metainfo.xml" \
    "$APPDIR/usr/share/metainfo/io.github.artur282.OxideZap.metainfo.xml"
install -Dm644 "$ROOT/LICENSE" \
    "$APPDIR/usr/share/licenses/oxidezap/LICENSE"
ln -s io.github.artur282.OxideZap.svg "$APPDIR/.DirIcon"

mkdir -p "$OUTPUT_DIR"
OUTPUT="$OUTPUT_DIR/${APP_NAME}-${VERSION}-${ARCH}.AppImage"
PARTIAL="${OUTPUT}.part"

echo "Building $OUTPUT"
# CI validates the AppStream file with appstreamcli. appimagetool 1.9.1 only
# looks for the legacy <id>.appdata.xml name, and its internal validation cannot
# see the desktop entry inside the AppDir, so skip its optional check.
rm -f "$PARTIAL"
ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" \
    --no-appstream --runtime-file "$RUNTIME" "$APPDIR" "$PARTIAL"
mv -f "$PARTIAL" "$OUTPUT"

echo "AppImage written to $OUTPUT"
