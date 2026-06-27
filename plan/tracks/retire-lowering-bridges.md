section: side
status: done
title: retire-lowering-bridges
handle: claude-fable-2715d4
date: 2026-06-19
notes: plan/retire-lowering-bridges.md
summary: move-off-Guile — retire the per-package Guile lowering bridges (tests/*-drv.scm). A package's derivation file name is `guix build -d -e '(@ (system M) pkg)'` and its output is `guix build -e '...'` — no `guix repl` script needed. Replaces the ts-eval-drv.scm (9 uses) and td-builder-drv.scm bridges across all gates, deleting 2 .scm files and ~10 guix-repl invocations; establishes the `-d -e` pattern for retiring the remaining package-lowering bridges.
