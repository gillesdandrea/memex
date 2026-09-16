#!/usr/bin/env bash
# Build a self-contained Linux AppImage for the Qt app.
#
# Usage: build-appimage.sh [version]
#
# The AppImage bundles the memex-qt executable, its Qt runtime (via
# linuxdeploy-plugin-qt), and the memex CLI beside it, so the app finds
# the CLI through its existing beside-the-executable lookup. QML/JS stays
# embedded in the executable; only the Qt runtime modules are bundled.
#
# Requires a Linux host with Qt 6.10+ development packages (qmake on PATH),
# a compatible linker (LLD, Gold or Mold), curl, and jq.
# Release builds should run on the oldest supported distro (see the
# release workflow) so the AppImage keeps a low glibc baseline.
set -euo pipefail
app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
repo_root="$(cd "$app_dir/../.." && pwd)"
version="${1:-dev}"
case "$(uname -s)" in
    Linux) ;;
    *) echo 'AppImage packaging requires a Linux host.' >&2; exit 2 ;;
esac
case "$(uname -m)" in
    x86_64|aarch64) arch="$(uname -m)" ;;
    *) echo "Unsupported architecture for AppImage packaging: $(uname -m)" >&2; exit 2 ;;
esac
if ! command -v qmake6 >/dev/null 2>&1 && ! command -v qmake >/dev/null 2>&1; then
    echo 'qmake (Qt 6.10+) must be on PATH; see apps/qt/README.md.' >&2
    exit 2
fi
if command -v mbx >/dev/null 2>&1; then
    compiler=mbx
else
    echo 'Boxington is not installed; using Cargo without its compiler cache.' >&2
    compiler=cargo
fi

# Build a binary with Cargo and report its path, even when Boxington
# manages the target directory.
cargo_exe() {
    local manifest="$1" name="$2"
    shift 2
    local messages
    messages="$(mktemp "$app_dir/build/compiler-messages.XXXXXX")"
    trap 'rm -f "$messages"' RETURN
    if ! "$compiler" build --manifest-path "$manifest" --locked "$@" \
        --bin "$name" --message-format=json > "$messages"; then
        jq -r 'select(.reason == "compiler-message") | .message.rendered // empty' "$messages" >&2
        exit 1
    fi
    jq -r 'select(.reason == "compiler-message") | .message.rendered // empty' "$messages" >&2
    local executable
    executable="$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "'"${name}"'" and .executable != null and .profile.test == false) | .executable' "$messages" | head -n 1)"
    if [[ ! -f "${executable:-}" ]]; then
        echo "Cargo did not report a $name executable." >&2
        exit 1
    fi
    printf '%s\n' "$executable"
}

mkdir -p "$app_dir/build"
qt_exe="$(cargo_exe "$app_dir/Cargo.toml" memex-qt --profile release)"
cli_exe="$(cargo_exe "$repo_root/Cargo.toml" memex --release)"

bash "$app_dir/scripts/install-linuxdeploy.sh" "$app_dir/build/tools"
tools_bin="$app_dir/build/tools/bin"

staging="$(mktemp -d "$app_dir/build/appimage.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
appimage_dir="$staging/AppDir"
mkdir -p "$appimage_dir/usr/bin" "$appimage_dir/usr/share/doc/memex-qt"
cp "$qt_exe" "$appimage_dir/usr/bin/memex-qt"
cp "$cli_exe" "$appimage_dir/usr/bin/memex"
cp "$app_dir/packaging/memex-qt.desktop" "$appimage_dir/memex-qt.desktop"
cp "$app_dir/packaging/memex-qt.png" "$appimage_dir/memex-qt.png"
cp "$app_dir/README.md" "$app_dir/PARITY.md" "$repo_root/LICENSE" "$appimage_dir/usr/share/doc/memex-qt/"

# linuxdeploy-plugin-qt scans QML sources for imports; the app embeds its
# QML at runtime, so point the scanner at the source directory.
# XCB ships by default; offscreen is listed explicitly because the smoke
# test boots headless and must use the bundled plugin, not the host's.
export QML_SOURCES_PATHS="$app_dir/qml"
export EXTRA_PLATFORM_PLUGINS="libqxcb.so;libqoffscreen.so"
export PATH="$tools_bin:${PATH}"
(
    cd "$staging"
    "$tools_bin/linuxdeploy" \
        --appdir "$appimage_dir" \
        --executable "$appimage_dir/usr/bin/memex-qt" \
        --desktop-file "$appimage_dir/memex-qt.desktop" \
        --icon-file "$appimage_dir/memex-qt.png" \
        --plugin qt \
        --output appimage
)
produced="$(echo "$staging"/*.AppImage)"
if [[ ! -f "$produced" ]] || [[ "$produced" == *$'\n'* ]]; then
    echo 'linuxdeploy did not produce exactly one AppImage.' >&2
    exit 1
fi
artifact="$app_dir/build/memex-qt-${version}-linux-${arch}.AppImage"
mv "$produced" "$artifact"
# Record the basename so `sha256sum -c` works next to the download.
(cd "$app_dir/build" && sha256sum "$(basename "$artifact")" > "${artifact}.sha256")
printf '%s\n' "$artifact"
