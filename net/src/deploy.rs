// td-deploy — the HOST half of authenticated deployments.
//
// A deployment bundle's `manifest` already proves INTEGRITY: it carries a
// SHA-256 per payload, and the deployment id is the sha256 of the manifest
// bytes, so a bundle cannot be altered without changing its own name. What it
// cannot prove is AUTHENTICITY. Anyone who can write the volume can write a
// manifest that matches payloads they chose; the hashes agree with themselves.
// This signs the manifest so td-boot can tell a bundle td produced from one
// someone else assembled.
//
// The signature is DETACHED — `manifest.sig` beside `manifest`, never folded
// into it. The deployment id stays sha256(manifest), so a bundle can be
// re-signed under a new key (rotation, or promotion from a test key to a
// release key) WITHOUT becoming a different deployment; folding the signature
// in would make every re-signing a new id, and rollback would stop finding
// what it rolled back to.
//
// Signing is host-side and OUTSIDE any derivation: a key inside a build breaks
// reproducibility (the output depends on a secret) and offline purity (the key
// is an undeclared input). Nothing here ever runs on a target.
//
// WHERE THIS RUNS: no gate builds td-net from source (see the note in
// `ed25519_cross.rs` and the net rule in `builder/src/affected.rs`), so this
// applet's own tests are a developer and prep-time check:
//
//     CC=<cc> cargo test --manifest-path net/Cargo.toml
//
// The direction that can rot is covered where it does run: the engine's
// `ed25519.rs` carries committed fixtures and its tests ride the workspace
// gate. What is uncovered is `ring` changing what it signs.

// td-boot's OWN manifest contract, not a second copy of it: the header it
// requires and the size bound it enforces come from the crate that verifies,
// so a signer that drifted from them would emit signatures no machine could
// check and the failure would surface at boot rather than at signing.
use crate::protocol::{MANIFEST_HEADER, MAX_MANIFEST_BYTES};
use crate::sig::{from_hex, keygen, sign_msg, to_hex, verify_msg, write_keypair};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

fn die(msg: String) -> ! {
    eprintln!("td-deploy: {msg}");
    std::process::exit(1);
}

/// Read a manifest exactly as td-boot will, then sign those bytes.
///
/// The bytes are signed verbatim — no canonicalisation, no trailing-newline
/// fixups — because the verifier hashes the same file to derive the deployment
/// id, and anything this function normalised would be a byte the two halves
/// disagree about.
fn sign_manifest(manifest: &Path, pkcs8: &[u8]) -> Result<String, String> {
    let bytes = read_as_td_boot_would(manifest)?;
    // The size bound's own argument, applied to the CONTENT: a manifest td-boot
    // would refuse to parse is one whose signature could never be checked. It
    // is also the only thing separating the two signing domains at the message
    // level — neither tool tags what it signs — so a deployment key asked to
    // sign an arbitrary blob says no.
    if !bytes.starts_with(MANIFEST_HEADER) {
        return Err(format!(
            "{} does not begin with td-boot's {} header; refusing to sign a blob \
             the verifier would reject",
            manifest.display(),
            String::from_utf8_lossy(MANIFEST_HEADER)
        ));
    }
    let sig = sign_msg(pkcs8, &bytes)?;
    Ok(format!("{}\n", to_hex(&sig)))
}

/// `td-boot`'s `open_real_file` + `read_bounded_real_file`, SHARED rather than
/// mirrored: a signer that accepts what the target refuses produces a bundle
/// that fails at boot instead of at signing, and a rule written twice to match
/// is the arrangement that guarantees the two eventually will not.
///
/// The one check that stays here is the signer's own. Emptiness is not a thing
/// td-boot has an opinion about — an empty manifest simply fails its header
/// check — but signing nothing is worth refusing where the signature is made.
fn read_as_td_boot_would(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = crate::realfile::read_bounded_real_file(path, "the manifest", MAX_MANIFEST_BYTES)
        .map_err(|error| error.to_string())?;
    if bytes.is_empty() {
        return Err(format!(
            "{} is empty — refusing to sign nothing",
            path.display()
        ));
    }
    Ok(bytes)
}

/// A signature written OVER the manifest changes the deployment id, which is
/// the one thing the detached form exists to prevent — `td-deploy sign m k m`
/// would silently rename the deployment it just authorised. Compared by
/// resolved path so a symlink or a `./` spelling cannot slip past; an OUT that
/// does not exist yet is the normal case and resolves to nothing.
fn refuse_signing_over_the_manifest(manifest: &Path, out: &Path) -> Result<(), String> {
    let (Ok(m), Ok(o)) = (manifest.canonicalize(), out.canonicalize()) else {
        return Ok(());
    };
    if m == o {
        return Err(format!(
            "OUT is the manifest itself ({}); the signature is DETACHED, and writing it \
             here would change the deployment id",
            m.display()
        ));
    }
    Ok(())
}

/// Write through a temporary and rename, so a crash leaves either the old
/// signature or the new one and never a truncated file that reads as neither.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("sig.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {e}", tmp.display(), path.display())
    })
}

// Matched as a SLICE rather than by `a.len()` plus `a[2]`: the crate-level
// allow for panicking indexing is grandfathered for applets that pre-date the
// lint rules (main.rs), and new code should not need it. The arity is the
// pattern, so the arguments cannot be read out of step with the guard.
pub fn run(a: &[String]) {
    match a {
        [_, verb, priv_path, pub_path] if verb == "keygen" => {
            let (pkcs8, pubkey) = keygen().unwrap_or_else(|e| die(e));
            write_keypair(priv_path, &pkcs8, pub_path, &pubkey).unwrap_or_else(|e| die(e));
            println!(
                "td-deploy: keygen OK — private (pkcs8, 0600) -> {priv_path}, \
                 public (hex) -> {pub_path}"
            );
        }
        [_, verb, manifest, priv_path, out] if verb == "sign" => {
            let (manifest, out) = (Path::new(manifest), Path::new(out));
            refuse_signing_over_the_manifest(manifest, out).unwrap_or_else(|e| die(e));
            let pkcs8 =
                std::fs::read(priv_path).unwrap_or_else(|e| die(format!("read {priv_path}: {e}")));
            let hex = sign_manifest(manifest, &pkcs8).unwrap_or_else(|e| die(e));
            write_atomic(out, hex.as_bytes())
                .unwrap_or_else(|e| die(format!("write {}: {e}", out.display())));
            println!(
                "td-deploy: sign OK — {} -> {}",
                manifest.display(),
                out.display()
            );
        }
        [_, verb] if verb == "selftest" => selftest(),
        _ => {
            eprintln!(
                "usage:\n  td-deploy keygen PRIV PUB\n  \
                 td-deploy sign MANIFEST PRIV OUT\n  td-deploy selftest"
            );
            std::process::exit(2);
        }
    }
}

/// A scratch directory this process OWNS, created rather than created-if-absent.
///
/// The name is predictable (it is a pid), so `create_dir_all` would happily
/// adopt whatever a local user had planted there — and this is a tool that may
/// run as root, which makes a planted `manifest` symlink an arbitrary-file
/// clobber. `create_dir` refuses an existing path, symlink included, and 0700
/// stops anything being planted INSIDE the directory afterwards.
fn private_scratch_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("td-deploy-selftest-{}", std::process::id()));
    // A leftover from a crashed run with this pid; removing a SYMLINK here
    // fails, which is what keeps the create below fail-closed.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .unwrap_or_else(|e| die(format!("mkdir {}: {e}", dir.display())));
    dir
}

/// Sign a manifest on a FRESH key and assert both directions: the signature
/// verifies, and each way of being wrong is refused. Keys are fresh per run, so
/// this asserts over a distribution rather than one recorded point.
fn selftest() {
    let dir = private_scratch_dir();
    let manifest = dir.join("manifest");
    let body = b"td-deployment-v1\n\
                 0000000000000000000000000000000000000000000000000000000000000000  bzImage\n\
                 1111111111111111111111111111111111111111111111111111111111111111  initramfs.cpio\n\
                 2222222222222222222222222222222222222222222222222222222222222222  root.erofs\n";
    std::fs::write(&manifest, body).unwrap_or_else(|e| die(format!("write manifest: {e}")));

    let (pkcs8, pubkey) = keygen().unwrap();
    let hex = sign_manifest(&manifest, &pkcs8).unwrap_or_else(|e| die(e));
    let sig = from_hex(&hex).unwrap_or_else(|e| die(format!("own signature: {e}")));
    if !verify_msg(&pubkey, body, &sig) {
        die("a freshly signed manifest did not verify".into());
    }

    // Wrong key.
    let (_, other) = keygen().unwrap();
    if verify_msg(&other, body, &sig) {
        die("a manifest verified under a key that did not sign it".into());
    }
    // Tampered manifest — one byte of one digest, the change an attacker
    // swapping a payload would have to make.
    let mut tampered = body.to_vec();
    if let Some(b) = tampered.get_mut(20) {
        *b = if *b == b'0' { b'1' } else { b'0' };
    }
    if verify_msg(&pubkey, &tampered, &sig) {
        die("a tampered manifest verified".into());
    }
    // Oversized manifests are refused rather than signed.
    let big = dir.join("big");
    std::fs::write(&big, vec![b'x'; (MAX_MANIFEST_BYTES + 1) as usize])
        .unwrap_or_else(|e| die(format!("write big: {e}")));
    if sign_manifest(&big, &pkcs8).is_ok() {
        die("a manifest too large for td-boot to read was signed anyway".into());
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!("td-deploy: selftest OK — signed, verified, and refused wrong key/tamper/oversize");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, INCLUDING when an assertion
    /// fails — the manual `remove_dir_all` at the end of a test is exactly the
    /// line an early panic skips, so a failing test used to leave its fixture
    /// behind every run.
    struct Scratch(std::path::PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Scratch {
        let d = std::env::temp_dir().join(format!("td-deploy-t-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        Scratch(d)
    }

    /// The property the whole feature turns on: what `ring` signs here, the
    /// ENGINE verifier accepts — that is the code compiled into td-boot, where
    /// no external crate may go. A disagreement is a correctly signed bundle
    /// that refuses to boot.
    #[test]
    fn what_ring_signs_the_engine_verifier_accepts() {
        let dir = tmp("cross");
        let manifest = dir.join("manifest");
        let body = b"td-deployment-v1\n\
                     aa  bzImage\n";
        std::fs::write(&manifest, body).unwrap();
        let (pkcs8, pubkey) = keygen().unwrap();
        let hex = sign_manifest(&manifest, &pkcs8).unwrap();
        let sig = from_hex(&hex).unwrap();

        let mut pk = [0u8; 32];
        pk.copy_from_slice(&pubkey);
        let mut s = [0u8; 64];
        s.copy_from_slice(&sig);
        assert!(
            crate::ed25519::verify(&pk, body, &s),
            "the engine verifier rejected a signature ring produced"
        );
        // Same rejection half, through the engine verifier this time.
        let (_, other) = keygen().unwrap();
        let mut opk = [0u8; 32];
        opk.copy_from_slice(&other);
        assert!(!crate::ed25519::verify(&opk, body, &s), "wrong key must be refused");
    }

    #[test]
    fn the_signature_is_over_the_exact_bytes_so_the_id_is_untouched() {
        let dir = tmp("exact");
        let manifest = dir.join("manifest");
        // No trailing newline, and a byte a canonicaliser would be tempted to
        // touch: the signer must sign what is on disk.
        let body = b"td-deployment-v1\n  trailing-space-and-no-final-newline  ";
        std::fs::write(&manifest, body).unwrap();
        let (pkcs8, pubkey) = keygen().unwrap();
        let sig = from_hex(&sign_manifest(&manifest, &pkcs8).unwrap()).unwrap();
        assert!(verify_msg(&pubkey, body, &sig), "signed bytes are the file's bytes");
    }

    #[test]
    fn re_signing_under_a_new_key_leaves_the_manifest_byte_identical() {
        let dir = tmp("resign");
        let manifest = dir.join("manifest");
        let body = b"td-deployment-v1\nzz  bzImage\n";
        std::fs::write(&manifest, body).unwrap();
        let before = std::fs::read(&manifest).unwrap();
        let (a_priv, a_pub) = keygen().unwrap();
        let (b_priv, b_pub) = keygen().unwrap();
        let sig_a = from_hex(&sign_manifest(&manifest, &a_priv).unwrap()).unwrap();
        let sig_b = from_hex(&sign_manifest(&manifest, &b_priv).unwrap()).unwrap();
        // D3: the id is sha256(manifest), and signing did not touch the manifest.
        assert_eq!(before, std::fs::read(&manifest).unwrap(), "signing must not rewrite the manifest");
        assert_ne!(sig_a, sig_b, "different keys, different signatures");
        assert!(verify_msg(&a_pub, body, &sig_a));
        assert!(verify_msg(&b_pub, body, &sig_b));
        assert!(!verify_msg(&a_pub, body, &sig_b), "keys are not interchangeable");
    }

    /// A signing key readable by anyone forges whatever it authorises, and a
    /// second `keygen` that silently replaced one would leave machines pinned
    /// to a key nothing matches. Both are asserted, since neither is visible
    /// in the signature the tool produces.
    #[test]
    fn a_generated_private_key_is_not_readable_and_is_not_replaceable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("keyperm");
        let priv_path = dir.join("k.pkcs8");
        let pub_path = dir.join("k.pub");
        let (p, pubkey) = keygen().unwrap();
        let (ps, pps) = (
            priv_path.to_string_lossy().to_string(),
            pub_path.to_string_lossy().to_string(),
        );
        write_keypair(&ps, &p, &pps, &pubkey).unwrap();
        let mode = std::fs::metadata(&priv_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "private key must not be group/world readable");
        // Refuses rather than truncating — either half already present is an error.
        assert!(write_keypair(&ps, &p, &pps, &pubkey).is_err(), "must not replace a key");
        let kept = std::fs::read(&priv_path).unwrap();
        assert_eq!(kept, p, "the existing key survived the refusal");
    }

    /// A blob td-boot's parser would reject is a blob whose signature could
    /// never be checked — and with no domain string in either message, the
    /// header is the only thing stopping a deployment key signing arbitrary
    /// operator-chosen bytes.
    #[test]
    fn a_manifest_that_is_not_one_is_refused() {
        let dir = tmp("header");
        let (pkcs8, _) = keygen().unwrap();
        let good = dir.join("good");
        std::fs::write(&good, b"td-deployment-v1\nzz  bzImage\n").unwrap();
        assert!(sign_manifest(&good, &pkcs8).is_ok(), "the real header signs");
        for (name, body) in [
            ("wrong", &b"td-deployment-v2\nzz  bzImage\n"[..]),
            ("blob", &b"anything at all\n"[..]),
            ("short", &b"td-deploy"[..]),
            ("leading", &b"\ntd-deployment-v1\n"[..]),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            refused_because(sign_manifest(&p, &pkcs8), "does not begin with");
        }
    }

    /// The detached form exists so re-signing does not rename the deployment.
    /// Writing the signature over the manifest would do exactly that.
    #[test]
    fn signing_over_the_manifest_is_refused() {
        let dir = tmp("outpath");
        let manifest = dir.join("manifest");
        std::fs::write(&manifest, b"td-deployment-v1\nzz  bzImage\n").unwrap();
        let sig = dir.join("manifest.sig");
        assert!(refuse_signing_over_the_manifest(&manifest, &sig).is_ok(), "beside it is fine");
        assert!(
            refuse_signing_over_the_manifest(&manifest, &manifest).is_err(),
            "the same path must be refused"
        );
        // And by a spelling that only resolves to the same file.
        let indirect = dir.join(".").join("manifest");
        assert!(
            refuse_signing_over_the_manifest(&manifest, &indirect).is_err(),
            "a different spelling of the same file must be refused too"
        );
    }

    /// td-boot refuses a symlinked manifest; so must the signer, or a bundle
    /// signs here and is rejected there.
    #[test]
    fn a_symlinked_manifest_is_refused_as_td_boot_refuses_it() {
        let dir = tmp("symlink");
        let real = dir.join("real");
        std::fs::write(&real, b"td-deployment-v1\nzz  bzImage\n").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (pkcs8, _) = keygen().unwrap();
        assert!(sign_manifest(&real, &pkcs8).is_ok(), "the real file signs");
        refused_because(sign_manifest(&link, &pkcs8), "must be a real regular file");
    }

    /// Each refusal is matched against the DIAGNOSTIC it should carry, not
    /// merely against `is_err()`: every one of these fixtures could fail for an
    /// unrelated reason — an unwritable temp dir, say — and a bare `is_err()`
    /// would call that a pass while testing nothing.
    #[track_caller]
    fn refused_because(r: Result<String, String>, needle: &str) {
        match r {
            Ok(_) => panic!("expected a refusal mentioning {needle:?}, got success"),
            Err(e) => assert!(e.contains(needle), "refused, but for {e:?} not {needle:?}"),
        }
    }

    #[test]
    fn an_empty_or_oversized_or_missing_manifest_is_refused() {
        let dir = tmp("bounds");
        let (pkcs8, _) = keygen().unwrap();
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        refused_because(sign_manifest(&empty, &pkcs8), "refusing to sign nothing");
        // Both bound cases carry the real header, so what they exercise is the
        // SIZE — padding alone would be refused for the wrong reason and the
        // `edge` assertion would stop proving the bound is inclusive.
        let padded = |n: u64| {
            let mut v = MANIFEST_HEADER.to_vec();
            v.resize(usize::try_from(n).unwrap(), b'x');
            v
        };
        let big = dir.join("big");
        std::fs::write(&big, padded(MAX_MANIFEST_BYTES + 1)).unwrap();
        refused_because(
            sign_manifest(&big, &pkcs8),
            &format!("the manifest exceeds {MAX_MANIFEST_BYTES} bytes"),
        );
        // Exactly at the bound is fine — the bound is td-boot's read limit.
        let edge = dir.join("edge");
        std::fs::write(&edge, padded(MAX_MANIFEST_BYTES)).unwrap();
        assert!(sign_manifest(&edge, &pkcs8).is_ok(), "at the bound");
        refused_because(sign_manifest(&dir.join("nope"), &pkcs8), "the manifest ");
        refused_because(sign_manifest(&dir, &pkcs8), "must be a real regular file");
    }
}
