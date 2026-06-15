# plan/input-recipes.md — reconstruct individual INPUT recipes (move-off-Guile §5)

Track: **input-recipes** (DESIGN §7.1 move-off-Guile; the §5 "toolchain retired
LAST" frontier — the follow-on named by the now-done **input-resolution** track:
"reconstruct individual input recipes (start retiring the resolver itself), the
corpus-independence endgame, package-by-package").
Claim: claude-fable-2715d4, 2026-06-14.
Single writer: the claiming agent.

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

**Next durable step (not done here):** make the recipe gates assert reproducibility
on td's OWN terms via `td-builder check` (double-build) instead of `guix build
--check`. The `td-check` gate already proves td owns this oracle for the `td-build`
subject (builder = `td-builder`); rolling it onto these gates needs td's executor to
run `gnu-build-system` (guile-builder) drvs — staging the drv's full build closure +
validating guile builds in td's sandbox. That is its own increment; until then the
reproducibility leg is `guix build --check` (the property is intrinsic; the
mechanism is the removable oracle).

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

## Next increments (the rest of the frontier)

- nano's DIRECT inputs — **ncurses** and **gettext-minimal** — still need more:
  - gettext-minimal: `makeFlags` (literal `"VERBOSE=yes"`) + the `doc` output (done)
    + configureFlags (done) + TWO phases — `patch-fixed-paths` (pure-literal
    substitute*, expressible NOW) and `patch-tests` (a `lambda*` w/ `inputs`,
    `with-fluids`, `find-files` with a regexp, `cons`) — the second needs new
    phase-body vocabulary (find-files file args, with-fluids).
  - ncurses: a custom `configure` replacement, a `post-install` phase, and an
    `apply-rollup-patch` phase that FETCHES an extra fixed-output source — needs
    phase-level source inputs + `invoke`.
  Done so far: configureFlags + multi-URI, multi-output, minimal-phase
  (substitute*/which), store-path-baking phase + `tests?`. Remaining phase
  vocabulary: `makeFlags`, find-files, with-fluids, invoke, phase-level fetched
  sources.
- Eventually: regenerate the input lock from td's OWN reconstructed recipes (not
  `specification->package`), package-by-package, toolchain LAST.

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
