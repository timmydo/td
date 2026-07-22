---
title: Move the workflow off GitHub features (backup-only remote)
labels: [infra]
blocked-by: none
---

## What

GitHub (and the sr.ht mirror) is a git backup remote only — nothing reads or
writes the GitHub API. The backlog, claiming, landing, code review, CI, and the
daily backstop all move into the repo + local git. `gh` is removed from the
tree; branch protection and GitHub Actions are retired.

## Entry points

- `issues/README.md`, `issues/TEMPLATE.md` — the file-based backlog.
- `AGENTS.md` "Parallel work", principle 2, principle 5, "Tests" — rewritten for
  file backlog, branch-as-claim, single-integrator `git squash-in` landing, and
  reviews recorded in the commit message.
- `.github/` — deleted (workflows, branch protection, issue template).
- `ci/revert-suspect.sh` — git-only (drop `--open-pr`/`gh`).
- `ci/daily-backstop.md` — host-cron only (drop the Action + `gh`).
- Local gate: `td-builder affected-checks --run` absorbs the ci.yml lint steps.

## Done

A fresh agent, following only AGENTS.md, can pick an item from `issues/open/`,
claim it by pushing `issue-NNNN-*`, land it via the integrator's `git squash-in`
with reviews in the commit message, and close it with `git mv` — with no `gh`
invocation anywhere in the tree (`grep -rn "gh " --include=*.sh --include=*.md`
finds only prose references, no calls) and `.github/` absent.

## Collisions

Touches `AGENTS.md`, `.github/**`, `ci/revert-suspect.sh`, `ci/daily-backstop.md`,
and adds `issues/**`. Also folds the retired ci.yml lint checks (shell `bash -n`,
`tests/heal-revert.sh`, no-tabs-in-`.scm`) into the local gate so coverage is not
lost — coordinate with anything touching `builder/src/check*.rs`.
