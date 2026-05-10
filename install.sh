#!/bin/bash
set -e
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
TARGET="x86_64-unknown-linux-musl"
MUSL_PATH="${MUSL_PATH:?Set MUSL_PATH to your musl-cross-make output/bin directory}"

[[ ! -d "$MUSL_PATH" ]] && echo "Error: musl toolchain not found at $MUSL_PATH" && exit 1

echo "Building rem-exec + rem-execd..."
PATH="$MUSL_PATH:$PATH" cargo build --release --target "$TARGET"

strip "target/$TARGET/release/rem-exec"
strip "target/$TARGET/release/rem-execd"

mkdir -p "$INSTALL_DIR"
cp "target/$TARGET/release/rem-exec" "$INSTALL_DIR/"
cp "target/$TARGET/release/rem-execd" "$INSTALL_DIR/"

echo "Installed:"
ls -lh "$INSTALL_DIR/rem-exec" "$INSTALL_DIR/rem-execd"
rem-exec --version
