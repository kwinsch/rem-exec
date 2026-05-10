#!/bin/bash
set -e

MUSL_PATH="${MUSL_PATH:?Set MUSL_PATH to your musl-cross-make output/bin directory}"
STORE="${STORE:-$HOME/.local/share/rem-exec/bin}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

[[ ! -d "$MUSL_PATH" ]] && echo "Error: musl toolchain not found at $MUSL_PATH" && exit 1

declare -A TARGETS=(
    [x86_64]="x86_64-unknown-linux-musl"
    [aarch64]="aarch64-unknown-linux-musl"
    [riscv64]="riscv64gc-unknown-linux-musl"
)

declare -A STRIP_PREFIX=(
    [x86_64]="x86_64-linux-musl"
    [aarch64]="aarch64-linux-musl"
    [riscv64]="riscv64-linux-musl"
)

mkdir -p "$STORE"

# Build rem-execd for all architectures
for arch in "${!TARGETS[@]}"; do
    target="${TARGETS[$arch]}"
    echo "Building rem-execd for $arch ($target)..."
    PATH="$MUSL_PATH:$PATH" cargo build --release --target "$target" --bin rem-execd
    "$MUSL_PATH/${STRIP_PREFIX[$arch]}-strip" "target/$target/release/rem-execd"
    cp "target/$target/release/rem-execd" "$STORE/rem-execd-$arch"
done

# Build rem-exec (local CLI) for host architecture
HOST_TARGET="x86_64-unknown-linux-musl"
echo "Building rem-exec for local host..."
PATH="$MUSL_PATH:$PATH" cargo build --release --target "$HOST_TARGET" --bin rem-exec
"$MUSL_PATH/x86_64-linux-musl-strip" "target/$HOST_TARGET/release/rem-exec"

mkdir -p "$INSTALL_DIR"
cp "target/$HOST_TARGET/release/rem-exec" "$INSTALL_DIR/"

echo ""
echo "Installed rem-exec to $INSTALL_DIR/"
echo "Binary store ($STORE):"
ls -lh "$STORE/"
echo ""
rem-exec --version
