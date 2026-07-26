# rem-exec

Agent-friendly remote process execution over SSH.

Run a command and get its exit code and output in one call, or start a process,
disconnect, and read output later. All responses are JSON. Designed as a
transport layer for AI agents: the request travels through the SSH channel's
stdin as framed JSON, so command arguments and input bytes reach the remote
verbatim — there is no remote shell to escape or inject into.

`rx put -` reads stdin, so it composes with whatever secret store you already
use — no dependency, no opinion:

```bash
pass show infra/db_password | rx put - host1:/run/secrets/db_pass --mode 0600
op read op://vault/item/f   | rx put - host1:/run/secrets/db_pass --mode 0600
rxv get host1/db_password   | rx put - host1:/run/secrets/db_pass --mode 0600
```

[rem-exec-vault](https://github.com/kwinsch/rem-exec-vault) (`rxv`) is a separate
tool built to the same contract — one JSON object per invocation, typed error
codes, the same exit codes ([docs/CONTRACT.md](docs/CONTRACT.md), duplicated
verbatim in both repositories) — so an agent that has learned `rx` can read it
without being taught twice. Using either tool commits you to nothing about the
other.

## Features

- **One contract** — every command emits exactly one JSON object with a `type`,
  on stdout, errors included; `code` is machine-branchable and `hint` names the
  fix. Exit 0/1/2 = succeeded / failed / malformed call
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
  is opt-in (`RX_AUTO_DEPLOY`), off by default
- **Connection reuse** — SSH multiplexing across operations to a host
- **Multi-arch** — static musl binaries for x86_64, aarch64, riscv64, armv7
- **Embedded skill file** — `rx skill` prints complete machine-readable docs

## Install

```bash
# Rust source install: installs rx and rxd locally.
cargo install rem-exec

# Download static rxd binaries for remote deployment.
rx cache fetch
```

`rx deploy HOST` downloads the matching `rxd` release asset itself if the local
cache lacks it, so `rx cache fetch` is only needed to pre-seed a cache — before
travelling to a site with no internet, or to warm all architectures at once:

```bash
rx cache fetch                  # cache every supported rxd architecture
rx cache fetch --arch x86_64    # one architecture
rx cache fetch --version v0.1.0 # a specific release

rx deploy host1 host2                   # several hosts in one call
rx deploy host --offline                # refuse to download; use the cache only
rx deploy host --binary ./target/…/rxd  # push a local build (no release needed)
```

Cached assets are stored per version (`rxd-v0.4.0-x86_64`), so upgrading `rx`
can never deploy the previous version's binary.

`rx` runs locally. `rxd` is the binary copied to remote hosts by `rx deploy` and
auto-deploy. Only the controller needs external tools installed — `ssh` for
every operation, plus `scp`/`curl`/`sha256sum` for `cache fetch` and deploy; the remote
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
rxv get host1/db_password | rx put - host:/run/secrets/db_pass --mode 0600
```

## Agent usage

Point the agent at the target host; it runs `rx skill` once to learn the tool,
then operates autonomously. Set `RX_AUTO_DEPLOY=on` when the agent should also
be allowed to install `rxd` on hosts it finds unprepared.

```bash
rx run host -- doas apt-get -y update
RX_AUTO_DEPLOY=on rx run host -- doas apt-get -y update   # deploys rxd if needed
```

By default `rx` never changes a host you did not point it at: a missing or
mismatched `rxd` is reported as a typed `not_deployed` error naming the command
that fixes it. `RX_AUTO_DEPLOY=local` allows that repair from the local cache
without any download; `=on` allows fetching too. There is no flag for it: whether
hosts may change under you belongs to the environment rx runs in, not to each
invocation. (The pre-0.4 `REM_EXEC_*` spellings still work.)

## Security notes

The trust boundary is the controller — the machine `rx` runs on, holding the SSH
keys and, when paired with [rem-exec-vault](https://github.com/kwinsch/rem-exec-vault),
an unlocked age identity.

- **HOST is validated, and never reaches `ssh` unterminated.** OpenSSH reads a
  destination beginning with `-` as an option, so `-oProxyCommand=...` would run
  an arbitrary command locally. `rx` rejects such destinations with a typed
  `bad_host` error, and every `ssh`/`scp` invocation passes `--` before the
  destination so the boundary holds even if a code path skips the check. This
  matters most when the host string comes from an inventory file, a ticket, or a
  model's output rather than from you.
- **Command, env, cwd and stdin never touch a remote shell.** They ride as JSON
  over the SSH channel; the remote login shell only ever starts `rxd serve`.
- **Deploy is atomic.** The binary is uploaded to a temp name and renamed into
  place, so a failed transfer cannot truncate the live `rxd` and a running one
  does not block the install with `ETXTBSY`.
- **Local state is private.** The base directory (`$XDG_RUNTIME_DIR/rem-exec`, or
  `/tmp/rem-exec-$uid`) is verified to be a non-symlink directory you own at mode
  0700 before it is used for ControlMaster sockets or the daemon socket. If it
  cannot be secured, multiplexing is dropped rather than used unsafely.
- **`get` never returns a torn file.** The body is bounded to the size announced
  in the header and the remote re-stats afterwards; a file that changed mid-read
  fails with `file_changed` and writes nothing locally.
- **Trusted PATH assumed on the controller.** `ssh`, `scp`, `curl` and
  `sha256sum` are resolved through `PATH`. This is a single-user-controller tool.

## Architecture

Two binaries:
- **rx** — local CLI, plus an optional local caching daemon (opt-in via
  `RX_DAEMON=1`; direct SSH is the default and canonical path)
- **rxd** — remote binary (static, no dependencies, deployed via `rx deploy`)

Communication flows over SSH. No custom ports, no daemons to manage on the remote.

## Distribution

- crates.io: `cargo install rem-exec`
- GitHub Releases: static `rx` and `rxd` binaries for x86_64, aarch64, riscv64, and armv7
- Auto-deploy cache: `~/.local/share/rem-exec/bin/rxd-{version}-{arch}` (`rx cache fetch`)

Release downloads include `SHA256SUMS`. Verify downloaded assets before use in
production workflows.

## License

MIT
