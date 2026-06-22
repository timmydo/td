// recipe-fd.ts — `fd` (the fast find alternative), built FROM SOURCE by td. Part of the
// Rust-focused minimal distro: the shipped userland (procs/fd/ripgrep/sd/eza/bat, PR #80)
// moves from guix-packaged to td-built-from-source via build-recipe (buildSystem "rust",
// the uutils-`cat`/youki pattern). The source is the upstream `fd-find` 10.2.0 crate tarball
// (keyed `fd-source`); its 114-crate dependency closure is vendored as `.crate` fixed-output
// fetches in tests/fd.lock (TD_VENDOR_CRATES, sha256 == each Cargo.lock checksum). Pure-Rust
// deps — no crypto/C build (unlike russh's aws-lc), so run_rust needs no C build-env. The
// .drv is assembled + realized by td (no guix (derivation …) / no guix-daemon); the
// rustc/cargo/gcc seed is external (§5, retired last).
//
// Named "fd" (a from-source Rust tool with no (gnu packages) oracle by design — guix has no
// rust-fd-find), so it is enrolled in self-host-specs in tests/guix-dependence.scm like the
// other td-built Rust tools, NOT resolved to a guix oracle by the census.
// fd's `default` feature pulls `use-jemalloc` → jemalloc-sys, which runs a C
// ./configure the offline scrubbed build-env can't satisfy (needs sed + a working C
// toolchain on PATH). Build with the system allocator instead: noDefaultFeatures drops
// use-jemalloc, features keeps "completions" (shell completion generation). Pure Rust.
recipe({
  name: "fd",
  version: "10.2.0",
  buildSystem: "rust",
  bins: ["fd"],
  noDefaultFeatures: true,
  features: ["completions"],
});
