#!/bin/sh
# Apply the main-branch protection ruleset for td (DESIGN §7.2: PR-gated
# landings). Run by a repo admin with an authenticated `gh` (gh auth login).
#
#   ./.github/setup-branch-protection.sh
#
# Both GitHub-hosted jobs are REQUIRED status checks: `lint` and `check-fast`.
# The td-ci-fast store image is published to GHCR (ci/build-ci-image.sh) and
# `check-fast` has been green on recent PRs, so it is now mandatory — a PR is
# not mergeable until it passes.
# (Since #26 CI runs the FAST tier only — `./check.sh check-fast`. The full
# hermetic loop is the dev-machine gate, DESIGN §7.2 step 2, plus the image
# pipeline's validate job; it is NOT a per-PR status check, so this requires the
# `check-fast` context — there is no `check` job.)
#
# What the ruleset enforces on main:
#   - no direct pushes: changes land only via pull request;
#   - 1 approving review, NOT dismissed on new pushes (so the bot's rebases /
#     force-pushes don't drop the human approval — #82, dismiss_stale=false);
#   - required status checks (strict: branch must be current with main);
#   - linear history (rebase/squash merges only — matches the repo's
#     fast-forward convention);
#   - no force pushes, no branch deletion.
#
# NOTE (enforcement): GitHub only enforces rulesets on PRIVATE repos under
# GitHub Pro / a paid org plan. On a free private repo this applies but does
# not enforce. See .github/BRANCH-PROTECTION.md.
#
# NOTE (reviews): a PR author cannot approve their own PR. With a single
# GitHub account, required reviews deadlock — agent branches must be pushed
# by a second (machine) account so the human account can approve.
set -eu

repo=$(gh repo view --json nameWithOwner -q .nameWithOwner)
checks='{"context": "lint"}, {"context": "check-fast"}'

# Prefer rebase/squash merges; merge commits would break the linear-history
# rule below.
gh api -X PATCH "repos/$repo" \
  -F allow_merge_commit=false \
  -F allow_rebase_merge=true \
  -F allow_squash_merge=true \
  -F delete_branch_on_merge=true >/dev/null
echo "repo merge settings: rebase/squash only, auto-delete merged branches"

# Replace any previous version of the ruleset (idempotent re-runs).
existing=$(gh api "repos/$repo/rulesets" -q '.[] | select(.name == "protect-main") | .id' | head -n1 || true)
method="POST"
path="repos/$repo/rulesets"
if [ -n "$existing" ]; then
  method="PUT"
  path="repos/$repo/rulesets/$existing"
fi

gh api -X "$method" "$path" --input - <<EOF >/dev/null
{
  "name": "protect-main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
  },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_linear_history" },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 1,
        "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": false,
        "allowed_merge_methods": ["rebase", "squash"]
      }
    },
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": true,
        "required_status_checks": [ $checks ]
      }
    }
  ],
  "bypass_actors": []
}
EOF
echo "ruleset 'protect-main' applied ($method): PRs only, 1 review (not dismissed on push), required checks: lint + check-fast${existing:+ (replaced previous)}"
exit 0
