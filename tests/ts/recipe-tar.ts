// recipe-tar.ts — td's OWN recipe for GNU tar (lever 4: retire the Guix toolchain
// leaf tools, build them with td's own builder; move-off-Guile §5). tar is THE
// unpacker every build's `unpack` phase runs; today it comes from guix's `tar`
// package. Plain autotools, no build inputs beyond the seed.
//
// guix's tar origin carries a source snippet, so its lowered source is a
// patch-and-repacked .tar.zst (not the raw upstream .tar.xz). td uses that exact
// guix-prepared source from the pinned lock (faithful to the snippet) and unpacks
// it with the seed's `tar`, which auto-detects zstd via the `zstd` program — so
// `zstd` is pinned as a build input in tests/tar-no-guix.lock. The compiler seed
// (gcc/glibc/binutils) stays external (§5, retired last).
recipe({
  name: "tar",
  version: "1.35",
  source: fetchSource(
    "mirror://gnu/tar/tar-1.35.tar.xz",
    "05nw7q7sazkana11hnf3f77lmybw1j9j6lsk93bsxirf6hvzyqjd"),
  buildSystem: "gnu",
});
