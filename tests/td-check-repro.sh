#!/bin/sh
# tests/td-check-repro.sh — the recipe gates' shared DURABLE reproducibility leg
# (DESIGN §7.1 input-recipes; prime directive 1 on td's OWN terms).
#
# `td-builder check` builds DRV TWICE in two independent user-namespace sandboxes
# and compares the per-output NAR hashes — td's own reproducibility verdict, with
# NO `guix build --check` and no daemon in it. This is the assertion that survives
# Guix's retirement (unlike the byte-identity / NAR-equal "migration oracle" legs),
# so corpus-pkgconfig / corpus-libatomic / corpus-popt / corpus-gzip all share it
# rather than each re-deriving reproducibility from Guix.
#
#   td-check-repro.sh TD_BUILDER DRV INPUTS_FILE SCRATCH
#
# INPUTS_FILE: the drv's direct-input output paths (the build-closure seed — the
#   recipe `*-drv.scm` scripts emit them as `TD_IN=`). This script runs
#   `$TD_GUIX gc -R` over them + DRV to stage the FULL build closure td's sandbox
#   binds in.
# SCRATCH: a writable dir (created fresh, removed on exit).
# TD_GUIX (env): how to invoke guix for the closure walk (e.g.
#   "guix time-machine -C channels.scm --").
#
# Exits non-zero (printing the td-builder check output) if td's two builds disagree
# or the build errors — so a non-reproducible recipe reds the gate, on td's terms.
set -eu

tb="$1"; drv="$2"; infile="$3"; sc="$4"
: "${TD_GUIX:?TD_GUIX must say how to invoke guix}"

chmod -R u+w "$sc" 2>/dev/null || true; rm -rf "$sc"; mkdir -p "$sc"
cleanup() { chmod -R u+w "$sc" 2>/dev/null || true; rm -rf "$sc"; }

# Realize the drv's build inputs first. A fixed-output SOURCE may have been GC'd
# (its output dropped once the package was built), which would make the closure
# walk below fail and starve td's rebuild of the source. Re-realizing the input
# derivations re-fetches it (a permitted offline fixed-output fetch); deps already
# in the store are returned from cache (fast).
$TD_GUIX gc --references "$drv" 2>/dev/null | grep '\.drv$' \
  | xargs -r $TD_GUIX build >/dev/null 2>&1 || true

{ cat "$infile"; echo "$drv"; } | xargs $TD_GUIX gc -R | sort -u > "$sc/paths.txt"
echo "   staged build closure: $(wc -l < "$sc/paths.txt") store items"

if ! "$tb" check "$drv" "$sc/paths.txt" "$sc/c" > "$sc/out.txt" 2>"$sc/err.txt"; then
  echo "FAIL: td-builder check reported NON-reproducible (or errored):" >&2
  cat "$sc/out.txt" "$sc/err.txt" >&2
  cleanup; exit 1
fi
# td-builder check exits 0 only when EVERY output's two builds agree; require at
# least one "reproducible" line and no "NOT reproducible" as a defensive backstop.
if ! grep -q 'reproducible' "$sc/out.txt" || grep -qi 'not reproducible' "$sc/out.txt"; then
  echo "FAIL: td-builder check did not confirm the outputs reproducible:" >&2
  cat "$sc/out.txt" >&2
  cleanup; exit 1
fi
grep '^CHECK ' "$sc/out.txt" | sed 's/^CHECK /   td double-build agrees: /'
cleanup
