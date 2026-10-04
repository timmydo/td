//! The td LUKS2 token that carries a sealed protector (DESIGN.md "LUKS2
//! tokens"): exactly `type`, `keyslots`, `role`, `public` and `private`.

use td_json::Json;
use td_tpm::SealedObject;

/// The LUKS2 token type td's tokens carry.
pub const TOKEN_TYPE: &str = "td-protector";
/// The highest LUKS2 keyslot or token number (LUKS2 has 32 of each).
pub const MAX_SLOT: u8 = 31;
/// Bounds on the sealed object's TPM2B_PUBLIC and TPM2B_PRIVATE contents.
pub const MAX_PUBLIC_BYTES: usize = 256;
pub const MAX_PRIVATE_BYTES: usize = 512;
/// The longest token text `Token::decode` admits; an encoded token at both
/// bounds is under 1700 bytes.
pub const MAX_TOKEN_JSON: usize = 4096;

const ONE_KEYSLOT: &str = "td token does not name exactly one keyslot";

/// Which protector a token carries (td-install/ENCRYPTION.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Sealed by the installer to PCR 12 at zero alone.
    FirstBoot,
    /// Sealed by the selector to the observed PCR 4 and 9 values.
    DeviceBound,
}
impl Role {
    /// The `role` value written in the token.
    pub fn name(self) -> &'static str {
        match self {
            Self::FirstBoot => "first-boot",
            Self::DeviceBound => "device-bound",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "first-boot" => Some(Self::FirstBoot),
            "device-bound" => Some(Self::DeviceBound),
            _ => None,
        }
    }
}

/// A LUKS2 keyslot or token number as LUKS2 writes it: decimal, 0 to 31,
/// with no sign, space or leading zero.
pub(crate) fn slot_number(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 2 || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u8>().ok().filter(|slot| *slot <= MAX_SLOT)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0x0f] {
            if let Some(digit) = DIGITS.get(usize::from(nibble)) {
                out.push(char::from(*digit));
            }
        }
    }
    out
}

fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// Lowercase hexadecimal of 1 to `max` bytes.
fn unhex(field: &str, text: &str, max: usize) -> Result<Vec<u8>, String> {
    if text.is_empty() || !text.len().is_multiple_of(2) || text.len() / 2 > max {
        return Err(format!(
            "td token {field} is not 1 to {max} bytes of hexadecimal"
        ));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for [high, low] in text.as_bytes().as_chunks::<2>().0 {
        match (nibble(*high), nibble(*low)) {
            (Some(high), Some(low)) => out.push(high << 4 | low),
            _ => return Err(format!("td token {field} is not lowercase hexadecimal")),
        }
    }
    Ok(out)
}

/// A td token: the keyslot it opens, its protector's role and the sealed
/// object. The sealed object is public: the TPM alone can unseal it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    keyslot: u8,
    role: Role,
    sealed: SealedObject,
}
impl Token {
    /// A token within the format's bounds.
    pub fn new(keyslot: u8, role: Role, sealed: SealedObject) -> Result<Self, String> {
        if keyslot > MAX_SLOT {
            return Err(format!("td token keyslot {keyslot} is above {MAX_SLOT}"));
        }
        for (field, bytes, max) in [
            ("public", &sealed.public, MAX_PUBLIC_BYTES),
            ("private", &sealed.private, MAX_PRIVATE_BYTES),
        ] {
            if bytes.is_empty() || bytes.len() > max {
                return Err(format!("td token {field} is not 1 to {max} bytes"));
            }
        }
        Ok(Self {
            keyslot,
            role,
            sealed,
        })
    }

    pub fn keyslot(&self) -> u8 {
        self.keyslot
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn sealed(&self) -> &SealedObject {
        &self.sealed
    }

    /// The token as a JSON object, keys in the documented order.
    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("type".into(), Json::Str(TOKEN_TYPE.into())),
            (
                "keyslots".into(),
                Json::Arr(vec![Json::Str(self.keyslot.to_string())]),
            ),
            ("role".into(), Json::Str(self.role.name().into())),
            ("public".into(), Json::Str(hex(&self.sealed.public))),
            ("private".into(), Json::Str(hex(&self.sealed.private))),
        ])
    }

    /// Compact JSON text, as `cryptsetup token import` takes it.
    pub fn encode(&self) -> String {
        self.to_json().to_string()
    }

    /// Admit token text of at most `MAX_TOKEN_JSON` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_TOKEN_JSON {
            return Err(format!("td token is longer than {MAX_TOKEN_JSON} bytes"));
        }
        let value = td_json::parse_slice(bytes).map_err(|e| format!("td token JSON: {e}"))?;
        Self::from_json(&value)
    }

    /// Admit a parsed token object with exactly the five keys.
    pub fn from_json(value: &Json) -> Result<Self, String> {
        match Self::fields(value)? {
            (Some(keyslot), role, sealed) => Self::new(keyslot, role, sealed),
            (None, _, _) => Err(ONE_KEYSLOT.into()),
        }
    }

    /// Admit an orphan: a td token whose `keyslots` array is empty because
    /// cryptsetup stripped its destroyed keyslot from it. Every other field
    /// must still be the format. Returns its role.
    pub fn orphan_from_json(value: &Json) -> Result<Role, String> {
        match Self::fields(value)? {
            (None, role, _) => Ok(role),
            (Some(_), _, _) => Err("td token names a keyslot, so it is no orphan".into()),
        }
    }

    /// The five fields, with `keyslots` holding one keyslot or none.
    fn fields(value: &Json) -> Result<(Option<u8>, Role, SealedObject), String> {
        let pairs = value.as_obj().ok_or("td token is not a JSON object")?;
        let mut kind = None;
        let mut keyslots = None;
        let mut role = None;
        let mut public = None;
        let mut private = None;
        for (key, value) in pairs {
            let slot = match key.as_str() {
                "type" => &mut kind,
                "keyslots" => &mut keyslots,
                "role" => &mut role,
                "public" => &mut public,
                "private" => &mut private,
                _ => return Err(format!("td token has an unknown key {key:?}")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("td token repeats the key {key:?}"));
            }
        }
        let field = |value: Option<&Json>, name: &str| -> Result<String, String> {
            value
                .ok_or_else(|| format!("td token has no {name}"))?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("td token {name} is not a string"))
        };
        if field(kind, "type")? != TOKEN_TYPE {
            return Err(format!("token is not of type {TOKEN_TYPE}"));
        }
        let keyslots = keyslots
            .ok_or("td token has no keyslots")?
            .as_arr()
            .ok_or("td token keyslots is not an array")?;
        let keyslot = match keyslots {
            [] => None,
            [only] => Some(
                only.as_str()
                    .and_then(slot_number)
                    .ok_or("td token keyslot is not a number from 0 to 31")?,
            ),
            _ => return Err(ONE_KEYSLOT.into()),
        };
        let role = Role::from_name(&field(role, "role")?).ok_or("td token role is unknown")?;
        let public = unhex("public", &field(public, "public")?, MAX_PUBLIC_BYTES)?;
        let private = unhex("private", &field(private, "private")?, MAX_PRIVATE_BYTES)?;
        Ok((keyslot, role, SealedObject { public, private }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed() -> SealedObject {
        SealedObject {
            public: vec![0x00, 0x08, 0xab, 0xcd],
            private: vec![0x01, 0x02, 0xfe],
        }
    }

    const TEXT: &str = r#"{"type":"td-protector","keyslots":["1"],"role":"first-boot","public":"0008abcd","private":"0102fe"}"#;

    #[test]
    fn tokens_encode_exactly_and_round_trip() {
        let token = Token::new(1, Role::FirstBoot, sealed()).unwrap();
        assert_eq!(token.encode(), TEXT);
        assert_eq!(Token::decode(TEXT.as_bytes()).unwrap(), token);
        let bound = Token::new(31, Role::DeviceBound, sealed()).unwrap();
        let text = bound.encode();
        assert!(text.contains(r#""keyslots":["31"],"role":"device-bound""#));
        assert_eq!(Token::decode(text.as_bytes()).unwrap(), bound);
        assert_eq!(bound.keyslot(), 31);
        assert_eq!(bound.role(), Role::DeviceBound);
        assert_eq!(bound.sealed(), &sealed());
        // Key order and whitespace are not part of the format.
        let reordered = r#"{ "private":"0102fe", "public":"0008abcd", "role":"first-boot",
            "keyslots":[ "1" ], "type":"td-protector" }"#;
        assert_eq!(Token::decode(reordered.as_bytes()).unwrap(), token);
        // Both bounds encode under MAX_TOKEN_JSON.
        let widest = Token::new(
            31,
            Role::DeviceBound,
            SealedObject {
                public: vec![0xff; MAX_PUBLIC_BYTES],
                private: vec![0xff; MAX_PRIVATE_BYTES],
            },
        )
        .unwrap();
        assert!(widest.encode().len() < 1700);
        assert_eq!(Token::decode(widest.encode().as_bytes()).unwrap(), widest);
    }

    #[test]
    fn tokens_refuse_other_keys_types_and_values() {
        let refused = |text: &str| Token::decode(text.as_bytes()).unwrap_err();
        let with = |from: &str, to: &str| TEXT.replacen(from, to, 1);
        assert!(refused(&with(r#""role""#, r#""rolf""#)).contains("unknown key \"rolf\""));
        assert!(refused(&with(r#","role":"first-boot""#, "")).contains("no role"));
        assert!(refused(&with("}", r#","extra":1}"#)).contains("unknown key \"extra\""));
        assert!(refused(&with("}", r#","role":"first-boot"}"#)).contains("duplicate"));
        assert!(refused(&with("td-protector", "luks2-keyring")).contains("not of type"));
        assert!(refused(&with(r#"["1"]"#, "[1]")).contains("keyslot is not a number"));
        assert!(refused(&with(r#"["1"]"#, r#""1""#)).contains("not an array"));
        assert!(refused(&with(r#"["1"]"#, "[]")).contains("exactly one keyslot"));
        assert!(refused(&with(r#"["1"]"#, r#"["1","2"]"#)).contains("exactly one keyslot"));
        for keyslot in ["01", "32", "-1", "+1", " 1", "1 ", "", "100", "1e1"] {
            let text = with(r#"["1"]"#, &format!("[{keyslot:?}]"));
            assert!(
                refused(&text).contains("keyslot is not a number"),
                "{keyslot:?}"
            );
        }
        assert!(refused(&with("first-boot", "recovery")).contains("role is unknown"));
        assert!(refused(&with(r#""first-boot""#, "true")).contains("role is not a string"));
        assert!(refused(&with("0008abcd", "0008ABCD")).contains("not lowercase"));
        assert!(refused(&with("0008abcd", "0008abc")).contains("hexadecimal"));
        assert!(refused(&with("0008abcd", "")).contains("hexadecimal"));
        assert!(refused(&with("0102fe", "0102fg")).contains("not lowercase"));
        let wide = "00".repeat(MAX_PUBLIC_BYTES + 1);
        assert!(refused(&with("0008abcd", &wide)).contains("1 to 256 bytes"));
        let wide = "00".repeat(MAX_PRIVATE_BYTES + 1);
        assert!(refused(&with("0102fe", &wide)).contains("1 to 512 bytes"));
        assert!(refused("[]").contains("not a JSON object"));
        assert!(refused("{").contains("td token JSON"));
        let long = format!("{TEXT}{}", " ".repeat(MAX_TOKEN_JSON));
        assert!(refused(&long).contains("longer than 4096 bytes"));
        // A constructed value can repeat a key the parser would refuse.
        let mut pairs = Token::new(1, Role::FirstBoot, sealed())
            .unwrap()
            .to_json()
            .as_obj()
            .unwrap()
            .to_vec();
        pairs.push(("type".into(), Json::Str(TOKEN_TYPE.into())));
        assert!(Token::from_json(&Json::Obj(pairs))
            .unwrap_err()
            .contains("repeats the key \"type\""));
        assert!(Token::new(32, Role::FirstBoot, sealed()).is_err());
        // An orphan is the format with no keyslot, and nothing else.
        let parse = |text: &str| td_json::parse_slice(text.as_bytes()).unwrap();
        assert_eq!(
            Token::orphan_from_json(&parse(&with(r#"["1"]"#, "[]"))),
            Ok(Role::FirstBoot)
        );
        assert!(Token::orphan_from_json(&parse(TEXT))
            .unwrap_err()
            .contains("no orphan"));
        let orphan = |from: &str, to: &str| {
            let text = with(r#"["1"]"#, "[]").replacen(from, to, 1);
            Token::orphan_from_json(&parse(&text)).unwrap_err()
        };
        assert!(orphan("0008abcd", "0008ABCD").contains("not lowercase"));
        assert!(orphan("first-boot", "recovery").contains("role is unknown"));
        assert!(orphan("}", r#","extra":1}"#).contains("unknown key"));
        assert!(orphan("[]", r#"["1","2"]"#).contains("exactly one keyslot"));
        let empty = SealedObject {
            public: Vec::new(),
            private: vec![1],
        };
        assert!(Token::new(1, Role::FirstBoot, empty).is_err());
    }

    #[test]
    fn slot_numbers_are_canonical_decimal_up_to_31() {
        for slot in 0..=31u8 {
            assert_eq!(slot_number(&slot.to_string()), Some(slot));
        }
        for text in ["", "32", "00", "01", "-0", "a", "1.0", "255", "٣"] {
            assert_eq!(slot_number(text), None, "{text:?}");
        }
    }
}
