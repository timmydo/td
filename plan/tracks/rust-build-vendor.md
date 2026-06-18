section: side
status: claimed
title: rust-build-vendor
handle: claude-fable-c018e3
date: 2026-06-18
notes: plan/rust-build-vendor.md
summary: Increment 2 of the rust-build arc — td builds a Rust crate WITH dependencies. Each dep .crate is a fixed-output url-fetch (static.crates.io URL + Cargo.lock sha256); run_rust assembles a cargo vendor dir (unpack + minimal .cargo-checksum.json) and builds `cargo --offline --frozen`; build-recipe routes *.crate lock entries to TD_VENDOR_CRATES. Proven on a demo binary depending on itoa + ryu; reproducible by td-builder check, guix/Guile off PATH.
