# uutils-coreutils — td builds the full Rust coreutils multicall from source

Handle: claude-opus-3267ea — started 2026-06-20. First step of the Rust-focused minimal
distro steer (see memory `td-rust-focus-distro-direction`): make the shipped userland
td-built Rust from source, not guix-packaged. Extends the uu_cat demo (recipe-cat.ts /
rust-uutils gate, 139 crates) to the WHOLE coreutils.

## Target

The published `coreutils` crate 0.9.0 (crates.io): a single multicall binary
(`default-run = "coreutils"`, default feature `feat_common_core`) dispatching to all the
common utilities (ls/cp/mv/rm/cat/mkdir/ln/…). Its Cargo.lock has 508 packages — 507
fetchable registry crates (all uu_* members + shared deps; only the root `coreutils` has
no checksum). Plain `cargo build --release` (what run_rust does) builds it — no recipe
`features` field needed.

## Scope (this PR = the capability; td.scm shipping is a follow-up)

- **`tests/ts/recipe-uutils.ts`** — name "uutils", buildSystem "rust", bins ["coreutils"]
  (the binary the crate produces). Source omitted; supplied via the lock as `uutils-source`
  (the coreutils-0.9.0.crate tarball). Named "uutils" (NOT "coreutils") so the census does
  not collide with GNU coreutils' oracle — added to `self-host-specs` in
  guix-dependence.scm (a from-source Rust tool, no gnu oracle by design, like `cat`).
- **`tests/uutils-coreutils.lock`** — generated from coreutils-0.9.0's Cargo.lock: the
  `uutils-source` crate tarball + 507 static.crates.io `.crate` fixed-output fetches
  (sha256 = the Cargo.lock checksum) + the rust/cargo/gcc/coreutils/bash/tar/gzip seed
  (copied from cat-uutils.lock).
- **gate** `mk/gates/<NNN>-rust-coreutils.mk` — build via build-recipe (guix/Guile off
  PATH, offline, vendored); ALL-DURABLE (no guix oracle): structural (binary built, .drv
  carries TD_VENDOR_CRATES), behavioral (the multicall binary actually works: `coreutils
  ls`, `coreutils cp`, `coreutils cat` round-trips), repro (td-builder check double-build
  across the whole ~507-crate graph).

## Follow-up (separate PR)

Ship td's coreutils in `system/td.scm` (the userland becomes td-built Rust). Needs a
mechanism to reference a td-built store path as a system package — distinct from this
capability PR.

## Result (build green)

Gate `rust-coreutils` (343) PASSES: td built `uutils-0.9.0` (multicall `coreutils` binary)
at `/gnu/store/788c34…` from the 507-crate vendored closure — structural (.drv carries
TD_VENDOR_CRATES + stage0 builder + td's own ts-eval), behavioral (mkdir/cp/cat/ls/mv/rm
round-trip through the ONE binary), repro (td-builder check double-build agrees over the
507-crate graph). The binary is a genuine multicall: **79 utilities**, 13M, links only
libc/libm/libgcc_s.

## Verified-red

- **Behavioral is load-bearing** (confirmed): the ONE binary has distinct per-util
  semantics — `coreutils true` → exit 0, `coreutils false` → exit 1, `coreutils echo` →
  output — so dispatch is real, not a stub that always succeeds. The gate's mv/rm legs
  assert the NEGATIVE (the source file is GONE after mv, the file is GONE after rm), so a
  no-op multicall reds them.
- **census**: `uutils` must be in self-host-specs — else the census globs recipe-uutils.ts,
  tries `specification->package "uutils"`, and (no such package) errors. With it excluded,
  the census is unchanged (owned-recipes still 26) — confirmed PASS.
