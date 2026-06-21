# host-sandbox-stage0 — retire the spine's last guix-as-packager site

**Handle:** claude-opus-7e12d1 · **Claimed:** 2026-06-21 · base: origin/main @ b0994e7 (#133)

## Goal (North-Star rung 1: no guix process in a build path)

`check.sh:190`:
```
tb=$(guix build -L . -e '(@ (system td-builder) td-builder)')/bin/td-builder
```
is the LAST `guix build -e (@ (system M) PKG)` packager invocation on the loop **spine**
— it runs on the host, before the sandbox exists, to produce the td-builder binary that
BECOMES the host-sandbox container. Everything else (the gate tool-use sites) was routed
onto the cargo-built stage0 by [[guix-builder-route]]; the spine site is outside that
track's scope, so it's still guix.

Swap it for `tools/bootstrap-td-builder.sh` (stage0): cargo compiles `builder/` from
source against the pinned toolchain store paths read from `tests/td-builder-rust.lock`
as plain strings — `env -i`, offline, **no guix/guile on PATH** (the script already
asserts this). Same mechanism the gnu+rust gates use via cache-lib `load_stage0`.

## Why this is honest (own, then diverge)

- stage0 is **behaviorally equal** to the guix-built td-builder (the `bootstrap` gate,
  #93, proves it: created guix-free, runs, bit-reproducible double-build, behaviorally
  equal to yet a distinct binary from the guix-built one).
- So the host-sandbox built BY stage0 must run the whole loop identically — the durable
  proof is simply that `./check.sh` stays green with the spine builder swapped.
- The guix-built td-builder survives ONLY where it is a genuine ORACLE (rust-build
  self-host gate 330, bootstrap gate 170, the td-builder package gate 175) — untouched.

## Scope boundary (what this track does NOT do)

- `check.sh:196` `guix shell … --search-paths` (the sandbox toolchain PATH) — that's the
  toolchain SEED, retired by [[seed-tarball]] (serve it from the frozen tarball). Separate
  site, separate track. NOT touched here.
- `check.sh:75` `guix describe` (pin check) — could be replaced by reading the pin from
  channels.scm directly, but it's a verification call, not a build/packager path. Out of
  scope unless trivial to fold in.

## Open question to resolve first

Are the pinned toolchain store paths in `tests/td-builder-rust.lock` guaranteed present
on the host at prelude time (line 190 runs BEFORE the `guix shell` warm on 196)? Today
`guix build -e (system td-builder)` realizes td-builder's closure (incl. the toolchain)
as a side effect. stage0's bootstrap only READS those paths — if a cold host lacks them
the bootstrap fails with "pinned seed not present". Plan: confirm they're in the warmed
channel closure; if a cold host can miss them, add a minimal warm (realize the lock's
toolchain paths — a fixed-input realize like warm-tsgo, NOT a packager `-e`).

## Sub-task ladder

1. [ ] Confirm the toolchain-lock paths are present at prelude time (or add a warm).
2. [ ] Swap `check.sh:190` to call `tools/bootstrap-td-builder.sh` into a cached dir;
       point the host-sandbox `tb` at the stage0 binary.
3. [ ] `./check.sh` green end-to-end (the durable proof — the loop runs on a stage0-built
       host-sandbox).
4. [ ] Verified-red: break the bootstrap path (e.g. point at a bad lock) and watch the
       prelude fail closed (no silent guix fallback).
5. [ ] guix-surface / guix-dependence unaffected (spine site isn't in the ratchet's
       scanned set — confirm, don't assume).

## Verified-red log

(to be filled)

## Notes

- Exclusive landing: `check.sh` is the shared spine. Announce + sequence with the
  seed-tarball agent (also edits check.sh for the `:196` toolchain seed).
- Full-loop escalation is mandatory for check.sh changes (affected-checks: loop spine).
