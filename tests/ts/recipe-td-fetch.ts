// recipe-td-fetch.ts — td's recipe for td-fetch ITSELF (td's own seed fetcher,
// fetch/), authored in TypeScript: a from-source Rust build of a SEED TOOL
// (move-off-Guile §5; td OWNS fetching). `buildSystem: "rust"` selects td-builder's
// cargo phase runner (build::run_rust); `bins` is what it installs into `$out/bin`.
//
// No `source` here: the crate is the in-tree fetch/ tree, supplied at build time by
// the lock's `td-fetch-source` entry (the gate interns the CURRENT tree with td's own
// recursive addToStore), not an upstream fetch. The dependency closure (ureq +
// rustls/ring + sha2, 73 crates) is supplied as vendored `.crate` fetches in the lock
// (TD_VENDOR_CRATES); ring's C build script is served by run_rust's C set-paths (the
// gcc-toolchain seed, as for russh's aws-lc). `td-builder build-recipe` resolves every
// input from the pinned lock (no specification->package), ASSEMBLES the .drv in Rust
// (no guix (derivation …)) and REALIZES it daemon-free (no guix-daemon) — built by the
// td-bootstrapped stage0, so nothing in td-fetch's build path is guix/Guile. The
// rustc/cargo/gcc seed in the lock is the guix-built toolchain, retired LAST (§5).
recipe({
  name: "td-fetch",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-fetch"],
});
