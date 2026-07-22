---
title: Harden the shared ~/.td/sources cache against staleness and cross-worktree races
labels: [bootstrap, robustness]
blocked-by: none
---

## What

The warmed-source cache is now a single shared `$HOME/.td/sources` across all
worktrees (previously per-worktree `<root>/.td-build-cache/sources`). Sharing is
the point — one `td-feed warm sources` warms every tree — but a permanent,
mutable, filename-keyed cache surfaces three second-order hazards that the
per-worktree layout hid. None currently produces a wrong build (each fails safe
against the pinned sha256 / compiled seed-digest), but each degrades diagnostics
or auto-recovery. Harden them.

1. **Stale kernel-header version masks a failed warm.** `kh_seed_present`
   (`builder/src/check_loop.rs`) matches any `linux-headers-*-<arch>.tar`,
   version-independent. In a permanent shared cache old versions accumulate, so
   after a Linux pin bump a *failed* warm of the new version is masked by a
   lingering old `.tar` → the fresh-`td-feed` retry in `warm_kh_arches` is
   suppressed → the ladder later interns the exact new version and fails with a
   confusing "not warm". Make the presence check version-exact (derive the
   expected version from the `linux-source` pin, as feed's
   `warm_kernel_headers_from_pins` already does). Builder currently has neither a
   Linux-version parser nor a public pin loader, so this adds a small amount of
   surface to the best-effort warm path.

2. **Derived headers are trusted by filename, not by generator identity.**
   `warm_kernel_headers_from_pins` (`feed/src/main.rs`) returns early on
   `out.exists()`. A Linux *version* bump changes the filename (regenerates), but
   a change to the header-generation logic at the same version leaves an
   identically named stale entry that re-running `td-feed warm sources` never
   repairs (the compiled seed-digest gate rejects the stale bytes, so it fails
   safe rather than silently wrong). Key derived entries by source+generator
   identity, or verify-and-replace on mismatch.

3. **Verify→consume is not atomic (TOCTOU).** `verified_source_tarball`
   (`builder/src/bootstrap.rs`) hashes the file, closes it, and returns the
   *path*; consumers (`unpack_stage0_source`, `build_mes`) reopen that path to
   extract. In a shared mutable cache another worktree could replace the file
   between verify and reopen. In normal operation the window does not open —
   `warm_sources` skips an already-present+verified tarball (`continue // already
   warm + verified`) rather than rewriting it, and pins are version-stamped so
   same-name/different-bytes does not occur — but a digest-keyed (CAS) layout or
   a verify-on-a-private-snapshot would close it structurally. This is also a
   pre-existing property of the shared `~/.td/feed/store`.

## Entry points

- `builder/src/check_loop.rs` — `kh_seed_present`, `warm_kh_arches` (finding 1).
- `feed/src/main.rs` — `warm_kernel_headers_from_pins`, `warm_sources`,
  `sources_dir` (findings 1, 2).
- `builder/src/bootstrap.rs` — `verified_source_tarball`, `shared_sources_dir`,
  its consumers `unpack_stage0_source`/`build_mes` (finding 3).
- `recipes/src/bin/td_recipe_eval/check_runner.rs` — `shared_sources_dir`,
  `intern_source`/`intern_linux_headers` (the ladder read path).
- Gates: `bootstrap-*` (the heavy source-bootstrap ladder) exercise the warm +
  intern path end to end; `td-feed warm sources` / `warm kernel-headers <arch>`
  are the warmers.

## Done

- A test bumps (or simulates bumping) the Linux pin with a stale
  `linux-headers-<old>-<arch>.tar` present in the shared cache and asserts
  `warm_kh_arches` reports the arch as missing (retry not suppressed) — i.e. the
  presence check is version-exact.
- A test asserts a stale same-version derived header is refreshed (or rejected
  and re-produced) by re-running the warm, not silently kept.
- The verify→consume path either operates on a digest-keyed entry or a private
  snapshot, demonstrated by a test that mutates the shared path after verify and
  shows the consumer still builds the verified bytes (or fails closed).

## Collisions

- `builder/src/check_loop.rs` — exclusive-landing file; coordinate.
- `feed/src/main.rs`, `builder/src/bootstrap.rs`,
  `recipes/src/bin/td_recipe_eval/check_runner.rs` — the sources-cache resolvers
  are duplicated across these three dependency-free crates and must stay
  identical; change all three together.
- Disjoint from any active `issue-*` branch touching the bootstrap ladder or the
  feed warmers — check `git ls-remote --heads origin 'issue-*'` before claiming.
