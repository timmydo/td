#!/bin/sh
# tests/td-shell-userland.sh — the REAL `td shell` product command over the REAL shipped
# Rust userland, built by td's OWN NATIVE /td/store toolchain. This is the end-to-end
# USE-CASE gate: a person types `td shell ripgrep -- rg PATTERN tree` (and a multi-tool
# `td shell ripgrep fd -- …`) and the shipped tools build on demand — LINKED BY THE NATIVE
# x86_64 /td/store gcc/binutils/glibc + relinked rust, NOT the guix rust/gcc-toolchain — and
# actually do their job, from td's OWN store paths, with no `guix` process anywhere in the
# resolve/build/exec path.
#
# THE CUTOVER (#258 workstream, piece D): until now `td shell` built the userland with the guix
# rust + gcc-toolchain seed. run_shell now builds a vendored rust package with the native
# /td/store toolchain handed in via TD_SHELL_NATIVE_* (this gate pre-provisions it: gate 416's
# assembly builds/fetches the native gcc/binutils/glibc + relinked rust and interns them at
# /td/store; run_shell retargets the seed lock onto them — dropping the guix rust/gcc-toolchain
# — and puts run_rust in native link mode). The guix rust/gcc-toolchain path is RETIRED for the
# product command: a vendored rust build with no native toolchain provisioned is a hard error,
# never a guix-rust fallback. The recipe-owned package checks and the image gate (122) keep
# the bespoke `crate-free-build.sh` harness (a separate capability); this gate is the
# PRODUCT-COMMAND cutover.
#
# The tools RUN on the host PATH the way a user runs them: their interp/RUNPATH are /td/store, so
# the gate exposes the physical native store at /td/store (a symlink on the sandbox's writable
# tmpfs root — the same store the store-ns own-root binds in gate 424, here made resolvable for a
# direct host exec). guix/Guile are SCRUBBED from PATH, so a green run proves no `guix` process
# is in the resolve/build/exec path. Legs (all DURABLE behavioral — no guix oracle):
#
#   A [DURABLE behavioral]   `td shell ripgrep -- rg needle tree` finds the needle line (and
#                            NOT the unrelated file), with guix/Guile SCRUBBED from PATH.
#   B [DURABLE native-linked] the `rg` on the composed PATH is td's OWN build at a td store path
#                            (under the td-shell cache), carries ZERO /gnu/store bytes, and its
#                            interp is the /td/store x86_64 glibc loader (the native cutover).
#   C [DURABLE load-bearing] an unknown package errors ("no td recipe"), it does NOT fall back
#                            to guix.
#   D [DURABLE multi-tool]   `td shell ripgrep fd -- …` composes a real user environment: fd
#                            finds a file by name under the tree and runs rg inside the match —
#                            two td-built, native-linked, guix-free tools cooperating in one shell.
#
# Crate closures (ripgrep + fd) are warmed GUIX-FREE by the check.sh prelude (`td-feed warm
# crate`, the cargo-proxy verifying each .crate sha256 == the crates.io index cksum — the
# upstream pin, NOT a guix artifact) into .td-build-cache/crate-vendor/<pkg>/; the coreutils/
# bash/tar/gzip build seed stays guix-built (retired last by the source bootstrap). HEAVY: the
# native gcc build is ~45 min (from-seed adds the ~98-min cross build); it reuses gate 416's
# assembly. NOT run outside the loop sandbox.
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

# ============================================================================================
# 1. Assemble the native /td/store toolchain — gate 416's proven assembly, sourced ASSEMBLE-ONLY.
#    Sets TB / ROOT / TD_STORE_DIR=/td/store, sources cache-lib + the x86_64 libs, loads stage0,
#    and exports TDSN_* (the interned /td/store toolchain) before its own hello.rs probe.
# ============================================================================================
export TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY=1
. tests/rust-x86_64-runtime-store-native.sh
unset TD_RUST_STORE_NATIVE_ASSEMBLE_ONLY
STORE="$TDSN_STORE"; SNDB="$TDSN_DB"
NGREL="$TDSN_NGREL"; NBREL="$TDSN_NBREL"; GLREL="$TDSN_GLREL"; RUSTREL="$TDSN_RUSTREL"
test -x "$STORE/$RUSTREL/bin/cargo" -a -x "$STORE/$NGREL/bin/gcc" -a -e "$STORE/$GLREL/lib/libc.so.6" \
  || fail "assemble-only did not produce the expected /td/store toolchain layout"

# td-recipe-eval (the guix-free recipe evaluator, recipes/ crate). Warm it directly (like gate
# 424) so this stays a lean HEAVY gate, not the whole build-recipes corpus prelude.
if [ ! -s "$ROOT/.td-build-cache/recipe-eval/recipe-eval-path" ]; then
  TD_GUIX="${GUIX:-guix}" sh tests/recipe-eval-tool.sh "$ROOT/.td-build-cache/recipe-eval" >/dev/null \
    || fail "could not build td's Rust recipe evaluator (recipes/ crate)"
fi
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
test -x "$TD_RECIPE_EVAL" || fail "td recipe evaluator not executable"
echo ">> td tools (guix-free): stage0=$TB  recipe-eval=$TD_RECIPE_EVAL  native toolchain assembled at /td/store"

# The warmed crate closures (host PREP) the rust recipes build from.
VENDOR_ROOT="`pwd`/.td-build-cache/crate-vendor"
for p in ripgrep fd; do
  test -d "$VENDOR_ROOT/$p/vendor" \
    || fail "$p crate closure not warmed at $VENDOR_ROOT/$p — HOST PREP \`td-feed warm crate' (check.sh prelude) must provision it first (the offline gate cannot egress)"
done

LOCK=tests/ripgrep.lock   # ripgrep.lock == fd.lock for the seed; used for the guix build seed + SCRUB

# A scrubbed PATH for the td shell process: coreutils + bash from the pinned seed, NO guix/Guile
# — so a green run PROVES td shell used no guix process. (These retired-last seed bytes are the
# shell the user's command runs IN; the deliverable tools carry no guix — asserted in legs B/D.)
# coreutils + bash are DECLARED gate inputs (#353): resolved by the runner.
cu=${TD_GATE_INPUT_COREUTILS:-}
sh_=${TD_GATE_INPUT_BASH:-}
test -n "$cu" -a -n "$sh_" || { echo "ERROR: TD_GATE_INPUT_{COREUTILS,BASH} unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }
if ls "$cu/bin" "$sh_/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi
SCRUB="$cu/bin:$sh_/bin"

scratch="`pwd`/.td-build-cache/td-shell-userland"
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch/tmp"

# ============================================================================================
# 2. Stage a COMBINED seed+native store for the BUILD (gate 424's form): the guix build seed
#    (coreutils/bash/tar/gzip — cp/chmod/tar for run_rust, retired last) content-scanned into a
#    staging store, with the native /td/store toolchain trees copied in beside it, so ALL build
#    inputs stage from ONE store. run_shell hands this to build-recipe as TD_SHELL_NATIVE_STORE.
# ============================================================================================
GUIX=${GUIX:-guix}
seedroots=`grep ' /gnu/store/' "$LOCK" | grep -vE -- '-rust-|-gcc-toolchain-' | sed 's/^[^ ]* //'`
test -n "$seedroots" || fail "no guix build seed (coreutils/bash/tar/gzip) in $LOCK"
echo "$seedroots" | xargs $GUIX build >/dev/null 2>"$scratch/guix-build.err" \
  || { tail -5 "$scratch/guix-build.err" >&2; fail "could not realize the guix build seed"; }
{ echo "$seedroots"
  "$TB" store-query "$TD_BUILDER_DB" references 2>/dev/null | sed 's/^[^|]*|//' | grep '^/gnu/store/' || true
} | sort -u > "$scratch/seed-roots"
WSTORE="$scratch/seed-store"; mkdir -p "$WSTORE"
: > "$scratch/seed-closure"
while read -r r; do
  test -n "$r" || continue
  "$TB" store-closure-scan /gnu/store "$r" >> "$scratch/seed-closure" || fail "store-closure-scan $r failed"
done < "$scratch/seed-roots"
sort -u "$scratch/seed-closure" -o "$scratch/seed-closure"
while read -r p; do
  test -n "$p" || continue
  b=`basename "$p"`
  test -e "$WSTORE/$b" || cp -a "$p" "$WSTORE/$b" || fail "staging $p into the seed store failed"
done < "$scratch/seed-closure"
# TD_SHELL_NATIVE_DB is the legacy set-together companion; the engine content-scans the store and
# no longer reads it — a placeholder path, never /var/guix/db.
WDB="$WSTORE/.unused-legacy-db"; : > "$WDB"
for rel in "$NGREL" "$NBREL" "$GLREL" "$RUSTREL"; do
  test -d "$WSTORE/$rel" || cp -a "$STORE/$rel" "$WSTORE/$rel"
done
chmod -R u+w "$WSTORE/$NGREL" "$WSTORE/$NBREL" "$WSTORE/$GLREL" "$WSTORE/$RUSTREL" 2>/dev/null || true

# The native toolchain lock lines run_shell appends to the retargeted seed lock (the guix
# rust/gcc-toolchain lines it drops are replaced by these /td/store lines).
NATIVE_LOCK="$scratch/native-toolchain.lock"
{
  echo "rust-1.96.0-x86_64-store-native /td/store/$RUSTREL seed"
  echo "gcc-14.3.0-x86_64-native /td/store/$NGREL seed"
  echo "binutils-2.44-x86_64-native /td/store/$NBREL seed"
  echo "glibc-2.41-x86_64 /td/store/$GLREL seed"
} > "$NATIVE_LOCK"

# Native link mode: run_rust bakes these into rg/fd (the native gcc is a PLAIN gcc — no
# ld-wrapper). libgcc_s.so.1 lives in the RUST tree's lib/ (the #255 assembly co-located it).
test -e "$STORE/$RUSTREL/lib/libgcc_s.so.1" || fail "no libgcc_s.so.1 in the rust tree lib/"
interp="/td/store/$GLREL/lib/ld-linux-x86-64.so.2"
rpath="/td/store/$GLREL/lib:/td/store/$RUSTREL/lib"
bdir="/td/store/$GLREL/lib"

# ============================================================================================
# 3. Expose the physical native store at /td/store so the native-linked tools RUN on the host
#    PATH (their interp/RUNPATH are /td/store). The sandbox root is a fresh writable tmpfs, so a
#    symlink is enough — this is the direct-host-exec equivalent of gate 424's store-ns bind.
# ============================================================================================
if [ -e /td/store ] || [ -L /td/store ]; then
  # Idempotent across a heavy pair sharing a sandbox; only ever OUR symlink.
  cur=`readlink /td/store 2>/dev/null || true`
  test "$cur" = "$STORE" || fail "/td/store already exists and is not our native store ($cur) — refusing to clobber"
else
  mkdir -p /td || fail "could not create /td on the sandbox root (is the tmpfs root writable?)"
  ln -sfn "$STORE" /td/store || fail "could not symlink /td/store -> $STORE"
fi
test -x "/td/store/$NGREL/bin/gcc" || fail "/td/store symlink does not resolve the native toolchain"
echo ">> /td/store exposed (native toolchain resolvable for a host exec of the built tools)"

cache="$scratch/pkgs"; mkdir -p "$cache/tmp"
READELF="$STORE/$NBREL/bin/readelf"
test -x "$READELF" || fail "no native readelf at $READELF"

# td shell, run with guix/Guile OFF PATH (env -i + scrubbed PATH ⇒ no guix process). The crate
# closure comes from TD_SHELL_VENDOR_ROOT; the NATIVE /td/store toolchain from TD_SHELL_NATIVE_*
# (run_shell retargets the seed lock onto it and drops the guix rust/gcc-toolchain — the cutover).
tdshell() {
  env -i HOME="$cache" TMPDIR="$cache/tmp" PATH="$SCRUB" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SHELL_LOCKS=tests \
    TD_SHELL_CACHE="$cache" TD_SHELL_VENDOR_ROOT="$VENDOR_ROOT" \
    TD_SHELL_NATIVE_STORE="$WSTORE" TD_SHELL_NATIVE_DB="$WDB" TD_SHELL_NATIVE_EXTRA_DBS="$SNDB" \
    TD_SHELL_NATIVE_INTERP="$interp" TD_SHELL_NATIVE_RPATH="$rpath" TD_SHELL_NATIVE_BDIR="$bdir" \
    TD_SHELL_NATIVE_LOCK="$NATIVE_LOCK" \
    "$TB" shell "$@"
}

# assert a td-built tool binary is native-linked: zero /gnu/store bytes, interp = the /td/store ld.
assert_native_linked() {
  bin=$1; what=$2
  test -x "$bin" || fail "$what: not executable ($bin)"
  if grep -q -a -- '/gnu/store' "$bin"; then fail "$what: the built binary contains /gnu/store bytes ($bin)"; fi
  si=`"$READELF" -l "$bin" 2>/dev/null | grep -o "$interp" | head -1`
  test -n "$si" || fail "$what: not linked vs the /td/store glibc loader ($interp): $bin"
  echo "   [native-linked] $what: zero /gnu/store bytes; interp = the /td/store x86_64 ld"
}

# A fixture tree: a needle hidden in one file, and an unrelated file that must NOT match.
tree="$cache/tree"; mkdir -p "$tree/sub"
printf 'alpha line\nthe needle is here\nbeta line\n' > "$tree/sub/hay.txt"
printf 'nothing to see\n' > "$tree/other.log"

# --- Leg A: DURABLE behavioral (build + run td's ripgrep over a real task, no guix) ---------
echo ">> [DURABLE behavioral] td shell ripgrep -- rg needle tree (native /td/store toolchain, guix/Guile OFF PATH)"
out=`tdshell ripgrep -- rg needle "$tree" 2>"$cache/a.err"` \
  || { tail -40 "$cache/a.err" >&2; fail "td shell ripgrep -- rg exited nonzero"; }
echo "$out" | grep -q 'needle' || fail "td-built rg did not find the 'needle' line (got: $out)"
echo "$out" | grep -q 'other.log' && fail "td-built rg matched the unrelated file (over-match)"
echo "   ok: td built its own ripgrep with the native /td/store toolchain (no guix on PATH) and rg found the needle (not the unrelated file)"

# --- Leg B: DURABLE native-linked (the rg on PATH is td's OWN build, native-linked) ---------
echo ">> [DURABLE native-linked] the rg on PATH is td's own build at a td store path, linked vs /td/store"
rb=`tdshell ripgrep -- bash -c 'command -v rg'` || fail "could not locate rg on the composed PATH"
case "$rb" in
  "$cache"/ripgrep/newstore/*-ripgrep-*/bin/rg) : ;;
  *) fail "rg on PATH is '$rb' — not a td-built path under $cache" ;;
esac
assert_native_linked "$rb" "rg"

# --- Leg C: DURABLE load-bearing (unknown package errors, NO guix fallback) -----------------
echo ">> [DURABLE load-bearing] an unknown package errors — no guix fallback"
if tdshell no-such-package-xyzzy -- true >/dev/null 2>"$cache/c.err"; then
  fail "td shell no-such-package-xyzzy SUCCEEDED — it must error, not fall back to guix"
fi
grep -q "no td recipe for" "$cache/c.err" \
  || { cat "$cache/c.err" >&2; fail "unknown-package failure was not the 'no td recipe' error (a guix fallback?)"; }
echo "   ok: errored with 'no td recipe for ...'; td shell does not reach for guix"

# --- Leg D: DURABLE multi-tool (a real user environment: fd finds files, rg greps them) -----
echo ">> [DURABLE multi-tool] td shell ripgrep fd -- fd finds the file, rg greps inside it (both native /td/store)"
work=`tdshell ripgrep fd -- fd hay "$tree" -x rg needle 2>"$cache/d.err"` \
  || { tail -40 "$cache/d.err" >&2; fail "td shell ripgrep fd -- (fd -x rg) exited nonzero"; }
echo "$work" | grep -q 'needle' \
  || fail "the fd+rg pipeline did not surface the needle (got: $work) — multi-tool env broken"
# Both tools must be td-built store paths on the SAME composed PATH — and both native-linked.
fb=`tdshell ripgrep fd -- bash -c 'command -v fd'` || fail "could not locate fd on the composed PATH"
case "$fb" in
  "$cache"/fd/newstore/*-fd-*/bin/fd) : ;;
  *) fail "fd on PATH is '$fb' — not a td-built path under $cache" ;;
esac
assert_native_linked "$fb" "fd"
echo "   ok: fd ($fb) and rg both td-built with the native /td/store toolchain, guix-free, cooperating in one td shell — a real userland"

echo "PASS: the REAL \`td shell' product command builds and runs the shipped Rust userland"
echo "      (ripgrep + fd) LINKED BY td's OWN NATIVE x86_64 /td/store toolchain (gcc + binutils +"
echo "      glibc + relinked rust; the guix rust/gcc-toolchain path retired for td shell) — guix/Guile"
echo "      scrubbed from PATH, so no guix process; rg greps a needle, fd+rg compose a real task, both"
echo "      td-built + native-linked (zero /gnu/store bytes, interp = /td/store); an unknown package"
echo "      errors with no guix fallback. End-to-end use-case cutover, all DURABLE."
