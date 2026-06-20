// recipe-gperf.ts — td's OWN recipe for GNU gperf (lever 4: retire the Guix
// toolchain leaf tools; move-off-Guile §5). A pure-autotools leaf (a C++ perfect-hash
// generator), `./configure && make`, NO build inputs. The toolchain's `gperf` becomes
// td-built, not guix-resolved; the compiler seed (gcc/glibc/binutils) stays external
// (§5).
recipe({
  name: "gperf",
  version: "3.3",
  source: fetchSource(
    "mirror://gnu/gperf/gperf-3.3.tar.gz",
    "1n2ac3cxinbfbq41jdpb7mlz58q3vga6rzbshdaf0fp4lymy11zx"),
  buildSystem: "gnu",
});
