# edge-owned-23 — drive the guix-dependence edge-owned metric to 23/23

Handle: claude-opus-feb041 — claimed 2026-06-19.

## Goal

PR #95 added the edge-owned census (a recipe is credited only when its declared input
edges that are owned recipes are wired to td outputs) and chained grep + nano. The
census then listed the remaining guix-wired edges. This track closes them so EVERY
owned recipe is built FROM td inputs.

Remaining edges (from the #95 census): readline→ncurses, gettext-minimal→libunistring
+ncurses, bash→readline+ncurses. All deps already td-built (corpus-deps), so chainable.

## Two pieces

1. **Generalize the build-plan gate.** Folded the per-subject gates (365 grep, 366
   nano) into ONE manifest-driven gate: it loops over every `tests/td-chained-edges.txt`
   line, builds the deps + subject in a SHARED scratch (so bash←readline←ncurses builds
   each dep once), and asserts per subject — DURABLE structural (subject .drv references
   td's deps, NOT guix's), DURABLE behavioral (a per-subject case; library subjects
   assert their .so), DURABLE repro (`td-builder check`), MIGRATION ORACLE (distinct
   path). 366 deleted. The behavioral LD path is the subject's own `lib/` + the dep lib
   dirs (gettext's msgfmt needs its own libgettextsrc.so + td's ncurses/libunistring).

2. **ncurses --with-shared.** gettext's libtextstyle builds a SHARED lib that links
   ncurses; td's ncurses shipped only a non-PIC static `libncurses.a` →
   `ld: relocation R_X86_64_32 … recompile with -fPIC`. Added `--with-shared` to
   recipe-ncurses.ts so ncurses ships PIC shared libs. Blast radius is bounded:
   `system/td.scm` has NO ncurses (the shipped OS uses guix's ncurses, not td's recipe),
   so only corpus-deps (its run-test already sets LD_LIBRARY_PATH to the lib's own dir)
   and the build-plan chains consume td's ncurses. nano/bash now dynamically link it
   (resolved at runtime from the dep lib dir).

## Result

edge-owned **23 / 23** — every owned recipe's declared input edges wired to td outputs.
The deepest is bash ← td's readline ← td's ncurses (a 2-level td DAG). corpus-union /
shipped-system td-reproducible counts unchanged (the census closure is guix-lowered).

## Verified-red

- **--with-shared load-bearing (gettext)**: on static ncurses gettext's build red at
  `ld: … recompile with -fPIC` (libtextstyle); with `--with-shared` it builds and
  `msgfmt --version` runs (libtextstyle loads td's shared ncurses).
- **generalized gate structural**: break the `td-recipe-output` marking → grep builds
  with guix's pcre2 → `FAIL: grep's .drv does NOT reference td's pcre2`, exit 2.
- **census edge metric**: drop a manifest line (bash) → edge-owned 23→22, bash →
  guix-wired, census exit 1.

The Guix byte-count legs stay the removable migration oracle; edge-owned is durable
(td's own recipes + manifest, proven by the build-plan gate).
