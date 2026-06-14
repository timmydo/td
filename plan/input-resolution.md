# plan/input-resolution.md — retire input resolution (move-off-Guile §5)

Track: **input-resolution** (DESIGN §7.1 move-off-Guile; the §5 "toolchain retired
LAST" frontier — the deepest remaining Guile).
Claim: claude-fable-44df36, 2026-06-14.
Single writer: the claiming agent.

## Goal

`system/td-build.scm` (and `system/td-recipe.scm`) resolve a recipe's inputs to
store paths via Guile's `specification->package` → `package-derivation` → output
path. Fully resolving a name (`ncurses` → its store path) means lowering that
package's ENTIRE derivation graph — i.e. reconstructing its recipe in td (the
corpus-independence endgame). So this is retired LAST and package-by-package; one
increment can only **decouple**, not yet replace the resolver.

## Inc.1 — `resolve`: additive equivalence (DONE 2026-06-14)

The loop-sandbox/td-check pattern: prove td's capability ALONGSIDE Guile's before
any swap (directive 3 — the build is untouched).

- **`td-builder resolve LOCKFILE NAME...`** (Rust, `builder/src/main.rs`): reads a
  pinned lock (`NAME <store-path>` per line, `#` comments) and prints each name's
  path — the lookup `td-build.scm` does via Guile, now with NO Guile. Errors loudly
  on a name the lock does not cover.
- **`tests/td-build-inputs.lock`**: the PINNED resolution for the nano recipe's
  declared inputs (ncurses + gettext-minimal), the ungrafted `out` paths at the
  channel pin. A pinned artifact — regenerate on a channel bump (exclusive landing,
  like DIGESTS): `guix repl -L . tests/resolve-lock.scm ncurses gettext-minimal`.
- **`tests/resolve-lock.scm`**: the RESOLVER + oracle — resolves names exactly as
  `td-build.scm` does (`specification->package` → `package-derivation #:graft? #f`
  → out path). Stays Guile (the §5 toolchain, retired last); generates the lock and
  serves as the rung's live oracle.
- **Rung `resolve`** (HEAVY_RUNGS; heavy only for the warm td-builder compile, no
  VM): td-builder's lock resolution is store-path-EQUAL to Guile's LIVE resolution
  for ncurses + gettext-minimal. `./check.sh resolve` green, ~25s.

Honest scope: this moves the lock CONSUMPTION to Rust and pins resolution as an
artifact; the resolver (computing the lock) stays Guile. It is the lockfile-style
decoupling step — the prerequisite for the SWAP (the build consuming td's resolved
inputs), which is Inc.2.

## Next increments

- Inc.3+ — reconstruct individual input recipes (start retiring the resolver
  itself), the corpus-independence endgame, package-by-package.

## Inc.2 — the SWAP: the build consumes td's resolution (DONE 2026-06-14)

The `td-build` nano build now CONSUMES `td-builder resolve` (over the pinned lock)
for its declared deps instead of Guile's `specification->package`.

- **`system/td-build.scm`** — `td-build-components`/`td-rust-build-derivation` gain
  `#:resolved-dep-paths`. When supplied (by `td-builder resolve`, NOT Guile), the
  recipe's deps enter as td-resolved input-SOURCES (already-realized store paths),
  so **no `specification->package` runs for the deps**. Default (#f) is the old
  path (Guile-resolved input-derivations) — so hello + the existing td-drv-* and
  corpus rungs are untouched. The toolchain stays Guile (retired even later, §5).
- **Representation note (honest):** deps move from input-DERIVATIONS to
  input-SOURCES (td only has the out-path from the lock, not a drv), so the nano
  `.drv` and output PATH differ from the Guile-resolved one — the differential is
  BEHAVIORAL (byte-identical `--version`) at a distinct path, like `td-build-deps`,
  not byte-identical `.drv`. A byte-identical-`.drv` swap would need the lock to
  carry dep DRV paths (`read-derivation-from-file`) — a later refinement.
- **Rung `td-build-resolved`** (HEAVY_RUNGS; `tests/td-build-resolved-drv.scm`):
  (a) SWAP — the nano `.drv`'s input-sources are EXACTLY td-builder's resolved dep
  paths AND ncurses/gettext are NOT input-derivations (Guile did not resolve the
  deps); (b) REPRODUCIBLE (`--check`); (c) BEHAVIORAL (`--version` == corpus nano),
  distinct path. `./check.sh td-build-resolved` green, ~47s.

Spike before the rung confirmed: nano built with deps as input-sources runs and is
`--version` byte-identical to the corpus nano; ncurses/gettext are in
`derivation-sources`, absent from `derivation-inputs`.

## Verified-red log

(green committed first per the "commit before red variants" gotcha; restored via
`git checkout`)

- **R1 lock is load-bearing** (resolve) — perturb `tests/td-build-inputs.lock` (one
  wrong byte in ncurses's path) ⇒ `td-builder resolve` returns the perturbed path ⇒
  it `!=` Guile's live oracle ⇒ the `resolve` rung reds. Proves td genuinely READS
  the lock (not hardcoded) and the equivalence is real, not vacuous.
- **R2 the build consumes td's resolution** (td-build-resolved) — perturb
  `tests/td-build-inputs.lock` (wrong ncurses path) ⇒ `td-builder resolve` feeds the
  bad path as the nano build's input-SOURCE ⇒ the source is not a valid store path ⇒
  the build FAILS (daemon rejects the input-source). Proves the swap is real: the
  build genuinely consumes td's resolution, so a wrong resolution breaks it.
