# rust-store-native — working notes

Handle: claude-opus-5cd532 · claimed 2026-06-26 · section: side (parallel-safe)

## Goal (human, 2026-06-26)

A usable, **guix-free** Rust userspace assembled **without** the guix `operating-system`
declaration (`system/td.scm`) — the td-native replacement that lets the `.scm` system
path eventually retire. The Rust toolchain at `/td/store` comes from the **upstream Rust
release tarball** (static.rust-lang.org), **not** the guix-built rust seed: the build
recipe is essentially *relinking a binary tarball* to `/td/store`. Upstream bytes carry
**no `/gnu/store` strings and no guix provenance**, so the guix part is eliminated at the
source rather than relabeled (this is NOT the demoted `store-relocate` of a guix binary —
that only relabels guix bytes; here the bytes were never guix's).

"Relocate now, source-rebuild later": rust is the accepted irreducibly-binary seed for
now; a true from-source rustc bootstrap (mrustc-scale) is a later, separate goal.

## Hard dependency (surfaced 2026-06-26)

Running an upstream rustc from `/td/store` needs **glibc ≥ 2.17** there (rustc's std uses
`__cxa_thread_atexit_impl`, added in glibc 2.17). The only `/td/store` glibc today is
**2.16.0** (the mesboot glibc). A new-enough glibc is the gcc agent's `glibc-final` rung
([[source-bootstrap]]), **not yet built**. So:

- The **fetch + relink recipe** and its **structural + supply-chain** legs are built NOW.
- The **`/td/store`-runtime behavioral** leg (rustc actually runs from `/td/store` and
  compiles a binary that runs in the own-root with `/gnu/store` absent) is **PENDING
  `glibc-final`** — marked explicitly, never faked. It flips green with a one-line
  relink-target swap once `glibc-final` lands.

## Progress

- **1a — DONE & verified (commit: pin + warm).** `tests/rust-upstream.lock` (rust 1.84.0,
  official sha256, content-addressed path) + `tools/warm-rust-upstream.sh`. Warm ran green:
  td-fetch pulled 343814532 bytes, sha matched, daemon-stored at the pinned path. ELF
  inspection of the fetched rustc/cargo/std: ZERO `/gnu/store` bytes, interp
  `/lib64/ld-linux-x86-64.so.2`, RUNPATH `$ORIGIN/../lib` (relative).
- **2a — DONE & verified (commit: td-owned ELF rewriter).** `builder/src/elf.rs` +
  `elf-interp`/`elf-set-interp` subcommands. NO patchelf (host patchelf is guix-provided —
  human direction 2026-06-26: build the feature in td first). Minimal ELF64-LE PT_INTERP
  reader/writer (size-bounded, errors on a too-long interp rather than truncating). 4 unit
  tests pass on cargo-test; validated on the REAL rustc: interp retargeted
  `/lib64/ld-linux-x86-64.so.2` -> `/td/store/ld`, confirmed by readelf, in-place, valid
  ELF, zero `/gnu/store`. RUNPATH needs no rewrite (already relative), so this one feature
  covers the relink.
- **Next — 2b/2c:** the relink RECIPE (`store-add-recursive` the rust tree into `/td/store`,
  `elf-set-interp` rustc/cargo, populate the tree's `lib/` with the `/td/store` glibc+libgcc_s
  so `$ORIGIN/../lib` resolves) + the structural gate (interp ∈ `/td/store`, no `/gnu/store`,
  closure-complete) + supply-chain leg + verified-red. Runtime leg PENDING `glibc-final`.

## Brick ladder

1. **rust-upstream-fetch** — pin the upstream Rust release tarball (version + sha256) and
   td-fetch it guix-free (host warm-prep, the warm-tsgo pattern: td-fetch → daemon
   add-to-store == FOD path; sandbox consumes offline). Sanity: the *un*relinked rustc
   from the tarball runs `rustc --version` on the host (proves a real working compiler,
   not guix). DURABLE supply-chain: fetched sha == the upstream pin; no `/gnu/store`
   bytes and no guix provenance in the tarball.
2. **rust-relink** (this track's core) — `td-builder` interns rustc+cargo+std into a td
   store (`store-add-recursive`) and **relinks** to `/td/store`: patch ELF interp +
   RUNPATH on every rustc/cargo/std binary to the `/td/store` glibc loader + lib dir
   (the binary-safe patch the relink path already knows). DURABLE structural: the
   interned tree has **no `/gnu/store`** bytes, interp/RUNPATH now point at `/td/store`,
   the closure is complete. Behavioral (`rustc --version` from the own-root): **PENDING
   glibc-final**.
3. **rust-compile-store-native** (pending glibc-final) — the relinked rustc, with its C
   linker retargeted to the `/td/store` gcc/glibc (the build-wrapper trick, reused from
   the userland C path), compiles a hello-world Rust program → a `/td/store`-linked
   binary that runs in the `store-ns` own-root, `/gnu/store` absent.
4. **rust-userspace** — build the Rust userland tools (ripgrep/fd/sd/procs/eza/bat/uutils)
   with the store-native rustc, assemble them with `td-builder profile`, run the profile
   in the own-root → a usable Rust userspace, no `.scm`, `/gnu/store` absent.

## Verified-red plan

- rust-upstream-fetch: corrupt the pinned sha → fetch verification fails (red).
- rust-relink structural: skip the interp/RUNPATH patch on one binary → the
  "no /gnu/store" / "interp→/td/store" assertion finds a residual `/gnu/store` (red).

## Parallel-safety

New gate file(s) under `mk/gates/`, a new build recipe + (if needed) a small `td-builder`
relink subcommand, and host warm-prep scripts. **No** edit to `system/td.scm`, `check.sh`
sandbox provisioning, the `Makefile`, or the gcc lane's `tests/bootstrap-*.sh`. Consumes
the `/td/store` glibc as a black box (so it composes with `glibc-final` without touching
it). builder/src changes validate on the `check-engine` smoke tier.
