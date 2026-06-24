section: mainline
status: claimed
handle: claude-fable-65585b
date: 2026-06-24
title: corpus-crate-free-rest
summary: Phase-2a continuation of corpus-crate-free (#166, shipped Rust userland done). Scale the guix-free crate path (cargo-proxy + tools/warm-cargo-proxy.sh + shared tests/crate-free-build.sh) to the REMAINING crates.io corpus rust packages — uutils-coreutils (crate `coreutils` 0.9.0, 507 crates, bin coreutils) and youki (`youki` 0.6.0, 663 crates) — so they too build their crate closures GUIX-FREE (no guix build, no /gnu/store crate FOD, no oracle; content-address = the shipped Cargo.lock pin). Additive `rust-<p>-crate-free` gates alongside the guix-path gates (343/344), which stay the differential oracle ("own, then diverge"). russh (td-russh-demo, 188 crates) is a LOCAL demo source — not a crates.io package — so it needs a source-from-disk warm path, NOT the proxy throwaway-fetch; deferred. PHASE 2b (the real end goal: DROP the /gnu/store crate strings from the locks + retire the guix-path crate FODs) follows once the whole corpus is owned guix-free. Notes in plan/corpus-crate-free-rest.md.
