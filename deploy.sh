#!/bin/bash
set -e
# Thin wrapper around `rx deploy`. Detects arch and deploys automatically.
for host in "$@"; do
    rx deploy "$host"
done
