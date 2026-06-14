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

- Inc.2 — wire the lock-resolution into `td-build` (replace the live Guile
  `specification->package` call in the build path with `td-builder resolve` over
  the pinned lock; differential: same build).
- Inc.3+ — reconstruct individual input recipes (start retiring the resolver
  itself), the corpus-independence endgame, package-by-package.

## Verified-red log

(green committed first per the "commit before red variants" gotcha; restored via
`git checkout`)

- **R1 lock is load-bearing** — perturb `tests/td-build-inputs.lock` (one wrong
  byte in ncurses's path) ⇒ `td-builder resolve` returns the perturbed path ⇒ it
  `!=` Guile's live oracle ⇒ the `resolve` rung reds. Proves td genuinely READS the
  lock (not hardcoded) and the equivalence is real, not vacuous.
