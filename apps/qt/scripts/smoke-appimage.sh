#!/usr/bin/env bash
# Smoke-test a memex-qt AppImage without FUSE or a system Qt install:
# extract it, check the payload, and boot the app headless.
#
# Usage: smoke-appimage.sh <AppImage>
set -euo pipefail
case "$(uname -s)" in
    Linux) ;;
    *) echo 'AppImage smoke tests require a Linux host.' >&2; exit 2 ;;
esac
image_input="${1:?Usage: smoke-appimage.sh <AppImage>}"
image="$(cd "$(dirname "$image_input")" && pwd)/$(basename "$image_input")"
if [[ ! -f "$image" ]]; then
    echo "AppImage not found: $image_input" >&2
    exit 2
fi
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(cd "$work" && "$image" --appimage-extract >/dev/null)
root="$work/squashfs-root"
[[ -x "$root/AppRun" ]] || { echo 'AppRun is missing or not executable.' >&2; exit 1; }
[[ -x "$root/usr/bin/memex-qt" ]] || { echo 'Bundled memex-qt is missing.' >&2; exit 1; }
[[ -x "$root/usr/bin/memex" ]] || { echo 'Bundled memex CLI is missing.' >&2; exit 1; }
if ! ls "$root"/usr/lib/libQt6Core.so.6 >/dev/null 2>&1; then
    echo 'Bundled Qt runtime (libQt6Core) is missing.' >&2
    exit 1
fi
for plugin in libqoffscreen.so libqxcb.so; do
    if ! ls "$root"/usr/plugins/platforms/"$plugin" >/dev/null 2>&1; then
        echo "Bundled platform plugin is missing: $plugin" >&2
        exit 1
    fi
done
"$root/usr/bin/memex" --version >/dev/null
# Isolate from any Qt SDK on the host so the boot test can only pass with
# the bundled plugins, never with system ones masking a missing payload.
unset QT_PLUGIN_PATH QML2_IMPORT_PATH QML_IMPORT_PATH QT_QPA_PLATFORM_PLUGIN_PATH
export QT_QPA_PLATFORM=offscreen
export QT_QUICK_BACKEND=software
export XDG_RUNTIME_DIR="$work/runtime"
mkdir -p "$XDG_RUNTIME_DIR"
# Boot through the real AppImage runtime: --appimage-extract-and-run sets
# up the bundled library/plugin paths exactly as a user launch would.
# Executing the extracted AppRun directly would bypass that environment
# and resolve host libraries instead of the bundle.
ls -l "$image"
df -h /tmp "$work"
# A healthy GUI stays alive until killed; anything else is a boot failure.
set +e
timeout 25s "$image" --appimage-extract-and-run >"$work/boot.log" 2>&1
code=$?
set -e
if [[ $code -ne 124 ]]; then
    echo "App exited during boot (exit $code):" >&2
    cat "$work/boot.log" >&2
    echo '--- diagnostic boot: extracted AppRun with bundle env ---' >&2
    set +e
    APPDIR="$root" APPIMAGE="$image" \
        LD_LIBRARY_PATH="$root/usr/lib:${LD_LIBRARY_PATH:-}" \
        QT_PLUGIN_PATH="$root/usr/plugins" \
        QT_DEBUG_PLUGINS=1 timeout 15s "$root/AppRun" >"$work/direct.log" 2>&1
    direct=$?
    set -e
    echo "direct boot exit: $direct" >&2
    tail -n 50 "$work/direct.log" >&2 || true
    echo '--- bundled linkage (missing entries) ---' >&2
    LD_LIBRARY_PATH="$root/usr/lib" ldd "$root/usr/bin/memex-qt" 2>&1 | grep 'not found' >&2 || true
    echo '--- AppRun target, rpath, Qt resolution, plugin layout ---' >&2
    ls -la "$root/AppRun" >&2
    readelf -d "$root/usr/bin/memex-qt" 2>&1 | grep -Ei 'rpath|runpath' >&2 || echo '(no rpath/runpath)' >&2
    LD_LIBRARY_PATH="$root/usr/lib" ldd "$root/usr/bin/memex-qt" 2>&1 | grep -Ei 'libQt6(Core|Gui|Qml|Quick)|libstdc\+\+|libc\.so' >&2 || true
    ls "$root/usr/plugins/platforms/" >&2
    cat "$root/usr/bin/qt.conf" >&2 || echo '(no qt.conf)' >&2
    exit 1
fi
if grep -Ei 'module ".*" is not installed|QQmlApplicationEngine failed|Could not load the Qt platform plugin' "$work/boot.log"; then
    echo 'QML/platform boot errors detected (see matches above).' >&2
    exit 1
fi
echo "OK: $image extracts, bundles Qt + CLI, and stays up headless."
