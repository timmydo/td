// recipe-procs.ts — `procs` (a modern `ps` replacement), built FROM SOURCE by td. Continues
// moving the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) from guix-packaged to
// td-built-from-source via build-recipe (the uutils-`cat` pattern). The source is the upstream
// `procs` 0.14.10 crate tarball (keyed `procs-source`); its 297-crate closure is vendored as
// `.crate` fixed-output static.crates.io fetches in tests/procs.lock. Pure Rust (no C build).
// The .drv is assembled + realized by td (no guix (derivation …) / no guix-daemon); the
// rustc/cargo/gcc seed is external (§5, retired last).
//
// The guix-dependence census auto-classifies a buildSystem "rust" recipe as self-hosted
// (no (gnu packages) oracle by design) — no manual enrollment needed.
recipe({
  name: "procs",
  version: "0.14.10",
  buildSystem: "rust",
  bins: ["procs"],
});
