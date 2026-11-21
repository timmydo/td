//! Authenticator-side CTAP requests from a browser. Only parsed fields
//! cross the boundary to signing and credential storage.
use crate::fido_cbor::{decode, Value};
use crate::web_assertion::website_rp;

pub const MAX_CREDENTIALS: usize = 32;
const MAX_ID: usize = 1023;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Error {
    InvalidCommand = 0x01,
    InvalidLength = 0x03,
    InvalidCbor = 0x12,
    MissingParameter = 0x14,
    LimitExceeded = 0x15,
    UnsupportedExtension = 0x16,
    UnsupportedAlgorithm = 0x26,
    UnsupportedOption = 0x2b,
    InvalidOption = 0x2c,
    NotAllowed = 0x30,
}

pub struct Credentials<'a> {
    ids: [&'a [u8]; MAX_CREDENTIALS],
    len: usize,
}

impl<'a> Credentials<'a> {
    pub fn ids(&self) -> &[&'a [u8]] {
        self.ids.get(..self.len).unwrap_or_default()
    }

    fn parse(value: Option<&Value<'a>>) -> Result<Self, Error> {
        let mut ids = Self {
            ids: [&[]; MAX_CREDENTIALS],
            len: 0,
        };
        let Some(value) = value else { return Ok(ids) };
        let Value::Array(descriptors) = value else {
            return Err(Error::InvalidCbor);
        };
        if descriptors.len() > MAX_CREDENTIALS {
            return Err(Error::LimitExceeded);
        }
        for descriptor in descriptors {
            let kind = text(member(descriptor, "type")?)?;
            let id = bytes(member(descriptor, "id")?)?;
            if id.is_empty() || id.len() > MAX_ID {
                return Err(Error::InvalidLength);
            }
            if kind == "public-key" {
                *ids.ids.get_mut(ids.len).ok_or(Error::LimitExceeded)? = id;
                ids.len += 1;
            } else {
                return Err(Error::UnsupportedOption);
            }
        }
        Ok(ids)
    }
}

pub struct Create<'a> {
    rp: &'a str,
    client_data_hash: &'a [u8; 32],
    user_id: &'a [u8],
    user_name: Option<&'a str>,
    exclude: Credentials<'a>,
    discoverable: bool,
    verify_user: bool,
}
impl<'a> Create<'a> {
    pub fn rp(&self) -> &'a str {
        self.rp
    }
    pub fn client_data_hash(&self) -> &'a [u8; 32] {
        self.client_data_hash
    }
    pub fn user_id(&self) -> &'a [u8] {
        self.user_id
    }
    pub fn user_name(&self) -> Option<&'a str> {
        self.user_name
    }
    pub fn exclude(&self) -> &Credentials<'a> {
        &self.exclude
    }
    pub fn discoverable(&self) -> bool {
        self.discoverable
    }
    pub fn verify_user(&self) -> bool {
        self.verify_user
    }
}

pub struct Get<'a> {
    rp: &'a str,
    client_data_hash: &'a [u8; 32],
    allow: Credentials<'a>,
    presence: bool,
    verify_user: bool,
}
impl<'a> Get<'a> {
    pub fn rp(&self) -> &'a str {
        self.rp
    }
    pub fn client_data_hash(&self) -> &'a [u8; 32] {
        self.client_data_hash
    }
    pub fn allow(&self) -> &Credentials<'a> {
        &self.allow
    }
    pub fn presence(&self) -> bool {
        self.presence
    }
    pub fn verify_user(&self) -> bool {
        self.verify_user
    }
}

pub enum Request<'a> {
    Info,
    /// CTAP 2.0 empty pinAuth: request selection only, never registration.
    Select,
    Create(Create<'a>),
    Get(Get<'a>),
}

fn member<'v, 'a>(v: &'v Value<'a>, name: &str) -> Result<&'v Value<'a>, Error> {
    v.map()
        .map_err(|_| Error::InvalidCbor)?
        .iter()
        .find_map(|(k, value)| matches!(k, Value::Text(s) if *s == name).then_some(value))
        .ok_or(Error::MissingParameter)
}

fn text<'a>(v: &Value<'a>) -> Result<&'a str, Error> {
    v.text().map_err(|_| Error::InvalidCbor)
}

fn bytes<'a>(v: &Value<'a>) -> Result<&'a [u8], Error> {
    v.bytes().map_err(|_| Error::InvalidCbor)
}

fn rp<'a>(v: &Value<'a>) -> Result<&'a str, Error> {
    let site = text(v)?;
    if website_rp(site) {
        Ok(site)
    } else {
        Err(Error::NotAllowed)
    }
}

#[derive(Default)]
struct Options {
    rk: bool,
    uv: bool,
    up: Option<bool>,
}

fn options(v: Option<&Value<'_>>, create: bool) -> Result<Options, Error> {
    let mut out = Options::default();
    if let Some(v) = v {
        for (k, v) in v.map().map_err(|_| Error::InvalidCbor)? {
            let b = match v {
                Value::Simple(20) => false,
                Value::Simple(21) => true,
                _ => return Err(Error::InvalidCbor),
            };
            match text(k)? {
                "rk" if create => out.rk = b,
                "uv" => out.uv = b,
                "up" if !create => out.up = Some(b),
                _ => return Err(Error::InvalidOption),
            }
        }
    }
    Ok(out)
}

pub fn parse(input: &[u8]) -> Result<Request<'_>, Error> {
    let (&command, body) = input.split_first().ok_or(Error::InvalidLength)?;
    match command {
        4 if body.is_empty() => return Ok(Request::Info),
        4 => return Err(Error::InvalidLength),
        1 | 2 => (),
        _ => return Err(Error::InvalidCommand),
    }
    let value = decode(body).map_err(|_| Error::InvalidCbor)?;
    let create = command == 1;
    let last = if create { 7 } else { 5 };
    let extension = last - 1;
    let selection = create && matches!(value.get(&Value::Unsigned(8)), Ok(Some(Value::Bytes([]))));
    for (key, field) in value.map().map_err(|_| Error::InvalidCbor)? {
        match key.unsigned().map_err(|_| Error::InvalidCbor)? {
            8 if selection => (),
            9 if selection && matches!(field, Value::Unsigned(1 | 2)) => (),
            n if n == extension => match field.map().map_err(|_| Error::InvalidCbor)? {
                [] => (),
                [(Value::Text("hmac-secret"), Value::Simple(20))] if create => (),
                _ => return Err(Error::UnsupportedExtension),
            },
            n if (1..=last).contains(&n) => (),
            _ => return Err(Error::UnsupportedOption),
        }
    }
    let optional = |n| {
        value
            .get(&Value::Unsigned(n))
            .map_err(|_| Error::InvalidCbor)
    };
    let required = |n| optional(n)?.ok_or(Error::MissingParameter);
    let options = options(optional(last)?, create)?;
    let hash = bytes(required(if create { 1 } else { 2 })?)?
        .try_into()
        .map_err(|_| Error::InvalidLength)?;
    if create {
        let site = rp(member(required(2)?, "id")?)?;
        let user = required(3)?;
        let id = bytes(member(user, "id")?)?;
        if id.is_empty() || id.len() > 64 {
            return Err(Error::InvalidLength);
        }
        let name = user
            .get(&Value::Text("name"))
            .map_err(|_| Error::InvalidCbor)?
            .map(text)
            .transpose()?;
        if name.is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control)) {
            return Err(Error::InvalidOption);
        }
        let Value::Array(algorithms) = required(4)? else {
            return Err(Error::InvalidCbor);
        };
        let mut es256 = false;
        for params in algorithms {
            let kind = text(member(params, "type")?)?;
            let alg = member(params, "alg")?;
            if !matches!(alg, Value::Unsigned(_) | Value::Negative(_)) {
                return Err(Error::InvalidCbor);
            }
            es256 |= kind == "public-key" && alg == &Value::Negative(6);
        }
        if !es256 {
            return Err(Error::UnsupportedAlgorithm);
        }
        let exclude = Credentials::parse(optional(5)?)?;
        if selection {
            return Ok(Request::Select);
        }
        Ok(Request::Create(Create {
            rp: site,
            client_data_hash: hash,
            user_id: id,
            user_name: name,
            exclude,
            discoverable: options.rk,
            verify_user: options.uv,
        }))
    } else {
        let site = rp(required(1)?)?;
        let list = optional(3)?;
        let allow = Credentials::parse(list)?;
        if list.is_some() && allow.ids().is_empty() {
            return Err(Error::InvalidOption);
        }
        Ok(Request::Get(Get {
            rp: site,
            client_data_hash: hash,
            allow,
            presence: options.up.unwrap_or(true),
            verify_user: options.uv,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fido_cbor::Encoder;

    fn assertion(site: &str, extra: Option<(u8, &[u8])>) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 2).unwrap();
        e.head(0, 1).unwrap();
        e.text(site).unwrap();
        e.head(0, 2).unwrap();
        e.bytes(&[31; 32]).unwrap();
        let mut packet = vec![2];
        packet.extend(e.finish().unwrap());
        if let Some((key, bytes)) = extra {
            packet[1] = 0xa3;
            packet.push(key);
            packet.extend_from_slice(bytes);
        }
        packet
    }

    #[test]
    fn browser_commands_have_no_local_or_management_capability() {
        for cmd in 0..=255 {
            if ![1, 2, 4].contains(&cmd) {
                assert_eq!(parse(&[cmd]).err(), Some(Error::InvalidCommand));
            }
        }
        assert!(matches!(parse(&[4]), Ok(Request::Info)));
        for site in [
            "td.invalid",
            "TD.INVALID",
            "td.invalid.",
            "example.com/path",
            "",
        ] {
            assert_eq!(parse(&assertion(site, None)).err(), Some(Error::NotAllowed));
        }
        for field in [6, 7, 8, 9, 10] {
            assert_eq!(
                parse(&assertion("example.com", Some((field, &[0x40])))).err(),
                Some(Error::UnsupportedOption)
            );
        }
        assert_eq!(
            parse(&assertion("example.com", Some((3, &[0x80])))).err(),
            Some(Error::InvalidOption)
        );
        let packet = assertion("example.com", None);
        let Ok(Request::Get(request)) = parse(&packet) else {
            panic!("expected assertion")
        };
        assert_eq!(request.rp, "example.com");
        assert_eq!(request.client_data_hash, &[31; 32]);
        assert!(request.allow.ids().is_empty() && request.presence && !request.verify_user);
    }

    #[test]
    fn registration_binds_the_account_and_requires_an_offered_es256_key() {
        let mut e = Encoder::new();
        e.head(5, 5).unwrap();
        e.head(0, 1).unwrap();
        e.bytes(&[77; 32]).unwrap();
        e.head(0, 2).unwrap();
        e.head(5, 1).unwrap();
        e.text("id").unwrap();
        e.text("accounts.example").unwrap();
        e.head(0, 3).unwrap();
        e.head(5, 2).unwrap();
        e.text("id").unwrap();
        e.bytes(b"account1").unwrap();
        e.text("name").unwrap();
        e.text("Alice").unwrap();
        e.head(0, 4).unwrap();
        e.head(4, 1).unwrap();
        e.head(5, 2).unwrap();
        e.text("alg").unwrap();
        e.head(1, 6).unwrap();
        e.text("type").unwrap();
        e.text("public-key").unwrap();
        e.head(0, 7).unwrap();
        e.head(5, 2).unwrap();
        e.text("rk").unwrap();
        e.boolean(true).unwrap();
        e.text("uv").unwrap();
        e.boolean(true).unwrap();
        let mut packet = vec![1];
        packet.extend(e.finish().unwrap());
        let Ok(Request::Create(r)) = parse(&packet) else {
            panic!("expected registration")
        };
        assert_eq!(
            (r.rp, r.user_name, r.user_id),
            ("accounts.example", Some("Alice"), b"account1".as_slice())
        );
        assert!(r.discoverable && r.verify_user && r.exclude.ids().is_empty());
        // Firefox serializes the switch even when the website leaves it off.
        let options_at = packet
            .windows(3)
            .position(|w| w == [7, 0xa2, 0x62])
            .unwrap();
        let with_extensions = |extensions: &[u8]| {
            let mut p = packet.clone();
            p[1] += 1;
            p.splice(
                options_at..options_at,
                [6].into_iter().chain(extensions.iter().copied()),
            );
            p
        };
        let disabled = b"\xa1\x6bhmac-secret\xf4";
        assert!(matches!(
            parse(&with_extensions(disabled)),
            Ok(Request::Create(_))
        ));
        for refused in [
            &b"\xa1\x6bhmac-secret\xf5"[..],
            &b"\xa1\x6bhmac-secret\x00"[..],
            &b"\xa1\x63foo\xf4"[..],
            &b"\xa2\x63foo\xf4\x6bhmac-secret\xf4"[..],
        ] {
            assert_eq!(
                parse(&with_extensions(refused)).err(),
                Some(Error::UnsupportedExtension)
            );
        }
        assert_eq!(
            parse(&assertion("accounts.example", Some((4, disabled)))).err(),
            Some(Error::UnsupportedExtension)
        );
        let mut select = packet.clone();
        select[1] += 2;
        select.extend_from_slice(&[8, 0x40, 9, 1]);
        assert!(matches!(parse(&select), Ok(Request::Select)));
        let array = packet
            .windows(3)
            .position(|w| w == [4, 0x81, 0xa2])
            .unwrap()
            + 1;
        for index in [array + 1, options_at] {
            let mut malformed = packet.clone();
            malformed[array] = 0x82;
            malformed.insert(index, 0xa0);
            assert_eq!(parse(&malformed).err(), Some(Error::MissingParameter));
        }
        let alg = packet.iter().position(|&b| b == 0x26).unwrap();
        packet[alg] = 0x27;
        assert_eq!(parse(&packet).err(), Some(Error::UnsupportedAlgorithm));
    }
}
