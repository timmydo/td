// recipe-cat.ts — a REAL coreutils tool built from source (rust-build Inc.3): the
// uutils `cat` (crate uu_cat 0.9.0, whose [[bin]] is named `cat`). buildSystem
// "rust" selects td-builder's cargo phase runner; the dependency closure (139
// crates) is supplied as vendored `.crate` fetches in the lock (TD_VENDOR_CRATES).
// The source is the upstream uu_cat crate tarball, also a lock-supplied fetch
// (keyed `cat-source`), so no fetchSource. build-recipe assembles + realizes the
// .drv with no guix (derivation …) / no guix-daemon; the rustc/cargo/gcc seed is
// external (§5). This is the coreutils-replacement demo: a from-source Rust `cat`.
recipe({
  name: "cat",
  version: "0.9.0",
  buildSystem: "rust",
  bins: ["cat"],
});
