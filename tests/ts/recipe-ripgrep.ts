// recipe-ripgrep.ts — `ripgrep` (the `rg` fast grep), built FROM SOURCE by td. Continues
// moving the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) from guix-packaged
// to td-built-from-source via build-recipe (the uutils-`cat` / fd pattern). The source is the
// upstream `ripgrep` 14.1.1 crate tarball (keyed `ripgrep-source`); its 57-crate dependency
// closure is vendored as `.crate` fixed-output static.crates.io fetches in tests/ripgrep.lock.
// Pure Rust at DEFAULT features on a gnu target: jemallocator is musl-only (cfg-excluded) and
// pcre2 is behind a non-default feature, so neither C build is pulled — no `noDefaultFeatures`
// needed. The .drv is assembled + realized by td (no guix (derivation …) / no guix-daemon);
// the rustc/cargo/gcc seed is external (§5, retired last). The binary is `rg`.
//
// The guix-dependence census auto-classifies a buildSystem "rust" recipe as self-hosted
// (no (gnu packages) oracle by design) — no manual enrollment needed.
recipe({
  name: "ripgrep",
  version: "14.1.1",
  buildSystem: "rust",
  bins: ["rg"],
});
