//! UNSAFE.md §26 in terms the compiler cannot express. A `#[target_feature]`
//! function on a CPU without the feature is undefined behaviour, and nothing but
//! this test ties a call to a check for its features. `accel.rs` and `lib.rs` are
//! short and fully known, so the test holds all of each: no count of tokens can see
//! what a respelled constructor, a shadowed macro or a free function would do.
//!
//! It stops a mistake, and makes a way round it loud in the diff. It is not proof
//! against a hostile author, who could edit this file too: review is what meets one.

#![allow(clippy::indexing_slicing, clippy::unwrap_used)]

use std::path::Path;
use td_source_scan::{self as scan, source, squeeze, squeezed, unsafe_allows, unsafe_blocks};

/// `accel.rs` without its comments. To change it is to amend UNSAFE.md §26.
const ACCEL: &str = r#"
use crate::{aes_ni, chacha_avx2, sha_ni};

#[derive(Clone, Copy)]
pub(crate) struct AesClmul(());

impl AesClmul {
    pub(crate) fn detect() -> Option<Self> {
        (is_x86_feature_detected!("aes")
            && is_x86_feature_detected!("pclmulqdq")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1"))
        .then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn new_key(self, schedule: &[u32], wide: bool) -> Box<aes_ni::Key> {
        unsafe { aes_ni::new_key(schedule, wide) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn encrypt_block(self, key: &aes_ni::Key, block: &[u8; 16]) -> [u8; 16] {
        unsafe { aes_ni::encrypt_block(key, block) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn ctr(self, key: &aes_ni::Key, nonce: &[u8; 12], first: u32, data: &mut [u8]) {
        unsafe { aes_ni::ctr(key, nonce, first, data) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn ghash(self, key: &aes_ni::Key, y: &mut [u64; 2], data: &[u8]) {
        unsafe { aes_ni::ghash(key, y, data) }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Avx2(());

impl Avx2 {
    pub(crate) fn detect() -> Option<Self> {
        is_x86_feature_detected!("avx2").then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn chacha20(self, key: &[u32; 8], nonce: &[u32; 3], first: u32, data: &mut [u8]) {
        unsafe { chacha_avx2::crypt(key, nonce, first, data) }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ShaNi(());

impl ShaNi {
    pub(crate) fn detect() -> Option<Self> {
        (is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1"))
        .then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn compress256(self, h: &mut [u32; 8], block: &[u8; 64]) {
        unsafe { sha_ni::compress256(h, block) }
    }
}
"#;

/// `lib.rs` without its comments: which files are the crate, and how they are read.
const LIB: &str = r#"
#[cfg(target_arch = "x86_64")]
mod accel;
pub mod aes256_ctr;
pub mod aes_gcm;
#[cfg(target_arch = "x86_64")]
mod aes_ni;
pub mod bcrypt_pbkdf;
mod bignum;
pub mod chacha20_poly1305;
#[cfg(target_arch = "x86_64")]
mod chacha_avx2;
pub mod ct;
pub mod curve25519;
mod der;
pub mod ec;
pub mod hmac;
pub mod keccak;
#[cfg(test)]
mod leakage;
#[cfg(test)]
mod leakage_tests;
pub mod mlkem768;
pub mod random;
pub mod rsa;
pub mod sha;
#[cfg(target_arch = "x86_64")]
mod sha_ni;
#[cfg(test)]
mod test_corpus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refused by a cryptographic check")
    }
}

impl std::error::Error for Refused {}
"#;

/// A proof type, the module its methods call and each whole call there, and the
/// features both must name.
struct Proof {
    name: &'static str,
    module: &'static str,
    called: &'static [&'static str],
    features: &'static [&'static str],
}

const PROOFS: &[Proof] = &[
    Proof {
        name: "AesClmul",
        module: "aes_ni",
        called: &[
            "new_key(schedule,wide)",
            "encrypt_block(key,block)",
            "ctr(key,nonce,first,data)",
            "ghash(key,y,data)",
        ],
        features: &["aes", "pclmulqdq", "ssse3", "sse4.1"],
    },
    Proof {
        name: "Avx2",
        module: "chacha_avx2",
        called: &["crypt(key,nonce,first,data)"],
        features: &["avx2"],
    },
    Proof {
        name: "ShaNi",
        module: "sha_ni",
        called: &["compress256(h,block)"],
        features: &["sha", "ssse3", "sse4.1"],
    },
];

fn base() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
}

fn sources() -> Vec<(String, String)> {
    let tree = scan::read_tree(base()).unwrap();
    assert!(tree.other.is_empty(), "{:?}", tree.other);
    tree.rs
}

#[test]
fn accel_and_lib_are_exactly_the_reviewed_text() {
    let files = sources();
    assert_eq!(squeeze(&source(&files, "accel.rs")), squeeze(ACCEL));
    assert_eq!(squeeze(&source(&files, "lib.rs")), squeeze(LIB));
    assert_eq!(unsafe_blocks(ACCEL), 6);
    assert_eq!(unsafe_allows(ACCEL), 6);
    // A method without `self` would be a safe call with no check behind it.
    assert_eq!(squeeze(ACCEL).matches("(self,").count(), 6);
}

#[test]
fn unsafe_is_in_accel_and_nowhere_else() {
    for (name, text) in &sources() {
        if name != "accel.rs" {
            assert_eq!(unsafe_blocks(text), 0, "{name}");
            assert_eq!(unsafe_allows(text), 0, "{name}");
            assert!(!squeeze(text).contains("unsafe"), "{name}");
        }
        for form in ["fn", "impl", "trait"] {
            assert_eq!(scan::unsafe_items(text, form), 0, "{name}: unsafe {form}");
        }
    }
}

#[test]
fn no_memory_is_reached_except_through_safe_rust() {
    let all = squeezed(&sources());
    for banned in [
        "asm!",
        "global_asm",
        "transmute",
        "*const",
        "*mut",
        "from_raw",
        "as_ptr",
        "as_mut_ptr",
        "MaybeUninit",
        "zeroed",
        "staticmut",
        "extern\"",
        "_mm_load",
        "_mm_store",
        "_mm_lddqu",
        "_mm_stream",
        "_mm256_load",
        "_mm256_store",
        "_mm256_lddqu",
        "_mm256_stream",
        "_mm256_maskload",
        "_mm256_maskstore",
        "gather",
    ] {
        assert!(!all.contains(banned), "{banned}");
    }
}

// Each could put other code behind the text the test above holds: a macro shadowing
// std's detection, or a module read from another file.
#[test]
fn nothing_can_stand_in_for_what_accel_names() {
    let all = squeezed(&sources());
    for banned in [
        "macro_rules",
        "macro_use",
        "externcrate",
        "include!",
        "#[path",
        "path=",
        "r#path",
    ] {
        assert!(!all.contains(banned), "{banned}");
    }
}

// rustc skips a first line of `#!` that does not open an attribute, and a scan that
// knows nothing of that would read what follows as it is told to by the line's tail.
// No file here has one, so none may. td-source-scan's own handling is another matter.
#[test]
fn no_source_begins_with_a_shebang() {
    for (name, _) in &sources() {
        let text = std::fs::read_to_string(base().join(name)).unwrap();
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        assert!(!text.starts_with("#!"), "{name}");
    }
}

// The manifest is where a crate could be read from somewhere other than src/.
#[test]
fn the_manifest_reads_the_crate_from_src_and_denies_unsafe() {
    let manifest = include_str!("../Cargo.toml");
    let lines: Vec<&str> = manifest.lines().map(str::trim).collect();
    assert!(lines.contains(&"unsafe_code = \"deny\""));
    for line in &lines {
        assert!(!line.starts_with("[lib"), "{line}");
        assert!(!line.starts_with("[[bin"), "{line}");
        assert!(!line.starts_with("build"), "{line}");
        assert!(
            !line.starts_with("path") || line.contains("td-source-scan"),
            "{line}"
        );
    }
}

#[test]
fn each_check_asks_exactly_what_its_module_enables() {
    let files = sources();
    let accel = squeeze(ACCEL);
    for (name, text) in &files {
        let is_backend = PROOFS
            .iter()
            .any(|proof| name == &format!("{}.rs", proof.module));
        if !is_backend {
            assert!(!squeeze(text).contains("target_feature"), "{name}");
        }
    }
    for proof in PROOFS {
        let Proof {
            name,
            module,
            called,
            features,
        } = proof;
        let asks: Vec<String> = features
            .iter()
            .map(|feature| format!("is_x86_feature_detected!(\"{feature}\")"))
            .collect();
        let asks = asks.join("&&");
        let asks = if features.len() > 1 {
            format!("({asks})")
        } else {
            asks
        };
        let start =
            format!("impl{name}{{pub(crate)fndetect()->Option<Self>{{{asks}.then_some(Self(()))}}");
        let at = accel.find(&start).unwrap();
        let rest = &accel[at..];
        let body = &rest[..rest.find("#[derive(").unwrap_or(rest.len())];
        assert_eq!(
            body.matches("unsafe{").count(),
            body.matches(&format!("unsafe{{{module}::")).count(),
            "{name} calls outside {module}"
        );
        assert_eq!(body.matches("unsafe{").count(), called.len(), "{name}");
        // Each block is that one call and nothing else.
        for call in *called {
            let block = format!("unsafe{{{module}::{call}}}");
            assert_eq!(body.matches(&block).count(), 1, "{name}: {call}");
        }

        let text = source(&files, &format!("{module}.rs"));
        let squeezed_module = squeeze(&text);
        let attributes = squeezed_module.matches("target_feature(enable=").count();
        let expected = format!("target_feature(enable=\"{}\")", features.join(","));
        assert_eq!(
            squeezed_module.matches(&expected).count(),
            attributes,
            "{module}"
        );
        assert!(attributes > 0, "{module}");
        // What `accel` calls is defined here, under the features: not re-exported from
        // another file, which a same-named definition here would conflict with.
        for call in *called {
            let function = call.split('(').next().unwrap();
            let definition = format!("{expected}]pub(crate)fn{function}(");
            assert_eq!(
                squeezed_module.matches(&definition).count(),
                1,
                "{function}"
            );
        }
        assert!(!squeezed_module.contains("cfg"), "{module}");
        // Every free function in it asks for those features, and none can be
        // called from outside without going through `accel`'s `unsafe`.
        let functions = text
            .lines()
            .filter(|line| line.starts_with("fn ") || line.starts_with("pub(crate) fn "));
        assert_eq!(functions.count(), attributes, "{module}");
    }
}

#[test]
fn only_this_crate_can_reach_the_backends() {
    let lib = squeeze(LIB);
    for module in ["accel", "aes_ni", "chacha_avx2", "sha_ni"] {
        let declared = format!("#[cfg(target_arch=\"x86_64\")]mod{module};");
        assert_eq!(lib.matches(&declared).count(), 1, "{module}");
    }
}
