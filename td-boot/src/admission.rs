//! The protected tier's admission record (td-install/ENCRYPTION.md
//! "Verified account handoff"): 46 bytes the installed selector writes after
//! a protected release once its PCR 11 measurement of the selected
//! deployment succeeded, td-kexec carries as the handoff archive's second
//! member (`protocol::ADMIT_MEMBER`), the deployment initramfs's `td-boot
//! admit` moves to `/run/td-admit/v1`, and td-authd consumes at a paired
//! generation's Prepare. td-boot and td-authd compile this one file; it
//! uses core alone and reads nothing.

/// `TDADMIT1`, the record's first eight bytes.
pub const MAGIC: &[u8; 8] = b"TDADMIT1";
pub const VERSION: u8 = 1;
/// Magic, version, UID, method and manifest ID.
pub const RECORD_BYTES: usize = 46;
/// The UID every td protector token names: the primary account, the one
/// principal td-login/TOKEN-LOGIN.md's tier has.
pub const PRIMARY_UID: u32 = 1000;
/// Under the system's `/run`: the root:root mode-0700 directory `admit`
/// creates, the record in it, and td-authd's consumed marker beside it.
pub const DIRECTORY: &str = "td-admit";
pub const RECORD: &str = "v1";
pub const CONSUMED: &str = "consumed";
/// The record admits only while the deployment kernel's `CLOCK_BOOTTIME`,
/// which starts at the kexec just after the selector wrote it, reads at
/// most this many seconds.
pub const MAX_AGE_SECONDS: u64 = 300;

/// Which release the record follows: a primary protector's admits one
/// session; a recovery release, and a release whose deployment was not
/// measured, admits none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// 0: a fido2-recovery or recovery-key release, an upgrade boot, or a
    /// boot without the PCR 11 measurement.
    None,
    /// 1: the TPM PIN.
    TpmPin,
    /// 2: a fido2-primary security key.
    Fido2Primary,
}

impl Method {
    pub fn byte(self) -> u8 {
        match self {
            Self::None => 0,
            Self::TpmPin => 1,
            Self::Fido2Primary => 2,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            1 => Some(Self::TpmPin),
            2 => Some(Self::Fido2Primary),
            _ => None,
        }
    }

    /// Whether a release by this method may admit a session.
    pub fn admits(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The record's fields. Integers are big-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub uid: u32,
    pub method: Method,
    /// The selected deployment's manifest ID, `sha256(manifest)`.
    pub manifest: [u8; 32],
}

impl Record {
    /// `TDADMIT1`, version 1, the UID, the method byte and the manifest ID.
    pub fn encode(&self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0u8; RECORD_BYTES];
        let (magic, rest) = bytes.split_at_mut(MAGIC.len());
        magic.copy_from_slice(MAGIC);
        let (version, rest) = rest.split_at_mut(1);
        version.fill(VERSION);
        let (uid, rest) = rest.split_at_mut(4);
        uid.copy_from_slice(&self.uid.to_be_bytes());
        let (method, manifest) = rest.split_at_mut(1);
        method.fill(self.method.byte());
        manifest.copy_from_slice(&self.manifest);
        bytes
    }

    /// Exactly `encode`'s shape: 46 bytes, the magic, version 1 and a
    /// known method. Anything else is refused, naming what.
    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let bytes: &[u8; RECORD_BYTES] = bytes
            .try_into()
            .map_err(|_| "the admission record is not 46 bytes")?;
        let (magic, rest) = bytes.split_at(MAGIC.len());
        if magic != MAGIC {
            return Err("the admission record's magic is not TDADMIT1");
        }
        let [version, a, b, c, d, method, manifest @ ..] = rest else {
            return Err("the admission record is not 46 bytes");
        };
        if *version != VERSION {
            return Err("the admission record's version is not 1");
        }
        let method =
            Method::from_byte(*method).ok_or("the admission record's method is not 0, 1 or 2")?;
        let manifest: [u8; 32] = manifest
            .try_into()
            .map_err(|_| "the admission record is not 46 bytes")?;
        Ok(Self {
            uid: u32::from_be_bytes([*a, *b, *c, *d]),
            method,
            manifest,
        })
    }
}

/// A deployment ID, 64 lowercase hex digits, as its 32 bytes.
pub fn manifest_id(hex: &[u8]) -> Option<[u8; 32]> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let (pairs, []) = hex.as_chunks::<2>() else {
        return None;
    };
    if pairs.len() != 32 {
        return None;
    }
    let mut id = [0u8; 32];
    for (byte, [high, low]) in id.iter_mut().zip(pairs) {
        *byte = (digit(*high)? << 4) | digit(*low)?;
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const ID: &str = "00112233445566778899aabbccddeeff0123456789abcdeffedcba9876543210";

    /// An independent literal: the spec's 46 bytes for UID 1000, the TPM
    /// PIN and `ID`.
    const LITERAL: [u8; 46] = [
        b'T', b'D', b'A', b'D', b'M', b'I', b'T', b'1', 0x01, 0x00, 0x00, 0x03, 0xe8, 0x01, 0x00,
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32,
        0x10,
    ];

    fn record(method: Method) -> Record {
        Record {
            uid: PRIMARY_UID,
            method,
            manifest: manifest_id(ID.as_bytes()).unwrap(),
        }
    }

    #[test]
    fn the_record_is_the_spec_literal() {
        assert_eq!(record(Method::TpmPin).encode(), LITERAL);
        assert_eq!(Record::decode(&LITERAL), Ok(record(Method::TpmPin)));
        assert_eq!(RECORD_BYTES, 46);
        assert_eq!(MAGIC, b"TDADMIT1");
        assert_eq!(PRIMARY_UID, 1000);
        assert_eq!(
            (DIRECTORY, RECORD, CONSUMED),
            ("td-admit", "v1", "consumed")
        );
        assert_eq!(MAX_AGE_SECONDS, 300);
    }

    #[test]
    fn each_method_has_its_byte_and_only_primaries_admit() {
        for (method, byte, admits) in [
            (Method::None, 0, false),
            (Method::TpmPin, 1, true),
            (Method::Fido2Primary, 2, true),
        ] {
            let bytes = record(method).encode();
            assert_eq!(bytes[13], byte);
            assert_eq!(Record::decode(&bytes).unwrap().method, method);
            assert_eq!(method.admits(), admits);
        }
    }

    #[test]
    fn anything_but_the_exact_shape_is_refused() {
        let good = LITERAL;
        for (index, value) in [(0, b'X'), (7, b'2'), (8, 0), (8, 2), (13, 3), (13, 0xff)] {
            let mut bad = good;
            bad[index] = value;
            assert!(Record::decode(&bad).is_err(), "{index} {value}");
        }
        assert!(Record::decode(&good[..45]).is_err());
        let mut long = good.to_vec();
        long.push(0);
        assert!(Record::decode(&long).is_err());
        assert!(Record::decode(&[]).is_err());
    }

    #[test]
    fn a_manifest_id_is_64_lowercase_hex() {
        assert_eq!(manifest_id(ID.as_bytes()).unwrap()[..2], [0x00, 0x11]);
        let upper = ID.to_uppercase();
        for bad in [
            &ID[..63],
            &upper,
            &format!("{ID}0"),
            "",
            &ID.replace('a', "g"),
        ] {
            assert_eq!(manifest_id(bad.as_bytes()), None, "{bad}");
        }
    }
}
