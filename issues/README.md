# Backlog

The work backlog is plain markdown files, one per work item — no GitHub Issues.
GitHub (and the sr.ht mirror) is only a git backup remote now.

- **Menu** — `ls issues/open/` is the open backlog. Closed items live in
  `issues/closed/`, kept for history and so `re #N` references resolve.
- **File** — `issues/open/NNNN-slug.md`, `NNNN` a zero-padded id, from
  `TEMPLATE.md`. Allocate the next free id as `max(all ids in issues/) + 1`
  (ids continue the old shared GitHub issue/PR number space; the last used was
  #553). The integrator renames on the rare collision at landing.
- **Claim** — push a branch `issue-NNNN-slug`. The claim board is
  `git ls-remote --heads origin 'issue-*'`; a claimed branch with no new
  commits for a few days is reclaimable. (This replaces the draft-PR claim.)
- **Close** — `git mv issues/open/NNNN-*.md issues/closed/` in the landing
  commit. The directory is the sole source of truth for open/closed; there is
  no status field to maintain, and no closing keyword fires anything.
- **Reference** an item you are NOT closing with `re #N` / `see #N`.

The `daily-red` backstop signal is a well-known `issues/open/daily-red.md`
the daily agent writes on a real regression and `git mv`s to closed when the
fix/revert lands (see `ci/daily-backstop.md`).

The full claim → review → land protocol is in `AGENTS.md`, "Parallel work".
