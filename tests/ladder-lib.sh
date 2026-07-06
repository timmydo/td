#!/bin/sh
# tests/ladder-lib.sh — plumbing for the recipe LADDER (#378 slices 2+3), sourced by
# bootstrap-chain.sh (the chain driver) and the dev harness. NO build logic lives here —
# the rungs are recipes (recipes/src/recipes/*.rs) executed by the engine (mesboot-build);
# this lib only: interns the pinned sources/patches/seed tree (content-addressed, so the
# lock paths are deterministic), resolves the declared host TOOL packages from the loop
# PATH (replacing the deleted ladder's `ls /gnu/store/*pkg*` scavenging), writes the
# per-rung locks, emits the rung recipes, and drives `td-builder build-plan --auto`.
#
# Requires (caller): set -eu, ROOT, fail(), $TB + TD_BUILDER_* (load_stage0),
# TD_RECIPE_EVAL (load_recipe_eval), and the td-fetched tarballs warm under
# .td-build-cache/sources (td-feed warm sources).

# ladder_tool_root NAME PROBE — the store package root providing PROBE. Resolution
# order: (1) the PINNED committed lock (tests/hello-no-guix.lock — regenerated on
# channel bumps, so the tool set is pinned, unlike the deleted ladder's bare globs);
# (2) the loop PATH (`command -v`); (3) the store glob the old ladder used (flex/
# bison/m4 are in no committed lock — same hermeticity as before, shrink later).
ladder_tool_root() {
  _r=`grep " /gnu/store/" tests/hello-no-guix.lock 2>/dev/null | sed 's/^[^ ]* //; s/ .*$//' \
      | grep -E "/[a-z0-9]{32}-$1-[0-9]" | head -1`
  if [ -z "$_r" ] || [ ! -x "$_r/bin/$2" ]; then
    _bin=`command -v "$2" 2>/dev/null || true`
    if [ -n "$_bin" ]; then
      _real=`readlink -f "$_bin"` && _r=`dirname "$_real"` && _r=`dirname "$_r"`
    fi
  fi
  if [ -z "$_r" ] || [ ! -x "$_r/bin/$2" ]; then
    _b=`ls /gnu/store/*-"$1"-[0-9]*/bin/"$2" /gnu/store/*-"$1"-minimal-[0-9]*/bin/"$2" 2>/dev/null | sort | head -1`
    test -n "$_b" && _r=${_b%/bin/*}
  fi
  test -n "$_r" -a -x "$_r/bin/$2" \
    || { echo "ladder: cannot resolve the $1 package (probe $2) — not in tests/hello-no-guix.lock, not on PATH, no /gnu/store/*-$1-* on this host" >&2; return 1; }
  printf '%s\n' "$_r"
}

# ladder_setup WORKDIR — intern everything + write tools.map. Idempotent (re-interns are
# content-addressed no-ops); WORKDIR is stable across runs so build-plan's per-step
# cached_realization is the warm reuse.
ladder_setup() {
  LW=$1
  mkdir -p "$LW/store" "$LW/locks" "$LW/recipes" "$LW/scratch"
  # --- host tool packages: re-resolved EVERY call (a glob-resolved tool is not a GC
  # root — a path can vanish between runs; a changed root correctly re-keys exactly
  # the rungs that consume it, since tool paths ride into their locks/drvs) ----------
  : > "$LW/tools.map"
  for spec in bash:bash coreutils:ls sed:sed grep:grep gawk:awk tar:tar gzip:gzip \
              bzip2:bzip2 xz:xz findutils:find diffutils:diff flex:flex bison:bison \
              m4:m4 make:make python:python3; do
    _n=${spec%%:*}; _p=${spec##*:}
    _root=`ladder_tool_root "$_n" "$_p"` || return 1
    printf '%s %s\n' "$_n" "$_root" >> "$LW/tools.map"
  done
  # Idempotence for the INTERNS: an intern into an existing read-only store path
  # EACCESes, so they run ONCE per pin-set — keyed on the source locks + patches +
  # the seed tree; a pin change wipes and re-interns.
  # LADDER_SETUP_V bumps force a re-setup when the source SET grows.
  _pinsum=`{ echo ladder-setup-v2; cat seed/sources/*.lock seed/patches/*.patch; find seed/stage0 -type f | sort | xargs cat; } 2>/dev/null | sha256sum | cut -d' ' -f1`
  if [ -f "$LW/setup-ok" ] && [ "`cat "$LW/setup-ok"`" = "$_pinsum" ]; then
    # slice 4: add the x86_64 kernel headers to an already-warm ladder WITHOUT a full
    # re-setup (idempotent — interns only if absent, a NEW store path so no EACCES). This
    # is why adding the x86_64 rungs did NOT need a LADDER_SETUP_V bump.
    if ! grep -q '^linux-headers-x86-64 ' "$LW/srcs.map" 2>/dev/null; then
      _lkv=`ls seed/sources/linux-*.lock | head -1`
      _vv=`sed -n 's/^file linux-\(.*\)\.tar\..*$/\1/p' "$_lkv" | head -1`
      _khx=".td-build-cache/sources/linux-headers-$_vv-x86_64.tar.gz"
      test -f "$_khx" || { echo "ladder: x86_64 kernel-headers tarball not warm ($_khx)" >&2; return 1; }
      _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive linux-headers-x86-64 "$_khx" "$LW/store" "$LW/db"` \
        || { echo "ladder: intern linux-headers-x86-64 failed" >&2; return 1; }
      printf 'linux-headers-x86-64 %s\n' "$_p" >> "$LW/srcs.map"
    fi
    ladder_stage_tdstore || return 1
    return 0
  fi
  rm -rf "$LW/store" "$LW/db" "$LW/srcs.map" "$LW/setup-ok"
  mkdir -p "$LW/store"
  # --- intern the pinned sources (seed/sources locks; td-fetched tarballs) ----------
  # map: LOCKSTEM -> intern NAME (the lock entry name rungs reference)
  : > "$LW/srcs.map"
  for spec in \
    "mes-0.27.1:mes-source" "nyacc-1.00.2:nyacc" "tcc-0.9.26:tcc-source" \
    "make-3.80:make-mesboot0-source" "patch-2.5.9:patch-mesboot-source" \
    "binutils-2.20.1a:binutils-mesboot-source" "gcc-core-2.95.3:gcc-core-source" \
    "glibc-2.2.5:glibc-mesboot0-source" "make-3.82:make-mesboot-source" \
    "gcc-core-4.6.4:gcc-464-core" "gcc-g++-4.6.4:gcc-464-gpp" \
    "gmp-4.3.2:gmp" "mpfr-2.4.2:mpfr" "mpc-1.0.3:mpc" \
    "gawk-3.1.8:gawk-mesboot-source" "glibc-mesboot-2.16.0:glibc-216-source" \
    "gcc-4.9.4:gcc-494-source" "gcc-14.3.0:gcc-14-source" \
    "gcc14-gmp-6.3.0:gmp63" "gcc14-mpfr-4.2.1:mpfr421" "gcc14-mpc-1.3.1:mpc131" \
    "binutils-2.44:binutils-244-source" "glibc-2.41:glibc-241-source"; do
    _stem=${spec%%:*}; _name=${spec##*:}
    _lock=`ls seed/sources/$_stem*.lock 2>/dev/null | head -1`
    test -n "$_lock" || { echo "ladder: no seed/sources lock for $_stem" >&2; return 1; }
    _file=".td-build-cache/sources/`sed -n 's/^file //p' "$_lock" | head -1`"
    test -f "$_file" || { echo "ladder: pinned tarball not warm ($_file) — run 'td-feed warm sources'" >&2; return 1; }
    _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive "$_name" "$_file" "$LW/store" "$LW/db"` \
      || { echo "ladder: intern $_name failed" >&2; return 1; }
    printf '%s %s\n' "$_name" "$_p" >> "$LW/srcs.map"
  done
  # the host-produced kernel-headers tarball (td-feed warm sources)
  _lk=`ls seed/sources/linux-*.lock | head -1`
  _v=`sed -n 's/^file linux-\(.*\)\.tar\..*$/\1/p' "$_lk" | head -1`
  _kh=".td-build-cache/sources/linux-headers-$_v-i386.tar.gz"
  test -f "$_kh" || { echo "ladder: kernel-headers tarball not warm ($_kh)" >&2; return 1; }
  _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive linux-headers "$_kh" "$LW/store" "$LW/db"` \
    || { echo "ladder: intern linux-headers failed" >&2; return 1; }
  printf 'linux-headers %s\n' "$_p" >> "$LW/srcs.map"
  # the x86_64 kernel-headers tarball (slice 4 cross rungs; td-feed warm sources produces BOTH
  # archs, so this is always warm alongside the i386 set). Interned as `linux-headers-x86-64`.
  _khx=".td-build-cache/sources/linux-headers-$_v-x86_64.tar.gz"
  test -f "$_khx" || { echo "ladder: x86_64 kernel-headers tarball not warm ($_khx)" >&2; return 1; }
  _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive linux-headers-x86-64 "$_khx" "$LW/store" "$LW/db"` \
    || { echo "ladder: intern linux-headers-x86-64 failed" >&2; return 1; }
  printf 'linux-headers-x86-64 %s\n' "$_p" >> "$LW/srcs.map"
  # --- intern the vendored boot patches + the stage0 seed tree ----------------------
  for pp in binutils-boot-2.20.1a gcc-boot-2.95.3 glibc-boot-2.2.5 glibc-bootstrap-system-2.2.5 \
            gcc-boot-4.6.4 glibc-boot-2.16.0 glibc-bootstrap-system-2.16.0; do
    _f="seed/patches/$pp.patch"; test -f "$_f" || { echo "ladder: missing $_f" >&2; return 1; }
    _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive "patch-$pp" "$_f" "$LW/store" "$LW/db"` \
      || { echo "ladder: intern patch $pp failed" >&2; return 1; }
    printf 'patch-%s %s\n' "$pp" "$_p" >> "$LW/srcs.map"
  done
  _p=`TD_STORE_DIR=/td/store "$TB" store-add-recursive stage0-source "$ROOT/seed/stage0" "$LW/store" "$LW/db"` \
    || { echo "ladder: intern stage0-source failed" >&2; return 1; }
  printf 'stage0-source %s\n' "$_p" >> "$LW/srcs.map"
  printf '%s' "$_pinsum" > "$LW/setup-ok"
  ladder_stage_tdstore || return 1
}

# Stage every interned item into build-plan's shared td-store: realize re-keys a bare
# closure entry whose BASENAME lives under scratch/tdstore, so the sandbox binds our
# interned sources/patches from there (they exist nowhere else on disk). Self-healing
# (re-run per setup call — a wiped scratch repopulates).
ladder_stage_tdstore() {
  mkdir -p "$LW/scratch/tdstore"
  while read -r _n _p; do
    _b=${_p##*/}
    test -e "$LW/scratch/tdstore/$_b" \
      || cp -al "$LW/store/$_b" "$LW/scratch/tdstore/$_b" 2>/dev/null \
      || cp -a "$LW/store/$_b" "$LW/scratch/tdstore/$_b" \
      || { echo "ladder: stage $_b into tdstore failed" >&2; return 1; }
  done < "$LW/srcs.map"
}

# ladder_map NAME — the interned path (srcs.map) or tool root (tools.map).
ladder_map() {
  _v=`sed -n "s/^$1 //p" "$LW/srcs.map" "$LW/tools.map" 2>/dev/null | head -1`
  test -n "$_v" || { echo "ladder: no map entry for \`$1'" >&2; return 1; }
  printf '%s\n' "$_v"
}

# ladder_lock RUNG SOURCE-NAME ENTRY…  — write locks/RUNG-no-guix.lock. Each ENTRY is
# either `tool:NAME` / `src:NAME` (resolved via the maps) or `rung:NAME` (a prior rung —
# a placeholder path; --auto re-keys it td-recipe-output and build-plan substitutes the
# built output).
ladder_lock() {
  _rung=$1; _srcname=$2; shift 2
  _lf="$LW/locks/$_rung-no-guix.lock"; : > "$_lf"
  _sp=`ladder_map "$_srcname"` || return 1
  printf '%s-source %s source\n' "$_rung" "$_sp" >> "$_lf"
  for e in "$@"; do
    _k=${e%%:*}; _n=${e##*:}
    case "$_k" in
      rung) printf '%s /td/store/pending-%s\n' "$_n" "$_n" >> "$_lf" ;;
      *) _p=`ladder_map "$_n"` || return 1
         printf '%s %s seed\n' "$_n" "$_p" >> "$_lf" ;;
    esac
  done
}

# ladder_emit RUNG… — emit each rung's recipe JSON into $LW/recipes (the --auto
# ownership scope: ONLY rung recipes live here, so tool inputs stay seed-resolved).
ladder_emit() {
  for r in "$@"; do
    "$TD_RECIPE_EVAL" emit "$r" > "$LW/recipes/$r.json" || { echo "ladder: emit $r failed" >&2; return 1; }
  done
}

# ladder_build TARGET — drive the engine: build-plan --auto over the rung graph.
ladder_build() {
  env -i HOME="$LW" TMPDIR="$LW" TD_STORE_DIR=/td/store \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$TB" build-plan --auto "$1" "$LW/recipes" "$LW/locks" /gnu/store "$LW/scratch" \
      >"$LW/build-$1.out" 2>"$LW/build-$1.err" \
    || { tail -40 "$LW/build-$1.err" >&2; echo "ladder: build-plan --auto $1 failed" >&2; return 1; }
  cat "$LW/build-$1.out"
}

# ladder_out RUNG — the built output dir for RUNG from the last plan run (the shared
# td-store the plan stages through).
ladder_out() {
  _o=`sed -n "s/^STEP $1 //p" "$LW"/build-*.out 2>/dev/null | tail -1`
  test -n "$_o" || { echo "ladder: no STEP output recorded for $1" >&2; return 1; }
  printf '%s/tdstore/%s\n' "$LW/scratch" "${_o##*/}"
}
