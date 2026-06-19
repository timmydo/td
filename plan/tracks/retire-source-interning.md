section: side
status: claimed
title: retire-source-interning
handle: claude-fable-510345
date: 2026-06-18
notes: plan/retire-source-interning.md
summary: move-off-Guile §5 — retire ALL THREE pure tree-interning Guile helpers (tests/td-builder-source.scm, td-vendor-demo-source.scm, td-russh-demo-source.scm). The gates' source PREP swaps `guix repl … lower-object` (daemon interns the tree + registers it in /var/guix/db) for td's OWN recursive addToStore (`td-builder store-add-recursive`, gate 285's primitive) via a tests/intern-src.sh helper: td interns the source into its OWN store dir + td.db, no daemon. build-recipe gains an optional source-store (SRC-STORE-DIR SRC-DB): it reads the no-ref source closure from td.db and stages the tree from the td store dir at its canonical path (an optional per-closure-entry on-disk location, carried through closure.txt so the `check` double-build honours it too). Gates 330 (rust-build) + 335 (rust-vendor) + 345 (rust-russh) lose their `guix repl`; durable run/repro/distinct legs unchanged. boot.scm is NOT in scope (config/image lowering, retired last).
