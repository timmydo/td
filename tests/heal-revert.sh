#!/usr/bin/env bash
# Test ci/revert-suspect.sh — the optimistic-merge heal primitive (DESIGN §7.2).
#
# DURABLE assertion (no Guix oracle): the heal actually does its job — the revert
# reverses the suspect's change — and it self-guards against revert storms
# (refuses to revert a revert). git-driven, and the loop sandbox has no git
# (like no diffutils/awk), so this runs in CI's `lint` job (hosted, has git) —
# wired in .github/workflows/ci.yml — not as a ./check.sh loop gate.
set -euo pipefail

script=$(cd "$(dirname "$0")/.." && pwd)/ci/revert-suspect.sh
test -x "$script" || { echo "FAIL: $script not executable" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

git init -q
git config user.email heal-test@td.local
git config user.name  heal-test
git config commit.gpgsign false

printf 'good\n' > f.txt
git add f.txt
git commit -qm "base: good content"

printf 'BROKEN\n' > f.txt
git commit -qam "bad: breaks main"
bad=$(git rev-parse --short HEAD)

# 1. Revert the suspect (HEAD) — the durable "it does its job" leg.
"$script" >/dev/null
test "$(git rev-parse --abbrev-ref HEAD)" = "heal/revert-$bad" \
  || { echo "FAIL: not on heal/revert-$bad (on $(git rev-parse --abbrev-ref HEAD))" >&2; exit 1; }
case "$(git log -1 --format=%s)" in
  "Revert "*) ;;
  *) echo "FAIL: HEAD is not a revert commit" >&2; exit 1 ;;
esac
test "$(cat f.txt)" = "good" \
  || { echo "FAIL: revert did not restore good content (got '$(cat f.txt)')" >&2; exit 1; }
echo "ok: revert of $bad restored good content on heal/revert-$bad"

# 2. Loop guard — refuse to revert a revert (HEAD is now the revert commit).
rc=0
"$script" --ref HEAD >/dev/null 2>&1 || rc=$?
test "$rc" -eq 3 \
  || { echo "FAIL: loop guard did not fire on a revert commit (rc=$rc, want 3)" >&2; exit 1; }
echo "ok: loop guard refused to revert a revert (rc=3)"

echo "PASS: heal-revert"
