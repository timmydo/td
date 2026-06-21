// recipe-youki.ts — youki, the Rust OCI container runtime, built from source by td.
// The postponed crun replacement (a from-source Rust runtime beats a prebuilt blob),
// unlocked now that td-builder builds Rust from source (rust-build path). The published
// `coreutils`-style `youki` crate 0.6.0 produces the `youki` binary ([[bin]] name
// "youki"). buildSystem "rust" selects td-builder's cargo phase runner; plain `cargo
// build --release` builds it — youki has NO default features, so no seccomp/systemd/wasm
// (build.rs only probes libseccomp/pkg-config under CARGO_FEATURE_SECCOMP, and falls back
// when git is absent), so it builds from the vendored crates + the standard rust/gcc seed
// with no extra system deps. The 663-crate dependency closure is vendored as `.crate`
// fetches in the lock (TD_VENDOR_CRATES); the source is the upstream `youki` crate tarball,
// keyed `youki-source` (so no fetchSource). build-recipe assembles + realizes the .drv with
// no guix (derivation …) / no guix-daemon; the rustc/cargo/gcc seed is external (§5).
//
// A from-source Rust tool, no (gnu packages) oracle by design — listed in self-host-specs
// (like cat/uutils). Rust-focused minimal distro: the userland is td-built Rust from source.
recipe({
  name: "youki",
  version: "0.6.0",
  buildSystem: "rust",
  bins: ["youki"],
});
