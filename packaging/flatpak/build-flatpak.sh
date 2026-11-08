#!/usr/bin/env bash
set -euo pipefail

APP_ID="io.github.artur282.OxideZap"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
MANIFEST="$ROOT/packaging/flatpak/$APP_ID.yml"
OUTPUT_DIR="$ROOT/dist"
BUILD_DIR="$ROOT/target/flatpak-build"
REPO_DIR="$ROOT/target/flatpak-repo"
STATE_DIR="$ROOT/target/flatpak-state"
RUNTIME_REPO="https://flathub.org/repo/flathub.flatpakrepo"

usage() {
    cat <<EOF
Build an OxideZap Flatpak bundle.

Usage: packaging/flatpak/build-flatpak.sh [--output-dir DIR]

Options:
  --output-dir DIR   Write the .flatpak bundle here (default: dist)
  -h, --help         Show this help

Environment:
  VERSION                       Bundle version (default: v + Cargo.toml version)
  INSTALL_DEPS_FROM_FLATHUB     Set to 1 to let flatpak-builder install the
                                GNOME runtime and Rust SDK extension from Flathub

The build downloads the Cargo dependencies pinned in Cargo.lock. It needs the
Flathub remote and the org.gnome.Sdk//51 runtime:

  flatpak remote-add --user --if-not-exists flathub \\
    https://flathub.org/repo/flathub.flatpakrepo
  flatpak install --user flathub org.gnome.Sdk//51 org.gnome.Platform//51
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output-dir)
            if [ "$#" -lt 2 ]; then
                echo "error: --output-dir needs a directory" >&2
                exit 2
            fi
            OUTPUT_DIR="$2"
            shift 2
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

if ! command -v flatpak-builder >/dev/null 2>&1; then
    echo "error: flatpak-builder was not found." >&2
    echo "Install it with: sudo apt install flatpak-builder" >&2
    exit 1
fi

if [ ! -f "$MANIFEST" ]; then
    echo "error: missing manifest: $MANIFEST" >&2
    exit 1
fi

CARGO_VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
if [ -z "${VERSION:-}" ]; then
    if [ -z "$CARGO_VERSION" ]; then
        echo "error: could not determine a version; set VERSION" >&2
        exit 1
    fi
    VERSION="v$CARGO_VERSION"
fi

mkdir -p "$OUTPUT_DIR" "$REPO_DIR"

BUILDER_ARGS=(--user --state-dir="$STATE_DIR" --force-clean --repo="$REPO_DIR")
if [ "${INSTALL_DEPS_FROM_FLATHUB:-0}" = "1" ]; then
    BUILDER_ARGS+=(--install-deps-from=flathub)
fi

echo "Building $APP_ID from $MANIFEST"
flatpak-builder "${BUILDER_ARGS[@]}" "$BUILD_DIR" "$MANIFEST"

ARCH=$(flatpak --default-arch)
OUTPUT="$OUTPUT_DIR/oxidezap-${VERSION}-${ARCH}.flatpak"
PARTIAL="${OUTPUT}.part"

rm -f "$PARTIAL"
flatpak build-bundle \
    --runtime-repo="$RUNTIME_REPO" \
    "$REPO_DIR" "$PARTIAL" "$APP_ID"
mv -f "$PARTIAL" "$OUTPUT"

echo "Flatpak bundle written to $OUTPUT"
