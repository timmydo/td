#!/bin/sh
# tests/rust-store-native.sh — RELINK the upstream Rust toolchain to /td/store with td's OWN
# ELF rewriter (no patchelf), intern it GUIX-FREE, and assert the durable STRUCTURAL +
# SUPPLY-CHAIN legs. The bytes are the upstream Rust release tarball (static.rust-lang.org,
# pinned in seed/sources/rust-*.lock) — NOT a guix build — so this eliminates guix at the
# source rather than relabeling guix bytes.
#
# Legs:
#   [supply-chain]  the warmed tarball's sha256 == the lock pin (upstream Rust, not guix).
#   [provenance]    the upstream rustc/cargo carry ZERO /gnu/store bytes.
#   [relink]        td's OWN `elf-set-interp` retargets PT_INTERP to /td/store (no patchelf).
#   [structural]    after relink rustc/cargo interp ∈ /td/store; interned guix-free via
#                   store-add-recursive into a content-addressed /td/store path with NO
#                   /gnu/store anywhere.
#   [PENDING glibc-final]  the /td/store-RUNTIME leg (populate lib/ with the /td/store
#                   glibc>=2.17 + libgcc_s, RUN rustc from the store-ns own-root) is BLOCKED
#                   on the gcc lane's glibc-final — the /td/store glibc is 2.16.0 and rust
#                   needs GLIBC_2.17. Marked explicitly, never faked.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
sha() { sha256sum "$1" | cut -d' ' -f1; }   # diffutils/binutils are absent from the loop sandbox

. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> relink driver (stage0 td-builder, guix-free): $TB"

# --- [supply-chain] the warmed upstream tarball matches the lock sha256 (upstream, not guix) -
LOCK=`ls seed/sources/rust-*.lock 2>/dev/null | head -1`
test -n "$LOCK" || fail "no seed/sources/rust-*.lock pin"
SHA=`sed -n 's/^sha256 //p' "$LOCK" | head -1`
FILE=`sed -n 's/^file //p' "$LOCK" | head -1`
TARBALL=".td-build-cache/sources/$FILE"
test -f "$TARBALL" || fail "warmed $TARBALL absent — run td-feed warm sources (host PREP)"
test "`sha "$TARBALL"`" = "$SHA" || fail "warmed $TARBALL sha256 != lock pin ($SHA) — corrupt fetch or stale lock"
echo "   [supply-chain] $FILE matches the lock sha256 ($SHA) — upstream Rust bytes, not guix"

scratch=`mktemp -d "${TMPDIR:-/tmp}/rust-store-native.XXXXXX"`
trap 'rm -rf "$scratch"' EXIT
top="${FILE%.tar.gz}"

# extract a lean but representative subset: the rustc + cargo binaries and rustc's own .so
# libs (skip the bulky rustlib std archives — the structural relink does not need them).
tar -xzf "$TARBALL" -C "$scratch" --exclude="$top/rustc/lib/rustlib" \
  "$top/rustc/bin/rustc" "$top/rustc/lib" "$top/cargo/bin/cargo"
tree="$scratch/tree"; mkdir -p "$tree/bin" "$tree/lib"
cp "$scratch/$top/rustc/bin/rustc" "$tree/bin/rustc"
cp "$scratch/$top/cargo/bin/cargo" "$tree/bin/cargo"
cp "$scratch/$top"/rustc/lib/*.so "$tree/lib/" 2>/dev/null || true

# --- [provenance] the upstream binaries carry NO /gnu/store (the point of upstream-not-guix) -
for b in "$tree/bin/rustc" "$tree/bin/cargo"; do
  n=`grep -c -a '/gnu/store' "$b" || true`
  test "$n" = 0 || fail "$b contains $n /gnu/store reference(s) — not guix-free upstream"
done
echo "   [provenance] upstream rustc + cargo carry zero /gnu/store bytes"

# --- RELINK: td's OWN elf-set-interp retargets the interpreter to /td/store (no patchelf) ----
for b in rustc cargo; do
  "$TB" elf-set-interp "$tree/bin/$b" /td/store/ld || fail "elf-set-interp $b"
done

# --- [structural] interp is now under /td/store, the original /lib64 loader is gone ----------
for b in rustc cargo; do
  i=`"$TB" elf-interp "$tree/bin/$b"`
  case "$i" in
    /td/store/*) ;;
    *) fail "interp of $b not relinked to /td/store (got: $i)" ;;
  esac
done
echo "   [structural] rustc + cargo interp relinked to /td/store (was /lib64/ld-linux-x86-64.so.2)"

# --- intern the relinked tree GUIX-FREE into /td/store via store-add-recursive --------------
# TD_STORE_DIR=/td/store makes the LOGICAL content-addressed path /td/store/<hash>-NAME; the
# bytes land physically under $store (store-ns binds $store at /td/store at runtime — the
# pending leg). Read the interned binaries back from the physical path (gate 398's pattern).
store="$scratch/store"; db="$scratch/db.sqlite"; mkdir -p "$store"
export TD_STORE_DIR=/td/store
out=`"$TB" store-add-recursive rust-1.96.0-store-native "$tree" "$store" "$db"` || fail "store-add-recursive"
case "$out" in
  /td/store/*-rust-1.96.0-store-native) ;;
  *) fail "interned path not content-addressed under /td/store (got: $out)" ;;
esac
phys="$store/`basename "$out"`"
test -d "$phys" || fail "interned tree missing physically at $phys"
echo "   [structural] interned content-addressed at $out (guix-free, td's own store_db)"

# --- [structural] the INTERNED tree has zero /gnu/store anywhere; interp still under /td/store
# (grep -q, not awk: the loop sandbox userland has no awk)
if grep -r -a -q '/gnu/store' "$phys" 2>/dev/null; then
  fail "interned tree contains a /gnu/store reference: `grep -r -a -l '/gnu/store' "$phys" 2>/dev/null | head -1`"
fi
ri=`"$TB" elf-interp "$phys/bin/rustc"`
case "$ri" in /td/store/*) ;; *) fail "interned rustc interp not under /td/store (got: $ri)" ;; esac
echo "   [structural] interned tree: zero /gnu/store, rustc interp ∈ /td/store ($ri)"

# --- [PENDING glibc-final] the /td/store-RUNTIME leg ----------------------------------------
echo "   [PENDING glibc-final] the /td/store-RUNTIME leg (populate lib/ with the /td/store"
echo "      glibc>=2.17 + libgcc_s, RUN rustc from the store-ns own-root with /gnu/store ABSENT)"
echo "      is BLOCKED: the /td/store glibc is 2.16.0, rust needs GLIBC_2.17. Flips green with a"
echo "      relink-target swap when the gcc lane's glibc-final lands."

echo "PASS: rust-store-native — the upstream Rust 1.96.0 toolchain (guix-free bytes, sha==pin,"
echo "  zero /gnu/store) is RELINKED to /td/store by td's OWN ELF rewriter (no patchelf) and"
echo "  interned guix-free; interp ∈ /td/store. Runtime leg pending glibc-final."
