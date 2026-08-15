// The ed25519 + hex primitives td's two SIGNING domains share. `ring` signs on
// this side; what verifies differs by domain — `ring` again for narinfos
// (td-subst fetch), and the engine's dependency-free verifier for deployment
// manifests, since td-boot may carry no external crate.
//
// One copy rather than two for the reason `engine/src/crc32.rs` is one copy:
// the private-per-caller version is a divergence waiting to happen, and these
// are the functions where a divergence is a signature that verifies under one
// half of td and not the other.
//
// The DOMAINS stay separate even though the primitives do not. td-subst's key
// says "this store path came from this builder" and the deployment key says
// "this is a system you may boot"; sharing one key would make a substituter
// compromise a boot compromise. Nothing here generates or names a key — that
// is each applet's own verb.

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode lowercase-or-uppercase hex, over BYTES.
///
/// The `&s[i..i + 2]` form this replaces sliced on CHAR boundaries and so
/// panicked on any multi-byte character — `from_hex("a€")` aborted with `end
/// byte index 2 is not a char boundary`. That is reachable from an untrusted
/// peer today: `subst::fetch` hands a fetched narinfo's `Sig:` value here
/// BEFORE checking the signature, so a hostile binary cache could kill the
/// fetching builder with two bytes. Nibble decoding is also stricter than
/// `from_str_radix`, which accepts a leading `+`.
pub(crate) fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().as_bytes();
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".into());
    }
    // `as_chunks` rather than `chunks_exact` so each pair is a `[u8; 2]` and
    // the destructuring is total — no arm for a short chunk that cannot occur.
    s.as_chunks::<2>()
        .0
        .iter()
        .map(|&[hi, lo]| Ok((nibble(hi)? << 4) | nibble(lo)?))
        .collect()
}

fn nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("bad hex byte {b:#04x}")),
    }
}

/// Sign, and VERIFY what was signed before handing it back.
///
/// The readback is this crate's rule wherever nothing observable distinguishes
/// the wrong outcome (UNSAFE.md's losetup and termios cases make the same
/// argument): a corrupted signature looks exactly like a good one until a
/// machine refuses to boot, or a builder refuses a cache. Checking here rather
/// than at each call site is what makes it hold for both signing domains.
pub(crate) fn sign_msg(pkcs8: &[u8], msg: &[u8]) -> Result<Vec<u8>, String> {
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| format!("bad private key: {e}"))?;
    let sig = kp.sign(msg).as_ref().to_vec();
    if !verify_msg(kp.public_key().as_ref(), msg, &sig) {
        return Err("the signature just produced does not verify under its own key".into());
    }
    Ok(sig)
}

pub(crate) fn verify_msg(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    UnparsedPublicKey::new(&ED25519, pubkey).verify(msg, sig).is_ok()
}

/// A fresh keypair: pkcs8 private half, raw 32-byte public half.
///
/// Fallible rather than panicking: both steps are reachable from a `keygen`
/// verb an operator runs, and a failed draw from the system RNG should print a
/// diagnostic, not a backtrace.
pub(crate) fn keygen() -> Result<(Vec<u8>, Vec<u8>), String> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|e| format!("generate ed25519 key: {e}"))?;
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|e| format!("parse generated key: {e}"))?;
    Ok((pkcs8.as_ref().to_vec(), kp.public_key().as_ref().to_vec()))
}

/// Write a freshly generated keypair, private half FIRST and restrictively.
///
/// `std::fs::write` is wrong for a signing key twice over: it creates at
/// `0666 & umask` — world-readable on an ordinary host, and a signing key any
/// local reader can copy is one that forges whatever it authorises — and it
/// TRUNCATES, so a second `keygen` silently destroys a key that machines are
/// still pinned to, leaving nothing that matches. Both halves are therefore
/// created exclusively (`create_new`), so an existing path is an error rather
/// than a replacement, and the private half is `0600` from the moment it
/// exists rather than chmod'ed after — a window is all a reader needs.
pub(crate) fn write_keypair(priv_path: &str, pkcs8: &[u8], pub_path: &str, pubkey: &[u8]) -> Result<(), String> {
    write_new(priv_path, pkcs8, 0o600)?;
    write_new(pub_path, format!("{}\n", to_hex(pubkey)).as_bytes(), 0o644)
}

fn write_new(path: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| format!("create {path}: {e}"))?;
    f.write_all(bytes).map_err(|e| format!("write {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_and_refuses_malformed() {
        let bytes = [0x00u8, 0x01, 0x7f, 0x80, 0xff];
        assert_eq!(to_hex(&bytes), "00017f80ff");
        assert_eq!(from_hex("00017f80ff").unwrap(), bytes);
        // Trailing newline is how every committed `.pub` is written.
        assert_eq!(from_hex("00017f80ff\n").unwrap(), bytes);
        assert!(from_hex("abc").is_err(), "odd length");
        assert!(from_hex("zz").is_err(), "not hex");
        assert_eq!(from_hex("00017F80FF").unwrap(), bytes, "uppercase decodes too");
        // `from_str_radix` took a sign; a signature parser must not.
        assert!(from_hex("+f").is_err(), "a signed nibble is not hex");
    }

    /// The `Sig:` value of a fetched narinfo reaches `from_hex` BEFORE the
    /// signature is checked, so a hostile cache decides these bytes. Slicing
    /// `&s[i..i + 2]` panicked on every one of them.
    #[test]
    fn a_multi_byte_character_is_an_error_and_not_a_panic() {
        // All even-length, so each reaches the decoder the old form panicked in
        // rather than stopping at the length check.
        for s in ["a\u{20ac}", "\u{20ac}\u{20ac}", "\u{00e9}\u{00e9}", "ab\u{00e9}"] {
            assert!(from_hex(s).is_err(), "{s:?} must be refused, not fatal");
        }
    }

    #[test]
    fn a_fresh_key_signs_and_verifies_and_a_second_one_does_not() {
        let (pkcs8, pubkey) = keygen().unwrap();
        assert_eq!(pubkey.len(), 32, "raw ed25519 public key");
        let msg = b"td-deployment-v1\n";
        let sig = sign_msg(&pkcs8, msg).unwrap();
        assert_eq!(sig.len(), 64, "raw ed25519 signature");
        assert!(verify_msg(&pubkey, msg, &sig));
        // The rejection half: an acceptance-only test passes against a verifier
        // that returns true unconditionally.
        let (_, other) = keygen().unwrap();
        assert!(!verify_msg(&other, msg, &sig), "a different key must not verify");
        assert!(!verify_msg(&pubkey, b"td-deployment-v2\n", &sig), "a changed message must not verify");
        let mut bad = sig.clone();
        if let Some(b) = bad.first_mut() {
            *b ^= 1;
        }
        assert!(!verify_msg(&pubkey, msg, &bad), "a mangled signature must not verify");
    }
}
