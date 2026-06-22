// recipe-sd.ts — `sd` (the intuitive find-and-replace, a `sed` alternative), built FROM
// SOURCE by td. Continues moving the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat,
// PR #80) from guix-packaged to td-built-from-source via build-recipe (the uutils-`cat`
// pattern). The source is the upstream `sd` 1.0.0 crate tarball (keyed `sd-source`); its
// 111-crate closure is vendored as `.crate` fixed-output static.crates.io fetches in
// tests/sd.lock. Pure Rust (no C build). The .drv is assembled + realized by td (no guix
// (derivation …) / no guix-daemon); the rustc/cargo/gcc seed is external (§5, retired last).
//
// The guix-dependence census auto-classifies a buildSystem "rust" recipe as self-hosted
// (no (gnu packages) oracle by design) — no manual enrollment needed.
recipe({
  name: "sd",
  version: "1.0.0",
  buildSystem: "rust",
  bins: ["sd"],
});
