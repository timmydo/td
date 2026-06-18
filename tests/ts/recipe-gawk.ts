// recipe-gawk.ts — td's OWN recipe for GNU awk (lever 4: retire the Guix
// toolchain leaf tools, build them with td's own builder; move-off-Guile §5).
// gawk is the awk the build env uses; today it comes from guix's `gawk` package.
//
// Plain autotools. guix's gawk links libsigsegv (stack-overflow diagnostics) and
// uses bash at runtime; libsigsegv is OPTIONAL — configure disables it when
// absent — so td builds gawk without pinning it. The compiler seed (gcc/glibc/
// binutils) stays external (§5, retired last).
//
// gawk 5.3.0's io.c casts a K&R empty-parameter function pointer
// (`(ssize_t(*)())read`); the seed's gcc-15 makes -Wincompatible-pointer-types a
// hard error, so the build needs -Wno-incompatible-pointer-types. This CFLAGS
// flag carries internal whitespace (-O2 -g preserve the default optimization),
// exercising the recipe DSL's JSON-encoded configureFlags (one ./configure arg).
recipe({
  name: "gawk",
  version: "5.3.0",
  source: fetchSource(
    "mirror://gnu/gawk/gawk-5.3.0.tar.xz",
    "02x97iyl9v84as4rkdrrkfk2j4vy4r3hpp3rkp3gh3qxs79id76a"),
  buildSystem: "gnu",
  configureFlags: ["CFLAGS=-O2 -g -Wno-incompatible-pointer-types"],
});
