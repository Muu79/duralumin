#!/usr/bin/env bash
# Build .deb packages for amd64 and arm64 and place them at predictable paths
# so cargo-dist can pick them up as extra-artifacts.
#
# Runs inside the cargo-dist global-artifacts job (ubuntu-22.04, root of repo).
set -euo pipefail

cargo install cargo-deb --locked

mkdir -p dist/deb

# ---- amd64 (native) -------------------------------------------------------
cargo deb -p duralumin-cli
find target/debian -maxdepth 1 -name "*.deb" -exec cp {} dist/deb/dura-amd64.deb \;

# ---- arm64 (cross-compiled) -----------------------------------------------
# Only gcc-aarch64-linux-gnu is needed: sqlite is bundled, rustls uses ring
# (pure Rust), and arboard's wayland backend uses dlopen at runtime.
rustup target add aarch64-unknown-linux-gnu
sudo apt-get install -y gcc-aarch64-linux-gnu

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc

# --no-strip: host `strip` can't process aarch64 ELFs; the release profile
# already excludes debug info so the size hit is negligible.
cargo deb -p duralumin-cli --target aarch64-unknown-linux-gnu --no-strip
find target/aarch64-unknown-linux-gnu/debian -maxdepth 1 -name "*.deb" \
    -exec cp {} dist/deb/dura-arm64.deb \;

# ---- systemd service file -------------------------------------------------
cp contrib/dura.service dist/deb/dura.service
