section: side
status: done
title: guix-builder-route
handle: claude-fable-a94246
date: 2026-06-21
notes: plan/guix-builder-route.md
summary: move-off-Guile §5 — lower the guix-surface packager count (#114) by routing the loop's td-builder TOOL-USE sites off `guix build -e '(@ (system td-builder) td-builder)'` onto the td-bootstrapped stage0 builder (cache-lib load_stage0 / TB), the mechanism the gnu+rust gates already use. The guix-built td-builder stays ONLY at the genuine oracle legs (rust-build gtb 330, bootstrap gtb 170, the td-builder package gate 175). PR 1: the 8 store-backend gates (register/add/add-tree/gc/verify/gc-sweep/add-referenced/backend) — count 34→26. Follow-on PRs: drv-* (230/235/240/245), loop-* (265/270), td-check/resolve/rootless/sandbox-hardening/td-realize/td-offline/build-hermetic, then td-ts-eval routing (step 3). Each routed gate keeps its behavioral assertion (proves stage0's td-builder does the op) + a durable "tb is the stage0 path" structural leg, and re-baselines tests/guix-surface.expected (the ratchet locks the shrink).
