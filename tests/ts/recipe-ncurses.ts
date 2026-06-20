// recipe-ncurses.ts — td's OWN recipe for ncurses (move-off-Guile §5, lever 4:
// reconstruct the shipped-system closure). Terminal library; a build input of
// bash and nano. Today from guix's `ncurses`. (guix labels its package
// 6.2.20210619 — the 6.2 tarball plus an out-of-origin patch rollup; td builds
// the upstream 6.2 release from the same clean tarball.)
//
// --without-cxx-binding: ncurses 6.2's optional C++ binding (libncurses++) does
// not compile under the seed's gcc-15 — ncurses' `bool` macro collides with
// libstdc++'s std::hash<unsigned char>/<bool> specializations; it is unused by
// bash/nano (they link the C library). --enable-overwrite: install curses.h et al
// into include/ directly (autotools defaults to include/ncurses/ when --prefix is
// set), where consumers expect them.
//
// --with-shared: build PIC shared libs (libncurses.so) in addition to the static
// archives. ncurses defaults to static-only; a non-PIC libncurses.a cannot be linked
// into a SHARED object — gettext's libtextstyle does exactly that and failed with
// `ld: relocation R_X86_64_32 … recompile with -fPIC`. Shared libs let the gettext
// edge chain td's ncurses (build-plan); executable consumers (bash, nano) link the
// shared lib too (resolved at runtime from the dep's lib dir).
recipe({
  name: "ncurses",
  version: "6.2",
  source: fetchSource(
    "mirror://gnu/ncurses/ncurses-6.2.tar.gz",
    "17bcm2z1rdx5gmzj5fb8cp7f28aw5b4g2z4qvvqg3yg0fq66wc1h"),
  buildSystem: "gnu",
  configureFlags: ["--without-cxx-binding", "--enable-overwrite", "--with-shared"],
});
