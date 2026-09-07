//! CTAP assertion codec. The enrolled key and fresh challenge are caller authority.

use super::fido_cbor::{self as cbor, Encoder, Value};
use super::{crypto, fido_hid, tpm};

pub const RP_ID: &str = "td.invalid";
pub const MAX_CREDENTIAL_ID: usize = 1024;

pub struct Es256PublicKey {
    x: [u8; 32],
    y: [u8; 32],
}

impl Es256PublicKey {
    /// Shape validation only; the TPM verifies curve membership during use.
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
    pub fn verify<T: tpm::Transport>(
        self,
        response: &[u8],
        key: &Es256PublicKey,
        tpm: &mut tpm::Client<T>,
    ) -> Result<AssertionInfo, String> {
        let parsed = self.parse(response)?;
        tpm.verify_es256(&key.x, &key.y, &parsed.digest, &parsed.r, &parsed.s)?;
        Ok(parsed.info)
    }

    fn parse(&self, response: &[u8]) -> Result<ParsedAssertion, String> {
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
            return Err("CTAP assertion is not a single allow-list response".into());
        }
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
        let extensions = data.get(37..).ok_or("short authenticator data")?;
        if flags & 0x80 != 0 {
            let tail = cbor::decode(extensions)?;
            for (key, _) in tail.map()? {
                key.text()?;
            }
        } else if !extensions.is_empty() {
            return Err("unexpected trailing authenticator data".into());
        }
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

struct ParsedAssertion {
    digest: [u8; 32],
    r: [u8; 32],
    s: [u8; 32],
    info: AssertionInfo,
}

fn signature(bytes: &[u8]) -> Result<([u8; 32], [u8; 32]), String> {
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
        expected.extend(
            b"\x03\x81\xa2\x62id\x41\x07\x64type\x6apublic-key\x05\xa1\x62up\xf5",
        );
        assert_eq!(request.bytes(), expected);
        assert!(AssertionRequest::new(&[], [0; 32], 1024).is_err());
        assert!(AssertionRequest::new(&[0; 1025], [0; 32], 7609).is_err());
        assert!(AssertionRequest::new(&[0; 1024], [0; 32], 1024).is_err());
        assert!(AssertionRequest::new(&[7], [3; 32], expected.len() - 1).is_err());
        assert!(AssertionRequest::new(&[7], [3; 32], expected.len()).is_ok());
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
    #[ignore = "requires explicitly supplied pinned host swtpm; never accesses hardware"]
    fn emulator_assertion_binds_challenge_extensions_and_signature() {
        fn hex(value: &str) -> Vec<u8> {
            value
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect()
        }
        // Independently signed using host OpenSSL 3.5.7; no private key or runtime dependency.
        let x = hex("ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a");
        let y = hex("552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553");
        let client = hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca");
        let auth = hex("34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401");
        let signature = hex("3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7");
        let key = Es256PublicKey {
            x: x.try_into().unwrap(),
            y: y.try_into().unwrap(),
        };
        let client: [u8; 32] = client.try_into().unwrap();
        let root = std::env::temp_dir().join(format!("td-ctap-oracle-{}", std::process::id()));
        assert!(!root.exists());
        let emulator = tpm::tests::Emulator::start(&root);
        let mut tpm = emulator.client();
        let response = |auth: &[u8], sig: &[u8]| {
            let mut out = Encoder::new();
            out.head(5, 2).unwrap();
            out.head(0, 2).unwrap();
            out.bytes(auth).unwrap();
            out.head(0, 3).unwrap();
            out.bytes(sig).unwrap();
            let mut bytes = vec![0];
            bytes.extend(out.finish().unwrap());
            bytes
        };
        let good = response(&auth, &signature);
        let info = AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&good, &key, &mut tpm)
            .unwrap();
        assert_eq!(info.counter, 7);
        let mut changed_client = client;
        changed_client[0] ^= 1;
        assert!(AssertionRequest::new(&[7], changed_client, 1024)
            .unwrap()
            .verify(&good, &key, &mut tpm)
            .is_err());
        let mut changed_extension = auth.clone();
        *changed_extension.last_mut().unwrap() = 2;
        let changed = response(&changed_extension, &signature);
        assert!(AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&changed, &key, &mut tpm)
            .is_err());
        let mut changed_signature = signature;
        *changed_signature.last_mut().unwrap() ^= 1;
        let changed = response(&auth, &changed_signature);
        assert!(AssertionRequest::new(&[7], client, 1024)
            .unwrap()
            .verify(&changed, &key, &mut tpm)
            .is_err());
        drop(tpm);
        drop(emulator);
        std::fs::remove_dir_all(root).unwrap();
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
