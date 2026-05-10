#!/bin/bash
set -e
# Thin wrapper around `rem-exec deploy`. Detects arch and deploys automatically.
for host in "$@"; do
    rem-exec deploy "$host"
done
