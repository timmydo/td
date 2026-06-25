// recipe-td-subst.ts — td's recipe for td-subst ITSELF (subst/, td's substitute
// (binary-cache) server), authored in TypeScript: a from-source Rust build of a SEED TOOL
// (move-off-Guile §5; td OWNS its substitute serve/sign/fetch path). `buildSystem: "rust"`
// selects td-builder's cargo phase runner (build::run_rust); `bins` is what it installs.
//
// No `source` here: the crate is the in-tree subst/ tree, supplied at build time by the
// lock's `td-subst-source` entry (the gate interns the CURRENT tree with td's own recursive
// addToStore). td-subst shares td-feed/td-fetch's dependency closure exactly (ureq +
// rustls/ring + sha2 — subst adds `ring` as a DIRECT dep, already in that closure, so
// subst/Cargo.lock pins td-feed's exact versions), so tests/td-subst.lock reuses the same
// vendored `.crate` fetches (TD_VENDOR_CRATES); ring's C build script is served by
// run_rust's C set-paths (the gcc-toolchain seed). `td-builder build-recipe` resolves every
// input from the pinned lock (no specification->package), ASSEMBLES the .drv in Rust (no
// guix (derivation …)) and REALIZES it daemon-free, built by the td-bootstrapped stage0 —
// so nothing in td-subst's build path is guix/Guile. The rustc/cargo/gcc seed is retired
// LAST (§5).
recipe({
  name: "td-subst",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-subst"],
});
