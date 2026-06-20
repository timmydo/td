#!/bin/sh
# tests/ts-check.sh — the `ts` rung driver (DESIGN §7.1 ts-frontend, sub-task 1).
#
# Proves the TypeScript spec front-end is real and load-bearing, three ways,
# self-discriminating (the always-on negative control is baked in, like the
# `diff`/`oci-diff` rungs' DISCRIMINATE leg):
#
#   (1) TYPE-CHECK GOOD   — `tsc` accepts the well-typed v0 spec (exit 0). If a
#       valid spec stopped type-checking, the dialect is broken: red.
#   (2) TYPE-CHECK BAD    — `tsc` REJECTS spec-bad-fstype.ts (rootFsType "ext3",
#       outside the union) with a TYPE error (TS2322 on "ext3"/RootFsType), not
#       merely a nonzero exit. This is the verified-red baked in: if the types
#       stop being load-bearing — a wrong fs type slips through — this leg reds.
#   (3) TRANSPILE GOLDEN  — `tsc` emits the v0 spec to JS byte-identical to the
#       committed golden tests/ts/spec-v0.expected.js (types stripped). Verify
#       red by corrupting the golden.
#
# tsc does BOTH the check and the emit (human, 2026-06-13 — the pinned channel
# has no `swc` CLI that works and no `tsc` package; plan/ts-frontend.md). The
# Makefile rung resolves the pinned `node` and `td-typescript` and passes them in
# so this script never calls guix.
#
# tsgo migration (2026-06-20): the transpiler is now the TypeScript 7 NATIVE
# compiler (td-tsgo, `lib/tsc` — a static Go binary), so the check + emit run with
# NO node/V8. Proven drop-in: identical TS2322 on the bad spec + byte-identical emit.
#
# Inputs (env, set by the Makefile `ts` rung):
#   TD_TSGO   — the td-tsgo output dir (native tsc at $TD_TSGO/lib/tsc).
#   TD_TSDIR  — the tests/ts fixture dir.
#
# No `diff`/`cmp`: the check.sh sandbox has no diffutils (a recorded gotcha), so
# the golden compare is sha256 over the bytes, falling back to a string compare.
set -eu

: "${TD_TSGO:?TD_TSGO (the td-tsgo native compiler dir) must be set}"
: "${TD_TSDIR:?TD_TSDIR (the tests/ts fixture dir) must be set}"

tsc_bin="$TD_TSGO/lib/tsc"
dialect="$TD_TSDIR/td-spec.d.ts"
good="$TD_TSDIR/spec-v0.ts"
bad="$TD_TSDIR/spec-bad-fstype.ts"
golden="$TD_TSDIR/spec-v0.expected.js"

for f in "$tsc_bin" "$dialect" "$good" "$bad" "$golden"; do
  test -e "$f" || { echo "FAIL: missing input: $f" >&2; exit 1; }
done

# Pinned tsc flags. Shared check/emit knobs make the run deterministic and
# hermetic (a fixed lib, no ambient @types pulled — there is no node_modules):
common="--strict --target es2020 --lib es2020"
tsc() { "$tsc_bin" $common "$@"; }   # native tsgo binary — no node

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# --- (1) TYPE-CHECK GOOD: the well-typed spec must pass -----------------------
echo ">> (1) type-check GOOD: tsc accepts the well-typed v0 spec"
if ! tsc --noEmit "$dialect" "$good" >"$work/good.log" 2>&1; then
  echo "FAIL: tsc REJECTED the well-typed v0 spec — the dialect or spec is broken:" >&2
  cat "$work/good.log" >&2
  exit 1
fi
echo "   ok: spec-v0.ts type-checks clean"

# --- (2) TYPE-CHECK BAD: the ill-typed spec must fail, AS A TYPE ERROR --------
echo ">> (2) type-check BAD: tsc rejects rootFsType \"ext3\" with a type error"
if tsc --noEmit "$dialect" "$bad" >"$work/bad.log" 2>&1; then
  echo "FAIL: tsc ACCEPTED spec-bad-fstype.ts (rootFsType \"ext3\") — the types are NOT load-bearing; a value outside the RootFsType union slipped through." >&2
  cat "$work/bad.log" >&2
  exit 1
fi
if ! grep -q 'TS2322' "$work/bad.log" || ! grep -q 'ext3' "$work/bad.log"; then
  echo "FAIL: the bad spec was rejected, but NOT with the expected type error (TS2322 on \"ext3\") — an unrelated failure must not green this control:" >&2
  cat "$work/bad.log" >&2
  exit 1
fi
echo "   ok: rejected with TS2322 — $(grep -m1 TS2322 "$work/bad.log" | sed 's/^.*error //')"

# --- (3) TRANSPILE GOLDEN: emit must equal the committed golden ---------------
echo ">> (3) transpile GOLDEN: tsc emits spec-v0.ts to the committed JS, byte for byte"
tsc --newLine lf --removeComments --outDir "$work/emit" "$dialect" "$good"
emit="$work/emit/spec-v0.js"
test -s "$emit" || { echo "FAIL: tsc emitted no JS for spec-v0.ts." >&2; exit 1; }

same=no
if command -v sha256sum >/dev/null 2>&1; then
  he="$(sha256sum < "$emit" | cut -d' ' -f1)"
  hg="$(sha256sum < "$golden" | cut -d' ' -f1)"
  test "$he" = "$hg" && same=yes
else
  test "$(cat "$emit")" = "$(cat "$golden")" && same=yes
fi

if test "$same" != yes; then
  echo "FAIL: emitted JS does not match tests/ts/spec-v0.expected.js." >&2
  echo "----- emitted -----" >&2; cat "$emit" >&2
  echo "----- golden  -----" >&2; cat "$golden" >&2
  exit 1
fi
echo "   ok: emit is byte-identical to the golden (types stripped)"

echo "PASS: TS spec front-end — well-typed spec checks + emits the golden; an out-of-union rootFsType is rejected (TS2322)."
