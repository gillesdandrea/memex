#!/usr/bin/env bash
set -euo pipefail
# Shared by the local Docker image and the Fedora CI container.
# openssl-devel is for the bundled memex CLI (openssl-sys), not the Qt app.
dnf install -y --setopt=install_weak_deps=False \
    gcc-c++ git curl ca-certificates rust cargo clippy rustfmt lld \
    openssl-devel pkg-config \
    qt6-qtbase-devel qt6-qtbase-private-devel qt6-qtdeclarative-devel \
    qt6-qtdeclarative qt6-qtquickcontrols2-devel \
    xorg-x11-server-Xvfb mesa-dri-drivers tar gzip zstd jq file
