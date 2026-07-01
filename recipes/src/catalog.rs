//! The package catalog — every td recipe, declared in Rust.
//!
//! Keyed by a stable STEM (not the recipe name): the `-perturbed`
//! self-discrimination twins deliberately share a recipe `name` with their base
//! (e.g. `hello-perturbed` is name `hello`), so the stem is the stable key. The
//! `recipe-rs` gate proves the surface is self-consistent and keeps the
//! `tests/recipes-meta.json` census manifest in sync.
//!
//! NOTE (follow-up): a `build.rs` glob over
//! `src/recipes/*.rs` would let each recipe live in its own self-registering file
//! (the mk/gates "one file, no shared line" property). PR1 keeps a single central
//! table for reviewability; splitting it is mechanical and changes no behavior.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::types::{Recipe, Source};

/// Look up a recipe by `.ts` file stem (e.g. "hello", "gzip-perturbed").
pub fn lookup(stem: &str) -> Option<Recipe> {
    all().into_iter().find(|(s, _)| *s == stem).map(|(_, r)| r)
}

/// Every migrated recipe, paired with its `.ts` file stem.
pub fn all() -> Vec<(&'static str, Recipe)> {
    vec![
        ("bash", bash()),
        ("bat", bat()),
        ("cat", cat()),
        ("coreutils", coreutils()),
        ("diffutils", diffutils()),
        ("eza", eza()),
        ("fd", fd()),
        ("file", file()),
        ("findutils", findutils()),
        ("gawk", gawk()),
        ("grep", grep()),
        ("hello", hello(HELLO_SHA)),
        ("hello-perturbed", hello_perturbed()),
        ("less", less()),
        ("libsigsegv", libsigsegv()),
        ("libunistring", libunistring()),
        ("make", make()),
        ("ncurses", ncurses()),
        ("patch", patch()),
        ("pcre2", pcre2()),
        ("pkg-config", pkg_config()),
        ("pkg-config-perturbed", pkg_config_perturbed()),
        ("procs", procs()),
        ("readline", readline()),
        ("ripgrep", ripgrep()),
        ("sd", sd()),
        ("sed", sed()),
        ("tar", tar()),
        ("td-builder", td_builder()),
        ("td-cmake-demo", td_cmake_demo()),
        ("td-feed", td_feed()),
        ("td-fetch", td_fetch()),
        ("td-russh-demo", td_russh_demo()),
        ("td-subst", td_subst()),
        ("td-ts-eval", td_ts_eval()),
        ("td-vendor-demo", td_vendor_demo()),
        ("uutils", uutils()),
        ("xz", xz()),
        ("youki", youki()),
    ]
}

// --- leaf recipes (no phases) -------------------------------------------------

fn bash() -> Recipe {
    Recipe::gnu("bash", "5.2.37")
        .source(Source::one(
            "mirror://gnu/bash/bash-5.2.tar.gz",
            "1yrjmf0mqg2q8pqphjlark0mcmgf88b0acq7bqf4gx3zvxkc2fd1",
        ))
        .inputs(&["readline", "ncurses"])
        .configure_flags(&["CFLAGS=-O2 -g -std=gnu17"])
}

fn bat() -> Recipe {
    Recipe::rust("bat", "0.25.0")
        .bins(&["bat"])
        .no_default_features()
        .features(&["clap", "etcetera", "paging", "wild", "regex-fancy"])
}

fn cat() -> Recipe {
    Recipe::rust("cat", "0.9.0").bins(&["cat"])
}

fn coreutils() -> Recipe {
    Recipe::gnu("coreutils", "9.1").source(Source::one(
        "mirror://gnu/coreutils/coreutils-9.1.tar.xz",
        "08q4b0w7mwfxbqjs712l6wrwl2ijs7k50kssgbryg9wbsw8g98b1",
    ))
}

fn diffutils() -> Recipe {
    Recipe::gnu("diffutils", "3.12").source(Source::one(
        "mirror://gnu/diffutils/diffutils-3.12.tar.xz",
        "1zbxf8vv7z18ypddwqgzj51n426k959fiv4wxbyl34b0r2gpz2vw",
    ))
}

fn eza() -> Recipe {
    Recipe::rust("eza", "0.21.6").bins(&["eza"]).no_default_features()
}

fn fd() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}

fn file() -> Recipe {
    Recipe::gnu("file", "5.46").source(Source::one(
        "http://ftp.astron.com/pub/file/file-5.46.tar.gz",
        "1230v1sks2p4ijc7x68iy2z9sqfm17v5lmfwbq9l7ib0qp3pgk69",
    ))
}

fn findutils() -> Recipe {
    Recipe::gnu("findutils", "4.10.0").source(Source::one(
        "mirror://gnu/findutils/findutils-4.10.0.tar.xz",
        "1xd4y24qfsdfp3ndz7d5j49lkhbhpzgr13wrvsmx4izjgyvf11qk",
    ))
}

fn gawk() -> Recipe {
    Recipe::gnu("gawk", "5.3.0")
        .source(Source::one(
            "mirror://gnu/gawk/gawk-5.3.0.tar.xz",
            "02x97iyl9v84as4rkdrrkfk2j4vy4r3hpp3rkp3gh3qxs79id76a",
        ))
        .configure_flags(&["CFLAGS=-O2 -g -Wno-incompatible-pointer-types"])
}

fn grep() -> Recipe {
    Recipe::gnu("grep", "3.11")
        .source(Source::one(
            "mirror://gnu/grep/grep-3.11.tar.xz",
            "1avf4x8skxbqrjp5j2qr9sp5vlf8jkw2i5bdn51fl3cxx3fsxchx",
        ))
        .inputs(&["pcre2"])
}

const HELLO_SHA: &str = "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js";

fn hello(sha: &str) -> Recipe {
    Recipe::gnu("hello", "2.12.2").source(Source::one(
        "mirror://gnu/hello/hello-2.12.2.tar.gz",
        sha,
    ))
}

// The self-discrimination twin for the corpus-no-guix gate: a LOAD-BEARING recipe
// field (configureFlags) differs from base `hello`, so it assembles a DISTINCT .drv
// even though the source is resolved from the lock (a source-hash perturbation would
// be vacuous in the build-recipe path — see mk/gates/220-corpus-no-guix.mk).
fn hello_perturbed() -> Recipe {
    hello(HELLO_SHA).configure_flags(&["--disable-nls"])
}

fn less() -> Recipe {
    Recipe::gnu("less", "608")
        .source(Source::one(
            "mirror://gnu/less/less-608.tar.gz",
            "02f2d9d6hyf03va28ip620gjc6rf4aikmdyk47h7frqj18pbx6m6",
        ))
        .inputs(&["ncurses"])
        .configure_flags(&["CFLAGS=-O2 -std=gnu17"])
}

fn libsigsegv() -> Recipe {
    Recipe::gnu("libsigsegv", "2.14").source(Source::one(
        "mirror://gnu/libsigsegv/libsigsegv-2.14.tar.gz",
        "15d2r831xz94s7540nvb1gbfl062g7mrnj88m60wyr1kh10kkb6d",
    ))
}

fn libunistring() -> Recipe {
    Recipe::gnu("libunistring", "1.3").source(Source::one(
        "mirror://gnu/libunistring/libunistring-1.3.tar.xz",
        "09wmas38i9fw7l3sv92xkbvy7idcl76ifhzv7l7ia98xhdn7higj",
    ))
}

fn make() -> Recipe {
    Recipe::gnu("make", "4.4.1").source(Source::one(
        "mirror://gnu/make/make-4.4.1.tar.gz",
        "1cwgcmwdn7gqn5da2ia91gkyiqs9birr10sy5ykpkaxzcwfzn5nx",
    ))
}

fn ncurses() -> Recipe {
    Recipe::gnu("ncurses", "6.2")
        .source(Source::one(
            "mirror://gnu/ncurses/ncurses-6.2.tar.gz",
            "17bcm2z1rdx5gmzj5fb8cp7f28aw5b4g2z4qvvqg3yg0fq66wc1h",
        ))
        .configure_flags(&["--without-cxx-binding", "--enable-overwrite", "--with-shared"])
}

fn patch() -> Recipe {
    Recipe::gnu("patch", "2.8").source(Source::one(
        "mirror://gnu/patch/patch-2.8.tar.xz",
        "1qssgwgy3mfahkpgg99a35gl38vamlqb15m3c2zzrd62xrlywz7q",
    ))
}

fn pcre2() -> Recipe {
    Recipe::gnu("pcre2", "10.42").source(Source::one(
        "https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.42/pcre2-10.42.tar.bz2",
        "0h78np8h3dxlmvqvpnj558x67267n08n9zsqncmlqapans6csdld",
    ))
}

const PKG_CONFIG_URIS: [&str; 2] = [
    "http://fossies.org/linux/misc/pkg-config-0.29.2.tar.gz",
    "http://pkgconfig.freedesktop.org/releases/pkg-config-0.29.2.tar.gz",
];
const PKG_CONFIG_SHA: &str = "14fmwzki1rlz8bs2p810lk6jqdxsk966d8drgsjmi54cd00rrikg";

fn pkg_config() -> Recipe {
    Recipe::gnu("pkg-config", "0.29.2")
        .source(Source::list(&PKG_CONFIG_URIS, PKG_CONFIG_SHA))
        .configure_flags(&["--with-internal-glib"])
}

fn pkg_config_perturbed() -> Recipe {
    Recipe::gnu("pkg-config", "0.29.2")
        .source(Source::list(&PKG_CONFIG_URIS, PKG_CONFIG_SHA))
        .configure_flags(&["--without-internal-glib"])
}

fn procs() -> Recipe {
    Recipe::rust("procs", "0.14.10").bins(&["procs"])
}

fn readline() -> Recipe {
    Recipe::gnu("readline", "8.2.13")
        .source(Source::one(
            "mirror://gnu/readline/readline-8.2.tar.gz",
            "0dbw02ai0z8x6d9s14pl0hnaa2g1kdxnv8qqra1fx13ay5qp3srz",
        ))
        .inputs(&["ncurses"])
}

fn ripgrep() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1").bins(&["rg"])
}

fn sd() -> Recipe {
    Recipe::rust("sd", "1.0.0").bins(&["sd"])
}

fn sed() -> Recipe {
    Recipe::gnu("sed", "4.9").source(Source::one(
        "mirror://gnu/sed/sed-4.9.tar.gz",
        "0bi808vfkg3szmpy9g5wc7jnn2yk6djiz412d30km9rky0c8liyi",
    ))
}

fn tar() -> Recipe {
    Recipe::gnu("tar", "1.35").source(Source::one(
        "mirror://gnu/tar/tar-1.35.tar.xz",
        "05nw7q7sazkana11hnf3f77lmybw1j9j6lsk93bsxirf6hvzyqjd",
    ))
}

fn td_builder() -> Recipe {
    Recipe::rust("td-builder", "0.1.0").bins(&["td-builder"])
}

fn td_cmake_demo() -> Recipe {
    Recipe::cmake("td-cmake-demo", "0.1.0")
}

fn td_feed() -> Recipe {
    Recipe::rust("td-feed", "0.1.0").bins(&["td-feed"])
}

fn td_fetch() -> Recipe {
    Recipe::rust("td-fetch", "0.1.0").bins(&["td-fetch"])
}

fn td_russh_demo() -> Recipe {
    Recipe::rust("td-russh-demo", "0.1.0").bins(&["td-russh-demo"])
}

fn td_subst() -> Recipe {
    Recipe::rust("td-subst", "0.1.0").bins(&["td-subst"])
}

fn td_ts_eval() -> Recipe {
    Recipe::rust("td-ts-eval", "0.1.0").bins(&["td-ts-eval"])
}

fn td_vendor_demo() -> Recipe {
    Recipe::rust("td-vendor-demo", "0.1.0").bins(&["td-vendor-demo"])
}

fn uutils() -> Recipe {
    Recipe::rust("uutils", "0.9.0").bins(&["coreutils"])
}

fn xz() -> Recipe {
    Recipe::gnu("xz", "5.4.5").source(Source::one(
        "http://tukaani.org/xz/xz-5.4.5.tar.gz",
        "1mmpwl4kg1vs6n653gkaldyn43dpbjh8gpk7sk0gps5f6jwr0p0k",
    ))
}

fn youki() -> Recipe {
    Recipe::rust("youki", "0.6.0").bins(&["youki"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_emits_canonical_json_and_round_trips() {
        for (stem, r) in all() {
            let canon = r.to_json().to_canonical();
            // Structural self-consistency: re-parsing the emitted JSON and
            // re-canonicalising yields the same bytes (the durable round-trip).
            let reparsed = crate::json::parse(&canon)
                .unwrap_or_else(|e| panic!("{stem}: emitted invalid JSON: {e}"));
            assert_eq!(reparsed.to_canonical(), canon, "{stem}: not idempotent");
            assert!(!r.name.is_empty() && !r.version.is_empty(), "{stem}: missing fields");
        }
    }

    #[test]
    fn perturbed_twins_diverge_from_their_base() {
        // The self-discrimination property the corpus gates rely on: a perturbed
        // twin must NOT serialise identically to its base.
        let pairs = [
            ("hello", "hello-perturbed"),
            ("pkg-config", "pkg-config-perturbed"),
        ];
        for (base, pert) in pairs {
            let b = lookup(base).unwrap().to_json().to_canonical();
            let p = lookup(pert).unwrap().to_json().to_canonical();
            assert_ne!(b, p, "{pert} did not diverge from {base}");
        }
    }
}
