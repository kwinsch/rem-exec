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
something* emits exactly one JSON object with a `type` field, success or
failure. One boundary, drawn where the object would carry nothing a caller does
not already have: a successful `rxv get` is the plaintext alone. Every *failure*
is an object, everywhere, without exception.

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
| `rxv get` | the decrypted secret | stderr, **on failure only** — a successful read is the plaintext alone |
| `rx start --pipe` | the process stream | stderr, on success *and* failure — it carries the process `id` |
| everything else | the object | stdout |

The two byte-stream rows differ on success for a reason: `start --pipe`'s object
is the handle you need to `status`, `write` to or `kill` the process afterwards,
while a successful `get` has nothing to add to the bytes it just wrote.

That exception is not cosmetic. A command whose stdout is a byte stream must
write **zero bytes** there when it fails, or `rxv get … | rx put -` pipes an
error message into a remote file and `rx start --pipe … | consumer` feeds one to
the consumer as data. Everything downstream of those pipes depends on it — so in
both tools the routing is one switch, set before the first thing that can print,
rather than a decision repeated at each call site. `rx` learned that the hard
way: in 0.4.0 the success path was right and all three failure paths were not.

**stderr carries human notes only** — progress lines, warnings, the private key
from `rxv rekey --generate`. Never a result a caller has to parse. The one
exception is the argument-parser rejection below, which has nowhere else to go.

**Exit codes.**

| code | meaning |
|---|---|
| 0 | success |
| 1 | the call was usable and the operation failed |
| 2 | the call itself is unusable — nothing was attempted |

Exit 2 covers a mistyped flag and a rejected argument *value* alike: `--mode
9999`, a destination that is not `HOST:PATH`, a process ID that is not 8 hex
digits, a secret path with a `..` in it. They are one class of mistake — the
command line cannot be used — so which layer noticed is not something a caller
should be able to see. That matters because the layer is not stable: a value
parsed by hand today can become a parser-level check tomorrow without anything
observable changing. Both tools derive the status from the error `code` for
exactly that reason.

The line is *usable*, not *permanent*. `secret_not_found`, `not_found` and
`empty_stream` are all permanent too, but the invocation that produced them was
well-formed and it is the world, not the command line, that has to change —
those are exit 1. Nothing is ever both exit 2 and `retryable`.

**Which stream the object goes to is a separate question** — it follows the
table above, not the exit code. stdout carries the product, so an object goes
there unless the product is bytes. The one forced case is a rejection by the
argument parser itself: the subcommand is not known yet, so the object is the
whole of **stderr** and stdout stays byte-empty. If the command had turned out
to be `rxv get`, anything on stdout would have landed in whatever the pipe
feeds.

`rx run` and `rx wait` additionally propagate the remote command's exit status
(`rx run HOST -- false` exits 1; killed by signal N exits 128+N; a command that
never started exits 127). The JSON is the source of truth; the process exit is a
convenience.

**Errors are typed.**

```json
{"type":"error","code":"secret_not_found","message":"…","retryable":false,
 "hint":"run `rxv list` to see what is stored"}
```

- `code` is the stable part, and it is **always present**. **Branch on it, never
  on message text** — messages get rewritten, codes do not. A `switch` on `code`
  never has to handle a missing one; when nothing else fits, the answer is
  `internal`, not an absent field.
- `retryable` is always present. `true` means the identical call could plausibly
  succeed on a retry (a transient network failure, a host that just got its rxd
  deployed). It never means "the caller should change something" — that is what
  `hint` is for.

  A corollary both tools now hold to: a condition the caller must fix is never
  `retryable`. A missing directory, a path the user cannot write, a malformed
  process ID — these do not resolve themselves, and reporting them as retryable
  turns a caller's error handling into an infinite loop. When the fix is a
  different call, that belongs in `hint`.
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
