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
//! `crc32`, `gpt` and `fat` arrived with real-hardware boot. The
//! CRC-32 is here rather than beside either of its callers because two
//! unrelated formats want it — the xz decoder's stream checksums and the GPT
//! header and entry-array checksums — and a private copy per format is the
//! divergence this crate exists to prevent. `gpt` computes partition-table
//! bytes and `fat` an ESP's, both performing no I/O, so the target-side
//! installer `#[path]`-includes them (together with `crc32`) exactly as
//! td-boot includes `sha256`. Neither bin uses `gpt` or `fat`.
//!
//! `sha512` and `ed25519` arrive with AUTHENTICATED deployments, and are
//! target-side in that same way — which is why the crate's first line no
//! longer says the two engine bins are who this is shared with. Neither bin
//! uses either module: a deployment's signature is checked where the
//! deployment is installed and booted, by a program that cannot depend on a
//! crate at all. They are a pair, since the verifier reaches its hash as
//! `crate::sha512`, so a consumer `#[path]`-includes BOTH at its crate root.
pub mod crc32;
pub mod ed25519;
// SIGNING is a separate module because td-boot `#[path]`-includes `ed25519.rs`
// and nothing else: keeping the signer out of that file is what keeps it off
// the boot path. Its one caller is the recipe-check oracle.
pub mod ed25519_sign;
pub mod exit;
pub mod fat;
pub mod gpt;
pub mod json;
pub mod sha256;
pub mod sha512;
