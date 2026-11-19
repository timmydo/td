//! Signed tag lists retain source offsets for removing only the b= value.
use super::{Checked, Error};
use std::ops::Range;
use td_crypto::DkimAlgorithm;
use td_mime::{
    base64::{Decoder, Status},
    time::{Deadline, Tick},
    work::{Charge, Meter},
};

const MAX_TAG_BYTES: usize = 16 * 1024;
const MAX_TAGS: usize = 64;
const MAX_SIGNED: usize = 128;

fn invalid() -> Error {
    Error::Invalid
}
fn trim(s: &str) -> &str {
    s.trim_matches([' ', '\t', '\r', '\n'])
}

struct Tag<'a> {
    name: &'a str,
    value: &'a str,
    // Includes all FWS after '=' and before ';', as required for b=.
    span: Range<usize>,
}

pub(super) struct Tags<'a>(Vec<Tag<'a>>);
impl<'a> Tags<'a> {
    pub fn read(raw: &'a [u8]) -> Checked<Self> {
        if raw.len() > MAX_TAG_BYTES {
            return Err(Error::Unavailable);
        }
        let mut scan = raw;
        while let Some((&byte, rest)) = scan.split_first() {
            scan = match byte {
                b'\r' => rest
                    .strip_prefix(b"\n")
                    .filter(|s| matches!(s.first(), Some(b' ' | b'\t')))
                    .ok_or_else(invalid)?,
                b' '..=b'~' | b'\t' => rest,
                _ => return Err(invalid()),
            }
        }
        let text = std::str::from_utf8(raw).map_err(|_| invalid())?;
        let mut tags: Vec<Tag<'_>> = Vec::new();
        let mut at = 0;
        let mut parts = text.split(';').peekable();
        while let Some(part) = parts.next() {
            if trim(part).is_empty() && parts.peek().is_none() && !tags.is_empty() {
                break;
            }
            if tags.len() == MAX_TAGS {
                return Err(Error::Unavailable);
            }
            let (name, value) = part.split_once('=').ok_or_else(invalid)?;
            let start = at + name.len() + 1;
            let name = trim(name);
            if !name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || tags.iter().any(|item| item.name == name)
            {
                return Err(invalid());
            }
            td_alloc::push(
                &mut tags,
                Tag {
                    name,
                    value: trim(value),
                    span: start..at + part.len(),
                },
            )?;
            at += part.len() + 1;
        }
        Ok(Self(tags))
    }

    fn get(&self, name: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|tag| tag.name == name)
            .map(|tag| tag.value)
    }
    fn need(&self, name: &str) -> Checked<&'a str> {
        self.get(name).ok_or_else(invalid)
    }

    pub fn domain(&self) -> Option<&'a str> {
        self.get("d").filter(|d| dns_labels(d) && d.contains('.'))
    }
    pub fn selector(&self) -> Option<&'a str> {
        self.get("s").filter(|s| dns_labels(s))
    }
    pub fn algorithm(&self) -> Option<DkimAlgorithm> {
        match self.get("a")? {
            "rsa-sha256" => Some(DkimAlgorithm::Rsa),
            "ed25519-sha256" => Some(DkimAlgorithm::Ed25519),
            _ => None,
        }
    }
}

fn dns_labels(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').all(|part| {
            part.len() <= 63
                && part
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && part
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}
fn decimal(text: &str, max_digits: usize) -> Checked<u64> {
    if text.is_empty() || text.len() > max_digits || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    Ok(text.bytes().fold(0u64, |acc, b| {
        acc.saturating_mul(10).saturating_add(u64::from(b - b'0'))
    }))
}
fn relaxed(algo: &str) -> Checked<bool> {
    match algo {
        "simple" => Ok(false),
        "relaxed" => Ok(true),
        _ => Err(invalid()),
    }
}

fn field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b':')
}

fn nibble(b: u8) -> Checked<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(invalid()),
    }
}

// The key's human-readable n= uses MIME qp-section without DKIM's FWS stripping.
fn validate_notes(notes: &str) -> Checked<()> {
    let mut bytes = notes.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'=' => {
                nibble(bytes.next().ok_or_else(invalid)?)?;
                nibble(bytes.next().ok_or_else(invalid)?)?;
            }
            b' '..=b'~' | b'\t' => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

fn qp(encoded: &str) -> Checked<Vec<u8>> {
    let mut input = encoded.bytes().filter(|b| !b.is_ascii_whitespace());
    let mut out = td_alloc::with_capacity(encoded.len())?;
    while let Some(b) = input.next() {
        let byte = if b == b'=' {
            let hi = nibble(input.next().ok_or_else(invalid)?)?;
            let lo = nibble(input.next().ok_or_else(invalid)?)?;
            (hi << 4) | lo
        } else {
            b
        };
        td_alloc::push(&mut out, byte)?;
    }
    Ok(out)
}

fn identity_exact(identity: Option<&str>, domain: &str) -> Checked<bool> {
    let Some(value) = identity else {
        return Ok(true);
    };
    let decoded = qp(value)?;
    let id = std::str::from_utf8(&decoded).map_err(|_| invalid())?;
    let (local, host) = id.rsplit_once('@').ok_or_else(invalid)?;
    if !local.is_empty() && local != "\"\"" {
        let mut address = [0; crate::format::row::MAX_ADDRESS];
        if local.len() > 64 {
            return Err(invalid());
        }
        let test =
            td_alloc::format(format_args!("{local}@x.test")).map_err(|_| Error::Unavailable)?;
        crate::config::values::mailbox_key(&test, &mut address).map_err(|_| invalid())?;
    }
    if !dns_labels(host) {
        return Err(invalid());
    }
    if host.eq_ignore_ascii_case(domain) {
        return Ok(true);
    }
    if host.len() > domain.len() {
        let (prefix, suffix) = host.split_at(host.len() - domain.len());
        if prefix.ends_with('.') && suffix.eq_ignore_ascii_case(domain) {
            return Ok(false);
        }
    }
    Err(invalid())
}

pub(super) struct Signature<'a> {
    pub alg: DkimAlgorithm,
    pub domain: &'a str,
    pub selector: &'a str,
    pub exact_identity: bool,
    pub relaxed_header: bool,
    pub relaxed_body: bool,
    pub length: Option<u64>,
    pub signed: Vec<&'a str>,
    pub body_hash: [u8; 32],
    pub signature: Vec<u8>,
    pub skip: Range<usize>,
}

impl<'a> Signature<'a> {
    pub fn read(tags: &Tags<'a>, now: u64) -> Checked<Self> {
        if tags.need("v")? != "1" {
            return Err(invalid());
        }
        let alg = tags.algorithm().ok_or_else(invalid)?;
        let domain = tags.domain().ok_or_else(invalid)?;
        let selector = tags.selector().ok_or_else(invalid)?;
        let exact_identity = identity_exact(tags.get("i"), domain)?;
        let query = tags.get("q").unwrap_or("dns/txt");
        let mut dns_query = false;
        for method in query.split(':').map(trim) {
            let (kind, params) = method
                .split_once('/')
                .map_or((method, None), |(k, p)| (k, Some(p)));
            if !word(kind) {
                return Err(invalid());
            }
            if let Some(p) = params {
                if p.contains('|') {
                    return Err(invalid());
                }
                qp(p)?;
            }
            dns_query |= method == "dns/txt";
        }
        if !dns_query {
            return Err(invalid());
        }
        let c = tags.get("c").unwrap_or("simple");
        let (head, body) = c.split_once('/').unwrap_or((c, "simple"));
        let relaxed_header = relaxed(head)?;
        let relaxed_body = relaxed(body)?;
        let length = tags.get("l").map(|n| decimal(n, 76)).transpose()?;
        let time = tags.get("t").map(|n| decimal(n, 12)).transpose()?;
        let expiry = tags.get("x").map(|n| decimal(n, 12)).transpose()?;
        if expiry.is_some_and(|x| now > x || time.is_some_and(|t| t >= x)) {
            return Err(invalid());
        }
        if let Some(copies) = tags.get("z") {
            for copy in copies.split('|') {
                let (name, value) = trim(copy).split_once(':').ok_or_else(invalid)?;
                if !field_name(trim(name)) {
                    return Err(invalid());
                }
                qp(value)?;
            }
        }
        let mut signed = Vec::new();
        for name in tags.need("h")?.split(':').map(trim) {
            if signed.len() == MAX_SIGNED {
                return Err(Error::Unavailable);
            }
            if !field_name(name) {
                return Err(invalid());
            }
            td_alloc::push(&mut signed, name)?;
        }
        if !signed.iter().any(|name| name.eq_ignore_ascii_case("from")) {
            return Err(invalid());
        }
        let body_hash = decode(tags.need("bh")?, 32)?
            .as_slice()
            .try_into()
            .map_err(|_| invalid())?;
        let b = tags
            .0
            .iter()
            .find(|tag| tag.name == "b")
            .ok_or_else(invalid)?;
        let signature = decode(b.value, 512)?;
        Ok(Self {
            alg,
            domain,
            selector,
            exact_identity,
            relaxed_header,
            relaxed_body,
            length,
            signed,
            body_hash,
            signature,
            skip: b.span.start..b.span.end,
        })
    }
}

fn word(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn words(value: &str, target: &str, wildcard: bool) -> Checked<bool> {
    let mut found = false;
    for item in value.split(':').map(trim) {
        if !word(item) && !(wildcard && item == "*") {
            return Err(invalid());
        }
        found |= item == target || (wildcard && item == "*");
    }
    Ok(found)
}

pub(super) fn key(record: &[u8], sig: &Signature<'_>) -> Checked<Vec<u8>> {
    let tags = Tags::read(record)?;
    if let Some(version) = tags.get("v") {
        if version != "DKIM1" || !tags.0.first().is_some_and(|tag| tag.name == "v") {
            return Err(invalid());
        }
    }
    let kind = match sig.alg {
        DkimAlgorithm::Rsa => "rsa",
        DkimAlgorithm::Ed25519 => "ed25519",
    };
    if tags.get("k").unwrap_or("rsa") != kind
        || !words(tags.get("h").unwrap_or("sha256"), "sha256", false)?
        || !words(tags.get("s").unwrap_or("*"), "email", true)?
    {
        return Err(invalid());
    }
    if let Some(notes) = tags.get("n") {
        validate_notes(notes)?;
    }
    if let Some(flags) = tags.get("t") {
        if words(flags, "s", false)? && !sig.exact_identity {
            return Err(invalid());
        }
    }
    decode(tags.need("p")?, 1024)
}

fn decode(text: &str, maximum: usize) -> Checked<Vec<u8>> {
    let encoded = text.bytes().filter(|b| !b.is_ascii_whitespace()).count();
    if encoded == 0
        || encoded > maximum.div_ceil(3) * 4
        || (text.contains('=') && !encoded.is_multiple_of(4))
    {
        return Err(invalid());
    }
    let mut decoder = Decoder::default();
    let mut decoded = td_alloc::filled(maximum + 1, 0)?;
    let mut input = text.as_bytes();
    let mut length = 0;
    let budget = (text.len() as u64 + 1) * 8;
    let mut meter = Meter::new(
        Deadline::at(Tick(1)),
        Charge {
            records: budget,
            io_bytes: budget,
            output_bytes: budget,
            ..Charge::default()
        },
    );
    loop {
        let tail = decoded.get_mut(length..).ok_or_else(invalid)?;
        let step = decoder
            .poll(input, tail, true, Tick(0), &mut meter)
            .map_err(|_| Error::Unavailable)?;
        length += step.written;
        input = input.get(step.consumed..).ok_or_else(invalid)?;
        if length > maximum || decoder.is_encoding_problem() {
            return Err(invalid());
        }
        if step.status == Status::Complete {
            break;
        }
    }
    decoded.truncate(length);
    Ok(decoded)
}
