// recipe-patch.ts — td's OWN recipe for GNU patch (lever 4: retire the Guix
// toolchain leaf tools, build them with td's own builder; move-off-Guile §5).
// patch applies the source patches the gnu-build-system's patch phase needs;
// today it comes from guix's `patch` package. Plain autotools, no build inputs.
// The compiler seed (gcc/glibc/binutils) stays external (§5, retired last).
recipe({
  name: "patch",
  version: "2.8",
  source: fetchSource(
    "mirror://gnu/patch/patch-2.8.tar.xz",
    "1qssgwgy3mfahkpgg99a35gl38vamlqb15m3c2zzrd62xrlywz7q"),
  buildSystem: "gnu",
});
