#!/bin/sh
# Builds both binaries. Run on Linux (or WSL).
#   CLI: static musl binary, no dependencies   -> target/x86_64-unknown-linux-musl/release/pxe-installer
#   GUI: GTK 4 app (dynamic)                   -> target/release/pxe-installer-gui
# GUI build deps (Ubuntu): sudo apt install libgtk-4-dev build-essential pkg-config
set -e
cargo test
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
if [ "$1" != "--cli-only" ]; then
  cargo build --release --features gui
  # keep the GUI next to the CLI so it can find it
  cp target/x86_64-unknown-linux-musl/release/pxe-installer target/release/pxe-installer
fi
ls -l target/release/pxe-installer* target/x86_64-unknown-linux-musl/release/pxe-installer
