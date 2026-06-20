// recipe-which.ts — td's OWN recipe for GNU which (lever 4: retire the Guix
// toolchain leaf tools; move-off-Guile §5). The simplest pure-autotools leaf:
// plain `./configure && make`, NO build inputs. The toolchain's `which` becomes
// td-built, not guix-resolved; the compiler seed (gcc/glibc/binutils) stays
// external (§5).
recipe({
  name: "which",
  version: "2.21",
  source: fetchSource(
    "mirror://gnu/which/which-2.21.tar.gz",
    "1bgafvy3ypbhhfznwjv1lxmd6mci3x1byilnnkc7gcr486wlb8pl"),
  buildSystem: "gnu",
});
