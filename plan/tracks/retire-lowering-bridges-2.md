section: side
status: done
title: retire-lowering-bridges-2
handle: claude-fable-aca629
date: 2026-06-28
pr: 231
notes: plan/retire-lowering-bridges-2.md
summary: move-off-Guile §5 (north-star priority 3) — continue retire-lowering-bridges: retire 5 more `tests/*-drv.scm` Guile lowering bridges (registry, rollback, place, drv-emit, td-drv-add) by inlining `guix build -d -e`. The bridge's `.drv` is reproduced BYTE-IDENTICALLY by an `-e` expression that returns a `<derivation>` directly (open-connection → run-with-store → close), which hits guix's `derivation?` path and skips the `set-guile-for-build` the procedure/gexp paths inject. Pure refactor (same .drv, same output, same DIGESTS): −5 `.scm` (33→28), ~6 fewer `guix repl` sites; a `tools/guix-lower.sh` centralises the form. Bridges that ALSO write a spec / query path-info / carry a behavioral assertion / use inline `#~` gexps (generation-image, manifest-image, verify-place, daemon, build-hermetic, offline, td-drv-assemble, td-drv-build, td-builder-s4) are out of scope — oracle/input-resolution retired last.
