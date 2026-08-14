//! td-engine — the std-only code shared by td-builder and td-recipe-eval.
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
//! `crc32` and `gpt` arrived with real-hardware boot. The
//! CRC-32 is here rather than beside either of its callers because two
//! unrelated formats want it — the xz decoder's stream checksums and the GPT
//! header and entry-array checksums — and a private copy per format is the
//! divergence this crate exists to prevent. `gpt` computes partition-table
//! bytes and performs no I/O, so the target-side installer `#[path]`-includes
//! it (together with `crc32`) exactly as td-boot includes `sha256`.
pub mod crc32;
pub mod exit;
pub mod fat;
pub mod gpt;
pub mod json;
pub mod sha256;
