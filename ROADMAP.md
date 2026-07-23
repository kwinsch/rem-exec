# rem-exec roadmap

Shipped in 0.2.0: framed-JSON transport over `rxd serve` (no remote shell,
exact argv/stdin), `run` (sync with timeout→background), text-native output,
structured exit codes/signals, typed error codes, SSH connection multiplexing.

Shipped since 0.2.0:
- `--cwd` / `--env` on `run` and `start` — working dir + environment applied via
  chdir/setenv before exec, no shell wrapper.
- `rx wait HOST ID [--timeout]` — block server-side until exit or timeout,
  returning the same completed/running shapes as `run` (no client polling).
- `rx cp LOCAL HOST:PATH [--mode] [--owner] [--group]` — atomic streamed copy
  (temp → chmod/chown → rename), perms applied before the file is visible.
  `--mode` always works; `--owner`/`--group` need a privileged rxd.
- `run` ephemeral by default — fully-inlined `completed` deletes the process
  dir; skip when truncated or backgrounded; `--keep` retains state.
- Exec-failure legibility — when the command never starts (binary missing / not
  executable), `completed` carries a typed `exec_error`
  (`command_not_found`/`permission_denied`/`exec_format_error`/`errno_<n>`) with
  exit_code+signal null and a `rx: exec …` stderr line; `status` shows
  `exec_failed(reason)`. Distinguishes "tool isn't there" from a real 127.
- `rx ping HOST` — reachability + host identity `{version, protocol, arch, os,
  kernel, hostname, distro_id?, distro_version?}`, gathered natively in rxd
  (`uname(2)` + `/etc/os-release`, no remote shell). Pure probe, no state dir;
  honors auto-deploy (unset → `not_deployed` is the "deploy needed" signal). The
  distro fields are the useful bit (apk vs apt; Alpine ships busybox ash).
- Status-file race fix — status writes are atomic (temp→rename), so a concurrent
  poll never reads a truncated status and mis-reports a clean exit as
  `exited(unknown)`/null.
- Transfer completeness (cp + get) — the sender declares the byte count; the
  receiver installs the file (atomic temp→rename) only if exactly that many
  bytes arrive, else `incomplete_transfer`. Closes cp's old truncation gap (a
  dropped connection could atomically install a short file) and makes big SQL/DB
  copies safe both directions over flaky links. Constant memory, no size cap.
  Boundary: rx moves bytes faithfully, it does not snapshot a live DB — dump
  first (`sqlite3 .backup`, `pg_dump`).
- `rx get HOST:PATH LOCAL [--mode]` — streaming download, atomic local
  temp→rename, size-verified, source mode preserved unless overridden. rxd sends
  a one-line JSON header (size/mode or a typed error) then raw bytes; no base64.

Everything below is proposed, not committed. Ordered by agent-experience value.

## Nice-to-have

### `rx which HOST name...`  (companion to ping)
Stateless per-call tool-availability probe: rxd walks `$PATH` in Rust (no remote
shell) and returns `{name: path|null}`. Agent passes the tools it needs for the
task; no persistent "preferred tools" config (that drifts toward desired-state).
Kept separate from ping (ping is a fixed cheap round trip; which takes a list).

### Idempotency keys
Optional client-supplied key on `run`/`start` so a retried request after a
dropped connection reconciles to the same process instead of double-launching.

## Ruled out (anti-creep)

- Batch / sequence orchestration — document the pipe-a-script-to-`sh` pattern
  instead; building it re-invents a shell.
- Merged stdout+stderr view — split streams are fine for agents.
- Payload compression by default — keep the zero-C-dep static-musl story; SSH
  already compresses the channel, and rx owns the ssh invocation if we ever want
  to turn `-o Compression=yes` on. No C `zstd` dependency in the default build.
- Config-management / desired-state DSL — rx stays an imperative primitive.

## Maybe / later

- Live streaming for `run` (`--follow`): stream output while blocking, still
  ending with the structured result.
- `rx skill --json`: machine-readable schema of requests/responses for discovery.
