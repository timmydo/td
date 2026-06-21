section: side
status: claimed
title: youki
handle: claude-opus-3267ea
date: 2026-06-21
notes: plan/youki.md
summary: td builds youki (the Rust OCI container runtime, crate `youki` 0.6.0) FROM SOURCE via td-builder's rust-build path — the postponed crun replacement, unlocked now that td builds Rust from source. Same playbook as uutils-coreutils (#123): recipe-youki.ts (buildSystem rust, bins [youki]) + a 663-crate vendored lock + a gate. No default features (no seccomp/systemd/wasm) so it builds with the standard rust/gcc seed — no libseccomp/pkg-config. Rust-focused minimal distro: another userland tool td-built from source into td's own store, not guix-packaged.
