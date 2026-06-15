# plan/input-recipes.md — reconstruct individual INPUT recipes (move-off-Guile §5)

Track: **input-recipes** (DESIGN §7.1 move-off-Guile; the §5 "toolchain retired
LAST" frontier — the follow-on named by the now-done **input-resolution** track:
"reconstruct individual input recipes (start retiring the resolver itself), the
corpus-independence endgame, package-by-package").
Claim: claude-fable-2715d4, 2026-06-14.
Single writer: the claiming agent.

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

## Next increments (the rest of the frontier)

- nano's DIRECT inputs — **ncurses** and **gettext-minimal** — are the remaining
  heavy ones: each adds `makeFlags` and multiple `#:phases` (file-patching;
  ncurses also fetches a rollup-patch source) on top of the `doc` output (now
  supported). Reconstructing them store-path-equal needs the DSL + bridge to express
  `makeFlags` and (faithfully) phases — the largest follow-on. configureFlags +
  multi-URI (Inc.1) and multi-output (Inc.2) are the bricks done so far.
- Eventually: regenerate the input lock from td's OWN reconstructed recipes (not
  `specification->package`), package-by-package, toolchain LAST.

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
