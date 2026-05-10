#!/bin/bash
set -e
TARGET="x86_64-unknown-linux-musl"
BINARY="target/$TARGET/release/rem-execd"

[[ ! -f "$BINARY" ]] && echo "Build first: ./install.sh" && exit 1

for host in "$@"; do
    echo "Deploying rem-execd to $host..."
    ssh "$host" 'mkdir -p ~/.local/bin'
    scp "$BINARY" "$host":~/.local/bin/rem-execd
    ssh "$host" 'rem-execd --version 2>/dev/null || ~/.local/bin/rem-execd --version'
done
