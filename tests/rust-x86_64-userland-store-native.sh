#!/bin/sh
# tests/rust-x86_64-userland-store-native.sh — #258 workstream B ("build world" cutover): rebuild the
# shipped Rust userland (ripgrep, the template) with the NATIVE x86_64 /td/store toolchain instead of
# the guix rust + gcc-toolchain seed. It composes three PROVEN paths:
#
#   1. gate 416's assembly (tests/rust-x86_64-runtime-store-native.sh, sourced ASSEMBLE-ONLY): fetch or
#      build-from-seed the x86_64 cross toolchain, build the NATIVE x86_64 gcc 14.3.0 + binutils 2.44,
#      relink the upstream Rust 1.96.0 rustc/cargo to /td/store WITH the rustlib sysroot, and intern
#      them + the x86_64 glibc 2.41 at /td/store (+ /td/store/ld + a static bash). All /gnu/store-free.
#   2. the crate-free provisioning (tests/crate-free-build.sh): assert every vendored crate's sha256 is
#      pinned in ripgrep's shipped Cargo.lock (== the crates.io cksum td's cargo-proxy verified — the
#      guix-free oracle), and intern the source tree + the crate SET with td's own store-add-recursive.
#   3. the brick-8 corpus-toolchain path (tests/bootstrap-sed-corpus-store-native.sh): build the recipe
#      through `td-builder build-recipe`/run_rust with the /td/store toolchain substituted for the guix
#      seed — chained via TD_SEED_STORE + TD_EXTRA_DBS (the engine's multi-store closure) — and with
#      the #258 engine switch TD_RUST_STORE_{INTERP,RPATH,BDIR} so run_rust bakes the native gcc's
#      interp/RUNPATH/-B (the native gcc is a PLAIN gcc, not guix's ld-wrapper).
#
# The produced `rg` links the /td/store glibc 2.41 (interp = the /td/store x86_64 ld, RUNPATH = the
# /td/store libc/libgcc_s), RUNS in a store-ns own-root with /gnu/store ABSENT, and GREPS a needle in
# a tree (and not an unrelated file) — a real ripgrep doing real work, built by td's own toolchain.
# Reproducible: `td-builder check`'s double-build of the ripgrep drv agrees bit-for-bit.
#
# Legs (DURABLE — no guix oracle in any):
#   [supply-chain]  every vendored crate's sha256 is pinned in ripgrep's Cargo.lock (the upstream cksum).
#   [native-arch]   the linker rustc drives is the NATIVE x86_64 gcc/as/ld (ELF64 x86-64), at /td/store.
#   [no-guix]       `rg` references NO guix rust and NO guix gcc-toolchain; its interp/RUNPATH are
#                   /td/store; the compile-path toolchain binaries are guix-free (as gate 416/422 check).
#   [behavioral]    the /td/store-linked `rg` RUNS in an own-root (/gnu/store ABSENT), prints its version,
#                   and greps a needle line (and NOT the unrelated file) — it works as ripgrep.
#   [repro]         `td-builder check`'s double-build of the ripgrep drv is bit-for-bit reproducible.
# Self-discrimination (verified-red): drop TD_RUST_STORE_INTERP → run_rust links via the native gcc with
# NO baked interp, so `rg` gets the build host's /lib64 loader and cannot run in the own-root; drop the
# native toolchain from the lock → the guix rust/gcc-toolchain is used and the [no-guix] leg reds.
# HEAVY (the native gcc build is ~45 min; from-seed adds the ~98-min cross build). Reuses the crate
# closure the check.sh prelude warms (td-feed warm crate). NOT run outside the loop sandbox.
set -eu

PKG=ripgrep; CRATEDIR=ripgrep-14.1.1; SOURCEKEY=ripgrep-source; RECIPE=ripgrep; BIN=rg
LOCK=tests/ripgrep.lock
GUIX=${GUIX:-guix}   # the pinned guix (the gate passes it); provides the retired-last build seed only

# ============================================================================================
# 1. Assemble the native /td/store rust toolchain — gate 416's proven assembly, sourced as a library.
#    This sets up TB / ROOT / TD_STORE_DIR=/td/store, sources the cache + x86_64 libs, loads stage0,
#    and (at its ASSEMBLE-ONLY guard) exports TDSN_* and returns before its hello.rs probe.
# ============================================================================================
export TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY=1
. tests/rust-x86_64-runtime-store-native.sh
unset TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY
STORE="$TDSN_STORE"; SNDB="$TDSN_DB"
NGREL="$TDSN_NGREL"; NBREL="$TDSN_NBREL"; GLREL="$TDSN_GLREL"; RUSTREL="$TDSN_RUSTREL"; BBASE="$TDSN_BBASE"
test -x "$STORE/$RUSTREL/bin/cargo" -a -x "$STORE/$NGREL/bin/gcc" -a -e "$STORE/$GLREL/lib/libc.so.6" \
  || fail "assemble-only did not produce the expected /td/store toolchain layout"
# td-recipe-eval (the guix-free recipe evaluator, recipes/ crate). This gate warms it ITSELF (not via
# the whole build-recipes corpus prelude) so it stays a lean HEAVY gate; load_recipe_eval (cache-lib,
# sourced by the assembly above) then locates it.
if [ ! -s "$ROOT/.td-build-cache/recipe-eval/recipe-eval-path" ]; then
  TD_GUIX="${GUIX:-guix}" sh tests/recipe-eval-tool.sh "$ROOT/.td-build-cache/recipe-eval" >/dev/null \
    || fail "could not build td's Rust recipe evaluator (recipes/ crate)"
fi
load_recipe_eval || fail "no td-recipe-eval"
echo ">> [258] native /td/store toolchain assembled — building $PKG ($CRATEDIR) against it"

# ============================================================================================
# 2. Crate-free provisioning: assert the warmed crate set is pinned, intern the source + crate set.
# ============================================================================================
dest="$ROOT/.td-build-cache/crate-vendor/$PKG"
srctree="$dest/src/$CRATEDIR"; vendor="$dest/vendor"; cargolock="$srctree/Cargo.lock"
test -f "$srctree/Cargo.toml" || fail "no warmed source at $srctree — the check.sh prelude (td-feed warm crate) must provision it"
test -f "$cargolock" || fail "$srctree ships no Cargo.lock"
ncrate=`ls "$vendor"/*.crate 2>/dev/null | wc -l`
test "$ncrate" -ge 30 || fail "vendor dir $vendor has <30 crates ($ncrate) — re-run td-feed warm crate"
miss=0
for c in "$vendor"/*.crate; do
  sha=`sha256sum "$c" | cut -d' ' -f1`
  grep -qF "$sha" "$cargolock" || { echo "FAIL: crate `basename "$c"` sha $sha not pinned in $PKG's Cargo.lock" >&2; miss=$((miss + 1)); }
done
test "$miss" -eq 0 || fail "$miss vendored crate(s) not pinned by $PKG's Cargo.lock"
echo "   [supply-chain] all $ncrate vendored crates' sha256 are pinned in $PKG's Cargo.lock (== the crates.io cksum the cargo-proxy verified)"

scratch="$ROOT/.td-build-cache/$PKG-userland-store-native"; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/sb"
srcinfo=`sh tests/intern-src.sh "$TB" "$PKG-src" "$srctree" "$scratch/src" target vendor .cargo` || fail "intern source failed"
eval "$srcinfo"   # -> src, srcstore, srcdb
vinfo=`sh tests/intern-src.sh "$TB" "$PKG-vendor" "$vendor" "$scratch/vendor"` || fail "intern vendor tree failed"
vsrc=`echo "$vinfo" | sed -n "s/^src='\(.*\)'/\1/p"`
vstore=`echo "$vinfo" | sed -n "s/^srcstore='\(.*\)'/\1/p"`
vdb=`echo "$vinfo" | sed -n "s/^srcdb='\(.*\)'/\1/p"`
test -n "$vsrc" -a -n "$vstore" -a -n "$vdb" || fail "vendor intern produced no path"
echo "   [structural] interned the source ($src) + the $ncrate-crate set ($vsrc) content-addressed (store-add-recursive, no daemon)"

# ============================================================================================
# 3. Provision the guix build SEED (coreutils/tar/gzip — cp/chmod/tar for run_rust; retired LAST) into a
#    warm-seed store, and copy the native /td/store toolchain trees in beside it so ALL build inputs
#    stage from ONE store (the brick-8 combined-store form). The native toolchain refs come from $SNDB
#    (TD_EXTRA_DBS); the guix seed refs from the warmed seed db.
# ============================================================================================
seedroots=`grep ' /gnu/store/' "$LOCK" | grep -vE -- '-rust-|-gcc-toolchain-' | sed 's/^[^ ]* //'`
test -n "$seedroots" || fail "no guix build seed (coreutils/tar/gzip) in $LOCK"
# realize the seed closure (retired-last guix bytes; the DELIVERABLE `rg` carries none — asserted below)
echo "$seedroots" | xargs "$GUIX" build >/dev/null 2>&1 || fail "could not realize the guix build seed"
seedline=`TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite sh tools/warm-seed.sh "$ROOT/.td-build-cache/seed-userland" $seedroots` \
  || fail "warm-seed of the build seed failed"
WSTORE=`echo "$seedline" | cut -d' ' -f1`; WDB=`echo "$seedline" | cut -d' ' -f2`
for rel in "$NGREL" "$NBREL" "$GLREL" "$RUSTREL"; do
  test -d "$WSTORE/$rel" || cp -a "$STORE/$rel" "$WSTORE/$rel"
done
chmod -R u+w "$WSTORE/$NGREL" "$WSTORE/$NBREL" "$WSTORE/$GLREL" "$WSTORE/$RUSTREL" 2>/dev/null || true

# ============================================================================================
# 4. Rewrite the lock: the guix rust + rust-cargo + gcc-toolchain lines become the /td/store native
#    gcc + binutils + glibc + relinked rust; the coreutils/bash/tar/gzip build seed stays (retired last).
# ============================================================================================
newlock="$scratch/$PKG.lock"
{
  grep ' /gnu/store/' "$LOCK" | grep -vE -- '-rust-|-gcc-toolchain-'
  echo "rust-1.96.0-x86_64-store-native /td/store/$RUSTREL seed"
  echo "gcc-14.3.0-x86_64-native /td/store/$NGREL seed"
  echo "binutils-2.44-x86_64-native /td/store/$NBREL seed"
  echo "glibc-2.41-x86_64 /td/store/$GLREL seed"
  echo "$SOURCEKEY $src source"
} > "$newlock"
# guard: the rewritten lock names NO guix rust and NO guix gcc-toolchain (the cutover, checked at the lock).
if grep -qE -- '/gnu/store/[a-z0-9]+-(rust-|gcc-toolchain-)' "$newlock"; then fail "rewritten lock still names a guix rust/gcc-toolchain"; fi
echo "   [cutover] lock retargeted onto the /td/store native toolchain (guix rust + gcc-toolchain removed)"

# link-mode env: run_rust bakes the native gcc's interp/RUNPATH/-B (native gcc is a PLAIN gcc). libgcc_s
# lives in the native gcc tree; the produced `rg` resolves libc + libgcc_s from /td/store at run time.
lgrel=`cd "$STORE/$NGREL" && find . -name libgcc_s.so.1 2>/dev/null | head -1 | sed 's|^\./||'`
test -n "$lgrel" || fail "no libgcc_s.so.1 in the native gcc tree"
interp="/td/store/$GLREL/lib/ld-linux-x86-64.so.2"
rpath="/td/store/$GLREL/lib:/td/store/$NGREL/`dirname "$lgrel"`"
bdir="/td/store/$GLREL/lib"

# emit the ripgrep recipe (guix-free evaluator) + build it through run_rust with the /td/store toolchain.
sh tests/recipe-emit.sh "$RECIPE" > "$scratch/recipe.json" || fail "recipe-emit $RECIPE"
test -s "$scratch/recipe.json" || fail "recipe-emit produced no JSON"
cu=`grep -- '-coreutils-' "$newlock" | sed 's/^[^ ]* //' | head -1`
test -n "$cu" || fail "no coreutils in the rewritten lock for the scrubbed PATH"
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi

sb="$scratch/sb"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$SNDB" \
  TD_RUST_STORE_INTERP="$interp" TD_RUST_STORE_RPATH="$rpath" TD_RUST_STORE_BDIR="$bdir" \
  TD_PERSIST_STORE="$STORE" TD_PERSIST_DB="$SNDB" \
  "$TB" build-recipe "$scratch/recipe.json" "$newlock" "$sb" "$WSTORE" "$srcstore" "$srcdb" "$vsrc" "$vstore" "$vdb" \
  > "$sb.out" 2>"$sb.err" || { echo "FAIL: build-recipe (ripgrep on the /td/store toolchain):" >&2; tail -40 "$sb.err" >&2; exit 1; }
out=`sed -n 's/^OUT=out //p' "$sb.out"`
test -n "$out" || { echo "FAIL: build-recipe produced no output" >&2; cat "$sb.err" >&2; exit 1; }
case "$out" in /td/store/*-ripgrep-14.1.1) ;; *) fail "ripgrep output not content-addressed under /td/store (got: $out)" ;; esac
rgrel=${out#/td/store/}
echo "   [build] build-recipe built ripgrep at $out (persisted into the /td/store toolchain store)"

# --- [structural] the .drv sets TD_VENDOR_DIR + the native link mode, and references NO guix rust/gcc-toolchain
grep -q 'TD_VENDOR_DIR' "$sb"/*.drv || fail "the .drv lacks TD_VENDOR_DIR"
grep -q 'TD_RUST_STORE_INTERP' "$sb"/*.drv || fail "the .drv lacks TD_RUST_STORE_INTERP (native link mode not wired)"
if grep -oqE '/gnu/store/[a-z0-9]+-(rust-|gcc-toolchain-)' "$sb"/*.drv; then fail "the .drv references a guix rust/gcc-toolchain path"; fi

# --- [no-guix] the DELIVERABLE `rg` references NO guix rust / gcc-toolchain, and links the /td/store glibc
rgbin="$STORE/$rgrel/bin/$BIN"
test -x "$rgbin" || fail "no $BIN binary at $rgbin"
if grep -q -a -- '/gnu/store' "$rgbin"; then fail "the built $BIN contains /gnu/store bytes"; fi
si=`"$STORE/$NBREL/bin/readelf" -l "$rgbin" 2>/dev/null | grep -o "$interp" | head -1`
test -n "$si" || fail "$BIN not linked vs the /td/store glibc loader ($interp)"
echo "   [no-guix] the built $BIN carries zero /gnu/store bytes; interp = the /td/store x86_64 ld ($si)"

# --- [native-arch] the linker was the native x86_64 gcc/as/ld (ELF64) — the interned tools used to link
nhdr=`"$STORE/$NBREL/bin/readelf" -h "$STORE/$NGREL/bin/gcc" 2>/dev/null`
echo "$nhdr" | grep -i 'class:' | grep -q 'ELF64' || fail "the /td/store linker gcc is not ELF64"
echo "$nhdr" | grep -i 'machine:' | grep -qi 'x86-64' || fail "the /td/store linker gcc is not x86-64"
echo "   [native-arch] the linker rustc drove is the NATIVE x86_64 gcc + as/ld (ELF64 x86-64) at /td/store"

# ============================================================================================
# 5. [behavioral] run `rg` in the store-ns own-root: /gnu/store ABSENT, print the version, grep a needle
#    (and NOT the unrelated file). The probe uses ONLY bash builtins (printf) + the store's own binaries.
# ============================================================================================
cat > "$STORE/rgprobe.sh" <<PROBE
export PATH=/td/store/$rgrel/bin
export TMPDIR=/tmp
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
cd /tmp || exit 1
printf 'alpha line\nthe needle is here\nbeta line\n' > hay.txt
printf 'nothing to see\n' > other.txt
/td/store/$rgrel/bin/rg --version && echo RG-VERSION-OK
/td/store/$rgrel/bin/rg needle hay.txt other.txt
echo "RG-RC=\$?"
PROBE
out2=`"$TB" store-ns "$STORE" -- "/td/store/$BBASE/bin/bash" /td/store/rgprobe.sh 2>&1` \
  || { printf '%s\n' "$out2" | sed 's/^/     /' >&2; fail "store-ns rg run exited nonzero"; }
printf '%s\n' "$out2" | sed 's/^/     /'
printf '%s\n' "$out2" | grep -q '^GNU-ABSENT$'    || fail "/gnu/store is PRESENT in the own-root"
printf '%s\n' "$out2" | grep -q '^ripgrep 14\.1\.1' || fail "$BIN did not print its version from /td/store"
printf '%s\n' "$out2" | grep -q '^RG-VERSION-OK$'  || fail "$BIN --version did not run cleanly from /td/store"
printf '%s\n' "$out2" | grep -q 'needle'           || fail "$BIN did not find the 'needle' line"
printf '%s\n' "$out2" | grep -q 'other.txt'        && fail "$BIN matched the unrelated file (over-match)"
printf '%s\n' "$out2" | grep -q '^RG-RC=0$'        || fail "$BIN did not exit 0 in the own-root"
echo "   [behavioral] the /td/store-linked $BIN RAN in the own-root (/gnu/store ABSENT), printed its version, and grepped the needle (not the unrelated file)"

# ============================================================================================
# 6. [repro] td-builder check double-builds the ripgrep drv → bit-for-bit reproducible.
# ============================================================================================
rm -rf "$scratch/chk"
"$TB" check "$sb"/*.drv "$sb/closure.txt" "$scratch/chk" > "$scratch/checkout.txt" 2>"$scratch/chk.err" \
  || { echo "FAIL: NOT reproducible (td-builder check):" >&2; tail -8 "$scratch/checkout.txt" "$scratch/chk.err" >&2; exit 1; }
grep -qE "^CHECK out $out sha256:[0-9a-f]+ reproducible$" "$scratch/checkout.txt" \
  || { echo "FAIL: td-builder check did not confirm $out reproducible:" >&2; cat "$scratch/checkout.txt" >&2; exit 1; }
echo "   [repro] td-builder check double-build agrees the /td/store-toolchain $PKG build is reproducible"

echo "PASS: rust-x86_64-userland-store-native — $PKG (rg 14.1.1) built with its $ncrate-crate closure GUIX-FREE"
echo "  and LINKED by the NATIVE x86_64 /td/store toolchain (gcc 14.3.0 + binutils 2.44 + glibc 2.41, the guix"
echo "  rust + gcc-toolchain seed removed); the /td/store-linked rg RAN in a /gnu/store-absent own-root and grepped"
echo "  a needle; reproducible. The 'build world' cutover for the ripgrep template — no guix rust, no guix gcc."
