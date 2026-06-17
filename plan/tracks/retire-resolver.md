section: mainline
status: claimed
title: retire-resolver
handle: claude-fable-2715d4
date: 2026-06-16
notes: plan/retire-resolver.md
summary: Retire input resolution off Guile — regenerate the lock from td's OWN reconstructed recipes (package-by-package, toolchain last) instead of Guile's specification->package. First step landed: gettext-minimal's lock entry is sourced from td's recipe (td-build lowering, builder=td-builder), `td-builder resolve` returns it, and it diverges from specification->package (own, then diverge); ncurses (no td recipe) still resolves via Guile (td-resolve-recipe gate). Next: a generator that builds the whole lock from recipes (fallback to Guile for names without), then swap nano's resolved build onto the td-sourced lock.
