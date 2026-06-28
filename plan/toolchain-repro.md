# toolchain-repro — working notes

Handle: claude-opus-686775 — started 2026-06-27.

## Goal

Make the MODERN /td/store toolchain (gcc 14.3.0, glibc 2.41, binutils 2.44) build
byte-reproducibly so the interned content-addressed path is STABLE across builds and
machines. This is the prerequisite for td-subst chaining (a stable key) and the actual
"skip the 90-min rebuild" payoff (consumer fetch).

## What actually varies (ground truth to confirm in the diag run)

td's NAR (builder/src/nar.rs) hashes ONLY: node type, the executable bit (mode & 0o100),
file CONTENTS, symlink target, and the sorted directory structure. It does NOT hash
mtimes / uid / gid / non-exec mode bits. So filesystem "install mtimes" are IRRELEVANT to
the content-addressed path. Only file *contents* (+ exec bit + structure) can vary:

1. **Build-path leak in DWARF.** The modern rungs are built with the autoconf default
   `CFLAGS=-g -O2` inside a `mktemp -d` build dir. `-g` bakes the absolute build path into
   `.debug_*` (DW_AT_comp_dir / DW_AT_name), and that path is random per build → the
   compiler/linker binaries (cc1, ld, as, …) differ build-to-build. (This is the
   "cc1 stamp" of the td-toolchain-store-native caveat.)
2. **Archive member mtimes.** Installed `.a` files (libbfd.a, libopcodes.a, libctf.a;
   libgcc.a, libstdc++.a; libc.a …) carry the build-time mtime/uid/gid of each member,
   written by the build-time `ar` (the mesboot ar, which is NOT deterministic — the
   `--enable-deterministic-archives` configure flag only changes the BUILT ar's default).

## Fix

Reusable post-install normalization `tests/repro-lib.sh`:
`repro_normalize_tree DIR STRIP [LOADER LIBPATH]` runs
`strip --strip-debug --enable-deterministic-archives` over every ELF and every ar archive
in DIR. `--strip-debug` removes the build-path-bearing debug sections while KEEPING the
symbol table (so static libs/objects still link); `--enable-deterministic-archives` (`-D`)
zeros archive member mtime/uid/gid. STRIP is the freshly-built modern binutils `strip`
(run via the explicit ld-linux loader in the build sandbox, where /td/store is absent).

## Repro leg (durable, per gate)

Build the final modern rung TWICE, normalize both, assert the interned /td/store CA path
is byte-identical. This is an intrinsic double-build reproducibility assertion (DESIGN
"durable" menu) — it holds with no guix oracle in the room. Verified-red: the RAW
(un-normalized) double-build produces DIFFERENT CA paths.

## Ladder

- [x] inc1: binutils-2.44 (rung A, cheapest) reproducible + tests/repro-lib.sh helper. LANDED-READY.
      - [x] diag harness: build chain once (snapshot) + binutils-2.44 twice, confirm RAW
            CA paths differ (35 files: 26 ELF + 6 .a + 3 .la), confirm normalized CA paths match.
      - [x] fix 2 surprises the diag surfaced: strip SIGBUSes on its own in-tree binary
            (run strip from a copy outside the tree); libtool .la leak the build path via
            relink_command (drop them).
      - [x] wire repro_normalize_tree + the double-build repro leg into the gate.
      - [x] FROM-SEED gate GREEN: [repro] two builds normalized → byte-identical
            (nar-hash sha256:2ddda173…); [behavioral] stripped as/ld link → 42; [structural]
            /gnu/store absent. Verified-red: RAW double-build differs in 35 files.
- [ ] inc2 (follow-up): glibc-2.41 adopts the helper + repro leg.
- [ ] inc3 (follow-up): gcc-14 adopts the helper + repro leg (expensive double-build).

## Nuance for follow-ups (cross-run stability)

The [repro] leg proves binutils-2.44's BUILD is deterministic: two builds against the SAME
chain → identical /td/store path. The path observed differs run-to-run only because its INPUT
(the chain's glibc-shared CA hash, baked as the interp/RUNPATH string) differs — the chain
rungs (gcc-mesboot / glibc-mesboot) are NOT yet normalized, so the full closure is not yet
cross-machine stable. Fully stable cross-machine paths (what td-subst consumer-fetch needs)
require the same repro_normalize_tree treatment on the upstream chain rungs + glibc-2.41 /
gcc-14. This PR delivers the reusable mechanism + the binutils-2.44 layer.

## Dev harnesses (not gates)

- `.td-build-cache/_repro/chain.sh` — head of the binutils-244 gate + a snapshot of the
  three chain outputs (gccm1 / binutils-mesboot / glibc-shared) to
  `.td-build-cache/_repro-chain/`. Run ONCE via `tools/check-rung.sh` (~90 min).
- `.td-build-cache/_repro/iterate.sh` — loads the snapshot, builds binutils-2.44 twice,
  diffs + normalizes + compares CA paths. Fast iteration.
