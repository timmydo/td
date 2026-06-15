// recipe-pkg-config.ts — td's OWN recipe for pkg-config, authored in TypeScript
// (input-recipes: reconstruct individual INPUT recipes, the move-off-Guile §5
// follow-on to input-resolution; toolchain retired LAST).
//
// pkg-config is ncurses's native-input — a real package in nano's build graph.
// Reconstructing it from upstream coordinates (NOT looking it up in the Guix
// corpus) store-path-equal to the corpus oracle means its resolution can be
// backed by td's OWN recipe instead of Guile's specification->package — one
// package off the resolver (the toolchain stays Guile, §5).
//
// Two recipe-DSL firsts vs hello/nano, both flowing through the boa evaluator's
// generic JSON capture: a #:configure-flags recipe (`configureFlags`) and a
// MULTI-URI source (mirror fallbacks). The `corpus-pkgconfig` gate proves this
// lowers store-path-equal (NAR-hash-equal) to the pinned corpus's pkg-config
// (Guix is the oracle) and that the flags + the URI list are load-bearing.
recipe({
  name: "pkg-config",
  version: "0.29.2",
  source: fetchSource(
    ["http://fossies.org/linux/misc/pkg-config-0.29.2.tar.gz",
     "http://pkgconfig.freedesktop.org/releases/pkg-config-0.29.2.tar.gz"],
    "14fmwzki1rlz8bs2p810lk6jqdxsk966d8drgsjmi54cd00rrikg"),
  buildSystem: "gnu",
  configureFlags: ["--with-internal-glib"],
});
