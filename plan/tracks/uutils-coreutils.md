section: side
status: done
title: uutils-coreutils
handle: claude-opus-3267ea
date: 2026-06-20
pr: 123
notes: plan/uutils-coreutils.md
summary: td builds the FULL uutils coreutils multicall binary (crate `coreutils` 0.9.0, default feat_common_core — ls/cp/mv/rm/cat/…) from source via td-builder's rust-build path — extending the uu_cat demo to the whole Rust coreutils. recipe-uutils.ts + a 507-crate vendored lock + a gate (build + multicall behavioral + repro). First step of the Rust-focused minimal distro (human steer 2026-06-20): the userland becomes td-built Rust from source, not guix-packaged. Follow-up: ship it in td.scm.
