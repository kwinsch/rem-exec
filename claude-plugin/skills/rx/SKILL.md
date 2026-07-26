---
name: rx
description: |
  Remote command execution and file transfer over SSH via the rx CLI. Use when
  running commands on remote hosts, transferring files to or from them, or
  managing detached remote processes. Every call returns one JSON object with
  typed errors and real exit codes, and argv/stdin travel verbatim — prefer rx
  over ad-hoc `ssh host '…'` when you need clean machine-readable results or
  there is anything to quote. One tool in the box, not a replacement for local
  tools.
allowed-tools: Bash(rx:*)
---

# rx — remote process execution over SSH

Confirm the binary first: `rx --version`. If it is missing, install it
(`cargo install rem-exec`, or a release binary from
https://github.com/kwinsch/rem-exec/releases), then invoke this skill again —
the guide below is emitted by the installed binary, so it always matches the
version on this machine.

!`rx skill`

rxv (rem-exec-vault) is a separate secret-store tool built to the same JSON
output contract; if it is installed, its own skill applies. Neither tool
requires the other.
