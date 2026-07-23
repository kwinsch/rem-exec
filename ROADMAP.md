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

Everything below is proposed, not committed. Ordered by agent-experience value.

## Nice-to-have

### 1. `rx get HOST:PATH LOCAL`
Reverse of `cp`: stream a remote file down. Pairs with `cp` for round-tripping
config/artifacts.

### 2. Merged stdout+stderr view
Optional interleaved capture so causality (which line came before which) is
preserved for debugging. `run --merge`, or a combined field / a third capture
file written with a tee.

### 3. `rx ping HOST`
Fast typed reachability + `{version, protocol, arch}` in one round trip, so an
agent can check connectivity / whether a (re)deploy is needed before a batch.

### 4. Idempotency keys
Optional client-supplied key on `run`/`start` so a retried request after a
dropped connection reconciles to the same process instead of double-launching.

### 5. Batch / sequence
`run` a list of commands, stop on first non-zero, return per-step results —
or just document the "pipe a script to `sh`" pattern as the intended answer.

### 6. Payload compression (cp + large reads)
Compress large text payloads on the wire — real win over WireGuard between sites.
Position, not yet decided:
- Do NOT make a C `zstd` dependency the default: it fights the tool's core value
  (zero-C-dep, trivially static-musl for x86_64/aarch64/riscv64) and adds a codec
  attack surface. libzstd via `cc` complicates the riscv64/aarch64 musl builds.
- Free ~80% of the benefit first: SSH already compresses, and rx owns the ssh
  invocation — add `-o Compression=yes` (opt-in `--compress`, or default-on for
  `cp`). zlib is weaker than zstd but costs nothing and works today.
- If we want zstd specifically, put it behind a cargo feature flag with a
  per-payload `compression: none|zstd` wire marker (mirrors the utf8/base64
  `encoding` field), applied to both `cp` bodies and large output reads — not
  just cp. Keep the default build dependency-free. zstd's stored-block behavior
  means already-compressed data isn't penalized, so "always on when enabled" is
  safe.

## Maybe / later

- Live streaming for `run` (`--follow`): stream output while blocking, still
  ending with the structured result.
- `rx skill --json`: machine-readable schema of requests/responses for discovery.
