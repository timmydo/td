//! CTAP assertion codec. The enrolled key and fresh challenge are caller authority.

use super::fido_cbor::{self as cbor, Encoder, Value};
use super::{crypto, fido_hid};

pub const RP_ID: &str = "td.invalid";
pub const MAX_CREDENTIAL_ID: usize = 1024;

/// What verifies an ES256 signature over a SHA-256 digest under a
/// public-only P-256 key `(x, y)`; td-secret's is its TPM. A verifier
/// grants nothing: the codec decides what a verified assertion means.
pub trait Es256Verifier {
    fn verify_es256(
        &mut self,
        x: &[u8; 32],
        y: &[u8; 32],
        digest: &[u8; 32],
        r: &[u8; 32],
        s: &[u8; 32],
    ) -> Result<(), String>;
}

/// A credential's public short name: the first four bytes of its SHA-256.
pub fn fingerprint(credential: &[u8]) -> [u8; 4] {
    let [a, b, c, d, ..] = crypto::digest(credential);
    [a, b, c, d]
}

#[derive(Clone, PartialEq, Eq)]
pub struct Es256PublicKey {
    x: [u8; 32],
    y: [u8; 32],
}

impl Es256PublicKey {
    /// The fixed public EC2/ES256/P-256 map, without unconsumed COSE metadata.
    pub fn canonical_cose(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(77);
        bytes.extend_from_slice(&[0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20]);
        bytes.extend_from_slice(&self.x);
        bytes.extend_from_slice(&[0x22, 0x58, 0x20]);
        bytes.extend_from_slice(&self.y);
        bytes
    }

    /// Shape validation only; the verifier checks curve membership during use.
    pub fn from_cose(bytes: &[u8]) -> Result<Self, String> {
        let value = cbor::decode(bytes)?;
        if value.required(&Value::Unsigned(1))? != &Value::Unsigned(2)
            || value.required(&Value::Unsigned(3))? != &Value::Negative(6)
            || value.required(&Value::Negative(0))? != &Value::Unsigned(1)
            || value.get(&Value::Negative(3))?.is_some()
        {
            return Err("credential is not a public-only ES256 P-256 key".into());
        }
        Ok(Self {
            x: value
                .required(&Value::Negative(1))?
                .bytes()?
                .try_into()
                .map_err(|_| "invalid P-256 x coordinate length")?,
            y: value
                .required(&Value::Negative(2))?
                .bytes()?
                .try_into()
                .map_err(|_| "invalid P-256 y coordinate length")?,
        })
    }
}

/// One request owns the allow-list identity and exact client-data hash it sent.
pub struct AssertionRequest {
    credential: Vec<u8>,
    client_data_hash: [u8; 32],
    bytes: Vec<u8>,
}

impl Drop for AssertionRequest {
    fn drop(&mut self) {
        self.credential.fill(0);
        self.client_data_hash.fill(0);
        self.bytes.fill(0);
    }
}

pub struct AssertionInfo {
    pub counter: u32,
    pub user_verified: bool,
    pub backup_eligible: bool,
    pub backed_up: bool,
}

impl AssertionRequest {
    /// max_message comes from getInfo, or the CTAP default of 1024 bytes.
    /// client_data_hash must bind fresh randomness and the trusted operation.
    pub fn new(
        credential: &[u8],
        client_data_hash: [u8; 32],
        max_message: usize,
    ) -> Result<Self, String> {
        if credential.is_empty() || credential.len() > MAX_CREDENTIAL_ID {
            return Err("invalid CTAP credential ID length".into());
        }
        let mut out = Encoder::new();
        out.head(5, 4)?;
        out.head(0, 1)?;
        out.text(RP_ID)?;
        out.head(0, 2)?;
        out.bytes(&client_data_hash)?;
        out.head(0, 3)?;
        out.head(4, 1)?;
        out.head(5, 2)?;
        out.text("id")?;
        out.bytes(credential)?;
        out.text("type")?;
        out.text("public-key")?;
        out.head(0, 5)?;
        out.head(5, 1)?;
        out.text("up")?;
        out.boolean(true)?; // Absent uv defaults false, including on tokens without UV.
        let mut encoded = out.finish()?;
        if encoded.len() >= max_message.min(fido_hid::MAX_MESSAGE) {
            encoded.fill(0);
            return Err("CTAP assertion request exceeds authenticator message limit".into());
        }
        let mut bytes = Vec::with_capacity(encoded.len() + 1);
        bytes.push(2); // authenticatorGetAssertion
        bytes.extend_from_slice(&encoded);
        encoded.fill(0);
        Ok(Self {
            credential: credential.to_vec(),
            client_data_hash,
            bytes,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the request on success or failure; never releases store material.
    pub fn verify<V: Es256Verifier>(
        self,
        response: &[u8],
        key: &Es256PublicKey,
        verifier: &mut V,
    ) -> Result<AssertionInfo, String> {
        let parsed = self.parse(response)?;
        verifier.verify_es256(&key.x, &key.y, &parsed.digest, &parsed.r, &parsed.s)?;
        Ok(parsed.info)
    }

    pub(super) fn parse(&self, response: &[u8]) -> Result<ParsedAssertion, String> {
        if response.len() > cbor::MAX_BYTES {
            return Err("CTAP response byte limit".into());
        }
        let (&status, bytes) = response
            .split_first()
            .ok_or("missing CTAP response status")?;
        if status != 0 {
            return Err(format!("CTAP assertion refused: {status:#04x}"));
        }
        let value = cbor::decode(bytes)?;
        if let Some(descriptor) = value.get(&Value::Unsigned(1))? {
            if descriptor.required(&Value::Text("type"))?.text()? != "public-key"
                || descriptor.required(&Value::Text("id"))?.bytes()? != self.credential
            {
                return Err("CTAP assertion credential does not match allow list".into());
            }
        }
        entity(&value, "CTAP assertion is not a single allow-list response")?;
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(..32) != Some(crypto::digest(RP_ID.as_bytes()).as_slice()) {
            return Err("CTAP assertion RP hash mismatch".into());
        }
        let flags = *data.get(32).ok_or("short authenticator flags")?;
        if flags & 1 == 0 || flags & 0x40 != 0 || flags & 0x18 == 0x10 {
            return Err("invalid assertion presence, attested-data or backup flags".into());
        }
        let counter = u32::from_be_bytes(
            data.get(33..37)
                .ok_or("short authenticator counter")?
                .try_into()
                .map_err(|_| "invalid authenticator counter")?,
        );
        extension_tail(flags, data)?;
        let (r, s) = signature(value.required(&Value::Unsigned(3))?.bytes()?)?;
        let mut signed = Vec::with_capacity(data.len() + 32);
        signed.extend_from_slice(data);
        signed.extend_from_slice(&self.client_data_hash);
        let digest = crypto::digest(&signed);
        signed.fill(0);
        Ok(ParsedAssertion {
            digest,
            r,
            s,
            info: AssertionInfo {
                counter,
                user_verified: flags & 4 != 0,
                backup_eligible: flags & 8 != 0,
                backed_up: flags & 16 != 0,
            },
        })
    }
}

pub(super) struct ParsedAssertion {
    pub(super) digest: [u8; 32],
    pub(super) r: [u8; 32],
    pub(super) s: [u8; 32],
    pub(super) info: AssertionInfo,
}

/// A silent selection over several allowed IDs. It authenticates nothing.
pub struct IdentifyRequest {
    credentials: Vec<Vec<u8>>,
    bytes: Vec<u8>,
}

impl IdentifyRequest {
    /// up=false, no PIN and no extension. The caller batches to the list limit.
    pub fn new(
        credentials: &[&[u8]],
        client_data_hash: [u8; 32],
        max_message: usize,
    ) -> Result<Self, String> {
        if credentials.is_empty() {
            return Err("empty CTAP identify allow list".into());
        }
        let mut owned: Vec<Vec<u8>> = Vec::with_capacity(credentials.len());
        for id in credentials {
            if id.is_empty() || id.len() > MAX_CREDENTIAL_ID {
                return Err("invalid CTAP credential ID length".into());
            }
            if owned.iter().any(|old| old.as_slice() == *id) {
                return Err("duplicate CTAP identify credential".into());
            }
            owned.push(id.to_vec());
        }
        let mut out = Encoder::new();
        out.head(5, 4)?;
        out.head(0, 1)?;
        out.text(RP_ID)?;
        out.head(0, 2)?;
        out.bytes(&client_data_hash)?;
        out.head(0, 3)?;
        out.head(4, owned.len() as u64)?;
        for id in &owned {
            out.head(5, 2)?;
            out.text("id")?;
            out.bytes(id)?;
            out.text("type")?;
            out.text("public-key")?;
        }
        out.head(0, 5)?;
        out.head(5, 1)?;
        out.text("up")?;
        out.boolean(false)?;
        let encoded = out.finish()?;
        if encoded.len() >= max_message.min(fido_hid::MAX_MESSAGE) {
            return Err("CTAP identify request exceeds authenticator message limit".into());
        }
        let mut bytes = Vec::with_capacity(encoded.len() + 1);
        bytes.push(2); // authenticatorGetAssertion
        bytes.extend_from_slice(&encoded);
        Ok(Self {
            credentials: owned,
            bytes,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The selected allow-list index, or None for CTAP2_ERR_NO_CREDENTIALS.
    pub fn select(&self, response: &[u8]) -> Result<Option<usize>, String> {
        if response.len() > cbor::MAX_BYTES {
            return Err("CTAP response byte limit".into());
        }
        let (&status, bytes) = response
            .split_first()
            .ok_or("missing CTAP response status")?;
        match status {
            0 => {}
            0x2e if bytes.is_empty() => return Ok(None),
            _ => return Err(format!("CTAP identify refused: {status:#04x}")),
        }
        let value = cbor::decode(bytes)?;
        let index = match value.get(&Value::Unsigned(1))? {
            Some(descriptor) => {
                if descriptor.required(&Value::Text("type"))?.text()? != "public-key" {
                    return Err("CTAP identify selected a non-public-key credential".into());
                }
                let id = descriptor.required(&Value::Text("id"))?.bytes()?;
                self.credentials
                    .iter()
                    .position(|candidate| candidate.as_slice() == id)
                    .ok_or("CTAP identify selected a credential outside its allow list")?
            }
            // CTAP permits omission only for a single-entry allow list.
            None if self.credentials.len() == 1 => 0,
            None => return Err("CTAP identify response omits its credential".into()),
        };
        entity(&value, "CTAP identify is not a single allow-list selection")?;
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(..32) != Some(crypto::digest(RP_ID.as_bytes()).as_slice()) {
            return Err("CTAP identify RP hash mismatch".into());
        }
        let flags = *data.get(32).ok_or("short authenticator flags")?;
        // A silent answer that claims presence or verification was not silent.
        if flags & 0x45 != 0 || flags & 0x18 == 0x10 {
            return Err("invalid silent identify flags".into());
        }
        data.get(33..37).ok_or("short authenticator counter")?;
        extension_tail(flags, data)?;
        value.required(&Value::Unsigned(3))?.bytes()?;
        Ok(Some(index))
    }
}

/// Optional user entity, credential count and userSelected of an allow-list answer.
fn entity(value: &Value<'_>, refusal: &str) -> Result<(), String> {
    if let Some(user) = value.get(&Value::Unsigned(4))? {
        let id = user.required(&Value::Text("id"))?.bytes()?;
        if id.is_empty() || id.len() > 64 {
            return Err("invalid CTAP user handle".into());
        }
        for field in ["name", "displayName"] {
            if let Some(field) = user.get(&Value::Text(field))? {
                field.text()?;
            }
        }
    }
    if value
        .get(&Value::Unsigned(5))?
        .is_some_and(|count| count != &Value::Unsigned(1))
        || value.get(&Value::Unsigned(6))?.is_some()
    {
        return Err(refusal.into());
    }
    Ok(())
}

/// Authenticator data after the counter: one text-keyed map exactly when ED is set.
fn extension_tail(flags: u8, data: &[u8]) -> Result<(), String> {
    let extensions = data.get(37..).ok_or("short authenticator data")?;
    if flags & 0x80 != 0 {
        let tail = cbor::decode(extensions)?;
        for (key, _) in tail.map()? {
            key.text()?;
        }
    } else if !extensions.is_empty() {
        return Err("unexpected trailing authenticator data".into());
    }
    Ok(())
}

pub(super) fn signature(bytes: &[u8]) -> Result<([u8; 32], [u8; 32]), String> {
    if !(8..=72).contains(&bytes.len())
        || bytes.first() != Some(&0x30)
        || bytes.get(1).copied().map(usize::from) != Some(bytes.len() - 2)
    {
        return Err("invalid ES256 DER sequence".into());
    }
    let mut rest = bytes.get(2..).ok_or("short ES256 sequence")?;
    let r = integer(&mut rest)?;
    let s = integer(&mut rest)?;
    if !rest.is_empty() {
        return Err("trailing ES256 signature data".into());
    }
    Ok((r, s))
}

fn integer(rest: &mut &[u8]) -> Result<[u8; 32], String> {
    if rest.first() != Some(&2) {
        return Err("missing ES256 DER integer".into());
    }
    let count = usize::from(*rest.get(1).ok_or("short ES256 integer length")?);
    if !(1..=33).contains(&count) {
        return Err("invalid ES256 integer length".into());
    }
    let mut value = rest.get(2..2 + count).ok_or("truncated ES256 integer")?;
    *rest = rest
        .get(2 + count..)
        .ok_or("truncated ES256 integer tail")?;
    let first = *value.first().ok_or("empty ES256 integer")?;
    if first & 0x80 != 0 {
        return Err("negative ES256 integer".into());
    }
    if first == 0 {
        if value.get(1).is_none_or(|next| next & 0x80 == 0) {
            return Err("zero or nonminimal ES256 integer".into());
        }
        value = value.get(1..).ok_or("short ES256 integer padding")?;
    }
    if value.len() > 32 {
        return Err("oversized ES256 integer".into());
    }
    let mut out = [0; 32];
    out.get_mut(32 - value.len()..)
        .ok_or("ES256 integer extent")?
        .copy_from_slice(value);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(data: &[u8], credential: Option<&[u8]>, count: Option<u64>) -> Vec<u8> {
        let mut out = Encoder::new();
        out.head(
            5,
            2 + u64::from(credential.is_some()) + u64::from(count.is_some()),
        )
        .unwrap();
        if let Some(id) = credential {
            out.head(0, 1).unwrap();
            out.head(5, 2).unwrap();
            out.text("id").unwrap();
            out.bytes(id).unwrap();
            out.text("type").unwrap();
            out.text("public-key").unwrap();
        }
        out.head(0, 2).unwrap();
        out.bytes(data).unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&[0x30, 6, 2, 1, 1, 2, 1, 2]).unwrap();
        if let Some(count) = count {
            out.head(0, 5).unwrap();
            out.head(0, count).unwrap();
        }
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }

    fn data() -> Vec<u8> {
        let mut bytes = crypto::digest(RP_ID.as_bytes()).to_vec();
        bytes.extend([1, 0, 0, 0, 7]);
        bytes
    }

    #[test]
    fn literal_request_binds_allow_list_presence_and_hash() {
        let request = AssertionRequest::new(&[7], [3; 32], 1024).unwrap();
        let mut expected = b"\x02\xa4\x01\x6atd.invalid\x02\x58\x20".to_vec();
        expected.extend([3; 32]);
        expected.extend(b"\x03\x81\xa2\x62id\x41\x07\x64type\x6apublic-key\x05\xa1\x62up\xf5");
        assert_eq!(request.bytes(), expected);
        assert!(AssertionRequest::new(&[], [0; 32], 1024).is_err());
        assert!(AssertionRequest::new(&[0; 1025], [0; 32], 7609).is_err());
        assert!(AssertionRequest::new(&[0; 1024], [0; 32], 1024).is_err());
        assert!(AssertionRequest::new(&[7], [3; 32], expected.len() - 1).is_err());
        assert!(AssertionRequest::new(&[7], [3; 32], expected.len()).is_ok());
    }

    #[test]
    fn literal_identify_request_is_silent_over_every_allowed_id() {
        let request = IdentifyRequest::new(&[&[7], &[8, 9]], [3; 32], 1024).unwrap();
        let mut expected = b"\x02\xa4\x01\x6atd.invalid\x02\x58\x20".to_vec();
        expected.extend([3; 32]);
        expected.extend(b"\x03\x82\xa2\x62id\x41\x07\x64type\x6apublic-key");
        expected.extend(b"\xa2\x62id\x42\x08\x09\x64type\x6apublic-key\x05\xa1\x62up\xf4");
        assert_eq!(request.bytes(), expected);
        assert!(IdentifyRequest::new(&[], [0; 32], 1024).is_err());
        assert!(IdentifyRequest::new(&[&[]], [0; 32], 1024).is_err());
        assert!(IdentifyRequest::new(&[&[7], &[7]], [0; 32], 1024).is_err());
        assert!(IdentifyRequest::new(&[&[0; 1025]], [0; 32], 7609).is_err());
        assert!(IdentifyRequest::new(&[&[7], &[8, 9]], [3; 32], expected.len()).is_ok());
        assert!(IdentifyRequest::new(&[&[7], &[8, 9]], [3; 32], expected.len() - 1).is_err());
    }

    #[test]
    fn identify_selects_by_index_and_authenticates_nothing() {
        let ids: [&[u8]; 3] = [&[7], &[8], &[9]];
        let batch = IdentifyRequest::new(&ids, [3; 32], 1024).unwrap();
        let single = IdentifyRequest::new(&ids[1..2], [3; 32], 1024).unwrap();
        let mut silent = data();
        silent[32] = 0;
        // The signature is never parsed: identify verifies nothing.
        for (id, index) in [(&[7][..], 0), (&[8], 1), (&[9], 2)] {
            let reply = response(&silent, Some(id), None);
            assert_eq!(batch.select(&reply).unwrap(), Some(index));
        }
        assert_eq!(
            single.select(&response(&silent, None, None)).unwrap(),
            Some(0)
        );
        assert_eq!(
            single
                .select(&response(&silent, Some(&[8]), Some(1)))
                .unwrap(),
            Some(0)
        );
        assert!(batch.select(&response(&silent, None, None)).is_err());
        assert!(batch.select(&response(&silent, Some(&[6]), None)).is_err());
        assert!(single.select(&response(&silent, Some(&[7]), None)).is_err());
        assert_eq!(
            batch.select(&response(&silent, Some(&[7]), Some(2))).err(),
            Some("CTAP identify is not a single allow-list selection".into())
        );
        assert_eq!(batch.select(&[0x2e]).unwrap(), None);
        for refused in [&[0x2e, 0][..], &[0x31], &[0x27], &[], &[0x01]] {
            assert!(batch.select(refused).is_err());
        }
        // UP, UV and AT must be clear; BS needs BE; BE alone and RFU bits are admitted.
        for (flags, admitted) in [
            (0x00, true),
            (0x08, true),
            (0x18, true),
            (0x22, true),
            (0x01, false),
            (0x04, false),
            (0x05, false),
            (0x40, false),
            (0x10, false),
        ] {
            let mut bytes = silent.clone();
            bytes[32] = flags;
            let result = batch.select(&response(&bytes, Some(&[8]), None));
            assert_eq!(result.is_ok(), admitted, "flags {flags:#04x}");
        }
        let mut wrong_rp = silent.clone();
        wrong_rp[0] ^= 1;
        assert!(batch
            .select(&response(&wrong_rp, Some(&[8]), None))
            .is_err());
        for length in 0..silent.len() {
            assert!(batch
                .select(&response(&silent[..length], Some(&[8]), None))
                .is_err());
        }
        let mut extended = silent.clone();
        extended[32] = 0x80;
        extended.extend(b"\xa1\x6bcredProtect\x01");
        assert_eq!(
            batch
                .select(&response(&extended, Some(&[8]), None))
                .unwrap(),
            Some(1)
        );
        extended[32] = 0;
        assert!(batch
            .select(&response(&extended, Some(&[8]), None))
            .is_err());
        let mut typed = response(&silent, Some(&[8]), None);
        let at = typed.windows(10).position(|s| s == b"public-key").unwrap();
        typed[at] ^= 1;
        assert!(batch.select(&typed).is_err());
    }

    #[test]
    fn response_checks_identity_flags_extensions_and_entire_signed_input() {
        let request = AssertionRequest::new(&[7], [3; 32], 1024).unwrap();
        let data = data();
        for descriptor in [None, Some(&[7][..])] {
            let parsed = request
                .parse(&response(&data, descriptor, Some(1)))
                .unwrap();
            let mut signed = data.clone();
            signed.extend([3; 32]);
            assert_eq!(parsed.digest, crypto::digest(&signed));
            assert_eq!(parsed.info.counter, 7);
        }
        assert!(request.parse(&response(&data, Some(&[8]), None)).is_err());
        assert!(request.parse(&response(&data, None, Some(2))).is_err());
        assert!(request.parse(&[0x2e]).is_err());
        for flags in [0, 2, 0x41, 0x11] {
            let mut bad = data.clone();
            bad[32] = flags;
            assert!(request.parse(&response(&bad, None, None)).is_err());
        }
        for flags in [3, 0x21, 0x23] {
            let mut future = data.clone();
            future[32] = flags;
            let parsed = request.parse(&response(&future, None, None)).unwrap();
            future.extend([3; 32]);
            assert_eq!(parsed.digest, crypto::digest(&future));
        }
        let mut bad_rp = data.clone();
        bad_rp[0] ^= 1;
        assert!(request.parse(&response(&bad_rp, None, None)).is_err());
        for length in 0..data.len() {
            assert!(request
                .parse(&response(&data[..length], None, None))
                .is_err());
        }
        let mut extended = data.clone();
        extended[32] |= 0x80;
        extended.extend(b"\xa1\x6bcredProtect\x01");
        let parsed = request.parse(&response(&extended, None, None)).unwrap();
        let mut signed = extended.clone();
        signed.extend([3; 32]);
        assert_eq!(parsed.digest, crypto::digest(&signed));
        extended[32] &= !0x80;
        assert!(request.parse(&response(&extended, None, None)).is_err());
        extended[32] |= 0x80;
        extended.push(0);
        assert!(request.parse(&response(&extended, None, None)).is_err());
    }

    #[test]
    fn cose_requires_public_es256_coordinates() {
        let mut out = Encoder::new();
        out.head(5, 5).unwrap();
        out.head(0, 1).unwrap();
        out.head(0, 2).unwrap();
        out.head(0, 3).unwrap();
        out.head(1, 6).unwrap();
        out.head(1, 0).unwrap();
        out.head(0, 1).unwrap();
        out.head(1, 1).unwrap();
        out.bytes(&[3; 32]).unwrap();
        out.head(1, 2).unwrap();
        out.bytes(&[4; 32]).unwrap();
        let bytes = out.finish().unwrap();
        let key = Es256PublicKey::from_cose(&bytes).unwrap();
        assert_eq!(key.x, [3; 32]);
        assert_eq!(key.y, [4; 32]);
        for index in [2, 4, 6] {
            let mut bad = bytes.clone();
            bad[index] ^= 1;
            assert!(Es256PublicKey::from_cose(&bad).is_err());
        }
        for size in 0..bytes.len() {
            assert!(Es256PublicKey::from_cose(&bytes[..size]).is_err());
        }
    }

    #[test]
    fn optional_user_schema_and_user_selected_are_checked() {
        let request = AssertionRequest::new(&[7], [3; 32], 1024).unwrap();
        let build = |id: &[u8], field: &str, text: bool, selected: bool| {
            let mut out = Encoder::new();
            out.head(5, 3 + u64::from(selected)).unwrap();
            out.head(0, 2).unwrap();
            out.bytes(&data()).unwrap();
            out.head(0, 3).unwrap();
            out.bytes(&[0x30, 6, 2, 1, 1, 2, 1, 2]).unwrap();
            out.head(0, 4).unwrap();
            out.head(5, 3).unwrap();
            out.text("id").unwrap();
            out.bytes(id).unwrap();
            out.text("icon").unwrap();
            out.head(0, 42).unwrap();
            out.text(field).unwrap();
            if text {
                out.text("local").unwrap();
            } else {
                out.head(0, 42).unwrap();
            }
            if selected {
                out.head(0, 6).unwrap();
                out.boolean(false).unwrap();
            }
            let mut bytes = vec![0];
            bytes.extend(out.finish().unwrap());
            bytes
        };
        for field in ["name", "displayName"] {
            for length in [0, 1, 64, 65] {
                let bytes = build(&vec![7; length], field, true, false);
                assert_eq!(request.parse(&bytes).is_ok(), (1..=64).contains(&length));
            }
            assert!(request.parse(&build(&[7], field, false, false)).is_err());
            assert!(request.parse(&build(&[7], field, true, true)).is_err());
        }
    }

    #[test]
    fn der_accepts_full_width_and_required_sign_padding_only() {
        let sequence = |r: &[u8], s: &[u8]| {
            let mut bytes = vec![0x30, (4 + r.len() + s.len()) as u8, 2, r.len() as u8];
            bytes.extend(r);
            bytes.extend([2, s.len() as u8]);
            bytes.extend(s);
            bytes
        };
        let ordinary = [0x7f; 32];
        let mut padded = vec![0];
        padded.extend([0x80; 32]);
        for r in [ordinary.as_slice(), padded.as_slice()] {
            for s in [ordinary.as_slice(), padded.as_slice()] {
                let parsed = signature(&sequence(r, s)).unwrap();
                assert_eq!(parsed.0, [*r.last().unwrap(); 32]);
                assert_eq!(parsed.1, [*s.last().unwrap(); 32]);
            }
        }
        assert!(signature(&sequence(&[1; 33], &[1])).is_err());
        let mut nonminimal = vec![0];
        nonminimal.extend(ordinary);
        assert!(signature(&sequence(&nonminimal, &[1])).is_err());
        assert!(signature(&sequence(&[0x80; 32], &[1])).is_err());
    }

    #[test]
    fn der_requires_two_positive_minimal_bounded_integers() {
        let good = [0x30, 6, 2, 1, 1, 2, 1, 2];
        let (r, s) = signature(&good).unwrap();
        assert_eq!((r[31], s[31]), (1, 2));
        for bytes in [
            vec![0x30, 6, 2, 1, 0, 2, 1, 2],
            vec![0x30, 6, 2, 1, 128, 2, 1, 2],
            vec![0x30, 7, 2, 2, 0, 1, 2, 1, 2],
            vec![0x30, 0x81, 6, 2, 1, 1, 2, 1, 2],
        ] {
            assert!(signature(&bytes).is_err());
        }
        for length in 0..good.len() {
            assert!(signature(&good[..length]).is_err());
        }
        let padded = [0x30, 7, 2, 2, 0, 128, 2, 1, 2];
        assert_eq!(signature(&padded).unwrap().0[31], 128);
    }
}
