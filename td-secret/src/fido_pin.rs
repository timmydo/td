//! Private, single-use PIN-authorized enrollment and assertions. No device I/O.

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
    max_credentials: usize,
    aaguid: [u8; 16],
    can_enroll: bool,
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
        let aaguid = info
            .required(&Value::Unsigned(3))?
            .bytes()?
            .try_into()
            .map_err(|_| "invalid getInfo AAGUID length")?;
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
        let mut can_enroll = true;
        if let Some(algorithms) = info.get(&Value::Unsigned(10))? {
            can_enroll = false;
            let mut seen = Vec::new();
            for algorithm in array(algorithms)? {
                let kind = algorithm.required(&Value::Text("type"))?.text()?;
                let alg = algorithm.required(&Value::Text("alg"))?;
                if !matches!(alg, Value::Unsigned(_) | Value::Negative(_)) {
                    return Err("invalid portable credential algorithm".into());
                }
                if seen.contains(&(kind, alg)) {
                    return Err("duplicate portable credential algorithm".into());
                }
                seen.push((kind, alg));
                can_enroll |= kind == "public-key" && alg == &Value::Negative(6);
            }
        }
        Ok(Self {
            protocol,
            permissions: option(options, "pinUvAuthToken", false)?,
            max_message: limit(&info, 5, 1024, cbor::MAX_BYTES)?,
            max_id: limit(&info, 8, MAX_CREDENTIAL_ID, MAX_CREDENTIAL_ID)?,
            max_credentials: limit(&info, 7, 8, 8)?,
            aaguid,
            can_enroll,
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

pub(super) struct Intent {
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

pub(super) trait Operation {
    fn challenge(&self) -> &[u8; 32];
    fn permission(&self) -> u64;
}
impl Operation for Intent {
    fn challenge(&self) -> &[u8; 32] {
        &self.challenge
    }
    fn permission(&self) -> u64 {
        2
    }
}

pub(super) struct KeyRequest<I = Intent> {
    profile: Profile,
    intent: I,
    bytes: Secret,
}
impl<I: Operation> KeyRequest<I> {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    /// Each transition consumes its state, including on refusal. No retry policy.
    pub(super) fn with_pin(
        self,
        response: &[u8],
        pin: Pin,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<PinRequest<I>, String> {
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
            out.head(0, self.intent.permission())?; // One operation, fixed RP.
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

pub(super) struct PinRequest<I = Intent> {
    profile: Profile,
    intent: I,
    keys: Keys,
    public: PublicKey,
    bytes: Secret,
}
impl<I: Operation> PinRequest<I> {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    fn authorize(&self, response: &[u8]) -> Result<Secret, String> {
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
        Ok(self
            .profile
            .protocol
            .authenticate(&token.0, self.intent.challenge()))
    }
}
impl PinRequest {
    pub(super) fn finish(
        self,
        response: &[u8],
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<HmacRequest, String> {
        let auth = self.authorize(response)?;
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

/// Creation authority remains private until a fresh PIN/hmac-secret proof succeeds.
pub(super) struct Creation {
    challenge: [u8; 32],
    user: [u8; 32],
    excluded: Vec<Secret>,
}
impl Drop for Creation {
    fn drop(&mut self) {
        clear(&mut self.challenge);
        clear(&mut self.user);
    }
}
impl Operation for Creation {
    fn challenge(&self) -> &[u8; 32] {
        &self.challenge
    }
    fn permission(&self) -> u64 {
        1
    }
}
impl Creation {
    fn encode(&self, profile: &Profile, auth: &[u8]) -> Result<Secret, String> {
        let mut out = Encoder::new();
        out.head(5, 8 + u64::from(!self.excluded.is_empty()))?;
        out.head(0, 1)?;
        out.bytes(&self.challenge)?;
        out.head(0, 2)?;
        out.head(5, 2)?;
        out.text("id")?;
        out.text(RP_ID)?;
        out.text("name")?;
        out.text("td personal vault")?;
        out.head(0, 3)?;
        out.head(5, 3)?;
        out.text("id")?;
        out.bytes(&self.user)?;
        out.text("name")?;
        out.text("td personal vault")?;
        out.text("displayName")?;
        out.text("td personal vault")?;
        out.head(0, 4)?;
        out.head(4, 1)?;
        out.head(5, 2)?;
        out.text("alg")?;
        out.head(1, 6)?;
        out.text("type")?;
        out.text("public-key")?;
        if !self.excluded.is_empty() {
            out.head(0, 5)?;
            out.head(4, self.excluded.len() as u64)?;
            for id in &self.excluded {
                out.head(5, 2)?;
                out.text("id")?;
                out.bytes(&id.0)?;
                out.text("type")?;
                out.text("public-key")?;
            }
        }
        out.head(0, 6)?;
        out.head(5, 1)?;
        out.text("hmac-secret")?;
        out.boolean(true)?;
        out.head(0, 7)?;
        out.head(5, 1)?;
        out.text("rk")?;
        out.boolean(false)?;
        // up defaults true; older tokens reject its explicit inclusion here.
        out.head(0, 8)?;
        out.bytes(auth)?;
        out.head(0, 9)?;
        out.head(0, profile.protocol.number())?;
        command(1, out, profile.max_message)
    }
}
impl Profile {
    /// The backend supplies every existing ID; none may be omitted to fit a token.
    pub(super) fn enrollment(
        self,
        challenge: [u8; 32],
        user: [u8; 32],
        excluded: &[&[u8]],
    ) -> Result<KeyRequest<Creation>, String> {
        if !self.can_enroll {
            return Err("portable token does not advertise ES256 creation".into());
        }
        if excluded.len() > self.max_credentials {
            return Err("portable exclusion list exceeds token capacity".into());
        }
        let mut intent = Creation {
            challenge,
            user,
            excluded: Vec::with_capacity(excluded.len()),
        };
        for id in excluded {
            if id.is_empty() || id.len() > self.max_id {
                return Err(
                    "invalid excluded credential ID length for portable token profile".into(),
                );
            }
            if intent.excluded.iter().any(|old| old.0.as_ref() == *id) {
                return Err("duplicate portable excluded credential".into());
            }
            intent.excluded.push(Secret((*id).into()));
        }
        // Check the entire future command before spending a PIN attempt.
        let auth = Secret::zeroed(if self.protocol == Protocol::One {
            16
        } else {
            32
        });
        intent.encode(&self, &auth.0)?;
        let bytes = command(6, client_pin(self.protocol, 2, 2)?, self.max_message)?;
        Ok(KeyRequest {
            profile: self,
            intent,
            bytes,
        })
    }
}
impl PinRequest<Creation> {
    pub(super) fn make(self, response: &[u8]) -> Result<MakeRequest, String> {
        let auth = self.authorize(response)?;
        let bytes = self.intent.encode(&self.profile, &auth.0)?;
        Ok(MakeRequest {
            profile: self.profile,
            intent: self.intent,
            bytes,
        })
    }
}

pub(super) struct MakeRequest {
    profile: Profile,
    intent: Creation,
    bytes: Secret,
}
impl MakeRequest {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes.0
    }

    /// Attestation is untrusted metadata. Only the subsequent assertion proves a key.
    pub(super) fn proof(
        self,
        response: &[u8],
        challenge: [u8; 32],
        salt: [u8; 32],
    ) -> Result<EnrollmentProof<KeyRequest>, String> {
        if challenge == self.intent.challenge {
            return Err("portable enrollment proof requires a fresh challenge".into());
        }
        // maxMsgSize limits token input, not an attestation-bearing output.
        let value = response_value(response, cbor::MAX_BYTES)?;
        if let Some(enterprise) = value.get(&Value::Unsigned(4))? {
            if boolean(enterprise)? {
                return Err("unrequested portable enterprise attestation".into());
            }
        }
        let format = value.required(&Value::Unsigned(1))?.text()?;
        if format.is_empty() || format.len() > 64 {
            return Err("invalid portable attestation format".into());
        }
        match value.get(&Value::Unsigned(3))? {
            Some(statement) => {
                let statement = statement.map()?;
                if format == "none" && !statement.is_empty() {
                    return Err("nonempty none attestation statement".into());
                }
            }
            None if format != "none" => return Err("missing attestation statement".into()),
            None => {}
        }
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(..32) != Some(crypto::digest(RP_ID.as_bytes()).as_slice()) {
            return Err("portable enrollment RP hash mismatch".into());
        }
        let flags = *data.get(32).ok_or("short portable enrollment flags")?;
        if flags & 0xc5 != 0xc5 || flags & 0x18 != 0 {
            return Err(
                "portable enrollment requires UP, UV, AT, ED and a device-bound key".into(),
            );
        }
        let aaguid = data.get(37..53).ok_or("short portable enrollment AAGUID")?;
        if aaguid != self.profile.aaguid && !(format == "none" && aaguid == [0; 16]) {
            return Err("portable enrollment AAGUID changed".into());
        }
        let length = usize::from(u16::from_be_bytes(
            data.get(53..55)
                .ok_or("short portable credential length")?
                .try_into()
                .map_err(|_| "portable credential length extent")?,
        ));
        if length == 0 || length > self.profile.max_id {
            return Err("created credential ID exceeds portable token profile".into());
        }
        let id = data
            .get(55..55 + length)
            .ok_or("short portable credential ID")?;
        if self.intent.excluded.iter().any(|old| old.0.as_ref() == id) {
            return Err("portable enrollment returned an excluded credential".into());
        }
        let tail = data
            .get(55 + length..)
            .ok_or("missing portable credential key")?;
        let (cose, key_len) = cbor::prefix(tail)?;
        if cose.map()?.len() != 5
            || cose.required(&Value::Unsigned(1))? != &Value::Unsigned(2)
            || cose.required(&Value::Unsigned(3))? != &Value::Negative(6)
            || cose.required(&Value::Negative(0))? != &Value::Unsigned(1)
        {
            return Err("invalid portable public ES256 COSE profile".into());
        }
        let key = PublicKey::from_coordinates(
            cose.required(&Value::Negative(1))?
                .bytes()?
                .try_into()
                .map_err(|_| "credential x length")?,
            cose.required(&Value::Negative(2))?
                .bytes()?
                .try_into()
                .map_err(|_| "credential y length")?,
        )?;
        let extensions = cbor::decode(
            tail.get(key_len..)
                .ok_or("missing portable enrollment extensions")?,
        )?;
        for (name, _) in extensions.map()? {
            name.text()?;
        }
        if !boolean(extensions.required(&Value::Text("hmac-secret"))?)? {
            return Err("portable enrollment did not enable hmac-secret".into());
        }
        let credential = Credential {
            id: Secret(id.into()),
            cose: Secret(tail.get(..key_len).ok_or("credential key extent")?.into()),
            salt,
        };
        let state = self.profile.assertion(id, key, challenge, salt)?;
        Ok(EnrollmentProof { state, credential })
    }
}

struct Credential {
    id: Secret,
    cose: Secret,
    salt: [u8; 32],
}
impl Drop for Credential {
    fn drop(&mut self) {
        clear(&mut self.salt);
    }
}
/// Candidate identity stays attached to the exact proof request through every step.
pub(super) struct EnrollmentProof<S> {
    state: S,
    credential: Credential,
}
impl EnrollmentProof<KeyRequest> {
    pub(super) fn bytes(&self) -> &[u8] {
        self.state.bytes()
    }
    pub(super) fn with_pin(
        self,
        response: &[u8],
        pin: Pin,
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrollmentProof<PinRequest>, String> {
        Ok(EnrollmentProof {
            state: self.state.with_pin(response, pin, entropy)?,
            credential: self.credential,
        })
    }
}
impl EnrollmentProof<PinRequest> {
    pub(super) fn bytes(&self) -> &[u8] {
        self.state.bytes()
    }
    pub(super) fn finish(
        self,
        response: &[u8],
        entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
    ) -> Result<EnrollmentProof<HmacRequest>, String> {
        Ok(EnrollmentProof {
            state: self.state.finish(response, entropy)?,
            credential: self.credential,
        })
    }
}
impl EnrollmentProof<HmacRequest> {
    pub(super) fn bytes(&self) -> &[u8] {
        self.state.bytes()
    }
    pub(super) fn finish(self, response: &[u8]) -> Result<EnrolledCredential, String> {
        let output = self.state.finish(response)?;
        if output.info.backup_eligible || output.info.backed_up {
            return Err("portable enrollment proof is not device-bound".into());
        }
        Ok(EnrolledCredential {
            credential: self.credential,
            output,
        })
    }
}
/// Backend-only evidence from one signed UV hmac-secret proof; no vault publication.
pub(super) struct EnrolledCredential {
    credential: Credential,
    output: HmacOutput,
}
impl EnrolledCredential {
    pub(super) fn id(&self) -> &[u8] {
        &self.credential.id.0
    }
    pub(super) fn cose(&self) -> &[u8] {
        &self.credential.cose.0
    }
    pub(super) fn salt(&self) -> &[u8; 32] {
        &self.credential.salt
    }
    pub(super) fn output(&self) -> &HmacOutput {
        &self.output
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
    fn create_key(label: &str, excluded: &[&[u8]]) -> KeyRequest<Creation> {
        Profile::parse(&fixture(label, "info"))
            .unwrap()
            .enrollment(
                fixture(label, "create_challenge").try_into().unwrap(),
                fixture(label, "user").try_into().unwrap(),
                excluded,
            )
            .unwrap()
    }
    fn create_pin(label: &str, excluded: &[&[u8]]) -> PinRequest<Creation> {
        let mut calls = 0;
        let request = create_key(label, excluded);
        assert_eq!(request.bytes(), fixture(label, "key_request"));
        let result = request
            .with_pin(
                &fixture(label, "create_key_response"),
                Pin::new(fixture(label, "pin").into_boxed_slice()).unwrap(),
                &mut |bytes| {
                    let field = match calls {
                        0 => "create_scalar",
                        1 => "create_iv_pin",
                        _ => panic!("extra entropy"),
                    };
                    calls += 1;
                    bytes.copy_from_slice(&fixture(label, field));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(calls, if label.starts_with("p1") { 1 } else { 2 });
        assert_eq!(result.bytes(), fixture(label, "create_pin_request"));
        result
    }
    fn make(label: &str, excluded: &[&[u8]]) -> MakeRequest {
        create_pin(label, excluded)
            .make(&fixture(label, "create_pin_response"))
            .unwrap()
    }
    // Response-only tests do not repeat key agreement for every malformed byte.
    fn make_parser(label: &str, excluded: &[&[u8]]) -> MakeRequest {
        let request = create_key(label, excluded);
        MakeRequest {
            profile: request.profile,
            intent: request.intent,
            bytes: Secret::zeroed(0),
        }
    }
    fn proof_request(
        label: &str,
        make_response: &[u8],
        challenge: [u8; 32],
    ) -> EnrollmentProof<HmacRequest> {
        let proof = make_parser(label, &[])
            .proof(
                make_response,
                challenge,
                fixture(label, "salt").try_into().unwrap(),
            )
            .unwrap();
        proof_steps(label, proof)
    }
    fn proof_steps(
        label: &str,
        proof: EnrollmentProof<KeyRequest>,
    ) -> EnrollmentProof<HmacRequest> {
        assert_eq!(proof.bytes(), fixture(label, "key_request"));
        let mut calls = 0;
        let proof = proof
            .with_pin(
                &fixture(label, "key_response"),
                Pin::new(fixture(label, "pin").into_boxed_slice()).unwrap(),
                &mut |bytes| {
                    let field = match calls {
                        0 => "scalar",
                        1 => "iv_pin",
                        _ => panic!("extra entropy"),
                    };
                    calls += 1;
                    bytes.copy_from_slice(&fixture(label, field));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(proof.bytes(), fixture(label, "pin_request"));
        proof
            .finish(&fixture(label, "pin_response"), &mut |bytes| {
                bytes.copy_from_slice(&fixture(label, "iv_salt"));
                Ok(())
            })
            .unwrap()
    }
    fn make_data(label: &str) -> Vec<u8> {
        let bytes = fixture(label, "make_none");
        response_value(&bytes, 1024)
            .unwrap()
            .required(&Value::Unsigned(2))
            .unwrap()
            .bytes()
            .unwrap()
            .to_vec()
    }
    fn make_response(data: &[u8], format: &str) -> Vec<u8> {
        let mut out = Encoder::new();
        out.head(5, 2).unwrap();
        out.head(0, 1).unwrap();
        out.text(format).unwrap();
        out.head(0, 2).unwrap();
        out.bytes(data).unwrap();
        let mut response = vec![0];
        response.extend(out.finish().unwrap());
        response
    }

    #[test]
    fn enrollment_transcripts_bind_creation_permission_and_fresh_uv_secret_proof() {
        for label in LABELS {
            for (format, excluded, expected) in [
                ("make_none", &[][..], "make_request"),
                (
                    "make_packed",
                    &[b"prior-primary".as_slice(), b"prior-backup".as_slice()][..],
                    "make_excluded",
                ),
            ] {
                let created = make(label, excluded);
                assert_eq!(created.bytes(), fixture(label, expected));
                let proof = created
                    .proof(
                        &fixture(label, format),
                        fixture(label, "challenge").try_into().unwrap(),
                        fixture(label, "salt").try_into().unwrap(),
                    )
                    .unwrap();
                let proof = proof_steps(label, proof);
                assert_eq!(proof.bytes(), fixture(label, "enroll_assertion"));
                let enrolled = proof.finish(&fixture(label, "enroll_response")).unwrap();
                assert_eq!(enrolled.id(), fixture(label, "credential_id"));
                assert_eq!(enrolled.cose(), fixture(label, "cose"));
                assert_eq!(enrolled.salt().as_slice(), fixture(label, "salt"));
                assert_eq!(enrolled.output().bytes(), fixture(label, "output"));
                assert!(enrolled.output().info.user_verified);
                assert_eq!(enrolled.output().info.counter, 7);
            }
        }
    }

    #[test]
    fn enrollment_responses_refuse_truncation_flags_identity_and_missing_extension() {
        let label = "p2-scoped";
        let challenge = fixture(label, "challenge").try_into().unwrap();
        let salt = fixture(label, "salt").try_into().unwrap();
        let response = fixture(label, "make_none");
        let refuse = |response: &[u8]| {
            make_parser(label, &[])
                .proof(response, challenge, salt)
                .is_err()
        };
        for length in 0..response.len() {
            assert!(refuse(&response[..length]), "length {length}");
        }
        for flags in [0x85, 0xc1, 0xc4, 0x45, 0xcd, 0xd5, 0xdd] {
            let mut data = make_data(label);
            data[32] = flags;
            assert!(refuse(&make_response(&data, "none")), "flags {flags:x}");
        }
        for offset in [0, 37, 53, 54] {
            let mut data = make_data(label);
            data[offset] ^= 1;
            assert!(refuse(&make_response(&data, "none")), "offset {offset}");
        }
        let mut data = make_data(label);
        let len = data.len();
        data[len - 1] = 0xf4;
        assert!(refuse(&make_response(&data, "none")));
        data[len - 1] = 0x01;
        assert!(refuse(&make_response(&data, "none")));
        data.truncate(len - 14);
        assert!(refuse(&make_response(&data, "none")));
        let mut trailing = response.clone();
        trailing.push(0);
        assert!(refuse(&trailing));
        assert!(refuse(&[0x19]));
        assert!(refuse(&[0; 1025]));
        for format in ["", "packed", &"x".repeat(65)] {
            assert!(refuse(&make_response(&make_data(label), format)));
        }
        let mut statement = response.clone();
        statement[1] += 1;
        statement.extend([3, 0xa1, 1, 2]);
        assert!(refuse(&statement));
        let id = fixture(label, "credential_id");
        assert!(make_parser(label, &[&id])
            .proof(&response, challenge, salt)
            .is_err());
        assert!(make_parser(label, &[])
            .proof(
                &response,
                fixture(label, "create_challenge").try_into().unwrap(),
                salt
            )
            .is_err());
        let mut empty_statement = response.clone();
        empty_statement[1] += 1;
        empty_statement.extend([3, 0xa0]);
        assert!(!refuse(&empty_statement));
        let mut non_map = make_response(&make_data(label), "packed");
        non_map[1] += 1;
        non_map.extend([3, 0x80]);
        assert!(refuse(&non_map));
        for (value, admitted) in [(0xf4, true), (0xf5, false), (0, false)] {
            let mut enterprise = response.clone();
            enterprise[1] += 1;
            enterprise.extend([4, value]);
            assert_eq!(!refuse(&enterprise), admitted);
        }
        let padded = |size: usize| {
            let mut out = Encoder::new();
            out.head(5, 3).unwrap();
            out.head(0, 1).unwrap();
            out.text("packed").unwrap();
            out.head(0, 2).unwrap();
            out.bytes(&make_data(label)).unwrap();
            out.head(0, 3).unwrap();
            out.head(5, 1).unwrap();
            out.text("x5c").unwrap();
            out.head(4, 1).unwrap();
            out.bytes(&vec![0; size]).unwrap();
            let mut response = vec![0];
            response.extend(out.finish().unwrap());
            response
        };
        let padding = cbor::MAX_BYTES - padded(0).len() - 2;
        let exact = padded(padding);
        assert_eq!(exact.len(), cbor::MAX_BYTES);
        assert!(exact.len() > Profile::parse(&fixture(label, "info")).unwrap().max_message);
        assert!(!refuse(&exact));
        assert!(refuse(&padded(padding + 1)));
        let mut anonymous = make_parser(label, &[]);
        anonymous.profile.aaguid = [7; 16];
        assert!(anonymous.proof(&response, challenge, salt).is_ok());
        let mut mismatched = make_parser(label, &[]);
        mismatched.profile.aaguid = [7; 16];
        assert!(mismatched
            .proof(&fixture(label, "make_packed"), challenge, salt)
            .is_err());
    }

    #[test]
    fn enrollment_cose_requires_exact_public_es256_curve_membership() {
        let label = "p1-legacy";
        let data = make_data(label);
        let key_offset = 55 + fixture(label, "credential_id").len();
        let key_size = fixture(label, "cose").len();
        let challenge = fixture(label, "challenge").try_into().unwrap();
        let salt = fixture(label, "salt").try_into().unwrap();
        let mut variants = Vec::new();
        for offset in [2, 4, 6] {
            let mut key = fixture(label, "cose");
            key[offset] ^= 1;
            variants.push(key);
        }
        let mut private = fixture(label, "cose");
        private[0] += 1;
        private.extend([0x23, 0x41, 1]);
        variants.push(private);
        let mut off_curve = fixture(label, "cose");
        off_curve[10..42].fill(0xff);
        variants.push(off_curve);
        let mut off_curve = fixture(label, "cose");
        off_curve[45..77].fill(0);
        variants.push(off_curve);
        let mut short_x = fixture(label, "cose");
        short_x[9] = 31;
        short_x.remove(10);
        variants.push(short_x);
        for key in variants {
            let mut changed = data[..key_offset].to_vec();
            changed.extend(key);
            changed.extend(&data[key_offset + key_size..]);
            assert!(make_parser(label, &[])
                .proof(&make_response(&changed, "none"), challenge, salt)
                .is_err());
        }
    }

    #[test]
    fn enrollment_proof_refuses_valid_signed_backup_flags_and_substituted_key_or_hash() {
        let label = "p2-scoped";
        let challenge = fixture(label, "challenge").try_into().unwrap();
        for (field, reason) in [
            ("enroll_be", "portable enrollment proof is not device-bound"),
            ("enroll_bs", "portable enrollment proof is not device-bound"),
            ("enroll_no_uv", "portable assertion requires UV"),
            ("enroll_missing", "missing signed hmac-secret extension"),
            ("enroll_short", "invalid PIN ciphertext length"),
        ] {
            let response = fixture(label, field);
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
            let key = &intent(label, challenge).key;
            key.verify(&crypto::digest(&signed), &r, &s).unwrap();
            let result =
                proof_request(label, &fixture(label, "make_none"), challenge).finish(&response);
            assert_eq!(result.err().unwrap(), reason);
        }
        let response = fixture(label, "enroll_response");
        let result = proof_request(label, &fixture(label, "make_none"), [9; 32]).finish(&response);
        assert_eq!(
            result.err().unwrap(),
            "portable assertion signature mismatch"
        );
        let mut data = make_data(label);
        let key_offset = 55 + fixture(label, "credential_id").len();
        let key = fixture("p1-scoped", "cose");
        data[key_offset..key_offset + key.len()].copy_from_slice(&key);
        let result =
            proof_request(label, &make_response(&data, "none"), challenge).finish(&response);
        assert_eq!(
            result.err().unwrap(),
            "portable assertion signature mismatch"
        );
    }

    #[test]
    fn enrollment_limits_preserve_every_exclusion_before_pin_processing() {
        let label = "p2-scoped";
        let challenge = fixture(label, "create_challenge").try_into().unwrap();
        let user = fixture(label, "user").try_into().unwrap();
        let profile = || Profile::parse(&fixture(label, "info")).unwrap();
        assert!(profile()
            .enrollment(challenge, user, &[b"duplicate", b"duplicate"])
            .is_err());
        assert!(profile().enrollment(challenge, user, &[b""]).is_err());
        let ids: Vec<Vec<u8>> = (0..9).map(|i| vec![i]).collect();
        let refs: Vec<&[u8]> = ids.iter().map(Vec::as_slice).collect();
        assert!(profile().enrollment(challenge, user, &refs[..8]).is_ok());
        assert!(profile().enrollment(challenge, user, &refs).is_err());
        let mut count = fixture(label, "info");
        count[1] += 1;
        count.extend([7, 1]);
        assert!(Profile::parse(&count)
            .unwrap()
            .enrollment(challenge, user, &refs[..2])
            .is_err());
        for advertised in [9, 23] {
            *count.last_mut().unwrap() = advertised;
            assert!(Profile::parse(&count)
                .unwrap()
                .enrollment(challenge, user, &refs[..8])
                .is_ok());
            assert!(Profile::parse(&count)
                .unwrap()
                .enrollment(challenge, user, &refs)
                .is_err());
        }
        let mut limited_id = profile();
        limited_id.max_id = 2;
        assert!(limited_id.enrollment(challenge, user, &[b"abc"]).is_err());
        let size = fixture(label, "make_excluded").len();
        let mut exact = profile();
        exact.max_message = size;
        assert!(exact
            .enrollment(challenge, user, &[b"prior-primary", b"prior-backup"])
            .is_ok());
        let mut short = profile();
        short.max_message = size - 1;
        assert!(short
            .enrollment(challenge, user, &[b"prior-primary", b"prior-backup"])
            .is_err());
        let mut algorithms = fixture(label, "info");
        algorithms[1] += 1;
        algorithms.extend(b"\x0a\x81\xa2\x63alg\x27\x64type\x6apublic-key");
        let unsupported = Profile::parse(&algorithms).unwrap();
        assert!(unsupported.enrollment(challenge, user, &[]).is_err());
        // Advertised creation algorithms do not invalidate an existing ES256 key.
        let i = intent(label, [1; 32]);
        assert!(Profile::parse(&algorithms)
            .unwrap()
            .assertion(
                &i.credential.0,
                PublicKey::from_coordinates(
                    &fixture(label, "x").try_into().unwrap(),
                    &fixture(label, "y").try_into().unwrap()
                )
                .unwrap(),
                [1; 32],
                [2; 32]
            )
            .is_ok());
        let at = algorithms.iter().position(|b| *b == 0x27).unwrap();
        algorithms[at] = 0x26;
        assert!(Profile::parse(&algorithms)
            .unwrap()
            .enrollment(challenge, user, &[])
            .is_ok());
        algorithms[at] = 0xf5;
        assert!(Profile::parse(&algorithms).is_err());
        let mut future = fixture(label, "info");
        future[1] += 1;
        future.extend(
            b"\x0a\x82\xa2\x63alg\x26\x64type\x66future\xa2\x63alg\x26\x64type\x6apublic-key",
        );
        assert!(Profile::parse(&future)
            .unwrap()
            .enrollment(challenge, user, &[])
            .is_ok());

        for count in [0, 2] {
            let mut algorithms = fixture(label, "info");
            algorithms[1] += 1;
            algorithms.extend([10, 0x80 + count]);
            for _ in 0..count {
                algorithms.extend(b"\xa2\x63alg\x26\x64type\x6apublic-key");
            }
            assert!(Profile::parse(&algorithms).is_err());
        }
        let oversized = vec![9; MAX_CREDENTIAL_ID + 1];
        assert!(profile()
            .enrollment(challenge, user, &[&oversized])
            .is_err());
        let large_ids: Vec<Vec<u8>> = (0..8).map(|i| vec![i; MAX_CREDENTIAL_ID]).collect();
        let refs: Vec<&[u8]> = large_ids.iter().map(Vec::as_slice).collect();
        let mut large = profile();
        large.max_message = cbor::MAX_BYTES;
        assert!(large.enrollment(challenge, user, &refs).is_err());
    }

    #[test]
    fn enrollment_cancellation_errors_drop_each_pending_stage_without_output() {
        let label = "p2-scoped";
        for response in [vec![], vec![0x31], vec![0x32], vec![0x34], vec![0; 1025]] {
            let result = create_key(label, &[]).with_pin(
                &response,
                Pin::new(fixture(label, "pin").into_boxed_slice()).unwrap(),
                &mut |_| panic!("malformed key reply spent entropy"),
            );
            assert!(result.is_err());
            assert!(create_pin(label, &[]).make(&response).is_err());
        }
        for fail_call in 0..2 {
            let mut calls = 0;
            let result = create_key(label, &[]).with_pin(
                &fixture(label, "create_key_response"),
                Pin::new(fixture(label, "pin").into_boxed_slice()).unwrap(),
                &mut |bytes| {
                    let call = calls;
                    calls += 1;
                    if call == fail_call {
                        bytes.fill(8);
                        return Err("entropy failed".into());
                    }
                    bytes.copy_from_slice(&fixture(label, "create_scalar"));
                    Ok(())
                },
            );
            assert_eq!(result.err().unwrap(), "entropy failed");
            assert_eq!(calls, fail_call + 1);
        }
        let proof = make_parser(label, &[])
            .proof(
                &fixture(label, "make_none"),
                fixture(label, "challenge").try_into().unwrap(),
                fixture(label, "salt").try_into().unwrap(),
            )
            .unwrap();
        assert!(proof
            .with_pin(
                &[0x31],
                Pin::new(b"1234".to_vec().into_boxed_slice()).unwrap(),
                &mut |_| panic!("malformed proof key reply spent entropy")
            )
            .is_err());
    }
}
