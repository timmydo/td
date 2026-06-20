section: side
status: claimed
title: corpus-leaf-recipes
handle: claude-opus-69899c
date: 2026-06-19
notes: plan/corpus-leaf-recipes.md
summary: add OWNED recipes for pure-autotools LEAF packages (which + gperf; m4 dropped — read-only bootstrap defeats patch_shebangs) — each builds via td-builder build-recipe with guix/Guile off PATH, runs/ships, is reproducible by td-builder check, and diverges when perturbed; moves the corpus-union census 23/320 -> 25/324.
