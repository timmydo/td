// recipe-td-cmake-demo.ts — a cmake-based C package (the cmake-build-system
// increment), authored in TypeScript. buildSystem "cmake" selects td-builder's OWN
// cmake phase runner (build::run_cmake): an out-of-source `cmake` configure -> make
// -> make install, with NO gnu-build-system and NO guix/Guile in the build path
// (move-off-Guile §5). The source is the in-tree tests/cmake-demo project
// (lock-supplied, keyed `td-cmake-demo-source`), so no fetchSource — like the rust
// self-host / vendor demos. build-recipe assembles + realizes the .drv with no guix
// (derivation …) / no guix-daemon; cmake/gcc/make are the external SEED (§5, retired
// last), exactly as the autotools path uses make/gcc.
recipe({
  name: "td-cmake-demo",
  version: "0.1.0",
  buildSystem: "cmake",
});
