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

Shipped in 0.3.0:
- `rx put` — `cp` renamed (kept as an undocumented alias), so the transfer pair
  reads `put`/`get`. `put -` streams stdin: a value goes from a pipeline to a
  remote file without a local temp file or an argv, at 0600 before the file is
  visible. `rxv get host1/db_password | rx put - host1:/run/secrets/db_pass`.
  Since stdin has no length known in advance, the body is length-framed with an
  end marker (`src/framing.rs`) — a severed stream ends without its marker and is
  rejected as `incomplete_transfer`, exactly as a short sized transfer is.
  Carried as a separate `put_stream` action rather than a flag on `put`: an older
  rxd ignores unknown *fields* (and would write the frame headers into the file)
  but rejects an unknown *action*. Boundary worth stating: put guarantees "what
  rx read landed, or nothing landed" — the remote does write plaintext to disk
  (that is the point), and a pipe cannot tell a producer that finished from one
  that died.
- Empty-stream guard — a zero-byte stdin transfer is refused (`empty_stream`,
  not retryable) unless `--allow-empty`. A failed producer sends a *well-formed*
  stream of zero bytes, so framing cannot catch it; installing it would blank a
  good secret and report success.
- Transport error fidelity — a body-write failure no longer masks the receiver's
  answer. rxd answering early (unwritable target) closed the pipe and rx reported
  `broken pipe`, so error quality depended on whether the payload fit in the pipe
  buffer; it also stopped auto-deploy from firing for large files against a host
  with no rxd.
- Deploy policy — `--auto-deploy=off|local|on` (env `REM_EXEC_AUTO_DEPLOY`,
  `1` still means on), default **off**. rx and rxd are halves of one protocol, so
  there is no separate version to pin; the pin is the rx binary, and what an
  operator controls is when *hosts* change. `off` never deploys as a side effect,
  `local` repairs from the cache without downloading, `on` may fetch. Explicit
  `rx deploy` is always allowed and completes the job it was given.
- Deploy workflow — `rx deploy HOST...` takes several hosts, fetches the matching
  asset when the cache lacks it, `--offline` refuses to download, `--binary PATH`
  pushes a local build (so an unreleased rxd can be tested). The cache is keyed
  by version, so an upgraded rx can no longer deploy the previous version's
  binary and fail *after* overwriting the remote. rx refuses to replace an rxd
  that is ahead of it — later protocol, or a later build of the same protocol —
  without `--allow-downgrade`; that host belongs to a newer rx, and repairing
  this one by breaking that one is not a trade rx gets to make silently.
  Version comparisons only fire when the ordering is provable, so an unreadable
  version never causes a needless deploy nor blocks an explicit repair.
- `ping` reports `up_to_date` + `local_version`. Version skew belongs in the
  health probe, not as a warning on every command: an older rxd is still correct
  for nearly every request, and a per-command warning trains an agent to ignore
  it.
- Modes are octal strings (`"0600"`) in `copied`/`got`/`get_stream`, matching
  what `--mode` accepts. A decimal `384` is the same number and unreadable.

Everything below is proposed, not committed. Ordered by agent-experience value.

## Next

### Declared capabilities instead of inferred compatibility

Compatibility is currently inferred from the protocol number, which answers
"can we talk" but not "does this host support the request I am about to send".
`put_stream` exposed the gap: it was additive, so protocol 2 stayed 2, and a
0.2.x rxd looks current right up until that one request fails. The stopgap is
`put -`'s own version check.

As the wire settles, additive changes become the normal kind, so this recurs
per feature and protocol equality over-approximates a little more each time —
while exact-version equality over-constrains, forcing a fleet redeploy for
patches that changed nothing you use. Both are proxies for the real question.

Answer it directly: have `version`/`ping` declare a monotonic capability level
(or a feature list), and let each request state its floor. `put -`'s pre-flight
then stops being a special case and becomes the general rule, correctly scoped —
deploy when the host genuinely lacks the capability, not when a digit differs.
Worth building at the second or third additive action, not the first.

### Deploy cache pruning

Keying the cache by version (0.3.0) fixed deploying the previous release's
binary, but nothing removes the old entries: the store grows by one set per
release — up to four arches, ~4.5 MB — and 0.2.x's unversioned `rxd-<arch>`
files are orphaned outright by the rename, since only `rxd-<version>-<arch>` is
ever read now.

Prune from `rx setup`: drop entries older than the running rx, plus the
unversioned leftovers. Keeping the current version and one predecessor is the
useful shape — the predecessor is what you deploy when rolling a host back —
so this is "keep N", not "delete everything else". `--prune-all` for reclaiming
the lot. Worth doing before the cache has enough versions in it to matter.

### Stale transfer temps

If rxd is killed mid-transfer (SIGHUP on a severed session, not the ordinary
dropped-connection case, which already cleans up) a `.rxd-put-*.tmp` survives in
the target directory holding partial content at 0600. Sweep temps older than an
hour on the next put into that directory, or from `rx clean`.

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
