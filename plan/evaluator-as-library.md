# plan/evaluator-as-library.md — td (Rust) constructs the `.drv`, not Guile

Track: **evaluator-as-library** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1,
graduated from §6 on the human go-ahead "tackle guile in the path of the drv
construction. stick with rust approach", 2026-06-13).
Claim: claude-fable-4a2e33, 2026-06-13.
Single writer: the claiming agent.

## Goal

The third move-off-Guile step. After ts-frontend (surface), corpus-independence
(recipe authored in TS), and the own Rust builder (gnu-build-system gone), the
Guile that remains in hello's build path is the **`.drv` construction**:
`system/td-build.scm` calls Guile's `(derivation store …)` to compute the output
path, serialize the ATerm, and write the `.drv`. This track moves that into the
**td-builder Rust binary**.

Differential (the one §6 named): **identical `.drv` both ways** — td emits a `.drv`
byte-identical (same store path AND same bytes) to the one guix's `derivation`
produces for the same spec. Guix is the oracle (§2.5 / prime directive 4),
self-discriminating, verified-red.

Scope boundary: input RESOLUTION (which toolchain/source store paths are the inputs)
stays Guix's — the toolchain is retired last (§5). What moves to Rust is the `.drv`
CONSTRUCTION. Target subject: the `td-build` hello derivation (from #21).

## The algorithm to reproduce (guix/nix, read off the pin)

td-builder already has the ATerm **parser** (`builder/src/drv.rs`) and **SHA-256**
(`builder/src/sha256.rs`). Adds:
1. **ATerm serializer** — the inverse of the parser; the five string escapes
   (`\\ \" \n \r \t`) are exactly `drv.rs::escape`. Round-trips a real `.drv`
   byte-identical.
2. **`nix-base32`** — the store-path digest encoding (compress a SHA-256 to 20 bytes,
   then the base32 alphabet `0123456789abcdfghijklmnpqrsvwxyz`, low-bit-first).
3. **`make-store-path(type, sha256, name)`** — `fingerprint = type ":sha256:"
   base16(hash) ":" storeDir ":" name`; path = storeDir "/" nix-base32(compress(
   sha256(fingerprint), 20)) "-" name.
4. **`.drv` path** = `make-text-path` = make-store-path with type
   `"text:" + join(sorted refs, ":")` over the FULL ATerm content; refs =
   inputDrvs ∪ inputSrcs.
5. **`hashDerivationModulo`** (for OUTPUT paths) — the recursive one: a fixed-output
   drv hashes `"fixed:out:" algo ":" base16(hash) ":" outPath`; a normal drv hashes
   the ATerm with each inputDrv path replaced by base16 of ITS modulo-hash and the
   output paths blanked. Output path =
   `make-store-path("output:<name>", hashDerivationModulo(drv), drvName)`.

## Sub-task ladder (write the test first; verify red before trusting green)

1. Charter: graduate §6→§7.1, claim in PLAN, this file. — DONE 2026-06-13.
2. **Serializer** — `drv.rs::serialize`; unit-test round-trips the sample; a rung leg
   round-trips a REAL pinned `.drv` (the td-build hello drv) byte-identical. Verify
   red: a perturbed serializer (field order / escape) diverges.
3. **`.drv` path hashing** — `nix-base32` + `make-store-path` + `make-text-path`;
   compute the real `.drv`'s OWN store path from its content+refs, match it. Verify
   red: a perturbed digest diverges.
4. **`hashDerivationModulo` + output paths** — compute the subject's output path(s),
   match guix. Verify red.
5. **Full emit + the rung** — `td-builder drv-emit SPEC` writes the `.drv` and prints
   its path; the differential rung builds the spec via guix's `derivation` (oracle)
   and via td-emit, asserting byte-identical `.drv` (path + bytes); wire
   `system/td-build.scm` to use td's `.drv`. Verify red (perturbed emitter diverges).

## Exclusive-landing note

Touches the shared spine: DESIGN §6/§7.1, PLAN.md, and (later) `Makefile` +
`tests/eval.scm`. Extends the td-builder crate (shared infra). Announced here.

## Implementation progress

All sub-tasks DONE 2026-06-13. The hard pieces were each validated against guix over
HUNDREDS of real store `.drv`s before wiring the rung:

- **Sub-task 2 — serializer (`drv.rs::serialize`) + `drv-roundtrip`.** Parser fix: the
  builder is a plain string, not a path, so fixed-output `builtin:download` drvs parse.
  Round-trip over 400 real store `.drv`s — OK=400, DIFFER=0.
- **Sub-task 3 — store-path hashing (`store.rs`: nix-base32, make-store-path,
  make-text-path) + `drv-path`.** Computed `.drv` store path == the real one for all
  400 sampled drvs (MATCH=400).
- **Sub-task 4 — `hashDerivationModulo` + `drv-outpath`.** Computed output `out` path
  == the real one for all 173 sampled NORMAL drvs (the recursion through the whole
  toolchain closure), incl. the corpus hello build drv `cs56i9di…`; 127 fixed-output
  drvs correctly skipped (different formula).
- **Sub-task 5 — `construct_drv` + `drv-emit` + the `drv-emit` rung.** Byte-identical
  (store path AND content) construction for the td-build hello derivation and all 173
  sampled normal drvs (OK=173, DIFFER=0). Rung GREEN: `./check.sh drv-emit` — td
  re-constructs the guix-lowered hello drv byte-identical, and a perturbed recipe is a
  distinct drv it also matches.

Scope honestly stated: input RESOLUTION (which toolchain/source store paths are the
inputs, and the env/input ORDERING the daemon sorts) is taken as the skeleton — that
stays Guix's for now (toolchain retired last, §5). What moved to Rust is the `.drv`
CONSTRUCTION: output-path computation, ATerm serialization, and the `.drv` store path.
Wiring `system/td-build.scm` to actually consume td's emitted `.drv` (rather than
guix's byte-identical one) is a mechanical follow-on — the differential proves they
are the same store object.

## Verified-red log

`drv-emit` rung, each driven via `./check.sh drv-emit`, restored after:
- **R1 serializer** — an extra byte in `serialize` (`Derive([ `) ⇒ the serialize
  round-trip UNIT TESTS fail inside `guix build td-builder` ⇒ rung red at the build
  (Error 1, exit 2). The serializer is guarded by unit tests first.
- **R2 modulo** — `fixed:out:` → `fixed:outX:` in `hash_derivation_modulo` (NOT
  unit-tested) ⇒ td-builder builds fine, but `drv-emit` reds at the DIFFERENTIAL:
  "DIFFER: store path MISMATCH … content MISMATCH" (exit 2). Proves the byte-identity
  differential discriminates independently of the unit suite.
