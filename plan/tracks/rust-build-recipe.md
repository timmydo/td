section: side
status: claimed
title: rust-build-recipe
handle: claude-fable-c018e3
date: 2026-06-18
notes: plan/rust-build-recipe.md
summary: route the rust-build self-host onto the own-builder-daemon build-recipe rail — `build-recipe` dispatches buildSystem ("gnu"→autotools-build, "rust"→rust-build); td-builder self-hosts via a TS recipe + pinned lock, .drv assembled by td (no guix (derivation …)) + realized daemon-free (no guix-daemon), guix/Guile scrubbed from PATH. Replaces the Guile td-rust-selfhost-derivation.
