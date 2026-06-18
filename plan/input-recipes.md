# plan/input-recipes.md — reconstruct individual INPUT recipes (move-off-Guile §5)

Track: **input-recipes** (DESIGN §7.1 move-off-Guile; the §5 "toolchain retired
LAST" frontier — the follow-on named by the now-done **input-resolution** track:
"reconstruct individual input recipes (start retiring the resolver itself), the
corpus-independence endgame, package-by-package").
Claim: claude-fable-2715d4, 2026-06-14.
Single writer: the claiming agent.

## Step (d): RETIRE system/td-recipe.scm + the byte-identity oracle (PR #68)

Human (2026-06-16): "retire the scm files" → "drop the byte-identity oracle
wholesale." Deleted `system/td-recipe.scm` (the gnu-build-system bridge), the 7
`corpus-*` byte-identity gates, and their 14 `ts-recipe-*-{drv,diff}.scm` (+ removed
the `(system td-recipe)` import from `eval.scm`). The durable own-builder gates
(`td-build`/`-deps`/`-resolved`/`-phases`/`-corpus`/`-gettext`) remain and cover
hello, nano, gzip, popt, libatomic-ops, gettext behaviorally + reproducibly.
pkg-config LOSES coverage (its only gate was byte-identity; the own builder can't
build it — glib C-standard wall) — accepted by the human; excluded from the
guix-dependence census (6 td-built, was 7). Exercises the "removable migration
oracle" clause of CLAUDE.md's durable discipline AHEAD of Guix retirement, per
explicit direction; surviving gates stay all-durable. Follow-ups: own-builder
pkg-config (glib fix), and re-add perturbed-diverges/phases-load-bearing
self-discrimination legs to the td-build-* gates (lost with the corpus diff gates).

## Differential + durable test legs (convention, 2026-06-15)

Human concern (2026-06-15): the recipe gates validate Guix *compatibility*
(byte-identity), so when Guix is retired they'd need rewriting, not deletion.
Resolution — codified in CLAUDE.md "Differential + durable discipline": every
reconstruction gate must carry at least one DURABLE assertion (one that holds with
no Guix oracle), and the Guix byte-identity/NAR legs are the removable "migration
oracle." Applied to the four recipe gates here:

- `corpus-pkgconfig` — DURABLE behavioral: built `pkg-config --version` → `0.29.2`.
- `corpus-libatomic` — DURABLE structural: out ships `lib/libatomic_ops.a` +
  `include/atomic_ops.h`.
- `corpus-popt` — DURABLE structural: out ships `lib/libpopt.so` + `include/popt.h`.
- `corpus-gzip` — DURABLE behavioral: built gzip `compress | decompress` round-trip.
- All four: the drv-equal + NAR-equal legs are now labeled `[MIGRATION ORACLE —
  removable when Guix is retired]`; the self-discrimination legs were already durable.

Verified-red (durable legs): broke `corpus-gzip`'s behavioral expectation (expect a
value the round-trip won't produce) ⇒ the gate reds at the `[DURABLE: behavioral]`
leg ("the built gzip did not round-trip … the artifact does not function", exit 2),
with NO Guix oracle involved — proving the durable leg is a real, non-vacuous check
of the artifact. Restored. (The structural `test -f` legs are non-vacuous by
construction.)

**Durable reproducibility via td-check (DONE for corpus-gzip):** `corpus-gzip`'s
reproducibility leg is now td's OWN double-build — `td-builder check` builds the
recipe `.drv` TWICE in independent userns sandboxes and compares per-output NAR
hashes (no `guix build --check` in that verdict). A spike confirmed td's executor
runs a `gnu-build-system` (guile-builder) recipe drv unchanged (it execs `drv.builder`
generically; the gate stages the build closure via `guix gc -R` over the drv's
direct-input output paths, emitted as `TD_IN=` by tests/ts-recipe-gzip-drv.scm).
`guix build --check` is kept as a MIGRATION-ORACLE cross-check (memoized), mirroring
the `td-check` gate's structure. So gzip is reproducible on td's terms today.

Rolled out to ALL four recipe gates (human direction 2026-06-15: do it everywhere,
no piecemeal). The leg is the shared `tests/td-check-repro.sh` (stages the build
closure via `input-output-paths` → `guix gc -R`, runs `td-builder check`); each
recipe `*-drv.scm` emits the closure seed as `TD_IN=`. libatomic-ops (multi-output)
verified reproducible on BOTH outputs. `guix build --check` stays as a memoized
MIGRATION-ORACLE cross-check in each gate. Cost: each gate adds ~2 un-memoized
sandbox builds (the loop-latency trade the human accepted for uniformity); a future
memoization of the td-check verdict would recover it.

Verified-red (R6, td-check leg): stage an EMPTY build closure ⇒ `td-builder check`
cannot build the drv ⇒ the `[DURABLE: reproducibility]` leg reds ("td-builder check
reported NON-reproducible (or errored)", exit 2). Proves the leg genuinely runs td's
double-build against the staged closure — not a no-op. Restored; gate green.

## Where we are

`input-resolution` (DONE, PRs #44/#45) moved the CONSUMPTION of input resolution
to Rust — `td-builder resolve` reads a pinned lock (`name → store-path`) with no
Guile, and the `td-build` nano build consumes it. But the RESOLVER that computes
the lock is still Guile's `specification->package` (which looks the package
DEFINITION up in `(gnu packages …)` and lowers its whole derivation graph). To
retire the resolver itself, td must RECONSTRUCT a package's recipe from upstream
coordinates — the corpus-independence pattern (TS recipe → `system/td-recipe.scm`
bridge → store-path-equal to the corpus oracle) — applied **package-by-package** to
the inputs, the toolchain LAST.

## Inc.1 — reconstruct pkg-config (a real input), the configureFlags DSL step

`corpus`/`corpus-deps` reconstruct the TOP package (hello/nano). This reconstructs
one of nano's INPUTS — **pkg-config** (ncurses's `native-input`, hence a real
package in nano's transitive build graph) — store-path-equal to the corpus oracle,
so pkg-config's resolution can be backed by td's OWN recipe rather than
`specification->package`. One package off the resolver; the toolchain stays Guile
(§5, retired last).

Why pkg-config first: of nano's input graph it is the smallest package whose corpus
definition is reconstructible with a SMALL DSL extension — single output, no custom
`#:phases` (so the default `gnu-build-system` builder matches), no inputs. It needs
exactly two recipe-DSL firsts, both of which flow through the boa evaluator's
generic `JSON.stringify(recipe)` capture with NO evaluator change:

  1. **`configureFlags`** — pkg-config sets `#:configure-flags '("--with-internal-glib")`.
     `gnu-build-system` reads `#:configure-flags` as a G-EXPRESSION wrapping a quoted
     list (`#~'( … )`) that is spliced verbatim into the build expression, so the
     bridge reconstructs exactly `#~(quote #$flags)` to converge.
  2. **multi-URI source** — pkg-config's upstream is a list of mirror URLs; the source
     derivation (hence the whole package derivation) is byte-identical only when the
     URI shape matches, so `fetchSource` + the bridge carry a URI LIST.

Build-free spike (host guix == pin 520785e) confirmed byte-identity BEFORE coding:
a reconstructed pkg-config with the URI list + `#:configure-flags #~(quote #$flags)`
lowers to the corpus oracle drv `dgzxhfbbj4lc5kfd8wz8jq2ng1j7q05z-pkg-config-0.29.2.drv`;
the only diff from a naive reconstruction was the builder's `(quote …)` wrapper.

### Pieces

- `tests/ts/td-spec.d.ts` — `Source.uri: string | readonly string[]`,
  `fetchSource(uri: string | readonly string[], …)`, `Recipe.configureFlags?`.
- `tests/ts/recipe-pkg-config.ts` — the recipe; `recipe-pkg-config-perturbed.ts` —
  one changed configure flag (the differential's discriminator).
- `system/td-recipe.scm` — bridge: a declared URI list passes through as a list;
  declared `configureFlags` become `#:configure-flags #~(quote #$flags)`. Omitted/
  empty ⇒ default arguments, so hello/nano lower byte-identically (the
  `corpus`/`corpus-deps` oracles are untouched — directive 3).
- `tests/ts-recipe-pkgconfig-diff.scm` — self-discriminating differential.
- `tests/ts-recipe-pkgconfig-drv.scm` — lowers TD_DRV / ORACLE_DRV / ORACLE_OUT.
- `mk/gates/305-corpus-pkgconfig.mk` — the `corpus-pkgconfig` heavy gate.

### Gate `corpus-pkgconfig` proves

  (a) CONVERGE — pkg-config (recipe-pkg-config.ts) lowers to the corpus oracle drv;
  (b) DISCRIMINATE-flags — a perturbed configure flag diverges (not vacuous);
  (c) flags LOAD-BEARING — stripping `configureFlags` diverges;
  (d) multi-URI LOAD-BEARING — collapsing the URI list to one URL diverges;
  (e) BUILD + `--check` (prime directive 1, verdict-memoized) — the built object is
      path-identical AND NAR-hash-equal to the corpus oracle's.

## Inc.2 — multi-output recipes (PR #?? — corpus-libatomic)

The next recipe-DSL brick: an `outputs` field. Many corpus packages split off a
`debug`/`static`/`doc` output, and an extra output enters the build derivation —
nano's DIRECT inputs ncurses + gettext-minimal BOTH carry a `doc` output, so
multi-output is a prerequisite for reconstructing them. Demonstrated byte-identical
on **libatomic-ops** — the cleanest multi-output package: it sets NO configure-flags
and NO custom phases, so the extra output (`debug`) is the ONLY thing beyond a leaf
recipe (the capability is isolated for a clean verified-red). Not in nano's direct
graph, but the capability it adds is exactly what ncurses/gettext need.

Build-free spike (host guix == pin) confirmed byte-identity first: reconstructing
libatomic-ops with `(outputs '("out" "debug"))` lowers to the corpus oracle drv
`h11sba49rynr607zml6vls57dpafjwbv-libatomic-ops-7.8.2.drv`; a single `("out")`
diverges.

### Pieces

- `tests/ts/td-spec.d.ts` — `Recipe.outputs?: readonly string[]`.
- `tests/ts/recipe-libatomic-ops.ts` — the recipe (`outputs: ["out","debug"]`);
  `recipe-libatomic-ops-perturbed.ts` — one wrong source-hash byte (the
  differential's non-vacuity discriminator).
- `system/td-recipe.scm` — `recipe-outputs`: declared outputs (vector→list) become
  the package's `(outputs …)`; omitted ⇒ `("out")`, byte-identical to specifying
  none, so hello/nano/pkg-config lower unchanged (verified — those oracles stay
  green).
- `tests/ts-recipe-libatomic-diff.scm` — self-discriminating differential;
  `tests/ts-recipe-libatomic-drv.scm` — TD_DRV / ORACLE_DRV / TD_OUT / ORACLE_OUT.
- `mk/gates/310-corpus-libatomic.mk` — the `corpus-libatomic` heavy gate.

### Gate `corpus-libatomic` proves

  (a) CONVERGE — libatomic-ops (out + debug) lowers to the corpus oracle drv;
  (b) DISCRIMINATE-src — a perturbed source diverges (not vacuous);
  (c) outputs LOAD-BEARING — stripping `outputs` (→ single `out`) diverges;
  (d) OUTPUT-SET — the lowered derivation declares BOTH outputs (out + debug);
  (e) BUILD + `--check` (verdict-memoized) — the built `out` object is path-identical
      AND NAR-hash-equal to the corpus oracle's.

## Inc.3 — custom build phases (PR #?? — corpus-popt)

The phase frontier: a `phases` field. nano's own inputs patch source files in
custom `#:phases`, so phases are required to reach them. Key feasibility finding:
a phases gexp built PROGRAMMATICALLY from structured data is byte-identical to the
`(modify-phases …)` form the corpus package writes by hand (spike confirmed popt's
drv `h1n1ndlihs7j2p4kvy0wxq142rmb4v0r` before coding). So phases are DATA in the TS
surface; `gnu-build-system`/`(guix build utils)` (substitute*/which/modify-phases)
stay the build-time toolchain (retired LAST), only the phase DATA comes from TS.

Demonstrated byte-identical on **popt** — the cleanest phase package: its ONLY
non-default argument is one `patch-test` phase (two `substitute*` source patches,
one literal + one `(which "echo")`, trailing `#t`), nothing else. The minimal
phase vocabulary this rung lands: `add-{before,after}` an anchor, a `(lambda _ …)`
body of `substitute*` ops, replacement either a literal string or `{which: PROG}`,
optional trailing `#t`.

### Pieces

- `tests/ts/td-spec.d.ts` — `Phase`/`Substitution`/`Replacement` types +
  `Recipe.phases?`.
- `tests/ts/recipe-popt.ts` — the recipe; `recipe-popt-perturbed.ts` — one wrong
  source-hash byte (the differential's non-vacuity discriminator).
- `system/td-recipe.scm` — `recipe-phases`/`phase->gexp`/`substitution->gexp`:
  lower the phase DATA to the byte-identical `(modify-phases …)` gexp; omitted ⇒ no
  `#:phases`, so recipes without phases lower unchanged (existing oracles stay green).
- `tests/ts-recipe-popt-diff.scm` — self-discriminating differential;
  `tests/ts-recipe-popt-drv.scm` — TD_DRV / ORACLE_DRV / ORACLE_OUT.
- `mk/gates/315-corpus-popt.mk` — the `corpus-popt` heavy gate.

### Gate `corpus-popt` proves

  (a) CONVERGE — popt (with the phase) lowers to the corpus oracle drv;
  (b) DISCRIMINATE-src — a perturbed source diverges (not vacuous);
  (c) phases LOAD-BEARING — stripping `phases` diverges;
  (d) BUILD + `--check` (verdict-memoized) — the built object is path-identical AND
      NAR-hash-equal to the corpus popt
      (`13kvphyxjy7mz3i7lrzyqixi16sa3rc057mbl97kjncf9jm8lx54`).

## Inc.4 — phases that bake a build store path; `tests?` (PR #?? — corpus-gzip)

The next phase-vocabulary bricks: a substitution replacement can be a
`string-append` of literal strings + build store paths (`{output: NAME}` →
`(assoc-ref outputs NAME)`, `{input: NAME}` → `(assoc-ref inputs NAME)`), lowered
through a `(lambda* (#:key outputs/inputs …) …)`; and a `tests` field
(`#:tests? #f`). This is the idiom nano's DIRECT inputs use to inject store paths
in their phases. Demonstrated byte-identical on **gzip** — its
`use-absolute-name-of-gzip` phase rewrites `exec 'gzip'` to
`exec <out>/bin/gzip` and it builds with `#:tests? #f`. Build-free spikes confirmed
first: arg ORDER is irrelevant (gnu-build-system normalizes it) and the
bridge-generated `string-append`/`lambda*` gexp lowers to the oracle drv
`6pajp3gyq2sr4s6j12zw36qnbk8l023q`.

### Pieces

- `tests/ts/td-spec.d.ts` — `RefPart`, `Replacement.stringAppend`,
  `Phase.lambdaArgs?`, `Recipe.tests?`.
- `tests/ts/recipe-gzip.ts` (+ `-perturbed.ts` — wrong source-hash byte).
- `system/td-recipe.scm` — `ref-part->gexp` + `subst-replacement->gexp`
  (stringAppend), `phase-lambda` (`lambda*` formals), `recipe-arguments` (`#:tests?`).
- `tests/ts-recipe-gzip-{diff,drv}.scm`; `mk/gates/320-corpus-gzip.mk`.

### Gate `corpus-gzip` proves

  (a) CONVERGE — gzip (path-ref phase + `#:tests? #f`) lowers to the corpus oracle drv;
  (b) DISCRIMINATE-src — a perturbed source diverges;
  (c) phase LOAD-BEARING — stripping `phases` diverges;
  (d) BUILD + `--check` — path-identical AND NAR-hash-equal to the corpus gzip
      (`0qhr884lpk7yl67ckyjmx89g0wn10mh5331plz9z4hpgq7wf5dls`).

## Inc.5 — reconstruct gettext-minimal: the full phase-body DSL (PR #?? — corpus-gettext)

**nano's first direct input reconstructed.** gettext-minimal needs everything: a
`doc` output (done), configureFlags (done), `makeFlags` (NEW — literal, same gexp
shape as configureFlags), build inputs (libunistring/libxml2/ncurses, resolved by
the bridge), and TWO custom phases — `patch-fixed-paths` (literal substitute* over
file LISTS) and `patch-tests` (the full vocabulary). Build-free spike confirmed
byte-identity FIRST (oracle `q6s49zzqb2vcs49sj6n59j25w7209nwx`), then the bridge
generated it programmatically from JSON, byte-identical.

New bridge/DSL — a recursive phase-body AST (`Phase.body`):
- FileArg: `{list}` (quoted file list), `{findFiles: [dir, regex]}` →
  `(find-files …)`, `{cons: [a, b]}` → `(cons …)`.
- Clause: optional `match` vars `((from var…) to)` so `to` can reference a submatch.
- Replacement: `{var}` (bare bound symbol), `{format: [fmt, part…]}` →
  `(format #f …)`, plus the existing string/which/stringAppend.
- Stmt: `{substitute, clauses}`, `{letWhich: [{name,prog}], body}` →
  `(let* ((name (which prog))) …)`, `{withDefaultPortEncodingFalse, body}` →
  `(with-fluids ((%default-port-encoding #f)) …)`.
- `Recipe.makeFlags` → `#:make-flags #~(quote …)`.

### Pieces
- `system/td-recipe.scm` — `filearg->gexp`/`clause->gexp`/`stmt->gexp` + `body`
  wiring in `phase->gexp`; `{var}`/`{format}` in the replacement; `makeFlags`.
- `tests/ts/td-spec.d.ts` — `FileArg`/`Clause`/`Stmt`/recursive `body`, `RefPart`
  +`{var}`, `Replacement` +`{var}`/`{format}`, `Recipe.makeFlags`.
- `tests/ts/recipe-gettext-minimal.ts` (+ `-perturbed.ts`).
- `tests/ts-recipe-gettext-{diff,drv}.scm`; `mk/gates/325-corpus-gettext.mk`.
- `tests/td-check-repro.sh` — re-realize the drv's build inputs before staging (a
  GC'd fixed-output SOURCE is re-fetched — permitted offline; needed because
  gettext's source had been dropped after its output was built).

### Gate `corpus-gettext` proves
  (a) CONVERGE — gettext-minimal lowers to the corpus oracle drv;
  (b) DISCRIMINATE-src — a perturbed source diverges;
  (c) phases LOAD-BEARING — stripping `phases` diverges;
  (d) DURABLE behavioral — the built `msgfmt --version` runs (0.23.1);
  (e) DURABLE reproducibility — td-builder check double-build (no Guix);
  (f) MIGRATION ORACLE — byte-identical out (path + NAR) + guix build --check agrees.

## Next increments (the rest of the frontier)

- nano's OTHER direct input, **ncurses**, remains — the hardest: a custom
  `configure` REPLACEMENT phase, a `post-install` phase, and an `apply-rollup-patch`
  phase that FETCHES an extra fixed-output source (invisible-mirror.net) + `invoke`.
  Needs new phase-body vocabulary: `replace`-a-phase, phase-level fetched sources,
  `invoke`, `patch-makefile-SHELL`/`for-each`. Its own (large) increment.
- Then: nano's two direct inputs are both off `specification->package` →
  regenerate the input lock (Inc.1/2) from td's OWN reconstructed recipes,
  package-by-package, toolchain LAST.
- Phase vocabulary DONE: configureFlags + multi-URI, multi-output, makeFlags,
  tests?, and the phase-body AST (substitute*/which/stringAppend/format, file
  lists, find-files, cons, match vars, let-which, with-fluids).

## Verified-red log (Inc.4, store-path-baking phase + tests? — corpus-gzip)

- **R5 store-path phase load-bearing** — make `recipe-phases` return `#f` always
  (ignore gzip's phase). The candidate gzip drv then lacks the
  `use-absolute-name-of-gzip` phase ⇒ diverges from the corpus oracle
  `6pajp3gyq2sr4s6j12zw36qnbk8l023q…`; the differential reds at leg (a) CONVERGE
  (exit 1). Proves the generated `string-append`/`assoc-ref outputs`/`lambda*` gexp
  is exactly what makes gzip converge. Restored; gate green (`./check.sh
  corpus-gzip`, NAR-hash-equal `0qhr884lpk7yl67ckyjmx89g0wn10mh5331plz9z4hpgq7wf5dls`).

## Verified-red log (Inc.3, phases — corpus-popt)

- **R4 phases load-bearing** — make `recipe-phases` return `#f` always (ignore the
  declared phase). The candidate popt drv then lacks `#:phases` ⇒ diverges from the
  corpus oracle `h1n1ndlihs7j2p4kvy0wxq142rmb4v0r…`; the differential reds at leg (a)
  CONVERGE (exit 1, "does NOT reproduce the corpus oracle's derivation"). Proves the
  phase DATA is load-bearing and the generated modify-phases gexp is exactly what
  makes popt converge. Restored; gate green (`./check.sh corpus-popt`,
  NAR-hash-equal `13kvphyxjy7mz3i7lrzyqixi16sa3rc057mbl97kjncf9jm8lx54`).

## Verified-red log

(green committed first — commit `0b1189b` — per the "commit before red variants"
gotcha; each red was a one-line bridge edit, then `git checkout system/td-recipe.scm`.)

- **R1 configureFlags load-bearing** — make `recipe-arguments` DROP the declared
  flags (return `'()` always). The candidate pkg-config drv falls back to the
  no-flags drv `1825487dg29vxghjzs9m0z9r39hlckn3-pkg-config-0.29.2.drv` ≠ the corpus
  oracle `dgzxhfbbj4lc5kfd8wz8jq2ng1j7q05z…`; the `corpus-pkgconfig` gate reds at leg
  (a) CONVERGE (`./check.sh corpus-pkgconfig` exit 2; differential exit 1, "does NOT
  reproduce the corpus oracle's derivation"). Proves the convergence is real and the
  `#~(quote #$flags)` reconstruction is exactly what makes pkg-config converge — not
  vacuous.
- **R2 multi-URI load-bearing** — make `recipe-uri` collapse the declared URL LIST to
  its first element (`vector-ref u 0`). The source derivation changes, so the
  candidate drv becomes `1an130fvw33dvmmaw7b2jilrh9q6y0bk-pkg-config-0.29.2.drv` ≠ the
  oracle; the differential reds at leg (a) CONVERGE (exit 1). Proves the bridge
  genuinely honours the declared mirror-list shape (a single URL would diverge).

Both restored; tree clean; gate green again (`./check.sh corpus-pkgconfig`,
NAR-hash-equal to the corpus oracle `127q8jdmd6afiz866ab3wga46dlw65n6r76cm28gikwid544f6g0`).
The in-gate discriminator legs (perturbed flag, flags-stripped, single-URI) keep this
self-discriminating every loop.

### Inc.2 (multi-output, corpus-libatomic)

- **R3 outputs load-bearing** — make `recipe-outputs` IGNORE the declared outputs
  (return `'("out")` always). The candidate libatomic-ops drv falls back to the
  single-output drv `8r1vzpyyq5wh95536c8yw7dafjr8kkjp-libatomic-ops-7.8.2.drv` ≠ the
  corpus oracle `h11sba49rynr607zml6vls57dpafjwbv…`; the differential reds at leg (a)
  CONVERGE (exit 1, "does NOT reproduce the corpus oracle's derivation"). Proves the
  `outputs` field is load-bearing — declaring the `debug` output is exactly what makes
  libatomic-ops converge.

Restored; tree clean; gate green (`./check.sh corpus-libatomic`, NAR-hash-equal on the
out output `1xz2xsb7ay7cpxdl2qdxv1d2m6mxhmx2nbn992bgp9dqwxyv4v74`). The in-gate legs
(perturbed source, outputs-stripped, output-set) keep it self-discriminating.

## Verified-red log (Inc.5, full phase-body DSL — corpus-gettext)

- **R7 phase-body constructs load-bearing** — break one construct in the bridge
  (`filearg->gexp` emits the bare `dir` string instead of `(find-files dir regex)`)
  ⇒ gettext-minimal's generated phase-body gexp differs ⇒ it DIVERGES from the
  corpus oracle (the `corpus-gettext` differential reds at leg (a) CONVERGE: "does
  NOT reproduce the corpus oracle's derivation"). Proves the phase-body constructs
  (find-files / with-fluids / match vars / let-which / cons / format) are exactly
  what make gettext converge, not decorative. Restored; gate green. (The td-check
  leg is covered by R6 — the shared `td-check-repro.sh` helper.)

## Step (a) toward retiring td-recipe.scm: td's builder runs recipe phases in Rust (PR #?? — td-build-phases)

The endgame (`how do we get rid of td-recipe.scm and use our own tooling for .drv`)
needs td's OWN builder to run a recipe's custom phases, not gnu-build-system's
Guile. This lands that: `td-builder autotools-build` gains a phase interpreter
(`builder/src/json.rs` — a zero-dep JSON reader; `build.rs` — applies the recipe's
`substitute*` phases after unpack via the toolchain's `sed`/`find`, descending
`letWhich`/`with-fluids`, resolving `{var}`/`{output}`/`{which}`/`{format}`/match-vars,
file lists/`find-files`/`cons`). No regex crate — the toolchain's sed/find do the
matching (the toolchain is retired LAST anyway). `system/td-build.scm` passes the
recipe's `phases` as JSON via `TD_PHASES`; both the flat `substitutions` form
(gzip/popt) and the nested `body` form (gettext) are handled.

Demonstrated by `td-build-phases`: gzip is built by td's OWN builder applying its
`use-absolute-name-of-gzip` phase in Rust — STRUCTURAL (builder = `td-builder`, not
`guile`), DURABLE behavioral (the installed gunzip execs the absolute
`<out>/bin/gzip`, i.e. td's runner applied the phase; gzip round-trips), DURABLE
reproducibility (`td-builder check` double-build), at a DISTINCT path from the
corpus gzip. zero new crate deps (cargo-test gate stays green); 3 new json unit tests.

This is the first of the steps to retire td-recipe.scm: the OWN-builder path can now
run phases. Remaining: route the corpus recipes through this path (dropping
byte-identity for behavioral+reproducible, per "own then diverge"), then input
resolution off Guile, toolchain LAST.

Verified-red (R8, td's phase runner): make `apply_phases` a no-op (skip applying
the recipe's phases) ⇒ td's builder no longer patches gunzip.in ⇒ the installed
gunzip still execs `'gzip'` (not `<out>/bin/gzip`) ⇒ the `td-build-phases` gate reds
at the `[DURABLE: behavioral]` leg ("the installed gunzip does not exec …/bin/gzip —
td's phase runner did not apply the phase", exit 2). Proves the behavioral leg
genuinely verifies td's OWN runner applied the phase — not a no-op, no Guix involved.
Restored; gate green.

## Step (b) toward retiring td-recipe.scm: route the corpus recipes through td's builder (PR #66 — td-build-corpus)

With td's builder running phases (step a), the reconstructed recipes can be built by
td's OWN builder instead of gnu-build-system / td-recipe. This routes them:
`td-build-corpus` builds **popt** (a substitute*/which phase) and **libatomic-ops**
(a multi-output recipe → td builds a single `out`) through `system/td-build` —
builder = `td-builder`, configure-flags + phases applied in Rust — each STRUCTURAL
(builder = td-builder), DURABLE behavioral (ships its lib/header), DURABLE
reproducibility (`td-builder check`), at a DISTINCT path from the corpus build (own
builder → own path, per "own then diverge"). gzip is the sibling `td-build-phases`
gate. So gzip + popt + libatomic-ops now build with NO gnu-build-system / Guile —
td-recipe.scm is replaceable for them.

Also fixed: `system/td-build.scm` now converts a mirror-LIST source URI (vector →
list) like td-recipe.scm, so multi-URI recipes lower through the own-builder path
(pkg-config now LOWERS via td-build).

Verified-red (R9, STRUCTURAL leg — the gate's headline claim): point
`td-build-components`'s builder at `…/bin/td-builderXX` (NOT td-builder) ⇒ the
derivation's `(basename (derivation-builder drv))` is no longer `td-builder` ⇒ the
`[STRUCTURAL]` leg reds ("FAIL: popt builder is 'td-builderXX', expected
td-builder", exit 2) BEFORE any build. Proves the structural assertion genuinely
verifies td's OWN Rust binary is the derivation's builder — i.e. that "no
gnu-build-system / no Guile in the build" is a checked fact, not a vacuous label.
(The phase-runner + behavioral build pipeline is R8; the td-check repro leg is R6 —
both shared with the sibling gates.) Restored; gate green.

Deferred (gnu-build-system fidelity, the remaining bulk):
- **pkg-config** — `--with-internal-glib` compiles a bundled glib that hits a
  C-standard error under td's build env (`goption.c: expected identifier before
  'bool'`) that gnu-build-system's env avoids; needs CFLAGS/standard-phase fidelity.
- Multi-OUTPUT splitting (debug/doc/static) — td builds a single `out`; faithful
  multi-output (strip→debug, doc separation) is later work.
- Input resolution + toolchain — retired LAST (§5).

## Step (c) toward retiring td-recipe.scm: gettext-minimal through td's builder (PR #67 — td-build-gettext)

Routed nano's MOST elaborate input — gettext-minimal (build inputs
libunistring/libxml2/ncurses, configure flags, a makeFlag, and TWO custom phases
exercising the FULL phase-body vocabulary: findFiles, cons, letWhich, withFluids,
format, stringAppend) — through `system/td-build` (builder = td-builder, phases in
Rust, no gnu-build-system). New gate `td-build-gettext` (350): STRUCTURAL (builder =
td-builder) + DURABLE behavioral (msgfmt + xgettext `--version` report 0.23.1) +
DURABLE reproducibility (`td-builder check`) + INDEPENDENCE (distinct path, single
`out` where the corpus splits `doc`). No Guix byte-identity leg — all durable.

Turned out the blocker was NOT a missing standard phase (the deferred note guessed
`patch-source-shebangs`): td runs `bash ./configure` explicitly and passes
`SHELL=bash` to make, and the recipe's own phases handle the relevant `/bin/sh`
references, so configure/make/install completed unaided. The real gap was a BUG in
td's `find_files` (builder/src/build.rs): its `find … | while …; do … grep -qE && …; done`
helper, under `set -e`, left the pipeline at grep's exit 1 whenever the LAST file in
the tree did NOT match the regex — a spurious "find-files failed". gettext's
`(find-files "gettext-tools/tests" "^(lang-sh|msg…)")`, where most files don't match,
hit it on the first real own-builder run (gzip/popt/libatomic never use findFiles, so
it stayed latent). Fix: `if … then … fi` body (a non-match no longer fails the loop)
+ `set -eo pipefail` (a genuine `find` failure stays fatal).

Verified-red (R10, find_files load-bearing): with the original `grep -qE && printf`
body, building gettext-minimal via td's own builder reds in-phase — "td-builder:
autotools-build: find-files in …/gettext-tools/tests failed", build exit 1 — BEFORE
configure. Proves the find_files fix is load-bearing for gettext's build (and thus
the gate's durable behavioral leg). A hermetic Rust unit test is impractical
(find_files shells out to find/grep, absent in the cargo-test toolchain), so the
gettext own-builder build IS the regression guard. Fixed; build completes + tools run.

## Lever 4 batch 2: 6 more toolchain leaves via td's own builder (branch lever4-toolchain-batch2)

Reconstructed xz, diffutils, patch, file, coreutils, gawk as td recipes built via
`td-builder build-recipe`; toolchain-no-guix (222) now covers 9 leaves
(make/sed/grep + these 6). Census re-baselined: owned 9→15, shipped-system
td-reproducible 5→11 (0.36%→0.78%). Two builder capabilities this needed:

- configureFlags now travel as a JSON array (each element = one ./configure arg),
  so a flag with internal whitespace survives the drv round-trip (same encoding as
  TD_PHASES). gawk needs `CFLAGS=-O2 -g -Wno-incompatible-pointer-types` for the
  seed's gcc-15.
- patch-source-shebangs (in Rust), mtime-PRESERVING: rewrite `#!/bin/sh` build
  scripts to the seed bash (the #67 notes guessed this; gawk's build-aux/install-sh,
  run directly by its install rule, is the case that finally needs it). Preserving
  mtime is load-bearing — see R-red below.

Verified-red (all observed before the green):
- gawk, no CFLAGS: io.c `(ssize_t(*)())read` cast → gcc-15 -Wincompatible-pointer-types
  hard error, build exit 1. Proves the configureFlags-whitespace fix is load-bearing.
- gawk, with CFLAGS but no patch-shebangs: build-aux/install-sh `#!/bin/sh` → "required
  file not found", install exit 127. Proves patch-shebangs is load-bearing.
- coreutils, naive patch-shebangs (mtime bumped to now): aclocal.m4 seen stale →
  maintainer-mode rebuild → `aclocal-1.16: command not found`, make exit 127
  (fullcheck run 1). Proves the mtime-PRESERVATION is load-bearing. Plus a Rust unit
  test (build::tests) asserting the rewrite touches only abs sh/bash outside the
  store, keeps the exec bit + trailing args, and preserves mtime.
