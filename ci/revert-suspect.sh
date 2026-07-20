#!/usr/bin/env bash
# revert-suspect.sh — form the revert of the commit that broke main.
#
# The optimistic-merge heal primitive (DESIGN §7.2). Main is NON-strict, so two
# independently-green branches can squash-land into a red main (green(A)+green(B)
# != green(A∪B)). Healing is an AGENT DUTY, not an automated workflow: when an
# agent sees a gate red on main, it runs this to revert the suspect — the squash
# commit at main's HEAD — onto a heal/revert-<sha> branch, and the integrator
# squash-ins that branch so main returns to green with no human rebase toil.
# Squash (one commit per landing) is what makes the suspect unambiguous and the
# revert atomic. GitHub is a backup remote only — this forms the revert branch
# locally; landing is the integrator's `git squash-in` (AGENTS.md "Parallel work").
#
#   ci/revert-suspect.sh                 # revert HEAD on a new heal/revert-<sha> branch
#   ci/revert-suspect.sh --ref <sha>     # revert a specific commit
#
# The revert branch is based on the CURRENT HEAD (main) and reverts the suspect,
# so it stays a minimal revert even if main advanced past the suspect.
#
# Loop guard: refuses to revert a commit that is ITSELF a revert (subject starts
# "Revert ") and exits 3 — a revert that itself reds main is a human's problem,
# not something to revert-storm.
set -euo pipefail

ref=HEAD
while [ $# -gt 0 ]; do
  case "$1" in
    --ref) ref=$2; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

sha=$(git rev-parse --verify "$ref^{commit}")
short=$(git rev-parse --short "$sha")
subject=$(git log -1 --format=%s "$sha")

# Loop guard: never revert a revert.
case "$subject" in
  "Revert "*|"revert "*)
    echo "refusing: $short is itself a revert ('$subject') — heal loop guard; a human should look" >&2
    exit 3 ;;
esac

# Squash landings are single-parent (git revert needs no -m); guard a stray true
# merge commit anyway.
parents=$(git rev-list --parents -n1 "$sha" | wc -w)
mflag=()
[ "$parents" -gt 2 ] && mflag=(-m 1)

branch="heal/revert-$short"
git switch -c "$branch"
git revert --no-edit "${mflag[@]}" "$sha"
echo "prepared revert of $short (\"$subject\") on branch $branch"
echo "land it: the integrator runs 'git squash-in $branch' on main, then pushes."
