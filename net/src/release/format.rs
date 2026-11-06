//! Exact signed wire formats. All numbers and hex are canonical ASCII.
use super::{hash, Result};
use crate::sig;

pub const SMALL: u64 = 16 * 1024;
pub const INDEX_LIMIT: u64 = 16 * 1024 * 1024;
pub const RECORD_LIMIT: usize = 200_000;
pub const FILES: [(&str, u64); 3] = [
    ("bzImage", 256 * 1024 * 1024),
    ("initramfs.cpio", 512 * 1024 * 1024),
    ("root.erofs", 8 * 1024 * 1024 * 1024),
];
pub const DAY: u64 = 86400;

pub fn hex(text: &str, bytes: usize) -> Result<Vec<u8>> {
    if text.len() != bytes * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("noncanonical hex".into());
    }
    sig::from_hex(text)
}

pub fn number(text: &str) -> Result<u64> {
    let n = text.parse::<u64>().map_err(|_| "invalid unsigned number")?;
    if n.to_string() != text {
        return Err("noncanonical number".into());
    }
    Ok(n)
}

pub fn profile(text: &str) -> Result<()> {
    if !matches!(text, "system-x86-64" | "server-x86-64") {
        return Err("unsupported release profile".into());
    }
    Ok(())
}

pub fn lifetime(issued: u64, expires: u64, now: u64, maximum: u64) -> Result<()> {
    if issued > now || expires <= now || expires <= issued || expires - issued > maximum {
        return Err("release metadata is expired, future-dated or exceeds its lifetime".into());
    }
    Ok(())
}

pub struct Lines<'a>(std::str::Lines<'a>);
impl<'a> Lines<'a> {
    pub fn new(text: &'a str) -> Result<Self> {
        if !text.is_ascii() || !text.ends_with('\n') || text.contains('\r') {
            return Err("metadata must be newline-terminated ASCII".into());
        }
        Ok(Self(text.lines()))
    }
    pub fn next(&mut self) -> Result<&'a str> {
        self.0.next().ok_or_else(|| "truncated metadata".into())
    }
    pub fn expect(&mut self, expected: &str) -> Result<()> {
        if self.next()? != expected {
            return Err("unexpected metadata field".into());
        }
        Ok(())
    }
    pub fn num(&mut self) -> Result<u64> {
        number(self.next()?)
    }
    pub fn end(&mut self) -> Result<()> {
        if self.0.next().is_some() {
            return Err("extra metadata fields".into());
        }
        Ok(())
    }
}

fn envelope(text: &str) -> Result<(&str, Vec<u8>)> {
    let without = text.strip_suffix('\n').ok_or("unterminated signature")?;
    let (body, signature) = without.rsplit_once('\n').ok_or("missing signature")?;
    let body = text.get(..body.len() + 1).ok_or("signature boundary")?;
    Ok((body, hex(signature, 64)?))
}

pub fn signed(body: &str, key: &[u8]) -> Result<String> {
    Ok(format!(
        "{body}{}\n",
        sig::to_hex(&sig::sign_msg(key, body.as_bytes())?)
    ))
}

pub(super) fn verified<'a>(text: &'a str, key: &[u8]) -> Result<&'a str> {
    let (body, signature) = envelope(text)?;
    if !sig::verify_msg(key, body.as_bytes(), &signature) {
        return Err("invalid release signature".into());
    }
    Ok(body)
}

#[derive(Clone)]
pub struct Root {
    pub text: String,
    pub version: u64,
    pub expires: u64,
    pub root_key: Vec<u8>,
    pub release_key: Vec<u8>,
}
impl Root {
    pub fn parse(text: &str, previous: Option<&Root>) -> Result<Self> {
        if text.len() as u64 > SMALL {
            return Err("root metadata too large".into());
        }
        let (old_envelope, own_sig) = envelope(text)?;
        let (body, old_sig) = envelope(old_envelope)?;
        let mut lines = Lines::new(body)?;
        lines.expect("td-release-root-v1")?;
        let version = lines.num()?;
        let expires = lines.num()?;
        let root_key = hex(lines.next()?, 32)?;
        let release_key = hex(lines.next()?, 32)?;
        lines.end()?;
        if version == 0 || !sig::verify_msg(&root_key, old_envelope.as_bytes(), &own_sig) {
            return Err("invalid root self-signature or version".into());
        }
        if let Some(old) = previous {
            if old.version.checked_add(1) != Some(version)
                || !sig::verify_msg(&old.root_key, body.as_bytes(), &old_sig)
            {
                return Err(
                    "root rotation must be sequential and signed by the previous root".into(),
                );
            }
        }
        Ok(Self {
            text: text.into(),
            version,
            expires,
            root_key,
            release_key,
        })
    }
    pub fn fresh(&self, now: u64) -> Result<()> {
        if self.expires <= now {
            return Err("trusted release root expired; provision a newer root or rotate it".into());
        }
        Ok(())
    }
}

pub struct Head {
    pub root: u64,
    pub version: u64,
    pub issued: u64,
    pub expires: u64,
    pub profile: String,
    pub release: String,
}
impl Head {
    pub fn root_hint(text: &str) -> Result<u64> {
        if text.len() as u64 > SMALL {
            return Err("head metadata too large".into());
        }
        let mut lines = Lines::new(text)?;
        lines.expect("td-release-head-v1")?;
        lines.num()
    }
    pub fn parse(text: &str, root: &Root, now: u64, wanted: &str) -> Result<Self> {
        let mut l = Lines::new(verified(text, &root.release_key)?)?;
        l.expect("td-release-head-v1")?;
        let result = Self {
            root: l.num()?,
            version: l.num()?,
            issued: l.num()?,
            expires: l.num()?,
            profile: l.next()?.into(),
            release: l.next()?.into(),
        };
        l.end()?;
        hex(&result.release, 32)?;
        profile(&result.profile)?;
        if result.root != root.version || result.profile != wanted || result.version == 0 {
            return Err("head root, version or profile mismatch".into());
        }
        lifetime(result.issued, result.expires, now, DAY)?;
        root.fresh(now)?;
        Ok(result)
    }
}

#[derive(Clone)]
pub struct Object {
    pub size: u64,
    pub digest: String,
}
pub struct Payload {
    pub name: &'static str,
    pub size: u64,
    pub digest: String,
    pub objects: Vec<Object>,
}
pub struct Release {
    pub sequence: u64,
    pub expires: u64,
    pub commit: String,
    pub manifest: String,
    pub payloads: Vec<Payload>,
}
impl Release {
    pub fn parse(text: &str, root: &Root, now: u64, wanted: &str) -> Result<Self> {
        if text.len() as u64 > INDEX_LIMIT {
            return Err("release index too large".into());
        }
        let mut l = Lines::new(verified(text, &root.release_key)?)?;
        l.expect("td-release-image-v1")?;
        if l.num()? != root.version {
            return Err("release signed under a different root".into());
        }
        let sequence = l.num()?;
        let issued = l.num()?;
        let expires = l.num()?;
        lifetime(issued, expires, now, 30 * DAY)?;
        if l.next()? != wanted {
            return Err("release profile mismatch".into());
        }
        profile(wanted)?;
        let commit = l.next()?.to_string();
        hex(&commit, 20)?;
        let manifest_hex = l.next()?;
        if manifest_hex.len() > 8192 {
            return Err("deployment manifest too large".into());
        }
        let manifest = String::from_utf8(hex(manifest_hex, manifest_hex.len() / 2)?)
            .map_err(|_| "manifest is not UTF-8")?;
        let mut payloads = Vec::new();
        let mut records = 0;
        for (name, maximum) in FILES {
            let fields: Vec<_> = l.next()?.split(' ').collect();
            let [tag, file, size, digest, count] = fields.as_slice() else {
                return Err("invalid payload header".into());
            };
            if *tag != "file" || *file != name {
                return Err("unexpected payload name".into());
            }
            let size = number(size)?;
            hex(digest, 32)?;
            let count = usize::try_from(number(count)?).map_err(|_| "object count overflow")?;
            if size == 0 || size > maximum || count == 0 || count > RECORD_LIMIT - records {
                return Err("payload size or object count exceeds bounds".into());
            }
            records += count;
            let mut objects = Vec::new();
            let mut total = 0u64;
            for _ in 0..count {
                let (length, digest) = l.next()?.split_once(' ').ok_or("invalid object record")?;
                let length = number(length)?;
                hex(digest, 32)?;
                if length == 0 || length > td_engine::image_chunks::MAX as u64 {
                    return Err("object exceeds size bound".into());
                }
                total += length;
                objects.push(Object {
                    size: length,
                    digest: digest.into(),
                });
            }
            if total != size {
                return Err("objects do not cover payload".into());
            }
            payloads.push(Payload {
                name,
                size,
                digest: (*digest).into(),
                objects,
            });
        }
        l.end()?;
        let expected = manifest_for(&payloads);
        if sequence == 0 || expected != manifest {
            return Err("release manifest does not match payload index".into());
        }
        Ok(Self {
            sequence,
            expires,
            commit,
            manifest,
            payloads,
        })
    }
}

pub fn manifest_for(payloads: &[Payload]) -> String {
    let mut text = String::from("td-deployment-v1\n");
    for payload in payloads {
        text.push_str(&format!("{}  {}\n", payload.digest, payload.name));
    }
    text
}

pub struct State {
    pub root: Root,
    pub head_version: u64,
    pub head_digest: String,
    pub sequence: u64,
    pub release_digest: String,
    pub time: u64,
    pub profile: String,
}
impl State {
    pub fn rotate(&mut self, text: &str) -> Result<()> {
        self.root = Root::parse(text, Some(&self.root))?;
        // An offline-root-authorized epoch cannot inherit poisoned online counters.
        self.head_version = 0;
        self.head_digest = "0".repeat(64);
        self.sequence = 0;
        self.release_digest = "0".repeat(64);
        Ok(())
    }
    pub fn init(root: &str, wanted: &str, now: u64) -> Result<Self> {
        profile(wanted)?;
        let root = Root::parse(root, None)?;
        root.fresh(now)?;
        Ok(Self {
            root,
            head_version: 0,
            head_digest: "0".repeat(64),
            sequence: 0,
            release_digest: "0".repeat(64),
            time: now,
            profile: wanted.into(),
        })
    }
    pub fn text(&self) -> String {
        format!(
            "td-release-state-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            sig::to_hex(self.root.text.as_bytes()),
            self.head_version,
            self.head_digest,
            self.sequence,
            self.release_digest,
            self.time,
            self.profile
        )
    }
    pub fn parse(text: &str) -> Result<Self> {
        let mut l = Lines::new(text)?;
        l.expect("td-release-state-v1")?;
        let encoded = l.next()?;
        if encoded.len() as u64 > SMALL * 2 {
            return Err("stored root too large".into());
        }
        let root =
            String::from_utf8(hex(encoded, encoded.len() / 2)?).map_err(|_| "invalid root text")?;
        let result = Self {
            root: Root::parse(&root, None)?,
            head_version: l.num()?,
            head_digest: l.next()?.into(),
            sequence: l.num()?,
            release_digest: l.next()?.into(),
            time: l.num()?,
            profile: l.next()?.into(),
        };
        l.end()?;
        hex(&result.head_digest, 32)?;
        hex(&result.release_digest, 32)?;
        profile(&result.profile)?;
        Ok(result)
    }
    pub fn clock(&self, now: u64) -> Result<()> {
        if now < self.time {
            return Err("clock moved behind trusted release time".into());
        }
        Ok(())
    }
    pub fn accept(
        &mut self,
        head: &Head,
        head_text: &str,
        release: &Release,
        now: u64,
    ) -> Result<()> {
        self.clock(now)?;
        let digest = hash(head_text.as_bytes());
        if head.version < self.head_version
            || (head.version == self.head_version && digest != self.head_digest)
            || release.sequence < self.sequence
            || (release.sequence == self.sequence && head.release != self.release_digest)
        {
            return Err("release replay or version equivocation".into());
        }
        self.head_version = head.version;
        self.head_digest = digest;
        self.sequence = release.sequence;
        self.release_digest = head.release.clone();
        self.time = now;
        Ok(())
    }
}
