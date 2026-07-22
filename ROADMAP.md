# rem-exec roadmap

Shipped in 0.2.0: framed-JSON transport over `rxd serve` (no remote shell,
exact argv/stdin), `run` (sync with timeout→background), text-native output,
structured exit codes/signals, typed error codes, SSH connection multiplexing.

Shipped since 0.2.0:
- `--cwd` / `--env` on `run` and `start` — working dir + environment applied via
  chdir/setenv before exec, no shell wrapper.
- `rx wait HOST ID [--timeout]` — block server-side until exit or timeout,
  returning the same completed/running shapes as `run` (no client polling).

Everything below is proposed, not committed. Ordered by agent-experience value.

## Near-term (high value, small)

### 1. `rx cp LOCAL HOST:REMOTE [--mode MODE] [--owner USER] [--group GRP]`
Copy a file to the remote with perms applied *before* it becomes visible.
- Value over `cat f | rx run host -- doas tee`: atomic (temp + rename), perms
  set on the temp file first (no world-readable window for secrets), binary-safe
  streaming, structured result.
- Wire: `Request::Put { path, mode, owner, group }`; file bytes ride as the
  request body (streamed, unbounded via the pipe path for large files).
- rxd: write to `path.tmp-<rand>` in the target dir, `fchmod(mode)`, best-effort
  `fchown(owner,group)`, `fsync`, `rename` into place.
- Caveat: `--mode` always works; `--owner`/`--group` need privilege (rxd runs as
  the SSH user) — document as best-effort, or require an elevated rxd.
- Consider a companion `rx get HOST:REMOTE LOCAL` for the reverse direction.

## Nice-to-have

### 2. Merged stdout+stderr view
Optional interleaved capture so causality (which line came before which) is
preserved for debugging. `run --merge`, or a combined field / a third capture
file written with a tee.

### 3. `run --ephemeral`
Auto-`clean` the process dir once a `run` returns fully inline (not truncated),
so agents running many short commands don't accumulate remote state. Skip
auto-clean when truncated (output still needed via `stdout`).

### 4. `rx ping HOST`
Fast typed reachability + `{version, protocol, arch}` in one round trip, so an
agent can check connectivity / whether a (re)deploy is needed before a batch.

### 5. Idempotency keys
Optional client-supplied key on `run`/`start` so a retried request after a
dropped connection reconciles to the same process instead of double-launching.

### 6. Batch / sequence
`run` a list of commands, stop on first non-zero, return per-step results —
or just document the "pipe a script to `sh`" pattern as the intended answer.

## Maybe / later

- Live streaming for `run` (`--follow`): stream output while blocking, still
  ending with the structured result.
- `rx skill --json`: machine-readable schema of requests/responses for discovery.
