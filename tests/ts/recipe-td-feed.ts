// recipe-td-feed.ts — td's recipe for td-feed ITSELF (feed/, td's local HTTP mirror of
// every network-downloaded artifact), authored in TypeScript: a from-source Rust build of
// a SEED TOOL (move-off-Guile §5; td OWNS its fetch/serve path). `buildSystem: "rust"`
// selects td-builder's cargo phase runner (build::run_rust); `bins` is what it installs.
//
// No `source` here: the crate is the in-tree feed/ tree, supplied at build time by the
// lock's `td-feed-source` entry (the gate interns the CURRENT tree with td's own recursive
// addToStore). td-feed shares td-fetch's dependency closure exactly (ureq + rustls/ring +
// sha2, 73 crates — only the bin name differs), so tests/td-feed.lock reuses td-fetch's
// vendored `.crate` fetches (TD_VENDOR_CRATES); ring's C build script is served by
// run_rust's C set-paths (the gcc-toolchain seed). `td-builder build-recipe` resolves every
// input from the pinned lock (no specification->package), ASSEMBLES the .drv in Rust (no
// guix (derivation …)) and REALIZES it daemon-free, built by the td-bootstrapped stage0 —
// so nothing in td-feed's build path is guix/Guile. The rustc/cargo/gcc seed is retired
// LAST (§5).
recipe({
  name: "td-feed",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-feed"],
});
