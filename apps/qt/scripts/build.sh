#!/usr/bin/env bash
set -euo pipefail
app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="${1:-release}"
case "$profile" in
    dev|release) ;;
    *) echo 'Usage: build.sh [dev|release]' >&2; exit 2 ;;
esac
case "$(uname -s)" in
    Linux) platform=linux ;;
    Darwin) platform=macos ;;
    *) echo 'Supported build hosts: Linux and macOS.' >&2; exit 2 ;;
esac
if command -v mbx >/dev/null 2>&1; then
    compiler=mbx
else
    echo 'Boxington is not installed; using Cargo without its compiler cache.' >&2
    compiler=cargo
fi
mkdir -p "$app_dir/build"
manifest="$(mktemp "$app_dir/build/compiler-messages.XXXXXX")"
staging="$(mktemp -d "$app_dir/build/package.XXXXXX")"
trap 'rm -f "$manifest"; rm -rf "$staging"' EXIT
# Cargo reports the actual executable path even when mbx manages the target dir.
if ! "$compiler" build --manifest-path "$app_dir/Cargo.toml" --locked \
    --profile "$profile" --bin memex-qt --message-format=json > "$manifest"; then
    jq -r 'select(.reason == "compiler-message") | .message.rendered // empty' "$manifest" >&2
    exit 1
fi
jq -r 'select(.reason == "compiler-message") | .message.rendered // empty' "$manifest" >&2
executable="$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "memex-qt" and .executable != null and .profile.test == false) | .executable' "$manifest")"
if [[ ! -f "$executable" ]]; then
    echo 'Cargo did not report a memex-qt executable.' >&2
    exit 1
fi
name="memex-qt-$platform-$(uname -m)-$profile"
mkdir "$staging/$name"
cp "$executable" "$staging/$name/memex-qt"
cp "$app_dir/README.md" "$app_dir/PARITY.md" "$app_dir/../../LICENSE" "$staging/$name/"
artifact="$app_dir/build/$name.tar.gz"
tar -czf "$artifact" -C "$staging" "$name"
printf '%s\n' "$artifact"
