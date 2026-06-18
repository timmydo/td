// recipe-xz.ts — td's OWN recipe for XZ Utils (lever 4: retire the Guix
// toolchain leaf tools, build them with td's own builder; move-off-Guile §5).
// xz is the .tar.xz (de)compressor the build env uses to unpack sources; today
// it comes from guix's `xz` package (specification->package). Plain autotools,
// no build inputs. The compiler seed (gcc/glibc/binutils) stays external (§5).
recipe({
  name: "xz",
  version: "5.4.5",
  source: fetchSource(
    "http://tukaani.org/xz/xz-5.4.5.tar.gz",
    "1mmpwl4kg1vs6n653gkaldyn43dpbjh8gpk7sk0gps5f6jwr0p0k"),
  buildSystem: "gnu",
});
