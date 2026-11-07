//! `openssh-key-v1` Ed25519 private keys and one-line public keys: existing
//! files, host keys above all, stay valid across the move from OpenSSH.

use crate::error::{Error, Result};
use crate::kex::Secret32;
use crate::kex::{ed25519_public_blob, parse_ed25519_public, HOST_KEY_ALGORITHM};
use crate::wire::{Reader, Writer};
use td_encoding::{base64, from_base64};
use td_primitives::aes256_ctr::Aes256Ctr;
use td_primitives::bcrypt_pbkdf::bcrypt_pbkdf;
use td_primitives::curve25519::ed25519;
use td_primitives::sha::{Digest, Sha256};

const MAGIC: &[u8] = b"openssh-key-v1\0";
const BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
const END: &str = "-----END OPENSSH PRIVATE KEY-----";
const CIPHER_NONE: &str = "none";
const CIPHER_AES: &str = "aes256-ctr";
const KDF_NONE: &str = "none";
const KDF_BCRYPT: &str = "bcrypt";
const AES_KEY_LEN: usize = 32;
const AES_IV_LEN: usize = 16;
const AES_BLOCK: usize = 16;
const NONE_BLOCK: usize = 8;
/// A work factor beyond this is treated as hostile: it would pin a CPU for
/// hours decoding a file someone else handed over.
const MAX_ROUNDS: u32 = 1024;
/// Far above any Ed25519 key file; refuses absurd input before decoding it.
pub const MAX_PRIVATE_KEY_FILE: usize = 16 * 1024;

/// Key material on the heap, wiped however its scope ends.
struct Wiped(Vec<u8>);

impl Drop for Wiped {
    fn drop(&mut self) {
        td_primitives::ct::zero(&mut self.0);
    }
}

/// Overwrites key-file text, which for an unencrypted key is the key.
/// The secret section without its comment and padding: two check words, then the
/// algorithm, public key and both halves of the private one, each length-prefixed.
const SECRET_SECTION_FIXED: usize = 8 + (4 + HOST_KEY_ALGORITHM.len()) + (4 + 32) + (4 + 64) + 4;
/// The rest of the file around that section: magic, cipher, KDF and its options, the
/// key count and the public blob, all a good deal smaller than this.
const OUTER_ROOM: usize = 256;

fn secret_section_size(comment: usize, block: usize) -> usize {
    SECRET_SECTION_FIXED
        .saturating_add(comment)
        .saturating_add(block)
}

pub fn wipe_text(text: String) {
    drop(Wiped(text.into_bytes()));
}

/// The fingerprint's base64 drops padding, as OpenSSH prints it.
pub fn fingerprint_of_blob(blob: &[u8]) -> String {
    let encoded = base64(&Sha256::digest(blob));
    format!("SHA256:{}", encoded.trim_end_matches('='))
}

/// One plain `ssh-ed25519 BASE64 [comment]` public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub key: [u8; 32],
    pub comment: String,
}

impl PublicKey {
    pub fn parse_line(line: &str) -> Result<Self> {
        let line = line.trim();
        let mut parts = line.splitn(3, [' ', '\t']);
        if parts.next() != Some(HOST_KEY_ALGORITHM) {
            return Err(Error::refused("not an ssh-ed25519 public key"));
        }
        let encoded = parts
            .next()
            .ok_or_else(|| Error::refused("public key has no key data"))?;
        let comment = parts.next().unwrap_or("").trim().to_string();
        let blob =
            from_base64(encoded).ok_or_else(|| Error::refused("public key is not base64"))?;
        let key = parse_ed25519_public(&blob)
            .map_err(|_| Error::refused("public key data does not hold a plain ssh-ed25519 key"))?;
        Ok(Self { key, comment })
    }

    pub fn blob(&self) -> Vec<u8> {
        ed25519_public_blob(&self.key)
    }

    pub fn to_line(&self) -> String {
        let encoded = base64(&self.blob());
        if self.comment.is_empty() {
            format!("{HOST_KEY_ALGORITHM} {encoded}\n")
        } else {
            format!("{HOST_KEY_ALGORITHM} {encoded} {}\n", self.comment)
        }
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of_blob(&self.blob())
    }
}

/// An `openssh-key-v1` file as read, before any passphrase is known.
pub struct PrivateKeyFile {
    public: [u8; 32],
    cipher: String,
    salt: Vec<u8>,
    rounds: u32,
    secret_section: Vec<u8>,
}

impl Drop for PrivateKeyFile {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl PrivateKeyFile {
    /// In a file with no passphrase this section is the key in the clear.
    fn wipe(&mut self) {
        td_primitives::ct::zero(&mut self.secret_section);
    }

    /// `parse` of a file's bytes, which are wiped however it goes.
    pub fn parse_owned(bytes: Vec<u8>) -> Result<Self> {
        let bytes = Wiped(bytes);
        let text = std::str::from_utf8(&bytes.0)
            .map_err(|_| Error::refused("private key file is not text"))?;
        Self::parse(text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        if text.len() > MAX_PRIVATE_KEY_FILE {
            return Err(Error::refused("private key file is too large"));
        }
        let body = {
            let mut lines = text.lines().map(str::trim);
            if lines.next() != Some(BEGIN) {
                return Err(Error::refused(
                    "not an OpenSSH private key (openssh-key-v1)",
                ));
            }
            let mut body = Wiped(Vec::with_capacity(text.len()));
            let mut closed = false;
            for line in lines {
                if line == END {
                    closed = true;
                    break;
                }
                body.0.extend_from_slice(line.as_bytes());
            }
            if !closed {
                return Err(Error::refused("private key file has no end marker"));
            }
            body
        };
        let not_base64 = || Error::refused("private key is not base64");
        let decoded = Wiped(
            std::str::from_utf8(&body.0)
                .ok()
                .and_then(from_base64)
                .ok_or_else(not_base64)?,
        );
        let structure = decoded
            .0
            .strip_prefix(MAGIC)
            .ok_or_else(|| Error::refused("private key has the wrong magic"))?;
        let mut reader = Reader::new(structure);
        let bad = |_| Error::refused("private key structure is malformed");
        let cipher = reader.str().map_err(bad)?.to_string();
        let kdf = reader.str().map_err(bad)?.to_string();
        let kdf_options = reader.string().map_err(bad)?;
        let key_count = reader.u32().map_err(bad)?;
        if key_count != 1 {
            return Err(Error::refused("private key file holds more than one key"));
        }
        let public_blob = reader.string().map_err(bad)?;
        let secret_section = reader.string().map_err(bad)?.to_vec();
        let (salt, rounds) = match kdf.as_str() {
            KDF_NONE => {
                if !kdf_options.is_empty() {
                    return Err(Error::refused("private key has stray KDF options"));
                }
                (Vec::new(), 0)
            }
            KDF_BCRYPT => {
                let mut options = Reader::new(kdf_options);
                let salt = options.string().map_err(bad)?.to_vec();
                let rounds = options.u32().map_err(bad)?;
                options.finish().map_err(bad)?;
                if rounds == 0 || rounds > MAX_ROUNDS || salt.is_empty() {
                    return Err(Error::refused("private key has unusable KDF parameters"));
                }
                (salt, rounds)
            }
            other => {
                return Err(Error::refused(format!(
                    "unsupported private key KDF: {other}"
                )))
            }
        };
        match (cipher.as_str(), kdf.as_str()) {
            (CIPHER_NONE, KDF_NONE) | (CIPHER_AES, KDF_BCRYPT) => {}
            (other, _) => {
                return Err(Error::refused(format!(
                    "unsupported private key cipher: {other}"
                )))
            }
        }
        let public = parse_ed25519_public(public_blob)
            .map_err(|_| Error::refused("td-ssh only supports Ed25519 private keys"))?;
        Ok(Self {
            public,
            cipher,
            salt,
            rounds,
            secret_section,
        })
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher != CIPHER_NONE
    }

    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    /// The decrypted key. `None` passphrase on an encrypted file, or a wrong
    /// one, reports the same refusal.
    pub fn decrypt(&self, passphrase: Option<&[u8]>) -> Result<PrivateKey> {
        let mut section = Wiped(self.secret_section.clone());
        let block = if self.is_encrypted() {
            let passphrase = passphrase
                .filter(|p| !p.is_empty())
                .ok_or_else(wrong_passphrase)?;
            apply_key_cipher(passphrase, &self.salt, self.rounds, &mut section.0)?;
            AES_BLOCK
        } else {
            NONE_BLOCK
        };
        parse_secret_section(&section.0, block, &self.public)
    }
}

fn wrong_passphrase() -> Error {
    Error::refused("incorrect passphrase supplied to decrypt private key")
}

fn apply_key_cipher(pass: &[u8], salt: &[u8], rounds: u32, data: &mut [u8]) -> Result<()> {
    let mut derived = [0u8; AES_KEY_LEN + AES_IV_LEN];
    let mut keystream = || -> Result<()> {
        bcrypt_pbkdf(pass, salt, rounds, &mut derived).map_err(|_| wrong_passphrase())?;
        let (key, iv) = derived.split_at(AES_KEY_LEN);
        let key: &[u8; AES_KEY_LEN] = key.try_into().map_err(|_| wrong_passphrase())?;
        let iv: &[u8; AES_IV_LEN] = iv.try_into().map_err(|_| wrong_passphrase())?;
        Aes256Ctr::new(key, iv).apply(data);
        Ok(())
    };
    let result = keystream();
    td_primitives::ct::zero(&mut derived);
    result
}

fn parse_secret_section(section: &[u8], block: usize, public: &[u8; 32]) -> Result<PrivateKey> {
    if section.is_empty() || !section.len().is_multiple_of(block) {
        return Err(wrong_passphrase());
    }
    let mut reader = Reader::new(section);
    let check_a = reader.u32().map_err(|_| wrong_passphrase())?;
    let check_b = reader.u32().map_err(|_| wrong_passphrase())?;
    // The check ints are how a wrong passphrase shows; nothing else is read.
    if check_a != check_b {
        return Err(wrong_passphrase());
    }
    let bad = |_| Error::refused("private key contents are malformed");
    if reader.str().map_err(bad)? != HOST_KEY_ALGORITHM {
        return Err(Error::refused("td-ssh only supports Ed25519 private keys"));
    }
    let stated_public = reader.array::<32>().map_err(bad)?;
    // Borrowed from the section, which the caller wipes: no copy of the seed.
    let combined = reader.string().map_err(bad)?;
    let comment = String::from_utf8_lossy(reader.string().map_err(bad)?).into_owned();
    // Deterministic padding 1, 2, 3, ... is required: OpenSSH writes and checks it.
    for (index, byte) in reader.rest().iter().enumerate() {
        if usize::from(*byte) != index + 1 {
            return Err(Error::refused("private key padding is malformed"));
        }
    }
    if combined.len() != 64 {
        return Err(Error::refused("private key contents are malformed"));
    }
    let (seed, second_half) = combined.split_at(32);
    let seed: &[u8; 32] = seed.try_into().map_err(bad_length)?;
    let key = PrivateKey::from_seed(seed, &comment)?;
    if key.signing.public_key() != stated_public
        || stated_public != *public
        || second_half != stated_public.as_slice()
    {
        return Err(Error::refused("private key does not match its public half"));
    }
    Ok(key)
}

fn bad_length(_: std::array::TryFromSliceError) -> Error {
    Error::refused("private key contents are malformed")
}

/// The signing key cannot give its seed back, and the signer hand-off and key
/// writing need it, so the seed is kept beside it, wiped with it.
pub struct PrivateKey {
    signing: ed25519::SigningKey,
    seed: Secret32,
    pub comment: String,
}

impl PrivateKey {
    pub fn from_seed(seed: &[u8; 32], comment: &str) -> Result<Self> {
        Ok(Self {
            signing: ed25519::SigningKey::from_seed(seed)
                .map_err(|_| Error::refused("private key contents are malformed"))?,
            seed: Secret32(*seed),
            comment: comment.to_string(),
        })
    }

    /// The private seed, for the one hand-off to the signer stage. The caller
    /// wipes its copy.
    pub fn seed(&self) -> [u8; 32] {
        self.seed.0
    }

    pub fn public(&self) -> PublicKey {
        PublicKey {
            key: self.signing.public_key(),
            comment: self.comment.clone(),
        }
    }

    pub fn sign(&self, message: &[u8]) -> Result<[u8; 64]> {
        self.signing
            .sign(message)
            .map_err(|_| Error::refused("Ed25519 signing was refused"))
    }

    /// The part that holds the seed, written once into room for all of it, so that
    /// no earlier and shorter copy is left behind on the heap.
    fn secret_section(&self, public: &[u8; 32], check: u32) -> Writer {
        let mut secret = Writer::with_capacity(secret_section_size(self.comment.len(), NONE_BLOCK));
        secret.u32(check).u32(check);
        secret.str(HOST_KEY_ALGORITHM).string(public);
        let mut combined = [0u8; 64];
        let (head, tail) = combined.split_at_mut(32);
        head.copy_from_slice(&self.seed.0);
        tail.copy_from_slice(public);
        secret.string(&combined);
        td_primitives::ct::zero(&mut combined);
        secret.str(&self.comment);
        let mut pad = 1u8;
        while !secret.as_slice().len().is_multiple_of(NONE_BLOCK) {
            secret.u8(pad);
            pad = pad.wrapping_add(1);
        }
        secret
    }

    /// Writes `openssh-key-v1` in the clear, as every form of this text is the key.
    /// `check` comes from the caller's entropy.
    pub fn to_file(&self, check: u32) -> String {
        let public = self.signing.public_key();
        let section = Wiped(self.secret_section(&public, check).into_vec());

        let mut writer = Writer::with_capacity(section.0.len().saturating_add(OUTER_ROOM));
        writer.raw(MAGIC);
        writer.str(CIPHER_NONE).str(KDF_NONE).string(&[]);
        writer.u32(1).string(&ed25519_public_blob(&public));
        writer.string(&section.0);
        drop(section);

        let structure = Wiped(writer.into_vec());
        let encoded = Wiped(base64(&structure.0).into_bytes());
        let mut out = String::with_capacity(encoded.0.len() + encoded.0.len() / 70 + 128);
        out.push_str(BEGIN);
        out.push('\n');
        for line in encoded.0.chunks(70) {
            out.push_str(&String::from_utf8_lossy(line));
            out.push('\n');
        }
        out.push_str(END);
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// A real OpenSSH_10.3p1 `ssh-keygen -a 2 -Z aes256-ctr` file, comment `td-ssh fixture`
    /// and passphrase `hunter2`: a test fixture, not a real identity.
    const ENCRYPTED_FIXTURE: &str = include_str!("../tests/fixtures/protected_ed25519");

    /// Growing would leave a shorter copy of the seed on the heap, unwiped.
    #[test]
    fn the_seed_bearing_section_is_written_into_room_made_up_front() {
        for comment in [
            "",
            "c",
            "a rather longer comment than most people would write",
        ] {
            let key = PrivateKey::from_seed(&[5; 32], comment).unwrap();
            let public = key.signing.public_key();
            let section = key.secret_section(&public, 7);
            let size = secret_section_size(comment.len(), NONE_BLOCK);
            assert_eq!(section.capacity(), size, "grew past its room");
            assert!(section.as_slice().len() <= size);
        }
    }

    #[test]
    fn base64_matches_rfc4648_and_is_strict() {
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain), *encoded);
            assert_eq!(from_base64(encoded).unwrap(), *plain);
        }
        for bad in [
            "Z", "Zg=", "Z===", "Zh==", "Zm9=", "Zg==Zg==", "=g==", "Z!==", "Zm=v",
        ] {
            assert!(from_base64(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_parsed_key_file_wipes_its_secret_section() {
        let mut parsed =
            PrivateKeyFile::parse(&PrivateKey::from_seed(&[4; 32], "").unwrap().to_file(1))
                .unwrap();
        assert!(parsed.secret_section.iter().any(|byte| *byte != 0));
        parsed.wipe();
        assert!(parsed.secret_section.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn unencrypted_files_round_trip() {
        let key = PrivateKey::from_seed(&[5; 32], "alice@host").unwrap();
        let file = key.to_file(0x1234_5678);
        let parsed = PrivateKeyFile::parse(&file).unwrap();
        assert!(!parsed.is_encrypted());
        assert_eq!(parsed.public(), key.signing.public_key());
        let back = parsed.decrypt(None).unwrap();
        assert_eq!(back.comment, "alice@host");
        assert_eq!(back.seed(), [5; 32]);
    }

    #[test]
    fn a_stock_encrypted_file_decrypts_and_refuses_a_wrong_passphrase() {
        let parsed = PrivateKeyFile::parse(ENCRYPTED_FIXTURE).unwrap();
        assert!(parsed.is_encrypted());
        let key = parsed.decrypt(Some(b"hunter2")).unwrap();
        assert_eq!(parsed.public(), key.signing.public_key());
        assert_eq!(key.comment, "td-ssh fixture");
        for wrong in [Some(&b"wrong"[..]), Some(&b""[..]), None] {
            let error = parsed.decrypt(wrong).err().unwrap().to_string();
            assert!(error.contains("incorrect passphrase"), "{error}");
        }
    }

    #[test]
    fn a_mismatched_public_half_is_refused() {
        let key = PrivateKey::from_seed(&[7; 32], "").unwrap();
        let mut section = Writer::new();
        section.u32(1).u32(1).str(HOST_KEY_ALGORITHM);
        section.string(&[0u8; 32]);
        section.string(&[0u8; 64]);
        section.str("");
        while !section.as_slice().len().is_multiple_of(8) {
            section.u8(u8::try_from(section.as_slice().len() % 8).unwrap());
        }
        let result = parse_secret_section(section.as_slice(), 8, &key.signing.public_key());
        assert!(result.is_err());
    }

    #[test]
    fn malformed_key_files_are_refused_without_panicking() {
        assert!(PrivateKeyFile::parse("").is_err());
        assert!(PrivateKeyFile::parse("hello\n").is_err());
        assert!(PrivateKeyFile::parse(&format!("{BEGIN}\nZm9v\n")).is_err());
        assert!(PrivateKeyFile::parse(&format!("{BEGIN}\nZm9v\n{END}\n")).is_err());
        let good = PrivateKey::from_seed(&[8; 32], "c").unwrap().to_file(1);
        let body: String = good
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let decoded = from_base64(&body).unwrap();
        for cut in 0..decoded.len() {
            let text = format!("{BEGIN}\n{}\n{END}\n", base64(&decoded[..cut]));
            if let Ok(parsed) = PrivateKeyFile::parse(&text) {
                let _ = parsed.decrypt(None);
            }
        }
    }

    #[test]
    fn hostile_kdf_rounds_are_refused() {
        let mut options = Writer::new();
        options.string(&[1; 16]).u32(MAX_ROUNDS + 1);
        let mut writer = Writer::new();
        writer.raw(MAGIC);
        writer
            .str(CIPHER_AES)
            .str(KDF_BCRYPT)
            .string(options.as_slice());
        writer.u32(1).string(&ed25519_public_blob(&[1; 32]));
        writer.string(&[0; 16]);
        let text = format!("{BEGIN}\n{}\n{END}\n", base64(writer.as_slice()));
        let error = PrivateKeyFile::parse(&text).err().unwrap().to_string();
        assert!(error.contains("KDF"), "{error}");
    }

    #[test]
    fn public_lines_round_trip_and_refuse_other_key_types() {
        let public = PublicKey {
            key: [3; 32],
            comment: "a comment with spaces".into(),
        };
        let line = public.to_line();
        assert_eq!(PublicKey::parse_line(&line).unwrap(), public);
        assert!(public.fingerprint().starts_with("SHA256:"));
        assert!(!public.fingerprint().ends_with('='));
        assert!(PublicKey::parse_line("ssh-rsa AAAA x").is_err());
        assert!(PublicKey::parse_line("ssh-ed25519").is_err());
        assert!(PublicKey::parse_line("ssh-ed25519 !!!").is_err());
        assert!(PublicKey::parse_line("ssh-ed25519 Zm9v").is_err());
    }

    #[test]
    fn a_public_line_holding_a_key_of_small_order_is_refused() {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let line = PublicKey {
            key: identity,
            comment: String::new(),
        }
        .to_line();
        assert!(PublicKey::parse_line(&line).is_err());
    }
}
