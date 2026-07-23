# Releasing rem-exec

`rx setup` / `rx deploy` / auto-deploy fetch static `rxd` binaries from the
GitHub Release whose tag matches `CARGO_PKG_VERSION` (e.g. `v0.2.0`), and verify
them against `SHA256SUMS`. So the release must exist, be tagged correctly, and
carry assets whose hashes match — build and checksum *before* publishing.

## Prerequisites

- musl cross toolchain on PATH (see `MUSL_PATH` in `.cargo/config.toml`):
  `export PATH="$MUSL_PATH:$PATH"`
- rustup targets: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `riscv64gc-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`
- `.cargo/config.toml` sets the per-target musl linkers.

## Build (all arches, fully static)

Always build with `crt-static` forced. Without it, **riscv64gc-musl links
dynamically** (interpreter `/lib/ld-musl-riscv64.so.1`), which breaks the
static-portability guarantee and auto-deploy onto hosts without musl.

```bash
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

## Publish (order matters)

1. Bump `version` in `Cargo.toml`; commit. (Bump `PROTOCOL_VERSION` only on a
   breaking wire change — a mismatch triggers redeploy via the version check.)
2. Push commits: `git push origin master`.
3. Tag the release commit and push it: `git tag -a v0.2.0 -m v0.2.0 && git push origin v0.2.0`.
4. Create the release with all 7 assets:
   `gh release create v0.2.0 dist/rx-* dist/rxd-* dist/SHA256SUMS --title ... --notes ...`
5. Verify the hashed release resolves and auto-deploy works:
   `rx setup --version v0.2.0 --force` (downloads + checksum-verifies), then
   `rx deploy <host>` against a test host (version/protocol check must pass).
