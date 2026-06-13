//! Store-path hashing — the daemon's content-addressed naming
//! (nix/libstore/store-api.cc makeStorePath / makeTextPath, nix/libutil/hash.cc
//! printHash32 + compressHash, read off the pin). Lets td compute a `.drv`'s own
//! store path (and, later, derivation output paths) WITHOUT guile — the
//! evaluator-as-library track.

use crate::sha256::{self, Sha256};

/// Guix's store directory (the daemon's `storeDir`).
pub const STORE_DIR: &str = "/gnu/store";

/// The nix store-path base-32 alphabet (omits e, o, u, t).
const BASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// nix base-32 of a digest, low-bit-first, MSB char first — the exact order
/// printHash32 emits (n from nrChars-1 down to 0).
pub fn base32(hash: &[u8]) -> String {
    let nchars = (hash.len() * 8 + 4) / 5; // ceil(bits / 5)
    let mut s = Vec::with_capacity(nchars);
    for n in (0..nchars).rev() {
        let b = n * 5;
        let (i, j) = (b / 8, b % 8);
        let c = (hash[i] as u16 >> j)
            | if i + 1 < hash.len() {
                (hash[i + 1] as u16) << (8 - j)
            } else {
                0
            };
        s.push(BASE32[(c & 0x1f) as usize]);
    }
    String::from_utf8(s).expect("base32 alphabet is ASCII")
}

/// XOR-fold a hash down to `size` bytes (nix compressHash).
fn compress_hash(hash: &[u8], size: usize) -> Vec<u8> {
    let mut out = vec![0u8; size];
    for (i, &b) in hash.iter().enumerate() {
        out[i % size] ^= b;
    }
    out
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// makeStorePath(type, inner-hash-hex, name): fingerprint
/// `type:sha256:<hex>:<storeDir>:<name>`, hashed and compressed to 20 bytes, then
/// base-32 — the store path's digest part.
pub fn make_store_path(ty: &str, inner_hash_hex: &str, name: &str) -> String {
    let fingerprint = format!("{ty}:sha256:{inner_hash_hex}:{STORE_DIR}:{name}");
    let compressed = compress_hash(&sha256_bytes(fingerprint.as_bytes()), 20);
    format!("{STORE_DIR}/{}-{}", base32(&compressed), name)
}

/// makeTextPath: a `.drv` (or any addTextToStore item) is content-addressed with
/// its references carried in the type (`text:` + the sorted refs), and the inner
/// hash is sha256 of the file content.
pub fn make_text_path(name: &str, content: &[u8], refs: &[String]) -> String {
    let mut refs = refs.to_vec();
    refs.sort();
    let ty = format!("text:{}", refs.join(":"));
    let content_hex = sha256::to_base16(&sha256_bytes(content));
    make_store_path(&ty, &content_hex, name)
}

/// The `<name>` part of a store path `/gnu/store/<32-char digest>-<name>` — the
/// 32-char base-32 digest plus its `-` separator are fixed-width.
pub fn name_from_store_path(path: &str) -> Option<String> {
    let base = path.rsplit('/').next()?;
    // 32 digest chars + '-'; the name itself may contain '-'.
    if base.len() > 33 && base.as_bytes()[32] == b'-' {
        Some(base[33..].to_string())
    } else {
        None
    }
}

/// The store path a `.drv` with `content` and `refs` (its inputDrvs ∪ inputSrcs)
/// would be written to.
pub fn drv_store_path(name: &str, content: &[u8], refs: &[String]) -> String {
    make_text_path(name, content, refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_extraction() {
        assert_eq!(
            name_from_store_path("/gnu/store/2nfg943asrl9dv64zrr1a4kpb25mfafd-hello-2.12.2.drv")
                .unwrap(),
            "hello-2.12.2.drv"
        );
        assert_eq!(name_from_store_path("/gnu/store/short").is_none(), true);
    }

    #[test]
    fn base32_length_for_20_bytes() {
        // A 20-byte (compressed) hash encodes to 32 base-32 chars — store-path width.
        assert_eq!(base32(&[0u8; 20]).len(), 32);
        assert_eq!(base32(&[0u8; 20]), "0".repeat(32));
    }
}
