#!/bin/sh
# tests/userland-x86_64-store-native.sh — host-sandbox-stage0 inc2: the guix-less daily-suite
# CAPTURED SET's C userland (busybox + GNU make) at /td/store, NO GUIX BYTES. From the
# 229-byte seed, td builds the i686 chain → gcc 14.3.0, CROSSES UP to a native x86_64
# toolchain (gcc 14.3.0 + glibc 2.41 + libgcc_s; reused from the x86_64 gate as a function
# library), then builds busybox 1.37.0 + GNU make 4.4.1 FROM upstream source (td-fetch,
# sha-pinned) with that toolchain — DYNAMIC against the /td/store glibc 2.41 (interp =
# /td/store/ld, RUNPATH = $ORIGIN/../lib). The set + its glibc/libgcc closure is interned
# content-addressed at /td/store, and `busybox sh` runs a script that drives `make` in the
# store-ns own-root with /gnu/store ABSENT.
#
# This is the daily-suite harness userland, guix-byte-free by construction: upstream source
# + td's own from-source /td/store toolchain. busybox (a POSIX userland) is deliberate — a
# silent dependency on a GNUism surfaces as a loud failure. (td-builder, the engine, joins
# the set via rust-store-native rung 3; this gate proves the busybox+make half.) HEAVY
# (~90 min from the seed; directive 1 — no cache for the authoritative gate). NOT a BUILD_GATE.
#
# Legs (DURABLE — no guix oracle):
#   [supply-chain] busybox + make tarballs match their lock sha256 (the sha IS the oracle).
#   [provenance]   the built busybox/make carry zero /gnu/store bytes.
#   [no-guix]      the interned /td/store set (bins + td-built glibc/libgcc) has zero
#                  /gnu/store anywhere; the relinked interp is /td/store/ld.
#   [structural]   the tree's lib/ carries the key runtime sonames (libc/libm/libgcc_s);
#                  full closure completeness is proven by the behavioral own-root run.
#   [behavioral]   busybox sh runs + `make --version` runs from /td/store in the store-ns
#                  own-root → the real version strings.
#   [structural]   inside the own-root /td/store IS the store AND /gnu/store is ABSENT.
# Verified-red (in-gate): without the elf-set-interp relink the own-root run FAILS (the
# build-dir interp does not exist in the own-root) — the relink is load-bearing.
set -eu

# --- source the x86_64 toolchain gate as a FUNCTION LIBRARY (build_* rungs + pinned vars) --
export TD_X86_64_LIB=1
. tests/bootstrap-x86_64-toolchain-store-native.sh
unset TD_X86_64_LIB
# in scope: ROOT, fail(), sha(), lf(), make_curated_path, the build_* rungs, KH_X86_64_TB.

# --- [supply-chain] busybox + make tarballs match their lock sha256 -------------------------
BB_LOCK=`ls seed/sources/busybox-*.lock 2>/dev/null | head -1`
test -n "$BB_LOCK" || fail "no seed/sources/busybox-*.lock pin"
BB_TB=".td-build-cache/sources/`lf "$BB_LOCK" file`"
test -f "$BB_TB" || fail "warmed $BB_TB absent — run 'td-feed warm sources' (host PREP)"
test "`sha "$BB_TB"`" = "`lf "$BB_LOCK" sha256`" || fail "warmed $BB_TB sha256 != lock pin"
MK_LOCK=`ls seed/sources/make-4.4*.lock 2>/dev/null | head -1`
test -n "$MK_LOCK" || fail "no seed/sources/make-4.4*.lock pin"
MK_TB=".td-build-cache/sources/`lf "$MK_LOCK" file`"
test -f "$MK_TB" || fail "warmed $MK_TB absent — run 'td-feed warm sources' (host PREP)"
test "`sha "$MK_TB"`" = "`lf "$MK_LOCK" sha256`" || fail "warmed $MK_TB sha256 != lock pin"
echo "   [supply-chain] busybox + make-4.4.1 match their lock sha256 — upstream bytes, not guix"

# An x86_64 cc wrapper that builds RUNNABLE binaries (interp = the build-dir glibc loader, so
# configure tests + build-time tools run now) + RUNPATH $ORIGIN/../lib (so the shipped tree
# finds its libs). The final binary's interp is relinked to /td/store/ld afterward.
#   $1=outfile $2=XGCC2 $3=XGLIBC $4=XLIBGCCDIR ; reads $KHINC (Linux UAPI headers root)
# -idirafter "$KHINC": the glibc component ships glibc's own headers but NOT the Linux UAPI
# headers (linux/*, asm/* — they live in the build sysroot, not the glibc install), yet glibc's
# bits/local_lim.h #includes <linux/limits.h>. Add the warmed x86_64 kernel headers AFTER the
# system dirs so glibc's own headers win and the kernel ones only fill in linux/*, asm/*.
emit_cc() {
  csh=`command -v bash 2>/dev/null || command -v sh`
  # interp + rpath = the ABSOLUTE build-dir glibc/libgcc, so the test/built binaries RUN at build
  # time with NO LD_LIBRARY_PATH (which would poison the host build-driver gawk → SIGFPE). The
  # assemble step relinks interp → /td/store/ld and rpath → $ORIGIN/../lib for the shipped layout.
  printf '#!%s\nexec "%s/bin/%s-gcc" -isystem "%s/include" -idirafter "%s" -B"%s/lib" -L"%s/lib" -L"%s" -Wl,--dynamic-linker -Wl,"%s/lib/ld-linux-x86-64.so.2" -Wl,-rpath -Wl,"%s/lib:%s" "$@"\n' \
    "$csh" "$2" "$XTARGET" "$3" "$KHINC" "$3" "$3" "$4" "$3" "$3" "$4" > "$1"
  chmod 0555 "$1"
}

rewrite_bin_sh_shebangs() {
  _rbs_dir=$1
  _rbs_shell=$2
  "$TB" files "$_rbs_dir" | while IFS= read -r _rbs_f; do
    [ -f "$_rbs_f" ] || continue
    IFS= read -r _rbs_first < "$_rbs_f" || continue
    case "$_rbs_first" in
      '#!'*'/bin/sh'*)
        _rbs_tmp="$_rbs_f.td-shebang.$$"
        { printf '#!%s\n' "$_rbs_shell"; tail -n +2 "$_rbs_f"; } > "$_rbs_tmp" && mv "$_rbs_tmp" "$_rbs_f"
        ;;
    esac
  done
}

pin_autotools_mtime() {
  _pam_dir=$1
  "$TB" files "$_pam_dir" | while IFS= read -r _pam_f; do
    case "$_pam_f" in
      *.in|*.am|*.ac|*.m4|*/configure) touch -t 202601010101 "$_pam_f" ;;
    esac
  done
}

# build_make_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — GNU make 4.4.1, the
# build driver. Configure+build with the runnable cc; output: $out/make (interp relinked later).
build_make_x86_64() {
  mc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  src=`mktemp -d`/make; mkdir -p "$src"
  tar -xzf "$MK_TB" -C "$src" --strip-components=1 || { echo "make unpack failed" >&2; return 1; }
  # The sandbox has NO /bin/sh: run configure THROUGH the curated shell (its #!/bin/sh shebang
  # would otherwise fail "No such file or directory"), and rewrite any #!/bin/sh helper shebangs.
  rewrite_bin_sh_shebangs "$src" "$csh" || true
  # Tarball mtime ordering makes `make` try to re-run automake/autoconf (absent → Error 127).
  # Pin all autotools build-system files to ONE mtime so none is strictly newer than another
  # (a target is only rebuilt when a prerequisite is *strictly* newer) → no regeneration.
  pin_autotools_mtime "$src" || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc" "$xg" "$xgl" "$xlg"
  ( cd "$src"; bp="$xb/bin:$XBIN:$mc"   # $XBIN = the cross-fns _xbin scaffolding (awk/m4/bison/flex/cmp/...); target binaries find glibc via the absolute build-dir rpath the cc wrapper bakes (NO LD_LIBRARY_PATH — it would poison the host gawk)
    env PATH="$bp" CC="$wb/cc" CPP="$wb/cc -E" CONFIG_SHELL="$csh" SHELL="$csh" "$csh" ./configure --build="$XTARGET" --host="$XTARGET" --disable-dependency-tracking >cfg.log 2>&1 \
      || { echo "make configure failed" >&2; cp cfg.log "$ROOT/.td-build-cache/_makex-cfg.log" 2>/dev/null||true; cp config.log "$ROOT/.td-build-cache/_makex-config.log" 2>/dev/null||true; echo "--- config.log tail ---" >&2; tail -60 config.log >&2; return 1; }
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= make SHELL="$csh" CONFIG_SHELL="$csh" >build.log 2>&1 \
      || { echo "make build failed" >&2; cp build.log "$ROOT/.td-build-cache/_makex-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a make "$out/make" ) || return 1
  test -x "$out/make" || { echo "no x86_64 make produced" >&2; return 1; }
}

# build_busybox_x86_64 <cpath> <xgcc2> <xglibc> <xlibgccdir> <xbu> <out> — busybox 1.37.0
# (dynamic). build-host == target (both x86_64), so HOSTCC == CC (the runnable wrapper);
# CONFIG_STATIC off (dynamic vs /td/store glibc). Output: $out/busybox (interp relinked later).
build_busybox_x86_64() {
  mc=$1; xg=$2; xgl=$3; xlg=$4; xb=$5; out=$6
  rm -rf "$out"; mkdir -p "$out"
  csh=`command -v bash 2>/dev/null || command -v sh`
  bz=`command -v bzip2 2>/dev/null || ls /gnu/store/*bzip2*/bin/bzip2 2>/dev/null | head -1`
  test -n "$bz" || { echo "no bzip2 to unpack busybox" >&2; return 1; }
  src=`mktemp -d`/bb; mkdir -p "$src"
  "$bz" -dc "$BB_TB" | tar -xf - -C "$src" --strip-components=1 || { echo "busybox unpack failed" >&2; return 1; }
  # The sandbox has NO /bin/sh: busybox's Kbuild + gen scripts (#!/bin/sh) would fail. Rewrite the
  # shebangs to the curated shell and pass SHELL/CONFIG_SHELL to every make so recipes use it too.
  rewrite_bin_sh_shebangs "$src" "$csh" || true
  wb=`mktemp -d`/wb; mkdir -p "$wb"; emit_cc "$wb/cc" "$xg" "$xgl" "$xlg"
  ( cd "$src"; bp="$xb/bin:$XBIN:$mc"   # $XBIN = the cross-fns _xbin scaffolding (awk/m4/bison/flex/cmp/...); target binaries find glibc via the absolute build-dir rpath the cc wrapper bakes (NO LD_LIBRARY_PATH — it would poison the host gawk)
    env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh" CONFIG_SHELL="$csh" defconfig >cfg.log 2>&1 \
      || { echo "busybox defconfig failed" >&2; tail -20 cfg.log >&2; return 1; }
    # dynamic (not CONFIG_STATIC), non-PIE, point the linker at the build-dir glibc archives.
    _cfg=.config.td
    while IFS= read -r _line; do
      case "$_line" in
        CONFIG_STATIC*|'# CONFIG_STATIC'*|CONFIG_PIE*|'# CONFIG_PIE'*|CONFIG_EXTRA_LDFLAGS*|'# CONFIG_EXTRA_LDFLAGS'*) continue ;;
        *) printf '%s\n' "$_line" ;;
      esac
    done < .config > "$_cfg"
    mv "$_cfg" .config
    { echo '# CONFIG_STATIC is not set'; echo '# CONFIG_PIE is not set'; echo "CONFIG_EXTRA_LDFLAGS=\"-L$xgl/lib -L$xlg\""; } >> .config
    yes "" | env PATH="$bp" make CC="$wb/cc" HOSTCC="$wb/cc" SHELL="$csh" CONFIG_SHELL="$csh" oldconfig >/dev/null 2>&1
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= \
      make CC="$wb/cc" HOSTCC="$wb/cc" SKIP_STRIP=y SHELL="$csh" CONFIG_SHELL="$csh" -j"$(nproc)" >build.log 2>&1 \
      || { echo "busybox build failed" >&2; cp build.log "$ROOT/.td-build-cache/_bbx-build.log" 2>/dev/null||true; tail -25 build.log >&2; return 1; }
    cp -a busybox "$out/busybox" ) || return 1
  test -x "$out/busybox" || { echo "no x86_64 busybox produced" >&2; return 1; }
}

# ============================================================================================
# Build the i686 base FROM THE SEED, then CROSS UP to x86_64 — REUSING the x86_64 gate's rungs.
# (identical prologue to the rust-x86_64 gate.)
# ============================================================================================
cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
. tests/cache-lib.sh
. tests/x86_64-cross-fns.sh
. tests/x86_64-subst-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
export TD_STORE_DIR=/td/store
snwork=`mktemp -d`
MKX="$snwork/makex"; BBX="$snwork/bbx"
trap 'rm -rf "$snwork"; [ -n "${binsh_made:-}" ] && rm -f /bin/sh' EXIT INT TERM    # the build branch re-traps to also clean its chain temps
cstore="$snwork/closure-store"; cdb="$snwork/closure.db"; mkdir -p "$cstore"
# the static-bash fixture is a DECLARED gate input (#353): the runner resolved it.
bs=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$bs" || fail "TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs"
test -x "$bs/bin/bash" || fail "no static bash fixture at $bs"
bbase=`basename "$bs"`; cp -a "$bs" "$cstore/$bbase"; chmod -R u+w "$cstore"

# --- Get the x86_64 toolchain: FETCH the lock-keyed closure (x64-toolchain-subst, #223) if a
# substitute store is exposed, else BUILD it from the 229-byte seed (directive 1) and export the
# closure for the daily to publish. Either path sets XBU/XGCC2/XGLIBC/XLIBGCCDIR (via _x86_point
# or run_x86_64_cross). The substitute is an optimization, NEVER a correctness dependency.
if x86_64_resolve_closure "$cstore" "$cdb"; then
  echo ">> [subst/SKIP] fetched the x86_64 toolchain closure {binutils,gcc,glibc} — SKIPPED the ~98-min from-seed build"
else
  echo ">> [subst/MISS] no exposed substitute store — building the x86_64 toolchain from the 229-byte seed (directive 1)"
  # i686 base (21 rungs) + the 4 cross rungs are recipes (#378 slice 4); run_x86_64_cross
  # drives the whole graph via build-plan --auto and exports XBU XGCC2 XGLIBC XLIBGCCDIR XSTDCXXDIR.
  run_x86_64_cross "$cpath" || fail "the x86_64 cross toolchain (recipe ladder) failed to build from the seed"
  verify_x86_64_ownroot "$cpath" "$snwork" || fail "the x86_64 own-root verify failed"
  x86_64_build_closure "`pwd`/.td-build-cache/x86_64-closure-export" "$cstore" "$cdb" || fail "could not intern + subst-export the x86_64 toolchain closure"
fi
x86_64_verify_closure "$cpath" "$cstore" "$cdb" "$bbase" || fail "the x86_64 closure toolchain did not compile+run an x86_64 program → 42"
echo "   x86_64 toolchain ready (XGCC2=$XGCC2)"

# --- autoconf/recursive-make scaffolding (awk/m4/bison/flex/cmp/...) via the cross-fns _xbin:
# the SORTED, sandbox-tested build-tool set gate 414 uses (NOT an ad-hoc gawk glob — an
# unsorted pick grabbed a gawk that SIGFPEs in the sandbox). Build-drivers; no output bytes.
XBIN="$snwork/xbin"; _xbin "$XBIN"; export XBIN
test -x "$XBIN/awk" || fail "_xbin produced no awk for the build scaffolding"
# busybox's Kbuild (scripts/gen_build_files.sh) calls find/xargs, which _xbin doesn't carry.
# Use an EXPLICIT, bound /gnu/store findutils (sorted/deterministic) — _store_tool's `command -v`
# is unreliable here and yielded a broken `find` symlink. Build-drivers; no output bytes.
for t in find:findutils xargs:findutils bzip2:bzip2; do
  n=${t%%:*}; pk=${t##*:}
  b=`ls /gnu/store/*-"$pk"-*/bin/"$n" 2>/dev/null | sort | head -1`
  test -n "$b" -a -x "$b" && ln -sf "$b" "$XBIN/$n" || true
done
test -x "$XBIN/find" -a -x "$XBIN/bzip2" || fail "missing find/bzip2 for the busybox Kbuild scaffolding"
# busybox calls binutils by PLAIN name (AR=ar etc.; CROSS_COMPILE empty, host==target) — give it
# the cross binutils tools (x86_64; same arch as the host) under their unprefixed names.
for t in ar nm ranlib objcopy objdump strip size strings; do
  test -x "$XBU/bin/x86_64-pc-linux-gnu-$t" && ln -sf "$XBU/bin/x86_64-pc-linux-gnu-$t" "$XBIN/$t" || true
done
test -x "$XBIN/ar" || fail "no ar for the busybox build"

# --- the x86_64 Linux UAPI headers (warm, pinned) for the cc wrapper's -idirafter (glibc's
# headers #include <linux/*>; the glibc component doesn't carry them) ------------------------
KHINC="$snwork/kh"; mkdir -p "$KHINC"
tar -xzf "$KH_X86_64_TB" -C "$KHINC" || fail "could not extract the x86_64 kernel headers ($KH_X86_64_TB)"
test -f "$KHINC/linux/limits.h" || fail "x86_64 kernel headers missing linux/limits.h after extract"
export KHINC

# --- /bin/sh for popen()/system(): busybox's compiled split-include calls
# popen("find ...") and glibc's popen hardcodes /bin/sh, which the sandbox lacks (the same
# /bin/sh gap, but from a compiled libc call — no shebang to sed). The dev-shell root is a
# writable ephemeral tmpfs, so point /bin/sh at the curated shell: namespace-local, NEVER
# touches the host (the host root was pivoted away). The loop's gates share ONE outer
# host-sandbox, so if WE create it the EXIT trap removes it (else it would leak to later gates).
csh0=`command -v bash 2>/dev/null || command -v sh`
binsh_made=
[ -e /bin/sh ] || { mkdir -p /bin 2>/dev/null && ln -sf "$csh0" /bin/sh && binsh_made=1; }
test -e /bin/sh || fail "could not provide /bin/sh for popen() in the sandbox (root not writable?)"

# --- build the C userland (busybox + make) dynamic vs the x86_64 glibc 2.41 -----------------
build_make_x86_64    "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$MKX" || fail "the cross gcc did not build GNU make 4.4.1"
build_busybox_x86_64 "$cpath" "$XGCC2" "$XGLIBC" "$XLIBGCCDIR" "$XBU" "$BBX" || fail "the cross gcc did not build busybox 1.37.0"
for b in "$MKX/make" "$BBX/busybox"; do
  "$TB" text not-contains '/gnu/store' "$b" || fail "$b contains /gnu/store bytes — not guix-free"
done
echo "   [provenance] built busybox + make carry zero /gnu/store bytes"

# --- assemble the self-contained tree (bins + glibc/libgcc closure in lib/) -----------------
tree="$snwork/tree"; mkdir -p "$tree/bin" "$tree/lib"
cp "$BBX/busybox" "$tree/bin/busybox"; cp "$MKX/make" "$tree/bin/make"
for soname in libc.so.6 libdl.so.2 librt.so.1 libpthread.so.0 libm.so.6 libresolv.so.2; do
  s=`ls "$XGLIBC/lib/$soname" 2>/dev/null | head -1`; test -n "$s" -a -e "$s" && cp -L "$s" "$tree/lib/$soname" || true
done
cp -L "$XLIBGCCDIR/libgcc_s.so.1" "$tree/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1"
chmod -R u+w "$tree"
# relink each executable's interp to /td/store/ld (RUNPATH already $ORIGIN/../lib from the link)
for b in busybox make; do
  "$TB" elf-set-interp "$tree/bin/$b" /td/store/ld || fail "elf-set-interp $b"
  case `"$TB" elf-interp "$tree/bin/$b"` in /td/store/*) ;; *) fail "interp of $b not relinked to /td/store" ;; esac
  # rebase the absolute build-dir rpath onto the shipped layout (glibc co-located in ../lib)
  "$TB" elf-set-rpath "$tree/bin/$b" '$ORIGIN/../lib' || fail "elf-set-rpath $b"
done
# busybox applet symlinks (sh, sed, grep, …) so the userland is callable by name
( cd "$tree/bin"; for a in sh sed grep awk find tar gzip ls cat cp mkdir rm env printf xargs sort head tail wc tr cut; do ln -sf busybox "$a"; done )
echo "   [structural] busybox + make interp relinked to /td/store/ld; applet symlinks placed"

# --- intern the tree at /td/store + place the loader (a SEPARATE userland store under snwork) -
ustore="$snwork/userland-store"; udb="$snwork/userland.db"; mkdir -p "$ustore"
out=`"$TB" store-add-recursive userland-x86_64-store-native "$tree" "$ustore" "$udb"` || fail "store-add-recursive"
case "$out" in /td/store/*-userland-x86_64-store-native) ;; *) fail "interned path not content-addressed under /td/store (got: $out)" ;; esac
phys="$ustore/`basename "$out"`"; rel=${out#/td/store/}
test -x "$phys/bin/busybox" -a -x "$phys/bin/make" || fail "interned tree missing busybox/make"
"$TB" tree-not-contains '/gnu/store' "$phys" || fail "interned set contains /gnu/store"
echo "   [no-guix] interned $out — zero /gnu/store (busybox/make + td-built glibc/libgcc)"
for need in libc.so.6 libm.so.6 libgcc_s.so.1; do ls "$phys"/lib/*"$need"* >/dev/null 2>&1 || fail "interned lib/ missing $need"; done
echo "   [structural] the interned lib/ holds the userland runtime closure"
cp -L "$XGLIBC/lib/ld-linux-x86-64.so.2" "$ustore/ld" || fail "could not place the x86_64 loader at /td/store/ld"
"$TB" text not-contains '/gnu/store' "$ustore/ld" || fail "the /td/store/ld loader contains /gnu/store bytes"

# --- RUN busybox sh + make from /td/store in the store-ns own-root ---------------------------
cat > "$ustore/probe.sh" <<PROBE
export PATH=/td/store/$rel/bin    # the busybox applet symlinks (sh/sed/head/…) live here
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
busybox echo BUSYBOX-RAN
sed --version 2>/dev/null | head -1
make --version > /tmp/mv 2>&1 && echo MAKE-RAN   # MAKE-RAN reflects make's OWN exit (not head's)
head -1 /tmp/mv
PROBE
out2=`"$TB" store-ns "$ustore" -- "/td/store/$rel/bin/busybox" sh /td/store/probe.sh 2>&1` \
  || { printf '%s\n' "$out2" > "$snwork/userland.out"; while IFS= read -r line; do printf '     %s\n' "$line" >&2; done < "$snwork/userland.out"; fail "store-ns userland run exited nonzero"; }
printf '%s\n' "$out2" > "$snwork/userland.out"
while IFS= read -r line; do printf '     %s\n' "$line"; done < "$snwork/userland.out"
"$TB" text line-exact 'BUSYBOX-RAN' "$snwork/userland.out" || fail "busybox did not run from /td/store"
"$TB" text extract-prefix 'GNU Make 4.4' "$snwork/userland.out" >/dev/null || fail "make did not print its version from /td/store"
"$TB" text line-exact 'MAKE-RAN' "$snwork/userland.out" || fail "make --version did not run cleanly from /td/store"
echo "   [behavioral] busybox + make RAN from /td/store in the store-ns own-root → GNU Make 4.4.1"
"$TB" text line-exact 'GNU-ABSENT' "$snwork/userland.out" || fail "/gnu/store is PRESENT in the own-root"
echo "   [structural] inside td's own root /td/store IS the store AND /gnu/store is ABSENT"

# --- Increment 3: stage the C TOOLCHAIN into the harness so the guix-free loop can BUILD
# software, not just text. The closure {binutils, gcc, glibc} is at its lock-keyed /td/store
# paths in $cstore (fetched or built above; x86_64_verify_closure already proved it compiles a
# program → 42 in an own-root). Copy the three components alongside the userland set so the
# harness store carries a WORKING compiler, and record a manifest the check-harness compile leg
# reads. guix-byte-free (verified above); a paranoia grep guards a smuggled /gnu/store byte.
gccb=`basename "$XGCC2"`; glibcb=`basename "$XGLIBC"`; bub=`basename "$XBU"`
for comp in "$XBU" "$XGCC2" "$XGLIBC"; do
  cb=`basename "$comp"`
  [ -e "$ustore/$cb" ] || cp -a "$comp" "$ustore/$cb" || fail "could not stage toolchain component $cb into the harness store"
done
chmod -R u+w "$ustore"
# guix-byte-free check on the COMPILE-PATH binaries (as x86_64_verify_closure does), NOT a
# recursive grep — the latter reds on seed utility SCRIPTS (gcc install-tools, glibc mtrace
# shebangs) that are scaffolding, not the deliverable ([[td-x86-64-fetch-path-gotchas]]).
_xcc1=`"$TB" files-name-first cc1 "$ustore/$gccb"`
for _b in "$ustore/$glibcb/lib/libc.so.6" "$ustore/$gccb/bin/$XTARGET-gcc" "$_xcc1"; do
  { [ -n "$_b" ] && [ -e "$_b" ]; } || fail "staged toolchain missing a compile-path binary ($_b)"
  "$TB" text not-contains '/gnu/store' "$_b" || fail "staged harness toolchain binary $_b contains /gnu/store bytes"
done
test -x "$ustore/$gccb/bin/$XTARGET-gcc" || fail "staged toolchain missing $XTARGET-gcc"
echo "   [inc3] staged the C toolchain {binutils,gcc,glibc} into the harness — the guix-free loop can now COMPILE (not just drive text)"

# --- inc2c: PERSIST the validated /td/store harness for the guix-free check tier --------------
# Copy the harness (busybox+make + the staged C toolchain + glibc/libgcc closure + the
# /td/store/ld loader) OUT of the ephemeral $snwork into a stable cache the loop consumes via
# `./check.sh check-harness` (host-sandbox --store-from <here> --store-at /td/store --no-daemon,
# guix ABSENT). The guix-less daily VM SHIPS this dir; the capture half (this gate) needs a guix
# host, the consume half touches no guix. Atomic-ish: assemble beside, then swap into place.
hdir="$ROOT/.td-build-cache/harness"; htmp="$hdir.tmp.$$"
rm -rf "$htmp" "$hdir.old"; mkdir -p "$htmp/store"
cp -a "$ustore/." "$htmp/store/" || fail "could not copy the harness store to the cache"
printf '%s\n' "$rel" > "$htmp/rel"
# The compile leg (tests/harness-loop.sh) reads this manifest for the staged toolchain paths.
{ printf 'HT_TARGET=%s\n' "$XTARGET"; printf 'HT_GCC=%s\n' "$gccb"; \
  printf 'HT_GLIBC=%s\n' "$glibcb"; printf 'HT_BU=%s\n' "$bub"; } > "$htmp/toolchain"
[ -d "$hdir" ] && mv "$hdir" "$hdir.old"
mv "$htmp" "$hdir"; rm -rf "$hdir.old"
test -x "$hdir/store/$rel/bin/busybox" -a -x "$hdir/store/$rel/bin/make" -a -e "$hdir/store/ld" \
  || fail "persisted harness at $hdir is incomplete"
test -x "$hdir/store/$gccb/bin/$XTARGET-gcc" -a -s "$hdir/toolchain" \
  || fail "persisted harness is missing the staged C toolchain / manifest"
echo "   [inc2c] persisted the /td/store harness + C toolchain to .td-build-cache/harness (consumed by ./check.sh check-harness)"

echo "PASS: userland-x86_64-store-native — from the 229-byte seed, td built the i686 chain → gcc 14.3.0,"
echo "  crossed up to x86_64, and built busybox 1.37.0 + GNU make 4.4.1 from upstream source, DYNAMIC vs the"
echo "  /td/store glibc 2.41, interned at /td/store, and RAN them in the store-ns own-root → /gnu/store ABSENT,"
echo "  zero guix bytes. The C userland of the guix-less daily-suite captured set — plus the staged C toolchain"
echo "  {binutils,gcc,glibc} (Increment 3) so the guix-free check-harness loop COMPILES + runs real software."
