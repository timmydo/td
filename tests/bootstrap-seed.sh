#!/bin/sh
# tests/bootstrap-seed.sh — source-bootstrap BRICK 0: the irreducible, auditable, guix-free
# bottom of td's /td/store toolchain. td takes a tiny hand-auditable seed (stage0-posix's
# 229-byte hex0-seed + 618-byte kaem-optional-seed, vendored in seed/stage0/, NOT guix-built)
# and runs the seed kaem build with guix/Guile OFF PATH, producing the first stage0 artifacts
# (a full hex0 + kaem-0). No guix process, no /gnu/store in the build.
#
# Legs (all DURABLE — no guix oracle; the seed IS the bottom):
#   [DURABLE no-guix]          the vendored seeds match their pinned sha256 — auditable, NOT
#                              guix-built; the build runs with guix/Guile scrubbed from env.
#   [DURABLE self-reproduction] assembling each seed's OWN hex source (with the seed) yields a
#                              byte-identical seed — the binary seeds are verifiable from source.
#   [DURABLE behavioral]       the seed-built hex0 WORKS as an assembler: it assembles kaem-0,
#                              which matches its pin (the produced tool does its job).
#   [DURABLE repro]            two independent runs produce byte-identical artifacts.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

SEED=seed/stage0
HEX0_PIN=66c95985e668f20f2465c2b876f83fef066fd7c8c2dd3adb51a969f2d7120c8b
KAEM_PIN=153b8915b73bd07132b59538d10fe53d26578eb160a67db72af07aaa61c51b3b
sha() { sha256sum "$1" | cut -d' ' -f1; }

# --- [DURABLE no-guix] the vendored seeds are the pinned auditable bytes, not guix-built ----
test "`sha $SEED/bootstrap-seeds/POSIX/AMD64/hex0-seed`" = "$HEX0_PIN" \
  || fail "vendored hex0-seed sha256 != pin (seed drifted?)"
test "`sha $SEED/bootstrap-seeds/POSIX/AMD64/kaem-optional-seed`" = "$KAEM_PIN" \
  || fail "vendored kaem-optional-seed sha256 != pin"
if grep -q -a '/gnu/store' "$SEED/bootstrap-seeds/POSIX/AMD64/hex0-seed"; then
  fail "the seed contains /gnu/store bytes — not a clean non-guix seed"
fi
echo "   [DURABLE no-guix] vendored hex0-seed (229B) + kaem-optional-seed (618B) match their pins — auditable, NOT guix-built, no /gnu/store bytes"

# Run the seed kaem build in a fresh scratch with guix/Guile SCRUBBED from env (env -i: the
# static seeds need no PATH — they exec their inputs by relative path — so a green build proves
# NO guix process is involved). Returns the run dir.
run_seed_build() {
  out=`mktemp -d`
  cp -a "$SEED/." "$out/"
  chmod +x "$out/bootstrap-seeds/POSIX/AMD64/hex0-seed" "$out/bootstrap-seeds/POSIX/AMD64/kaem-optional-seed"
  mkdir -p "$out/AMD64/artifact"
  ( cd "$out" && env -i ./bootstrap-seeds/POSIX/AMD64/kaem-optional-seed ./AMD64/mescc-tools-seed-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "seed kaem build failed in $out" >&2; return 1; }
  echo "$out"
}

r1=`run_seed_build` || fail "the seed kaem build did not run (guix/Guile off env)"
trap 'rm -rf "$r1" "${r2:-}"' EXIT INT TERM
test -s "$r1/AMD64/artifact/hex0" -a -s "$r1/AMD64/artifact/kaem-0" \
  || fail "the seed build produced no hex0 / kaem-0 artifact"
echo "   built artifact/hex0 + artifact/kaem-0 with guix/Guile scrubbed from env (kaem-driven, the port)"

# --- [DURABLE self-reproduction] the seeds equal what their own sources assemble to ---------
test "`sha "$r1/AMD64/artifact/hex0"`" = "$HEX0_PIN" \
  || fail "seed-built hex0 != hex0-seed — the hex0_AMD64.hex0 source does not assemble to the seed"
test "`sha "$r1/AMD64/artifact/kaem-0"`" = "$KAEM_PIN" \
  || fail "seed-built kaem-0 != kaem-optional-seed"
echo "   [DURABLE self-reproduction] the seed assembles its OWN source to a byte-identical seed (hex0 + kaem) — the binary seeds are verifiable from the auditable hex source"

# --- [DURABLE behavioral] the seed-built hex0 actually works as an assembler -----------------
# (Producing kaem-0 == its pin already exercises hex0 end-to-end; re-assert it standalone.)
"$r1/AMD64/artifact/hex0" "$r1/AMD64/kaem-minimal.hex0" "$r1/AMD64/artifact/kaem-0b" 2>/dev/null \
  || fail "the seed-built hex0 could not run as an assembler"
test "`sha "$r1/AMD64/artifact/kaem-0b"`" = "$KAEM_PIN" \
  || fail "the seed-built hex0 assembled a wrong kaem-0"
echo "   [DURABLE behavioral] the seed-built hex0 runs as an assembler and reproduces kaem-0 — it works"

# --- [DURABLE repro] a second independent run is byte-identical ------------------------------
r2=`run_seed_build` || fail "the second seed build did not run"
test -s "$r2/AMD64/artifact/hex0" -a -s "$r2/AMD64/artifact/kaem-0" \
  || fail "the second seed build produced no/empty artifacts (hex0=`stat -c%s "$r2/AMD64/artifact/hex0" 2>/dev/null` kaem-0=`stat -c%s "$r2/AMD64/artifact/kaem-0" 2>/dev/null`)"
# Compare by sha256 (cmp/diffutils is absent from the loop sandbox).
test "`sha "$r1/AMD64/artifact/hex0"`" = "`sha "$r2/AMD64/artifact/hex0"`" \
  && test "`sha "$r1/AMD64/artifact/kaem-0"`" = "`sha "$r2/AMD64/artifact/kaem-0"`" \
  || fail "the seed build is NOT reproducible — r1 hex0=`sha "$r1/AMD64/artifact/hex0"` kaem-0=`sha "$r1/AMD64/artifact/kaem-0"` | r2 hex0=`sha "$r2/AMD64/artifact/hex0"` kaem-0=`sha "$r2/AMD64/artifact/kaem-0"`"
echo "   [DURABLE repro] two independent seed builds are byte-identical (reproducible)"

echo "PASS: source-bootstrap brick 0 — td's 229-byte auditable hex0-seed (NOT guix-built) drives"
echo "      the kaem seed build with guix/Guile off env, producing a full hex0 + kaem-0; the"
echo "      seeds self-reproduce from their hex source (verifiable, not blind trust), the built"
echo "      hex0 works as an assembler, and the build is reproducible. The irreducible, guix-free"
echo "      bottom of the /td/store toolchain is in place."
