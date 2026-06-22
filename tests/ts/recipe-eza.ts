// recipe-eza.ts — `eza` (a modern `ls` replacement), built FROM SOURCE by td. Continues
// moving the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) from guix-packaged to
// td-built-from-source via build-recipe (the uutils-`cat` pattern). The source is the upstream
// `eza` 0.21.6 crate tarball (keyed `eza-source`); its 233-crate closure is vendored as
// `.crate` fixed-output static.crates.io fetches in tests/eza.lock.
//
// eza's `default` feature is `git` → git2 → libgit2-sys/libz-sys/openssl-sys, a C build the
// offline scrubbed build-env can't satisfy. Build with noDefaultFeatures (drops the git
// integration → pure Rust; eza is still a full ls replacement, just without the git column) —
// the same recipe cargo-feature path fd uses to drop jemalloc. The .drv is assembled +
// realized by td (no guix (derivation …) / no guix-daemon); the rustc/cargo/gcc seed is
// external (§5). Auto-classified self-hosted by buildSystem (no census enrollment).
recipe({
  name: "eza",
  version: "0.21.6",
  buildSystem: "rust",
  bins: ["eza"],
  noDefaultFeatures: true,
});
