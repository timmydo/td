#!/usr/bin/env bash
# Shared helpers for recipe-owned checks. The package-specific assertion stays
# in recipes/src/recipes/<stem>.rs; this file keeps the old build/cache plumbing
# in one place while the per-package gate files go away.

set -euo pipefail

recipe_checks_prelude() {
  . tests/cache-lib.sh
  export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"
  load_stage0
  load_recipe_eval
  case "$TD_RECIPE_EVAL" in
    *.td-build-cache/*) : ;;
    *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;;
  esac
  echo "  [DURABLE structural] recipes evaluate with td's OWN td-recipe-eval ($TD_RECIPE_EVAL)"

  CU=${TD_GATE_INPUT_COREUTILS:-}
  test -n "$CU" || {
    echo "ERROR: TD_GATE_INPUT_COREUTILS unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2
    exit 1
  }
  if ls "$CU/bin" | grep -qE '^(guix|guile)$'; then
    echo "FAIL: guix/guile on the scrubbed PATH" >&2
    exit 1
  fi
  CACHE="$PWD/.td-build-cache/pkg"
  mkdir -p "$CACHE"
  export TD_GUIX="${TD_GUIX:-guix}"
  export CU CACHE GUIX="$TD_GUIX" ROOT="$PWD"
}

recipe_cached_build() {
  spec=$1
  lock=$2
  test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }
  grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
    || { echo "ERROR: could not realize the seed for $spec (regenerate locks on a channel bump)" >&2; exit 1; }
  cached_build "$spec" "$lock" || exit 1
  if [ -n "$hit" ]; then
    echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $out"
  else
    echo "  [STRUCTURAL] built with guix/Guile off PATH: $out"
  fi
  L="$ns/lib"
  export spec lock L
}

recipe_cached_repro_clean() {
  cached_check "$spec" || exit 1
  cached_clean
}

recipe_self_discriminates() {
  base=$1
  perturbed=${2:-$base-perturbed}
  rdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$base"'-[^ ]+\.drv' "$sd/err" "$sd/bout" 2>/dev/null | head -1`
  test -n "$rdrv" || { echo "FAIL: could not read the real $base .drv store path (self-discrimination leg)" >&2; exit 1; }
  pdir="$sd/perturbed"; rm -rf "$pdir"; mkdir -p "$pdir/b" "$pdir/tmp"
  sh tests/recipe-emit.sh "$perturbed" > "$pdir/recipe.json" || { echo "FAIL: recipe-emit $perturbed" >&2; exit 1; }
  : "${TB:?}"
  env -i HOME="$pdir" TMPDIR="$pdir/tmp" PATH="$CU/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$TB" build-recipe "$pdir/recipe.json" "$lock" "$pdir/b" /gnu/store > "$pdir/out" 2>&1 || true
  pdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$base"'-[^ ]+\.drv' "$pdir/out" 2>/dev/null | head -1`
  test -n "$pdrv" || { echo "FAIL: perturbed $base recipe did not assemble a .drv (self-discrimination leg)" >&2; tail -5 "$pdir/out" >&2; exit 1; }
  test "$pdrv" != "$rdrv" || { echo "FAIL: perturbed $base recipe assembled the SAME .drv ($rdrv) — recipe content is not load-bearing" >&2; exit 1; }
  echo "  [DURABLE self-discrimination] perturbed $base recipe -> distinct .drv (real $rdrv vs perturbed $pdrv)"
  rm -rf "$pdir"
}

recipe_gnu_version() {
  spec=$1
  bin=$2
  version_text=$3
  recipe_cached_build "$spec" "$PWD/tests/$spec-no-guix.lock"
  LD_LIBRARY_PATH="$L" "$ns/bin/$bin" --version | grep -q "$version_text" \
    || { echo "FAIL: $spec ($bin --version lacks '$version_text')" >&2; exit 1; }
  echo "  [DURABLE behavioral] $spec: $bin runs --version ($version_text) from td's own store output"
  recipe_cached_repro_clean
}

recipe_link_seed() {
  if [ -n "${RECIPE_GT_BIN:-}" ] && [ -n "${RECIPE_LINUX_HEADERS:-}" ] && [ -n "${RECIPE_NCURSES_LIB:-}" ]; then
    return
  fi
  RECIPE_GT_BIN=`for p in $($TD_GUIX build gcc-toolchain 2>/dev/null); do [ -x "$p/bin/gcc" ] && echo "$p/bin" && break; done`
  test -n "$RECIPE_GT_BIN" || { echo "ERROR: could not resolve gcc-toolchain for the link-test" >&2; exit 1; }
  RECIPE_LINUX_HEADERS=`for p in $($TD_GUIX build linux-libre-headers 2>/dev/null); do [ -f "$p/include/linux/limits.h" ] && echo "$p/include" && break; done`
  test -n "$RECIPE_LINUX_HEADERS" || { echo "ERROR: could not resolve linux-libre-headers for the link-test" >&2; exit 1; }
  RECIPE_NCURSES_LIB=`for p in $($TD_GUIX build ncurses 2>/dev/null); do [ -f "$p/lib/libncurses.so" ] && echo "$p/lib" && break; done`
  test -n "$RECIPE_NCURSES_LIB" || { echo "ERROR: could not resolve ncurses for readline's termcap link-test" >&2; exit 1; }
  export RECIPE_GT_BIN RECIPE_LINUX_HEADERS RECIPE_NCURSES_LIB
}

recipe_c_link_check() {
  spec=$1
  header=$2
  lib=$3
  pre=${4:-}
  xtra=${5:-}
  xtrun=${6:-}
  recipe_link_seed
  recipe_cached_build "$spec" "$PWD/tests/$spec-no-guix.lock"
  test -f "$ns/include/$header" || { echo "FAIL: $spec header $header missing from td output" >&2; exit 1; }
  printf '%s\n#include <%s>\nint main(void){return 0;}\n' "$pre" "$header" > "$sd/t.c"
  PATH="$RECIPE_GT_BIN:$PATH" C_INCLUDE_PATH="$RECIPE_LINUX_HEADERS" \
    "$RECIPE_GT_BIN/gcc" "$sd/t.c" -I"$ns/include" -L"$ns/lib" -Wl,--no-as-needed -l"$lib" $xtra -o "$sd/t" 2>"$sd/lk" \
    || { echo "FAIL: $spec link-test did not compile/link:" >&2; cat "$sd/lk" >&2; exit 1; }
  LD_LIBRARY_PATH="$ns/lib$xtrun" "$sd/t" \
    || { echo "FAIL: $spec link-test binary did not run (td lib not loadable)" >&2; exit 1; }
  echo "  [DURABLE behavioral] $spec: a C program links td's $header + lib$lib and runs (lib loadable)"
  if [ "$spec" = pcre2 ]; then
    LD_LIBRARY_PATH="$ns/lib" "$ns/bin/pcre2test" --version | grep -q '10.42' \
      || { echo "FAIL: pcre2test --version != 10.42" >&2; exit 1; }
    echo "  [DURABLE behavioral] pcre2test --version reports 10.42"
  fi
  recipe_cached_repro_clean
}

recipe_crate_free_build() {
  name=$1
  cratedir=$2
  lock=$3
  sourcekey=$4
  recipe=$5
  nsout=`sh tests/crate-free-build.sh "$name" "$cratedir" "$lock" "$sourcekey" "$recipe"` || exit 1
  eval "$nsout"
  ns="$NS"
  out="$OUT"
  export ns out
}

recipe_check_drv_repro() {
  rm -rf "$scratch/chk"
  "$tb" check-drv "$sd"/*.drv "$sd/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
    || { echo "FAIL: NOT reproducible (td-builder check):" >&2; tail -6 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }
  grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
    || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }
  echo "  [DURABLE repro] td-builder check double-build agrees the build is reproducible"
}

recipe_local_crate_lock_build() {
  name=$1
  source_dir=$2
  lock0=$3
  sourcekey=$4
  recipe=$5
  expected_bin=$6
  tb="$TB"
  test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }
  ncrate=`grep -cE '\.crate /gnu/store/' "$lock0"`
  test "$ncrate" -ge 2 || { echo "ERROR: lock has <2 vendored .crate deps ($ncrate)" >&2; exit 1; }
  scratch="$PWD/.td-build-cache/$name-recipe-check"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv
  grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
    || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }
  srcinfo=`sh tests/intern-src.sh "$tb" "$sourcekey-src" "$source_dir" "$scratch" target .cargo` \
    || { echo "ERROR: td could not intern $source_dir (store-add-recursive)" >&2; exit 1; }
  eval "$srcinfo"
  test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no source tree" >&2; exit 1; }
  lock="$scratch/$name.lock"; { cat "$lock0"; echo "$sourcekey $src"; } > "$lock"
  sh tests/recipe-emit.sh "$recipe" > "$scratch/recipe.json"
  sd="$scratch/b"; mkdir -p "$sd"
  env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$CU/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$tb" build-recipe "$scratch/recipe.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" > "$scratch/bout" 2>"$scratch/err" \
    || { echo "FAIL: build-recipe vendored build (guix/Guile off PATH):" >&2; tail -30 "$scratch/err" >&2; exit 1; }
  out=`sed -n 's/^OUT=out //p' "$scratch/bout"`
  test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }
  if grep -qx 'CACHE=hit' "$scratch/bout"; then hit=1; else hit=; fi
  ns="$sd/newstore/`basename "$out"`"
  test -x "$ns/bin/$expected_bin" || { echo "FAIL: build produced no binary at $ns/bin/$expected_bin" >&2; exit 1; }
  grep -q 'TD_VENDOR_CRATES' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_CRATES — the vendored path was not taken" >&2; exit 1; }
  grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv \
    || { echo "FAIL: the .drv builder is not the stage0 $TD_BUILDER_PATH" >&2; exit 1; }
  if [ -n "$hit" ]; then
    echo "  [STRUCTURAL] CACHE HIT — reused td's prior vendored build: $out"
  else
    echo "  [STRUCTURAL] td assembled + realized the .drv (TD_VENDOR_CRATES, $ncrate deps) with guix/Guile off PATH: $out"
  fi
  export tb scratch sd out ns hit
}

recipe_vendor_tree_rust_build() {
  name=$1
  source_dir=$2
  vendor=$3
  lock0=$4
  sourcekey=$5
  recipe=$6
  cargo_lock=$7
  min_crates=$8
  expected_bin=$9
  tb="$TB"
  ncrate=`ls "$vendor"/*.crate 2>/dev/null | wc -l`
  test "$ncrate" -ge "$min_crates" || {
    echo "ERROR: vendor dir $vendor has <$min_crates crates ($ncrate) — host prep must warm it first" >&2
    exit 1
  }
  test -f "$cargo_lock" || { echo "ERROR: no Cargo.lock at $cargo_lock" >&2; exit 1; }
  miss=0
  for c in "$vendor"/*.crate; do
    sha=`sha256sum "$c" | cut -d' ' -f1`
    grep -qF "$sha" "$cargo_lock" || { echo "FAIL: crate `basename $c` sha $sha is NOT pinned in $cargo_lock" >&2; miss=$((miss + 1)); }
  done
  test "$miss" -eq 0 || { echo "FAIL: $miss vendored crate(s) not pinned by $cargo_lock" >&2; exit 1; }
  echo "  [DURABLE supply-chain] all $ncrate vendored crates' sha256 are checksums pinned in $cargo_lock"

  scratch="$PWD/.td-build-cache/$name-recipe-check"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/sd"
  grep -v '\.crate ' "$lock0" | grep ' /gnu/store/' | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
    || { echo "ERROR: could not realize the toolchain seed" >&2; exit 1; }
  srcinfo=`sh tests/intern-src.sh "$tb" "$sourcekey-src" "$source_dir" "$scratch/src" target vendor .cargo` \
    || { echo "ERROR: intern source failed" >&2; exit 1; }
  eval "$srcinfo"
  vinfo=`sh tests/intern-src.sh "$tb" "$sourcekey-vendor" "$vendor" "$scratch/vendor"` \
    || { echo "ERROR: intern vendor tree failed" >&2; exit 1; }
  vsrc=`echo "$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`
  vstore=`echo "$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`
  vdb=`echo "$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`
  test -n "$vsrc" -a -n "$vstore" -a -n "$vdb" || { echo "ERROR: vendor intern produced no path" >&2; exit 1; }
  echo "  [DURABLE structural] td interned source + vendor as content-addressed trees: vendor $vsrc"
  seedlock="$scratch/seed.lock"; { grep -v '\.crate ' "$lock0" | grep -v "^$sourcekey "; echo "$sourcekey $src"; } > "$seedlock"
  sh tests/recipe-emit.sh "$recipe" > "$scratch/recipe.json"
  sd="$scratch/sd"
  env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$CU/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$tb" build-recipe "$scratch/recipe.json" "$seedlock" "$sd" /gnu/store "$srcstore" "$srcdb" "$vsrc" "$vstore" "$vdb" > "$scratch/bout" 2>"$scratch/err" \
    || { echo "FAIL: build-recipe (guix-free crates):" >&2; tail -40 "$scratch/err" >&2; exit 1; }
  out=`sed -n 's/^OUT=out //p' "$scratch/bout"`
  test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }
  ns="$sd/newstore/`basename "$out"`"
  test -x "$ns/bin/$expected_bin" || { echo "FAIL: no $expected_bin binary at $ns/bin/$expected_bin" >&2; exit 1; }
  grep -q 'TD_VENDOR_DIR' "$sd"/*.drv || { echo "FAIL: the .drv lacks TD_VENDOR_DIR" >&2; exit 1; }
  if grep -oqE '/gnu/store/[a-z0-9]+-[^ /]+\.crate' "$sd"/*.drv; then
    echo "FAIL: the .drv references a /gnu/store crate path (not guix-free)" >&2
    exit 1
  fi
  grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv \
    || { echo "FAIL: the .drv builder is not the stage0 $TD_BUILDER_PATH" >&2; exit 1; }
  echo "  [DURABLE structural] the .drv sets TD_VENDOR_DIR, references no /gnu/store crate path, and uses stage0: $out"
  export tb scratch sd out ns
}

recipe_cmake_local_build() {
  name=$1
  source_dir=$2
  lock0=$3
  sourcekey=$4
  recipe=$5
  expected_bin=$6
  expected_output=$7
  tb="$TB"
  case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac
  test -s "$lock0" || { echo "ERROR: no lock $lock0" >&2; exit 1; }
  scratch="$PWD/.td-build-cache/$name-recipe-check"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv
  grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
    || { echo "ERROR: could not realize the cmake seed" >&2; exit 1; }
  srcinfo=`sh tests/intern-src.sh "$tb" "$sourcekey-src" "$source_dir" "$scratch"` \
    || { echo "ERROR: td could not intern $source_dir (store-add-recursive)" >&2; exit 1; }
  eval "$srcinfo"
  test -n "$src" -a -d "$srcstore/`basename "$src"`" || { echo "ERROR: td interned no source tree" >&2; exit 1; }
  echo "  [DURABLE structural] td interned the current cmake source tree: $src"
  lock="$scratch/$name.lock"; { cat "$lock0"; echo "$sourcekey $src"; } > "$lock"
  sh tests/recipe-emit.sh "$recipe" > "$scratch/recipe.json"
  grep -q '"buildSystem":"cmake"' "$scratch/recipe.json" \
    || { echo "FAIL: recipe JSON is not buildSystem cmake" >&2; cat "$scratch/recipe.json" >&2; exit 1; }
  sd="$scratch/b"; mkdir -p "$sd"
  env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$CU/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$tb" build-recipe "$scratch/recipe.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" > "$scratch/bout" 2>"$scratch/err" \
    || { echo "FAIL: build-recipe cmake build (guix/Guile off PATH):" >&2; tail -30 "$scratch/err" >&2; exit 1; }
  out=`sed -n 's/^OUT=out //p' "$scratch/bout"`
  test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$scratch/err" >&2; exit 1; }
  if grep -qx 'CACHE=hit' "$scratch/bout"; then
    hit=1
  else
    hit=
    grep -q 'no guix (derivation), no Guile' "$scratch/err" \
      || { echo "FAIL: build-recipe did not assemble the .drv itself" >&2; cat "$scratch/err" >&2; exit 1; }
  fi
  grep -qE '\["cmake-build"\]' "$sd"/*.drv \
    || { echo "FAIL: the .drv did not select the cmake-build phase runner" >&2; exit 1; }
  grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$sd"/*.drv \
    || { echo "FAIL: the cmake .drv builder is not the bootstrapped stage0 $TD_BUILDER_PATH" >&2; exit 1; }
  ns="$sd/newstore/`basename "$out"`"
  test -x "$ns/bin/$expected_bin" || { echo "FAIL: cmake build produced no binary at $ns/bin/$expected_bin" >&2; exit 1; }
  got=`"$ns/bin/$expected_bin"`
  test "$got" = "$expected_output" || { echo "FAIL: $expected_bin printed '$got', expected '$expected_output'" >&2; exit 1; }
  echo "  [DURABLE behavioral] the cmake-built binary runs and prints '$got'"
  recipe_check_drv_repro

  oracle="$scratch/oracle.scm"
  { echo "(use-modules (guix packages) (guix gexp) (guix build-system cmake) ((guix licenses) #:prefix license:))";
    echo "(package (name \"$name-guix\") (version \"0.1.0\")";
    echo "  (source (local-file \"$PWD/$source_dir\" \"$sourcekey-src\" #:recursive? #t))";
    echo "  (build-system cmake-build-system) (arguments (list #:tests? #f))";
    echo "  (synopsis \"o\") (description \"cmake-build-system oracle.\") (home-page \"https://example.invalid\") (license license:gpl3+))"; } > "$oracle"
  gdrv=`$TD_GUIX build -d -f "$oracle" 2>/dev/null` \
    || { echo "ERROR: could not compute the guix cmake-build-system oracle derivation" >&2; exit 1; }
  gout=`printf '(use-modules (guix derivations))\n(for-each (lambda (o) (display (derivation-output-path (cdr o))) (newline)) (derivation-outputs (read-derivation-from-file "%s")))\n' "$gdrv" | $TD_GUIX repl 2>/dev/null | grep -oE '/gnu/store/[a-z0-9]+-'"$name"'-guix-[^ ]+' | head -1` || true
  test -n "$gout" || { echo "ERROR: could not read the guix oracle output path from $gdrv" >&2; exit 1; }
  if [ "$out" = "$gout" ]; then
    echo "FAIL: td's cmake-build path equals guix's cmake-build-system path" >&2
    exit 1
  fi
  echo "  [MIGRATION ORACLE, removable] distinct from guix's cmake-build-system build ($gout)"
  export tb scratch sd out ns
}
