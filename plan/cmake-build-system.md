# cmake-build-system — working notes

Handle: claude-opus-fcfa37  ·  Track: cmake-build-system  ·  2026-06-19

## Goal (DESIGN §5 move-off-Guile, corpus-independence)

Add a **cmake** build system to td-builder's own Rust builder (`builder/src/build.rs`),
so td can build a cmake-based package from source with NO gnu-build-system and NO
guix/Guile in the build path. Model it on the existing autotools (`gnu`) / rust paths
and the `build-recipe` dispatcher (`buildSystem "gnu"→autotools-build`,
`"rust"→rust-build`; add `"cmake"→cmake-build`). cmake is a guix-built SEED input
(toolchain retired last, §5), exactly as the autotools path uses make/gcc.

## Smallest increment (this PR)

A trivial cmake demonstrator (a tiny CMakeLists building a `hello`-style binary),
authored as a TS recipe + lock, proving the cmake build-system path end to end —
mirroring how rust-build/rust-vendor started with a demo, not an upstream package. A
real upstream cmake package is a FOLLOW-UP increment.

Pieces:
- `builder/src/build.rs::run_cmake` — set-paths -> unpack -> `cmake <src>
  -DCMAKE_INSTALL_PREFIX=$out -DCMAKE_BUILD_TYPE=Release` in a build dir -> `make` ->
  `make install`. Sibling of `run` (autotools). No Guile in the build.
- `builder/src/main.rs` — dispatch `Some("cmake-build")` → `build::run_cmake`; the
  `build-recipe` dispatcher routes `buildSystem "cmake"` → phase runner `"cmake-build"`.
- `tests/ts/td-spec.d.ts` — extend the `BuildSystem` union with `"cmake"`.
- `tests/cmake-demo/` — `CMakeLists.txt` + `hello.c` building `td-cmake-hello`.
- `tests/ts/recipe-td-cmake-demo.ts` + `tests/td-cmake-demo.lock` (seed: cmake +
  gcc-toolchain + make + coreutils + bash + tar + gzip; source interned at gate time).
- `mk/gates/350-cmake.mk` — builds the demo via `td-builder build-recipe` with
  guix/Guile scrubbed from PATH and asserts STRUCTURAL / DURABLE behavioral / DURABLE
  repro / removable migration-oracle.
- `tests/guix-dependence.scm` — add `td-cmake-demo` to `self-host-specs` (a from-scratch
  demo has no `specification->package` corpus oracle by design, like td-vendor-demo).

## Differential + durable discipline (gate legs)

- [STRUCTURAL] the build runs with guix/Guile off PATH and produces the binary, AND the
  .drv selected the cmake phase runner (`arg cmake-build`).
- [DURABLE behavioral] the binary runs and prints its expected line.
- [DURABLE repro] `td-builder check` double-build agrees the output is reproducible
  (td's own oracle, not `guix build --check`).
- [MIGRATION ORACLE, removable] the same demonstrator lowered through guix's
  `cmake-build-system` lands at a DISTINCT store path (own, then diverge). Labelled
  removable — retiring guix deletes this leg, not a rewrite.

## Verified-red evidence (2026-06-19/20)

The gate was committed by the first agent but NEVER reached a green baseline — it
parked on a structural check that could not pass. Two never-run gate bugs were found
and fixed (the `run_cmake` implementation itself is correct); each fix was observed
red→green, and the demonstrator legs were deliberately broken:

- **STRUCTURAL drv-arg leg.** Gate grepped the literal `'arg cmake-build'`, which
  appears in no `.drv`. Observed RED (`FAIL: the .drv did not select the cmake-build
  phase runner`) on the as-committed gate. Direct `.drv` inspection: td's cmake build
  encodes the phase runner as the builder args list `["cmake-build"]` (autotools
  encodes `["autotools-build"]`), so the leg discriminates cmake from the other build
  systems. Fixed the pattern to `\["cmake-build"\]` → GREEN.
- **MIGRATION ORACLE leg.** `guix repl | head -1` captured the GNU Guile startup
  BANNER, not the store path, and the unguarded pipeline aborted under `pipefail`.
  Observed RED (silent gate abort right after the repro leg). Fixed to grep the store
  path + guard the assignment → GREEN, now reporting the distinct guix path
  `…-td-cmake-demo-guix-0.1.0`.
- **DURABLE behavioral leg (RED-A).** Perturbed `hello.c`'s printed string →
  `FAIL: td-cmake-hello printed 'td cmake-build PERTURBED', expected 'td cmake-build
  hello'` (exit 2). Restored → green.
- **STRUCTURAL binary-present leg (RED-B).** Perturbed `CMakeLists.txt` install
  `DESTINATION bin` → `libexec` → `FAIL: cmake build produced no binary at
  …/bin/td-cmake-hello` (exit 2). Restored → green.

Restored tree re-runs GREEN (all four legs); working tree clean after the sweep.

## Sub-task ladder

1. [x] claim + draft PR
2. [x] BuildSystem union + dispatcher + run_cmake driver (cargo build green, unit smoke)
3. [x] demonstrator tree + recipe + lock + census enrollment
4. [x] gate 352-cmake.mk green (after fixing the two never-run gate bugs above)
5. [x] verified-red: behavioral (hello.c) + structural (CMakeLists) reds; oracle/drv-arg
       legs observed red→green on the fixes
6. [ ] full ./check.sh, rebase, affected-checks, ready PR + auto-merge armed
