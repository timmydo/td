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

## Next increments (the rest of the frontier)

- nano's DIRECT inputs — **ncurses** and **gettext-minimal** — are heavier: each has
  multiple `#:phases` (file-patching; ncurses also fetches a rollup-patch source) +
  a `doc` output + make-flags. Reconstructing them store-path-equal needs the DSL +
  bridge to express multi-output, `makeFlags`, and (faithfully) phases — a larger
  follow-on. pkg-config establishes the configureFlags + multi-URI rung first.
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
