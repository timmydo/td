// recipe-td-vendor-demo.ts — a Rust crate WITH dependencies (rust-build Inc.2),
// authored in TypeScript. buildSystem "rust" selects td-builder's cargo phase
// runner; the dependency closure (itoa + ryu) is supplied as vendored `.crate`
// fetches in the lock (TD_VENDOR_CRATES), not an upstream package resolution. As
// with the self-host, the source is the in-tree crate (lock-supplied), so no
// fetchSource. build-recipe assembles + realizes the .drv with no guix
// (derivation …) / no guix-daemon; the rustc/cargo/gcc seed is external (§5).
recipe({
  name: "td-vendor-demo",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-vendor-demo"],
});
