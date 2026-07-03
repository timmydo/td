//! build-plan — td CHAINS its OWN build outputs into downstream builds, with the plan
//! GENERATED from the recipe GRAPH (no manifest). For every owned recipe that has an
//! owned input edge (a declared input that is itself a td recipe), `td-builder build-plan
//! --auto` topo-sorts the owned-input closure, marks each edge `td-recipe-output`, and
//! builds the DAG — bash<-readline<-ncurses, grep<-pcre2, etc., each
//! dep built once (shared scratch). Subjects are DERIVED here from the recipe inputs, so
//! a new recipe's edges chain automatically — the same graph the guix-dependence census
//! reads to credit edge-owned. Per subject: DURABLE structural (the subject's .drv
//! references td's dep outputs AND NOT guix's), behavioral (runs from td's output loading
//! td's deps; a library subject's .so is present), repro (td-builder check double-build),
//! MIGRATION ORACLE (distinct path). guix/Guile SCRUBBED FROM PATH; toolchain + locks are
//! the guix-built seed (§5, retired last). The build-plan DRIVER is the cargo-bootstrapped
//! stage0 td-builder (load_stage0) — no guix-built td-builder (R2, #275: the guix-as-packager
//! surface is 0). The per-subject `guix build <S>` remains a removable differential oracle
//! (grows per package, retires wholesale with guix — NOT the ratcheted packager surface).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "build-plan",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> build-plan: --auto chains td-built deps into downstream builds (subjects DERIVED from the recipe graph, no manifest) — each subject's .drv references td's deps (NOT guix's), runs, reproducibly, at distinct paths"
set -euo pipefail; \
TD_RECIPE_EVAL=`TD_GUIX="$TD_GUIX" sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval"`; export TD_RECIPE_EVAL; \
test -x "$TD_RECIPE_EVAL" || { echo "ERROR: could not resolve td-recipe-eval" >&2; exit 1; }; \
grep ' /gnu/store/' "$PWD/tests/td-builder-rust.lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/build-plan/stage0"; load_stage0; \
tb="$TB"; \
cu=`grep -- '-coreutils-' "$PWD/tests/grep-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
root="$PWD/.td-build-cache/build-plan"; jd="$root/json"; mkdir -p "$jd" "$root/tmp"; \
owned=""; \
for s in `"$TD_RECIPE_EVAL" list`; do \
  case "$s" in *perturbed*) continue;; esac; \
  test -f "$PWD/tests/$s-no-guix.lock" || continue; \
  sh tests/recipe-emit.sh "$s" > "$jd/$s.json"; \
  test -s "$jd/$s.json" || { echo "ERROR: recipe-emit produced no JSON for $s" >&2; exit 1; }; \
  owned="$owned $s"; \
done; \
subjects=""; \
for s in $owned; do \
  inp=`grep -oE '"inputs":\[[^]]*\]' "$jd/$s.json" | sed 's/^"inputs"://' | grep -oE '"[^"]*"' | tr -d '"' || true`; \
  for i in $inp; do if echo " $owned " | grep -q " $i "; then subjects="$subjects $s"; break; fi; done; \
done; \
test -n "$subjects" || { echo "ERROR: derived no build-plan subjects from the recipe graph" >&2; exit 1; }; \
echo "  subjects derived from the recipe graph:$subjects"; \
for S in $subjects; do \
  edges=`grep -oE '"inputs":\[[^]]*\]' "$jd/$S.json" | sed 's/^"inputs"://' | grep -oE '"[^"]*"' | tr -d '"' | while read i; do echo " $owned " | grep -q " $i " && echo "$i"; done`; \
  { grep ' /gnu/store/' "$PWD/tests/$S-no-guix.lock" | grep -v 'td-recipe-output'; for d in $edges; do grep ' /gnu/store/' "$PWD/tests/$d-no-guix.lock" 2>/dev/null; done; } | sed 's/^[^ ]* //' | sort -u | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize guix seeds for $S" >&2; exit 1; }; \
  env -i HOME="$root" TMPDIR="$root/tmp" PATH="$cu/bin" TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" "$tb" build-plan --auto "$S" "$jd" "$PWD/tests" /gnu/store "$root" > "$root/out-$S" 2>"$root/err-$S" || { echo "FAIL: build-plan --auto $S (guix/Guile off PATH):" >&2; tail -30 "$root/err-$S" >&2; exit 1; }; \
  td_S=`sed -n "s/^STEP $S //p" "$root/out-$S"`; \
  test -n "$td_S" || { echo "FAIL: --auto did not report the $S step" >&2; cat "$root/out-$S" >&2; exit 1; }; \
  sdrv=`ls "$root/$S"/*.drv 2>/dev/null | head -1`; \
  test -s "$sdrv" || { echo "FAIL: $S's .drv missing" >&2; exit 1; }; \
  out="$root/$S/newstore/`basename "$td_S"`"; \
  ld="$out/lib"; \
  for d in $edges; do \
    td_d=`sed -n "s/^STEP $d //p" "$root/out-$S"`; \
    test -n "$td_d" || { echo "FAIL: --auto did not build edge $d of $S" >&2; exit 1; }; \
    grep -q "$td_d" "$sdrv" || { echo "FAIL: $S's .drv does NOT reference td's $d ($td_d)" >&2; exit 1; }; \
    if grep -q "^$d /" "$PWD/tests/$S-no-guix.lock"; then gp=`sed -n "s/^$d //p" "$PWD/tests/$S-no-guix.lock" | head -1`; else gp=`sed -n "s#^[^ ]*-$d-[^ ]* \(/gnu/store/[^ ]*\)#\1#p" "$PWD/tests/$S-no-guix.lock" | head -1`; fi; \
    if [ -n "$gp" ] && grep -q "$gp" "$sdrv"; then echo "FAIL: $S's .drv STILL references guix's $d ($gp)" >&2; exit 1; fi; \
    ld="$ld:$root/tdstore/`basename "$td_d"`/lib"; \
  done; \
  echo "  [$S DURABLE structural] --auto chained$(for d in $edges; do printf ' %s' "$d"; done); .drv references td's edges and NOT guix's"; \
  case "$S" in \
    grep) printf 'foobar\nbaz\n' | LD_LIBRARY_PATH="$ld" "$out/bin/grep" -P 'o{2}' | grep -qx foobar || { echo "FAIL: $S -P match" >&2; exit 1; }; bh="grep -P matches via td's pcre2" ;; \
    bash) LD_LIBRARY_PATH="$ld" "$out/bin/bash" -c 'echo $BASH_VERSION' | grep -q '^5' || { echo "FAIL: bash run" >&2; exit 1; }; bh="bash runs loading td's readline + ncurses" ;; \
    readline) ls "$out"/lib/libreadline.so* >/dev/null 2>&1 || { echo "FAIL: libreadline.so missing" >&2; exit 1; }; bh="libreadline.so present (library subject)" ;; \
    less) LD_LIBRARY_PATH="$ld" "$out/bin/less" --version | grep -q 'less 608' || { echo "FAIL: less --version" >&2; exit 1; }; bh="less --version 608 loads td's ncurses" ;; \
    *) echo "FAIL: no behavioral check defined for subject $S — add one" >&2; exit 1 ;; \
  esac; \
  echo "  [$S DURABLE behavioral] $bh"; \
  if [ -f "$root/$S/repro-ok" ] && [ "$root/$S/repro-ok" -nt "$sdrv" ]; then echo "  [$S DURABLE repro] CACHED"; else \
    rm -rf "$root/$S/chk"; \
    env -i HOME="$root" TMPDIR="$root/tmp" PATH="$cu/bin" "$tb" check-drv "$sdrv" "$root/$S/closure.txt" "$root/$S/chk" >/dev/null 2>"$root/chkerr-$S" || { echo "FAIL: chained $S NOT reproducible:" >&2; tail -6 "$root/chkerr-$S" >&2; exit 1; }; \
    touch "$root/$S/repro-ok"; echo "  [$S DURABLE repro] td-builder check double-build agrees $S is reproducible"; fi; \
  gs=`$TD_GUIX build "$S" 2>/dev/null | grep -v -- '-debug\|-doc\|-static\|-lib$' | head -1 || true`; \
  if [ -n "$gs" ] && [ "$td_S" = "$gs" ]; then echo "FAIL: td's $S path equals guix's" >&2; exit 1; fi; \
  echo "  ==> $S edge-owned: built from td's edges ($td_S)"; \
done; \
echo "PASS: build-plan --auto chained EVERY owned recipe with owned input edges (DERIVED from the recipe graph, no manifest) — each subject's .drv references td's OWN dep outputs (not guix's), runs from td's output loading td's deps (durable; bash<-readline<-ncurses is a 2-level td DAG), is reproducible by td's own double-build (durable), and lands at a distinct store path from guix's (own, then diverge). guix/Guile SCRUBBED FROM PATH; the toolchain + locks are the guix-built seed (§5, retired last)."
"##,
    }
}
