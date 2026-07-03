use crate::types::{Recipe, Source};

const URIS: [&str; 2] = [
    "http://fossies.org/linux/misc/pkg-config-0.29.2.tar.gz",
    "http://pkgconfig.freedesktop.org/releases/pkg-config-0.29.2.tar.gz",
];
const SHA: &str = "14fmwzki1rlz8bs2p810lk6jqdxsk966d8drgsjmi54cd00rrikg";

pub fn recipe() -> Recipe {
    Recipe::gnu("pkg-config", "0.29.2")
        .source(Source::list(&URIS, SHA))
        .configure_flags(&["--with-internal-glib"])
}
