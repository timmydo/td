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

- **1a — DONE & verified (commit: pin + warm).** `tests/rust-upstream.lock` (rust 1.96.0
  — the latest stable, human 2026-06-26; official sha256, content-addressed path) +
  `tools/warm-rust-upstream.sh`. Warm ran green: td-fetch pulled 377083597 bytes, sha
  matched, daemon-stored at the pinned path. ELF
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
- **2b/2c — DONE & verified (commit: relink + structural gate).** `mk/gates/410-rust-store-native.mk`
  + `tests/rust-store-native.sh`: the guix-free stage0 td-builder relinks the upstream
  rustc/cargo interp -> `/td/store` (`elf-set-interp`) and interns the tree guix-free
  (`store-add-recursive`, `TD_STORE_DIR=/td/store`). GATE GREEN under `./check.sh
  rust-store-native`: [supply-chain] sha==pin, [provenance] zero /gnu/store, [structural]
  interp relinked to /td/store + interned content-addressed at
  `/td/store/<hash>-rust-1.96.0-store-native` with zero /gnu/store. Pin migrated to
  `seed/sources/rust-1.96.0.lock` (manifest-warmed, no check.sh edit). The runtime leg is
  the explicit `[PENDING glibc-final]` echo, not faked.
- **RUNTIME LEG — GREEN from-seed + verified-red (2026-06-28, claude-opus-5cd532).** Both blockers are now
  resolved on main: glibc 2.41 (#199) AND — decisively — the **x86_64 toolchain (#201)**.
  The earlier "one-line relink-target swap" claim was WRONG: it ignored ARCHITECTURE. The
  whole `/td/store` toolchain was i686 (`ld-linux.so.2`); the rust pin is **x86_64**, so an
  x86_64 rustc could never run against the i686 glibc 2.41. #201 crosses up to a native
  x86_64 toolchain at `/td/store` (x86_64 glibc 2.41 + `libgcc_s.so.1`) and factored its
  **cross rungs** into a *sourceable* helper (`tests/x86_64-cross-fns.sh`), handing off
  step 8 = "[rust] flip the runtime leg green" to this track. New gate
  **`mk/gates/416-rust-x86_64-runtime-store-native.mk`** + `tests/rust-x86_64-runtime-store-native.sh`:
  - **REUSES the #201 chain without copying it** — sources the x86_64 gate as a function
    library via a one-line `TD_X86_64_LIB=1` guard (behavior-preserving; exposes the 21
    `build_*` rungs + verified pinned-input vars, returns before its build driver). The
    only cross-track edit; no 5th inline copy of the ~800-line base.
  - **Runtime closure (measured, not assumed):** rustc/cargo NEED only the x86_64 glibc
    2.41 libs + `libgcc_s.so.1` + **`libz.so.1`** (libLLVM links zlib dynamically). The
    toolchain provides glibc + libgcc; zlib it does NOT — so the gate **builds x86_64 zlib
    1.3.1 FROM SOURCE** with the cross gcc (new pin `seed/sources/zlib-1.3.1.lock`).
  - **Relink = interp only.** `librustc_driver`'s RUNPATH is already `$ORIGIN/../lib`, and
    glibc's loader resolves the transitive `libz` (via libLLVM, no RUNPATH) from that same
    dir — verified with `env -i` + `--inhibit-cache` (own-root conditions). So the gate
    co-locates the full closure in the tree's `lib/` and relinks **only the interp** →
    `/td/store/ld` (short, fits the original slot in-place; td's own `elf-set-interp`, no
    patchelf; no rpath growth — the in-place rewriter can't grow it).
  - **Own-root run:** intern the self-contained tree at `/td/store`, place the x86_64 loader
    at `/td/store/ld`, RUN `rustc -vV` + `cargo --version` under `store-ns` → rustc/cargo
    1.96.0, `/gnu/store` ABSENT. Durable legs: supply-chain, provenance, no-guix, structural
    (complete lib closure + interp ∈ /td/store), behavioral.
  - **Validated on the host first** (prototype): the full closure runs with a custom interp +
    all libs co-located; libz/libgcc are load-bearing (closure fails without them →
    verified-red material). zlib `configure --shared` + `make libz.so.1.3.1` confirmed.
  - HEAVY (~90 min from-seed; directive 1). **Authoritative from-seed run GREEN (2026-06-28):** built
    the i686 chain → gcc 14.3.0, crossed up to the x86_64 toolchain (glibc 2.41 + libgcc_s), built
    x86_64 zlib, relinked + interned the upstream Rust 1.96.0 toolchain at
    `/td/store/1fifhn8zk0i6x86n7x64b8dzc99yrm2h-rust-1.96.0-x86_64-store-native` (zero /gnu/store), and
    RAN `rustc -vV` + `cargo --version` from `/td/store` in the store-ns own-root → rustc/cargo 1.96.0
    (LLVM 22.1.2), `GNU-ABSENT`.
- **Then** rung 3 (the relinked rustc compiles a Rust program → a `/td/store`-linked binary
  that runs in the own-root) and rung 4 (build the Rust userland + assemble via `td-builder
  profile --store-native`).

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
- rust-relink structural — DONE 2026-06-26: skipped the `elf-set-interp` loop and re-ran
  `./check.sh rust-store-native` → the gate REDDENED exactly at the structural leg:
  `FAIL: interp of rustc not relinked to /td/store (got: /lib64/ld-linux-x86-64.so.2)`.
  Restored the green script (committed first, per [[td-commit-before-red-variants]]). So the
  structural assertion is load-bearing — the relink is the thing it checks.
- rust-x86_64-runtime behavioral + closure-completeness — DONE 2026-06-28: re-ran the gate's exact tail
  (relink interp via td's `elf-set-interp` → co-locate the closure → `store-add-recursive` at /td/store →
  run `rustc -vV` under `store-ns`) on the host with a real working x86_64 closure (host glibc 2.39 +
  libgcc_s + libz stand-ins; the assertion is arch/abi-identical). GREEN: rustc 1.96.0 ran in the own-root
  (LLVM 22.1.2 loaded). Then dropped `libz.so.1` from the interned closure and re-ran → **RED**: `rustc:
  error while loading shared libraries: libz.so.1: cannot open shared object file`. So the behavioral +
  closure-completeness legs are load-bearing — `libz` (the libLLVM transitive dep I initially mis-read as
  absent) is exactly the thing the own-root run depends on, and removing any closure member reds it.

## Parallel-safety

New gate file(s) under `mk/gates/`, a new build recipe + (if needed) a small `td-builder`
relink subcommand, and host warm-prep scripts. **No** edit to `system/td.scm`, `check.sh`
sandbox provisioning, the `Makefile`, or the gcc lane's `tests/bootstrap-*.sh`. Consumes
the `/td/store` glibc as a black box (so it composes with `glibc-final` without touching
it). builder/src changes validate on the `check-engine` smoke tier.
