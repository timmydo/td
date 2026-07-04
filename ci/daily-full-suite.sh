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
# The loop entry (check.sh retired, #318): host cargo builds td-builder, else the
# pre-placed guix-free stage0 (the harness VM ships one).
if command -v cargo >/dev/null 2>&1; then
  cargo build --release --quiet --manifest-path builder/Cargo.toml
  TDB=builder/target/release/td-builder
else
  TDB=$(ls .td-build-cache/stage0/store/*/bin/td-builder 2>/dev/null | head -1)
fi
[ -n "$TDB" ] && [ -x "$TDB" ] || { echo "daily: FATAL: no td-builder (need host cargo or a pre-placed stage0)" >&2; exit 1; }
echo ">> daily backstop: full td-builder check on origin/main ($main)"
# Host-prep: warm the stage0 toolchain seed into /gnu/store (#311 — the loop's
# provision_stage0 no longer realizes it with `guix build`; it resolves from a td-subst
# store or fails closed). The daily runner is guix-having, so it warms the guix-built pin
# out-of-band here (gcc-toolchain is a cheap union guix realizes offline) so the loop
# finds the seed present — and so the daily can bootstrap the seed publication below even
# after a channel bump. Best-effort: a failure here surfaces as the loop's own clear
# fail-closed message.
sh tools/warm-stage0-seed.sh || echo ">> daily backstop: WARN — could not warm the stage0 seed out-of-band (provision_stage0 will fail closed if it is absent)" >&2
# TD_SUBST_FORCE_BUILD=1: the daily is the SOLE from-seed authoritative build + publisher
# (x64-toolchain-subst). Suppress the fetch short-circuit so gate 414 ALWAYS builds the x86_64
# toolchain from seed and re-produces the closure export to publish below — otherwise a persistent
# ~/.td/subst (the very thing the per-PR loop needs) would make the daily FETCH its own prior
# publish and never rebuild/republish (self-starvation).
# TD_CHECK_CHAIN_CACHE= (set-and-empty = force-cold, #317): the per-PR loop reuses the machine-wide
# warm chain bricks by default; the daily stays the authoritative from-seed proof that the
# whole bootstrap chain still BUILDS, so it must never consume a warm brick.
TD_CHECK_CHAIN_CACHE= TD_SUBST_FORCE_BUILD=1 TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} "$TDB" check >"$hlog" 2>&1 || heavy_rc=$?
heavy_fail=$(grep -E '^FAIL' "$hlog" | head -5 | tr '\n' ';')

# td-builder check's own integrity guard (guard_pinned_guix, builder/src/check_loop.rs)
# aborts BEFORE any gate runs when the runner host isn't provisioned with guix at all.
# That is a runner-provisioning problem, not a gate regression (issue #268) — a bare
# heavy=red/system=red with no *_fail is indistinguishable from a real break, so detect
# it here and report it distinctly instead of sending an agent hunting for a code
# regression that doesn't exist.
#
# The matched text must track guard_pinned_guix's actual FATAL wording (check_loop.rs):
# #316 (2026-07-03) ported check.sh's "check.sh: FATAL: host guix (...) != pinned..."
# message to td-builder's "td-builder check: FATAL: could not read the host guix commit
# (...)" without updating this grep, so every guix-less run since silently fell through
# to the heavy=red/system=red path this guard exists to avoid (reproduced on a guix-less
# runner: heavy_rc=1/system_rc=1 with empty heavy_fail/system_fail, exit 7 instead of the
# distinct exit 10). Match on the stable "FATAL: could not read the host guix commit"
# substring rather than the whole sentence so an unrelated wording tweak doesn't silently
# reopen this gap again.
if [ $heavy_rc -ne 0 ] && grep -q '^td-builder check: FATAL: could not read the host guix commit' "$hlog"; then
  env_error=1
  env_error_msg="runner host not provisioned: host guix missing/mismatched vs channels.scm (see issue #268) — no gate ran, not a code regression"
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
# harness (busybox+make, NO guix). The heavy ./check.sh above ran gate 420, which persists
# .td-build-cache/harness; consume it here. This is the loop the guix-less VM runs. Only
# attempt it when the harness was persisted (heavy green enough to reach gate 420); a missing
# harness is a heavy-suite problem already reported, not a separate harness failure.
if [ $env_error -eq 1 ]; then
  harness_rc=4; harness_fail="SKIPPED — $env_error_msg"
  echo ">> daily backstop: check-harness SKIPPED — $env_error_msg"
elif [ -d .td-build-cache/harness/store ] && [ -s .td-build-cache/harness/rel ]; then
  echo ">> daily backstop: td-builder check check-harness on origin/main ($main) — guix-free /td/store loop"
  "$TDB" check check-harness >"$xlog" 2>&1 || harness_rc=$?
  harness_fail=$(grep -E '^FAIL|^(check\.sh|td-builder check): FATAL' "$xlog" | head -5 | tr '\n' ';')
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
