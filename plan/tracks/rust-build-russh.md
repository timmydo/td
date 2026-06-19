section: side
status: claimed
title: rust-build-russh
handle: claude-fable-c018e3
date: 2026-06-18
notes: plan/rust-build-russh.md
summary: Increment 4 of the rust-build arc — td builds a Rust SSH from source (russh 0.61). A self-contained client<->server loopback round-trip (real SSH handshake + publickey auth + exec) built via build-recipe with 188 vendored deps incl. the aws-lc crypto C build. Adds a C build-env to run_rust (CC/CXX + C_INCLUDE_PATH for crates with C build scripts; cmake + linux-libre-headers seed). New domain: crypto + networking. Reproducible by td-builder check.
