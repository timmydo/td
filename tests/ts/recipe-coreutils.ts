// recipe-coreutils.ts — td's OWN recipe for GNU coreutils (lever 4: retire the
// Guix toolchain leaf tools, build them with td's own builder; move-off-Guile
// §5). coreutils (ls/cp/mv/install/mkdir/…) is THE core of every build
// environment — the scrubbed-PATH the build-recipe gate runs under is itself a
// coreutils/bin. Today it comes from guix's `coreutils` package.
//
// Plain autotools: ./configure && make && make install. guix's coreutils links
// optional helpers (gmp for `factor` bignums, acl/attr/libcap for xattr/caps);
// those are OPTIONAL — configure auto-disables them when absent, so td builds the
// full core (the file utilities the toolchain needs) without pinning them. The
// compiler seed (gcc/glibc/binutils) stays external (§5, retired last).
recipe({
  name: "coreutils",
  version: "9.1",
  source: fetchSource(
    "mirror://gnu/coreutils/coreutils-9.1.tar.xz",
    "08q4b0w7mwfxbqjs712l6wrwl2ijs7k50kssgbryg9wbsw8g98b1"),
  buildSystem: "gnu",
});
