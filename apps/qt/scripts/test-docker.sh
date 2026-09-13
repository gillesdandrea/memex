#!/usr/bin/env bash
set -euo pipefail
app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workspace_dir="$(cd "$app_dir/../.." && pwd)"
checkout_id="$(printf '%s' "$workspace_dir" | cksum | awk '{print $1}')"
image="${MEMEX_QT_TEST_IMAGE:-memex-qt-test}"
target_volume="${MEMEX_QT_TARGET_VOLUME:-memex-qt-target-$checkout_id}"
docker build -t "$image" -f "$app_dir/Dockerfile" "$app_dir"
docker run --rm \
    --mount "type=bind,source=$workspace_dir,target=/workspace" \
    --mount "type=volume,source=memex-qt-cargo,target=/root/.cargo" \
    --mount "type=volume,source=$target_volume,target=/workspace/apps/qt/target" \
    "$image" bash /workspace/apps/qt/scripts/test-container.sh
