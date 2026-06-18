// recipe-diffutils.ts — td's OWN recipe for GNU diffutils (lever 4: retire the
// Guix toolchain leaf tools, build them with td's own builder; move-off-Guile
// §5). diffutils (diff/cmp/sdiff) is a build-environment tool today resolved via
// guix's `diffutils` package. Plain autotools, no build inputs. The compiler
// seed (gcc/glibc/binutils) stays external (§5, retired last).
recipe({
  name: "diffutils",
  version: "3.12",
  source: fetchSource(
    "mirror://gnu/diffutils/diffutils-3.12.tar.xz",
    "1zbxf8vv7z18ypddwqgzj51n426k959fiv4wxbyl34b0r2gpz2vw"),
  buildSystem: "gnu",
});
