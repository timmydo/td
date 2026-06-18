// recipe-findutils.ts — td's OWN recipe for GNU findutils (lever 4: retire the
// Guix toolchain leaf tools, build them with td's own builder; move-off-Guile
// §5). findutils (find/xargs) backs the recipe DSL's findFiles phase and many
// build steps; today it comes from guix's `findutils` package. Plain autotools.
//
// As with tar, guix's findutils origin carries a snippet, so its lowered source
// is a patch-and-repacked .tar.zst; td uses that guix-prepared source from the
// pinned lock and unpacks it with the seed's `tar` (zstd auto-detected via the
// pinned `zstd` build input). The compiler seed (gcc/glibc/binutils) stays
// external (§5, retired last).
recipe({
  name: "findutils",
  version: "4.10.0",
  source: fetchSource(
    "mirror://gnu/findutils/findutils-4.10.0.tar.xz",
    "1xd4y24qfsdfp3ndz7d5j49lkhbhpzgr13wrvsmx4izjgyvf11qk"),
  buildSystem: "gnu",
});
