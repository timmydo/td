// recipe-td-russh-demo.ts — a Rust SSH built from source (rust-build Inc.4): a
// self-contained russh client<->server loopback round-trip (real SSH handshake +
// publickey auth + exec). buildSystem "rust" selects td-builder's cargo phase
// runner; the 188-crate dependency closure (russh + tokio + the aws-lc crypto
// backend) is supplied as vendored `.crate` fetches in the lock (TD_VENDOR_CRATES).
// The crypto backend has a C build script, so the build env carries CC/CXX +
// kernel headers (run_rust's C set-paths) + cmake in the seed. As with the other
// rust recipes the source is the in-tree crate (lock-supplied), so no fetchSource.
// build-recipe assembles + realizes the .drv with no guix (derivation …) / no
// guix-daemon; the rustc/cargo/gcc/cmake/headers seed is external (§5).
recipe({
  name: "td-russh-demo",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-russh-demo"],
});
