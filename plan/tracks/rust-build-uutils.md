section: side
status: claimed
title: rust-build-uutils
handle: claude-fable-c018e3
date: 2026-06-18
notes: plan/rust-build-uutils.md
summary: Increment 3 of the rust-build arc — td builds a REAL uutils coreutils tool (uu_cat 0.9.0 -> the 'cat' binary) from source via build-recipe + vendored deps, no builder code change. Source + 139 dep crates are static.crates.io fixed-output fetches (Cargo.lock-pinned); guix/Guile off PATH; reproducible by td-builder check (verified on host). The real coreutils-replacement demo.
