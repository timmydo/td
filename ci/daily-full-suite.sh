#!/usr/bin/env bash
# ci/daily-full-suite.sh — the DAILY BACKSTOP runner (loop-governance, human 2026-06-21).
#
# Main no longer blocks PRs on the full ./check.sh (Option B, DESIGN §7.2): engine/heavy
# PRs land on `check-engine` smoke + lint + check-fast + review. The full heavy+system
# suite is instead run ONCE DAILY on fresh main by a scheduled agent, which heals any
# regression by opening a FIX-OR-REVERT PR (no auto-merge — a human merges). This script
# is the mechanical half: run the whole suite on fresh main and write a machine-readable
# verdict; the agent reads the verdict and does the triage + PR.
#
# Usage:
#   ci/daily-full-suite.sh [--no-system] [--verdict FILE]
# Exit: a bitfield over the suites — 1 heavy red, 2 system red, 4 harness red
#   (guix-free /td/store tier); 0 = all green, up to 7. A leg that could not run
#   because the RUNNER isn't provisioned for it (no host guix/loop-toolchain for
#   heavy/system; no local + no fetchable harness for the harness leg) does NOT
#   set its bit — see env_error/env_error_msg and harness_env_error/harness_fail
#   in the verdict. Setup errors exit 8/9/10 (before any suite runs, or before any
#   GATE anywhere ran — heavy/system AND harness all unprovisioned), kept out of
#   the bitfield range:
#     8  - unknown CLI argument
#     9  - git fetch of origin/main failed
#     10 - runner host not provisioned for EVERY leg: the loop prelude refused to
#          start (no host guix / loop toolchain on PATH — see issue #268) AND no
#          local/fetchable /td/store harness (harness — see issue #315). This is
#          a RUNNER problem, not a code regression: no gate ran anywhere, so there
#          is nothing to triage/revert on the td side. A runner that can run at
#          least the harness leg does NOT hit this exit — see below.
set -uo pipefail

# Classify a td-builder check FATAL as a runner-provisioning condition (not a code
# regression) vs a genuine failure. One shared matcher for the call sites below (the
# heavy/system guix-guard and the harness leg's own not-provisioned guard) so the
# "td-builder check: FATAL: " prefix — the part shared with check_loop.rs's `fatal()`
# helper — has a single place to update instead of copies that can drift out of sync
# with each other, which is exactly how this classification silently broke before (#268,
# then again via e0f8401 — issue #315).
is_unprovisioned_fatal() {
  grep -qE "^td-builder check: FATAL: ($2)" "$1"
}

verdict=".td-daily-verdict"
run_system=1
while [ $# -gt 0 ]; do
  case "$1" in
    --no-system) run_system=0; shift ;;
    --verdict) verdict=$2; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 8 ;;
  esac
done

git fetch origin main -q || { echo "daily-full-suite: fetch failed" >&2; exit 9; }
main=$(git rev-parse --short origin/main)
hlog=$(mktemp); slog=$(mktemp); xlog=$(mktemp)
trap 'rm -f "$hlog" "$slog" "$xlog"' EXIT

heavy_rc=0; system_rc=0; system_fail=""; harness_rc=0; harness_fail=""
env_error=0; env_error_msg=""
# The loop entry (check.sh retired, #318): host cargo builds td-builder, else the
# pre-placed guix-free stage0 (the harness VM ships one).
if command -v cargo >/dev/null 2>&1; then
  cargo build --release --quiet --manifest-path builder/Cargo.toml
  TDB=builder/target/release/td-builder
else
  # Prefer the CURRENT placement out of the stage0 memo (.stage0-meta line 2 = Cb):
  # it is the one placement the #309 stale-sweep always KEEPS, so this pick cannot
  # vanish mid-run — the raw glob's lexicographic-first choice could name a STALE
  # placement (the #293 hazard class) that a mid-check slow-path sweep then unlinks
  # between this suite's three invocations. Glob fallback: a pre-placed harness VM
  # image may ship a store dir without the memo.
  cb=$(sed -n 2p .td-build-cache/stage0/.stage0-meta 2>/dev/null || true)
  TDB=""
  if [ -n "$cb" ] && [ -x ".td-build-cache/stage0/store/$(basename "$cb")/bin/td-builder" ]; then
    TDB=".td-build-cache/stage0/store/$(basename "$cb")/bin/td-builder"
  else
    TDB=$(ls .td-build-cache/stage0/store/*/bin/td-builder 2>/dev/null | head -1)
  fi
fi
[ -n "$TDB" ] && [ -x "$TDB" ] || { echo "daily: FATAL: no td-builder (need host cargo or a pre-placed stage0)" >&2; exit 1; }
echo ">> daily backstop: full td-builder check on origin/main ($main)"
# The stage0 td-builder compiles from source with the ENVIRONMENT's rust (provision-rust/cc),
# so there is no guix-built toolchain seed to warm here — the runner brings rust + cc.
# TD_SUBST_FORCE_BUILD=1: the daily is the SOLE from-seed authoritative build + publisher
# (x64-toolchain-subst). Suppress the fetch short-circuit so gate 414 ALWAYS builds the x86_64
# toolchain from seed and re-produces the closure export to publish below — otherwise a persistent
# ~/.td/subst (the very thing the per-PR loop needs) would make the daily FETCH its own prior
# publish and never rebuild/republish (self-starvation).
# TD_CHECK_CHAIN_CACHE= (set-and-empty = force-cold, #317): the per-PR loop reuses the machine-wide
# warm chain bricks by default; the daily stays the authoritative from-seed proof that the
# whole bootstrap chain still BUILDS, so it must never consume a warm brick.
TD_CHECK_CHAIN_CACHE= TD_SUBST_FORCE_BUILD=1 TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} "$TDB" check >"$hlog" 2>&1 || heavy_rc=$?
# Widened to also catch a pre-gate FATAL (e.g. provision_stage0 failing for a real,
# non-guix reason with env_error staying 0) — matches the harness leg's own FATAL-inclusive
# grep below, so a heavy-tier FATAL doesn't leave heavy_fail empty in the verdict (#417 review).
heavy_fail=$(grep -E '^FAIL|^td-builder check: FATAL' "$hlog" | head -5 | tr '\n' ';')

# td-builder check's own fail-closed guards (builder/src/check_loop.rs `run()`) abort BEFORE
# any gate runs when the runner host isn't provisioned for the standard tier at all. That is
# a runner-provisioning problem, not a gate regression (issue #268) — a bare
# heavy=red/system=red with no *_fail is indistinguishable from a real break, so detect it
# here and report it distinctly instead of sending an agent hunting for a code regression
# that doesn't exist.
#
# The matched substrings must track check_loop.rs's actual FATAL wording. History: #316
# (2026-07-03) ported check.sh's old guard to td-builder's own "FATAL: could not read the
# host guix commit (...)" wording; e0f8401 (2026-07-05, #406) then REMOVED that guard
# entirely in favor of a simpler "no guix on PATH" check, but did not update this grep — so
# every guix-less daily run since silently fell through to the heavy=red/system=red path
# this guard exists to avoid (issue #315). #416 (2026-07-06) then dropped the guix pin
# entirely and added a SECOND runner-provisioning fatal: a "loop toolchain:" fatal when a
# core tool (bash/make/…) doesn't resolve to an in-store bin dir — i.e. the host didn't
# bring the base userland under /gnu/store on PATH. Both are HOST gaps, not code
# regressions. Match on stable substrings rather than whole sentences so an unrelated
# wording tweak doesn't silently reopen this gap again.
if [ $heavy_rc -ne 0 ] && is_unprovisioned_fatal "$hlog" 'no guix on PATH|loop toolchain:'; then
  env_error=1
  env_error_msg="runner host not provisioned: the loop prelude could not resolve host guix and/or the base loop toolchain under /gnu/store on PATH (see issue #268) — no gate ran, not a code regression"
  heavy_fail="$env_error_msg"
  echo ">> daily backstop: $env_error_msg"
fi

if [ "$run_system" = 1 ] && [ $env_error -eq 0 ]; then
  echo ">> daily backstop: td-builder check check-system on origin/main ($main)"
  TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} "$TDB" check check-system >"$slog" 2>&1 || system_rc=$?
  system_fail=$(grep -E '^FAIL' "$slog" | head -5 | tr '\n' ';')
elif [ "$run_system" = 1 ]; then
  system_rc=1
  system_fail="SKIPPED — $env_error_msg"
  echo ">> daily backstop: check-system SKIPPED — $env_error_msg"
fi

# host-sandbox-stage0 inc2c: the GUIX-FREE harness tier — the loop on td's OWN /td/store
# harness (busybox+make, NO guix). This is the loop a guix-less runner can ALWAYS attempt,
# independent of whether heavy/system ran at all (issue #315): `check-harness` resolves its
# own precondition — it consumes a locally-persisted .td-build-cache/harness (e.g. this same
# run's heavy suite reached gate 420), or FETCHES the daily-published signed harness
# substitute (tools/resolve-harness.sh) when absent locally, and only THEN fails closed. So
# the harness leg's precondition is "harness provisioned/fetchable", never "did heavy run" —
# do not gate the attempt on env_error or on a local .td-build-cache/harness check.
harness_env_error=0
echo ">> daily backstop: td-builder check check-harness on origin/main ($main) — guix-free /td/store loop"
"$TDB" check check-harness >"$xlog" 2>&1 || harness_rc=$?
# `check-harness` can FATAL two ways before its own loop content ever runs: the harness
# itself unfetchable/unprovisioned, OR its own stage0 td-builder failing to build
# (load_stage0_tb, check_loop.rs) — both are runner-provisioning gaps, not a harness-loop
# regression, so classify either as harness_env_error.
if [ $harness_rc -ne 0 ] && is_unprovisioned_fatal "$xlog" 'no provisioned /td/store harness|could not build the guix-free stage0 td-builder'; then
  harness_env_error=1
  harness_fail="runner not provisioned: no local /td/store harness and none fetchable from a substitute store (see issue #315) — not a code regression"
  echo ">> daily backstop: $harness_fail"
else
  harness_fail=$(grep -E '^FAIL|^td-builder check: FATAL' "$xlog" | head -5 | tr '\n' ';')
fi

# Per-leg verdict state, computed once as plain variables (not inline &&/|| ternaries in the
# echo below — a nested ternary there previously hid a since-fixed unreachable branch, #417
# review) so each leg's env-error/skip/genuinely-red states stay easy to audit at a glance.
heavy_state=green
[ $heavy_rc -ne 0 ] && heavy_state=red
[ $env_error -eq 1 ] && heavy_state=unprovisioned

if [ "$run_system" != 1 ]; then
  system_state=skipped
elif [ $env_error -eq 1 ]; then
  system_state=unprovisioned
elif [ $system_rc -eq 0 ]; then
  system_state=green
else
  system_state=red
fi

if [ $harness_env_error -eq 1 ]; then
  harness_state=unprovisioned
elif [ $harness_rc -eq 0 ]; then
  harness_state=green
else
  harness_state=red
fi

{
  echo "commit=$main"
  echo "date=$(date -Is)"
  echo "env_error=$env_error"
  echo "env_error_msg=$env_error_msg"
  echo "heavy=$heavy_state"
  echo "heavy_rc=$heavy_rc"
  echo "heavy_fail=$heavy_fail"
  echo "system=$system_state"
  echo "system_rc=$system_rc"
  echo "system_fail=$system_fail"
  echo "harness=$harness_state"
  echo "harness_rc=$harness_rc"
  echo "harness_env_error=$harness_env_error"
  echo "harness_fail=$harness_fail"
} > "$verdict"

# Exit 10 (full abort, nothing to triage) ONLY when EVERY leg is unprovisioned — heavy/
# system need host guix/loop-toolchain and the harness leg has neither a local nor a
# fetchable /td/store harness. A runner that can run at least one leg (issue #315: e.g. a
# guix-less runner with a fetchable harness substitute) falls through to the normal verdict
# below instead, so its harness leg's real green/red still reaches the agent rather than
# being folded away.
if [ $env_error -eq 1 ] && [ $harness_env_error -eq 1 ]; then
  echo ">> daily backstop: RUNNER NOT PROVISIONED at $main — no gate could run anywhere (heavy/system: $env_error_msg; harness: $harness_fail)"
  echo ">> daily backstop: this is a HOST setup gap, not a code regression — no fix-or-revert PR is warranted from this alone. Provision host guix/loop-toolchain (heavy/system) or expose a /td/store harness substitute (harness — issue #315), then re-run."
  cat "$verdict"
  exit 10
fi
if [ $env_error -eq 1 ]; then
  echo ">> daily backstop: heavy/system SKIPPED (runner not provisioned: $env_error_msg) — the harness leg ran independently below"
fi

rc=0
[ $env_error -eq 0 ] && [ $heavy_rc -ne 0 ] && rc=$((rc+1))
[ $env_error -eq 0 ] && [ "$run_system" = 1 ] && [ $system_rc -ne 0 ] && rc=$((rc+2))
[ $harness_env_error -eq 0 ] && [ $harness_rc -ne 0 ] && rc=$((rc+4))
if [ $rc -eq 0 ] && [ $env_error -eq 1 ]; then
  # Reached only when the "both unprovisioned" exit-10 abort above did NOT fire, i.e. the
  # harness leg (guix-free) DID run — but heavy/system never ran (no host guix), so this is
  # not a full-suite proof: no .td-last-green, no publish steps (their gate exports don't
  # exist — heavy didn't reach them). rc==0 here also GUARANTEES harness_rc==0 (a genuinely
  # red harness with heavy/system unprovisioned instead falls to the `else` RED branch below,
  # since it sets bit 4) — so this is always the harness-green case, never harness-red.
  echo ">> daily backstop: PARTIAL at $main — harness leg GREEN; heavy/system unprovisioned this run ($env_error_msg, issue #315) — not a full-suite proof, .td-last-green NOT recorded"
elif [ $rc -eq 0 ]; then
  echo "$main" > .td-last-green   # seed of the future `stable` marker
  echo ">> daily backstop: ALL GREEN at $main (recorded .td-last-green)"
  # toolchain-subst-default (#209): the heavy suite (incl. gate 412) is green, so the
  # lock-keyed toolchain export gate 412 persisted is the authoritative from-seed build.
  # Sign + publish it to the loop's substitute store so the resolver (tools/resolve-toolchain.sh)
  # FETCHES it instead of the ~90-min rebuild. Guarded: a no-op (clear message) unless the daily
  # runner provides the signing key + a td-subst binary. Trust anchor = tests/td-subst.pub.
  _exp=.td-build-cache/toolchain-subst-export
  _store=${TD_SUBST_STORE:-$HOME/.td/subst}
  _sb=${TD_SUBST_BIN:-$(command -v td-subst 2>/dev/null || true)}
  if ! ls "$_exp"/*.narinfo >/dev/null 2>&1; then
    echo ">> publish-toolchain-subst: SKIP — no export at $_exp (gate 412 persisted none this run)"
  elif [ -z "${TD_SUBST_PRIVKEY:-}" ]; then
    echo ">> publish-toolchain-subst: SKIP — TD_SUBST_PRIVKEY unset (set the daily-runner signing secret to publish)"
  elif [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-toolchain-subst: SKIP — no td-subst binary (set TD_SUBST_BIN)"
  elif "$_sb" sign "$_exp" "$TD_SUBST_PRIVKEY" >/dev/null 2>&1; then
    mkdir -p "$_store"; cp -a "$_exp"/. "$_store"/
    echo ">> publish-toolchain-subst: signed + published the lock-keyed toolchain to $_store (the loop resolver fetches it; trust = tests/td-subst.pub)"
  else
    echo ">> publish-toolchain-subst: WARN — td-subst sign failed; not published"
  fi
  # x64-toolchain-subst: also sign + publish the x86_64 toolchain CLOSURE that gate 414 subst-exported
  # this run (binutils-2.44 + gcc-14.3.0 + glibc-2.41-x86_64), and STASH the td-subst binary into the
  # store so the `td-builder check` host prelude (subst_env) can expose it — the per-PR loop
  # FETCHES the closure + SKIPS the ~98-min from-seed cross build (fallback to from-seed on miss).
  _xexp=.td-build-cache/x86_64-closure-export
  if ! ls "$_xexp"/*.narinfo >/dev/null 2>&1; then
    echo ">> publish-x86_64-closure: SKIP — no export at $_xexp (gate 414 built none this run)"
  elif [ -z "${TD_SUBST_PRIVKEY:-}" ] || [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-x86_64-closure: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set"
  elif "$_sb" sign "$_xexp" "$TD_SUBST_PRIVKEY" >/dev/null 2>&1; then
    mkdir -p "$_store"; cp -a "$_xexp"/. "$_store"/
    cp -a "$_sb" "$_store/td-subst"   # stash the consumer's td-subst (subst_env exposes it)
    echo ">> publish-x86_64-closure: signed + published the x86_64 toolchain closure to $_store + stashed td-subst (the per-PR loop FETCHES the closure, SKIPS the ~98-min build)"
  else
    echo ">> publish-x86_64-closure: WARN — td-subst sign failed; not published"
  fi
  # x86_64 NATIVE toolchain (#258 prereq): sign + publish the native binutils-2.44 + gcc-14.3.0 closure
  # that gate 422 subst-exported this run (tests/td-toolchain-x86_64-native.lock), so the per-PR loop
  # FETCHES the native toolchain + SKIPS the ~45-min native build (fallback to from-cross build on miss).
  # Same signing key + trust anchor (tests/td-subst.pub) as the cross closure.
  _nxexp=.td-build-cache/x86_64-native-closure-export
  if ! ls "$_nxexp"/*.narinfo >/dev/null 2>&1; then
    echo ">> publish-x86_64-native-closure: SKIP — no export at $_nxexp (gate 422 built none this run)"
  elif [ -z "${TD_SUBST_PRIVKEY:-}" ] || [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-x86_64-native-closure: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set"
  elif "$_sb" sign "$_nxexp" "$TD_SUBST_PRIVKEY" >/dev/null 2>&1; then
    mkdir -p "$_store"; cp -a "$_nxexp"/. "$_store"/
    echo ">> publish-x86_64-native-closure: signed + published the native x86_64 toolchain closure to $_store (the per-PR loop FETCHES the native toolchain, SKIPS the ~45-min native build)"
  else
    echo ">> publish-x86_64-native-closure: WARN — td-subst sign failed; not published"
  fi
  # seed substitutes (#311): publish the pinned stage0 SEED closure (tests/
  # td-builder-rust.lock) so the loop's seed resolver (tools/resolve-seed.sh, called by
  # the provision_stage0 prelude) can realize a MISSING seed with NO guix process —
  # fail-closed, no guix fallback. publish-seed-subst.sh captures the closure by
  # content-scanning the live store bytes (zero guix-db reads) and is idempotent: it
  # re-exports only when a lock root is unpublished (i.e. after a channel bump).
  if [ -z "${TD_SUBST_PRIVKEY:-}" ] || [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-seed-subst: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set"
  elif TD_BUILDER="$TDB" TD_SUBST_BIN="$_sb" TD_SUBST_PRIVKEY="$TD_SUBST_PRIVKEY" \
       sh tools/publish-seed-subst.sh tests/td-builder-rust.lock "$_store"; then
    :
  else
    echo ">> publish-seed-subst: WARN — seed publish failed; not published"
  fi
  # harness-subst (#314): the heavy suite ran gate 420, which persisted .td-build-cache/harness
  # (the from-source, guix-byte-free /td/store busybox+make + staged C toolchain + the rel/toolchain
  # metadata). Sign + publish it as ONE whole-tree substitute (fixed name `td-harness`) so a
  # GUIX-LESS runner with an empty .td-build-cache/harness FETCHES it (tools/resolve-harness.sh, from
  # run_check_harness) instead of needing a local guix-hosted heavy build to have produced it — the
  # circularity that kept the cloud daily runner guix-dependent (#294). Same signing key + trust
  # anchor (tests/td-subst.pub) as the toolchain closures. This runner consumes its harness LOCALLY
  # (gate 420 built it); the fetch path is for the separate guix-less runner.
  if [ ! -d .td-build-cache/harness/store ] || [ ! -s .td-build-cache/harness/rel ]; then
    echo ">> publish-harness-subst: SKIP — no persisted .td-build-cache/harness (gate 420 did not complete this run)"
  elif [ -z "${TD_SUBST_PRIVKEY:-}" ] || [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-harness-subst: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set"
  elif TD_BUILDER="$TDB" TD_SUBST_BIN="$_sb" TD_SUBST_PRIVKEY="$TD_SUBST_PRIVKEY" \
       sh tools/publish-harness-subst.sh .td-build-cache/harness "$_store" >/dev/null 2>&1; then
    echo ">> publish-harness-subst: signed + published the /td/store harness to $_store (a guix-less runner FETCHES it for check-harness — no local guix build needed)"
  else
    echo ">> publish-harness-subst: WARN — harness publish failed; not published"
  fi
else
  echo ">> daily backstop: RED (heavy_rc=$heavy_rc system_rc=$system_rc harness_rc=$harness_rc) — agent: triage \`git log <last-green>..$main\`, reproduce the failing gate, open a FIX-OR-REVERT PR (no auto-merge). Suspect-revert helper: ci/revert-suspect.sh --ref <sha> --open-pr"
fi
cat "$verdict"
exit $rc
