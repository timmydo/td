# x86_64-toolchain — working notes

Handle: claude-opus-d5df49 · claimed 2026-06-27 · section: side (parallel-safe)

## Goal (human, 2026-06-27 — "x86_64 toolchain first")

Cross the full-source bootstrap UP from **i686** to a **native x86_64** toolchain at
`/td/store`, so the x86_64 upstream Rust pin (`rust-store-native`, #196) can actually RUN
from `/td/store` and the Rust userland (procs/fd/ripgrep/…) can be built there.

### Why this is needed (the finding that reframed task 6)

The full-source bootstrap is **i686/32-bit by construction** (mes → tcc → gcc → glibc, the
standard bootstrappable-builds shape). The modern rungs stayed i686 too:
`build_glibc_241` / `build_gcc_14` configure `--host=i686-unknown-linux-gnu
--disable-multilib`, and the glibc 2.41 ships **`ld-linux.so.2`** (32-bit). So `/td/store`
is a 32-bit userland today (the C/C++ "→42" demos are 32-bit ELF).

The `rust-store-native` pin is **`rust-1.96.0-x86_64-unknown-linux-gnu`** (64-bit). An
x86_64 rustc cannot run against an i686 glibc 2.41 (wrong loader, wrong arch). So
glibc≥2.17 was **necessary but not sufficient** — the runtime leg is blocked on an
**architecture** match. (The #196 track note + the `td-glibc-241-store-native` /
`td-rust-store-native-track` memories claimed a "one-line relink-target swap" unblocks it
once glibc-final lands; that was wrong — it ignored arch. Corrected.)

`bootstrap-mescc.sh:8` already names this transition: *"Built i686 … gcc later
cross-builds to 64-bit; the x86_64 MesCC self-host path is immature."* This track is that
gcc-cross-to-64-bit step, which the chain never actually took.

## Approach: guix-style cross-then-native, built BY the existing i686 gcc 14.3.0

The existing i686 gcc 14.3.0 + binutils 2.44 are a capable modern toolchain. Use them to
cross-build an x86_64 toolchain (Linux-From-Scratch / crosstool-NG shape):

1. **cross binutils 2.44** — `--target=x86_64-pc-linux-gnu` (build/host i686). Produces
   `x86_64-pc-linux-gnu-{as,ld,ar,…}` that RUN on i686 and emit x86_64.
2. **cross gcc 14.3.0 stage1** — `--target=x86_64-pc-linux-gnu --enable-languages=c
   --without-headers --disable-shared --disable-threads --disable-libssp …` built by i686
   gcc 14. A C-only cross-cc with no libc, enough to compile glibc.
3. **x86_64 kernel headers** — host warm-prep (the existing `KH_TB` is **i386**; need an
   `x86_64`/`x86` ARCH=x86_64 `headers_install`). New `tools/warm-kernel-headers-x86_64.sh`
   (or extend the existing warm), pinned linux-4.14.67, produced on the host (sandbox
   can't run the kernel build — same constraint as the i386 headers).
4. **x86_64 glibc 2.41** — `--host=x86_64-pc-linux-gnu --build=i686-…` built by the
   stage1 cross-cc; produces `libc.so.6` + `ld-linux-x86-64.so.2`. Same glibc-2.41 gotchas
   as the i686 build (no DT_RPATH/DT_RUNPATH in libc.so.6; gawk by name; modern binutils).
5. **cross gcc 14.3.0 stage2** — `--target=x86_64 --enable-languages=c,c++
   --enable-shared` against the x86_64 glibc sysroot → **libgcc_s.so.1** (rustc needs it
   dynamically) + libstdc++.
6. **[rung X1 — milestone]** the cross-gcc compiles an x86_64 C AND C++ program (interp =
   `/td/store/<x86_64-glibc>/lib/ld-linux-x86-64.so.2`, RUNPATH = the x86_64 glibc + gcc
   libdirs) that RUNS in the `store-ns` own-root → 42, `/gnu/store` ABSENT. **This unblocks
   the x86_64 rust runtime leg.**
7. **[rung X2]** a **native** x86_64 gcc 14.3.0 (the gcc binary itself x86_64), built by
   the cross-gcc (`--build=i686 --host=x86_64 --target=x86_64`) → "native 64-bit gcc".
8. **[rust]** flip the `rust-store-native` runtime leg green on x86_64 (relink rustc to the
   /td/store x86_64 glibc + drop libgcc_s into rustc's lib/, RUN rustc from the own-root,
   compile hello-world → runs), then build the x86_64 rust userland.

Rung X1 is this session's target (the rust-unblocking milestone). X2 + rust follow.

## Build/iteration model

The i686 base (the existing 21-function chain → gcc 14.3.0 + binutils 2.44 + glibc 2.41 +
the SHARED glibc 2.16.0) is ~86 min from the seed and is the prerequisite for every cross
rung. Iterate with a cached dev harness (`tools/check-rung.sh`, memory
`td-check-rung`): the harness builds + caches the i686 base once in
`.td-build-cache/sbdev1/x86_64-*/`, then rebuilds only the cross rung under test. The
AUTHORITATIVE gate still builds the whole chain from the 229-byte seed (directive 1).

New self-contained script `tests/bootstrap-x86_64-toolchain-store-native.sh` = the
glibc-241 chain (copied — the lane's superset convention) + the new x86_64 cross functions
+ the x86_64 own-root verification. New gate `mk/gates/414-…`.

## Legs (DURABLE — no guix oracle in any)

- **[pinned-input]** chain tarballs + boot patches + gcc-14.3.0 + binutils-2.44 +
  glibc-2.41 + the x86_64 kernel headers match sha256.
- **[no-guix]** built with gcc/g++/cc/guile/guix DENIED; no `/gnu/store` in the x86_64
  glibc 2.41 `libc.so.6` NOR the cross gcc/cc1.
- **[content-addr]** interned `/td/store/<nar-hash>-<name>`.
- **[behavioral]** an x86_64 DYNAMIC C AND C++ program links vs the x86_64 glibc 2.41 and
  RUNS in the own-root → 42; the binary is `ELF 64-bit` with interp
  `ld-linux-x86-64.so.2`.
- **[structural]** inside the own-root `/td/store` IS the store AND `/gnu/store` ABSENT.

## Verified-red plan

- **arch leg:** assert the verify program is `ELF 64-bit LSB` with interp
  `…/ld-linux-x86-64.so.2` (the whole point — distinguishes it from the i686 lane). Break:
  point the cross compile at the i686 gcc → the program is 32-bit → the `ELF 64-bit`
  assertion reddens.
- **behavioral:** the x86_64 program returns 42 in the own-root. Break: drop the baked
  x86_64 interp → it can't run in the own-root (no /lib64 loader there).
- **no-guix:** break by leaving a `/gnu/store` path in the x86_64 glibc → the no-guix leg
  reddens.

## Parallel-safety

New gate file + new self-contained script + a new x86_64 kernel-headers warm (host prep).
**No** edit to `system/td.scm`, `check.sh`, the `Makefile`, or the i686 lane's
`tests/bootstrap-*.sh` (so it composes with the `source-bootstrap` mainline track without
colliding). builder/src changes (if any) validate on `check-engine`.

## Progress

- 2026-06-27 — track claimed; ladder designed; the i686/x86_64 arch finding surfaced and
  the stale rust records corrected.
- 2026-06-28 — **rung X1 GREEN in the dev harness** (cached i686 base). All four cross rungs
  build + the own-root verify passes:
  `cross binutils 2.44 → cross gcc 14 stage1 → x86_64 glibc 2.41 → cross gcc 14 stage2
  (libgcc_s.so.1)`, then an x86_64 (ELF64) C **and** C++ program runs in the store-ns
  own-root → `CRC=42 CPPRC=42 GNU-ABSENT`, interned at
  `/td/store/<hash>-glibc-2.41-x86_64`, no `/gnu/store`. The cross bugs found+fixed
  (each verified-red before the fix):
  1. cross build wrapper needed `-idirafter <glibc216>/include` (NOT `-isystem`) so
     libstdc++'s `<cstdlib>` `#include_next <stdlib.h>` resolves.
  2. split cross toolchain → bake `--with-as`/`--with-ld` into the cross gcc (else it execs
     a plain `as`).
  3. glibc `--with-binutils` must point at the **plain-named** cross tooldir
     (`$xbu/$XTARGET/bin`) so `OBJCOPY` resolves to the x86_64 binutils.
  Dev-harness-only fixes (the from-seed gate is unaffected): a shim recreating the cached
  binutils-2.44 `as`/`ld` baked interp path; rung caching (`X86_RUNG_CACHE`).
- **Verified-red (arch leg):** the `ELF64` assertion REJECTS an i686 binary
  (`dbg-vred`: compile c.c with the i686 gcc14 → readelf class != ELF64 → red). The whole
  point of the track (x86_64, not i686) is load-bearing.
- Next: the authoritative from-seed gate (`./check.sh bootstrap-x86_64-toolchain-store-native`,
  no cache — directive 1); then land. Then rung X2 (native x86_64 gcc) + the rust x86_64
  runtime/userland.
