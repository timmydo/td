# plan/corpus-leaf-recipes.md — owned recipes for pure-autotools leaf packages

Track: **corpus-leaf-recipes** (DESIGN §7.1 corpus-independence follow-on; move-off-Guile §5).
Claim: claude-opus-69899c, 2026-06-19. Single writer: the claiming agent.

## Goal

Move the corpus-union census (was td-reproducible 23/320) by adding a BATCH of OWNED
recipes for straightforward `./configure && make` LEAF packages whose deps are ALREADY
in the owned toolchain seed — NO new build system, NO new toolchain work. Each recipe:
- builds via `td-builder build-recipe` with guix/Guile OFF PATH (structural),
- runs / ships its lib+header (durable behavioral),
- is reproducible via `td-builder check` double-build (durable repro),
- diverges when perturbed (verified-red, self-discrimination leg).

## Candidate triage (guix build-system + inputs at the pin)

Queried `package-build-system` / `package-inputs` / `package-native-inputs`:

| pkg    | bs  | inputs | native       | verdict                                    |
|--------|-----|--------|--------------|--------------------------------------------|
| which  | gnu | ()     | ()           | PICK — cleanest pure leaf, zero args       |
| gperf  | gnu | ()     | ()           | PICK — pure leaf (C++), only no-parallel-tests |
| m4     | gnu | ()     | ()           | PICK — leaf; minor /bin/sh substitute phase |
| gmp    | gnu | ()     | (m4)         | defer — needs m4 as a build-time tool      |
| bzip2  | gnu | ()     | ()           | DROP — custom Makefile (no ./configure), multi-output, shared-lib phases |
| bison  | gnu | (flex) | (perl m4)    | DROP — needs perl; circular with flex      |
| flex   | gnu | (bison)| (help2man m4)| DROP — needs perl/help2man; circular       |

Picks (pure autotools leaves, smallest-increment order): **which** → **gperf** → **m4**.

## Lock recipe

A leaf lock = the 15-line toolchain SEED (sed coreutils file make gzip bzip2 tar patch
gcc-toolchain grep xz gawk diffutils bash findutils — identical across sed/patch/etc.)
+ a `<spec>-source <store-path>` line. No named `inputs` for these leaves.
Source path derived via `package-source-derivation` → `derivation->output-path`.
Toolchain seed copied verbatim from an existing leaf lock (tests/sed-no-guix.lock head -15).

## Sub-task ladder + verified-red log

### Recipe 1: which (2.21) — pure autotools leaf, zero inputs

- recipe: `tests/ts/recipe-which.ts` (mirror://gnu/which/which-2.21.tar.gz, sha256
  1bgafvy3ypbhhfznwjv1lxmd6mci3x1byilnnkc7gcr486wlb8pl).
- lock: `tests/which-no-guix.lock` = 15-line toolchain seed (from sed lock) +
  `which-source /gnu/store/5mxjvwfd2wrw5w4pm5am1wvqb3bv5r1h-which-2.21.tar.gz`.
- source realized into the store (warm-store prep, §5: fixed-output fetch, content-
  addressed, pinned by hash; the loop stays offline).
- gate: added `which` to `corpus_SPECS` in mk/gates/220-corpus-no-guix.mk; behavioral
  case asserts `which --version` (which v2.21) AND `which ls` locates `$CU/bin/ls`.
- census: `tests/guix-dependence.expected` re-baselined 23/320 -> **24/322** corpus-union
  (which also enters shipped-system 19->20 / 3130). Generated snapshot, not the spine.
- self-discrimination leg: `tests/ts/recipe-which-perturbed.ts` adds a load-bearing
  `configureFlags: ["--disable-iberty"]`; the gate assembles its .drv and asserts a
  DISTINCT store path from the real which .drv (the corpus path resolves SOURCE from
  the lock, so the source-hash perturbation pattern is vacuous here — a recipe FIELD
  that flows into the drv is the load-bearing perturbation).

VERIFIED-RED (drv-level, 2026-06-19):
  - real which            -> /gnu/store/8firkjn57dnq2fi0nxbncf8szsk3nwal-which-2.21.drv
  - perturbed (+flag)     -> /gnu/store/fdkkqp7s7i77zvmlzid5aw80lairrchb-which-2.21.drv  [DISTINCT -> green]
  - identical copy (probe)-> /gnu/store/8firkjn57dnq2fi0nxbncf8szsk3nwal-which-2.21.drv  [== real -> gate's pdrv!=rdrv FAILS = RED]
  So the leg is non-vacuous: a load-bearing field change diverges; an identical recipe
  matches and reds the gate. Confirmed by direct assemble before trusting the pass.

HOST NOTE: two other heavy agents saturated the host (load spiked to 130+ when a 2nd
corpus check + drained orphan builds piled up). Capped/killed my fan-out, validated
which ALONE (single build) + drv-assembly off the fan-out to avoid the >2-check ceiling.

which build-pkg result: ok which built + repro
/gnu/store/m78xz1wg0psmm740c5fih8vxg9q5zgv5-which-2.21 ; behavioral legs both PASS
(which v2.21 + locates $CU/bin/ls); distinct from guix's ffihi…-which-2.21. COMMITTED.

### Recipe 2: gperf (3.3) — pure autotools leaf (C++), zero inputs

- recipe: `tests/ts/recipe-gperf.ts` (mirror://gnu/gperf/gperf-3.3.tar.gz, sha256
  1n2ac3cxinbfbq41jdpb7mlz58q3vga6rzbshdaf0fp4lymy11zx).
- lock: `tests/gperf-no-guix.lock` = 15-line seed + gperf-source. Source realized.
- gate: `gperf` in corpus_SPECS; behavioral asserts `gperf --version` (GNU gperf 3.3)
  AND a real hash-table generation (`%%\nfoo\nbar\n%%` -> output contains in_word_set).
- census: 24/322 -> **25/324** corpus-union (gperf is a build-tool only, not in the
  shipped base system, so shipped-system stays 20/3130).
- self-discrimination: `recipe-gperf-perturbed.ts` adds load-bearing
  `configureFlags: ["--disable-dependency-tracking"]`.

VERIFIED-RED (drv-level, 2026-06-19):
  - real gperf       -> /gnu/store/wqzly9mwxf06w0n2ycx2pysckjk4w9mi-gperf-3.3.drv
  - perturbed (+flag)-> /gnu/store/i6ckg2mgmbnji7299gkv2vppmrn4q631-gperf-3.3.drv  [DISTINCT -> green]
  (same identical-copy => same-drv reasoning as which proves the leg non-vacuous.)

gperf build-pkg result: ok gperf built + repro
/gnu/store/w6b9p6jjlmh23qgb897295kx5q4rckv5-gperf-3.3 ; behavioral PASS (GNU gperf 3.3
+ in_word_set hash-gen); distinct from guix's fhs71…-gperf-3.3. COMMITTED.

### Recipe 3 (ATTEMPTED, DROPPED): m4 (1.4.19)

m4 lowers + its drv assembles + its perturbed drv diverges, BUT it does NOT build via
td's plain autotools path:

    td-builder: autotools-build: patch-shebang ./m4-1.4.19/bootstrap: Permission denied (os error 13)

m4's tarball ships a read-only `bootstrap` script; td's `patch_shebangs` phase
(builder/src/build.rs) cannot rewrite a read-only file. Fixing this is td-builder
(Rust) work — out of scope for this recipe-authoring track, and builder/src changes
bust the whole corpus cache. So m4 is DROPPED here (the task: "drop a candidate that
needs work td doesn't own yet"). All m4 files removed; census back to 25/324. A future
builder increment that chmods-before-patch (or skips read-only non-shebang files)
unblocks m4 — noted for the td-builder track.

Final batch: **which + gperf** (2 OWNED pure-autotools leaf recipes), census
23/320 -> 25/324.
