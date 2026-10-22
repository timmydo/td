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
//! writers, and passes `ostree_deploy` its `renameat2` publication: this
//! crate stays free of syscalls. `flathub_app` holds §W.9's declarations
//! and the pure half of its admission: the cache's state formats and
//! rules 3 and 4.
//!
//! `sha512` and `ed25519` arrive with AUTHENTICATED deployments, and are
//! target-side in that same way — which is why the crate's first line no
//! longer says the two engine bins are who this is shared with. Neither bin
//! uses either module: a deployment's signature is checked where the
//! deployment is installed and booted, by a program that cannot depend on a
//! crate at all. They are a pair, since the verifier reaches its hash as
//! `crate::sha512`, so a consumer `#[path]`-includes BOTH at its crate root.
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
// harnesses provision trusted keys and optional volume identities, while
// target td-install binds the chosen identity in the live environment.
// See td-install/DESIGN.md §§5–7 for the shared-source contract.
pub mod application;
pub mod application_spec;
pub mod cpio;
pub mod crc32;
// `debian_root` parses recipes/src/debian-root.lock: td-recipe-eval lists
// its pins, td-builder assembles and admits the root against it.
pub mod debian_root;
pub mod dynamic_application;
pub mod ed25519;
pub mod elf;
#[cfg(test)]
mod elf_synth;
// `embed_scan` reads Rust source for embeds and test modules: td-recipe-eval's
// build script and td-builder's fingerprints read through one scanner.
pub mod embed_scan;
pub mod launcher;
pub mod ostree;
pub mod ostree_deploy;
pub mod permissions;
pub mod principals;
// SIGNING is a separate module because, of the two ed25519 files, td-boot
// `#[path]`-includes only the VERIFIER: keeping the signer out of that file is
// what keeps it off the boot path. Its one caller is the recipe-check oracle.
pub mod ed25519_sign;
pub mod exit;
pub mod fat;
pub mod flathub_app;
pub mod gpt;
pub mod gzip;
pub mod host_path;
pub mod iso9660;
pub mod json;
pub mod local_source;
pub mod sha256;
pub mod sha512;
pub mod target_profile;
pub mod xz;

#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;
