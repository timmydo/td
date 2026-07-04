use crate::types::{Recipe, Source};

const URIS: [&str; 2] = [
    "http://fossies.org/linux/misc/pkg-config-0.29.2.tar.gz",
    "http://pkgconfig.freedesktop.org/releases/pkg-config-0.29.2.tar.gz",
];
const SHA: &str = "14fmwzki1rlz8bs2p810lk6jqdxsk966d8drgsjmi54cd00rrikg";

pub fn recipe() -> Recipe {
    // CFLAGS pins the C standard to gnu17: pkg-config 0.29.2 bundles glib 2.x, whose
    // goption.c uses `bool`/`true`/`false` as ordinary identifiers. GCC 14 defaults to
    // -std=gnu23, where those are keywords, so the bundled glib fails to compile
    // ("expected identifier before 'bool'"). gnu17 restores the pre-C23 meaning — the
    // same lever bash.rs / less.rs use for this GCC-14 wall. The flag is one
    // ./configure argument (build.rs preserves internal whitespace) and autoconf
    // propagates it to the AC_CONFIG_SUBDIRS glib sub-configure.
    Recipe::gnu("pkg-config", "0.29.2")
        .source(Source::list(&URIS, SHA))
        .configure_flags(&["--with-internal-glib", "CFLAGS=-O2 -g -std=gnu17"])
}
