#!/usr/bin/env bash
set -euo pipefail
app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$app_dir"
export QT_QPA_PLATFORM=offscreen
export QT_QUICK_BACKEND=software
export XDG_RUNTIME_DIR
XDG_RUNTIME_DIR="$(mktemp -d)"
trap 'rm -rf "$XDG_RUNTIME_DIR"' EXIT
cargo fmt --check
if command -v mbx >/dev/null 2>&1; then
    compiler=mbx
else
    echo 'Boxington is not installed in this container; using Cargo with the isolated Docker target volume.'
    compiler=cargo
fi
"$compiler" test --locked --features test-support
"$compiler" clippy --locked --all-targets --features test-support -- -D warnings
qmltestrunner -input tests/tst_presentation.qml
