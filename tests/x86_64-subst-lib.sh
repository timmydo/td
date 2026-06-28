# tests/x86_64-subst-lib.sh — helpers for the x86_64 toolchain's FETCH short-circuit
# (x64-toolchain-subst, human 2026-06-28: "the per-PR loop fetches the x86_64 toolchain instead
# of building it from seed"). Sourced by tests/bootstrap-x86_64-toolchain-store-native.sh.
#
# Two helpers:
#   build_td_subst_for_x86_64 <scratch>  -> echoes a td-BUILT (move-off-Guile §5) td-subst binary
#     path; builds it from source exactly like gate 359 (tests/toolchain-subst-default.sh), reusing
#     tests/td-subst.lock + tests/ts/recipe-td-subst.ts. Requires: $TB + the load_stage0 env
#     (TD_BUILDER_PATH/STORE/DB) + TD_TSGO/TD_TSDIR already set by the caller, host guix on PATH.
#   x86_64_subst_roundtrip <subst> <lock> <name> <iapath> <store> <sndb> <progrel> <bashbase>
#     publishes the REAL interned x86_64 component at its lock-keyed path as a SIGNED substitute
#     (ephemeral key — CI has no production secret), FETCHES it back through tools/resolve-toolchain.sh
#     into a CLEAN store, and RUNS the pre-built x86_64 program (interp = the lock-keyed path) from
#     the FETCHED-not-built bytes in a store-ns own-root -> 42. Plus the self-discrimination legs
#     (cold store -> MISS/fall back; wrong pinned key -> reject; wrong StorePath -> reject). This is
#     the consumer capability proven on the REAL cross-built x86_64 toolchain, not a fixture.

# build_td_subst_for_x86_64 <scratch>  (echoes the td-subst binary path; nonzero on failure)
build_td_subst_for_x86_64() {
  _scratch=$1
  _guix=${GUIX:-guix}
  _lock0="$(pwd)/tests/td-subst.lock"
  test -s "$_lock0" || { echo "build_td_subst: no $_lock0" >&2; return 1; }
  _cu=$(grep -- '-coreutils-' "$_lock0" | sed 's/^[^ ]* //' | head -1)
  test -n "$_cu" || { echo "build_td_subst: no coreutils in the lock" >&2; return 1; }
  mkdir -p "$_scratch/tmp" "$_scratch/b"; rm -f "$_scratch/b/"*.drv
  grep ' /gnu/store/' "$_lock0" | sed 's/^[^ ]* //' | xargs "$_guix" build >/dev/null \
    || { echo "build_td_subst: could not realize the seed + vendored .crate deps" >&2; return 1; }
  _srcinfo=$(sh tests/intern-src.sh "$TB" td-subst-src "$(pwd)/subst" "$_scratch" target vendor .cargo) \
    || { echo "build_td_subst: could not intern the subst crate tree" >&2; return 1; }
  eval "$_srcinfo"
  _lock="$_scratch/td-subst.lock"; { cat "$_lock0"; echo "td-subst-source $src"; } > "$_lock"
  sh tests/ts-emit.sh "$(pwd)/tests/ts/recipe-td-subst.ts" > "$_scratch/subst.json"
  test -s "$_scratch/subst.json" || { echo "build_td_subst: ts-emit produced no JSON" >&2; return 1; }
  env -i HOME="$_scratch" TMPDIR="$_scratch/tmp" PATH="$_cu/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$TB" build-recipe "$_scratch/subst.json" "$_lock" "$_scratch/b" /var/guix/db/db.sqlite "$srcstore" "$srcdb" \
    > "$_scratch/bout" 2>"$_scratch/err" || { echo "build_td_subst: build-recipe failed:" >&2; tail -20 "$_scratch/err" >&2; return 1; }
  _out=$(sed -n 's/^OUT=out //p' "$_scratch/bout")
  _ts="$_scratch/b/newstore/$(basename "$_out")/bin/td-subst"
  test -x "$_ts" || { echo "build_td_subst: no td-subst binary at $_ts" >&2; return 1; }
  echo "$_ts"
}

# x86_64_subst_roundtrip <subst> <lock> <name> <iapath> <store> <sndb> <progrel> <bashbase>
#   subst    a td-subst binary (build_td_subst_for_x86_64)
#   lock     tests/td-toolchain-x86_64.lock
#   name     the component (glibc-2.41-x86_64)
#   iapath   the interned lock-keyed /td/store path of that component (already in <store>)
#   store    the physical store dir holding the interned bytes + the program
#   sndb     the store db recording StorePaths
#   progrel  the /td/store-relative path of a pre-built x86_64 program whose interp IS <iapath>'s ld
#   bashbase the basename of a static bash already copied into <store> (for the store-ns shell)
x86_64_subst_roundtrip() {
  _ts=$1; _lock=$2; _name=$3; _iap=$4; _store=$5; _sndb=$6; _progrel=$7; _bbase=$8
  _shdir=$(dirname "$(command -v sh)")
  # resolve-toolchain.sh / publish-toolchain-subst.sh need coreutils + sed/grep; give them an
  # explicit coreutils dir (like gate 359) rather than relying on $_shdir carrying them.
  _cu=$(grep -- '-coreutils-' tests/td-subst.lock | sed 's/^[^ ]* //' | head -1)
  test -n "$_cu" || { echo "roundtrip: no coreutils in tests/td-subst.lock" >&2; return 1; }
  _base=$(basename "$_iap")
  _W=$(mktemp -d); mkdir -p "$_W/out" "$_W/dest"
  "$_ts" keygen "$_W/priv" "$_W/pub" >/dev/null 2>&1 || { echo "roundtrip: td-subst keygen failed" >&2; return 1; }

  # --- PRODUCER: export + sign the REAL interned x86_64 component into a persistent store ---
  env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$_ts" TD_SUBST_PRIVKEY="$_W/priv" TD_STORE_DIR=/td/store \
    sh tools/publish-toolchain-subst.sh "$_lock" "$_name" "$_sndb" "$_store" "$_W/out" >/dev/null \
    || { echo "roundtrip: publish-toolchain-subst.sh failed" >&2; return 1; }
  test -f "$_W/out/$_base.narinfo" || { echo "roundtrip: publisher wrote no narinfo for $_base" >&2; return 1; }
  grep -q '^Sig: ' "$_W/out/$_base.narinfo" || { echo "roundtrip: narinfo not signed" >&2; return 1; }
  echo "   [subst/publish] exported + signed the REAL x86_64 $_name at its lock-keyed path into the substitute store"

  # --- CONSUMER: the resolver computes the lock path, fetches (sig + StorePath + NarHash verified),
  #     restores into a CLEAN store at the lock-keyed path ---
  _fresh="$_W/fresh"; mkdir -p "$_fresh"
  _got=$(env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$_ts" TD_SUBST_STORE="$_W/out" \
         TD_SUBST_PUBKEY="$_W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$_lock" "$_name" "$_W/dest") \
    || { echo "roundtrip: resolver MISSED on a populated store (should HIT)" >&2; return 1; }
  test "x$_got" = "x$_W/dest/$_base" || { echo "roundtrip: resolver printed '$_got' != $_W/dest/$_base" >&2; return 1; }
  test -e "$_got/lib/ld-linux-x86-64.so.2" || { echo "roundtrip: fetched tree is not the x86_64 glibc (no ld-linux-x86-64.so.2)" >&2; return 1; }
  echo "   [subst/fetch] resolve-toolchain.sh computed the lock path, fetched the signed substitute (sig+StorePath+NarHash verified) and restored it — no build"

  # --- DURABLE behavioral: RUN the pre-built x86_64 program from the FETCHED-not-built bytes.
  #     Place the fetched component at its lock path in a fresh store + the program + a static bash,
  #     bind that as /td/store and run -> 42. The program's interp is the lock path the fetch filled. ---
  cp -a "$_got" "$_fresh/$_base"
  mkdir -p "$_fresh/$(dirname "$_progrel")"; cp -a "$_store/$_progrel" "$_fresh/$_progrel"
  cp -a "$_store/$_bbase" "$_fresh/$_bbase"; chmod -R u+w "$_fresh"
  _sn='[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
/td/store/'"$_progrel"'; echo "FRC=$?"'
  _out2=$(env -i "$TB" store-ns "$_fresh" -- "/td/store/$_bbase/bin/bash" -c "$_sn" 2>&1) \
    || { printf '%s\n' "$_out2" | sed 's/^/     /' >&2; echo "roundtrip: store-ns run from FETCHED bytes exited nonzero" >&2; return 1; }
  echo "$_out2" | grep -q '^FRC=42$' || { printf '%s\n' "$_out2" | sed 's/^/     /' >&2; echo "roundtrip: program from FETCHED x86_64 glibc did not return 42" >&2; return 1; }
  echo "$_out2" | grep -q '^GNU-ABSENT$' || { echo "roundtrip: /gnu/store present in the fetched-bytes own-root" >&2; return 1; }
  echo "   [subst/run-from-fetched DURABLE] a DYNAMIC x86_64 program runs from the FETCHED (not rebuilt) lock-keyed glibc in the own-root → 42, /gnu/store absent"

  # --- SELF-DISCRIMINATION: a cold store -> MISS (exit 1) so the caller falls back to from-seed ---
  mkdir -p "$_W/cold"
  if env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$_ts" TD_SUBST_STORE="$_W/cold" \
     TD_SUBST_PUBKEY="$_W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$_lock" "$_name" "$_W/d2" >/dev/null 2>&1; then
    echo "roundtrip: resolver returned 0 on a COLD store (should MISS -> fall back)" >&2; return 1
  fi
  echo "   [subst/fallback DURABLE] a cold store → the resolver MISSES (exit 1) so the gate builds from seed — the substitute is an optimization, never a correctness dependency"

  # --- SELF-DISCRIMINATION: a WRONG pinned key -> rejected -> MISS (signature load-bearing) ---
  "$_ts" keygen "$_W/wrong.priv" "$_W/wrong.pub" >/dev/null 2>&1 || { echo "roundtrip: wrong-key keygen failed" >&2; return 1; }
  if env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$_ts" TD_SUBST_STORE="$_W/out" \
     TD_SUBST_PUBKEY="$_W/wrong.pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$_lock" "$_name" "$_W/d3" >/dev/null 2>&1; then
    echo "roundtrip: resolver ACCEPTED a substitute under a WRONG pinned key (signature not load-bearing)" >&2; return 1
  fi
  echo "   [subst/self-discrimination] a wrong pinned key → fetch rejected → MISS (ed25519 signature load-bearing)"

  # --- SELF-DISCRIMINATION: a validly-signed narinfo for a DIFFERENT StorePath -> rejected ---
  cp -r "$_W/out" "$_W/out2"
  sed -i 's#^StorePath: .*#StorePath: /td/store/00000000000000000000000000000000-'"$_name"'#' "$_W/out2/$_base.narinfo"
  grep -q '^StorePath: /td/store/00000000000000000000000000000000-' "$_W/out2/$_base.narinfo" || { echo "roundtrip: could not tamper StorePath" >&2; return 1; }
  "$_ts" sign "$_W/out2" "$_W/priv" >/dev/null 2>&1 || { echo "roundtrip: re-sign of tampered narinfo failed" >&2; return 1; }
  if env -i PATH="$_cu/bin:$_shdir" TD_BUILDER="$TB" TD_SUBST_BIN="$_ts" TD_SUBST_STORE="$_W/out2" \
     TD_SUBST_PUBKEY="$_W/pub" TD_STORE_DIR=/td/store sh tools/resolve-toolchain.sh "$_lock" "$_name" "$_W/d4" >/dev/null 2>&1; then
    echo "roundtrip: resolver ACCEPTED a validly-signed substitute whose StorePath != the lock path" >&2; return 1
  fi
  echo "   [subst/self-discrimination] a validly-signed narinfo for a DIFFERENT StorePath → MISS (the input-addressed name is load-bearing alongside the signature)"
  rm -rf "$_W"
}
