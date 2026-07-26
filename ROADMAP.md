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

Shipped in 0.4.0 — the shared rx/rxv contract (`docs/CONTRACT.md`, duplicated
verbatim in rem-exec-vault):
- Every exit path answers with one typed JSON object on stdout. Nineteen call
  sites still printed bare stderr text — a bad `--mode`, a destination that was
  not `HOST:PATH`, an unparseable `--env` — so a documented invariant was false
  for exactly the failures a caller hits first. `tests/cli_contract.rs` now
  drives each one, because the invariant drifted for lack of anything checking.
- `rx start` transport failures are classified like every other command's
  (`ssh_unreachable` / `not_deployed`); it was the one path that bypassed
  `transport_error_json` and reported an untyped line.
- **Arguments are validated before stdin is read.** `rx run HOST --env BAD --
  cmd` used to block forever on an idle pipe — an agent harness has no terminal,
  so stdin is captured, and the call was rejected only after EOF that never
  came. It also swallowed whatever a producer had written for a command that was
  never going to run.
- `rx daemon start|stop|status` emit objects instead of human text, and are
  idempotent: a daemon already in the requested state reports `changed:false`
  and exits 0 rather than failing.
- JSON follows the destination — pretty on a terminal, compact otherwise, with
  `--compact`/`--pretty` and `RX_JSON` to force it. Nobody should have to know a
  flag exists to stop paying for indentation in a pipe.
- `--auto-deploy` is gone: the deploy policy is `RX_AUTO_DEPLOY` only. It briefly
  became a validated enum during 0.4.0 development, which is what exposed the
  real problem — eight lines of it in all 18 subcommand helps, inert on four of
  them. Whether hosts may change under you is a property of the environment, not
  a per-call choice, so it follows `RX_CONNECT_TIMEOUT` and stays out of `--help`.
- Env vars are guessable from the binary name: `RX_JSON`, `RX_DAEMON`,
  `RX_AUTO_DEPLOY`, `RX_CONNECT_TIMEOUT`. The older `REM_EXEC_AUTO_DEPLOY`,
  `REM_EXEC_JSON` and `REM_EXEC_DAEMON` are still honored as fallbacks;
  `RX_CONNECT_TIMEOUT` arrived with the rename and never had a `REM_EXEC_` form.
- The guide is stamped with the version that shipped it, opens with the
  first-contact sequence (`ping` → `deploy` → work), states the stdin/TTY
  hazard, and names rxv — its secret-delivery example used to invoke a tool that
  does not exist.

Breaking, and deliberate before 1.0: argument errors moved from stderr text to
stdout JSON; an unusable argument value now exits 2; piped JSON is compact; `rx
daemon` prints objects. `PROTOCOL_VERSION` is unchanged — the rx↔rxd wire did
not move — but the version bump means `ping` reports `up_to_date:false` for a
0.3.x rxd, so run `rx deploy` across the fleet after upgrading.

Everything below is proposed, not committed. Ordered by agent-experience value.

## 0.4.x CLI polish (before any 0.5.0 bump)

The 0.4.0 line is where CLI structure gets polished; no 0.4.0 artefacts are
public, so breaking changes are still free here. Findings came from walking the
documented first-contact path (`ping` → `deploy` → `run`) against real hosts —
rootless podman containers, see the note at the end of this section.

All of it landed across three commits; what follows is the record of what
changed and why, not a plan.

**Slice 1 — response shapes:**
- Deploy failures are typed. A single-host failure is a plain `error` object
  with `deploy_failed` (or `ssh_unreachable`/`ssh_auth` when the transport is
  what broke); a batch keeps the aggregate with typed per-host entries. It used
  to report `{"type":"deployed","status":"failed","error":"<raw curl>"}` — a
  failure wearing a success type, with no code to branch on, on the one path
  every new host must cross.
- `type` leads every response (`serde_json` `preserve_order`). `json!()` sorted
  keys while struct-derived responses did not, so the identical `not_deployed`
  error read two different ways on `ping` and `run` — commands one and two of a
  first-time agent's experience.
- `not_deployed`'s hint names `RX_AUTO_DEPLOY`, not the retired `REM_EXEC_*`
  spelling, and is one line instead of a 200-char run-on. It is the most-seen
  hint in the tool.
- `retryable` is always serialized. It was omitted when false, so most errors
  lacked a field `docs/CONTRACT.md` advertises unconditionally.

**Slice 2 — breaking, all three in one commit:**
- `rx run HOST -- /nonexistent` exited **0** with `exec_error:"command_not_found"`,
  so `rx run … && next-step` proceeded after a command that never ran. Now exits
  **127**. A real exit 127 and a failed exec share the status but stay
  distinguishable in the JSON, which remains the source of truth.
- `deploy` is idempotent: a host already running this exact rxd answers
  `status:"current"` with `changed:false` and uploads nothing. The check runs
  before the binary is resolved, so an already-current host needs neither cache
  nor network. An explicit `--binary` always uploads — a local build can carry
  the same version string as the release and still be a different binary.
- `rx setup` → **`rx cache fetch`**, a namespace from the start (see below).

### Local-machinery namespaces

Three things are local-machine concerns rather than remote operations: the rxd
binary cache, the read-cache daemon, and (planned) installing rx's guide into an
agent harness. `daemon` is already a noun namespace with verb subcommands; the
other two should match, so the top-level list stays short and the shape is
predictable:

    rx cache  fetch [--version V] [--arch A]... [--force]
              prune                                   # planned, see below
    rx daemon start | stop | status
    rx skill  [install [--harness auto|claude-code|…]] # planned, see below

`setup` → `cache fetch` is a namespace from the start rather than a leaf that
gets converted later — cache pruning is already on this roadmap, so renaming to
a leaf `rx cache` now would buy a second breaking change when prune lands.
Nothing is published, so this is the free window for exactly one break.

**Harness plugin install (planned).** A command that detects an agent harness
(Claude Code and similar) and installs rx's guide into it, so an agent picks up
rx as a skill without being told about it by hand. This belongs under the
existing `skill` noun rather than a new `rx plugin` / `rx install` top-level:

- The artifact IS the skill — `rx skill` already means exactly this content.
  A second noun for the same artifact fragments it, and "plugin" overclaims
  where harnesses distinguish the two (in Claude Code a plugin is a bundle that
  may *contain* skills, commands, agents, MCP servers; what rx ships is one
  skill).
- `rx install` is out: it sits next to `rx deploy`, which installs rxd on remote
  hosts, and the two would read as variants of one another.
- Bare `rx skill` keeps printing and stays effect-free. Installation is an
  explicit verb the caller has to type — the same discovery-must-not-have-
  effects rule that removed the implicit unlock from bare `rxv`. Auto-detection
  chooses *where* to install, never *whether*, which is the
  no-implicit-mutation stance applied to the local machine.
- Open questions for the design: whether install is idempotent-by-default and
  reports `changed` like the rest of the contract (it should); whether it writes
  a version marker so a stale installed guide can be detected after an rx
  upgrade (`ping` already treats version skew as a first-class concern, and a
  guide that describes an older binary is the same class of problem); and
  whether `--harness auto` refusing on an unrecognized harness is a typed error
  with a hint naming the explicit flag (yes).

**Slice 3 — the contract's scope, and the text:**
- The contract governs *operations, not discovery*. It used to claim "no
  exceptions" and then carve out two (exit-2 had empty stdout; `skill` printed
  bytes), with a third unwritten: `--version` printed plain text to an agent the
  guide told to call it. Discovery — `--help`, `-h`, `help`, `--version`,
  `skill`, bare invocation — now prints for a reader and emits no object, which
  makes the agent-facing invariant genuinely absolute. Shared verbatim with
  rem-exec-vault.
- The parser's own rejections are typed. 0.4.0 typed the 19 argument errors rx
  checks itself but left clap's as prose. Split by clap error kind:
  `DisplayHelpOnMissingArgumentOrSubcommand` (bare `rx`) stays plain help with
  no JSON, so a human's first keystroke is not answered with a JSON blob;
  everything else emits a typed object that is the WHOLE of stderr, with stdout
  byte-empty. That last part is load-bearing: at parse time the subcommand is
  unknown, so every parse failure has to hold the line `rxv get | rx put -`
  depends on, not just the ones that turn out to be `get`.
- `--help` order now matches its own "Start here": `skill · ping deploy · run
  start wait · status stdout stderr list clean · write close-stdin kill ·
  put get · cache daemon`. `ping`/`deploy` were 13th and 12th, below
  `close-stdin`.
- after_help no longer says "Secrets live in rxv, the companion vault" — it
  overstated a coupling `CONTRACT.md` is at pains to deny, on the
  highest-traffic surface. It now shows `<producer> | rx put -` with rxv, pass,
  op and sops as interchangeable, and states rx needs none of them.
- `rem-execd` is gone; everything says `rxd`.
- `skill`'s description names its audience and size instead of saying "Print
  skill file", and both tools' after_help ends with the repository URL —
  someone who unpacked a release tarball has no README beside it.

**Slice 4 — what an agent's error handling actually does with the answer.**
Found by walking the same podman path with an external CLI review in hand. The
review's presentation findings (top-level density, `--auto-deploy` spam on every
subcommand help — since fixed by removing the flag — and clap prose inside
`message`, still open) are real; these are
the ones underneath them, where the response was not merely noisy but wrong.

- **`put` reported caller errors as `internal`, which is retryable.** A missing
  target directory and an unwritable one both answered
  `{"code":"internal","retryable":true}` with no hint — so the two put failures
  that can *never* succeed were the two rx told a caller to try again, on one of
  the two daily verbs. `get` had answered `not_found` for the same OS condition
  since it shipped. The mapping lives in `protocol::io_error_code` now, and both
  halves of the pair share it: ENOENT → `not_found`, EACCES → `bad_request`, and
  `internal` means only what the contract says it means. ENOENT's hint names
  `mkdir -p`; EACCES's names the privileged-rxd option.
- **Errors could still arrive with no `code` at all.** `Response::error()` — the
  untyped constructor — survived 0.4.0's typing pass at two call sites in the
  transport classifier's fall-through. A *local* `get` failure (unwritable
  destination on this machine) reached it, so the one field both skills tell
  callers to branch on was absent, after paying a `remote_deploy_status` round
  trip to diagnose a directory on the controller. Local failures are now typed
  where they happen, the fall-through says `internal`, and the constructor is
  gone so the shape cannot come back. `docs/CONTRACT.md` states the invariant.
- **ssh could prompt, and could not be bounded.** `ssh_command` set only the
  ControlMaster options: no `BatchMode`, so OpenSSH reached for an askpass helper
  against a host needing a password — on a desktop with one installed (DISPLAY is
  usually set) that is a GUI dialog no agent harness can answer, and without one
  it burned three auth attempts; and no `ConnectTimeout`, so a black-holed host
  cost **over 90 seconds** measured. Since rx deliberately has no fleet loop, the
  caller iterates inventory and pays that per dead host. Both are set now
  (`ConnectTimeout=10`, `RX_CONNECT_TIMEOUT` to override — env-only, so the
  command surface does not grow), and `ssh_auth` carries a hint naming ssh-agent.
  `deploy`'s `scp` had *no* options at all — not even ControlPath, so it opened a
  second connection moments after ssh established a multiplexed one; it shares
  the builder now.
- **Malformed process IDs no longer need the host.** `invalid_process_id` lived
  only in rxd, so `rx status <unreachable-host> NOTANID` answered
  `ssh_unreachable` + `retryable:true` after the full connect timeout: a typo
  wearing the shape of a transient network fault, which a retry loop treats as
  "try again forever". rx checks the 8-hex form before it spawns anything, beside
  the existing `bad_host` check. rxd keeps its check — an older rx against a
  current rxd must not lose it — and both emit the same code, so the answer does
  not depend on which side caught it.

Breaking: `put` and `get` error *codes* changed for missing/unwritable paths
(`internal` → `not_found`/`bad_request`) and those errors are no longer
`retryable`. A caller branching on `code` sees a more accurate answer; one
branching on `retryable` stops looping. BatchMode is a behaviour change for
anyone relying on an interactive password prompt — use ssh-agent. The
`--auto-deploy` flag is REMOVED; `RX_AUTO_DEPLOY=off|local|on` is the only knob,
so an invocation carrying the flag now exits 2. `rxd skill` is removed too — the
guide describes rx, and shipping it inside the remote binary put 22 KB and a
second copy on every host.

**Scope guard for the rest of 0.4.x: no new top-level command.** The remaining
work is fixing and pruning, and the surface is already the thing an agent has to
get past — 18 commands where the daily path is three. Anything shaped like
`rx which` or `skill install` waits for 0.5.0 and gets argued on its own merits
there. New *error codes* are held to the same test: `put`'s misclassification
was fixed by reusing the mapping `get` already had, not by inventing a code.

**Still open, roughly in the order they are worth doing:**

1. **Presentation — the external review's findings, all verified.**
   ~~The `--auto-deploy` enum dumped into every subcommand help~~ — **done**:
   the flag is gone and `RX_AUTO_DEPLOY` is the only knob. Top-level-only was
   the other candidate and was rejected: it makes flag *position* load-bearing
   (`rx run --auto-deploy=on` would start failing) for a caller that composes
   command lines. Env-only follows the rule `RX_CONNECT_TIMEOUT` already sets —
   a harness decision, not a per-call one — and it removes a flag that was
   accepted-but-inert on `skill`, `cache`, `daemon` and `deploy`. It cost 8 of
   the 21 lines of `rx skill --help`.
   Parser rejections carry clap prose inside `message`, with
   `Usage:` duplicated into a field meant for one short sentence — the `code` is
   right, the text is not. The skill needs a choose-your-path table (`run` vs
   `start`+`wait` vs `start --pipe` vs `put`/`get`) near the top, and should say
   outright that most work is `run` + `put`/`get` and the nine process verbs are
   the advanced section. `daemon` should be demoted in both help and skill: it is
   opt-in behind `RX_DAEMON`, does not handle ping/put/get, and sitting beside
   `run` overstates it.
2. **`rx daemon status` reports `changed:false` on a pure query**, where the
   field means nothing.
3. **The error envelope's key order differs between the tools** — rx emits
   `type,message,code,…`, rxv `type,code,message,…`, and `docs/CONTRACT.md`
   shows rxv's. `preserve_order` was adopted precisely so the same error reads
   the same way in both; it does not yet.

**Deliberately not doing:** renaming `get`/`status`/`list` for symmetry between
the tools (natural in each domain, and the collision is documented at the top of
`rxv skill`), and namespacing the process verbs under `rx proc`. The density is
real, but `cache`/`daemon`/`skill install` are *local-machine* namespaces —
`proc` would split the top level on a second axis while lengthening the
most-typed debugging commands. Treat it as a help-text problem first; revisit
only if item 1 does not fix the feel.

**Release hygiene:** `dist/` is untracked staging and the publish step globs it,
so a leftover set ships under the new tag — which nearly happened: the whole
directory sat at **0.3.0** against 0.4.0 source. Cleared, and `RELEASING.md` now
starts the build with `rm -rf dist` plus an embedded-version check after
checksumming, so staleness is structurally impossible rather than merely noticed.

**Test hosts:** rootless podman, two distros so `ping`'s distro detection is
exercised for real. rx shells out to bare `ssh`/`scp` with no `-F` and no option
escape hatch, so point it at non-default ports with a PATH shim (`bin/ssh`,
`bin/scp` execing the real binary with `-F <test config>`) rather than editing
`~/.ssh/config`. Leaves no residue.

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

`rx cache prune` (the second verb the `cache` namespace was created for): drop
entries older than the running rx, plus the unversioned leftovers. Keeping the
current version and one predecessor is the useful shape — the predecessor is
what you deploy when rolling a host back — so this is "keep N", not "delete
everything else". `--prune-all` for reclaiming the lot. Worth doing before the
cache has enough versions in it to matter.

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

## Deferred from the 0.3.1 security review

Deliberately kept out of the 0.3.1 patch so it stayed reviewable.

- **Split the monolithic sources.** `rx.rs` (~1.6k lines) and
  `remote/actions.rs` (~1.1k) have natural seams: put/get/deploy CLI, process
  lifecycle, transfer plumbing. Wanted, but as its own commit — never mixed
  into a security change.
- **`CloseStdin` deserves its own response type.** It currently answers
  `Written { bytes: 0 }`, which works but reads as ambiguous. A new variant is a
  wire change an older rx cannot parse, so it belongs to a protocol v3.
- **End-to-end `rx` ↔ SSH ↔ `rxd` smoke test.** The local `rxd serve` harness is
  strong and the injection boundary is unit-tested, but nothing exercises the
  real ssh path in CI. An opt-in `ssh localhost` test would close it.
- **Deterministic `file_changed` coverage.** The bound-and-re-stat logic is
  verified by hand (a growing file is detected; stable and empty files exit
  clean) and guarded against false positives in the suite, but racing a file
  change inside a fast test is still unsolved.
