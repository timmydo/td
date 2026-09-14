//! Private, single-use PIN-authorized hmac-secret assertion flow. No device I/O.

use super::fido_cbor::{self as cbor, Encoder, Value};
use super::fido_ctap::{AssertionInfo, AssertionRequest, MAX_CREDENTIAL_ID, RP_ID};
use super::fido_p256::{PublicKey, SecretScalar};
use super::{crypto, fido_aes};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    One,
    Two,
}

impl Protocol {
    fn number(self) -> u64 {
        match self {
            Self::One => 1,
            Self::Two => 2,
        }
    }
    fn iv_bytes(self) -> usize {
        match self {
            Self::One => 0,
            Self::Two => 16,
        }
    }
    fn authenticate(self, key: &[u8], message: &[u8]) -> Secret {
        let mut mac = crypto::hmac(key, message);
        let size = match self {
            Self::One => 16,
            Self::Two => 32,
        };
        let mut out = Secret::zeroed(size);
        for (dst, src) in out.0.iter_mut().zip(&mac) {
            *dst = *src;
        }
        clear(&mut mac);
        out
    }
}

fn clear(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

struct Secret(Box<[u8]>);
impl Secret {
    fn zeroed(size: usize) -> Self {
        Self(vec![0; size].into_boxed_slice())
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        clear(&mut self.0);
    }
}

/// Initial profile accepts printable ASCII (already NFC), never rewrites a PIN.
pub(super) struct Pin(Secret);
impl Pin {
    pub(super) fn new(bytes: Box<[u8]>) -> Result<Self, String> {
        let pin = Self(Secret(bytes));
        if !(4..=63).contains(&pin.0 .0.len())
            || !pin.0 .0.iter().all(|byte| (0x20..=0x7e).contains(byte))
        {
            return Err("portable PIN must be 4 through 63 printable ASCII bytes".into());
        }
        Ok(pin)
    }
}

pub(super) struct Profile {
    protocol: Protocol,
    permissions: bool,
    max_message: usize,
    max_id: usize,
}
impl Profile {
    /// Unsigned capability claims select a protocol, never credential identity.
    pub(super) fn parse(response: &[u8]) -> Result<Self, String> {
        let info = response_value(response, cbor::MAX_BYTES)?;
        let versions = array(info.required(&Value::Unsigned(1))?)?;
        let mut supported = false;
        for version in versions {
            supported |= matches!(version.text()?, "FIDO_2_0" | "FIDO_2_1" | "FIDO_2_2");
        }
        if !supported {
            return Err("unsupported portable CTAP version".into());
        }
        if info.required(&Value::Unsigned(3))?.bytes()?.len() != 16 {
            return Err("invalid getInfo AAGUID length".into());
        }
        let mut hmac_secret = false;
        for extension in array(info.required(&Value::Unsigned(2))?)? {
            hmac_secret |= extension.text()? == "hmac-secret";
        }
        let options = info.required(&Value::Unsigned(4))?;
        // Validate even unknown option values, while ignoring their semantics.
        for (key, value) in options.map()? {
            key.text()?;
            boolean(value)?;
        }
        if !hmac_secret || !option(options, "clientPin", false)? {
            return Err("portable vault requires hmac-secret and a configured PIN".into());
        }
        if option(options, "plat", false)?
            || !option(options, "up", true)?
            || option(options, "noMcGaPermissionsWithClientPin", false)?
        {
            return Err("token cannot provide removable PIN-authorized presence".into());
        }
        if let Some(force) = info.get(&Value::Unsigned(12))? {
            if boolean(force)? {
                return Err("token requires an explicit PIN change".into());
            }
        }
        let protocols = array(info.required(&Value::Unsigned(6))?)?;
        let mut seen = Vec::with_capacity(protocols.len());
        for value in protocols {
            let number = value.unsigned()?;
            if seen.contains(&number) {
                return Err("duplicate PIN protocol".into());
            }
            seen.push(number);
        }
        let protocol = if seen.contains(&2) {
            Protocol::Two
        } else if seen.contains(&1) {
            Protocol::One
        } else {
            return Err("no supported PIN protocol".into());
        };
        limit(&info, 7, 1, 1)?;
        Ok(Self {
            protocol,
            permissions: option(options, "pinUvAuthToken", false)?,
            max_message: limit(&info, 5, 1024, cbor::MAX_BYTES)?,
            max_id: limit(&info, 8, MAX_CREDENTIAL_ID, MAX_CREDENTIAL_ID)?,
        })
    }

    /// The trusted backend supplies the enrolled key and fresh operation hash.
    pub(super) fn assertion(
        self,
        credential: &[u8],
        key: PublicKey,
        challenge: [u8; 32],
        salt: [u8; 32],
    ) -> Result<KeyRequest, String> {
        if credential.is_empty() || credential.len() > self.max_id {
            return Err("credential ID exceeds portable token profile".into());
        }
        let intent = Intent {
            verifier: AssertionRequest::new(credential, challenge, self.max_message)?,
            credential: Secret(credential.into()),
            key,
            challenge,
            salt,
        };
        let out = client_pin(self.protocol, 2, 2)?;
        let bytes = command(6, out, self.max_message)?;
        Ok(KeyRequest {
            profile: self,
            intent,
            bytes,
        })
    }
}

struct Intent {
    verifier: AssertionRequest,
    credential: Secret,
    key: PublicKey,
    challenge: [u8; 32],
    salt: [u8; 32],
}
impl Drop for Intent {
    fn drop(&mut self) {
        clear(&mut self.challenge);
        clear(&mut self.salt);
    }
}

pub(super) struct KeyRequest {
    profile: Profile,
    intent: Intent,
    bytes: Secret,
}
impl KeyRequest {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    /// Each transition consumes its state, including on refusal. No retry policy.
    pub(super) fn with_pin(
        self,
        response: &[u8],
        pin: Pin,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<PinRequest, String> {
        let value = response_value(response, self.profile.max_message)?;
        let peer = agreement_key(value.required(&Value::Unsigned(1))?)?;
        let private = fresh_scalar(entropy)?;
        let public = private.public_key()?;
        let shared = private.agree(&peer)?;
        let keys = Keys::derive(self.profile.protocol, shared.bytes());
        drop(shared);
        drop(private);
        let mut hash = crypto::digest(&pin.0 .0);
        drop(pin);
        let encrypted = keys.encrypt(hash.get(..16).ok_or("PIN hash extent")?, entropy);
        clear(&mut hash);
        let encrypted = encrypted?;
        let mut out = client_pin(
            self.profile.protocol,
            if self.profile.permissions { 9 } else { 5 },
            if self.profile.permissions { 6 } else { 4 },
        )?;
        out.head(0, 3)?;
        encode_key(&mut out, &public)?;
        out.head(0, 6)?;
        out.bytes(&encrypted.0)?;
        if self.profile.permissions {
            out.head(0, 9)?;
            out.head(0, 2)?; // Only getAssertion, bound to our fixed RP.
            out.head(0, 10)?;
            out.text(RP_ID)?;
        }
        let bytes = command(6, out, self.profile.max_message)?;
        Ok(PinRequest {
            profile: self.profile,
            intent: self.intent,
            keys,
            public,
            bytes,
        })
    }
}

pub(super) struct PinRequest {
    profile: Profile,
    intent: Intent,
    keys: Keys,
    public: PublicKey,
    bytes: Secret,
}
impl PinRequest {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    pub(super) fn finish(
        self,
        response: &[u8],
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacRequest, String> {
        let value = response_value(response, self.profile.max_message)?;
        let encrypted = value.required(&Value::Unsigned(2))?.bytes()?;
        let length = encrypted
            .len()
            .checked_sub(self.profile.protocol.iv_bytes())
            .ok_or("short encrypted PIN token")?;
        if length != 32 && !(self.profile.protocol == Protocol::One && length == 16) {
            return Err("invalid PIN token length for selected protocol".into());
        }
        let token = self.keys.decrypt(encrypted, length)?;
        let auth = self
            .profile
            .protocol
            .authenticate(&token.0, &self.intent.challenge);
        drop(token);
        let salt_enc = self.keys.encrypt(&self.intent.salt, entropy)?;
        let salt_auth = self
            .profile
            .protocol
            .authenticate(&self.keys.hmac.0, &salt_enc.0);
        let mut out = Encoder::new();
        out.head(5, 7)?;
        out.head(0, 1)?;
        out.text(RP_ID)?;
        out.head(0, 2)?;
        out.bytes(&self.intent.challenge)?;
        out.head(0, 3)?;
        out.head(4, 1)?;
        out.head(5, 2)?;
        out.text("id")?;
        out.bytes(&self.intent.credential.0)?;
        out.text("type")?;
        out.text("public-key")?;
        out.head(0, 4)?;
        out.head(5, 1)?;
        out.text("hmac-secret")?;
        out.head(
            5,
            if self.profile.protocol == Protocol::One {
                3
            } else {
                4
            },
        )?;
        out.head(0, 1)?;
        encode_key(&mut out, &self.public)?;
        out.head(0, 2)?;
        out.bytes(&salt_enc.0)?;
        out.head(0, 3)?;
        out.bytes(&salt_auth.0)?;
        if self.profile.protocol == Protocol::Two {
            out.head(0, 4)?;
            out.head(0, 2)?;
        }
        out.head(0, 5)?;
        out.head(5, 1)?;
        out.text("up")?;
        out.boolean(true)?;
        // ClientPIN authorization provides UV; built-in uv is deliberately absent.
        out.head(0, 6)?;
        out.bytes(&auth.0)?;
        out.head(0, 7)?;
        out.head(0, self.profile.protocol.number())?;
        let bytes = command(2, out, self.profile.max_message)?;
        Ok(HmacRequest {
            intent: self.intent,
            keys: self.keys,
            max_message: self.profile.max_message,
            bytes,
        })
    }
}

pub(super) struct HmacRequest {
    intent: Intent,
    keys: Keys,
    max_message: usize,
    bytes: Secret,
}
/// Only produced after the enrolled credential signs UP, UV and extension bytes.
pub(super) struct HmacOutput {
    secret: Secret,
    pub(super) info: AssertionInfo,
}
impl HmacOutput {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.secret.0
    }
}
impl HmacRequest {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    pub(super) fn finish(self, response: &[u8]) -> Result<HmacOutput, String> {
        let value = response_value(response, self.max_message)?;
        let parsed = self.intent.verifier.parse(response)?;
        if !parsed.info.user_verified {
            return Err("portable assertion requires UV".into());
        }
        if self
            .intent
            .key
            .verify(&parsed.digest, &parsed.r, &parsed.s)
            .is_err()
        {
            return Err("portable assertion signature mismatch".into());
        }
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(32).is_none_or(|flags| flags & 0x80 == 0) {
            return Err("missing signed hmac-secret extension".into());
        }
        let extensions = cbor::decode(data.get(37..).ok_or("short authenticator data")?)?;
        let encrypted = extensions.required(&Value::Text("hmac-secret"))?.bytes()?;
        let secret = self.keys.decrypt(encrypted, 32)?;
        Ok(HmacOutput {
            secret,
            info: parsed.info,
        })
    }
}

struct Keys {
    protocol: Protocol,
    aes: Secret,
    hmac: Secret,
}
impl Keys {
    fn derive(protocol: Protocol, shared: &[u8; 32]) -> Self {
        let mut keys = Self {
            protocol,
            aes: Secret::zeroed(32),
            hmac: Secret::zeroed(32),
        };
        match protocol {
            Protocol::One => {
                let mut hash = crypto::digest(shared);
                keys.aes.0.copy_from_slice(&hash);
                keys.hmac.0.copy_from_slice(&hash);
                clear(&mut hash);
            }
            Protocol::Two => {
                let mut aes = crypto::hkdf(shared, &[0; 32], b"CTAP2 AES key");
                let mut hmac = crypto::hkdf(shared, &[0; 32], b"CTAP2 HMAC key");
                keys.aes.0.copy_from_slice(&aes);
                keys.hmac.0.copy_from_slice(&hmac);
                clear(&mut aes);
                clear(&mut hmac);
            }
        }
        keys
    }
    fn encrypt(
        &self,
        plaintext: &[u8],
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<Secret, String> {
        if !matches!(plaintext.len(), 16 | 32) {
            return Err("invalid PIN plaintext size".into());
        }
        let prefix = self.protocol.iv_bytes();
        let mut out = Secret::zeroed(prefix + plaintext.len());
        let (iv, bytes) = out.0.split_at_mut(prefix);
        if prefix != 0 {
            entropy(iv)?;
        }
        bytes.copy_from_slice(plaintext);
        let iv: &[u8; 16] = if prefix == 0 {
            &[0; 16]
        } else {
            (&*iv).try_into().map_err(|_| "IV extent")?
        };
        fido_aes::encrypt(
            self.aes
                .0
                .as_ref()
                .try_into()
                .map_err(|_| "AES key extent")?,
            iv,
            bytes,
        )?;
        Ok(out)
    }
    fn decrypt(&self, encrypted: &[u8], length: usize) -> Result<Secret, String> {
        let prefix = self.protocol.iv_bytes();
        if !matches!(length, 16 | 32) || encrypted.len() != prefix + length {
            return Err("invalid PIN ciphertext length".into());
        }
        let mut out = Secret::zeroed(length);
        out.0
            .copy_from_slice(encrypted.get(prefix..).ok_or("ciphertext extent")?);
        let iv = if prefix == 0 {
            &[0; 16]
        } else {
            encrypted
                .get(..prefix)
                .ok_or("IV extent")?
                .try_into()
                .map_err(|_| "IV length")?
        };
        fido_aes::decrypt(
            self.aes
                .0
                .as_ref()
                .try_into()
                .map_err(|_| "AES key extent")?,
            iv,
            &mut out.0,
        )?;
        Ok(out)
    }
}

fn fresh_scalar(
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<SecretScalar, String> {
    for _ in 0..8 {
        let mut bytes = Box::new([0; 32]);
        if let Err(error) = entropy(bytes.as_mut()) {
            clear(bytes.as_mut());
            return Err(error);
        }
        if let Ok(private) = SecretScalar::from_bytes(bytes) {
            return Ok(private);
        }
    }
    Err("entropy supplied no valid P-256 scalar in eight attempts".into())
}
fn client_pin(protocol: Protocol, subcommand: u64, count: u64) -> Result<Encoder, String> {
    let mut out = Encoder::new();
    out.head(5, count)?;
    out.head(0, 1)?;
    out.head(0, protocol.number())?;
    out.head(0, 2)?;
    out.head(0, subcommand)?;
    Ok(out)
}
fn encode_key(out: &mut Encoder, key: &PublicKey) -> Result<(), String> {
    let (x, y) = key.coordinates();
    out.head(5, 5)?;
    out.head(0, 1)?;
    out.head(0, 2)?;
    out.head(0, 3)?;
    out.head(1, 24)?;
    out.head(1, 0)?;
    out.head(0, 1)?;
    out.head(1, 1)?;
    out.bytes(&x)?;
    out.head(1, 2)?;
    out.bytes(&y)
}
fn agreement_key(value: &Value<'_>) -> Result<PublicKey, String> {
    if value.map()?.len() != 5
        || value.required(&Value::Unsigned(1))? != &Value::Unsigned(2)
        || value.required(&Value::Unsigned(3))? != &Value::Negative(24)
        || value.required(&Value::Negative(0))? != &Value::Unsigned(1)
    {
        return Err("invalid public PIN key agreement COSE profile".into());
    }
    PublicKey::from_coordinates(
        value
            .required(&Value::Negative(1))?
            .bytes()?
            .try_into()
            .map_err(|_| "PIN x length")?,
        value
            .required(&Value::Negative(2))?
            .bytes()?
            .try_into()
            .map_err(|_| "PIN y length")?,
    )
    .map_err(String::from)
}
fn command(code: u8, out: Encoder, max: usize) -> Result<Secret, String> {
    let encoded = Secret(out.finish()?.into_boxed_slice());
    if encoded.0.len() >= max {
        return Err("portable CTAP request exceeds message limit".into());
    }
    let mut bytes = Secret::zeroed(encoded.0.len() + 1);
    let (head, tail) = bytes.0.split_first_mut().ok_or("command extent")?;
    *head = code;
    tail.copy_from_slice(&encoded.0);
    Ok(bytes)
}
fn response_value(response: &[u8], max: usize) -> Result<Value<'_>, String> {
    if response.len() > max.min(cbor::MAX_BYTES) {
        return Err("portable CTAP response limit".into());
    }
    let (&status, bytes) = response
        .split_first()
        .ok_or("missing CTAP response status")?;
    if status != 0 {
        return Err(format!("portable CTAP refused: {status:#04x}"));
    }
    cbor::decode(bytes)
}
fn array<'a, 'b>(value: &'b Value<'a>) -> Result<&'b [Value<'a>], String> {
    match value {
        Value::Array(values) if !values.is_empty() => Ok(values),
        _ => Err("expected nonempty CTAP array".into()),
    }
}
fn boolean(value: &Value<'_>) -> Result<bool, String> {
    match value {
        Value::Simple(20) => Ok(false),
        Value::Simple(21) => Ok(true),
        _ => Err("expected CTAP boolean".into()),
    }
}
fn option(value: &Value<'_>, key: &str, default: bool) -> Result<bool, String> {
    value
        .get(&Value::Text(key))?
        .map(boolean)
        .transpose()
        .map(|v| v.unwrap_or(default))
}
fn limit(value: &Value<'_>, key: u64, default: usize, ceiling: usize) -> Result<usize, String> {
    match value.get(&Value::Unsigned(key))? {
        None => Ok(default),
        Some(number) => match number.unsigned()? {
            0 => Err("zero CTAP limit".into()),
            number => usize::try_from(number.min(ceiling as u64))
                .map_err(|_| "CTAP limit overflow".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LABELS: [&str; 4] = ["p1-legacy", "p1-scoped", "p2-legacy", "p2-scoped"];
    fn fixture(label: &str, key: &str) -> Vec<u8> {
        let line = include_str!("../tests/pin_vectors.txt")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|row| row.first() == Some(&label) && row.get(1) == Some(&key))
            .unwrap();
        line[2]
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn intent(label: &str, challenge: [u8; 32]) -> Intent {
        Intent {
            credential: Secret(b"fixture-id".to_vec().into_boxed_slice()),
            key: PublicKey::from_coordinates(
                &fixture(label, "x").try_into().unwrap(),
                &fixture(label, "y").try_into().unwrap(),
            )
            .unwrap(),
            challenge,
            salt: fixture(label, "salt").try_into().unwrap(),
            verifier: AssertionRequest::new(b"fixture-id", challenge, 1024).unwrap(),
        }
    }
    fn key_request(label: &str) -> KeyRequest {
        let profile = Profile::parse(&fixture(label, "info")).unwrap();
        profile
            .assertion(
                b"fixture-id",
                PublicKey::from_coordinates(
                    &fixture(label, "x").try_into().unwrap(),
                    &fixture(label, "y").try_into().unwrap(),
                )
                .unwrap(),
                fixture(label, "challenge").try_into().unwrap(),
                fixture(label, "salt").try_into().unwrap(),
            )
            .unwrap()
    }
    fn pin_request(label: &str) -> PinRequest {
        let mut calls = 0;
        let request = key_request(label);
        assert_eq!(request.bytes(), fixture(label, "key_request"));
        let pin = Pin::new(fixture(label, "pin").into_boxed_slice()).unwrap();
        let pending = request
            .with_pin(&fixture(label, "key_response"), pin, &mut |bytes| {
                let name = match calls {
                    0 => "scalar",
                    1 => "iv_pin",
                    _ => panic!("extra entropy"),
                };
                calls += 1;
                bytes.copy_from_slice(&fixture(label, name));
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, if label.starts_with("p1") { 1 } else { 2 });
        assert_eq!(pending.bytes(), fixture(label, "pin_request"));
        pending
    }
    // Isolate the response boundary for exhaustive hostile-response tests.
    fn response_request(label: &str, challenge: [u8; 32]) -> HmacRequest {
        let protocol = Profile::parse(&fixture(label, "info")).unwrap().protocol;
        HmacRequest {
            intent: intent(label, challenge),
            keys: Keys::derive(protocol, &fixture(label, "shared").try_into().unwrap()),
            max_message: 1024,
            bytes: Secret::zeroed(0),
        }
    }

    #[test]
    fn independent_complete_transcripts_cover_both_protocols_and_token_commands() {
        for label in LABELS {
            let pending = pin_request(label);
            assert_eq!(&*pending.keys.aes.0, fixture(label, "aes"));
            assert_eq!(&*pending.keys.hmac.0, fixture(label, "hmac"));
            let mut calls = 0;
            let assertion = pending
                .finish(&fixture(label, "pin_response"), &mut |bytes| {
                    calls += 1;
                    bytes.copy_from_slice(&fixture(label, "iv_salt"));
                    Ok(())
                })
                .unwrap();
            assert_eq!(calls, usize::from(label.starts_with("p2")));
            assert_eq!(assertion.bytes(), fixture(label, "assertion"));
            let output = assertion.finish(&fixture(label, "response")).unwrap();
            assert_eq!(output.bytes(), fixture(label, "output"));
            assert!(output.info.user_verified);
            assert_eq!(output.info.counter, 7);
        }
    }

    #[test]
    fn valid_signatures_do_not_override_uv_presence_rp_or_extension_policy() {
        for label in LABELS {
            let challenge = fixture(label, "challenge").try_into().unwrap();
            for (name, reason) in [
                ("no_uv", "portable assertion requires UV"),
                (
                    "no_up",
                    "invalid assertion presence, attested-data or backup flags",
                ),
                ("missing", "missing signed hmac-secret extension"),
                ("wrong_extension", "missing required CBOR member"),
                ("short_output", "invalid PIN ciphertext length"),
                ("wrong_rp", "CTAP assertion RP hash mismatch"),
            ] {
                let response = fixture(label, name);
                let value = response_value(&response, 1024).unwrap();
                let mut signed = value
                    .required(&Value::Unsigned(2))
                    .unwrap()
                    .bytes()
                    .unwrap()
                    .to_vec();
                signed.extend(challenge);
                let (r, s) = super::super::fido_ctap::signature(
                    value
                        .required(&Value::Unsigned(3))
                        .unwrap()
                        .bytes()
                        .unwrap(),
                )
                .unwrap();
                // Policy can reject before verification; independently pin fixture validity.
                intent(label, challenge)
                    .key
                    .verify(&crypto::digest(&signed), &r, &s)
                    .unwrap();
                assert_eq!(
                    response_request(label, challenge)
                        .finish(&response)
                        .err()
                        .as_deref(),
                    Some(reason),
                    "{label} {name}"
                );
            }
            let mut wrong_challenge = challenge;
            wrong_challenge[0] ^= 1;
            assert!(response_request(label, wrong_challenge)
                .finish(&fixture(label, "response"))
                .is_err());
            let mut wrong_key = response_request(label, challenge);
            wrong_key.intent.key = PublicKey::from_coordinates(
                &fixture("p2-scoped", "x").try_into().unwrap(),
                &fixture("p2-scoped", "y").try_into().unwrap(),
            )
            .unwrap();
            if label != "p2-scoped" {
                assert!(wrong_key.finish(&fixture(label, "response")).is_err());
            }
        }
    }

    #[test]
    fn every_signed_byte_mutation_and_response_truncation_is_refused() {
        for label in ["p1-legacy", "p2-scoped"] {
            let good = fixture(label, "response");
            let challenge = fixture(label, "challenge").try_into().unwrap();
            let value = response_value(&good, 1024).unwrap();
            for key in [2, 3] {
                let signed = value
                    .required(&Value::Unsigned(key))
                    .unwrap()
                    .bytes()
                    .unwrap();
                let start = signed.as_ptr() as usize - good.as_ptr() as usize;
                for offset in start..start + signed.len() {
                    let mut bad = good.clone();
                    bad[offset] ^= 1;
                    assert!(
                        response_request(label, challenge).finish(&bad).is_err(),
                        "{label} {offset}"
                    );
                }
            }
            for size in 0..good.len() {
                assert!(response_request(label, challenge)
                    .finish(&good[..size])
                    .is_err());
            }
            let mut trailing = good.clone();
            trailing.push(0);
            assert!(response_request(label, challenge)
                .finish(&trailing)
                .is_err());
            let mut wrong_id = good.clone();
            let offset = wrong_id
                .windows(10)
                .position(|s| s == b"fixture-id")
                .unwrap();
            wrong_id[offset] ^= 1;
            assert!(response_request(label, challenge)
                .finish(&wrong_id)
                .is_err());
        }
    }

    fn info(
        protocols: &[u64],
        option_name: &str,
        enabled: bool,
        extension: &str,
        max: u64,
    ) -> Vec<u8> {
        let mut out = Encoder::new();
        out.head(5, 6).unwrap();
        out.head(0, 1).unwrap();
        out.head(4, 1).unwrap();
        out.text("FIDO_2_1").unwrap();
        out.head(0, 2).unwrap();
        out.head(4, 1).unwrap();
        out.text(extension).unwrap();
        out.head(0, 3).unwrap();
        out.bytes(&[0; 16]).unwrap();
        out.head(0, 4).unwrap();
        let mut options = vec![("clientPin", true)];
        if option_name == "clientPin" {
            options[0].1 = enabled;
        } else {
            options.push((option_name, enabled));
        }
        options.sort_by_key(|(name, _)| (name.len(), *name));
        out.head(5, options.len() as u64).unwrap();
        for (name, enabled) in options {
            out.text(name).unwrap();
            out.boolean(enabled).unwrap();
        }
        out.head(0, 5).unwrap();
        out.head(0, max).unwrap();
        out.head(0, 6).unwrap();
        out.head(4, protocols.len() as u64).unwrap();
        for protocol in protocols {
            out.head(0, *protocol).unwrap();
        }
        let mut bytes = vec![0];
        bytes.extend(out.finish().unwrap());
        bytes
    }

    #[test]
    fn negotiation_pins_best_protocol_and_refuses_incompatible_profiles() {
        for protocols in [&[1, 2][..], &[2, 1], &[9, 1, 2]] {
            assert!(
                Profile::parse(&info(protocols, "alwaysUv", true, "hmac-secret", 1024))
                    .unwrap()
                    .protocol
                    == Protocol::Two
            );
        }
        for protocols in [&[][..], &[3], &[1, 1], &[2, 1, 2]] {
            assert!(
                Profile::parse(&info(protocols, "clientPin", true, "hmac-secret", 1024)).is_err()
            );
        }
        for (name, enabled) in [
            ("clientPin", false),
            ("plat", true),
            ("up", false),
            ("noMcGaPermissionsWithClientPin", true),
        ] {
            assert!(Profile::parse(&info(&[1], name, enabled, "hmac-secret", 1024)).is_err());
        }
        assert!(Profile::parse(&info(&[1], "clientPin", true, "other", 1024)).is_err());
        assert!(Profile::parse(&info(&[1], "clientPin", true, "hmac-secret", 0)).is_err());
        let profile =
            Profile::parse(&info(&[1], "clientPin", true, "hmac-secret", u64::MAX)).unwrap();
        assert_eq!(profile.max_message, cbor::MAX_BYTES);
        let good = fixture("p2-scoped", "info");
        let version = good
            .windows(8)
            .position(|part| part == b"FIDO_2_1")
            .unwrap();
        for last in *b"0129" {
            let mut changed = good.clone();
            changed[version + 7] = last;
            assert_eq!(Profile::parse(&changed).is_ok(), last != b'9');
        }
        for size in 0..good.len() {
            assert!(Profile::parse(&good[..size]).is_err());
        }
        let mut forced = good.clone();
        forced[1] += 1;
        forced.extend([12, 0xf5]);
        assert!(Profile::parse(&forced).is_err());
    }

    #[test]
    fn pin_and_entropy_admission_never_rewrites_or_retries_after_failure() {
        for size in 0..=65 {
            assert_eq!(
                Pin::new(vec![b'9'; size].into_boxed_slice()).is_ok(),
                (4..=63).contains(&size)
            );
        }
        for bytes in [b"123\0".as_slice(), b"123\n", "123é".as_bytes(), &[0xff; 4]] {
            assert!(Pin::new(bytes.into()).is_err());
        }
        let mut calls = 0;
        let result = fresh_scalar(&mut |bytes| {
            calls += 1;
            bytes.fill(0);
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(calls, 8);
        let mut calls = 0;
        let result = fresh_scalar(&mut |bytes| {
            calls += 1;
            bytes.fill(7);
            Err("entropy failed".into())
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
        let mut calls = 0;
        let result = fresh_scalar(&mut |bytes| {
            calls += 1;
            bytes.fill(0);
            if calls == 2 {
                bytes[31] = 1;
            }
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(calls, 2);
        for stage in [0, 1] {
            let mut calls = 0;
            assert!(key_request("p2-scoped")
                .with_pin(
                    &fixture("p2-scoped", "key_response"),
                    Pin::new(b"1234".to_vec().into_boxed_slice()).unwrap(),
                    &mut |bytes| {
                        let current = calls;
                        calls += 1;
                        if current == stage {
                            bytes.fill(7);
                            return Err("entropy failed".into());
                        }
                        bytes.copy_from_slice(&fixture("p2-scoped", "scalar"));
                        Ok(())
                    }
                )
                .is_err());
            assert_eq!(calls, stage + 1);
        }
        assert!(pin_request("p2-scoped")
            .finish(&fixture("p2-scoped", "pin_response"), &mut |bytes| {
                bytes.fill(7);
                Err("entropy failed".into())
            })
            .is_err());
    }

    #[test]
    fn key_and_token_responses_enforce_shape_lengths_and_status_before_entropy() {
        for label in LABELS {
            let good = fixture(label, "key_response");
            let mut bad = good.clone();
            let value = response_value(&good, 1024).unwrap();
            let x = value
                .required(&Value::Unsigned(1))
                .unwrap()
                .required(&Value::Negative(1))
                .unwrap()
                .bytes()
                .unwrap();
            let offset = x.as_ptr() as usize - good.as_ptr() as usize;
            bad[offset..offset + 32].fill(0xff);
            for response in [
                vec![],
                vec![0x31],
                vec![0x32],
                vec![0x34],
                bad,
                vec![0; 1025],
            ] {
                let mut calls = 0;
                assert!(key_request(label)
                    .with_pin(
                        &response,
                        Pin::new(b"1234".to_vec().into_boxed_slice()).unwrap(),
                        &mut |_| {
                            calls += 1;
                            Err("unexpected entropy".into())
                        }
                    )
                    .is_err());
                assert_eq!(calls, 0);
            }
            let pending = pin_request(label);
            for size in 0..=65 {
                let valid = match pending.profile.protocol {
                    Protocol::One => matches!(size, 16 | 32),
                    Protocol::Two => size == 48,
                };
                let mut out = Encoder::new();
                out.head(5, 1).unwrap();
                out.head(0, 2).unwrap();
                out.bytes(&vec![0; size]).unwrap();
                let mut response = vec![0];
                response.extend(out.finish().unwrap());
                // Reconstruct only the state under test; ECDH has separate transcript coverage.
                let state = PinRequest {
                    profile: Profile::parse(&fixture(label, "info")).unwrap(),
                    intent: intent(label, fixture(label, "challenge").try_into().unwrap()),
                    keys: Keys::derive(
                        pending.profile.protocol,
                        &fixture(label, "shared").try_into().unwrap(),
                    ),
                    public: SecretScalar::from_bytes(Box::new(
                        fixture(label, "scalar").try_into().unwrap(),
                    ))
                    .unwrap()
                    .public_key()
                    .unwrap(),
                    bytes: Secret::zeroed(0),
                };
                let result = state.finish(&response, &mut |bytes| {
                    bytes.fill(5);
                    Ok(())
                });
                assert_eq!(result.is_ok(), valid, "{label} {size}");
            }
        }
    }

    #[test]
    fn key_agreement_rejects_private_wrong_curve_algorithm_and_off_curve_points() {
        let bytes = fixture("p2-scoped", "key_response");
        let response = response_value(&bytes, 1024).unwrap();
        let key = response.required(&Value::Unsigned(1)).unwrap();
        let x = key.required(&Value::Negative(1)).unwrap().bytes().unwrap();
        let y = key.required(&Value::Negative(2)).unwrap().bytes().unwrap();
        for variant in 0..6 {
            let mut out = Encoder::new();
            out.head(5, if variant == 0 { 6 } else { 5 }).unwrap();
            out.head(0, 1).unwrap();
            out.head(0, if variant == 1 { 3 } else { 2 }).unwrap();
            out.head(0, 3).unwrap();
            out.head(1, if variant == 2 { 6 } else { 24 }).unwrap();
            out.head(1, 0).unwrap();
            out.head(0, if variant == 3 { 2 } else { 1 }).unwrap();
            out.head(1, 1).unwrap();
            out.bytes(if variant == 4 { &x[..31] } else { x }).unwrap();
            out.head(1, 2).unwrap();
            out.bytes(if variant == 5 { &[0; 32] } else { y }).unwrap();
            if variant == 0 {
                out.head(1, 3).unwrap();
                out.bytes(&[1; 32]).unwrap();
            }
            let encoded = out.finish().unwrap();
            assert!(agreement_key(&cbor::decode(&encoded).unwrap()).is_err());
        }
        for response in [vec![], vec![0x31], vec![0x32], vec![0x34], vec![0; 1025]] {
            let mut calls = 0;
            assert!(pin_request("p2-scoped")
                .finish(&response, &mut |_| {
                    calls += 1;
                    Err("unexpected entropy".into())
                })
                .is_err());
            assert_eq!(calls, 0);
        }
    }

    #[test]
    fn message_boundaries_admit_exact_length_and_refuse_excess() {
        let label = "p2-scoped";
        let mut pending = pin_request(label);
        let size = fixture(label, "assertion").len();
        pending.profile.max_message = size;
        assert!(pending
            .finish(&fixture(label, "pin_response"), &mut |bytes| {
                bytes.fill(1);
                Ok(())
            })
            .is_ok());
        let mut pending = pin_request(label);
        pending.profile.max_message = size - 1;
        assert!(pending
            .finish(&fixture(label, "pin_response"), &mut |bytes| {
                bytes.fill(1);
                Ok(())
            })
            .is_err());
        let challenge = fixture(label, "challenge").try_into().unwrap();
        let mut pending = response_request(label, challenge);
        pending.max_message = fixture(label, "response").len() - 1;
        assert!(pending.finish(&fixture(label, "response")).is_err());
        assert!(command(6, client_pin(Protocol::One, 2, 2).unwrap(), 6).is_ok());
        assert!(command(6, client_pin(Protocol::One, 2, 2).unwrap(), 5).is_err());
    }
}
