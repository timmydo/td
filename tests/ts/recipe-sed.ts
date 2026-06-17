// recipe-sed.ts — td's OWN recipe for GNU sed (lever 4: retire the Guix toolchain
// leaf tools, build them with td's own builder; move-off-Guile §5). Plain
// autotools, no build inputs. The toolchain's `sed` becomes td-built, not
// guix-resolved; the compiler seed (gcc/glibc/binutils) stays external (§5).
recipe({
  name: "sed",
  version: "4.9",
  source: fetchSource(
    "mirror://gnu/sed/sed-4.9.tar.gz",
    "0bi808vfkg3szmpy9g5wc7jnn2yk6djiz412d30km9rky0c8liyi"),
  buildSystem: "gnu",
});
