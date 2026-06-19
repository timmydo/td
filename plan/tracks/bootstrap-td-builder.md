section: side
status: claimed
title: bootstrap-td-builder
handle: claude-fable-2715d4
date: 2026-06-18
notes: plan/bootstrap-td-builder.md
summary: move-off-Guile §5 "build the seed with td" — a STAGE0 td-builder compiled straight from builder/ source by the pinned Rust toolchain (cargo, env -i, no guix/Guile/daemon, offline), breaking the bootstrap circularity where the first td-builder comes from `guix build -e '(@ (system td-builder) td-builder)'`. Brick 1: tools/bootstrap-td-builder.sh + a `bootstrap` gate proving stage0 is created guix-free, runs, is bit-reproducible (double-build), and is behaviorally equal to — yet a distinct binary from — the guix-built td-builder (own, then diverge). Next bricks: make the loop's builds USE stage0 as the in-store builder (build-recipe references the builder by store path ⇒ needs daemon-free placement).
