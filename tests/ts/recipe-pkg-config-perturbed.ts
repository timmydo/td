// recipe-pkg-config-perturbed.ts — the corpus-pkgconfig differential's
// configure-flags discriminator.
//
// Identical to recipe-pkg-config.ts EXCEPT one changed configure flag
// (--with-internal-glib -> --without-internal-glib). A different build
// configuration ⇒ a different build expression ⇒ a different derivation, so the
// `corpus-pkgconfig` gate must see this DIVERGE from the corpus oracle. If it ever
// converges, the differential has gone vacuous (verified-red) and the
// `configureFlags` the bridge now carries would not be load-bearing.
recipe({
  name: "pkg-config",
  version: "0.29.2",
  source: fetchSource(
    ["http://fossies.org/linux/misc/pkg-config-0.29.2.tar.gz",
     "http://pkgconfig.freedesktop.org/releases/pkg-config-0.29.2.tar.gz"],
    "14fmwzki1rlz8bs2p810lk6jqdxsk966d8drgsjmi54cd00rrikg"),
  buildSystem: "gnu",
  configureFlags: ["--without-internal-glib"],
});
