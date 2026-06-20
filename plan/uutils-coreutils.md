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

## Verified-red

- (to fill) gate behavioral: break a multicall assertion / drop a vendored crate → build
  or behavior reds.
- census: `uutils` must be in self-host-specs (else the census expects a gnu oracle and
  errors).
