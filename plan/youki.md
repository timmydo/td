# youki — td builds the Rust OCI container runtime from source

Handle: claude-opus-3267ea — started 2026-06-21. The postponed crun→youki replacement
(see memory `td-youki-postponed`): worth it now that td builds Rust from source (the
rust-build path). Same pattern as uutils-coreutils (#123). Rust-focused minimal distro
steer (memory `td-rust-focus-distro-direction`): the userland is td-built Rust from source.

## Target

The published `youki` crate 0.6.0 (crates.io): builds the `youki` OCI runtime binary
(`[[bin]] name = "youki"`). Cargo.lock has 664 packages — 663 fetchable registry crates
(only the root has no checksum).

**No default features** (`[features]` has no `default = [...]`), so plain `cargo build
--release` (what run_rust does) builds youki with NO seccomp/systemd/v1/v2/wasm — and
build.rs only probes libseccomp/pkg-config when `CARGO_FEATURE_SECCOMP` is set (skipped),
and falls back gracefully when git is absent. So youki builds from the vendored crates +
the standard rust/cargo/gcc seed — NO libseccomp/pkg-config/git needed (verified by reading
Cargo.toml + build.rs; confirmed by the build).

## This PR (the capability)

- `tests/ts/recipe-youki.ts` — name "youki", buildSystem "rust", bins ["youki"]. Source via
  the lock (`youki-source` = the youki-0.6.0.crate tarball); added to self-host-specs in
  guix-dependence.scm (a from-source Rust tool, no gnu oracle — like cat/uutils).
- `tests/youki.lock` — the youki crate tarball + 663 static.crates.io `.crate` fixed-output
  fetches (sha256 = Cargo.lock checksum) + the rust/cargo/gcc/coreutils/bash/tar/gzip seed.
- gate `rust-youki` — build via build-recipe (offline, vendored, guix/Guile off PATH);
  ALL-DURABLE: structural (.drv carries TD_VENDOR_CRATES + stage0 builder), behavioral
  (the td-built `youki` runs — `youki --version`/`--help` shows the OCI runtime), repro
  (td-builder check double-build over the 663-crate graph).
- affected-checks mapping: recipe-youki.ts + youki.lock → rust-youki.

## Shipping (later, coordinated)

Shipping youki (replacing crun) in the booted system goes through td's own daemon/store
(maintainer direction 2026-06-21) — coordinated with the own-builder-daemon track, NOT a
guix-daemon bridge (the closed #128 was the wrong direction).

## Result (green)

Gate `rust-youki` PASSES: td built `youki-0.6.0` at `/gnu/store/gcbp74…` from the 663-crate
vendored closure — structural (TD_VENDOR_CRATES + stage0 builder + td's own ts-eval),
behavioral (`youki version: 0.6.0`; `--help` lists the OCI lifecycle: create/start/state/
kill/delete/run/list), repro (td-builder double-build over 663 crates). 9.0M binary, links
only libc/libgcc_s (no system C deps — the no-features build). `commit:
…VERGEN_IDEMPOTENT_OUTPUT` confirms vergen's no-git fallback produced a DETERMINISTIC binary
(why repro holds).

## Verified-red (confirmed)

- **Behavioral is load-bearing**: the built binary is a genuine OCI runtime CLI —
  `youki --version` → `youki version: 0.6.0`, `youki --help` → create/start/state/kill/delete/
  run/list. A non-youki or broken binary fails the `--version`/`create` assertions.
- **census**: `youki` must be in self-host-specs — else the census globs recipe-youki.ts,
  tries `specification->package "youki"` (none), and errors. Excluded → census unchanged.
