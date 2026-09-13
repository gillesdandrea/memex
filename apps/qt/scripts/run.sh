#!/usr/bin/env bash
set -euo pipefail
app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if command -v mbx >/dev/null 2>&1; then
    exec mbx run --manifest-path "$app_dir/Cargo.toml" --bin memex-qt "$@"
fi
echo 'Boxington is not installed; using Cargo without its compiler cache.' >&2
exec cargo run --manifest-path "$app_dir/Cargo.toml" --bin memex-qt "$@"
