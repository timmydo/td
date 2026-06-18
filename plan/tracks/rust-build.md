section: side
status: claimed
title: rust-build
handle: claude-fable-a00773
date: 2026-06-17
notes: plan/rust-build.md
summary: give td-builder its OWN Rust-from-source build path (buildSystem:"rust", a cargo phase runner mirroring autotools-build) so td builds Rust crates with no gnu-build-system and no Guix cargo-build-system — rustc/gcc seed external (§5). Proven self-hosting (td-builder builds td-builder), then vendored-deps, then a uutils tool. Durable legs: binary runs + td's own double-build; guix cargo-build-system is the removable migration oracle.
