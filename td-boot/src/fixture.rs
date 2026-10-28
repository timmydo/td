//! The committed fixture deployment: payloads and the manifest they hash to.
//!
//! Shared, and by `#[path]` rather than by a crate, because the two things that
//! need it cannot depend on each other: `td-boot`'s own tests, where it is
//! `#[cfg(test)]` and never reaches the target binary, and the `recipes`
//! generator that bakes an installable deployment into the `td-install` check.
//! One definition, so the deployment the check stages is one td-boot's own
//! tests verify.

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
