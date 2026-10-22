//! Bounded verification of one detached OpenPGP v4 RSA signature.
//!
//! This is the subset a Flathub OSTree commit signature uses and nothing more:
//! one signature packet of class 0x00 (binary document), RSA, SHA-256 or
//! SHA-512, over bytes the caller supplies, checked against an RSA key the
//! caller names. There is no keyring, armor, compression, literal data,
//! revocation or web of trust. Which key to trust is the caller's reviewed
//! decision; this module only says whether that key made this signature.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

use ring::digest::{digest, SHA1_FOR_LEGACY_USE_ONLY};
use ring::signature::{
    RsaParameters, RsaPublicKeyComponents, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA512,
};

const SIGNATURE_TAG: u8 = 2;
const VERSION_4: u8 = 4;
const BINARY_DOCUMENT: u8 = 0x00;
const RSA_ENCRYPT_OR_SIGN: u8 = 1;
const RSA_SIGN_ONLY: u8 = 3;
const SHA256: u8 = 8;
const SHA512: u8 = 10;
const CREATION_TIME: u8 = 2;
const EXPIRATION_TIME: u8 = 3;
const ISSUER_KEY_ID: u8 = 16;
const ISSUER_FINGERPRINT: u8 = 33;
const CRITICAL: u8 = 0x80;
const MAX_PACKET_BYTES: usize = 16 * 1024;
const MAX_RSA_BYTES: usize = 1024;
const MIN_RSA_BITS: usize = 2048;
/// A signature dated further ahead of the verifier's clock than this is refused.
const MAX_CLOCK_SKEW_SECS: u64 = 24 * 60 * 60;

/// An RSA public key or subkey, identified by its v4 fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RsaKey {
    fingerprint: [u8; 20],
    created: u32,
    n: Vec<u8>,
    e: Vec<u8>,
}

impl RsaKey {
    /// Parse the body of a v4 public-key or public-subkey packet.
    pub(crate) fn from_packet_body(body: &[u8]) -> Result<Self, String> {
        if body.len() > MAX_PACKET_BYTES {
            return Err("OpenPGP key packet is too large".into());
        }
        let mut reader = Reader::new(body);
        if reader.byte("key version")? != VERSION_4 {
            return Err("OpenPGP key is not version 4".into());
        }
        let created = reader.u32("key creation time")?;
        let algorithm = reader.byte("key algorithm")?;
        if algorithm != RSA_ENCRYPT_OR_SIGN && algorithm != RSA_SIGN_ONLY {
            return Err(format!("OpenPGP key algorithm {algorithm} is not RSA"));
        }
        let (n, bits) = reader.mpi_with_bits("RSA modulus")?;
        let n = n.to_vec();
        let e = reader.mpi("RSA exponent")?.to_vec();
        reader.finish("key packet")?;
        if bits < MIN_RSA_BITS {
            return Err(format!(
                "RSA modulus has {bits} bits, fewer than {MIN_RSA_BITS}"
            ));
        }
        let length = u16::try_from(body.len()).map_err(|_| "key packet is too large")?;
        let mut framed = Vec::with_capacity(body.len() + 3);
        framed.push(0x99);
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(body);
        let fingerprint = <[u8; 20]>::try_from(digest(&SHA1_FOR_LEGACY_USE_ONLY, &framed).as_ref())
            .map_err(|_| "SHA-1 fingerprint is not 20 bytes".to_string())?;
        Ok(Self {
            fingerprint,
            created,
            n,
            e,
        })
    }

    pub(crate) fn fingerprint_hex(&self) -> String {
        hex_upper(&self.fingerprint)
    }
}

/// What a verified signature asserted beyond "this key signed these bytes".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) created: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    hash: u8,
    created: u32,
    expires_after: Option<u32>,
    issuer_fingerprint: Option<[u8; 20]>,
    issuer_key_id: Option<[u8; 8]>,
    /// Version through the end of the hashed subpackets: the signed trailer.
    hashed: Vec<u8>,
    value: Vec<u8>,
}

/// Accept the first of `packets` that verifies, or report why each did not.
pub(crate) fn verify_any(
    key: &RsaKey,
    packets: &[Vec<u8>],
    data: &[u8],
    now: u64,
) -> Result<Verified, Vec<String>> {
    let mut refusals = Vec::with_capacity(packets.len());
    for packet in packets {
        match verify(key, packet, data, now) {
            Ok(verified) => return Ok(verified),
            Err(error) => refusals.push(error),
        }
    }
    Err(refusals)
}

/// Verify one binary signature packet by `key` over `data` at time `now`.
pub(crate) fn verify(
    key: &RsaKey,
    packet: &[u8],
    data: &[u8],
    now: u64,
) -> Result<Verified, String> {
    let signature = parse_signature(packet)?;
    if let Some(issuer) = signature.issuer_fingerprint {
        if issuer != key.fingerprint {
            return Err(format!(
                "signature names issuer {}, not key {}",
                hex_upper(&issuer),
                key.fingerprint_hex()
            ));
        }
    }
    if let Some(id) = signature.issuer_key_id {
        if key.fingerprint.get(12..) != Some(id.as_slice()) {
            return Err(format!(
                "signature names issuer key id {}, not key {}",
                hex_upper(&id),
                key.fingerprint_hex()
            ));
        }
    }
    if signature.created < key.created {
        return Err("signature predates its key".into());
    }
    if u64::from(signature.created) > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
        return Err("signature is dated in the future".into());
    }
    if let Some(after) = signature.expires_after {
        if after != 0 && u64::from(signature.created) + u64::from(after) <= now {
            return Err("signature has expired".into());
        }
    }
    let length = u32::try_from(signature.hashed.len()).map_err(|_| "hashed area is too large")?;
    let mut message = Vec::with_capacity(data.len() + signature.hashed.len() + 6);
    message.extend_from_slice(data);
    message.extend_from_slice(&signature.hashed);
    message.extend_from_slice(&[VERSION_4, 0xff]);
    message.extend_from_slice(&length.to_be_bytes());
    let parameters: &RsaParameters = match signature.hash {
        SHA256 => &RSA_PKCS1_2048_8192_SHA256,
        SHA512 => &RSA_PKCS1_2048_8192_SHA512,
        other => return Err(format!("signature hash algorithm {other} is refused")),
    };
    if signature.value.len() > key.n.len() {
        return Err("RSA signature is longer than the key's modulus".into());
    }
    // An MPI drops leading zero octets; PKCS #1 compares at the modulus width.
    let mut value = vec![0u8; key.n.len() - signature.value.len()];
    value.extend_from_slice(&signature.value);
    RsaPublicKeyComponents {
        n: key.n.as_slice(),
        e: key.e.as_slice(),
    }
    .verify(parameters, &message, &value)
    .map_err(|_| {
        format!(
            "RSA signature does not verify under key {}",
            key.fingerprint_hex()
        )
    })?;
    Ok(Verified {
        created: signature.created,
    })
}

fn parse_signature(packet: &[u8]) -> Result<Signature, String> {
    let (tag, body) = one_packet(packet)?;
    if tag != SIGNATURE_TAG {
        return Err(format!("OpenPGP packet tag {tag} is not a signature"));
    }
    let mut reader = Reader::new(body);
    if reader.byte("signature version")? != VERSION_4 {
        return Err("OpenPGP signature is not version 4".into());
    }
    let class = reader.byte("signature class")?;
    if class != BINARY_DOCUMENT {
        return Err(format!(
            "OpenPGP signature class {class:#04x} is not a binary document"
        ));
    }
    let algorithm = reader.byte("signature algorithm")?;
    if algorithm != RSA_ENCRYPT_OR_SIGN && algorithm != RSA_SIGN_ONLY {
        return Err(format!(
            "OpenPGP signature algorithm {algorithm} is not RSA"
        ));
    }
    let hash = reader.byte("signature hash algorithm")?;
    if hash != SHA256 && hash != SHA512 {
        return Err(format!("signature hash algorithm {hash} is refused"));
    }
    let hashed_length = usize::from(reader.u16("hashed subpacket length")?);
    let hashed_area = reader.take(hashed_length, "hashed subpackets")?;
    let hashed = body
        .get(..reader.position)
        .ok_or("signature hashed trailer is outside its packet")?
        .to_vec();
    let unhashed_length = usize::from(reader.u16("unhashed subpacket length")?);
    let unhashed_area = reader.take(unhashed_length, "unhashed subpackets")?;
    reader.take(2, "signed hash prefix")?;
    let value = reader.mpi("RSA signature")?.to_vec();
    reader.finish("signature packet")?;

    let mut signature = Signature {
        hash,
        created: 0,
        expires_after: None,
        issuer_fingerprint: None,
        issuer_key_id: None,
        hashed,
        value,
    };
    let mut created = None;
    for (kind, data) in subpackets(hashed_area)? {
        let critical = kind & CRITICAL != 0;
        match kind & !CRITICAL {
            CREATION_TIME => {
                if created.replace(be_u32(data, "creation time")?).is_some() {
                    return Err("signature repeats its creation time".into());
                }
            }
            EXPIRATION_TIME => {
                if signature
                    .expires_after
                    .replace(be_u32(data, "expiration time")?)
                    .is_some()
                {
                    return Err("signature repeats its expiration time".into());
                }
            }
            ISSUER_FINGERPRINT => {
                if signature
                    .issuer_fingerprint
                    .replace(issuer_fingerprint(data)?)
                    .is_some()
                {
                    return Err("signature repeats its issuer fingerprint".into());
                }
            }
            ISSUER_KEY_ID => {
                if signature
                    .issuer_key_id
                    .replace(issuer_key_id(data)?)
                    .is_some()
                {
                    return Err("signature repeats its issuer key id".into());
                }
            }
            other if critical => {
                return Err(format!("signature has unknown critical subpacket {other}"));
            }
            _ => {}
        }
    }
    // Unhashed issuer hints are unauthenticated; they may only fill a gap.
    // Anything else there, critical or not, is ignored: an attacker can write
    // this area, so honouring it could only refuse a good signature.
    for (kind, data) in subpackets(unhashed_area)? {
        match kind & !CRITICAL {
            ISSUER_FINGERPRINT if signature.issuer_fingerprint.is_none() => {
                signature.issuer_fingerprint = Some(issuer_fingerprint(data)?);
            }
            ISSUER_KEY_ID if signature.issuer_key_id.is_none() => {
                signature.issuer_key_id = Some(issuer_key_id(data)?);
            }
            _ => {}
        }
    }
    signature.created = created.ok_or("signature has no hashed creation time")?;
    Ok(signature)
}

fn issuer_fingerprint(data: &[u8]) -> Result<[u8; 20], String> {
    match data.split_first() {
        Some((&VERSION_4, rest)) => rest
            .try_into()
            .map_err(|_| "v4 issuer fingerprint is not 20 bytes".to_string()),
        _ => Err("issuer fingerprint is not a v4 fingerprint".into()),
    }
}

fn issuer_key_id(data: &[u8]) -> Result<[u8; 8], String> {
    data.try_into()
        .map_err(|_| "issuer key id is not 8 bytes".to_string())
}

fn be_u32(data: &[u8], what: &str) -> Result<u32, String> {
    let raw: [u8; 4] = data
        .try_into()
        .map_err(|_| format!("signature {what} is not 4 bytes"))?;
    Ok(u32::from_be_bytes(raw))
}

/// Split one subpacket area into `(type, data)` pairs.
fn subpackets(area: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut reader = Reader::new(area);
    let mut out = Vec::new();
    while !reader.is_empty() {
        let first = reader.byte("subpacket length")?;
        let length = match first {
            0..=191 => usize::from(first),
            192..=254 => {
                let second = reader.byte("subpacket length")?;
                ((usize::from(first) - 192) << 8) + usize::from(second) + 192
            }
            255 => usize::try_from(reader.u32("subpacket length")?)
                .map_err(|_| "subpacket length does not fit")?,
        };
        if length == 0 {
            return Err("signature subpacket has no type".into());
        }
        let body = reader.take(length, "subpacket")?;
        let (&kind, data) = body
            .split_first()
            .ok_or("signature subpacket has no type")?;
        out.push((kind, data));
    }
    Ok(out)
}

/// Read exactly one packet with a definite length; return its tag and body.
fn one_packet(bytes: &[u8]) -> Result<(u8, &[u8]), String> {
    if bytes.len() > MAX_PACKET_BYTES {
        return Err("OpenPGP packet is too large".into());
    }
    let mut reader = Reader::new(bytes);
    let header = reader.byte("packet header")?;
    if header & 0x80 == 0 {
        return Err("OpenPGP packet header lacks its marker bit".into());
    }
    let (tag, length) = if header & 0x40 != 0 {
        let tag = header & 0x3f;
        let first = reader.byte("packet length")?;
        let length = match first {
            0..=191 => usize::from(first),
            192..=223 => {
                let second = reader.byte("packet length")?;
                ((usize::from(first) - 192) << 8) + usize::from(second) + 192
            }
            255 => usize::try_from(reader.u32("packet length")?)
                .map_err(|_| "packet length does not fit")?,
            _ => return Err("OpenPGP partial body lengths are refused".into()),
        };
        (tag, length)
    } else {
        let tag = (header >> 2) & 0x0f;
        let length = match header & 0x03 {
            0 => usize::from(reader.byte("packet length")?),
            1 => usize::from(reader.u16("packet length")?),
            2 => usize::try_from(reader.u32("packet length")?)
                .map_err(|_| "packet length does not fit")?,
            _ => return Err("OpenPGP indeterminate packet lengths are refused".into()),
        };
        (tag, length)
    };
    let body = reader.take(length, "packet body")?;
    reader.finish("packet")?;
    Ok((tag, body))
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn take(&mut self, length: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| format!("OpenPGP {what} length overflows"))?;
        let out = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| format!("OpenPGP {what} is truncated"))?;
        self.position = end;
        Ok(out)
    }

    fn byte(&mut self, what: &str) -> Result<u8, String> {
        self.take(1, what)?
            .first()
            .copied()
            .ok_or_else(|| format!("OpenPGP {what} is truncated"))
    }

    fn u16(&mut self, what: &str) -> Result<u16, String> {
        let raw: [u8; 2] = self
            .take(2, what)?
            .try_into()
            .map_err(|_| format!("OpenPGP {what} is truncated"))?;
        Ok(u16::from_be_bytes(raw))
    }

    fn u32(&mut self, what: &str) -> Result<u32, String> {
        let raw: [u8; 4] = self
            .take(4, what)?
            .try_into()
            .map_err(|_| format!("OpenPGP {what} is truncated"))?;
        Ok(u32::from_be_bytes(raw))
    }

    /// A canonical multiprecision integer: no leading zero octet or bit.
    /// GnuPG writes signatures this way; a zero-padded value is refused.
    fn mpi(&mut self, what: &str) -> Result<&'a [u8], String> {
        self.mpi_with_bits(what).map(|(value, _)| value)
    }

    fn mpi_with_bits(&mut self, what: &str) -> Result<(&'a [u8], usize), String> {
        let bits = usize::from(self.u16(what)?);
        let length = bits.div_ceil(8);
        if length == 0 || length > MAX_RSA_BYTES {
            return Err(format!("OpenPGP {what} has {bits} bits"));
        }
        let value = self.take(length, what)?;
        let first = value.first().copied().unwrap_or(0);
        let top_bits = 8 - first.leading_zeros() as usize;
        if first == 0 || (length - 1) * 8 + top_bits != bits {
            return Err(format!("OpenPGP {what} is not canonically encoded"));
        }
        Ok((value, bits))
    }

    fn finish(&self, what: &str) -> Result<(), String> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(format!("OpenPGP {what} has trailing bytes"))
        }
    }
}

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use td_engine::ostree::parse_commitmeta_signatures;

    const COMMIT: &str =
        include_str!("../../engine/tests/fixtures/flathub-firefox-7985364b.commit.hex");
    const COMMITMETA: &str =
        include_str!("../../engine/tests/fixtures/flathub-firefox-7985364b.commitmeta.hex");
    const SIGNED_AT: u32 = 1_792_503_025;

    fn hex(text: &str) -> Vec<u8> {
        let digits: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        crate::sig::from_hex(&digits).unwrap()
    }

    fn real() -> (RsaKey, Vec<u8>, Vec<u8>) {
        let key = crate::flathub::signing_key().unwrap();
        let signature = parse_commitmeta_signatures(&hex(COMMITMETA))
            .unwrap()
            .remove(0);
        (key, signature, hex(COMMIT))
    }

    /// A v4 signature packet with a five-octet new-format length.
    fn packet(class: u8, hash: u8, hashed: &[u8], unhashed: &[u8], value: &[u8]) -> Vec<u8> {
        let mut body = vec![VERSION_4, class, RSA_ENCRYPT_OR_SIGN, hash];
        body.extend_from_slice(&u16::try_from(hashed.len()).unwrap().to_be_bytes());
        body.extend_from_slice(hashed);
        body.extend_from_slice(&u16::try_from(unhashed.len()).unwrap().to_be_bytes());
        body.extend_from_slice(unhashed);
        body.extend_from_slice(&[0, 0]);
        let bits = u16::try_from(value.len() * 8 - value[0].leading_zeros() as usize).unwrap();
        body.extend_from_slice(&bits.to_be_bytes());
        body.extend_from_slice(value);
        let mut out = vec![0xc2, 0xff];
        out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
        out.extend(body);
        out
    }

    fn subpacket(kind: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![u8::try_from(data.len() + 1).unwrap(), kind];
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn real_flathub_signature_verifies_and_reports_its_time() {
        let (key, signature, commit) = real();
        let verified = verify(&key, &signature, &commit, u64::from(SIGNED_AT) + 60).unwrap();
        assert_eq!(verified.created, SIGNED_AT);
        // The same key, framed as a whole old-format subkey packet.
        let body = hex(crate::flathub::tests::SUBKEY_BODY);
        let mut framed = vec![0xb9];
        framed.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
        framed.extend(body);
        let (tag, body) = one_packet(&framed).unwrap();
        assert_eq!(tag, 14);
        assert_eq!(RsaKey::from_packet_body(body).unwrap(), key);
    }

    #[test]
    fn any_signed_byte_changed_fails_verification() {
        let (key, signature, commit) = real();
        let now = u64::from(SIGNED_AT) + 60;
        let mut data = commit.clone();
        data[100] ^= 1;
        assert!(verify(&key, &signature, &data, now)
            .unwrap_err()
            .contains("does not verify"));
        let mut extended = commit.clone();
        extended.push(0);
        assert!(verify(&key, &signature, &extended, now).is_err());
        // Byte 13 is inside the hashed area: header 3, fixed fields 4, length 2.
        let mut hashed = signature.clone();
        hashed[13] ^= 1;
        assert!(verify(&key, &hashed, &commit, now).is_err());
        let mut value = signature.clone();
        let last = value.len() - 1;
        value[last] ^= 1;
        assert!(verify(&key, &value, &commit, now)
            .unwrap_err()
            .contains("does not verify"));
    }

    #[test]
    fn signature_time_is_held_to_the_clock_and_the_key() {
        let (key, signature, commit) = real();
        let earliest = u64::from(SIGNED_AT) - MAX_CLOCK_SKEW_SECS;
        assert!(verify(&key, &signature, &commit, earliest).is_ok());
        assert!(verify(&key, &signature, &commit, earliest - 1)
            .unwrap_err()
            .contains("future"));
        let mut younger = key.clone();
        younger.created = SIGNED_AT + 1;
        assert!(verify(&younger, &signature, &commit, u64::MAX)
            .unwrap_err()
            .contains("predates"));
        let hashed = [
            subpacket(CREATION_TIME, &1_600_000_000u32.to_be_bytes()),
            subpacket(EXPIRATION_TIME, &10u32.to_be_bytes()),
        ]
        .concat();
        let expiring = packet(BINARY_DOCUMENT, SHA512, &hashed, &[], &[1]);
        assert!(verify(&key, &expiring, &commit, 1_600_000_010)
            .unwrap_err()
            .contains("expired"));
        assert!(verify(&key, &expiring, &commit, 1_600_000_009)
            .unwrap_err()
            .contains("does not verify"));
    }

    #[test]
    fn issuer_hints_must_name_the_key() {
        let (key, _, commit) = real();
        let created = subpacket(CREATION_TIME, &SIGNED_AT.to_be_bytes());
        let mut other = vec![VERSION_4];
        other.extend_from_slice(&[0x11; 20]);
        let hashed = [created.clone(), subpacket(ISSUER_FINGERPRINT, &other)].concat();
        assert!(verify(
            &key,
            &packet(0, SHA512, &hashed, &[], &[1]),
            &commit,
            u64::MAX
        )
        .unwrap_err()
        .contains("names issuer"));
        let unhashed = subpacket(ISSUER_KEY_ID, &[0x22; 8]);
        assert!(verify(
            &key,
            &packet(0, SHA512, &created, &unhashed, &[1]),
            &commit,
            u64::MAX
        )
        .unwrap_err()
        .contains("issuer key id"));
    }

    #[test]
    fn only_the_reviewed_signature_shape_parses() {
        let (_, signature, _) = real();
        let created = subpacket(CREATION_TIME, &SIGNED_AT.to_be_bytes());
        assert!(parse_signature(&signature).is_ok());
        assert!(parse_signature(&packet(0, SHA512, &created, &[], &[1])).is_ok());
        assert!(parse_signature(&packet(0, SHA256, &created, &[], &[1])).is_ok());

        let refused = |bytes: Vec<u8>, text: &str| {
            let error = parse_signature(&bytes).unwrap_err();
            assert!(error.contains(text), "{error} lacks {text}");
        };
        refused(packet(1, SHA512, &created, &[], &[1]), "binary document");
        refused(packet(0, 2, &created, &[], &[1]), "hash algorithm 2");
        refused(packet(0, SHA512, &[], &[], &[1]), "no hashed creation time");
        refused(
            packet(
                0,
                SHA512,
                &[created.clone(), created.clone()].concat(),
                &[],
                &[1],
            ),
            "repeats",
        );
        refused(
            packet(
                0,
                SHA512,
                &[created.clone(), subpacket(0x80 | 99, &[])].concat(),
                &[],
                &[1],
            ),
            "unknown critical subpacket 99",
        );
        // Unknown non-critical subpackets are skipped, as are unhashed ones.
        assert!(parse_signature(&packet(
            0,
            SHA512,
            &[created.clone(), subpacket(99, &[1, 2])].concat(),
            &subpacket(0x80 | 99, &[]),
            &[1]
        ))
        .is_ok());
        refused(packet(0, SHA512, &[2], &[], &[1]), "truncated");
        refused(packet(0, SHA512, &[0], &[], &[1]), "no type");

        let mut algorithm = signature.clone();
        algorithm[5] = 17;
        refused(algorithm, "not RSA");
        let mut version = signature.clone();
        version[3] = 3;
        refused(version, "not version 4");
        let mut trailing = signature.clone();
        trailing.push(0);
        refused(trailing, "trailing bytes");
        let mut truncated = signature.clone();
        truncated.pop();
        refused(truncated, "truncated");
        let mut tag = signature.clone();
        tag[0] = 0x99;
        refused(tag, "not a signature");
        let mut indeterminate = signature.clone();
        indeterminate[0] = 0x8b;
        refused(indeterminate, "indeterminate");
        refused(vec![0xc2, 0xe0], "partial");
        refused(vec![0x42, 0x00], "marker bit");
        refused(vec![0; MAX_PACKET_BYTES + 1], "too large");
        // A canonical MPI has no leading zero octet and an exact bit count.
        let mut zero = packet(0, SHA512, &created, &[], &[1, 1]);
        let at = zero.len() - 2;
        zero[at] = 0;
        refused(zero, "canonically");
        let mut bits = packet(0, SHA512, &created, &[], &[1]);
        let at = bits.len() - 2;
        bits[at] = 2;
        refused(bits, "canonically");
    }

    #[test]
    fn sha256_signature_by_another_rsa_key_verifies() {
        // A throwaway RSA-2048 key; GnuPG made the signature with SHA-256.
        let key = RsaKey::from_packet_body(&hex(include_str!(
            "../tests/fixtures/openpgp-rsa2048-sha256.key.hex"
        )))
        .unwrap();
        assert_eq!(
            key.fingerprint_hex(),
            "020A7631333E08DF444BAB06C8AD08B84E5264D4"
        );
        let signature = hex(include_str!(
            "../tests/fixtures/openpgp-rsa2048-sha256.sig.hex"
        ));
        assert_eq!(parse_signature(&signature).unwrap().hash, SHA256);
        let data = b"td openpgp sha256 fixture\n";
        let verified = verify(&key, &signature, data, u64::MAX / 2).unwrap();
        assert_eq!(verified.created, 1_792_652_452);
        // Flathub's key did not make it, and this key made no Flathub signature.
        let (flathub, flathub_signature, commit) = real();
        assert!(verify(&flathub, &signature, data, u64::MAX / 2)
            .unwrap_err()
            .contains("names issuer"));
        assert!(verify(&key, &flathub_signature, &commit, u64::MAX / 2)
            .unwrap_err()
            .contains("names issuer"));
    }

    #[test]
    fn the_first_signature_that_verifies_is_accepted() {
        let (key, good, commit) = real();
        let now = u64::from(SIGNED_AT) + 60;
        let created = subpacket(CREATION_TIME, &SIGNED_AT.to_be_bytes());
        let bad = packet(0, SHA512, &created, &[], &[1]);
        assert_eq!(
            verify_any(&key, &[bad.clone(), good.clone()], &commit, now)
                .unwrap()
                .created,
            SIGNED_AT
        );
        let refusals = verify_any(&key, &[bad.clone(), bad], &commit, now).unwrap_err();
        assert_eq!(refusals.len(), 2);
        assert!(verify_any(&key, &[], &commit, now).unwrap_err().is_empty());
    }

    #[test]
    fn a_value_wider_than_the_modulus_and_foreign_key_ids_are_refused() {
        let (key, _, commit) = real();
        let created = subpacket(CREATION_TIME, &SIGNED_AT.to_be_bytes());
        let wide = packet(0, SHA512, &created, &[], &vec![1; key.n.len() + 1]);
        assert!(verify(&key, &wide, &commit, u64::MAX)
            .unwrap_err()
            .contains("longer than"));
        let hashed = [created.clone(), subpacket(ISSUER_KEY_ID, &[0x33; 8])].concat();
        assert!(verify(
            &key,
            &packet(0, SHA512, &hashed, &[], &[1]),
            &commit,
            u64::MAX
        )
        .unwrap_err()
        .contains("issuer key id"));
        let mut other = vec![VERSION_4];
        other.extend_from_slice(&[0x44; 20]);
        let unhashed = subpacket(ISSUER_FINGERPRINT, &other);
        assert!(verify(
            &key,
            &packet(0, SHA512, &created, &unhashed, &[1]),
            &commit,
            u64::MAX
        )
        .unwrap_err()
        .contains("names issuer"));
    }

    #[test]
    fn long_subpacket_lengths_and_repeated_issuers() {
        let created = subpacket(CREATION_TIME, &SIGNED_AT.to_be_bytes());
        // Two-octet length: 200 + 1 type byte = 201 = (192 - 192) << 8 + 9 + 192.
        let mut two = vec![192, 9, 99];
        two.extend_from_slice(&[0; 200]);
        // Five-octet length.
        let five = vec![255, 0, 0, 0, 3, 99, 0, 0];
        let hashed = [created.clone(), two, five].concat();
        assert!(parse_signature(&packet(0, SHA512, &hashed, &[], &[1])).is_ok());
        let issuer = subpacket(ISSUER_KEY_ID, &[0x55; 8]);
        let repeated = [created.clone(), issuer.clone(), issuer].concat();
        assert!(parse_signature(&packet(0, SHA512, &repeated, &[], &[1]))
            .unwrap_err()
            .contains("repeats its issuer key id"));
        let mut fingerprint = vec![VERSION_4];
        fingerprint.extend_from_slice(&[0x66; 20]);
        let fingerprint = subpacket(ISSUER_FINGERPRINT, &fingerprint);
        let repeated = [created, fingerprint.clone(), fingerprint].concat();
        assert!(parse_signature(&packet(0, SHA512, &repeated, &[], &[1]))
            .unwrap_err()
            .contains("repeats its issuer fingerprint"));
    }

    #[test]
    fn only_rsa_v4_keys_of_at_least_2048_bits_parse() {
        let body = hex(crate::flathub::tests::SUBKEY_BODY);
        let mut version = body.clone();
        version[0] = 3;
        assert!(RsaKey::from_packet_body(&version)
            .unwrap_err()
            .contains("version 4"));
        let mut algorithm = body.clone();
        algorithm[5] = 22;
        assert!(RsaKey::from_packet_body(&algorithm)
            .unwrap_err()
            .contains("not RSA"));
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(RsaKey::from_packet_body(&trailing)
            .unwrap_err()
            .contains("trailing"));
        let small = [&body[..6], &[0x00, 0x09, 0x01, 0x00, 0x00, 0x02, 0x03][..]].concat();
        assert!(RsaKey::from_packet_body(&small)
            .unwrap_err()
            .contains("2048"));
        // 256 octets but 2047 bits: the floor is on bits, not octets.
        let mut narrow = body[..6].to_vec();
        narrow.extend_from_slice(&2047u16.to_be_bytes());
        narrow.push(0x7f);
        narrow.extend_from_slice(&[0xff; 255]);
        narrow.extend_from_slice(&[0x00, 0x11, 0x01, 0x00, 0x01]);
        assert!(RsaKey::from_packet_body(&narrow)
            .unwrap_err()
            .contains("2047 bits"));
        narrow[6..8].copy_from_slice(&2048u16.to_be_bytes());
        narrow[8] = 0x80;
        assert!(RsaKey::from_packet_body(&narrow).is_ok());
    }
}
