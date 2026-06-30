#!/bin/sh
# tests/bootstrap-patch.sh — source-bootstrap BRICK 5 (gcc toolchain), make-driven rung: the
# tcc-built GNU Make (brick-5 make rung) drives the seed-built TinyCC over GNU patch 2.5.9 to
# produce a working `patch` — the first program built by make IN the loop sandbox, and the tool
# the binutils/gcc rungs need to apply guix's bootstrap patches. Exactly guix's patch-mesboot.
#
# This rung clears the make-in-sandbox blocker. GNU Make's SHELL makefile-variable defaults to
# /bin/sh, which the loop sandbox does NOT have (no /bin/sh), and make IGNORES the SHELL *env*
# var by design — so recipe execution segfaults. The fix is the make *variable* override
# `make SHELL=<curated sh>` (guix gets /bin/sh for free from gash; td overrides instead). patch
# also needs guix's pch.c "avoid another segfault" workaround. Built serially (guix
# #:parallel-build? #f).
#
# i686, static. Sources (mes + nyacc + tcc + make + patch) are td-fetched, not vendored.
#
# Legs (DURABLE):
#   [pinned-input] mes + nyacc + tcc + make + patch tarballs match their lock sha256.
#   [no-guix]      built on a curated PATH with gcc/g++/cc/guile/guix DENIED; no /gnu/store in patch.
#   [behavioral]   the tcc-built make builds patch (a multi-file C program) in the sandbox; the
#                  patch binary RUNS, reports 2.5.9, and actually applies a unified diff. This is
#                  the make-in-sandbox blocker turned into a passing test.
#   [repro]        two independent patch builds (same dir) yield a byte-identical patch.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }
STAGE0=seed/stage0
A=AMD64

# --- [pinned-input] mes + nyacc + tcc + make + patch tarballs match their locks ----------------
lf() { sed -n "s/^$2 //p" "$1" | head -1; }
MES_LOCK=`ls seed/sources/mes-*.lock | head -1`;   NYACC_LOCK=`ls seed/sources/nyacc-*.lock | head -1`
TCC_LOCK=`ls seed/sources/tcc-0.9.26*.lock | head -1`; MAKE_LOCK=`ls seed/sources/make-*.lock | head -1`
PATCH_LOCK=`ls seed/sources/patch-*.lock | head -1`
for l in "$MES_LOCK" "$NYACC_LOCK" "$TCC_LOCK" "$MAKE_LOCK" "$PATCH_LOCK"; do test -n "$l" || fail "missing a seed/sources/*.lock"; done
MES_TB=".td-build-cache/sources/`lf "$MES_LOCK" file`";     NYACC_TB=".td-build-cache/sources/`lf "$NYACC_LOCK" file`"
TCC_TB=".td-build-cache/sources/`lf "$TCC_LOCK" file`";     MAKE_TB=".td-build-cache/sources/`lf "$MAKE_LOCK" file`"
PATCH_TB=".td-build-cache/sources/`lf "$PATCH_LOCK" file`"
for pair in "$MES_TB:`lf "$MES_LOCK" sha256`" "$NYACC_TB:`lf "$NYACC_LOCK" sha256`" "$TCC_TB:`lf "$TCC_LOCK" sha256`" \
            "$MAKE_TB:`lf "$MAKE_LOCK" sha256`" "$PATCH_TB:`lf "$PATCH_LOCK" sha256`"; do
  f=${pair%:*}; want=${pair##*:}
  test -f "$f" || fail "pinned tarball not warm ($f) — run 'td-feed warm sources'"
  test "`sha "$f"`" = "$want" || fail "warmed $f sha256 != lock pin ($want)"
done
echo "   [pinned-input] td-fetched mes + nyacc + tcc + make + patch tarballs match their lock sha256"

# --- curated build-driver PATH (gcc/cc/guile/guix DENIED) -------------------------------------
make_curated_path() {
  cdir=`mktemp -d`/bin; mkdir -p "$cdir"; oldifs=$IFS; IFS=:
  for d in $PATH; do [ -d "$d" ] || continue; for f in "$d"/*; do b=`basename "$f"`
    case "$b" in gcc|g++|cc|c++|cpp|gcc-*|g++-*|clang|clang*|tcc|guile|guild|guile-*|guix|guix-*) continue ;; esac
    [ -e "$cdir/$b" ] || ln -s "$f" "$cdir/$b" 2>/dev/null || true; done; done
  IFS=$oldifs; echo "$cdir"
}
# --- seed toolchain (brick 0+1) + canonical seedbin -------------------------------------------
build_toolchain() {
  tc=`mktemp -d`; cp -a "$STAGE0/." "$tc/"
  chmod +x "$tc/bootstrap-seeds/POSIX/$A/hex0-seed" "$tc/bootstrap-seeds/POSIX/$A/kaem-optional-seed"
  mkdir -p "$tc/$A/artifact" "$tc/$A/bin"
  ( cd "$tc" && env -i ./bootstrap-seeds/POSIX/$A/kaem-optional-seed ./$A/mescc-tools-seed-kaem.kaem \
      && env -i ./$A/artifact/kaem-0 ./$A/mescc-tools-mini-kaem.kaem ) >/dev/null 2>&1 \
    || { echo "seed toolchain build failed" >&2; return 1; }
  echo "$tc"
}
seedbin_for() {
  tc=$1; sb=`mktemp -d`/seedbin; mkdir -p "$sb"
  ln -sf "$tc/$A/artifact/M2" "$sb/M2-Planet"; ln -sf "$tc/$A/artifact/blood-elf-0" "$sb/blood-elf"
  ln -sf "$tc/$A/bin/M1" "$sb/M1"; ln -sf "$tc/$A/bin/hex2" "$sb/hex2"; ln -sf "$tc/$A/bin/kaem" "$sb/kaem"; echo "$sb"
}
# --- build + install Mes (i686); returns the prefix (mescc + libc+tcc.a + modules) -------------
build_mes_prefix() {
  tc=$1; cpath=$2; sb=`seedbin_for "$tc"`; M1B="$tc/$A/bin/M1"; HEX2B="$tc/$A/bin/hex2"; BE="$tc/$A/artifact/blood-elf-0"
  work=`mktemp -d`; tar -xzf "$MES_TB" -C "$work"; m="$work/`tar -tzf "$MES_TB" | head -1 | cut -d/ -f1`"
  tar -xzf "$NYACC_TB" -C "$work"; ny="$work/`tar -tzf "$NYACC_TB" | head -1 | cut -d/ -f1`"
  GLP="$ny/module:$m/mes/module:$m/module"
  ( cd "$m"; bp="$sb:$cpath"
    PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
      GUILE=true CC= MES_FOR_BUILD=mes bash configure.sh --prefix="$m/out" --host=i686-linux-gnu >cfg.log 2>&1 || { echo "mes configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    for step in bootstrap install; do
      PATH="$bp" GUILE_LOAD_PATH="$GLP" MES_PREFIX="$m" MES_ARENA=100000000 MES_MAX_ARENA=100000000 MES_STACK=8000000 \
        GUILE=true MES_FOR_BUILD=mes M1="$M1B" HEX2="$HEX2B" BLOOD_ELF="$BE" sh "$step.sh" >"$step.log" 2>&1 || { echo "mes $step failed" >&2; tail -8 "$step.log" >&2; exit 1; }
    done ) || return 1
  prefix="$m/out"; gsd=`ls -d "$prefix"/share/guile/site/* 2>/dev/null | head -1`
  mkdir -p "$gsd"; cp -a "$prefix/share/mes/module/." "$gsd/" 2>/dev/null; cp -a "$ny/module/." "$gsd/" 2>/dev/null
  test -x "$prefix/bin/mescc" -a -s "$prefix/lib/x86-mes/libc+tcc.a" || { echo "mes install incomplete" >&2; return 1; }
  echo "$prefix"
}
# --- build tcc (brick 4) at a given dir; leaves crt1.o/crti.o/crtn.o/libc.a + tcc there ---------
build_tcc() {
  tc=$1; cpath=$2; mesp=$3; t=$4; sb=`seedbin_for "$tc"`
  ln -sf "$mesp/bin/mescc" "$sb/mescc"; ln -sf "$mesp/bin/mes" "$sb/mes"
  NYM=`ls -d "$mesp"/share/guile/site/*/nyacc 2>/dev/null | head -1`; NYM="${NYM%/nyacc}"
  rm -rf "$t"; mkdir -p "$t"; tar -xzf "$TCC_TB" -C "$t" --strip-components=1
  ( cd "$t"; sed -i 's/volatile//' conftest.c 2>/dev/null || true; bp="$sb:$cpath"
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
      sh configure --cc=mescc --prefix="$t/out" --elfinterp=/lib/mes-loader --crtprefix=. --tccdir=. >cfg.log 2>&1 || { echo "tcc configure failed" >&2; tail -5 cfg.log >&2; exit 1; }
    env PATH="$bp" MES_PREFIX="$mesp" GUILE_LOAD_PATH="$NYM" host=i686-linux-gnu ONE_SOURCE=true prefix="$t/out" \
        MES_ARENA=20000000 MES_MAX_ARENA=20000000 MES_STACK=6000000 \
      sh bootstrap.sh >boot.log 2>&1 || { echo "tcc bootstrap failed" >&2; tail -10 boot.log >&2; exit 1; }
  ) || return 1
  test -x "$t/tcc" || { echo "no tcc produced" >&2; return 1; }
}
# --- build GNU Make with tcc, at a CALLER-GIVEN dir (brick-5 make rung) -------------------------
build_make() {
  tc=$1; cpath=$2; mesp=$3; tccd=$4; mk=$5
  rm -rf "$mk"; mkdir -p "$mk"; tar -xzf "$MAKE_TB" -C "$mk" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$mk/"
  mkdir -p "$mk/bin"; ln -sf "$tccd/tcc" "$mk/bin/tcc"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  ( cd "$mk"; bp="$mk/bin:$cpath"
    csh=`PATH="$bp" command -v sh`
    sed -i 's/@LIBOBJS@/getloadavg.o/; s/@REMOTE@/stub/' build.sh.in
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./configure "CC=tcc -static -L. -I$inc1 -I$inc2" "CPP=tcc -E -I$inc1 -I$inc2" LD=tcc \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "make configure failed" >&2; tail -6 cfg.log >&2; exit 1; }
    sed -i 's,^extern long int lseek.*,// &,' make.h 2>/dev/null || true
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./build.sh >build.log 2>&1 || { echo "make build.sh failed" >&2; tail -8 build.log >&2; exit 1; }
  ) || return 1
  test -x "$mk/make" || { echo "no make binary produced" >&2; return 1; }
}
# --- build GNU patch with the tcc-built make, at a CALLER-GIVEN dir (re-extracted for repro) ----
# guix's patch-mesboot: tcc compiles patch, GNU Make drives it. The loop sandbox has no /bin/sh,
# and make ignores the SHELL *env* var (uses the SHELL makefile variable, default /bin/sh) — so we
# override it on the make command line (`make SHELL=<curated sh>`). pch.c gets guix's "avoid
# another segfault" workaround. Serial (no -j), like guix's #:parallel-build? #f.
build_patch() {
  cpath=$1; mesp=$2; tccd=$3; mk=$4; pd=$5
  rm -rf "$pd"; mkdir -p "$pd/bin"; tar -xzf "$PATCH_TB" -C "$pd" --strip-components=1
  cp "$tccd"/crt1.o "$tccd"/crti.o "$tccd"/crtn.o "$tccd"/libc.a "$tccd"/libtcc1.a "$pd/"
  ln -sf "$tccd/tcc" "$pd/bin/tcc"; ln -sf "$mk/make" "$pd/bin/make"
  inc1="$mesp/include"; inc2="$mesp/include/x86"
  # guix's pch.c workaround ("avoid another segfault"): force the p_end loop off.
  sed -i 's/^    while (p_end >= 0) {/    p_end = -1;\n    while (0) {/' "$pd/pch.c"
  ( cd "$pd"; bp="$pd/bin:$cpath"
    csh=`PATH="$bp" command -v sh`
    env PATH="$bp" CONFIG_SHELL="$csh" "$csh" ./configure "CC=tcc -static -L. -I$inc1 -I$inc2" \
        "CPP=tcc -E -I$inc1 -I$inc2" "AR=tcc -ar" LD=tcc \
        --build=i686-unknown-linux-gnu --host=i686-unknown-linux-gnu --disable-nls >cfg.log 2>&1 \
      || { echo "patch configure failed" >&2; tail -8 cfg.log >&2; exit 1; }
    # Two make-in-sandbox fixes:
    #  (1) SHELL as a make *variable* (not env) — make ignores the SHELL env var, defaults to the
    #      absent /bin/sh, so recipes need the curated sh passed as a make variable.
    #  (2) CLEAR the inherited make env. The gate runs INSIDE the loop's outer `make -j2
    #      --output-sync=target`, which exports MAKEFLAGS (the jobserver fds + --output-sync) and
    #      MAKELEVEL. The minimal mes-libc make segfaults trying to honor an inherited jobserver, so
    #      we wipe MAKEFLAGS/MFLAGS/MAKELEVEL/GNUMAKEFLAGS for this nested, serial make.
    env PATH="$bp" MAKEFLAGS= MFLAGS= GNUMAKEFLAGS= MAKELEVEL= MAKE_TERMOUT= MAKE_TERMERR= \
        "$mk/make" SHELL="$csh" CONFIG_SHELL="$csh" \
        "CC=tcc -static -L. -I$inc1 -I$inc2" "AR=tcc -ar" >build.log 2>&1 \
      || { echo "patch make failed" >&2; tail -12 build.log >&2; exit 1; }
  ) || return 1
  test -x "$pd/patch" || { echo "no patch binary produced" >&2; return 1; }
}

cpath=`make_curated_path`
for bad in gcc g++ cc guile guix; do test ! -e "$cpath/$bad" || fail "curated PATH still exposes '$bad'"; done
tc=`build_toolchain` || fail "the seed toolchain (brick 0+1) did not build"
mesp=`build_mes_prefix "$tc" "$cpath"` || fail "Mes (MesCC self-host) did not build/install"
TCCD=`mktemp -d`/tcc; build_tcc "$tc" "$cpath" "$mesp" "$TCCD" || fail "MesCC did not build tcc"
MK=`mktemp -d`/makebuild; build_make "$tc" "$cpath" "$mesp" "$TCCD" "$MK" || fail "tcc did not build GNU Make"
PD=`mktemp -d`/patchbuild; build_patch "$cpath" "$mesp" "$TCCD" "$MK" "$PD" || fail "the tcc-built make did not build patch"
trap 'rm -rf "$tc" "$mesp" "`dirname "$TCCD"`" "`dirname "$MK"`" "`dirname "$PD"`" "`dirname "$cpath"`"' EXIT INT TERM

# --- [no-guix] -------------------------------------------------------------------------------
PATCH="$PD/patch"
if grep -q -a '/gnu/store' "$PATCH"; then fail "patch contains /gnu/store bytes"; fi
echo "   [no-guix] seed → Mes → MesCC → tcc → make → patch built with no gcc/guile/guix on PATH; no /gnu/store in patch"

# --- [behavioral] make built patch in the sandbox; patch runs, reports 2.5.9, and applies a diff -
head -c20 "$PATCH" | od -An -tx1 | grep -q '7f 45 4c 46 01' || fail "patch is not a 32-bit ELF"
ver=`env -i "$PATCH" --version 2>"$PD/run.err" | head -1` || { tail -3 "$PD/run.err" >&2; fail "the tcc-built patch did not run"; }
echo "$ver" | grep -q '2.5.9' || fail "patch --version gave [$ver], want a 2.5.9 banner"
# the durable proof patch DOES ITS JOB: apply a unified diff and check the result.
wd=`mktemp -d`; printf 'alpha\nbeta\ngamma\n' > "$wd/f.txt"
cat > "$wd/f.diff" <<'DIFF'
--- f.txt
+++ f.txt
@@ -1,3 +1,3 @@
 alpha
-beta
+BETA
 gamma
DIFF
( cd "$wd" && env -i "$PATCH" -p0 f.txt < f.diff ) >"$wd/p.log" 2>&1 || { tail -5 "$wd/p.log" >&2; rm -rf "$wd"; fail "patch could not apply a unified diff"; }
grep -q '^BETA$' "$wd/f.txt" || { rm -rf "$wd"; fail "patch ran but did not apply the diff (no BETA in f.txt)"; }
rm -rf "$wd"
echo "   [behavioral] the tcc-built make built patch in the sandbox; patch (32-bit i386 ELF) reports '$ver' and applied a unified diff — the make-in-sandbox blocker is cleared"

# --- [repro] a second independent patch build (same dir) is byte-identical ----------------------
sha1=`sha "$PATCH"`
build_patch "$cpath" "$mesp" "$TCCD" "$MK" "$PD" || fail "the second patch build did not run"
test "$sha1" = "`sha "$PATCH"`" || fail "patch is NOT reproducible — r1=$sha1 r2=`sha "$PATCH"`"
echo "   [repro] two independent patch builds produce a byte-identical patch (reproducible)"

echo "PASS: source-bootstrap brick 5 (make-driven rung) — from the 229-byte seed, the tcc-built GNU"
echo "      Make compiled GNU patch 2.5.9 IN the loop sandbox (SHELL override clears the no-/bin/sh"
echo "      segfault); patch runs + applies a diff, no gcc/guile/guix, no /gnu/store, reproducible."
echo "      binutils-mesboot0 (which patch's apply step + make build) is next."
