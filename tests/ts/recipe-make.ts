// recipe-make.ts — td's OWN recipe for GNU make, authored in TypeScript
// (lever 4: retire the Guix toolchain package-by-package, leaf tools first;
// move-off-Guile §5). make is a build-environment tool: today it comes from
// guix's `make` package (resolved via specification->package); this reconstructs
// it as a td recipe built by td's OWN builder (build-recipe), so the toolchain's
// `make` is td-built, not guix-resolved. The corpus's compiler seed
// (gcc-toolchain/glibc/binutils) stays external (§5, retired last); the leaves
// (make, sed, grep, …) are gcc-buildable and move first.
//
// Built WITHOUT guile support (guix's make links guile for $(guile …) make
// functions — unused by td's autotools recipes; a guile-free make is also one
// fewer Guile dependency). Plain autotools: ./configure && make && make install.
recipe({
  name: "make",
  version: "4.4.1",
  source: fetchSource(
    "mirror://gnu/make/make-4.4.1.tar.gz",
    "1cwgcmwdn7gqn5da2ia91gkyiqs9birr10sy5ykpkaxzcwfzn5nx"),
  buildSystem: "gnu",
});
