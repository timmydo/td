// td-subst's ed25519 + hex primitives: td-primitives signs and verifies narinfos.
// Nothing here generates or names a key — that is the applet's own verb.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use td_primitives::curve25519::ed25519::{self, SigningKey, PUBLIC_LEN, SEED_LEN, SIGNATURE_LEN};

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
    let key = from_pkcs8(pkcs8)?;
    let sig = key
        .sign(msg)
        .map_err(|_| "ed25519 signing failed".to_string())?;
    if !verify_msg(&key.public_key(), msg, &sig) {
        return Err("the signature just produced does not verify under its own key".into());
    }
    Ok(sig.to_vec())
}

pub(crate) fn verify_msg(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let (Ok(pubkey), Ok(sig)) = (
        <&[u8; PUBLIC_LEN]>::try_from(pubkey),
        <&[u8; SIGNATURE_LEN]>::try_from(sig),
    ) else {
        return false;
    };
    ed25519::verify(pubkey, msg, sig).is_ok()
}

// The key files `keygen` has always written: an RFC 5958 v2 OneAsymmetricKey,
// the seed and then its public half, each at a fixed offset. Machines hold these,
// so the shape does not change.
const PKCS8_HEAD: &[u8] = &[
    0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const PKCS8_MIDDLE: &[u8] = &[0x81, 0x21, 0x00];

/// The public half must be the seed's, so a damaged file refuses to sign.
fn from_pkcs8(pkcs8: &[u8]) -> Result<SigningKey, String> {
    let bad = || "bad private key: not an ed25519 PKCS#8 v2 key".to_string();
    let rest = pkcs8.strip_prefix(PKCS8_HEAD).ok_or_else(bad)?;
    let (seed, rest) = rest.split_at_checked(SEED_LEN).ok_or_else(bad)?;
    let public = rest.strip_prefix(PKCS8_MIDDLE).ok_or_else(bad)?;
    let seed = <&[u8; SEED_LEN]>::try_from(seed).map_err(|_| bad())?;
    let key = SigningKey::from_seed(seed).map_err(|_| bad())?;
    if key.public_key().as_slice() != public {
        return Err("bad private key: its public half is not the seed's".into());
    }
    Ok(key)
}

/// A fresh keypair: pkcs8 private half, raw 32-byte public half.
///
/// Fallible rather than panicking: both steps are reachable from a `keygen`
/// verb an operator runs, and a failed draw from the system RNG should print a
/// diagnostic, not a backtrace.
pub(crate) fn keygen() -> Result<(Vec<u8>, Vec<u8>), String> {
    let failed = |_| "generate ed25519 key: no entropy".to_string();
    let seed = SigningKey::generate_seed().map_err(failed)?;
    let public = SigningKey::from_seed(&seed).map_err(failed)?.public_key();
    let mut pkcs8 =
        Vec::with_capacity(PKCS8_HEAD.len() + SEED_LEN + PKCS8_MIDDLE.len() + PUBLIC_LEN);
    pkcs8.extend_from_slice(PKCS8_HEAD);
    pkcs8.extend_from_slice(&seed);
    pkcs8.extend_from_slice(PKCS8_MIDDLE);
    pkcs8.extend_from_slice(&public);
    Ok((pkcs8, public.to_vec()))
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
pub(crate) fn write_keypair(
    priv_path: &str,
    pkcs8: &[u8],
    pub_path: &str,
    pubkey: &[u8],
) -> Result<(), String> {
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
        assert_eq!(
            from_hex("00017F80FF").unwrap(),
            bytes,
            "uppercase decodes too"
        );
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
        for s in [
            "a\u{20ac}",
            "\u{20ac}\u{20ac}",
            "\u{00e9}\u{00e9}",
            "ab\u{00e9}",
        ] {
            assert!(from_hex(s).is_err(), "{s:?} must be refused, not fatal");
        }
    }

    // A key and signature written by ring's Ed25519KeyPair before td-net left
    // it: the file every existing machine holds must go on signing identically.
    const RING_PKCS8: &str = "3051020101300506032b6570042204205152af2a19d678300265f818bcd2f8ff44020c3a75c2876b3ff7f24440aad07f8121009b63491dc4a7378eb929a448da5aa11dac4946c74167753439d90d0cb8f81755";
    const RING_PUBLIC: &str = "9b63491dc4a7378eb929a448da5aa11dac4946c74167753439d90d0cb8f81755";
    const RING_SIGNATURE: &str = "c57935e941936373a7122fc30895b248e349b7275733926619f8ac3f2b2f2173a67361c1f609553a86b4c2b4ba1c22c8cb5f3959759dcda54652fb1822fecd0f";

    #[test]
    fn a_key_file_ring_wrote_signs_as_ring_signed() {
        let pkcs8 = from_hex(RING_PKCS8).unwrap();
        let sig = sign_msg(&pkcs8, b"td-deployment-v1\n").unwrap();
        assert_eq!(to_hex(&sig), RING_SIGNATURE);
        assert!(verify_msg(
            &from_hex(RING_PUBLIC).unwrap(),
            b"td-deployment-v1\n",
            &sig
        ));
    }

    #[test]
    fn a_fresh_key_has_the_shape_ring_wrote() {
        let (fresh, public) = keygen().unwrap();
        let old = from_hex(RING_PKCS8).unwrap();
        assert_eq!(fresh.len(), old.len());
        assert_eq!(fresh[..16], old[..16]);
        assert_eq!(fresh[48..51], old[48..51]);
        assert_eq!(fresh[51..], public[..]);
    }

    #[test]
    fn a_damaged_key_file_refuses_to_sign() {
        let good = from_hex(RING_PKCS8).unwrap();
        assert!(sign_msg(&good, b"m").is_ok());
        for at in [0, 15, 16, 47, 48, 50, 51, good.len() - 1] {
            let mut bad = good.clone();
            bad[at] ^= 1;
            assert!(sign_msg(&bad, b"m").is_err(), "byte {at}");
        }
        assert!(sign_msg(&good[..good.len() - 1], b"m").is_err(), "short");
        let mut long = good.clone();
        long.push(0);
        assert!(sign_msg(&long, b"m").is_err(), "trailing");
        assert!(!verify_msg(&[0; 31], b"m", &[0; 64]), "short public key");
        assert!(!verify_msg(&[0; 32], b"m", &[0; 63]), "short signature");
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
        assert!(
            !verify_msg(&other, msg, &sig),
            "a different key must not verify"
        );
        assert!(
            !verify_msg(&pubkey, b"td-deployment-v2\n", &sig),
            "a changed message must not verify"
        );
        let mut bad = sig.clone();
        if let Some(b) = bad.first_mut() {
            *b ^= 1;
        }
        assert!(
            !verify_msg(&pubkey, msg, &bad),
            "a mangled signature must not verify"
        );
    }
}
