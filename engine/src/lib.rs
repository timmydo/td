//! td-engine — the std-only, dependency-free code td's own programs share.
//!
//! Both engine bins hand-roll the same two primitives (a minimal JSON
//! value/parser/canonical writer and SHA-256, kept dependency-free on purpose
//! — see the module headers). They previously carried DIVERGED private copies;
//! this crate is the single source. The JSON module keeps the recipe surface's
//! representation (numbers as raw lexemes, objects as order-preserving `Vec`
//! entries, canonical writer with SORTED keys) plus the builder's read
//! accessors; the SHA-256 module is the lint-clean implementation plus the
//! builder's streaming file helper. `exit` is the third shared thing: the
//! status codes the two bins exchange when one runs the other.
//!
//! `crc32`, `gpt` and `fat` arrived with real-hardware boot. CRC-32 stays one
//! shared module because the xz decoder's stream checksums, gzip member
//! trailers, and GPT header and entry-array checksums all need it; a private
//! copy per format is the divergence this crate exists to prevent. `gpt`
//! computes partition-table bytes and `fat` an ESP's, both performing no I/O,
//! so the target-side
//! installer `#[path]`-includes them (together with `crc32`) exactly as
//! td-boot includes `sha256`. Neither bin uses `gpt` or `fat`.
//!
//! The `gzip` module holds the one std-only DEFLATE implementation shared
//! by builder source extraction, `td-net`'s bounded OSTree acquisition
//! (on the target only through `td-app`) and the `ostree_deploy`
//! materializer; `xz` is its LZMA2
//! counterpart, shared by builder source extraction and `td-net`'s Debian
//! index resolution. No target binary `#[path]`-includes either.
//!
//! `elf` (the read-only ELF inspection), `dynamic_application` (the
//! offline loader-graph validator) and `ostree_deploy` moved here from the
//! builder so the image build and `td-net`'s on-demand Flathub admission
//! (APPLICATIONS.md §W.9) run one implementation. The builder keeps the ELF
//! writers, and td-net, `ostree_deploy`'s only caller, passes it the rename
//! that publishes a deploy: this crate stays free of syscalls. `flathub_app`
//! holds §W.9's declarations and the pure half of its admission: the cache's
//! state formats and rules 3 and 4.
//!
//! `local_source` is back to being shared by BOTH engine bins (re #469
//! local-source-roster split): the exclusion rule and staging shape a
//! `local_source`/`local_source_trees` recipe is copied by must be exactly
//! one implementation, since td-recipe-eval interns those bytes and
//! td-builder independently re-derives their identity from the checkout —
//! a divergence here would let the two sides silently disagree about what
//! "the same tree" means. It does no hashing of its own: this crate is
//! `unsafe`-forbidding and dependency-free, and the NAR hash stays
//! td-builder's confined syscall surface (`nar.rs`).
// `cpio` writes selector appendices without parameterizing recipes: host
// harnesses provision optional volume identities, while target td-install
// binds the chosen identity in the live environment.
// See td-install/DESIGN.md §§5–7 for the shared-source contract.
pub mod application;
pub mod application_spec;
pub mod boot_info;
pub mod boot_info_source;
pub mod bzip2;
pub mod cpio;
pub mod crc32;
pub mod elf_edit;
pub mod rust_dist;
pub mod tar;
pub mod tar_write;
// `debian_root` parses recipes/src/debian-root.lock: td-recipe-eval lists
// its pins, td-builder assembles and admits the root against it.
pub mod debian_root;
pub mod dynamic_application;
pub mod elf;
#[cfg(test)]
mod elf_synth;
// `embed_scan` reads Rust source for embeds and test modules: td-recipe-eval's
// build script and td-builder's fingerprints read through one scanner.
pub mod embed_scan;
pub mod exit;
pub mod fat;
pub mod flathub_app;
pub mod gpt;
pub mod gzip;
pub mod host_path;
pub mod image_chunks;
pub mod iso9660;
pub mod json;
pub mod launcher;
pub mod local_source;
pub mod ostree;
pub mod ostree_deploy;
pub mod permissions;
pub mod principals;
pub mod sha256;
pub mod target_profile;
// `workload_accounts` is the workload account ledger's pure half, which
// td-firstboot `#[path]`-includes to seed it; td-builder and
// td-recipe-eval do not use it.
pub mod workload_accounts;
pub mod xz;

#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

/// `workload_accounts` keeps its own copy of the principal table's
/// application name grammar, since its coming readers do not compile
/// `principals`; hold the two equal.
#[cfg(test)]
mod account_name_grammar {
    fn admitted(name: &str) -> (bool, bool) {
        let principals = crate::principals::Registry::parse(&format!(
            "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\t{name}\t65536\n"
        ));
        let ledger = crate::workload_accounts::Ledger::parse(&format!(
            "td-accounts-v1\napplication\t{name}\t65536\n"
        ));
        (principals.is_ok(), ledger.is_ok())
    }

    #[test]
    fn the_ledger_and_the_principal_table_admit_the_same_names() {
        let mut names = Vec::new();
        // Every ASCII byte first, later and last, and a non-ASCII one.
        for byte in 0..=127u8 {
            let byte = char::from(byte);
            names.push(format!("{byte}"));
            names.push(format!("{byte}a"));
            names.push(format!("a{byte}"));
            names.push(format!("a{byte}a"));
        }
        names.push("m\u{e1}il".to_string());
        for length in [63, 64, 65] {
            names.push("a".repeat(length));
            names.push(format!("{}9", "a".repeat(length - 1)));
        }
        let mut admitted_any = false;
        for name in &names {
            let (principals, ledger) = admitted(name);
            assert_eq!(principals, ledger, "{name:?}");
            admitted_any |= ledger;
        }
        // The comparison is not between two refusals of everything.
        assert!(admitted_any);
        assert_eq!(admitted(&"a".repeat(64)), (true, true));
        assert_eq!(admitted(&"a".repeat(65)), (false, false));
    }
}

/// Hash-visible EROFS producer format. Change when packed bytes change.
pub const EROFS_FORMAT: &str = "zstd-shared-extents-v2";

pub mod native_cache;
