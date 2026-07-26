# Releasing rem-exec

`rx cache fetch` / `rx deploy` / auto-deploy fetch static `rxd` binaries from the
GitHub Release whose tag matches `CARGO_PKG_VERSION` (e.g. `v0.2.0`), and verify
them against `SHA256SUMS`. So the release must exist, be tagged correctly, and
carry assets whose hashes match — build and checksum *before* publishing.

## Prerequisites

- musl cross toolchain on PATH (see `MUSL_PATH` in `.cargo/config.toml`):
  `export PATH="$MUSL_PATH:$PATH"`
- rustup targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `riscv64gc-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`
- `.cargo/config.toml` sets the per-target musl linkers and `crt-static`.
- `gh` authenticated; `cargo` authenticated for crates.io.

## Versioning

Pre-1.0, so the **minor** is the breaking-change digit: anything that changes
the JSON shapes, the CLI surface or an error code's meaning is a minor bump.
0.3.0 → 0.4.0 was exactly this — errors moved from stderr prose to stdout JSON.
Agents consume `rx skill` fresh each session, so a clean break beats a
compatibility shim.

`PROTOCOL_VERSION` (currently 2) is separate and tracks only the rx↔rxd wire.
Bump it on a breaking wire change; leave it alone otherwise. The two are
compared at different points, and the difference decides what a release asks of
a fleet:

- **Protocol** equality is what lets an ordinary command run at all. A mismatch
  is the `not_deployed` error; an unchanged protocol means every deployed rxd
  keeps working against the new rx.
- **Full version** equality is what `ping`'s `up_to_date` and `rx deploy`'s
  idempotence compare. So after a release that left the protocol alone, `ping`
  still reports `up_to_date:false` and `rx deploy` still uploads — which is how
  rxd-side fixes reach hosts that were never broken.

So the release notes should say which of the two moved, and therefore whether a
fleet redeploy is required or merely recommended.

## Gate (before anything irreversible)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Formatting is stock rustfmt — no `rustfmt.toml`, so `cargo fmt` is whatever the
installed toolchain does and a contributor's editor agrees with it out of the
box. Adopted in 0.4.0; before that the tree was hand-formatted, so `git log`
crosses one mechanical reformat (see `.git-blame-ignore-revs`).

`tests/rxd_integration.rs` drives a real `rxd` over the local filesystem, so it
runs here without a remote host.

## Build (all arches, fully static)

`.cargo/config.toml` sets `crt-static` per target, so this is structural rather
than remembered. Without it **riscv64gc-musl links dynamically** (interpreter
`/lib/ld-musl-riscv64.so.1`), which breaks the static-portability guarantee and
auto-deploy onto hosts without musl — and it is the one musl target that does
not enable it by default, so a plain `cargo build --release --target` used to
produce a dynamic rxd that looked fine until it reached a host without musl.

Do **not** set `RUSTFLAGS` for the release build. Cargo lets it *replace* the
config's per-target `rustflags` rather than merging, so a `RUSTFLAGS` set for
any other reason silently drops `crt-static`.

**Clear `dist/` first.** It is untracked staging, so whatever a previous release
left behind survives — and step 4 below globs `dist/rx-*`, which would publish
those stale binaries under the new tag. Starting empty makes that impossible
rather than merely unlikely. (It has already happened: the whole directory sat
at 0.3.0 while the source was 0.4.0.)

```bash
rm -rf dist && mkdir dist
export PATH="$MUSL_PATH:$PATH"
for t in x86_64-unknown-linux-musl aarch64-unknown-linux-musl riscv64gc-unknown-linux-musl armv7-unknown-linux-musleabihf; do
  cargo build --release --target "$t"
done
```

Stage + strip with the arch-specific `strip`, using the release asset names
(`rx-<arch>`, `rxd-<arch>` where `<arch>` ∈ x86_64/aarch64/riscv64/armv7; note
the armv7 asset comes from the `armv7-unknown-linux-musleabihf` target and its
`arm-linux-musleabihf-strip`):

```bash
# e.g. aarch64-linux-musl-strip on target/aarch64-unknown-linux-musl/release/{rx,rxd}
```

Verify every binary is self-contained — `file` must show `statically linked` or
`static-pie linked`, never `dynamically linked` / `interpreter`. Check the
*whole* line; a truncated `file` output is how a dynamic riscv64 build slips
through:

```bash
file dist/rx-* dist/rxd-* | grep -i 'dynamic\|interpreter' && echo PROBLEM || echo "all static"
```

## Checksums

`SHA256SUMS` must list **bare filenames** (rxd matches the asset name exactly):

```bash
cd dist && sha256sum rx-* rxd-* > SHA256SUMS && sha256sum -c SHA256SUMS
```

Then confirm every staged binary really carries this version. Cross-arch builds
cannot be run locally, so grep the embedded string — enough to catch a leftover
from an earlier release, which is the failure this guards:

```bash
VERSION=$(grep -m1 '^version' ../Cargo.toml | cut -d'"' -f2)
for f in rx-* rxd-*; do grep -qa "$VERSION" "$f" || echo "STALE: $f"; done
```

## Before pushing anything

Run the compliance check from `SENSITIVE.md` (untracked, local only) over both
the staged diff and the full history. It must come back empty.

Note that the check's own pattern list is sensitive: keep it in `SENSITIVE.md`
and never paste it into a tracked file — including this one.

Also check what the crate tarball would carry:

```bash
cargo package --list        # neither SENSITIVE.md nor .cargo/ may appear
```

Both are excluded via `Cargo.toml`. `.cargo/config.toml` is dev-only
cross-compilation config — useless to a `cargo install` consumer, and it names
a local toolchain layout, so it stays out of the published crate.

## Publish (order matters)

crates.io comes **last**: a publish cannot be undone (only yanked), while a
GitHub release can be deleted and recreated. Publishing first would burn the
version number if the assets turn out wrong — and the published `rx` fetches
those assets, so they must already exist.

1. Bump `version` in `Cargo.toml`; commit. (Bump `PROTOCOL_VERSION` only on a
   breaking wire change — a mismatch triggers redeploy via the version check.)
2. Push commits: `git push origin master`.
3. Tag the release commit and push it (derive the tag from Cargo.toml — do not
   copy a stale version from an earlier release):
   ```bash
   VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
   git tag -a "v$VERSION" -m "v$VERSION" && git push origin "v$VERSION"
   ```
4. Create the release with all 9 assets (4 arches × 2 binaries + SHA256SUMS):
   `gh release create "v$VERSION" dist/rx-* dist/rxd-* dist/SHA256SUMS --title ... --notes ...`
5. Verify the hashed release resolves and auto-deploy works:
   `rx cache fetch --version "v$VERSION" --force` (downloads + checksum-verifies),
   then `rx deploy <host>` against a test host (version/protocol check must pass).
6. Publish to crates.io: `cargo publish` (dry-run first: `cargo publish --dry-run`).
