section: side
status: claimed
handle: claude-opus-7e12d1
date: 2026-06-21
title: host-sandbox-stage0
notes: plan/host-sandbox-stage0.md
summary: North-Star rung 1 (no guix process in a build path) — retire the LAST `guix build -e '(@ (system td-builder) td-builder)'` on the loop SPINE. `check.sh:190` builds the td-builder that BECOMES the host-sandbox container via guix's cargo-build-system (a guix packager process on the spine, before any gate runs). Swap it to the cargo-built STAGE0 (`tools/bootstrap-td-builder.sh`, the mechanism the gnu+rust gates already use via cache-lib load_stage0) — reads the pinned toolchain store paths from `tests/td-builder-rust.lock` as plain strings, NO guix invoked. This is the spine sibling of [[guix-builder-route]] (which routes the GATE tool-use sites; the spine `check.sh` site is out of its scope). Own, then diverge: stage0 is behaviorally equal to the guix-built td-builder (proven by the bootstrap gate, #93) but a distinct binary. Boundary: the toolchain SEED store paths must be present on the host (warmed channel closure today; the [[seed-tarball]] track serves them from the frozen tarball next — that retires `check.sh:196`'s `guix shell` toolchain, a separate site this track does NOT touch). Exclusive landing (check.sh is the shared spine) — sequence with the seed-tarball agent who also edits check.sh.
