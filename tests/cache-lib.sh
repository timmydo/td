# tests/cache-lib.sh — the shared content-addressed build-recipe cache wrapper used
# by the package-manager build gates (corpus-no-guix, toolchain-no-guix,
# corpus-deps-no-guix, rust-build). `td-builder build-recipe` assembles a
# DETERMINISTIC .drv, so a PERSISTENT cache (.td-build-cache/<gate>/, gitignored)
# lets td reuse a prior NAR-verified output for an UNCHANGED recipe (build-recipe
# prints CACHE=hit) and skip the build; on a verified hit the reproducibility
# double-build is skipped too (verdict memoized, like check-memo). Only a CHANGED
# recipe (⇒ different drv ⇒ miss) rebuilds. Reproducibility/behavior unweakened: the
# first build still double-builds, every run re-NAR-verifies the cached output, and
# the gate's own behavioral test (run/--version/link) re-runs each time.
#
# Caller presets: TB (td-builder path), CU (coreutils dir for the scrubbed PATH),
# CACHE (the gate's persistent cache root). cached_build/cached_check set/use the
# shell vars: sd (this spec's cache dir), out (store path), ns (newstore output dir),
# hit (non-empty on a cache hit).

# cached_build SPEC LOCK  — emit the recipe, build via build-recipe with caching.
# Sets sd, out, ns, hit. Returns non-zero (with a FAIL message) on a real failure.
cached_build() {
  _spec="$1"; _lock="$2"
  sd="$CACHE/$_spec"; mkdir -p "$sd/b" "$sd/tmp"
  sh tests/ts-emit.sh "tests/ts/recipe-$_spec.ts" > "$sd/recipe.json" \
    || { echo "FAIL: ts-emit $_spec" >&2; return 1; }
  test -s "$sd/recipe.json" || { echo "ERROR: ts-emit produced no JSON for $_spec" >&2; return 1; }
  rm -f "$sd/b/"*.drv                       # drop any stale drv; build-recipe rewrites the current one
  if env -i HOME="$sd" TMPDIR="$sd/tmp" PATH="$CU/bin" "$TB" build-recipe \
       "$sd/recipe.json" "$_lock" "$sd/b" /var/guix/db/db.sqlite > "$sd/bout" 2>"$sd/err"; then :; \
  else echo "FAIL: build-recipe $_spec (guix/Guile off PATH):" >&2; tail -20 "$sd/err" >&2; return 1; fi
  out=`sed -n 's/^OUT=out //p' "$sd/bout"`
  test -n "$out" || { echo "FAIL: build-recipe produced no output for $_spec" >&2; cat "$sd/err" >&2; return 1; }
  ns="$sd/b/newstore/`basename "$out"`"
  if grep -qx 'CACHE=hit' "$sd/bout"; then hit=1; else hit=; fi
}

# cached_check SPEC — prove reproducibility, memoized: skip the td-builder check
# double-build when the build was a cache HIT and a prior check already verified this
# (unchanged) drv. Otherwise run the real double-build and record the verdict.
cached_check() {
  _spec="$1"
  if [ -n "$hit" ] && [ -f "$sd/b/verified-reproducible" ]; then
    echo "  [DURABLE repro] CACHED: $_spec drv unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"
    return 0
  fi
  rm -rf "$sd/chk"
  "$TB" check "$sd/b/"*.drv "$sd/b/closure.txt" "$sd/chk" >/dev/null 2>"$sd/chkerr" \
    || { echo "FAIL: $_spec NOT reproducible (td-builder check):" >&2; tail -6 "$sd/chkerr" >&2; return 1; }
  : > "$sd/b/verified-reproducible"
  echo "  [DURABLE repro] td-builder check double-build agrees $_spec is reproducible"
}

# cached_clean — drop this spec's transient files, KEEP $sd/b (the cache).
cached_clean() {
  rm -rf "$sd/chk" "$sd/tmp" "$sd/t" "$sd/t.c" "$sd/lk" "$sd/bout" "$sd/err" "$sd/chkerr" "$sd/recipe.json"
  mkdir -p "$sd/tmp"
}
