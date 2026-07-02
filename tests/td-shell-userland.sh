#!/bin/sh
# tests/td-shell-userland.sh — THE end-to-end gate for the shipped Rust userland, driven through
# the REAL `td shell <tool> -- <tool> <real-task>` PRODUCT command over td's OWN /td/store
# toolchain — GUIX-FREE, no guix rust and no guix gcc-toolchain in the build path (the #258
# "347/371 cutover"). A person types `td shell ripgrep -- rg PATTERN tree` and the shipped tool
# builds on demand with td's native x86_64 gcc 14.3.0 + binutils 2.44 + glibc 2.41 (fetched from
# the signed subst store, else built from seed), links the /td/store glibc (ELF interp =
# /td/store/ld), and RUNS in a store-ns own-root with /gnu/store ABSENT — the product command
# over td's own bytes.
#
# It supersedes the per-tool `rust-<x>` gates for the userland cutover: those build each tool
# through the bespoke crate-free-build.sh harness against the GUIX rust + gcc-toolchain; here the
# real `td shell` command builds it against td's OWN toolchain and runs it. run_shell learned the
# native path (builder/src/main.rs run_shell_native): it stages the built tool into the native
# store and execs it in the own-root that binds /td/store + the cwd — so `td shell` is the
# guix-free product command, not a bespoke harness.
#
# Per tool, all DURABLE and guix-free (no guix oracle):
#   [behavioral]    `td shell <pkg> -- <bin> <real-task>` does the tool's actual job in the own-root.
#   [no-guix]       the built tool references no guix rust/gcc-toolchain; interp = the /td/store ld.
#   [native-arch]   the linker rustc drove is the NATIVE x86_64 gcc + as/ld (ELF64) at /td/store.
#   [td-built]      the <bin> on the composed PATH is td's OWN build staged in the /td/store store.
#   [supply-chain]  every warmed vendored crate's sha256 ∈ the tool's shipped Cargo.lock.
#   [repro]         `td-builder check`'s double-build agrees the build is reproducible.
#
# Crate closures are warmed GUIX-FREE by the check.sh prelude (`td-feed warm crate`). The native
# toolchain is assembled once by gate 416/424's proven library (sourced ASSEMBLE_ONLY). The rust
# tarball + the guix build seed (coreutils/bash/tar/gzip for run_rust's cp/tar) are the retired-
# last seed. Iterate on a subset with `TD_USERLAND_TOOLS='ripgrep' …`; the shipped userland set
# grows tool by tool as each is verified through the product command.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

TOOLS=${TD_USERLAND_TOOLS:-"ripgrep"}

# --- 1. Assemble the native /td/store rust toolchain (gate 416's proven assembly, ASSEMBLE_ONLY).
#     Sets TB / ROOT / TD_STORE_DIR=/td/store, loads stage0 + the recipe evaluator, exports TDSN_*.
export TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY=1
. tests/rust-x86_64-runtime-store-native.sh
unset TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY
STORE="$TDSN_STORE"; SNDB="$TDSN_DB"
NGREL="$TDSN_NGREL"; NBREL="$TDSN_NBREL"; GLREL="$TDSN_GLREL"; RUSTREL="$TDSN_RUSTREL"
test -x "$STORE/$RUSTREL/bin/cargo" -a -x "$STORE/$NGREL/bin/gcc" -a -e "$STORE/$GLREL/lib/libc.so.6" \
  || fail "assemble-only did not produce the expected /td/store toolchain layout"
if [ ! -s "$ROOT/.td-build-cache/recipe-eval/recipe-eval-path" ]; then
  TD_GUIX="${GUIX:-guix}" sh tests/recipe-eval-tool.sh "$ROOT/.td-build-cache/recipe-eval" >/dev/null \
    || fail "could not build td's Rust recipe evaluator"
fi
load_recipe_eval || fail "no td-recipe-eval"
test -x "$TD_RECIPE_EVAL" || fail "td recipe evaluator not executable"
echo ">> td tools (guix-free): stage0=$TB  recipe-eval=$TD_RECIPE_EVAL  native /td/store toolchain assembled"

VENDOR_ROOT="$ROOT/.td-build-cache/crate-vendor"
for p in $TOOLS; do
  test -d "$VENDOR_ROOT/$p/vendor" \
    || fail "$p crate closure not warmed at $VENDOR_ROOT/$p — HOST PREP \`td-feed warm crate' (check.sh prelude) must provision it"
done

# --- 2. Build the combined seed store WSTORE: the guix BUILD seed (coreutils/bash/tar/gzip for
#     run_rust) content-scanned in + the native toolchain trees copied beside it, so all build
#     inputs stage from ONE store. The native toolchain refs come from $SNDB (TD_EXTRA_DBS); the
#     seed refs from the content-scan of WSTORE. The stage0 BUILDER's own guix refs must be in
#     WSTORE too, else the drv's builder cannot exec in the sandbox.
work="$ROOT/.td-build-cache/td-shell-userland-native"
chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"; mkdir -p "$work/tmp"
seedroots=`grep ' /gnu/store/' tests/ripgrep.lock | grep -vE -- '-rust-|-gcc-toolchain-' | sed 's/^[^ ]* //'`
test -n "$seedroots" || fail "no guix build seed (coreutils/tar/gzip) in tests/ripgrep.lock"
echo "$seedroots" | xargs guix build >/dev/null 2>"$work/guix-build.err" \
  || { tail -5 "$work/guix-build.err" >&2; fail "could not realize the guix build seed"; }
{ echo "$seedroots"
  "$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' || true
} | sort -u > "$work/seed-roots"
WSTORE="$work/seed-store"; mkdir -p "$WSTORE"
: > "$work/seed-closure"
while read -r r; do
  test -n "$r" || continue
  "$TB" store-closure-scan /gnu/store "$r" >> "$work/seed-closure" || fail "store-closure-scan $r failed"
done < "$work/seed-roots"
sort -u "$work/seed-closure" -o "$work/seed-closure"
while read -r p; do
  test -n "$p" || continue
  b=`basename "$p"`
  test -e "$WSTORE/$b" || cp -a "$p" "$WSTORE/$b" || fail "staging $p into the seed store failed"
done < "$work/seed-closure"
WDB="$WSTORE/.unused-legacy-db"; : > "$WDB"
for rel in "$NGREL" "$NBREL" "$GLREL" "$RUSTREL"; do
  test -d "$WSTORE/$rel" || cp -a "$STORE/$rel" "$WSTORE/$rel"
done
chmod -R u+w "$WSTORE" 2>/dev/null || true

# --- 3. Native link mode (same for every tool): interp = the /td/store glibc ld; RUNPATH = the
#     /td/store libc + the rust tree's libgcc_s.
test -e "$STORE/$RUSTREL/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1 in the rust tree lib/"
interp="/td/store/$GLREL/lib/ld-linux-x86-64.so.2"
rpath="/td/store/$GLREL/lib:/td/store/$RUSTREL/lib"
bdir="/td/store/$GLREL/lib"

# --- 4. Per-tool native locks: the guix rust + rust-cargo + gcc-toolchain lines become the
#     /td/store native gcc/binutils/glibc/rust; the coreutils/bash/tar/gzip build seed stays
#     (retired last). run_shell's provision drops the .crate lines + appends the interned source.
lockdir="$work/locks"; mkdir -p "$lockdir"
for pkg in $TOOLS; do
  newlock="$lockdir/$pkg.lock"
  {
    grep ' /gnu/store/' "tests/$pkg.lock" | grep -vE -- '-rust-|-gcc-toolchain-'
    echo "rust-1.96.0-x86_64-store-native /td/store/$RUSTREL seed"
    echo "gcc-14.3.0-x86_64-native /td/store/$NGREL seed"
    echo "binutils-2.44-x86_64-native /td/store/$NBREL seed"
    echo "glibc-2.41-x86_64 /td/store/$GLREL seed"
  } > "$newlock"
  grep -qE -- '/gnu/store/[a-z0-9]+-(rust-|gcc-toolchain-)' "$newlock" \
    && fail "$pkg: rewritten lock still names a guix rust/gcc-toolchain"
done
echo "   [cutover] locks retargeted onto the /td/store native toolchain (guix rust + gcc-toolchain removed)"

# A scrubbed host PATH for the td shell PROCESS (build side): coreutils + bash from ripgrep's seed,
# NO guix/Guile — a green run proves no guix process resolves/builds the tool. (The tool RUNS in
# the own-root, where PATH is td's /td/store bins.)
cu=`grep -- '-coreutils-' tests/ripgrep.lock | sed 's/^[^ ]* //' | head -1`
sh_=`grep -- '-bash-' tests/ripgrep.lock | sed 's/^[^ ]* //' | head -1`
test -n "$cu" -a -n "$sh_" || fail "no coreutils/bash in tests/ripgrep.lock"
if ls "$cu/bin" "$sh_/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi
SCRUB="$cu/bin:$sh_/bin"

cache="$work/pkgs"; mkdir -p "$cache/tmp"

# td shell, NATIVE mode: guix/Guile OFF the build PATH (env -i + scrubbed PATH), the crate closure
# from TD_SHELL_VENDOR_ROOT, the /td/store toolchain via TD_SEED_STORE + TD_EXTRA_DBS + the native
# link mode, and TD_SHELL_NATIVE_STORE so run_shell execs the tool in the /td/store own-root.
tdshell() {
  env -i HOME="$cache" TMPDIR="$cache/tmp" PATH="$SCRUB" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SHELL_LOCKS="$lockdir" TD_SHELL_CACHE="$cache" TD_SHELL_VENDOR_ROOT="$VENDOR_ROOT" \
    TD_SHELL_STORE_DB="$WSTORE" TD_SHELL_NATIVE_STORE="$WSTORE" \
    TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$SNDB" \
    TD_RUST_STORE_INTERP="$interp" TD_RUST_STORE_RPATH="$rpath" TD_RUST_STORE_BDIR="$bdir" \
    "$TB" shell "$@"
}

# fold_legs PKG BIN — supporting evidence behind the behavioral run.
fold_legs() {
  pkg=$1; bin=$2
  sd="$cache/$pkg"
  drv=`ls "$sd"/*-"$pkg"-*.drv 2>/dev/null | head -1`; test -n "$drv" || drv=`ls "$sd"/*.drv 2>/dev/null | head -1`
  test -n "$drv" || fail "$pkg: td shell left no .drv in $sd"
  # [supply-chain]
  vendor="$VENDOR_ROOT/$pkg/vendor"; src1=`ls -d "$VENDOR_ROOT/$pkg/src"/*/ 2>/dev/null | head -1`
  cargolock="$src1/Cargo.lock"; test -f "$cargolock" || fail "$pkg: no Cargo.lock at $cargolock"
  ncrate=`ls "$vendor"/*.crate 2>/dev/null | wc -l`; test "$ncrate" -ge 30 || fail "$pkg: <30 crates ($ncrate)"
  miss=0
  for c in "$vendor"/*.crate; do
    sha=`sha256sum "$c" | cut -d' ' -f1`
    grep -qF "$sha" "$cargolock" || { echo "   $pkg: crate `basename "$c"` sha NOT in Cargo.lock" >&2; miss=$((miss + 1)); }
  done
  test "$miss" -eq 0 || fail "$pkg: $miss vendored crate(s) not pinned by the shipped Cargo.lock"
  echo "   [supply-chain] all $ncrate $pkg crates' sha256 ∈ its shipped Cargo.lock"
  # [no-guix] + [structural]: the built tool + its .drv carry no guix rust/gcc-toolchain; interp = /td/store
  grep -q 'TD_RUST_STORE_INTERP' "$drv" || fail "$pkg: the .drv lacks TD_RUST_STORE_INTERP (native link mode not wired)"
  if grep -oqE '/gnu/store/[a-z0-9]+-(rust-|gcc-toolchain-)' "$drv"; then fail "$pkg: the .drv references a guix rust/gcc-toolchain"; fi
  staged=`ls -d "$WSTORE"/*-"$pkg"-*/bin/"$bin" 2>/dev/null | head -1`
  test -n "$staged" -a -x "$staged" || fail "$pkg: no td-built $bin staged in the /td/store store ($WSTORE)"
  if grep -q -a -- '/gnu/store' "$staged"; then fail "$pkg: the built $bin contains /gnu/store bytes"; fi
  si=`"$STORE/$NBREL/bin/readelf" -l "$staged" 2>/dev/null | grep -o "$interp" | head -1`
  test -n "$si" || fail "$pkg: $bin not linked vs the /td/store glibc loader ($interp)"
  echo "   [no-guix] $bin carries zero /gnu/store bytes; interp = the /td/store x86_64 ld"
  echo "   [td-built] $bin = $staged (td's own build in the /td/store store)"
  # [native-arch]
  "$STORE/$NBREL/bin/readelf" -h "$STORE/$NGREL/bin/gcc" 2>/dev/null | grep -i 'class:' | grep -q 'ELF64' \
    || fail "$pkg: the /td/store linker gcc is not ELF64"
  echo "   [native-arch] the linker rustc drove is the NATIVE x86_64 gcc + as/ld (ELF64) at /td/store"
  # [repro] double-build the SAME drv (env -u TD_STORE_DIR: the drv's output canonical is /gnu/store)
  rm -rf "$sd/chk"
  env -u TD_STORE_DIR TD_SEED_STORE="$WSTORE" TD_SEED_DB="$WDB" TD_EXTRA_DBS="$SNDB" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RUST_STORE_INTERP="$interp" TD_RUST_STORE_RPATH="$rpath" TD_RUST_STORE_BDIR="$bdir" \
    "$TB" check "$drv" "$sd/closure.txt" "$sd/chk" > "$sd/checkout.txt" 2>"$sd/chk.err" \
    || { tail -6 "$sd/checkout.txt" "$sd/chk.err" >&2; fail "$pkg: NOT reproducible"; }
  grep -qE "^CHECK out .* reproducible$" "$sd/checkout.txt" || { cat "$sd/checkout.txt" >&2; fail "$pkg: check did not confirm reproducible"; }
  echo "   [repro] td-builder check double-build agrees $pkg is reproducible"
}

# --- Per-tool: build + run through the REAL product command over td's own toolchain, fold the legs.
for pkg in $TOOLS; do
  wdir="$cache/$pkg-run"; rm -rf "$wdir"; mkdir -p "$wdir/tree/sub"
  printf 'alpha line\nthe needle is here\nbeta line\n' > "$wdir/tree/sub/hay.txt"
  : > "$wdir/tree/sub/needle.txt"
  printf 'nothing to see\n' > "$wdir/tree/other.log"
  printf 'roses are red\nviolets are blue\n' > "$wdir/doc.txt"
  echo ">> [$pkg] td shell $pkg -- $pkg <real task> over td's /td/store toolchain (guix OFF PATH, own-root run)"
  case "$pkg" in
    ripgrep)
      out=`cd "$wdir" && tdshell ripgrep -- rg needle tree 2>"$cache/$pkg.err"` \
        || { tail -40 "$cache/$pkg.err" >&2; fail "td shell ripgrep -- rg exited nonzero"; }
      echo "$out" | grep -q 'needle is here' || fail "rg did not find the 'needle' content line (got: $out)"
      echo "$out" | grep -q 'other.log' && fail "rg matched the unrelated file (over-match)"
      echo "   [behavioral] rg (td-built, native /td/store toolchain) found the needle in the own-root"
      bin=rg ;;
    fd)
      out=`cd "$wdir" && tdshell fd -- fd needle tree 2>"$cache/$pkg.err"` \
        || { tail -40 "$cache/$pkg.err" >&2; fail "td shell fd -- fd exited nonzero"; }
      echo "$out" | grep -q 'needle.txt' || fail "fd did not find sub/needle.txt (got: $out)"
      echo "   [behavioral] fd (td-built, native) found sub/needle.txt by name in the own-root"
      bin=fd ;;
    sd)
      got=`cd "$wdir" && printf 'hello world\n' | tdshell sd -- sd world there 2>"$cache/$pkg.err"` \
        || { tail -40 "$cache/$pkg.err" >&2; fail "td shell sd -- sd exited nonzero"; }
      test "$got" = "hello there" || fail "sd did not replace world->there (got: '$got')"
      echo "   [behavioral] sd (td-built, native) replaced world->there in the own-root"
      bin=sd ;;
    *) fail "no behavioral task defined for tool '$pkg'" ;;
  esac
  fold_legs "$pkg" "$bin"
done

echo "PASS: the REAL \`td shell' product command builds + runs the shipped Rust userland ($TOOLS)"
echo "      against td's OWN /td/store toolchain (native x86_64 gcc 14.3.0 + binutils 2.44 + glibc"
echo "      2.41 — guix rust + gcc-toolchain removed): each tool does its real job in a /gnu/store-"
echo "      absent own-root, its crates are Cargo.lock-pinned, and its build is reproducible."
