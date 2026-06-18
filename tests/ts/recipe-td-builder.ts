// recipe-td-builder.ts — td's OWN recipe for td-builder ITSELF, authored in
// TypeScript: the self-hosting Rust build (move-off-Guile §5). `buildSystem:
// "rust"` selects td-builder's cargo phase runner (build::run_rust, the Guix
// cargo-build-system replacement); `bins` is what it installs into `$out/bin`.
//
// There is no `source` here: the crate is the in-tree builder/ tree, supplied at
// build time by the lock's `td-builder-source` entry (the gate interns the CURRENT
// tree), not an upstream fetch. `td-builder build-recipe` resolves every input from
// the pinned lock (no specification->package), ASSEMBLES the .drv in Rust (no guix
// (derivation …)) and REALIZES it daemon-free (no guix-daemon) — so nothing in
// td-builder's own build path is guix/Guile. The rustc/cargo/gcc seed in the lock
// is the guix-built toolchain, retired LAST (§5).
recipe({
  name: "td-builder",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-builder"],
});
