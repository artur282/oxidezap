#!/usr/bin/env bash
set -eu

APP_NAME="OxideZap"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"
APPDIR="$PREFIX/share/applications"
ICONDIR="$PREFIX/share/icons/hicolor/scalable/apps"
METAINFODIR="$PREFIX/share/metainfo"
AUTOSTART_FILE="${XDG_CONFIG_HOME:-$HOME/.config}/autostart/oxidezap.desktop"

usage() {
    cat <<EOF
Install $APP_NAME into a user-local prefix.

Usage: ./install.sh [--build|--no-build] [--uninstall]

Options:
  --build      Build the release binary before installing
  --no-build   Fail instead of building when the binary is missing
  --uninstall  Remove installed files; user data is kept
  -h, --help   Show this help

Environment:
  PREFIX       Install prefix (default: $HOME/.local)
EOF
}

refresh_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$APPDIR" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

BUILD_MODE="auto"
UNINSTALL=0

for arg in "$@"; do
    case "$arg" in
        --build) BUILD_MODE="always" ;;
        --no-build) BUILD_MODE="never" ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$UNINSTALL" -eq 1 ]; then
    rm -f \
        "$BINDIR/oxidezap" \
        "$APPDIR/oxidezap.desktop" \
        "$ICONDIR/oxidezap.svg" \
        "$METAINFODIR/io.github.artur282.OxideZap.metainfo.xml" \
        "$AUTOSTART_FILE"
    refresh_caches
    echo "Removed $APP_NAME from $PREFIX."
    echo "Your settings and account data were kept:"
    echo "  ${XDG_CONFIG_HOME:-$HOME/.config}/oxidezap"
    echo "  ${XDG_DATA_HOME:-$HOME/.local/share}/oxidezap"
    echo "  ${XDG_CACHE_HOME:-$HOME/.cache}/oxidezap"
    exit 0
fi

build_binary() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "error: cargo was not found." >&2
        echo "Install Rust from https://rustup.rs or provide target/release/oxidezap." >&2
        exit 1
    fi
    (cd "$SCRIPT_DIR" && cargo build --release)
}

case "$BUILD_MODE" in
    always)
        build_binary
        ;;
    auto)
        if [ ! -x "$SCRIPT_DIR/target/release/oxidezap" ] &&
           [ ! -x "$SCRIPT_DIR/oxidezap" ]; then
            build_binary
        fi
        ;;
    never)
        ;;
esac

if [ -x "$SCRIPT_DIR/target/release/oxidezap" ]; then
    BINARY="$SCRIPT_DIR/target/release/oxidezap"
elif [ -x "$SCRIPT_DIR/oxidezap" ]; then
    BINARY="$SCRIPT_DIR/oxidezap"
else
    echo "error: release binary not found." >&2
    echo "Build it with: cargo build --release" >&2
    exit 1
fi

DESKTOP_SOURCE="$SCRIPT_DIR/packaging/oxidezap.desktop"
ICON_SOURCE="$SCRIPT_DIR/assets/oxidezap.svg"
METAINFO_SOURCE="$SCRIPT_DIR/packaging/oxidezap.metainfo.xml"

for file in "$DESKTOP_SOURCE" "$ICON_SOURCE"; do
    if [ ! -f "$file" ]; then
        echo "error: missing $file" >&2
        exit 1
    fi
done

EXEC_PATH="$BINDIR/oxidezap"
case "$EXEC_PATH" in
    *[[:space:]]*)
        echo "error: PREFIX must not contain whitespace." >&2
        exit 1
        ;;
esac

mkdir -p "$BINDIR" "$APPDIR" "$ICONDIR" "$METAINFODIR"

install -m755 "$BINARY" "$BINDIR/oxidezap"
install -m644 "$ICON_SOURCE" "$ICONDIR/oxidezap.svg"

if [ -f "$METAINFO_SOURCE" ]; then
    install -m644 "$METAINFO_SOURCE" \
        "$METAINFODIR/io.github.artur282.OxideZap.metainfo.xml"
fi

TMP_DESKTOP=$(mktemp)
trap 'rm -f "$TMP_DESKTOP"' EXIT
EXEC_LINE="Exec=$EXEC_PATH %u" awk '
    /^Exec=/ { print ENVIRON["EXEC_LINE"]; next }
    { print }
' "$DESKTOP_SOURCE" > "$TMP_DESKTOP"
install -m644 "$TMP_DESKTOP" "$APPDIR/oxidezap.desktop"
rm -f "$TMP_DESKTOP"
trap - EXIT

refresh_caches

if command -v xdg-mime >/dev/null 2>&1; then
    if xdg-mime default oxidezap.desktop x-scheme-handler/whatsapp >/dev/null 2>&1; then
        MIME_STATUS="registered"
    else
        MIME_STATUS="not registered (xdg-mime failed)"
    fi
else
    MIME_STATUS="not registered (xdg-mime not found)"
fi

echo "Installed $APP_NAME:"
echo "  binary:   $BINDIR/oxidezap"
echo "  desktop:  $APPDIR/oxidezap.desktop"
echo "  icon:     $ICONDIR/oxidezap.svg"
if [ -f "$METAINFO_SOURCE" ]; then
    echo "  metainfo: $METAINFODIR/io.github.artur282.OxideZap.metainfo.xml"
fi
echo "  whatsapp: $MIME_STATUS"
echo

case ":$PATH:" in
    *":$BINDIR:"*)
        echo "Run: oxidezap"
        ;;
    *)
        echo "note: $BINDIR is not in your PATH."
        echo "Add it with:"
        echo "  export PATH=\"$BINDIR:\$PATH\""
        echo "Then run: oxidezap"
        ;;
esac
