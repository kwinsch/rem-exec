# Releasing rem-exec

`rx cache fetch` / `rx deploy` / auto-deploy fetch static `rxd` binaries from the
GitHub Release whose tag matches `CARGO_PKG_VERSION` (e.g. `v0.2.0`), and verify
them against `SHA256SUMS`. So the release must exist, be tagged correctly, and
carry assets whose hashes match — build and checksum *before* publishing.

## Prerequisites

- musl cross toolchain on PATH (see `MUSL_PATH` in `.cargo/config.toml`):
  `export PATH="$MUSL_PATH:$PATH"`
- rustup targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `riscv64gc-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`
- `.cargo/config.toml` sets the per-target musl linkers.

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

Always build with `crt-static` forced. Without it, **riscv64gc-musl links
dynamically** (interpreter `/lib/ld-musl-riscv64.so.1`), which breaks the
static-portability guarantee and auto-deploy onto hosts without musl.

**Clear `dist/` first.** It is untracked staging, so whatever a previous release
left behind survives — and step 4 below globs `dist/rx-*`, which would publish
those stale binaries under the new tag. Starting empty makes that impossible
rather than merely unlikely. (It has already happened: the whole directory sat
at 0.3.0 while the source was 0.4.0.)

```bash
rm -rf dist && mkdir dist
export PATH="$MUSL_PATH:$PATH"
export RUSTFLAGS="-C target-feature=+crt-static"
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
`static-pie linked`, never `dynamically linked` / `interpreter`.

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

## Publish (order matters)

crates.io comes **last**: a publish cannot be undone (only yanked), while a
GitHub release can be deleted and recreated. Publishing first would burn the
version number if the assets turn out wrong — and the published `rx` fetches
those assets, so they must already exist.

1. Bump `version` in `Cargo.toml`; commit. (Bump `PROTOCOL_VERSION` only on a
   breaking wire change — a mismatch triggers redeploy via the version check.)
2. Push commits: `git push origin master`.
3. Tag the release commit and push it: `git tag -a v0.3.0 -m v0.3.0 && git push origin v0.3.0`.
4. Create the release with all 9 assets (4 arches × 2 binaries + SHA256SUMS):
   `gh release create v0.3.0 dist/rx-* dist/rxd-* dist/SHA256SUMS --title ... --notes ...`
5. Verify the hashed release resolves and auto-deploy works:
   `rx cache fetch --version v0.3.0 --force` (downloads + checksum-verifies), then
   `rx deploy <host>` against a test host (version/protocol check must pass).
6. Publish to crates.io: `cargo publish` (dry-run first: `cargo publish --dry-run`).
