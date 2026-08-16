//! The committed fixture deployment: payloads, the manifest they hash to, and
//! the signatures over it.
//!
//! Shared, and by `#[path]` rather than by a crate, because the two things that
//! need it cannot depend on each other: `td-boot`'s own tests, where it is
//! `#[cfg(test)]` and never reaches the target binary, and the `recipes`
//! generator that bakes an installable deployment into the `td-install` check.
//!
//! One definition because the SIGNATURES are over the manifest these payloads
//! produce. A second copy of the payload bytes would let one drift under a
//! signature that still verifies for the other — and the direction that fails
//! is the quiet one: the recipe would stage a deployment td-boot refuses, and
//! the check would report a broken installer.
//!
//! No private key is here or anywhere. `td-boot/tests/README` records the rule
//! and the regeneration procedure: one key, every fixture manifest signed under
//! it, the private half discarded. td-boot cannot sign at all — `affected.rs`
//! refuses any file under `td-boot/src` that names `ed25519_sign` — so these
//! are vectors rather than something a test could produce.

/// The one public key every fixture deployment is signed under.
pub const PUBLIC_KEY: &str = "9ae001c47ad75dee6349d9355be09f8c19df0fbd6c038258795b10c84c24bbd1";

/// A second, UNRELATED public key — a real one from a second `td-deploy
/// keygen`, private half likewise discarded.
///
/// Generated rather than derived from the key above by flipping a bit, which is
/// what this was at first. An ed25519 public key is a compressed curve point,
/// so a flipped bit is not reliably a point at all: such a key is refused while
/// being DECODED, and a test built on one proves that a malformed key is
/// rejected rather than that a signature by somebody else's key is. Those are
/// different properties and only the second is what fail-closed means.
pub const OTHER_PUBLIC_KEY: &str =
    "8f73d78d4c82e12acdcc3d1a8addf056df16518732b235de0a824b0f26c62df2";

/// Keyed by tag; `""` is the default deployment's manifest.
///
/// Regenerate the SET together under one fresh key, private half discarded:
///
/// ```text
/// td-net deploy keygen PRIV PUB
/// td-net deploy sign <manifest> PRIV <out>      # once per tag below
/// ```
///
/// where each manifest is `manifest(tag, …)`. A changed payload constant
/// invalidates all of them, which td-boot's
/// `every_committed_fixture_signature_verifies_over_its_own_manifest` reports.
pub const SIGNATURES: [(&str, &str); 8] = [
    (
        "",
        "2e7f46310a09de44bcc2362dd5bf1282d72993fe2608bd92b8b42fd5c7d48378\
         0aca392552c748994765ef3036ee1d43289a5817c6405dd4abf22f272d41eb09",
    ),
    (
        "next",
        "c4324706c24841f5779c144d67a420174c669bcc2b19e12f3b4258a7eb3cf41a\
         d1941c12e9d0e2b72ea3d7f06759c485d55992e7d8251d7259b1c475d2f34805",
    ),
    (
        "previous",
        "debcdd8992e3df4742ac952e3dae6465f03d081af57175a873b216d31026825b\
         0e0a08023330b92d952c2e684457e36bfe9d5e05109baa7a6593de344b440603",
    ),
    (
        "wrong",
        "360c0421b77640ffaa274f7207e6f2838348a0e54d6cb2fd74d9816fe9ca6069\
         f795322b6b98b9275944f254f5fe34d64fc027e4f4f24c342c4cbe87675c8100",
    ),
    (
        "recovery",
        "9acf3fb487a9affda20b719cc6ab58323aa198c8dfddb3aea080ff7bfea8ee37\
         14e8659869f8211f6fdf81c5347ab37c39bf89b305c76e878953fc86c908190d",
    ),
    (
        "first",
        "ef85fb87748d1030c59a0d9433626def520a29bafc5cc368efa3585f23cf89e7\
         914a6eae5ad25a68b2fdf74579ff36a2c7938b97b7bea448dadf2d1e7d06960b",
    ),
    (
        "current",
        "ba3956a36ba965e826e44888776ad9b0d0188762e269651650a7e9867252e594\
         8c7e25d50e2fc862524c18397ea70ee8fbf5191b1a3a1ab54deac1f8f08c2204",
    ),
    (
        "corrupt",
        "10bd5340cdaa56ed2863fa7b81cb69a5e5a3f28ab9baeaa0c7e23dec66165b25\
         3b8656f83332571c05e5e683c13bd92fc53204efc6b0ce450ab3fef34cf2f207",
    ),
];

/// The three payloads a fixture deployment carries, keyed by tag.
pub fn payloads(tag: &str) -> [(&'static str, String); 3] {
    let suffix = if tag.is_empty() { "payload" } else { tag };
    [
        ("bzImage", format!("kernel-{suffix}\n")),
        ("initramfs.cpio", format!("initramfs-{suffix}\n")),
        ("root.erofs", format!("root-{suffix}\n")),
    ]
}

/// The manifest those payloads hash to.
///
/// The digest function is a PARAMETER rather than an import, which is what lets
/// one file serve two crates that reach SHA-256 by different names — td-boot
/// `#[path]`-includes `engine/src/sha256.rs` as `crate::sha256`, and the
/// recipes generator has the same code as a workspace dependency. Passing it in
/// is also why this module needs no imports at all, so it cannot drag anything
/// into the crate that includes it.
pub fn manifest(tag: &str, digest: impl Fn(&[u8]) -> String) -> String {
    let mut manifest = String::from("td-deployment-v1\n");
    for (label, bytes) in &payloads(tag) {
        manifest.push_str(&digest(bytes.as_bytes()));
        manifest.push_str("  ");
        manifest.push_str(label);
        manifest.push('\n');
    }
    manifest
}

/// The committed signature for a tag, as the hexadecimal TEXT a real
/// `manifest.sig` holds — so what a fixture writes goes through the shipped
/// parser on the way back in, rather than around it.
///
/// `Option` rather than a panic: this file is ordinary compiled code in the
/// recipes generator, where the crate's no-panic rule applies.
// A loop rather than the searching iterator adaptor, whose bare name is what
// `ladder.rs` reads as the host search utility in any body a recipe STAGES.
// Nothing stages this file today — that is the point of the `cfg(test)` on its
// module — so this is a precaution rather than a present requirement, kept
// because it costs a line and becomes load-bearing the moment anything does.
// `main.rs`, which this was extracted from, IS staged and so really is bound
// by it.
pub fn signature(tag: &str) -> Option<&'static str> {
    for (name, hex) in SIGNATURES {
        if name == tag {
            return Some(hex);
        }
    }
    None
}
