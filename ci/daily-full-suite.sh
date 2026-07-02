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
#   (guix-free /td/store tier); 0 = all green, up to 7. Setup errors exit 8/9/10
#   (before any suite runs, or before any GATE inside a suite runs), kept out of
#   the bitfield range:
#     8  - unknown CLI argument
#     9  - git fetch of origin/main failed
#     10 - runner host not provisioned: check.sh's own integrity guard refused
#          to start (e.g. host guix missing/mismatched vs channels.scm — see
#          issue #268). This is a RUNNER problem, not a code regression: no
#          gate ran, so there is nothing to triage/revert on the td side.
set -uo pipefail

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
echo ">> daily backstop: full ./check.sh on origin/main ($main)"
# TD_SUBST_FORCE_BUILD=1: the daily is the SOLE from-seed authoritative build + publisher
# (x64-toolchain-subst). Suppress the fetch short-circuit so gate 414 ALWAYS builds the x86_64
# toolchain from seed and re-produces the closure export to publish below — otherwise a persistent
# ~/.td/subst (the very thing the per-PR loop needs) would make the daily FETCH its own prior
# publish and never rebuild/republish (self-starvation).
TD_SUBST_FORCE_BUILD=1 TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} ./check.sh >"$hlog" 2>&1 || heavy_rc=$?
heavy_fail=$(grep -E '^FAIL' "$hlog" | head -5 | tr '\n' ';')

# check.sh's own integrity guard (host guix == pinned channels.scm commit) aborts
# BEFORE any gate runs when the runner host isn't provisioned with guix at all, or
# with a mismatched channel. That is a runner-provisioning problem, not a gate
# regression (issue #268) — a bare heavy=red/system=red with no *_fail is
# indistinguishable from a real break, so detect it here and report it distinctly
# instead of sending an agent hunting for a code regression that doesn't exist.
if [ $heavy_rc -ne 0 ] && grep -q '^check\.sh: FATAL: host guix' "$hlog"; then
  env_error=1
  env_error_msg="runner host not provisioned: host guix missing/mismatched vs channels.scm (see issue #268) — no gate ran, not a code regression"
  heavy_fail="$env_error_msg"
  echo ">> daily backstop: $env_error_msg"
fi

if [ "$run_system" = 1 ] && [ $env_error -eq 0 ]; then
  echo ">> daily backstop: ./check.sh check-system on origin/main ($main)"
  TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} ./check.sh check-system >"$slog" 2>&1 || system_rc=$?
  system_fail=$(grep -E '^FAIL' "$slog" | head -5 | tr '\n' ';')
elif [ "$run_system" = 1 ]; then
  system_rc=1
  system_fail="SKIPPED — $env_error_msg"
  echo ">> daily backstop: check-system SKIPPED — $env_error_msg"
fi

# host-sandbox-stage0 inc2c: the GUIX-FREE harness tier — the loop on td's OWN /td/store
# harness (busybox+make, NO guix). The heavy ./check.sh above ran gate 420, which persists
# .td-build-cache/harness; consume it here. This is the loop the guix-less VM runs. Only
# attempt it when the harness was persisted (heavy green enough to reach gate 420); a missing
# harness is a heavy-suite problem already reported, not a separate harness failure.
if [ $env_error -eq 1 ]; then
  harness_rc=4; harness_fail="SKIPPED — $env_error_msg"
  echo ">> daily backstop: check-harness SKIPPED — $env_error_msg"
elif [ -d .td-build-cache/harness/store ] && [ -s .td-build-cache/harness/rel ]; then
  echo ">> daily backstop: ./check.sh check-harness on origin/main ($main) — guix-free /td/store loop"
  ./check.sh check-harness >"$xlog" 2>&1 || harness_rc=$?
  harness_fail=$(grep -E '^FAIL|^check.sh: FATAL' "$xlog" | head -5 | tr '\n' ';')
else
  harness_rc=4; harness_fail="no .td-build-cache/harness persisted (gate 420 did not complete)"
  echo ">> daily backstop: check-harness SKIPPED — $harness_fail"
fi

{
  echo "commit=$main"
  echo "date=$(date -Is)"
  echo "env_error=$env_error"
  echo "env_error_msg=$env_error_msg"
  echo "heavy=$([ $heavy_rc -eq 0 ] && echo green || echo red)"
  echo "heavy_rc=$heavy_rc"
  echo "heavy_fail=$heavy_fail"
  echo "system=$([ "$run_system" = 1 ] && { [ $system_rc -eq 0 ] && echo green || echo red; } || echo skipped)"
  echo "system_rc=$system_rc"
  echo "system_fail=$system_fail"
  echo "harness=$([ $harness_rc -eq 0 ] && echo green || echo red)"
  echo "harness_rc=$harness_rc"
  echo "harness_fail=$harness_fail"
} > "$verdict"

if [ $env_error -eq 1 ]; then
  echo ">> daily backstop: RUNNER NOT PROVISIONED at $main — $env_error_msg"
  echo ">> daily backstop: this is a HOST setup gap, not a code regression — no fix-or-revert PR is warranted from this alone. Provision the runner with guix pulled to the channels.scm-pinned commit, then re-run."
  cat "$verdict"
  exit 10
fi

rc=0
[ $heavy_rc -ne 0 ] && rc=$((rc+1))
[ "$run_system" = 1 ] && [ $system_rc -ne 0 ] && rc=$((rc+2))
[ $harness_rc -ne 0 ] && rc=$((rc+4))
if [ $rc -eq 0 ]; then
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
  # store so check.sh host-prep (tools/warm-subst.sh) can expose it — the per-PR loop FETCHES the
  # closure + SKIPS the ~98-min from-seed cross build (fallback to from-seed on miss).
  _xexp=.td-build-cache/x86_64-closure-export
  if ! ls "$_xexp"/*.narinfo >/dev/null 2>&1; then
    echo ">> publish-x86_64-closure: SKIP — no export at $_xexp (gate 414 built none this run)"
  elif [ -z "${TD_SUBST_PRIVKEY:-}" ] || [ -z "$_sb" ] || [ ! -x "$_sb" ]; then
    echo ">> publish-x86_64-closure: SKIP — TD_SUBST_PRIVKEY / td-subst binary not set"
  elif "$_sb" sign "$_xexp" "$TD_SUBST_PRIVKEY" >/dev/null 2>&1; then
    mkdir -p "$_store"; cp -a "$_xexp"/. "$_store"/
    cp -a "$_sb" "$_store/td-subst"   # stash the consumer's td-subst (warm-subst.sh exposes it)
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
else
  echo ">> daily backstop: RED (heavy_rc=$heavy_rc system_rc=$system_rc harness_rc=$harness_rc) — agent: triage \`git log <last-green>..$main\`, reproduce the failing gate, open a FIX-OR-REVERT PR (no auto-merge). Suspect-revert helper: ci/revert-suspect.sh --ref <sha> --open-pr"
fi
cat "$verdict"
exit $rc
