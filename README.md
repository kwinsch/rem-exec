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
- **Safe file transfer** — `rx put`/`rx get` stream whole files both ways with
  constant memory and no size cap; the receiver installs the file only if the
  whole transfer arrived, so a dropped connection never leaves a truncated file
  (safe for large SQL/DB copies)
- **Pipe straight to a remote file** — `rx put -` streams stdin, applying the
  mode before the file is visible; a secret can go from a pipeline to
  `/run/secrets/…` at 0600 without touching local disk or an argv
- **Stdin piping** — stream scripts, configs, or binary blobs unbounded
- **Bidirectional pipe mode** — `stdin→remote`, `remote stdout→local stdout`
- **Deploy you control** — `rx deploy` detects the remote architecture and
  fetches the matching binary; deploying as a *side effect* of another command
  is opt-in (`--auto-deploy`), off by default
- **Connection reuse** — SSH multiplexing across operations to a host
- **Multi-arch** — static musl binaries for x86_64, aarch64, riscv64, armv7
- **Embedded skill file** — `rx skill` prints complete machine-readable docs

## Install

```bash
# Rust source install: installs rx and rxd locally.
cargo install rem-exec

# Download static rxd binaries for remote deployment.
rx setup
```

`rx deploy HOST` downloads the matching `rxd` release asset itself if the local
cache lacks it, so `rx setup` is only needed to pre-seed a cache — before
travelling to a site with no internet, or to warm all architectures at once:

```bash
rx setup                  # cache every supported rxd architecture
rx setup --arch x86_64    # one architecture
rx setup --version v0.1.0 # a specific release

rx deploy host1 host2                   # several hosts in one call
rx deploy host --offline                # refuse to download; use the cache only
rx deploy host --binary ./target/…/rxd  # push a local build (no release needed)
```

Cached assets are stored per version (`rxd-v0.3.0-x86_64`), so upgrading `rx`
can never deploy the previous version's binary.

`rx` runs locally. `rxd` is the binary copied to remote hosts by `rx deploy` and
auto-deploy. Only the controller needs external tools installed — `ssh` for
every operation, plus `scp`/`curl`/`sha256sum` for setup and deploy; the remote
needs nothing but the single static `rxd` binary.

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

# Transfer whole files both ways (verified atomic write; safe for big files)
rx put ./app.conf host:/etc/app/app.conf --mode 0640
rx get host:/var/backups/appdb.dump ./appdb.dump

# Pipe straight into a remote file — the plaintext never hits local disk,
# and the mode is applied before the file becomes visible
set -o pipefail
secret-store get db_password | rx put - host:/run/secrets/db_pass --mode 0600
```

## Agent usage

Point the agent at the target host; it runs `rx skill` once to learn the tool,
then operates autonomously. Add `--auto-deploy=on` when the agent should also
be allowed to install `rxd` on hosts it finds unprepared.

```bash
rx run host -- doas apt-get -y update
rx --auto-deploy=on run host -- doas apt-get -y update   # deploys rxd if needed
```

By default `rx` never changes a host you did not point it at: a missing or
mismatched `rxd` is reported as a typed `not_deployed` error naming the command
that fixes it. `--auto-deploy=local` allows that repair from the local cache
without any download; `=on` allows fetching too. The env var
`REM_EXEC_AUTO_DEPLOY` sets the same policy.

## Architecture

Two binaries:
- **rx** — local CLI, plus an optional local caching daemon (opt-in via
  `REM_EXEC_DAEMON=1`; direct SSH is the default and canonical path)
- **rxd** — remote binary (static, no dependencies, deployed via `rx deploy`)

Communication flows over SSH. No custom ports, no daemons to manage on the remote.

## Distribution

- crates.io: `cargo install rem-exec`
- GitHub Releases: static `rx` and `rxd` binaries for x86_64, aarch64, riscv64, and armv7
- Auto-deploy cache: `~/.local/share/rem-exec/bin/rxd-{arch}` (`rx setup`)

Release downloads include `SHA256SUMS`. Verify downloaded assets before use in
production workflows.

## License

MIT
