//! Canonical token/recovery metadata and assertion-before-unseal composition.

use super::fido_ctap::{AssertionRequest, Es256PublicKey, MAX_CREDENTIAL_ID, RP_ID};
use super::fido_enroll::{Credential, Info};
use super::{crypto, tpm};

const MAGIC: &[u8] = b"TDENROL1";
const DOMAIN: &[u8] = b"td-secret-enrollment-v1\0";
const KEY_BYTES: usize = 77;
const MAX_METADATA: usize = 8 + 4 + 1 + RP_ID.len() + 1 + 2 * (2 + MAX_CREDENTIAL_ID + KEY_BYTES);

pub enum Recovery<'a> {
    SecondToken(&'a Credential),
    Unrecoverable,
}

#[derive(Clone, Copy)]
pub enum Role {
    Primary,
    Recovery,
}

struct Token {
    id: Vec<u8>,
    key: Es256PublicKey,
}

impl Token {
    fn from_verified(credential: &Credential) -> Result<Self, String> {
        let token = Self {
            id: credential.id().to_vec(),
            key: Es256PublicKey::from_cose(credential.cose())?,
        };
        token.check()?;
        Ok(token)
    }

    fn check(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > MAX_CREDENTIAL_ID {
            return Err("invalid enrollment metadata credential ID".into());
        }
        Ok(())
    }

    fn encode(&self, bytes: &mut Vec<u8>) -> Result<(), String> {
        self.check()?;
        let size = u16::try_from(self.id.len()).map_err(|_| "credential ID length overflow")?;
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(&self.id);
        bytes.extend_from_slice(&self.key.canonical_cose());
        Ok(())
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.id.fill(0);
    }
}

/// Decoding validates structure, not authorization or disk integrity.
/// Its UID must be compared with the independently admitted human session.
pub struct Metadata {
    uid: u32,
    primary: Token,
    recovery: Option<Token>,
}

impl Metadata {
    /// UID is the independently admitted session, not a value inferred from a token.
    /// The caller must explicitly choose second-token recovery or unrecoverability.
    pub fn new(uid: u32, primary: &Credential, recovery: Recovery<'_>) -> Result<Self, String> {
        let value = Self {
            uid,
            primary: Token::from_verified(primary)?,
            recovery: match recovery {
                Recovery::SecondToken(token) => Some(Token::from_verified(token)?),
                Recovery::Unrecoverable => None,
            },
        };
        value.check()?;
        Ok(value)
    }

    fn check(&self) -> Result<(), String> {
        self.primary.check()?;
        if let Some(recovery) = &self.recovery {
            recovery.check()?;
            if recovery.id == self.primary.id || recovery.key == self.primary.key {
                return Err("recovery must have a distinct credential ID and public key".into());
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.check()?;
        let mut bytes = Vec::with_capacity(MAX_METADATA);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.uid.to_be_bytes());
        bytes.push(u8::try_from(RP_ID.len()).map_err(|_| "RP ID length overflow")?);
        bytes.extend_from_slice(RP_ID.as_bytes());
        bytes.push(u8::from(self.recovery.is_some()));
        self.primary.encode(&mut bytes)?;
        if let Some(recovery) = &self.recovery {
            recovery.encode(&mut bytes)?;
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8], expected_uid: u32) -> Result<Self, String> {
        if bytes.len() > MAX_METADATA {
            return Err("oversized enrollment metadata".into());
        }
        let mut reader = Reader(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err("invalid enrollment metadata version".into());
        }
        let uid = u32::from_be_bytes(
            reader
                .take(4)?
                .try_into()
                .map_err(|_| "short enrollment UID")?,
        );
        if uid != expected_uid {
            return Err("enrollment metadata belongs to another session".into());
        }
        let rp_len = usize::from(reader.byte()?);
        if reader.take(rp_len)? != RP_ID.as_bytes() {
            return Err("enrollment metadata RP mismatch".into());
        }
        let recovery = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err("invalid enrollment recovery policy".into()),
        };
        let value = Self {
            uid,
            primary: reader.token()?,
            recovery: if recovery {
                Some(reader.token()?)
            } else {
                None
            },
        };
        if !reader.0.is_empty() {
            return Err("trailing enrollment metadata".into());
        }
        value.check()?;
        Ok(value)
    }

    pub fn has_recovery(&self) -> bool {
        self.recovery.is_some()
    }

    pub fn binding(&self) -> Result<[u8; 32], String> {
        let mut bytes = DOMAIN.to_vec();
        let mut encoded = self.encode()?;
        bytes.extend_from_slice(&encoded);
        encoded.fill(0);
        let digest = crypto::digest(&bytes);
        bytes.fill(0);
        Ok(digest)
    }

    /// The caller must obtain a fresh challenge on the presented trusted operation.
    pub fn request(
        &self,
        role: Role,
        challenge: [u8; 32],
        info: &Info,
    ) -> Result<ReleaseRequest, String> {
        let token = match role {
            Role::Primary => &self.primary,
            Role::Recovery => self
                .recovery
                .as_ref()
                .ok_or("store is explicitly unrecoverable")?,
        };
        Ok(ReleaseRequest {
            request: info.assertion(&token.id, challenge)?,
            key: token.key.clone(),
            binding: self.binding()?,
            uid: self.uid,
        })
    }
}

pub struct ReleaseRequest {
    request: AssertionRequest,
    key: Es256PublicKey,
    binding: [u8; 32],
    uid: u32,
}

impl ReleaseRequest {
    pub fn bytes(&self) -> &[u8] {
        self.request.bytes()
    }

    /// No caller-supplied digest or key can replace those owned by this request.
    pub fn unseal<T: tpm::Transport>(
        self,
        response: &[u8],
        sealed: &tpm::BoundKey,
        mut client: tpm::Client<T>,
    ) -> Result<[u8; 32], String> {
        if sealed.uid() != self.uid {
            return Err("sealed store session mismatch".into());
        }
        let assertion = self.request.verify(response, &self.key, &mut client)?;
        if assertion.backup_eligible || assertion.backed_up {
            return Err("store release requires a device-bound token".into());
        }
        client.unseal_bound(sealed, &self.binding)
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8], String> {
        let bytes = self.0.get(..size).ok_or("truncated enrollment metadata")?;
        self.0 = self.0.get(size..).ok_or("truncated enrollment metadata")?;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8, String> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| "short metadata byte".into())
    }
    fn token(&mut self) -> Result<Token, String> {
        let size = usize::from(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| "short credential length")?,
        ));
        if size == 0 || size > MAX_CREDENTIAL_ID {
            return Err("invalid metadata credential length".into());
        }
        let id = self.take(size)?.to_vec();
        let encoded = self.take(KEY_BYTES)?;
        let key = Es256PublicKey::from_cose(encoded)?;
        if key.canonical_cose() != encoded {
            return Err("noncanonical enrollment public key".into());
        }
        Ok(Token { id, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fido_cbor::Encoder;
    use crate::fido_enroll::MakeCredential;

    const PRIMARY: &str = concat!(
        "a5010203262001215820",
        "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
        "225820552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553"
    );
    const SECOND: &str = concat!(
        "a5010203262001215820",
        "4ff5a0bbadb8ce30e0817af49576f71fb8951d81a1c04c669db218092c519a51",
        "225820e58a5635aa435a22ac2c0b6e3f50d70bc40910530c51fccbac6c198642f1a824"
    );
    const PRIMARY_SIGNATURE: &str = "3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7";
    const SECOND_SIGNATURE: &str = "30450220243295f44e901d44155d6069601fcca05846e935f266a0eda6ef12d8381b5fa7022100f2c5e7e60a2537c7450f470f16d5e4ed984ec3f27ab8ab5088922ebd749bc686";

    fn hex(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0, "hex fixture must contain whole bytes");
        s.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
            .collect()
    }
    fn challenge() -> [u8; 32] {
        hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca")
            .try_into()
            .unwrap()
    }
    fn info() -> Info {
        Info::parse(&hex(
            "00a20181684649444f5f325f30035000000000000000000000000000000000",
        ))
        .unwrap()
    }
    fn token(id: u8, key: &str) -> Token {
        Token {
            id: vec![id],
            key: Es256PublicKey::from_cose(&hex(key)).unwrap(),
        }
    }
    fn metadata(recovery: bool) -> Metadata {
        Metadata {
            uid: 1000,
            primary: token(7, PRIMARY),
            recovery: recovery.then(|| token(8, SECOND)),
        }
    }
    fn signed(signature: &str) -> Vec<u8> {
        signed_flags(signature, 0x81)
    }

    fn signed_flags(signature: &str, flags: u8) -> Vec<u8> {
        // Independently OpenSSL-signed authData || clientDataHash; public fixtures only.
        let mut auth = hex("34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401");
        auth[32] = flags;
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 2).unwrap();
        out.bytes(&auth).unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&hex(signature)).unwrap();
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }
    fn made(id: u8, key: &str) -> Vec<u8> {
        let mut auth = crypto::digest(RP_ID.as_bytes()).to_vec();
        auth.push(0x41);
        auth.extend([0; 20]);
        auth.extend([0, 1, id]);
        auth.extend(hex(key));
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 1).unwrap();
        out.text("none").unwrap();
        out.head(0, 2).unwrap();
        out.bytes(&auth).unwrap();
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }

    #[test]
    fn literal_format_and_independent_domain_hash_pin_the_complete_identity() {
        let record = metadata(false);
        let mut literal = hex("5444454e524f4c31000003e80a74642e696e76616c696400000107");
        literal.extend(hex(PRIMARY));
        assert_eq!(record.encode().unwrap(), literal);
        // Python hashlib.sha256 over the literal domain and record above.
        assert_eq!(
            record.binding().unwrap().as_slice(),
            hex("c1c54c5ab0a78fa206113b508fcb9aea2c9b93876f6d9b6b75b8ee58f42690fe")
        );
        assert_eq!(
            Metadata::decode(&literal, 1000).unwrap().encode().unwrap(),
            literal
        );
        assert!(record
            .request(Role::Recovery, challenge(), &info())
            .is_err());
        let paired = metadata(true);
        assert!(!record.has_recovery());
        assert!(paired.has_recovery());
        assert_ne!(paired.binding().unwrap(), record.binding().unwrap());
        for role in [Role::Primary, Role::Recovery] {
            let request = paired.request(role, challenge(), &info()).unwrap();
            assert_eq!(request.bytes()[0], 2);
        }
        let mut changed = metadata(true);
        changed.uid = 1001;
        assert_ne!(changed.binding().unwrap(), paired.binding().unwrap());
        changed.uid = 1000;
        changed.primary.id[0] ^= 1;
        assert_ne!(changed.binding().unwrap(), paired.binding().unwrap());
    }

    #[test]
    fn codec_rejects_truncation_ambiguous_recovery_and_noncanonical_fields() {
        for recovery in [false, true] {
            let wire = metadata(recovery).encode().unwrap();
            for len in 0..wire.len() {
                assert!(
                    Metadata::decode(&wire[..len], 1000).is_err(),
                    "{recovery}/{len}"
                );
            }
            for offset in [0, 11, 12, 13, 23, 24, 25, 27] {
                let mut changed = wire.clone();
                changed[offset] ^= 0xff;
                assert!(
                    Metadata::decode(&changed, 1000).is_err(),
                    "{recovery}/{offset}"
                );
            }
            let mut tail = wire.clone();
            tail.push(0);
            assert!(Metadata::decode(&tail, 1000).is_err());
            assert!(Metadata::decode(&wire, 1001).is_err());
        }
        let mut pair = metadata(true);
        pair.recovery.as_mut().unwrap().id = pair.primary.id.clone();
        assert!(pair.encode().is_err());
        pair.recovery = Some(token(8, PRIMARY));
        assert!(pair.encode().is_err());
        let mut duplicate = metadata(true).encode().unwrap();
        // The second record starts after the 24-byte header and 80-byte primary.
        duplicate[106] = 7;
        assert!(Metadata::decode(&duplicate, 1000).is_err());
        duplicate[106] = 8;
        duplicate[107..].copy_from_slice(&hex(PRIMARY));
        assert!(Metadata::decode(&duplicate, 1000).is_err());
        assert!(Metadata::decode(&vec![0; MAX_METADATA + 1], 1000).is_err());
        let mut max = metadata(true);
        max.primary.id = vec![7; MAX_CREDENTIAL_ID];
        max.recovery.as_mut().unwrap().id = vec![8; MAX_CREDENTIAL_ID];
        let bytes = max.encode().unwrap();
        assert_eq!(bytes.len(), MAX_METADATA);
        assert_eq!(
            Metadata::decode(&bytes, 1000).unwrap().encode().unwrap(),
            bytes
        );
        max.primary.id.push(7);
        assert!(max.encode().is_err());
    }

    #[test]
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_release_requires_signed_presence_and_the_sealed_enrollment() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!(
            "td-meta-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!root.exists());
        let _directory = Directory(root.clone());
        let emulator = tpm::tests::Emulator::start(&root);
        emulator.extend(&[7; 32]);
        let primary = MakeCredential::primary(info(), [1; 32], [2; 32])
            .unwrap()
            .proof(&made(7, PRIMARY), challenge())
            .unwrap()
            .verify(&signed(PRIMARY_SIGNATURE), &mut emulator.client())
            .unwrap();
        let recovery = MakeCredential::recovery(info(), [3; 32], [4; 32], &primary)
            .unwrap()
            .proof(&made(8, SECOND), challenge())
            .unwrap()
            .verify(&signed(SECOND_SIGNATURE), &mut emulator.client())
            .unwrap();
        assert!(Metadata::new(1000, &primary, Recovery::SecondToken(&primary)).is_err());
        let metadata = Metadata::new(1000, &primary, Recovery::SecondToken(&recovery)).unwrap();
        let key = emulator
            .client()
            .seal_bound(
                1000,
                tpm::Pcrs::parse("7").unwrap(),
                &[0x54; 32],
                &metadata.binding().unwrap(),
            )
            .unwrap();
        let release = |record: &Metadata, role, hash, response: &[u8], key: &tpm::BoundKey| {
            record
                .request(role, hash, &info())
                .unwrap()
                .unseal(response, key, emulator.client())
        };
        for (role, signature) in [
            (Role::Primary, PRIMARY_SIGNATURE),
            (Role::Recovery, SECOND_SIGNATURE),
        ] {
            assert_eq!(
                release(&metadata, role, challenge(), &signed(signature), &key).unwrap(),
                [0x54; 32]
            );
            let mut wrong = challenge();
            wrong[0] ^= 1;
            assert!(release(&metadata, role, wrong, &signed(signature), &key).is_err());
            let mut broken = signed(signature);
            *broken.last_mut().unwrap() ^= 1;
            assert!(release(&metadata, role, challenge(), &broken, &key).is_err());
        }
        assert!(release(
            &metadata,
            Role::Primary,
            challenge(),
            &signed(SECOND_SIGNATURE),
            &key
        )
        .is_err());
        // Independent signatures make these policy refusals, not tampering tests.
        for (flags, signature) in [
            (0x80, "304502200228eb83704476507ec20bd66a7b626fc691c1e92f23da10e94d7d51cd6f3cb1022100a136879aa59271061d264d48c2acd89cf8cff9bfb66027e84700b25827272b54"),
            (0x89, "3045022003f349527bcf052409fbf8babca5c3fcb2211227844fafa770941af537b4d1e20221009d566aa3b038604322a6ad01a6abed8bac7c6e45e0b8f482c8ef31c34fd205f3"),
            (0x99, "304402204d2b41c0d1cfcfaf198a44c7e6a2d022b4228bfffc449525f738e0bafcdddaa502200f9d01d6bec8380a01c80000d6c41e372f4ba552275ef4493822682b00136acb"),
        ] {
            assert!(release(&metadata, Role::Primary, challenge(),
                &signed_flags(signature, flags), &key).is_err());
        }
        let removed = Metadata::new(1000, &primary, Recovery::Unrecoverable).unwrap();
        assert!(release(
            &removed,
            Role::Primary,
            challenge(),
            &signed(PRIMARY_SIGNATURE),
            &key
        )
        .is_err());
        let mut wrapper = key.encode().unwrap();
        wrapper[8..40].copy_from_slice(&removed.binding().unwrap());
        let edited = tpm::BoundKey::decode(&wrapper).unwrap();
        assert!(release(
            &removed,
            Role::Primary,
            challenge(),
            &signed(PRIMARY_SIGNATURE),
            &edited
        )
        .is_err());
        let substituted = Metadata::new(1000, &recovery, Recovery::Unrecoverable).unwrap();
        wrapper[8..40].copy_from_slice(&substituted.binding().unwrap());
        let edited = tpm::BoundKey::decode(&wrapper).unwrap();
        assert!(release(
            &substituted,
            Role::Primary,
            challenge(),
            &signed(SECOND_SIGNATURE),
            &edited
        )
        .is_err());
        let other = Metadata::new(1001, &primary, Recovery::Unrecoverable).unwrap();
        assert!(release(
            &other,
            Role::Primary,
            challenge(),
            &signed(PRIMARY_SIGNATURE),
            &key
        )
        .is_err());
        // Refusals leave the authentic record usable.
        assert_eq!(
            release(
                &metadata,
                Role::Recovery,
                challenge(),
                &signed(SECOND_SIGNATURE),
                &key
            )
            .unwrap(),
            [0x54; 32]
        );
    }
}
