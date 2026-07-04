#!/bin/sh
# tests/seed-subst.sh — gate: the loop realizes a MISSING pinned /gnu/store seed through
# td's OWN signed substitute store instead of a `guix build` process (#311; unblocks the
# guix-less runner, re #294 gap (b)). Drives the REAL entry points end-to-end: the
# producer (tools/publish-seed-subst.sh — content-scanned closure capture, subst-export,
# ed25519 sign) against a scratch store with an ephemeral key, and the consumer through
# the loop's ACTUAL host prelude (tests/cache-lib.sh provision_stage0 -> tools/
# resolve-seed.sh) with a POISONED `guix` first on PATH, so ANY guix process invocation
# on the seed-resolve path reds the gate.
#
#   [DURABLE behavioral] PUBLISHER: publish-seed-subst.sh captures a pinned seed root's
#     runtime CLOSURE by content-scanning the live store bytes (no guix db read), exports
#     each member as a signed narinfo+nar, and is idempotent (re-run: nothing to do).
#   [DURABLE behavioral] PRELUDE FETCH: provision_stage0 — the loop's real host prelude —
#     resolves a seed lock whose root is ABSENT from the (scratch) seed root by FETCHING
#     it from the substitute store (sig + StorePath + NarHash verified), REF-CLOSED (the
#     narinfo References members restore too, byte-identical to the origin), the restored
#     seed binary RUNS, and stage0 is provisioned — with guix POISONED on PATH (no guix
#     process anywhere on the path).
#   [DURABLE structural] WARM PATH: with every root present the prelude succeeds without
#     any substitute store at all (bogus TD_SUBST_*) — presence short-circuits the fetch.
#   [SELF-DISCRIMINATION] FAIL-CLOSED: a missing seed + NO substitute store -> the
#     prelude FAILS with a clear message and does NOT invoke guix (the retired
#     `guix build` fallback would trip the poison shim — this leg is red on the old code).
#   [SELF-DISCRIMINATION] a WRONG pinned key reds the resolve (signature load-bearing);
#     a validly-signed narinfo for a DIFFERENT StorePath reds the resolve (the expected
#     NAME is load-bearing); an empty lock reds (the provenance-switch guard).
#   [DURABLE structural] the shared prelude (tests/cache-lib.sh) carries no `guix build`
#     invocation anymore — the site is retired, not just bypassed.
#
# Trust = ed25519 signature (pinned key; production anchor tests/td-subst.pub, ephemeral
# pair here) + the expected /gnu/store NAME per fetched narinfo. Seed BYTES stay the
# guix-built pin (retired last, per the north star); this removes the guix PROCESS.
# Uses the td-built td-subst from the td-subst gate's cache (needs: td-subst).
set -eu
cd "$(dirname "$0")/.."

. tests/cache-lib.sh
export TD_STAGE0_BASE="$(pwd)/.td-build-cache/stage0"
load_stage0; tb="$TB"

ts=`ls "$PWD"/.td-build-cache/td-subst/b/newstore/*/bin/td-subst 2>/dev/null | head -1 || true`
test -n "$ts" -a -x "$ts" || { echo "ERROR: no td-built td-subst binary (the td-subst gate must build it first)" >&2; exit 1; }

W="$PWD/.td-build-cache/seed-subst"
rm -rf "$W/store" "$W/store2" "$W/root" "$W/root2" "$W/root3" "$W/root4" "$W/nohome" "$W/poison"
mkdir -p "$W/poison" "$W/nohome"

# The poison shim: any `guix` invocation on the seed-resolve path reds loudly.
printf '#!/bin/sh\necho "seed-subst: POISON — a guix process was invoked on the seed-resolve path" >&2\nexit 97\n' > "$W/poison/guix"
chmod 0755 "$W/poison/guix"

# The fixture seed: ONE pinned root from the REAL stage0 seed lock (bash — small, and its
# runtime closure is non-trivial, so the ref-walk leg is exercised for real).
bashpath=`sed -n 's/^[^ ]* \(\/gnu\/store\/[^ ]*-bash-[0-9][^ ]*\)$/\1/p' tests/td-builder-rust.lock | head -1`
test -n "$bashpath" -a -d "$bashpath" || { echo "ERROR: no realized bash seed root in tests/td-builder-rust.lock" >&2; exit 1; }
bashbase=${bashpath##*/}
printf '%s %s\n' "$bashbase" "$bashpath" > "$W/seed.lock"

# --- [DURABLE behavioral] PUBLISHER: capture + export + sign the seed closure ---------
"$ts" keygen "$W/priv" "$W/pub" >/dev/null   # ephemeral pair (CI has no production secret)
env TD_BUILDER="$tb" TD_SUBST_BIN="$ts" TD_SUBST_PRIVKEY="$W/priv" TD_SEED_WARM="$W/warm" \
    PATH="$W/poison:$PATH" \
  sh tools/publish-seed-subst.sh "$W/seed.lock" "$W/store" > "$W/pub.out" 2>"$W/pub.err" \
  || { echo "FAIL: publish-seed-subst.sh (producer):" >&2; tail -10 "$W/pub.err" >&2; exit 1; }
test -f "$W/store/$bashbase.narinfo" || { echo "FAIL: publisher wrote no narinfo for $bashbase" >&2; exit 1; }
grep -q '^Sig: ' "$W/store/$bashbase.narinfo" || { echo "FAIL: publisher did not sign the narinfo" >&2; exit 1; }
nnar=`ls "$W/store"/*.narinfo | grep -c .`
test "$nnar" -ge 2 || { echo "FAIL: seed export has no closure (only $nnar narinfo — References not captured?)" >&2; exit 1; }
echo "  [DURABLE behavioral] PUBLISHER: publish-seed-subst.sh captured the pinned root's closure content-scanned ($nnar members, no guix db read), exported + signed it"
env TD_BUILDER="$tb" TD_SUBST_BIN="$ts" TD_SUBST_PRIVKEY="$W/priv" TD_SEED_WARM="$W/warm" \
  sh tools/publish-seed-subst.sh "$W/seed.lock" "$W/store" > "$W/pub2.out" 2>&1 \
  || { echo "FAIL: publisher re-run failed" >&2; cat "$W/pub2.out" >&2; exit 1; }
grep -q 'nothing to do' "$W/pub2.out" || { echo "FAIL: publisher re-run is not idempotent (no 'nothing to do')" >&2; cat "$W/pub2.out" >&2; exit 1; }
echo "  [DURABLE structural] PUBLISHER idempotence: a re-run with every root published is a no-op"

# --- [DURABLE behavioral] PRELUDE FETCH: the real host prelude, guix poisoned ---------
mkdir -p "$W/root"
env TD_LOCK="$W/seed.lock" TD_SEED_ROOT="$W/root" TD_BUILDER="$tb" \
    TD_SUBST_BIN="$ts" TD_SUBST_STORE="$W/store" TD_SUBST_PUBKEY="$W/pub" \
    PATH="$W/poison:$PATH" \
  sh -c '. tests/cache-lib.sh && provision_stage0' > "$W/p1.out" 2>"$W/p1.err" \
  || { echo "FAIL: provision_stage0 could not resolve the missing seed via the substitute store:" >&2; tail -10 "$W/p1.err" >&2; exit 1; }
if grep -q 'POISON' "$W/p1.err"; then echo "FAIL: the prelude fetch path invoked a guix process" >&2; exit 1; fi
test -d "$W/root/$bashbase" || { echo "FAIL: fetched seed root not restored at $W/root/$bashbase" >&2; exit 1; }
ran=`"$W/root/$bashbase/bin/bash" -c 'echo RAN-FETCHED'`
test "x$ran" = "xRAN-FETCHED" || { echo "FAIL: the fetched (not guix-built) seed bash did not run (got '$ran')" >&2; exit 1; }
nres=`ls "$W/root" | grep -c .`
test "$nres" -ge 2 || { echo "FAIL: the seed's closure members were not restored (only $nres tree(s) — References walk broken?)" >&2; exit 1; }
for b in `ls "$W/root"`; do
  oh=`"$tb" nar-hash "/gnu/store/$b"`; rh=`"$tb" nar-hash "$W/root/$b"`
  test -n "$oh" -a "x$oh" = "x$rh" || { echo "FAIL: restored $b differs from the origin (NAR $rh != $oh)" >&2; exit 1; }
done
echo "  [DURABLE behavioral] PRELUDE FETCH: provision_stage0 fetched the missing seed REF-CLOSED ($nres trees, each byte-identical to the origin), the restored bash RAN, stage0 provisioned — guix poisoned on PATH the whole way"

# --- [DURABLE structural] WARM PATH: presence short-circuits, no store needed ---------
mkdir -p "$W/empty"
env TD_LOCK="$W/seed.lock" TD_SEED_ROOT="$W/root" TD_BUILDER="$tb" \
    TD_SUBST_BIN="/nonexistent-td-subst" TD_SUBST_STORE="$W/empty" TD_SUBST_PUBKEY="$W/pub" \
    PATH="$W/poison:$PATH" \
  sh -c '. tests/cache-lib.sh && provision_stage0' > /dev/null 2>"$W/p2.err" \
  || { echo "FAIL: the all-present warm path failed (it must not need the substitute store):" >&2; tail -5 "$W/p2.err" >&2; exit 1; }
if grep -q 'POISON' "$W/p2.err"; then echo "FAIL: the warm path invoked a guix process" >&2; exit 1; fi
echo "  [DURABLE structural] WARM PATH: with every root present the prelude succeeds with a bogus substitute store — presence short-circuits the fetch"

# --- [SELF-DISCRIMINATION] FAIL-CLOSED: missing seed + no store -> red, and NO guix ---
mkdir -p "$W/root3"
if env -u TD_SUBST_BIN -u TD_SUBST_STORE -u TD_SUBST_PUBKEY HOME="$W/nohome" \
       TD_LOCK="$W/seed.lock" TD_SEED_ROOT="$W/root3" TD_BUILDER="$tb" \
       PATH="$W/poison:$PATH" \
     sh -c '. tests/cache-lib.sh && provision_stage0' > /dev/null 2>"$W/p3.err"; then
  echo "FAIL: provision_stage0 SUCCEEDED with a missing seed and NO substitute store (must fail closed)" >&2; exit 1
fi
if grep -q 'POISON' "$W/p3.err"; then
  echo "FAIL: the fail-closed path invoked a guix process (the retired guix-build fallback is back?)" >&2; exit 1
fi
grep -q 'no td-subst substitute store' "$W/p3.err" \
  || { echo "FAIL: fail-closed message is unclear:" >&2; cat "$W/p3.err" >&2; exit 1; }
echo "  [SELF-DISCRIMINATION] FAIL-CLOSED: a missing seed with no substitute store reds the prelude with a clear message and NO guix process (this leg is red on the retired guix-build fallback)"

# --- [SELF-DISCRIMINATION] wrong pinned key -> red ------------------------------------
"$ts" keygen "$W/wrong.priv" "$W/wrong.pub" >/dev/null
mkdir -p "$W/root2"
if env TD_SEED_ROOT="$W/root2" TD_BUILDER="$tb" TD_SUBST_BIN="$ts" \
       TD_SUBST_STORE="$W/store" TD_SUBST_PUBKEY="$W/wrong.pub" PATH="$W/poison:$PATH" \
     sh tools/resolve-seed.sh "$W/seed.lock" >/dev/null 2>&1; then
  echo "FAIL: resolve-seed ACCEPTED a substitute under a WRONG pinned key (signature not load-bearing)" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a wrong pinned key reds the resolve — the ed25519 signature is load-bearing"

# --- [SELF-DISCRIMINATION] validly-signed narinfo for a DIFFERENT StorePath -> red ----
cp -a "$W/store" "$W/store2"
sed -i "s#^StorePath: .*#StorePath: /gnu/store/00000000000000000000000000000000-not-the-seed#" "$W/store2/$bashbase.narinfo"
"$ts" sign "$W/store2" "$W/priv" >/dev/null
mkdir -p "$W/root4"
if env TD_SEED_ROOT="$W/root4" TD_BUILDER="$tb" TD_SUBST_BIN="$ts" \
       TD_SUBST_STORE="$W/store2" TD_SUBST_PUBKEY="$W/pub" PATH="$W/poison:$PATH" \
     sh tools/resolve-seed.sh "$W/seed.lock" >/dev/null 2>&1; then
  echo "FAIL: resolve-seed ACCEPTED a validly-signed substitute whose StorePath != the expected seed path" >&2; exit 1
fi
echo "  [SELF-DISCRIMINATION] a validly-signed narinfo for a DIFFERENT StorePath reds the resolve — the expected NAME is load-bearing alongside the signature"

# --- [SELF-DISCRIMINATION] an empty lock -> red (the provenance-switch guard) ---------
printf '# no seed lines\n' > "$W/empty.lock"
if env TD_SEED_ROOT="$W/root4" TD_BUILDER="$tb" PATH="$W/poison:$PATH" \
     sh tools/resolve-seed.sh "$W/empty.lock" >/dev/null 2>"$W/el.err"; then
  echo "FAIL: resolve-seed accepted a lock with no seed paths" >&2; exit 1
fi
grep -q 'no /gnu/store seed paths' "$W/el.err" || { echo "FAIL: empty-lock message unclear" >&2; cat "$W/el.err" >&2; exit 1; }
echo "  [SELF-DISCRIMINATION] a lock yielding no seed paths reds — provision-rust.sh cannot silently fall through to rustup/system-cc (the provenance-switch guard held)"

# --- [DURABLE structural] the shared prelude carries no guix-build invocation ---------
if grep -vE '^[[:space:]]*#' tests/cache-lib.sh | grep -qE "(guix|GUIX)[\"')}]*[[:space:]]+build"; then
  echo "FAIL: tests/cache-lib.sh still carries a guix-build invocation — the seed-realize site is not retired" >&2; exit 1
fi
echo "  [DURABLE structural] tests/cache-lib.sh carries no guix-build invocation — the seed-realize site is retired, not bypassed"

rm -rf "$W/store" "$W/store2" "$W/root" "$W/root2" "$W/root3" "$W/root4" "$W/nohome" "$W/empty" \
       "$W"/p*.out "$W"/p*.err "$W/el.err" "$W/pub.out" "$W/pub.err" "$W/pub2.out"
echo "PASS: seed realizations go through td's OWN signed substitute store, not a guix process (#311) — the producer (publish-seed-subst.sh) captures the pinned seed closure content-scanned (no guix db), exports + signs it (idempotent re-run); the loop's REAL host prelude (provision_stage0 -> resolve-seed.sh) fetches a missing seed ref-closed (ed25519 sig vs the pinned key + StorePath == expected path + NarHash verified, restored byte-identical, the fetched bash RUNS, stage0 provisioned) with guix POISONED on PATH; the all-present warm path needs no store; a missing seed with no store FAILS CLOSED with a clear message and no guix fallback; a wrong key / wrong-StorePath substitute / empty lock red the resolve; and the shared prelude carries no guix-build invocation anymore."
