# rem-exec

Agent-friendly remote process execution over SSH.

Run a command and get its exit code and output in one call, or start a process,
disconnect, and read output later. All responses are JSON. Designed as a
transport layer for AI agents: the request travels through the SSH channel's
stdin as framed JSON, so command arguments and input bytes reach the remote
verbatim — there is no remote shell to escape or inject into.

## Features

- **Run-to-completion** — `rx run` blocks up to a timeout, returns exit code +
  stdout + stderr in one response; long commands auto-background into a handle
- **Persistent processes** — `rx start` a command, disconnect, read output later
- **Exact transport** — argv and stdin are carried as framed JSON, never parsed
  by a remote shell; no quoting, no metacharacter injection, binary-safe
- **Text-native output** — UTF-8 output is returned as text; only real binary is
  base64-encoded (`encoding` field says which)
- **Structured results** — integer exit codes, signal numbers, typed error codes
- **Stdin piping** — stream scripts, configs, or binary blobs unbounded
- **Bidirectional pipe mode** — `stdin→remote`, `remote stdout→local stdout`
- **Auto-deploy** — detects remote architecture, deploys the correct binary
- **Connection reuse** — SSH multiplexing across operations to a host
- **Multi-arch** — static musl binaries for x86_64, aarch64, riscv64
- **Embedded skill file** — `rx skill` prints complete machine-readable docs

## Install

```bash
# Rust source install: installs rx and rxd locally.
cargo install rem-exec

# Download static rxd binaries for remote deployment.
rx setup
```

For remote deployment, `rx deploy` needs a static `rxd` binary cache. `rx setup`
downloads and verifies the GitHub Release binaries for x86_64, aarch64, and
riscv64:

```bash
rx setup                  # all supported rxd architectures
rx setup --arch x86_64    # one architecture
rx setup --version v0.1.0 # specific release
```

`rx` runs locally. `rxd` is the binary copied to remote hosts by `rx deploy` and
auto-deploy.

## Quick start

```bash
# Deploy to a remote host
rx deploy host

# Run a command to completion — exit code + output in one JSON response
# (ephemeral by default: remote process state is removed after a fully-inlined result)
rx run host -- uname -a
rx run --keep host -- uname -a   # retain process dir for later stdout/status

# Feed stdin and collect the result together
printf 'c\na\nb\n' | rx run host -- sort

# Start a long-lived process, read output later
id=$(rx start host -- journalctl -fu nginx | jq -r .id)
rx stdout host "$id" --offset 0

# Pipe a script (no escaping needed); bidirectional pipe
cat script.sh | rx start host -- sh
echo "input" | rx start --pipe host -- ./process.sh
```

## Agent usage

Set `REM_EXEC_AUTO_DEPLOY=1` and point the agent at the target host. The agent runs `rx skill` once to learn the tool, then operates autonomously.

```bash
export REM_EXEC_AUTO_DEPLOY=1
rx run host -- doas apt-get -y update    # auto-deploys if needed, elevates via doas
```

## Architecture

Two binaries:
- **rx** — local CLI + optional caching daemon
- **rxd** — remote binary (static, no dependencies, deployed via `rx deploy`)

Communication flows over SSH. No custom ports, no daemons to manage on the remote.

## Distribution

- crates.io: `cargo install rem-exec`
- GitHub Releases: static `rx` and `rxd` binaries for x86_64, aarch64, and riscv64
- Auto-deploy cache: `~/.local/share/rem-exec/bin/rxd-{arch}` (`rx setup`)

Release downloads include `SHA256SUMS`. Verify downloaded assets before use in
production workflows.

## License

MIT
