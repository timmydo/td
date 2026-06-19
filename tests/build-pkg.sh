# tests/build-pkg.sh — build AND reproducibility-check ONE package recipe into the
# shared content-addressed cache (.td-build-cache/pkg). This is the unit the parallel
# `build-recipes` phase fans out across cores (one process per package) BEFORE the
# check gates assert on the now-cache-warm artifacts. The expensive work lives here:
# `td-builder build-recipe` realizes the .drv once, then `td-builder check` rebuilds it
# TWICE more to prove reproducibility — 3 single-threaded builds per package (the
# builder runs make serially, NIX_BUILD_CORES=1), so `build-recipes` can run ~nproc of
# these at once with no internal oversubscription. The check gates then cache-hit the
# build and memo-skip the repro double-build, so they only run their durable behavioral
# + migration-oracle assertions.
#
# Env in (exported ONCE by the build-recipes target so each package reuses them):
#   TB (td-builder), CACHE (.td-build-cache/pkg), TD_NODE/TD_TSC/TD_TS_EVAL/TD_TSDIR
#   (ts-emit). The seed store paths are realized up front by build-recipes, so this
#   script does not touch guix. Arg 1: the spec (lock at tests/<spec>-no-guix.lock).
#
# Builds via cached_build + cached_check (tests/cache-lib.sh): an unchanged recipe
# cache-hits (reusing td's prior NAR-verified output and the memoized repro verdict);
# only a CHANGED recipe (⇒ different drv ⇒ miss) does the 3 builds. Verbose output goes
# to a per-package log so the parallel fan-out stays readable — only a one-line status
# prints; on failure the log tail is dumped and the script exits non-zero.
set -eu

spec="${1:?usage: build-pkg.sh SPEC}"
: "${TB:?}"; : "${CACHE:?}"
. tests/cache-lib.sh

lock="tests/$spec-no-guix.lock"
test -s "$lock" || { echo "FAIL $spec: no lock $lock" >&2; exit 1; }
CU=`grep -- '-coreutils-' "$lock" | sed 's/^[^ ]* //' | head -1`
test -n "$CU" || { echo "FAIL $spec: no coreutils in $lock for the scrubbed PATH" >&2; exit 1; }

mkdir -p "$CACHE"
log="$CACHE/$spec.buildlog"
if { cached_build "$spec" "$lock" && cached_check "$spec"; } >"$log" 2>&1; then
  if [ -n "$hit" ]; then echo "ok   $spec  CACHE HIT       $out"
  else                   echo "ok   $spec  built + repro   $out"; fi
else
  echo "FAIL $spec — see $log:" >&2; tail -20 "$log" >&2; exit 1
fi
