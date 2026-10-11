//! The protected tier's LUKS2 tokens (DESIGN.md "Protected roles"): the
//! tpm-pin, fido2 and `recovery-key` tokens, the `lockout` token's format
//! (its encryption is `lockout`'s), the `update-retry` token's (its tag is
//! `update_retry`'s), and the `td-upgrade` and `td-request` tokens. Pure
//! codecs: `encode` writes the compact JSON `cryptsetup token import`
//! takes, keys in the documented order, and `decode` and `from_json`
//! admit exactly that format, each key once, in any order.
//! Library code: nothing in production writes one before ENCRYPTION.md
//! item 9, and the reader only bounds and classifies them.

use crate::pin::{PinPcrs, PRIMARY_NAME_LEN, SALT_LEN};
use crate::token::{hex, slot_number, unhex, MAX_SLOT, MAX_TOKEN_JSON, TOKEN_TYPE};
use td_json::Json;
use td_tpm::SealedObject;

/// The token type of the upgrade's state token, its own so that a selector
/// from before increment 8 ignores it as foreign.
pub const UPGRADE_TYPE: &str = "td-upgrade";
/// The token type of a running-system request the selector carries out.
pub const REQUEST_TYPE: &str = "td-request";
/// The one account a td token admits: the primary account's UID.
pub const ACCOUNT: &str = "1000";
/// The longest FIDO2 credential ID a fido2 token carries.
pub const MAX_CREDENTIAL_ID: usize = 255;
/// Bounds on a tpm-pin object's TPM2B_PUBLIC and TPM2B_PRIVATE contents,
/// tighter than the device-bound token's so that the largest protected
/// header leaves 1 KiB of the JSON area spare. td creates the object, a
/// SHA-256 keyed-hash object whose public area the chain check holds to
/// 78 bytes and whose private area, in the TCG reference layout, is 158:
/// integrity HMAC 2 + 32, IV 2 + 16 and encrypted sensitive area 106.
pub const MAX_PIN_PUBLIC_BYTES: usize = 128;
pub const MAX_PIN_PRIVATE_BYTES: usize = 256;
/// A P-256 coordinate, a lockout nonce, ciphertext and tag, and the
/// upgrade's `from`.
pub const FIELD_LEN: usize = 32;

/// A fido2 token's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidoRole {
    /// Releases storage and admits one session.
    Primary,
    /// Releases storage and admits none.
    Recovery,
}
impl FidoRole {
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "fido2-primary",
            Self::Recovery => "fido2-recovery",
        }
    }
}

/// The volume shape an upgrade produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Tpm,
    Fido2,
}
impl Shape {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tpm => "tpm",
            Self::Fido2 => "fido2",
        }
    }
}

/// What a `td-request` token asks of the next selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    PinChange,
    TpmClear,
}
impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::PinChange => "pin-change",
            Self::TpmClear => "tpm-clear",
        }
    }
}

/// A tpm-pin protector: the sealed object, the selection its policy names,
/// the storage primary's Name recorded at its first seal and its PIN salt.
/// `keyslot` is `None` once cryptsetup stripped a killed keyslot from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TpmPin {
    pub keyslot: Option<u8>,
    pub pcrs: PinPcrs,
    pub primary: [u8; PRIMARY_NAME_LEN],
    pub salt: [u8; SALT_LEN],
    pub sealed: SealedObject,
}

/// A FIDO2 protector: the credential ID, its hmac-secret salt and its
/// P-256 public key. `keyslot` is `None` while it is pending (beside a
/// `td-upgrade` token) and otherwise an orphan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fido2 {
    pub keyslot: Option<u8>,
    pub role: FidoRole,
    pub credential: Vec<u8>,
    pub salt: [u8; SALT_LEN],
    pub x: [u8; FIELD_LEN],
    pub y: [u8; FIELD_LEN],
}

/// The lockout hierarchy's authorization, encrypted under the volume key
/// (`lockout::seal`), with the storage primary's Name it was taken under.
/// It names no keyslot. `proven` is set once a take's LockReset or a
/// proof under it succeeded; it lies outside the tag, since whoever can
/// write the header can remove the token anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockout {
    pub primary: [u8; PRIMARY_NAME_LEN],
    pub nonce: [u8; FIELD_LEN],
    pub ciphertext: [u8; FIELD_LEN],
    pub tag: [u8; FIELD_LEN],
    pub proven: bool,
}

/// A staged selector update's retry (ENCRYPTION.md "Selector update"): the
/// SHA-256 over the staged pair's two digests and its tag under a key the
/// volume key derives (`update_retry`). It names no keyslot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRetry {
    pub pair: [u8; FIELD_LEN],
    pub tag: [u8; FIELD_LEN],
}

/// One protected-tier token of td's types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    TpmPin(TpmPin),
    Fido2(Fido2),
    /// The marker naming the recovery key's keyslot.
    RecoveryKey {
        keyslot: Option<u8>,
    },
    Lockout(Lockout),
    UpdateRetry(UpdateRetry),
    /// The upgrade's shape and `from`, the SHA-256 of the data segment's
    /// digest and salt before reencryption.
    Upgrade {
        shape: Shape,
        from: [u8; FIELD_LEN],
    },
    Request {
        operation: Operation,
    },
}

/// Whether a `td-protector` token's `role` is a protected one, so that the
/// reader hands it here rather than to the device-bound codec.
pub fn is_protected_role(role: &str) -> bool {
    matches!(
        role,
        "tpm-pin"
            | "fido2-primary"
            | "fido2-recovery"
            | "recovery-key"
            | "lockout"
            | "update-retry"
    )
}

fn text(value: &str) -> Json {
    Json::Str(value.into())
}

fn bytes(value: &[u8]) -> Json {
    Json::Str(hex(value))
}

/// Exactly `N` bytes of lowercase hexadecimal.
fn fixed<const N: usize>(field: &str, text: &str) -> Result<[u8; N], String> {
    let bytes = unhex(field, text, N)?;
    <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| format!("td token {field} is not {N} bytes"))
}

impl Token {
    /// The keyslot the token names, if any.
    pub fn keyslot(&self) -> Option<u8> {
        match self {
            Self::TpmPin(TpmPin { keyslot, .. })
            | Self::Fido2(Fido2 { keyslot, .. })
            | Self::RecoveryKey { keyslot } => *keyslot,
            Self::Lockout(_)
            | Self::UpdateRetry(_)
            | Self::Upgrade { .. }
            | Self::Request { .. } => None,
        }
    }

    /// The token's `type`, and its `role` where it has one.
    pub fn kind(&self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::TpmPin(_) => (TOKEN_TYPE, Some("tpm-pin")),
            Self::Fido2(fido) => (TOKEN_TYPE, Some(fido.role.name())),
            Self::RecoveryKey { .. } => (TOKEN_TYPE, Some("recovery-key")),
            Self::Lockout(_) => (TOKEN_TYPE, Some("lockout")),
            Self::UpdateRetry(_) => (TOKEN_TYPE, Some("update-retry")),
            Self::Upgrade { .. } => (UPGRADE_TYPE, None),
            Self::Request { .. } => (REQUEST_TYPE, None),
        }
    }

    /// The bounds no array type holds: a keyslot up to 31, a credential ID
    /// of 1 to 255 bytes and sealed areas of 1 to 128 and 1 to 256 bytes.
    fn check(&self) -> Result<(), String> {
        if let Some(slot) = self.keyslot().filter(|slot| *slot > MAX_SLOT) {
            return Err(format!("td token keyslot {slot} is above {MAX_SLOT}"));
        }
        match self {
            Self::TpmPin(pin) => {
                for (field, bytes, max) in [
                    ("public", &pin.sealed.public, MAX_PIN_PUBLIC_BYTES),
                    ("private", &pin.sealed.private, MAX_PIN_PRIVATE_BYTES),
                ] {
                    if bytes.is_empty() || bytes.len() > max {
                        return Err(format!("td token {field} is not 1 to {max} bytes"));
                    }
                }
            }
            Self::Fido2(fido)
                if fido.credential.is_empty() || fido.credential.len() > MAX_CREDENTIAL_ID =>
            {
                return Err(format!(
                    "td token credential is not 1 to {MAX_CREDENTIAL_ID} bytes"
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// The token as a JSON object, keys in the documented order.
    pub fn to_json(&self) -> Result<Json, String> {
        self.check()?;
        let (kind, role) = self.kind();
        let keyslots = Json::Arr(
            self.keyslot()
                .map(|slot| Json::Str(slot.to_string()))
                .into_iter()
                .collect(),
        );
        let mut pairs = vec![
            ("type".to_owned(), text(kind)),
            ("keyslots".to_owned(), keyslots),
        ];
        if let Some(role) = role {
            pairs.push(("role".into(), text(role)));
        }
        let fields: Vec<(&str, Json)> = match self {
            Self::TpmPin(pin) => vec![
                ("account", text(ACCOUNT)),
                ("pcrs", text(pin.pcrs.name())),
                ("primary", bytes(&pin.primary)),
                ("salt", bytes(&pin.salt)),
                ("public", bytes(&pin.sealed.public)),
                ("private", bytes(&pin.sealed.private)),
            ],
            Self::Fido2(fido) => vec![
                ("account", text(ACCOUNT)),
                ("credential", bytes(&fido.credential)),
                ("salt", bytes(&fido.salt)),
                ("x", bytes(&fido.x)),
                ("y", bytes(&fido.y)),
            ],
            Self::RecoveryKey { .. } => Vec::new(),
            Self::Lockout(lockout) => vec![
                ("primary", bytes(&lockout.primary)),
                ("nonce", bytes(&lockout.nonce)),
                ("ciphertext", bytes(&lockout.ciphertext)),
                ("tag", bytes(&lockout.tag)),
                ("proven", text(if lockout.proven { "1" } else { "0" })),
            ],
            Self::UpdateRetry(retry) => {
                vec![("pair", bytes(&retry.pair)), ("tag", bytes(&retry.tag))]
            }
            Self::Upgrade { shape, from } => {
                vec![("shape", text(shape.name())), ("from", bytes(from))]
            }
            Self::Request { operation } => vec![("operation", text(operation.name()))],
        };
        pairs.extend(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Ok(Json::Obj(pairs))
    }

    /// Compact JSON text, as `cryptsetup token import` takes it.
    pub fn encode(&self) -> Result<String, String> {
        Ok(self.to_json()?.to_string())
    }

    /// Admit token text of at most `MAX_TOKEN_JSON` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_TOKEN_JSON {
            return Err(format!("td token is longer than {MAX_TOKEN_JSON} bytes"));
        }
        let value = td_json::parse_slice(bytes).map_err(|e| format!("td token JSON: {e}"))?;
        Self::from_json(&value)
    }

    /// Admit a parsed token object with exactly its format's keys.
    pub fn from_json(value: &Json) -> Result<Self, String> {
        let fields = Fields::new(value)?;
        let kind = fields.text("type")?;
        let token = match kind {
            TOKEN_TYPE => {
                let role = fields.text("role")?;
                match role {
                    "tpm-pin" => {
                        fields.keys(&[
                            "type", "keyslots", "role", "account", "pcrs", "primary", "salt",
                            "public", "private",
                        ])?;
                        fields.account()?;
                        Self::TpmPin(TpmPin {
                            keyslot: fields.keyslot()?,
                            pcrs: PinPcrs::from_name(fields.text("pcrs")?)
                                .ok_or("td token pcrs is not 4,9 or 4,7,9")?,
                            primary: fixed("primary", fields.text("primary")?)?,
                            salt: fixed("salt", fields.text("salt")?)?,
                            sealed: SealedObject {
                                public: unhex(
                                    "public",
                                    fields.text("public")?,
                                    MAX_PIN_PUBLIC_BYTES,
                                )?,
                                private: unhex(
                                    "private",
                                    fields.text("private")?,
                                    MAX_PIN_PRIVATE_BYTES,
                                )?,
                            },
                        })
                    }
                    "fido2-primary" | "fido2-recovery" => {
                        fields.keys(&[
                            "type",
                            "keyslots",
                            "role",
                            "account",
                            "credential",
                            "salt",
                            "x",
                            "y",
                        ])?;
                        fields.account()?;
                        Self::Fido2(Fido2 {
                            keyslot: fields.keyslot()?,
                            role: if role == "fido2-primary" {
                                FidoRole::Primary
                            } else {
                                FidoRole::Recovery
                            },
                            credential: unhex(
                                "credential",
                                fields.text("credential")?,
                                MAX_CREDENTIAL_ID,
                            )?,
                            salt: fixed("salt", fields.text("salt")?)?,
                            x: fixed("x", fields.text("x")?)?,
                            y: fixed("y", fields.text("y")?)?,
                        })
                    }
                    "recovery-key" => {
                        fields.keys(&["type", "keyslots", "role"])?;
                        Self::RecoveryKey {
                            keyslot: fields.keyslot()?,
                        }
                    }
                    "lockout" => {
                        fields.keys(&[
                            "type",
                            "keyslots",
                            "role",
                            "primary",
                            "nonce",
                            "ciphertext",
                            "tag",
                            "proven",
                        ])?;
                        fields.no_keyslot()?;
                        Self::Lockout(Lockout {
                            primary: fixed("primary", fields.text("primary")?)?,
                            nonce: fixed("nonce", fields.text("nonce")?)?,
                            ciphertext: fixed("ciphertext", fields.text("ciphertext")?)?,
                            tag: fixed("tag", fields.text("tag")?)?,
                            proven: match fields.text("proven")? {
                                "0" => false,
                                "1" => true,
                                _ => return Err("td token proven is not 0 or 1".into()),
                            },
                        })
                    }
                    "update-retry" => {
                        fields.keys(&["type", "keyslots", "role", "pair", "tag"])?;
                        fields.no_keyslot()?;
                        Self::UpdateRetry(UpdateRetry {
                            pair: fixed("pair", fields.text("pair")?)?,
                            tag: fixed("tag", fields.text("tag")?)?,
                        })
                    }
                    _ => return Err("td token role is not a protected role".into()),
                }
            }
            UPGRADE_TYPE => {
                fields.keys(&["type", "keyslots", "shape", "from"])?;
                fields.no_keyslot()?;
                Self::Upgrade {
                    shape: match fields.text("shape")? {
                        "tpm" => Shape::Tpm,
                        "fido2" => Shape::Fido2,
                        _ => return Err("td token shape is not tpm or fido2".into()),
                    },
                    from: fixed("from", fields.text("from")?)?,
                }
            }
            REQUEST_TYPE => {
                fields.keys(&["type", "keyslots", "operation"])?;
                fields.no_keyslot()?;
                Self::Request {
                    operation: match fields.text("operation")? {
                        "pin-change" => Operation::PinChange,
                        "tpm-clear" => Operation::TpmClear,
                        _ => return Err("td token operation is not pin-change or tpm-clear".into()),
                    },
                }
            }
            _ => return Err(format!("token type {kind:?} is not one of td's")),
        };
        token.check()?;
        Ok(token)
    }
}

/// A token object's keys, each at most once.
struct Fields<'a>(Vec<(&'a str, &'a Json)>);

impl<'a> Fields<'a> {
    fn new(value: &'a Json) -> Result<Self, String> {
        let pairs = value.as_obj().ok_or("td token is not a JSON object")?;
        let mut fields: Vec<(&str, &Json)> = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            if fields.iter().any(|(seen, _)| *seen == key.as_str()) {
                return Err(format!("td token repeats the key {key:?}"));
            }
            fields.push((key.as_str(), value));
        }
        Ok(Self(fields))
    }

    fn get(&self, key: &str) -> Result<&'a Json, String> {
        self.0
            .iter()
            .find(|(seen, _)| *seen == key)
            .map(|(_, value)| *value)
            .ok_or_else(|| format!("td token has no {key}"))
    }

    fn text(&self, key: &str) -> Result<&'a str, String> {
        self.get(key)?
            .as_str()
            .ok_or_else(|| format!("td token {key} is not a string"))
    }

    /// Exactly `keys`, after `type` and `role` chose them.
    fn keys(&self, keys: &[&str]) -> Result<(), String> {
        if let Some((key, _)) = self.0.iter().find(|(key, _)| !keys.contains(key)) {
            return Err(format!("td token has an unknown key {key:?}"));
        }
        for key in keys {
            self.get(key)?;
        }
        Ok(())
    }

    fn account(&self) -> Result<(), String> {
        if self.text("account")? != ACCOUNT {
            return Err(format!("td token account is not {ACCOUNT}"));
        }
        Ok(())
    }

    /// One keyslot, or none.
    fn keyslot(&self) -> Result<Option<u8>, String> {
        let keyslots = self
            .get("keyslots")?
            .as_arr()
            .ok_or("td token keyslots is not an array")?;
        match keyslots {
            [] => Ok(None),
            [only] => only
                .as_str()
                .and_then(slot_number)
                .map(Some)
                .ok_or_else(|| "td token keyslot is not a number from 0 to 31".into()),
            _ => Err("td token names more than one keyslot".into()),
        }
    }

    fn no_keyslot(&self) -> Result<(), String> {
        match self.keyslot()? {
            None => Ok(()),
            Some(_) => Err("td token of this kind names no keyslot".into()),
        }
    }
}

/// Fixture tokens the vectors (`tests/token_vectors.py`) encode
/// independently, shared with the reader's and the planner's tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) const PRIMARY: [u8; PRIMARY_NAME_LEN] = {
        let mut name = [0x11; PRIMARY_NAME_LEN];
        name[0] = 0x00;
        name[1] = 0x0b;
        name
    };

    pub(crate) fn tpm_pin(keyslot: Option<u8>) -> Token {
        Token::TpmPin(TpmPin {
            keyslot,
            pcrs: PinPcrs::Selector,
            primary: PRIMARY,
            salt: [0x22; 32],
            sealed: SealedObject {
                public: vec![0x00, 0x08, 0xab, 0xcd],
                private: vec![0x01, 0x02, 0xfe],
            },
        })
    }

    pub(crate) fn fido2(keyslot: Option<u8>, role: FidoRole) -> Token {
        Token::Fido2(Fido2 {
            keyslot,
            role,
            credential: vec![0x33; 16],
            salt: [0x44; 32],
            x: [0x55; 32],
            y: [0x66; 32],
        })
    }

    pub(crate) fn upgrade() -> Token {
        Token::Upgrade {
            shape: Shape::Tpm,
            from: [0x77; 32],
        }
    }

    pub(crate) fn request() -> Token {
        Token::Request {
            operation: Operation::PinChange,
        }
    }

    /// `tests/token_vectors.py`'s update-retry token.
    pub(crate) fn update_retry() -> Token {
        Token::UpdateRetry(UpdateRetry {
            pair: unhex_fixed("05483fb1d64a81bbee3bb71ea3becf9ee94b11fed3753a3bc74c0022f1990ee9"),
            tag: unhex_fixed("12d6440af0b8aecb63b8d1889e42ff561e478ab367cd9fac0f9089342a4d3dee"),
        })
    }

    fn unhex_fixed(text: &str) -> [u8; FIELD_LEN] {
        fixed("fixture", text).unwrap_or([0; FIELD_LEN])
    }

    pub(crate) fn lockout() -> Token {
        Token::Lockout(Lockout {
            primary: PRIMARY,
            nonce: [0xa0; 32],
            ciphertext: [0xc1; 32],
            tag: [0xd2; 32],
            proven: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    // `tests/token_vectors.py`'s exact texts.
    const TPM_PIN: &str = r#"{"type":"td-protector","keyslots":["2"],"role":"tpm-pin","account":"1000","pcrs":"4,9","primary":"000b1111111111111111111111111111111111111111111111111111111111111111","salt":"2222222222222222222222222222222222222222222222222222222222222222","public":"0008abcd","private":"0102fe"}"#;
    const FIDO2: &str = r#"{"type":"td-protector","keyslots":["3"],"role":"fido2-primary","account":"1000","credential":"33333333333333333333333333333333","salt":"4444444444444444444444444444444444444444444444444444444444444444","x":"5555555555555555555555555555555555555555555555555555555555555555","y":"6666666666666666666666666666666666666666666666666666666666666666"}"#;
    const PENDING: &str = r#"{"type":"td-protector","keyslots":[],"role":"fido2-recovery","account":"1000","credential":"33333333333333333333333333333333","salt":"4444444444444444444444444444444444444444444444444444444444444444","x":"5555555555555555555555555555555555555555555555555555555555555555","y":"6666666666666666666666666666666666666666666666666666666666666666"}"#;
    const MARKER: &str = r#"{"type":"td-protector","keyslots":["0"],"role":"recovery-key"}"#;
    const UPGRADE: &str = r#"{"type":"td-upgrade","keyslots":[],"shape":"tpm","from":"7777777777777777777777777777777777777777777777777777777777777777"}"#;
    const REQUEST: &str = r#"{"type":"td-request","keyslots":[],"operation":"pin-change"}"#;
    const UPDATE_RETRY: &str = r#"{"type":"td-protector","keyslots":[],"role":"update-retry","pair":"05483fb1d64a81bbee3bb71ea3becf9ee94b11fed3753a3bc74c0022f1990ee9","tag":"12d6440af0b8aecb63b8d1889e42ff561e478ab367cd9fac0f9089342a4d3dee"}"#;

    fn cases() -> Vec<(Token, &'static str)> {
        vec![
            (tpm_pin(Some(2)), TPM_PIN),
            (fido2(Some(3), FidoRole::Primary), FIDO2),
            (fido2(None, FidoRole::Recovery), PENDING),
            (Token::RecoveryKey { keyslot: Some(0) }, MARKER),
            (upgrade(), UPGRADE),
            (request(), REQUEST),
            (update_retry(), UPDATE_RETRY),
        ]
    }

    fn pin_fields(token: Token) -> TpmPin {
        match token {
            Token::TpmPin(pin) => pin,
            other => panic!("not a tpm-pin token: {other:?}"),
        }
    }

    fn fido_fields(token: Token) -> Fido2 {
        match token {
            Token::Fido2(fido) => fido,
            other => panic!("not a fido2 token: {other:?}"),
        }
    }

    #[test]
    fn tokens_encode_exactly_and_round_trip() {
        for (token, text) in cases() {
            assert_eq!(token.encode().unwrap(), text);
            assert_eq!(Token::decode(text.as_bytes()).unwrap(), token);
        }
        // The lockout token's exact text is pinned beside its encryption.
        let text = lockout().encode().unwrap();
        assert_eq!(Token::decode(text.as_bytes()).unwrap(), lockout());
        // Key order and whitespace are not part of the format.
        let reordered = r#"{ "operation":"tpm-clear", "keyslots":[ ], "type":"td-request" }"#;
        assert_eq!(
            Token::decode(reordered.as_bytes()).unwrap(),
            Token::Request {
                operation: Operation::TpmClear
            }
        );
        let fido = Token::decode(FIDO2.replace(r#"["3"]"#, "[]").as_bytes()).unwrap();
        assert_eq!(fido.keyslot(), None);
        assert_eq!(fido2(Some(3), FidoRole::Primary).keyslot(), Some(3));
        assert_eq!(upgrade().kind(), (UPGRADE_TYPE, None));
        assert_eq!(lockout().kind(), (TOKEN_TYPE, Some("lockout")));
        let shape = Token::Upgrade {
            shape: Shape::Fido2,
            from: [0; 32],
        };
        assert_eq!(
            Token::decode(shape.encode().unwrap().as_bytes()).unwrap(),
            shape
        );
        let pcrs = Token::TpmPin(TpmPin {
            pcrs: PinPcrs::SecureBoot,
            ..pin_fields(tpm_pin(None))
        });
        let text = pcrs.encode().unwrap();
        assert!(text.contains(r#""keyslots":[],"role":"tpm-pin","account":"1000","pcrs":"4,7,9""#));
        assert_eq!(Token::decode(text.as_bytes()).unwrap(), pcrs);
    }

    /// Each kind at its bounds: `tests/token_vectors.py`'s lengths, all
    /// under the reader's 4096-byte token bound.
    #[test]
    fn the_largest_tokens() {
        let largest = [
            (
                Token::TpmPin(TpmPin {
                    keyslot: Some(31),
                    pcrs: PinPcrs::SecureBoot,
                    primary: [0xff; 34],
                    salt: [0xff; 32],
                    sealed: SealedObject {
                        public: vec![0xff; MAX_PIN_PUBLIC_BYTES],
                        private: vec![0xff; MAX_PIN_PRIVATE_BYTES],
                    },
                }),
                1038,
            ),
            (
                Token::Fido2(Fido2 {
                    keyslot: Some(31),
                    role: FidoRole::Recovery,
                    credential: vec![0xff; MAX_CREDENTIAL_ID],
                    salt: [0xff; 32],
                    x: [0xff; 32],
                    y: [0xff; 32],
                }),
                824,
            ),
            (Token::RecoveryKey { keyslot: Some(31) }, 63),
            (
                Token::Lockout(Lockout {
                    primary: [0xff; 34],
                    nonce: [0xff; 32],
                    ciphertext: [0xff; 32],
                    tag: [0xff; 32],
                    proven: true,
                }),
                376,
            ),
            (
                Token::Upgrade {
                    shape: Shape::Fido2,
                    from: [0xff; 32],
                },
                125,
            ),
            (request(), 60),
            (
                Token::UpdateRetry(UpdateRetry {
                    pair: [0xff; 32],
                    tag: [0xff; 32],
                }),
                206,
            ),
        ];
        for (token, len) in largest {
            let text = token.encode().unwrap();
            assert_eq!(text.len(), len, "{text}");
            assert!(text.len() < MAX_TOKEN_JSON);
            assert_eq!(Token::decode(text.as_bytes()).unwrap(), token);
        }
    }

    /// `text` with the value of `field` replaced by `bytes` bytes of hex.
    fn wide(text: &str, field: &str, bytes: usize) -> String {
        let start = text.find(&format!(r#""{field}":""#)).unwrap() + field.len() + 4;
        let end = start + text[start..].find('"').unwrap();
        format!("{}{}{}", &text[..start], "ab".repeat(bytes), &text[end..])
    }

    #[test]
    fn tokens_refuse_what_is_not_the_format() {
        let refused = |text: &str| Token::decode(text.as_bytes()).unwrap_err();
        let with = |text: &str, from: &str, to: &str| {
            assert!(text.contains(from), "{from}");
            text.replacen(from, to, 1)
        };
        let cases: &[(String, &str)] = &[
            // Keys: unknown, missing, repeated, another kind's.
            (with(TPM_PIN, "}", r#","x":"00"}"#), "unknown key \"x\""),
            (
                with(MARKER, "}", r#","account":"1000"}"#),
                "unknown key \"account\"",
            ),
            (
                with(UPGRADE, "}", r#","role":"lockout"}"#),
                "unknown key \"role\"",
            ),
            (
                with(TPM_PIN, r#","salt":"#, r#","sal":"#),
                "unknown key \"sal\"",
            ),
            (with(FIDO2, r#","y":"#, r#","z":"#), "unknown key \"z\""),
            (
                with(REQUEST, r#","operation":"pin-change""#, ""),
                "has no operation",
            ),
            (with(MARKER, "}", r#","role":"recovery-key"}"#), "duplicate"),
            (with(MARKER, r#""type":"td-protector","#, ""), "has no type"),
            (with(MARKER, r#","role":"recovery-key""#, ""), "has no role"),
            (with(MARKER, r#""keyslots":["0"],"#, ""), "has no keyslots"),
            // Types and roles.
            (
                with(UPGRADE, "td-upgrade", "td-upgrades"),
                "is not one of td's",
            ),
            (
                with(MARKER, "recovery-key", "first-boot"),
                "not a protected role",
            ),
            (
                with(MARKER, "recovery-key", "secure-boot"),
                "not a protected role",
            ),
            (
                with(MARKER, r#""recovery-key""#, "1"),
                "role is not a string",
            ),
            (
                with(REQUEST, r#""td-request""#, "null"),
                "type is not a string",
            ),
            // Keyslots.
            (
                with(MARKER, r#"["0"]"#, r#"["0","1"]"#),
                "more than one keyslot",
            ),
            (
                with(MARKER, r#"["0"]"#, r#"["00"]"#),
                "keyslot is not a number",
            ),
            (
                with(MARKER, r#"["0"]"#, r#"["32"]"#),
                "keyslot is not a number",
            ),
            (with(MARKER, r#"["0"]"#, "[0]"), "keyslot is not a number"),
            (
                with(MARKER, r#"["0"]"#, r#""0""#),
                "keyslots is not an array",
            ),
            (with(UPGRADE, "[]", r#"["1"]"#), "names no keyslot"),
            (with(REQUEST, "[]", r#"["1"]"#), "names no keyslot"),
            (with(UPDATE_RETRY, "[]", r#"["1"]"#), "names no keyslot"),
            (
                with(UPDATE_RETRY, r#""tag":"#, r#""x":"00","tag":"#),
                "unknown key \"x\"",
            ),
            (
                with(UPDATE_RETRY, r#","tag":"12d6"#, r#","nottag":"12d6"#),
                "unknown key \"nottag\"",
            ),
            (wide(UPDATE_RETRY, "pair", 31), "pair is not 32 bytes"),
            // Values.
            (
                with(TPM_PIN, r#""1000""#, r#""1001""#),
                "account is not 1000",
            ),
            (
                with(FIDO2, r#""1000""#, r#""01000""#),
                "account is not 1000",
            ),
            (with(FIDO2, r#""1000""#, "1000"), "account is not a string"),
            (with(TPM_PIN, r#""4,9""#, r#""4,9,12""#), "pcrs is not"),
            (with(TPM_PIN, r#""4,9""#, r#""9,4""#), "pcrs is not"),
            (with(UPGRADE, r#""tpm""#, r#""TPM""#), "shape is not"),
            (with(REQUEST, "pin-change", "pin-reset"), "operation is not"),
            (
                with(
                    &lockout().encode().unwrap(),
                    r#""proven":"1""#,
                    r#""proven":"2""#,
                ),
                "proven is not 0 or 1",
            ),
            (
                with(
                    &lockout().encode().unwrap(),
                    r#""proven":"1""#,
                    r#""proven":true"#,
                ),
                "proven is not a string",
            ),
            (
                with(&lockout().encode().unwrap(), r#","proven":"1""#, ""),
                "has no proven",
            ),
            (with(TPM_PIN, "000b11", "000B11"), "not lowercase"),
            (with(TPM_PIN, "000b11", "000b1"), "hexadecimal"),
            (
                with(FIDO2, r#""credential":"33"#, r#""credential":"3"#),
                "hexadecimal",
            ),
            ("[]".into(), "not a JSON object"),
            ("{".into(), "td token JSON"),
        ];
        for (text, expected) in cases {
            let error = refused(text);
            assert!(error.contains(expected), "{text}: {error}");
        }
        // Lengths: too long, too short and empty for every byte field.
        assert!(refused(&wide(FIDO2, "credential", 256)).contains("1 to 255 bytes"));
        assert!(refused(&wide(FIDO2, "credential", 0)).contains("1 to 255 bytes"));
        assert!(refused(&wide(TPM_PIN, "public", 129)).contains("1 to 128 bytes"));
        assert!(refused(&wide(TPM_PIN, "public", 0)).contains("1 to 128 bytes"));
        assert!(refused(&wide(TPM_PIN, "private", 257)).contains("1 to 256 bytes"));
        assert!(refused(&wide(TPM_PIN, "private", 0)).contains("1 to 256 bytes"));
        let lockout = lockout().encode().unwrap();
        let fixed = [
            (TPM_PIN, "primary", 34),
            (TPM_PIN, "salt", 32),
            (FIDO2, "salt", 32),
            (FIDO2, "x", 32),
            (FIDO2, "y", 32),
            (lockout.as_str(), "primary", 34),
            (lockout.as_str(), "nonce", 32),
            (lockout.as_str(), "ciphertext", 32),
            (lockout.as_str(), "tag", 32),
            (UPGRADE, "from", 32),
        ];
        for (text, field, len) in fixed {
            assert!(Token::decode(wide(text, field, len).as_bytes()).is_ok());
            for bytes in [0, len - 1, len + 1] {
                let error = refused(&wide(text, field, bytes));
                assert!(error.contains(&format!("td token {field} is")), "{error}");
            }
        }
        assert!(refused(&with(&lockout, "[]", r#"["1"]"#)).contains("names no keyslot"));
        let long = format!("{MARKER}{}", " ".repeat(MAX_TOKEN_JSON));
        assert!(refused(&long).contains("longer than 4096 bytes"));
        // A constructed value can repeat a key the parser would refuse.
        let mut pairs = request().to_json().unwrap().as_obj().unwrap().to_vec();
        pairs.push(("type".into(), Json::Str(REQUEST_TYPE.into())));
        assert!(Token::from_json(&Json::Obj(pairs))
            .unwrap_err()
            .contains("repeats the key \"type\""));
    }

    #[test]
    fn encoding_refuses_values_outside_the_bounds() {
        let refused = |token: Token| token.encode().unwrap_err();
        assert!(refused(Token::RecoveryKey { keyslot: Some(32) }).contains("above 31"));
        let fido = |credential: Vec<u8>| {
            Token::Fido2(Fido2 {
                credential,
                ..fido_fields(fido2(Some(1), FidoRole::Primary))
            })
        };
        assert!(refused(fido(Vec::new())).contains("1 to 255 bytes"));
        assert!(refused(fido(vec![0; 256])).contains("1 to 255 bytes"));
        assert!(fido(vec![0; 255]).encode().is_ok());
        let pin = |public: usize, private: usize| {
            Token::TpmPin(TpmPin {
                sealed: SealedObject {
                    public: vec![1; public],
                    private: vec![1; private],
                },
                ..pin_fields(tpm_pin(Some(1)))
            })
        };
        assert!(refused(pin(0, 1)).contains("public is not 1 to 128"));
        assert!(refused(pin(129, 1)).contains("public is not 1 to 128"));
        assert!(refused(pin(1, 0)).contains("private is not 1 to 256"));
        assert!(refused(pin(1, 257)).contains("private is not 1 to 256"));
        assert!(pin(128, 256).encode().is_ok());
    }

    #[test]
    fn protected_roles_are_named() {
        for role in [
            "tpm-pin",
            "fido2-primary",
            "fido2-recovery",
            "recovery-key",
            "lockout",
        ] {
            assert!(is_protected_role(role));
        }
        for role in ["first-boot", "device-bound", "secure-boot", "config", ""] {
            assert!(!is_protected_role(role));
        }
    }
}
