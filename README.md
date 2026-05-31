# rem-exec

Agent-friendly remote process execution over SSH.

Start processes on remote hosts, pipe data in, read output later. All responses are JSON. Designed as a transport layer for AI agents that need to run commands on remote machines without fighting SSH argument escaping.

## Features

- **Persistent processes** — start a command, disconnect, read output later
- **Stdin piping** — pipe complex data (scripts, configs) without shell escaping issues
- **Bidirectional pipe mode** — `stdin→remote`, `remote stdout→local stdout`
- **Auto-deploy** — detects remote architecture, deploys the correct binary automatically
- **Multi-arch** — static musl binaries for x86_64, aarch64, riscv64
- **JSON protocol** — every response is structured, parseable, predictable
- **Embedded skill file** — `rx skill` prints complete machine-readable documentation

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

# Run a command
rx start host uname -a
rx stdout host <id>

# Pipe a script (no escaping needed)
cat script.sh | rx start host sh

# Bidirectional pipe
echo "input" | rx start --pipe host ./process.sh
```

## Agent usage

Set `REM_EXEC_AUTO_DEPLOY=1` and point the agent at the target host. The agent runs `rx skill` once to learn the tool, then operates autonomously.

```bash
export REM_EXEC_AUTO_DEPLOY=1
rx start host doas apt update    # auto-deploys if needed, elevates via doas
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
