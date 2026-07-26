# The rx / rxv agent contract

`rx` and `rxv` are **separate, independently usable tools**, and neither
requires the other: `rx` runs commands and moves files on remote hosts with no
opinion about where a secret came from, and `rxv` stores secrets with no opinion
about what reads them. They are separate projects so that choice stays with the
operator — `rx put -` takes any producer on stdin (`pass`, `op read`,
`vault kv get`, `sops -d`, a script), and `rxv get` feeds anything that reads
stdin.

What they share is this contract, so that an agent which has learned one already
knows how to read the other. Adopting one of them does not commit you to the
other.

> This file is duplicated verbatim in the `rem-exec` and `rem-exec-vault`
> repositories. Change it in both, in the same release.

## The rules

**One object per operation.** Every invocation of `rx` or `rxv` that *does
something* emits exactly one JSON object with a `type` field — success or
failure, no exceptions.

**Discovery prints for a reader.** `--help`, `-h`, `help`, `--version`, `skill`
and a bare invocation with no subcommand are not operations: they answer in
plain text, following ordinary CLI convention, and emit no object. They also
have no side effects — finding out what a tool is must not change anything,
which is why a bare `rxv` does not unlock the vault.

That line is what keeps the rule above absolute rather than nearly-absolute.
An earlier version of this contract said "no exceptions" and then carved out
two, while `--version` quietly printed `rx 0.4.0` to an agent the guide had
just told to call it. One honest boundary beats three silent ones.

**stdout carries the product.** For almost every operation the product *is* the
object, so that is where it goes. The exceptions are the ones whose product is
raw bytes:

| command | stdout | the object goes to |
|---|---|---|
| `rxv get` | the decrypted secret | stderr |
| `rx start --pipe` | the process stream | stderr |
| everything else | the object | stdout |

That exception is not cosmetic. `rxv get` must write **zero bytes** to stdout
when it fails, or `rxv get … | rx put -` would pipe an error message into a
remote file. Everything downstream of that pipe depends on it.

**stderr carries human notes only** — progress lines, warnings, the private key
from `rxv rekey --generate`. Never a result a caller has to parse. The one
exception is the argument-parser rejection below, which has nowhere else to go.

**Exit codes.**

| code | meaning |
|---|---|
| 0 | success |
| 1 | the call was understood and failed |
| 2 | the call was malformed — typed object on **stderr**, stdout stays empty |

A malformed call answers with the same `{"type":"error","code":…}` shape as
everything else, so a mistyped flag and a rejected argument value are one class
of failure rather than two. It goes to stderr because stdout must stay
byte-empty on exit 2: at the moment the parser rejects a call, the subcommand is
not yet known, and if it were `rxv get` then anything on stdout would land in
whatever the pipe feeds.

`rx run` and `rx wait` additionally propagate the remote command's exit status
(`rx run HOST -- false` exits 1; killed by signal N exits 128+N; a command that
never started exits 127). The JSON is the source of truth; the process exit is a
convenience.

**Errors are typed.**

```json
{"type":"error","code":"secret_not_found","message":"…","retryable":false,
 "hint":"run `rxv list` to see what is stored"}
```

- `code` is the stable part. **Branch on it, never on message text** — messages
  get rewritten, codes do not.
- `retryable` is always present. `true` means the identical call could plausibly
  succeed on a retry (a transient network failure, a host that just got its rxd
  deployed). It never means "the caller should change something" — that is what
  `hint` is for.
- `hint`, when present, names a concrete different command.

**Idempotence.** Asking for a state that already holds is success, not failure:
`rxv unlock` on an unlocked vault, `rxv lock` on a locked one, `rx daemon start`
on a running daemon and `rx deploy` on a host already running this exact rxd all
exit 0 and report `"changed": false`. "Ensure X" is what a caller actually
wants, and making it cost an error plus a string match is what turns a usable
tool into one an agent has to be taught around.

**JSON shape follows the destination.** Pretty when stdout is a terminal,
compact otherwise — so a person reading gets indentation and a pipe does not pay
for it. Force either with `--compact` / `--pretty`, or `RX_JSON` / `RXV_JSON`
set to `compact` or `pretty`.

## Shared codes

`bad_request` (the call was understood, its arguments cannot be used) and
`internal` (no better answer available) mean the same thing in both tools. Every
other code is specific to one of them and listed in its own guide — `rx skill`,
`rxv skill`.
