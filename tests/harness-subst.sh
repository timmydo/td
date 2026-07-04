#!/bin/sh
# tests/harness-subst.sh — gate: SHIP the /td/store harness to a guix-less runner via td-subst
# (#314). A runner with no guix and an EMPTY .td-build-cache/harness can't BUILD the harness (that
# needs a guix capture host); today `td-builder check check-harness` just FATALs. This gate proves
# the shipping mechanism end to end: the daily EXPORTS + SIGNS the whole harness tree as ONE
# fixed-name substitute (`td-harness`), and the guix-less runner's resolver (tools/resolve-harness.sh,
# the exact consumer run_check_harness calls) FETCHES + VERIFIES + RESTORES it, or FAILS CLOSED.
#
# The harness is a TREE-SET, not a lock-keyed per-path closure: a `store/` with content-addressed
# entries PLUS loose files (the /td/store/ld loader) PLUS the rel + toolchain metadata the loop
# reads. So it ships as ONE nar of the whole tree under a fixed name; trust = the ed25519 signature
# (pinned tests/td-subst.pub) + the signed NarHash (the harness is a content-addressed build output
# with no lock name to recompute). The daily republishes it every green run.
#
# Fixture, not the 90-min real harness (the precedent gate 358/359 sets for subst gates): a
# harness-SHAPED tree whose runnable member is a real static bash from hello's closure, plus a loose
# `ld` and the metadata. That exercises the byte-agnostic shipping path identically. The REAL harness
# running HARNESS-LOOP-OK from these bytes is proven by gate 420 (builds+runs the real busybox+make+
# gcc) + the daily's check-harness leg; here we prove the FETCH+RESTORE delivers a runnable,
# metadata-intact, byte-identical tree.
#
#   [DURABLE behavioral] FETCH RUNS: the whole harness tree is exported + signed into a store; the
#     resolver fetches it (sig + StorePath + NarHash verified), restores it, and a binary FROM THE
#     FETCHED store RUNS -> a harness obtained WITHOUT building it.
#   [DURABLE behavioral] TREE-SET INTACT: the fetched tree carries the loose /td/store/ld loader AND
#     the rel + toolchain (HT_TARGET/HT_GCC/HT_GLIBC/HT_BU) metadata the check-harness loop reads —
#     byte-identical to what was published.
#   [DURABLE behavioral] FAIL CLOSED: a cold store -> the resolver MISSES (exit 1, no path) so
#     check-harness fails closed with its provisioning message (no from-source fallback on a
#     guix-less runner).
#   [SELF-DISCRIMINATION] a WRONG pinned key -> rejected -> MISS. The ed25519 signature is load-bearing.
#   [SELF-DISCRIMINATION] a validly-signed narinfo for a DIFFERENT StorePath -> the resolver's own
#     StorePath==td-harness check rejects it.
# The subst binary is td-BUILT from source (move-off-Guile §5), reusing tests/td-subst.lock exactly
# like gates 358/359.
set -eu
cd "$(dirname "$0")/.."

. tests/cache-lib.sh
export TD_STAGE0_BASE="$(pwd)/.td-build-cache/stage0"
load_stage0; load_recipe_eval; tb="$TB"
export

# --- build td-subst from source (its own cache dir; CACHE=hit on reruns) ---
guix=${GUIX:-guix}
lock0="$(pwd)/tests/td-subst.lock"
test -s "$lock0" || { echo "ERROR: no $lock0" >&2; exit 1; }
# coreutils is a DECLARED gate input (#353): the runner resolved it from
# tests/td-subst.lock and exported it — no lock-grepping here.
cu=${TD_GATE_INPUT_COREUTILS:-}
test -n "$cu" || { echo "ERROR: TD_GATE_INPUT_COREUTILS unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }
shdir=$(dirname "$(command -v sh)")   # the helper scripts are shell scripts: coreutils $cu has no `sh`
scratch="$(pwd)/.td-build-cache/harness-subst"; mkdir -p "$scratch/tmp" "$scratch/b"; rm -f "$scratch/b/"*.drv
grep ' /gnu/store/' "$lock0" | sed 's/^[^ ]* //' | xargs "$guix" build >/dev/null \
  || { echo "ERROR: could not realize the seed + vendored .crate deps" >&2; exit 1; }
srcinfo=$(sh tests/intern-src.sh "$tb" td-subst-src "$(pwd)/subst" "$scratch" target vendor .cargo) \
  || { echo "ERROR: could not intern the subst crate tree" >&2; exit 1; }
eval "$srcinfo"
lock="$scratch/td-subst.lock"; { cat "$lock0"; echo "td-subst-source $src"; } > "$lock"
sh tests/recipe-emit.sh td-subst > "$scratch/subst.json"
test -s "$scratch/subst.json" || { echo "ERROR: ts-emit produced no JSON" >&2; exit 1; }
sd="$scratch/b"
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$tb" build-recipe "$scratch/subst.json" "$lock" "$sd" /gnu/store "$srcstore" "$srcdb" \
  > "$scratch/bout" 2>"$scratch/err" || { echo "FAIL: build-recipe td-subst:" >&2; tail -20 "$scratch/err" >&2; exit 1; }
out=$(sed -n 's/^OUT=out //p' "$scratch/bout")
ts="$sd/newstore/$(basename "$out")/bin/td-subst"
test -x "$ts" || { echo "FAIL: no td-subst binary at $ts" >&2; exit 1; }
echo "  [DURABLE structural] td-built td-subst from source (move-off-Guile §5): $out"

# --- build a harness-SHAPED fixture tree: a store/ with a content-addressed set (its runnable
#     member a real static bash), a LOOSE /td/store/ld loader (the tree-set member no per-path
#     export can express), a staged toolchain dir, and the rel + toolchain metadata ---
W="$scratch/w"; rm -rf "$W"; mkdir -p "$W"
H="$W/harness"
rel="zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-userland-x86_64-store-native"
gccb="yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-gcc-14.3.0-x86_64"
glibcb="xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-glibc-2.41-x86_64"
bub="wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww-binutils-2.44-x86_64"
target=x86_64-pc-linux-gnu
mkdir -p "$H/store/$rel/bin" "$H/store/$gccb/bin"
# the static-bash fixture is a DECLARED gate input (#353): the runner
# content-scanned hello's bash closure and exported the unique bash-static member.
fixt=${TD_GATE_INPUT_BASH_STATIC:-}
test -n "$fixt" || { echo "ERROR: TD_GATE_INPUT_BASH_STATIC unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }
test -x "$fixt/bin/bash" || { echo "FAIL: no static bash fixture at $fixt" >&2; exit 1; }
cp "$fixt/bin/bash" "$H/store/$rel/bin/busybox"; chmod 0755 "$H/store/$rel/bin/busybox"
printf '\177ELF-loader-stub\n' > "$H/store/ld"                       # loose store-root member
printf '#!/bin/sh\necho stub-gcc\n' > "$H/store/$gccb/bin/$target-gcc"; chmod 0755 "$H/store/$gccb/bin/$target-gcc"
printf '%s\n' "$rel" > "$H/rel"
{ printf 'HT_TARGET=%s\n' "$target"; printf 'HT_GCC=%s\n' "$gccb"; \
  printf 'HT_GLIBC=%s\n' "$glibcb"; printf 'HT_BU=%s\n' "$bub"; } > "$H/toolchain"

# --- producer: export the WHOLE tree + sign (ephemeral key — CI has no production secret) ---
"$ts" keygen "$W/priv" "$W/pub" >/dev/null
env -i PATH="$cu/bin:$shdir" TD_BUILDER="$tb" TD_SUBST_BIN="$ts" TD_SUBST_PRIVKEY="$W/priv" \
  sh tools/publish-harness-subst.sh "$H" "$W/store" >/dev/null \
  || { echo "FAIL: publish-harness-subst.sh (producer)" >&2; exit 1; }
test -f "$W/store/td-harness.narinfo" || { echo "FAIL: publisher wrote no td-harness.narinfo" >&2; exit 1; }
grep -q '^Sig: ' "$W/store/td-harness.narinfo" || { echo "FAIL: publisher did not sign the narinfo" >&2; exit 1; }
grep -q '^StorePath: /td/store/td-harness$' "$W/store/td-harness.narinfo" || { echo "FAIL: narinfo StorePath is not the fixed harness name" >&2; exit 1; }
echo "  [DURABLE behavioral] PUBLISHER: publish-harness-subst.sh exported + signed the WHOLE harness tree as one td-harness substitute"

# --- [DURABLE behavioral] FETCH RUNS + TREE-SET INTACT ---
D="$W/dest"
got=$(env -i PATH="$cu/bin:$shdir" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store" \
      TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-harness.sh "$D")
test "x$got" = "x$D" || { echo "FAIL: resolver did not print the restored harness dir (got '$got')" >&2; exit 1; }
# a binary FROM THE FETCHED store runs
ran=$(env -i "$D/store/$rel/bin/busybox" -c 'echo RAN-FETCHED-HARNESS')
test "x$ran" = "xRAN-FETCHED-HARNESS" || { echo "FAIL: the fetched (not built) harness binary did not run (got '$ran')" >&2; exit 1; }
# the loose /td/store/ld loader shipped (the tree-set member no per-path export can express)
cmp -s "$H/store/ld" "$D/store/ld" || { echo "FAIL: the loose /td/store/ld loader did not round-trip byte-identically" >&2; exit 1; }
# the runnable member is byte-identical to what was published
cmp -s "$H/store/$rel/bin/busybox" "$D/store/$rel/bin/busybox" || { echo "FAIL: the fetched harness binary is not byte-identical to the published one" >&2; exit 1; }
# the rel + toolchain metadata the check-harness loop reads survived intact
test "x$(cat "$D/rel")" = "x$rel" || { echo "FAIL: fetched rel != published rel" >&2; exit 1; }
HT_TARGET=; HT_GCC=; HT_GLIBC=; HT_BU=; . "$D/toolchain"
{ [ "x$HT_TARGET" = "x$target" ] && [ "x$HT_GCC" = "x$gccb" ] && [ "x$HT_GLIBC" = "x$glibcb" ] && [ "x$HT_BU" = "x$bub" ]; } \
  || { echo "FAIL: fetched toolchain manifest lost fields (HT_TARGET=$HT_TARGET HT_GCC=$HT_GCC HT_GLIBC=$HT_GLIBC HT_BU=$HT_BU)" >&2; exit 1; }
echo "  [DURABLE behavioral] FETCH RUNS + TREE-SET INTACT: fetched the signed harness (sig + StorePath + NarHash verified), a binary from the fetched store RAN, and the loose ld + rel + toolchain metadata round-tripped byte-identically -> a harness obtained WITHOUT building it"

# --- [DURABLE behavioral] FAIL CLOSED on a cold store ---
mkdir -p "$W/empty"
if env -i PATH="$cu/bin:$shdir" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/empty" \
   TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-harness.sh "$W/d2" >/dev/null 2>&1; then
  echo "FAIL: resolver returned 0 on a COLD store (should MISS -> fail closed)" >&2; exit 1
fi
test ! -e "$W/d2" || { echo "FAIL: resolver left a harness dir behind on a MISS" >&2; exit 1; }
echo "  [DURABLE behavioral] FAIL CLOSED: a cold store -> the resolver MISSES (exit 1, no dir) -> check-harness fails closed (no from-source fallback on a guix-less runner)"

# --- [SELF-DISCRIMINATION] a WRONG pinned key -> rejected -> MISS ---
"$ts" keygen "$W/wrong.priv" "$W/wrong.pub" >/dev/null
if env -i PATH="$cu/bin:$shdir" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store" \
   TD_SUBST_PUBKEY="$W/wrong.pub" TD_STORE_DIR=/td/store sh tools/resolve-harness.sh "$W/d3" >/dev/null 2>&1; then
  echo "FAIL: resolver ACCEPTED a substitute under a WRONG pinned key (signature not load-bearing)" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a wrong pinned key -> the resolver's fetch is rejected -> MISS (signature load-bearing)"

# --- [SELF-DISCRIMINATION] a validly-signed narinfo for a DIFFERENT StorePath -> the resolver's own
#     StorePath==td-harness check rejects it (td-subst fetch verifies sig + NarHash, NOT the name).
#     Strip the old Sig before re-signing: `td-subst sign` SKIPS already-signed narinfos, so without
#     this the leg would trip on a STALE signature (over the old StorePath) and never reach — let
#     alone test — the resolver's StorePath check. Stripping it mints a FRESH valid signature over
#     the wrong-path body, isolating the StorePath check as the ONLY line that can reject it. ---
cp -r "$W/store" "$W/store2"
sed -i -e 's#^StorePath: .*#StorePath: /td/store/00000000000000000000000000000000-not-the-harness#' \
       -e '/^Sig: /d' "$W/store2/td-harness.narinfo"
"$ts" sign "$W/store2" "$W/priv" >/dev/null
grep -q '^Sig: ' "$W/store2/td-harness.narinfo" || { echo "FAIL: re-sign did not mint a fresh signature over the wrong-path body" >&2; exit 1; }
if env -i PATH="$cu/bin:$shdir" TD_SUBST_BIN="$ts" TD_BUILDER="$tb" TD_SUBST_STORE="$W/store2" \
   TD_SUBST_PUBKEY="$W/pub" TD_STORE_DIR=/td/store sh tools/resolve-harness.sh "$W/d4" >/dev/null 2>&1; then
  echo "FAIL: resolver ACCEPTED a validly-signed substitute whose StorePath != /td/store/td-harness" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a validly-signed narinfo for a DIFFERENT StorePath -> resolver MISS (the fixed harness name is load-bearing alongside the signature)"

rm -rf "$W" "$scratch/tmp" "$scratch/bout" "$scratch/err"; mkdir -p "$scratch/tmp"
echo "PASS: the /td/store harness ships to guix-less runners via td-subst — the daily EXPORTS + SIGNS the whole tree (td-builder harness-subst-export + td-subst sign) as one td-harness substitute, and tools/resolve-harness.sh (the consumer run_check_harness calls) FETCHES it (ed25519 sig vs the pinned key + StorePath == td-harness + NarHash verified), restores a runnable, metadata-intact, byte-identical tree, and FAILS CLOSED on a cold store / wrong key / wrong StorePath. Closes the circularity that kept the cloud daily runner guix-dependent (#294)."
