#!/bin/sh
# tests/ts-eval-check.sh — the `ts-eval` rung driver (DESIGN §7.1 ts-frontend,
# sub-task 2). Proves the boa evaluator runs JS and that its curated global is
# hermetic, self-discriminating like the other rungs:
#
#   (1) a trivial expression evaluates to a known value — boa actually runs;
#   (2) `typeof Date === "undefined"` — the clock is REMOVED from the global
#       (verify red: stop deleting Date in the evaluator's prelude);
#   (3) `Math.random()` is DENIED — the evaluator exits nonzero with the denial
#       message (the always-on negative control: randomness must not slip
#       through, or eval is non-deterministic);
#   (4) Math otherwise still works — the curation is surgical, not a blanket
#       break (so a green is not the vacuous "everything throws").
#
# The Makefile `ts-eval` rung builds td-ts-eval and passes its binary in.
#
# Input (env): TD_TS_EVAL — the td-ts-eval binary.
set -eu

: "${TD_TS_EVAL:?TD_TS_EVAL (the td-ts-eval binary) must be set}"
test -x "$TD_TS_EVAL" || { echo "FAIL: $TD_TS_EVAL is not executable" >&2; exit 1; }

evaljs() { printf '%s' "$1" | "$TD_TS_EVAL"; }

echo ">> (1) boa evaluates a trivial expression to a known value"
got="$(evaljs '1 + 2 * 3')"
test "$got" = "7" || { echo "FAIL: expected 7, got '$got' — boa did not evaluate the expression." >&2; exit 1; }
echo "   ok: 1 + 2 * 3 => 7"

echo ">> (2) curated global: the clock (Date) is removed"
got="$(evaljs 'typeof Date')"
test "$got" = "undefined" || { echo "FAIL: Date is still present (typeof Date = '$got') — the clock was not removed; eval is not hermetic." >&2; exit 1; }
echo "   ok: typeof Date => undefined"

echo ">> (3) curated global: Math.random() is denied (negative control)"
if out="$(evaljs 'Math.random()' 2>&1)"; then
  echo "FAIL: Math.random() was ALLOWED (=> '$out') — randomness is not denied; eval is not deterministic." >&2
  exit 1
fi
printf '%s' "$out" | grep -q 'Math.random is denied' \
  || { echo "FAIL: Math.random() failed, but NOT with the denial message — an unrelated error must not green this control: $out" >&2; exit 1; }
echo "   ok: Math.random() denied"

echo ">> (4) curation is surgical: Math otherwise still works"
got="$(evaljs 'Math.max(1,4,2)')"
test "$got" = "4" || { echo "FAIL: Math.max broke (got '$got') — curation over-reached; a vacuous all-throws global would falsely green (3)." >&2; exit 1; }
echo "   ok: Math.max(1,4,2) => 4"

# --- (5) HERMETIC PROBES (DESIGN §7.1 acceptance #3) --------------------------
# A spec attempting I/O — network / fs / clock / randomness — must be REJECTED
# by the evaluator. Each probe MUST fail (the evaluator exits nonzero) AND fail
# for the right reason (the named global is absent or denied), so an unrelated
# error cannot green a probe. boa ships no fetch/fs/process/web APIs, and the
# curated prelude removes the clock + denies randomness; this asserts all of
# that as one I/O-rejection gate. The positive control is legs (1)/(4) above —
# benign code DOES evaluate — so this is not the vacuous "everything throws".
echo ">> (5) hermetic probes: every I/O attempt is rejected (network/fs/clock/randomness)"
# label | probe JS | substring the failure message must contain
probes='clock|new Date()|Date is not defined
randomness|Math.random()|Math.random is denied
network|fetch("http://example.invalid")|fetch is not defined
filesystem|require("fs")|require is not defined
process|process.exit(0)|process is not defined'

printf '%s\n' "$probes" | while IFS='|' read -r label js want; do
  test -n "$label" || continue
  if out="$(printf '%s' "$js" | "$TD_TS_EVAL" 2>&1)"; then
    echo "FAIL: $label probe was ALLOWED — \`$js\` evaluated to '$out'; the evaluator is not hermetic." >&2
    exit 1
  fi
  printf '%s' "$out" | grep -qF "$want" \
    || { echo "FAIL: $label probe failed, but NOT for the expected reason (wanted '$want'): $out" >&2; exit 1; }
  echo "   ok: $label rejected (\`$js\`)"
done || exit 1

echo "PASS: boa evaluates JS; the curated global removes the clock and denies randomness while leaving Math otherwise intact; every I/O attempt (network/fs/clock/randomness) is rejected (hermetic, deterministic)."
