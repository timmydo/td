# source-bootstrap — td's toolchain from source at /td/store, no guix bytes ever

Handle: claude-fable-db65ca · branch: td-native-store (PR: decision + native build engine)

## Decision (human, 2026-06-21)

> "source bootstrap first, no guix seed ever."

North star sharpened to **no guix *bytes*** (not just no guix process). A guix-captured
seed — even static — fails it: a static `bash` embeds 11 `/gnu/store` strings (glibc
locale/gconv/zoneinfo, bash's own `sh`/bashdb, a bare `/gnu/store`) and its provenance is
guix. A `/gnu/store→/td/store` byte rewrite (store-relocate, #140) only **relabels** guix
bytes — it does not make them td's. So the guix seed (frozen tarball OR relocated) is
**rejected as the foundation**. td's toolchain is built **from source at `/td/store`**.

This **supersedes** the relocated-seed Phases 2–3 of [[user-pm]]: store-relocate (#140) is
demoted from "the break" to at most a removable differential oracle. The native build path
(Phase 1/3) survives — it's the engine this track builds *on*.

## What's already landed (the enabler, this branch)

- **Native `/td/store` build path.** `td-builder build` (and `build-recipe`) stage inputs
  and set `NIX_STORE` at the active `store::store_dir()` (`/td/store` under `TD_STORE_DIR`),
  and the output is content-addressed at that prefix (`make_store_path_in`, Phase 1). So a
  `TD_STORE_DIR=/td/store` build is **native** — re-hashed at `/td/store`, NO post-hoc
  rewrite. (`builder/src/sandbox.rs`: `store_prefix()`, `store_path_name_in`; unit test
  `store_path_name_honors_the_active_prefix`. Validated e2e locally with a stand-in static
  builder; the guix-seed e2e gate was dropped — no non-guix static binary exists yet, which
  is exactly what brick 1 creates.)
- **stage0-builder flock.** Serialized stage0 build+place so concurrent gates sharing a
  `TD_STAGE0_BASE` don't collide ("File exists"). The bootstrap's own stage0 reuses this.

## The bootstrap is a PORT, not research

The bottom of the chain is auditable and reproducible — guix ships exactly this as its
"Full-Source Bootstrap"; live-bootstrap and stage0-posix are the upstream sources. We
vendor/pin those sources, build each stage from source at `/td/store`, and diff against the
guix oracle (same source both ways) until the oracle is retired.

## Brick ladder (each brick: one stage, from source, native at /td/store, verified-red)

The irreducible seed is a tiny hand-auditable binary (stage0-posix `hex0`, a few hundred
bytes) — NOT guix-built. Vendor it + the stage sources into the repo (offline loop), build
upward:

0. **seed + harness** — ✅ DONE (2026-06-22, kaem port). Vendored stage0-posix-x86 `3b9c2bb`'s
   229-byte `hex0-seed` + 618-byte `kaem-optional-seed` + hex sources + the seed kaem script
   into `seed/stage0/`. The `bootstrap-seed` gate (`mk/gates/360`) runs `kaem-optional-seed`
   over `mescc-tools-seed-kaem.kaem` with guix/Guile off env → a full `hex0` + `kaem-0`,
   ALL-DURABLE: seeds match pins (no-guix/auditable), self-reproduce from their hex source,
   the built hex0 works as an assembler, and two runs are byte-identical. (`/td/store` placement
   is deferred to the bricks that produce the dynamic toolchain; the stage0 assemblers are
   static, no store paths.) Next: brick 1 drives `kaem-0` over the rest of the chain.
1. **stage0-posix → M2-Planet** — ✅ DONE (2026-06-22). From brick 0's `kaem-0`, the
   `bootstrap-cc` gate (`mk/gates/362`) drives `mescc-tools-mini-kaem.kaem` over the minimal
   vendored source set (51 hex/C/M1 files: M2libc + M2-Planet + mescc-tools, in `seed/stage0/`)
   to **M2-Planet (a minimal C compiler)** + the core mescc-tools (M1/hex2/kaem) — guix off env.
   ALL-DURABLE: no-guix (no `/gnu/store` in M2-Planet), behavioral (M2-Planet COMPILES a C
   program, M1+hex2 link it, the ELF RUNS returning 7 — a real working toolchain), repro
   (byte-identical M2-Planet across two runs). Verified-red on the runtime value. Next: brick 2
   drives these tools over mes / tinycc.
2. **GNU Mes (mes-m2)** — ✅ DONE (2026-06-23). From brick 1's seed-built M2-Planet +
   mescc-tools, the `bootstrap-mes` gate (`mk/gates/364`) drives the GNU Mes RELEASE source —
   the pinned **`mes-0.27.1.tar.gz`** (`seed/sources/mes-*.lock`, sha256 `183a40ea…`),
   **td-fetched** (not vendored, not guix-fetched) in check.sh's prelude
   (`tools/warm-bootstrap-sources.sh`) into `.td-build-cache/sources/` — to a working **GNU Mes
   Scheme interpreter** (`mes-m2`), guix off env. 0.27.1 is the release *version-matched* to
   stage0-posix `3b9c2bb` (its `x86_64_defs.M1` carries the `xor_eax,eax` vocabulary the stage0
   M2-Planet emits; the 0.27 release predates it). ALL-DURABLE: pinned-input (warmed tarball ==
   lock sha256), no-guix (no `/gnu/store` in mes-m2), behavioral (mes-m2 EVALUATES Scheme —
   `(display 'Hello,M2-mes!)`→`Hello,M2-mes!`, `(+ 1 2 3 4)`→`10`), repro (byte-identical mes-m2).
   Verified-red: truncating a runtime module reds the behavioral leg. The td-fetch tarball pattern
   (`warm-bootstrap-sources.sh` + a per-source lock, no further check.sh touch) is the template
   for bricks 3-5. Next: brick 3 bootstraps tinycc from mes (`mescc`).
3. **MesCC self-host (mes-mescc)** — ✅ DONE (2026-06-23). After mes-m2, Mes's own C compiler
   **MesCC** (`module/mescc`, Scheme, parsing C with **nyacc**) compiles Mes's libc + rebuilds mes
   as `mes-mescc`, and emits **`libc+tcc.a`** (the TinyCC library, → brick 4). The `bootstrap-mescc`
   gate (`mk/gates/366`) drives it from brick-1's seed toolchain + the td-fetched mes-0.27.1 +
   nyacc-1.00.2 tarballs (181 files, 0 failures). ALL-DURABLE: pinned-input (both tarballs == lock
   sha256), no-guix (no gcc/guile/guix on the build PATH; no `/gnu/store` in mes-mescc), behavioral
   (mes-mescc EVALUATES Scheme — `MesCC-self-host!`, `(* 6 7)`→42 — and libc+tcc.a defines the
   compiled libc strlen/malloc/memcpy + tcc's abtod), repro (byte-identical mes-mescc). Verified-red:
   wrong lock sha reds pinned-input; the build/archive legs were seen red during dev.
   - **THE KEY: build i686 (32-bit), not x86_64.** guix's `mes-boot` ALWAYS builds
     `--host=i686-linux-gnu` even on x86-64 — it never builds x86_64 mes-boot; the whole Mes/TinyCC
     layer is i686 and gcc later cross-builds to 64-bit. x86_64 MesCC self-host is the immature path
     and fails mid-libc (this cost a long detour misdiagnosed as a mes-m2 `system*` bug). The amd64
     stage0 tools (M2-Planet/M1/hex2) target i686 fine via `--architecture`/defs — so **NO brick 0/1
     rework**: configure with `--host=i686-linux-gnu` → `mes_cpu=x86, mes_bits=32`.
   - **nyacc-1.00.2** — second source pin (same `seed/sources/*.lock` + `warm-bootstrap-sources.sh`
     pattern as brick 2): `https://download.savannah.nongnu.org/releases/nyacc/nyacc-1.00.2.tar.gz`,
     sha256 `f36e4fb7dd524dc3f4b354d3d5313f69e7ce5a6ae93711e8cf6d51eaa8d2b318`.
   - Build recipe: `configure.sh --host=i686-linux-gnu` with `GUILE=true CC= MES_FOR_BUILD=mes` (force
     the mescc path; host gcc/guile must not be picked up), `M1/HEX2/BLOOD_ELF/KAEM`=absolute seed
     tools, `GUILE_LOAD_PATH`=nyacc+mes modules, `MES_PREFIX`; Mes's own `mesar` archives (no binutils
     `ar`). Then `sh bootstrap.sh` → `bin/mes-mescc` + `mescc-lib/x86-mes/libc+tcc.a`.
   - Remaining for the gate: curated guix-scrubbed-but-build-tools env in the sandbox, reproducibility
     leg, verified-red. tinycc (brick 4) builds on `libc+tcc.a` next.
4. **TinyCC (tcc) from MesCC** — ✅ DONE (2026-06-23). MesCC + `libc+tcc.a` (brick 3) compile the
   mes-patched TinyCC (`tcc-0.9.26-1149-g46a75d0c`, the 30-patch fork MesCC can build, td-fetched
   `seed/sources/tcc-*.lock`, sha256 `f4f6ce12…`) — exactly guix's tcc-boot0 — to **`tcc`, the first
   real C compiler** in the chain. The `bootstrap-tcc` gate (`mk/gates/368`) builds seed → Mes (i686,
   installed) → MesCC → tcc. ALL-DURABLE: pinned-input (3 tarballs == locks), no-guix (no
   gcc/guile/guix on PATH; no `/gnu/store` in tcc), behavioral (tcc compiles+links a C program that
   RUNS returning 42; tcc 0.9.27, 32-bit i386 ELF), repro (byte-identical tcc).
   - **THE BUG (a long detour): `MES_ARENA`.** mescc crashed (segfault / `unbound-variable` /
     `eval/apply unknown continuation`) compiling tcc.c — misdiagnosed across shell (gash, refuted),
     interpreter (mes-m2 vs mes-mescc, both crash), flags, and even a mes-version realign (built
     stage0-posix 1.6.0 + mes 0.25.1 — guix's pair — which ALSO crashed). The real cause: mes's arena
     is in **cells**, the Mes/tcc layer is **32-bit (i686)**, and a "big" `MES_ARENA` (200M–2e9 cells)
     **overflows the 32-bit address space** → segfault. The guix DEFAULT (`MES_ARENA=20000000`, 20M
     cells ≈ 240MB) fits and compiles tcc.c. **No realign needed** — mes 0.27.1 (bricks 0-3) builds tcc
     fine with the sane arena. Lesson: match guix's env (incl. the *default* arena), don't crank knobs.
   - tcc-boot0 recipe: `configure --cc=mescc --elfinterp=/lib/mes-loader --crtprefix=. --tccdir=.`
     (host=i686, ONE_SOURCE=1, `volatile`→`` in conftest.c), then `sh bootstrap.sh` at the default
     arena → `./tcc`. The mescc script's `-L` dir (`share/guile/site/2.2`) must be populated with the
     mes modules (install leaves it empty; GUILE_LOAD_PATH=nyacc only — putting mes modules there
     crashes gash, per the parallel-agent finding).
5. **gcc toolchain (make → binutils → gcc)** — 🚧 first rung DONE (2026-06-23). A staged chain,
   landed rung by rung, mirroring guix's mesboot:
   - **make** ✅ — the `bootstrap-make` gate (`mk/gates/370`) builds seed → Mes → MesCC → tcc (bricks
     0-4), then tcc (`CC=tcc`) compiles **GNU Make 3.80** (`seed/sources/make-*.lock`) — tcc's first
     substantial real-program build (guix's make-mesboot0). DURABLE: pinned-input, no-guix (no
     gcc/guile/guix; no `/gnu/store` in make), behavioral (32-bit ELF, `GNU Make 3.80` runs), repro.
     Setup learned: brick-4 tcc has `crtprefix=.` so crt1.o/crti.o/crtn.o/libc.a are copied into the
     build dir; `-static` avoids the `/lib/mes-loader` interpreter (no root on host); mes `include`
     dirs feed `CPP=tcc -E`. make embeds its build path → repro builds at the same dir.
   - **mesboot tools (gzip + tcc-boot)** ✅ — the `bootstrap-tools` gate (`mk/gates/372`) has the
     seed-built tcc compile guix's gzip-mesboot (**gzip 1.2.4**, a scripted tcc build) and tcc-boot
     (**pristine tcc 0.9.27** — the brick-4 0.9.26 mes-fork compiles pristine 0.9.27, which then
     compiles+runs C → 33). Neither needs make. Setup: unset host `C_INCLUDE_PATH` (it leaks
     unparseable glibc headers; guix sets it to the mes includes); tcc-boot needs a configure pass
     for config.h + its own libtcc1.a to link programs.
   - **patch (make-driven)** ✅ — the `bootstrap-patch` gate (`mk/gates/374`): the tcc-built GNU Make
     compiles **GNU patch 2.5.9** IN the loop sandbox. This clears the make-in-sandbox blocker. The
     old note misdiagnosed it ("recursive makefile" — patch 2.5.9 is a flat build). TWO real causes,
     both env, neither a make bug: (1) make's `SHELL` makefile-variable defaults to `/bin/sh` (absent
     in the sandbox) and make **ignores the `SHELL` env var**, so recipes can't find a shell — fix:
     the make *variable* override `make SHELL=<curated sh>` (guix gets `/bin/sh` free from gash).
     (2) THE SEGFAULT: the gate runs INSIDE the loop's outer `make -j2 --output-sync=target`, which
     exports `MAKEFLAGS` (the **jobserver fds** + `--output-sync`) and `MAKELEVEL`; the minimal
     mes-libc make segfaults trying to honor an inherited jobserver — fix: clear
     `MAKEFLAGS/MFLAGS/GNUMAKEFLAGS/MAKELEVEL` for the nested serial make. ("Works on the host" = no
     outer make there; bootstrap-make passed because it builds make via `sh build.sh`, never running
     a nested make.) Plus guix's pch.c "avoid another segfault" workaround. Serial (guix
     `#:parallel-build? #f`). patch 2.5.9 sha256 `ecb5c646…`.
   - **binutils-mesboot0** ✅ — the `bootstrap-binutils` gate (`mk/gates/376`): the td-built `patch`
     applies guix's vendored boot patch (`seed/patches/binutils-boot-2.20.1a.patch` — drops C99isms,
     fixes malloc proto) and the tcc-built make drives tcc over **Binutils 2.20.1a** → `as` + `ld`.
     First RECURSIVE-make build (bfd/gas/ld/…). Three NEW blockers found+fixed (all env, via the
     cached-chain dev loop): (a) **awk** — `config.status` assembles the top Makefile with awk (absent
     on the sandbox PATH → empty Makefile → "No targets"); glob gawk from the store. (b) **crt across
     subdirs** — tcc's `crtprefix` is searched, NOT `LIBRARY_PATH` (proven via `tcc -vvv`), so crt must
     sit in tcc's absolute `out/lib`; libc via `LIBRARY_PATH`, headers via `C_INCLUDE_PATH` — guix's
     tcc-boot0 search-path setup. Without it, recursive sub-configures fail the link test →
     `GCC_NO_EXECUTABLES`. (c) **flex/bison** — `configure-binutils`'s AC_PROG_LEX/YACC (parsers are
     pre-generated+patched, maintainer-mode off → make never regenerates; flex/bison only satisfy
     configure); glob from the store. guix env: `CPPFLAGS=-D __GLIBC_MINOR__=6 -D MES_BOOTSTRAP=1`,
     `AR=tcc -ar`, `CXX=false`, `RANLIB=true`, serial, `--with-sysroot=/`. Build-time host tools
     (bzip2/awk/flex/bison) are scaffolding only — the `[no-guix]` leg verifies as/ld carry no
     `/gnu/store` bytes. Behavioral: as+ld assemble+link a tiny i386 program that runs → 42.
   - **gcc-core-mesboot0** ✅ (gcc 2.95.3) — **THE milestone**: the `bootstrap-gcc` gate (`mk/gates/378`)
     has the tcc-built make + binutils build a real **C compiler** from the seed (guix's
     gcc-core-mesboot0). The td-built patch applies guix's vendored `gcc-boot-2.95.3.patch` (disables
     DOC, avoids fixproto, fixes the libgcc archive trickery); the build uses binutils' `as`/`ld`/`ar`
     (`AR=ar`), a `config.cache` float-format hint, `CC="tcc -D __GLIBC_MINOR__=6"`, `LANGUAGES=c`, a
     `remove-info` step (no makeinfo) and an `install2` step that assembles `libgcc.a` + `libc.a` into
     gcc-lib. NEW blocker found+fixed (via the cached-chain-through-binutils dev harness): gcc's
     Makefiles exec helper scripts (`move-if-change`, `mkinstalldirs`, …) DIRECTLY via their
     `#!/bin/sh` shebang — absent in the sandbox; rewrite all such shebangs to the curated sh after
     configure. Behavioral: gcc reports 2.95.3 and **compiles+links+runs a C program → 42**.
   - **glibc-mesboot0** ✅ (glibc 2.2.5, #168) — the `bootstrap-glibc` gate (`mk/gates/380`): the seed
     gcc + binutils build the **C library** against host-produced Linux UAPI headers
     (`tools/warm-kernel-headers.sh` from the pinned linux-4.14.67 source — guix's headers are a
     prebuilt blob, rejected; must hand-generate `linux/version.h` or "kernel TOO OLD"). Blockers:
     `libgcc2.a` into gcc out/lib (glibc links `-lgcc2`); seed gcc's `cpp` on PATH (`scripts/cpp` does
     `which cpp`). Behavioral: a program statically links libc.a → 42.
   - **gcc-mesboot0** ✅ (gcc 2.95.3 rebuilt, #170) — the `bootstrap-gcc-mesboot0` gate (`mk/gates/382`):
     the FIRST gcc rebuilds GCC 2.95.3 with `CC=<gcc>` (not tcc) now resolving headers/libs to **glibc**
     instead of mes libc (guix's gcc-mesboot0) — the toolchain re-baseline. `RANLIB=true`, `LANGUAGES=c`,
     simpler install2. Behavioral: the glibc-based gcc compiles+links+runs C → 42.
   - **binutils-mesboot1** ✅ (binutils 2.20.1a rebuilt, #173) — the `bootstrap-binutils-mesboot1` gate
     (`mk/gates/384`): gcc-mesboot0 rebuilds binutils against glibc (guix's binutils-mesboot1). guix
     drops binutils-mesboot0's overrides for a **plain** configure: `CC=<gcc-mesboot0>`, the real
     `ar`/`ranlib`, glibc as libc; the boot patch's `MES_BOOTSTRAP` #ifdefs compile the real-glibc side.
     Two gotchas: NO `-B<glibc>/lib` (gcc's "never used" `-E` warning → autoconf marks `HAVE_LIMITS_H`=no
     → fibheap `LONG_MIN`; crt via `LIBRARY_PATH`) + PURE kernel UAPI headers (not the mes-merged set).
     Behavioral: the gcc-built, glibc-linked `as`+`ld` assemble+link+run C → 42.
   - **make-mesboot** ✅ (GNU Make 3.82, #174) — the `bootstrap-make-mesboot` gate
     (`mk/gates/386`): make-mesboot0 (the tcc-built make 3.80) rebuilds GNU Make 3.82 with gcc-mesboot0
     + glibc + binutils-mesboot0 — a glibc-linked make for the gcc-mesboot1 arc. Plain configure +
     `LIBS=-lc -lnss_files -lnss_dns -lresolv` (static glibc nss). Behavioral: make 3.82 parses a
     Makefile + runs a recipe → BUILT.
   - **gcc-core-mesboot1** ✅ (GCC 4.6.4, C, #176) — the `bootstrap-gcc-core-mesboot1` gate
     (`mk/gates/388`): the FIRST modern modular gcc, built by gcc-mesboot0 + binutils-mesboot1 +
     make-mesboot against glibc, with gmp 4.3.2 / mpfr 2.4.2 / mpc 1.0.3 unpacked **in-tree**. td's
     glibc is static-only, so (unlike guix's `-dynamic-linker`) td builds it STATIC (`LDFLAGS=-static
     -B<glibc>/lib`, link-only so no autoconf `-E` regression); `MAKEINFO=true` skips the texinfo docs;
     `cmp`/`diff` linked from the store (move-if-change in `make install`). Behavioral: gcc 4.6.4 → C → 42.
   - **gcc-mesboot1** ✅ (GCC 4.6.4, C AND C++, #178) — the `bootstrap-gcc-mesboot1` gate
     (`mk/gates/390`): overlays the gcc-g++-4.6.4 front-end + `--enable-languages=c,c++` (cc1plus + a
     static libstdc++) — the c++ compiler the next gcc (gcc-mesboot, GCC 4.9, itself C++) needs.
     Behavioral: gcc runs C → 42 AND g++ runs a C++ program → 42; repro gcc+g++ drivers + output.
   - **binutils-mesboot + gawk-mesboot** ✅ (#179) — the `bootstrap-binutils-gawk-mesboot` gate
     (`mk/gates/392`): the gcc-mesboot1 (c++) toolchain rebuilds binutils 2.20.1a (guix's binutils-mesboot)
     AND builds GNU awk 3.1.8 (guix's gawk-mesboot) — the two tools glibc-mesboot 2.16.0 needs. Behavioral:
     as+ld → C → 42; gawk `'{print $2}'` → beta + sums → 42. Repro: byte-identical as+ld+gawk.
   - **glibc-mesboot** ✅ DONE (2026-06-25, #183) (GNU libc 2.16.0, guix's glibc-mesboot) — the `bootstrap-glibc-mesboot` gate
     (`mk/gates/394`): the MODERN, nptl-threaded C library, built by gcc-mesboot1 + binutils-mesboot +
     gawk-mesboot in two stages (bootstrap headers → full nptl library). td builds it STATIC (guix shared:
     a shared build made the new libnsl.so leak the old static glibc-mesboot0's non-TLS errno); the BUILD
     tools get glibc-mesboot0+kernel headers via C_INCLUDE_PATH (target objects use -nostdinc). Library-only:
     drop the nscd program + texinfo `manual` (don't link/run statically), empty soversions.mk for install.
     Behavioral (green): a C program AND a pthread (nptl) program link statically + run → 42; repro: crt
     objects + a linked nptl program byte-identical across two builds. Two sandbox-only gotchas the cached
     dev harness can't see: the lock is named `glibc-mesboot-2.16.0.lock` so `glibc-*.lock|head -1` still
     resolves the chain's 2.2.5; Makeconfig's `SHELL := /bin/sh` + ~14 script shebangs are sed'd to the
     curated `sh` (the loop sandbox has no `/bin/sh`).
   - **gcc-mesboot** 🚧 (GCC 4.9.4, guix's gcc-mesboot — the FINAL mesboot gcc) — the `bootstrap-gcc-mesboot`
     gate (`mk/gates/396`): gcc-mesboot1 (4.6.4) + binutils-mesboot build GCC 4.9.4 against the static glibc
     2.16.0, from one pristine tarball (gmp/mpfr/mpc in-tree; no modular g++, no boot patch — the 7 guix
     origin patches all touch DISABLED components). td builds it STATIC (guix --enable-shared via the
     gcc-mesboot1-wrapper's dynamic linker): the static-only glibc means libgcc's `dl_iterate_phdr`-using
     unwinder can't link dynamically, so every compile-and-run test links static — done with LINK-ONLY flags
     that keep CC clean (LDFLAGS=`-static -B<glibc>/lib` for host link tests, CC_FOR_BUILD=`<gcc> -static`
     for the in-tree gmp/mpfr/mpc build tools), so autoconf header tests aren't polluted by a `-B` warning
     (the binutils-mesboot1 lesson). Dev harness GREEN in 3 iterations: gcc (GCC) 4.9.4 compiles+links a C
     program AND a C++ (libstdc++) program → 42. Repro: gcc/cpp drivers byte-identical + `gcc -S` output
     deterministic (cc1 carries a benign stabs stamp). Then the modern toolchain at `--prefix=/td/store`.
6. **glibc + binutils** — the C library + linker/assembler, native `/td/store` RUNPATH.
7. **coreutils / bash / make / sed / grep / tar / gzip / …** — the build userland td's
   recipes already assume, now from the `/td/store` source toolchain.
8. **retire the guix seed** — the corpus locks (`hello-no-guix.lock`, …) point at the
   `/td/store` toolchain; the guix toolchain seed is removed from every build's inputs;
   guix remains only as the removable `guix build --check` oracle (retired last, §5).

## Durable vs oracle

Each brick carries DURABLE assertions (the stage binary RUNS and builds the next stage; its
output is native `/td/store`, reproducible under `td-builder check`; NO `/gnu/store` byte in
it) and may carry a REMOVABLE guix oracle (the same source built by guix produces an
equivalent tree). The oracle is deleted when guix is retired; the durable legs are the keep.

## Verified-red

- Native build engine (this branch): revert the `NIX_STORE`→`store_dir()` wiring →
  the build sees `NIX_STORE=/gnu/store` → the "ran at /td/store" leg reds. (Seen locally.)
