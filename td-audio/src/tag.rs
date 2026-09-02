//! The PulseAudio tagstruct codec.
//!
//! §K.3 describes the encoding and the one distinction the decoder turns on: a
//! tagstruct is self-describing **at the value level and not at the message
//! level**. Every value carries its type tag, so a parser can walk one and skip
//! values without knowing what they mean — but nothing in the encoding says how
//! many values a command carries or what each one *is*. That comes from the
//! command number and the negotiated version.
//!
//! So this module gives two things and not three. It gives a `Reader` that
//! walks tagged values and refuses anything malformed, and a `Writer` that
//! emits them. It does NOT give schemas: those live with the commands, are
//! version-conditioned, and are checked by reading each expected value in turn
//! and then requiring the packet to be exhausted. The tags make a malformed
//! packet *detectable*; the schemas are what make a well-formed but unexpected
//! one an error.
//!
//! # Two corrections that a from-memory decoder gets wrong
//!
//! §K.3 records both, and each would produce a decoder that fails only against
//! real clients.
//!
//! **A boolean is its own pair of tag bytes and is NOT a `B`.** `'1'` and `'0'`
//! are complete values with no payload; `B` is an arbitrary byte with one. A
//! schema expecting `B` where libpulse writes `1` consumes a byte that belongs
//! to the next value and desynchronises the whole packet from there on. The
//! captured `CREATE_PLAYBACK_STREAM` below has sixteen of them.
//!
//! **`V` is a single volume and `v` is a channel volume vector.** They are
//! different tags carrying different payloads, and the sink-info schema needs
//! both.
//!
//! # Bounds
//!
//! Every length in a tagstruct is attacker-controlled: these bytes arrive from
//! a jailed application over a socket. So strings, arbitrary blocks, proplists
//! and channel counts are all bounded here rather than at the call site, and a
//! value that exceeds its bound is a decode error rather than an allocation.

use std::fmt;

/// `PA_TAG_STRING`.
const TAG_STRING: u8 = b't';
/// `PA_TAG_STRING_NULL` — a NULL string, which is distinct from an empty one.
const TAG_STRING_NULL: u8 = b'N';
/// `PA_TAG_U32`.
const TAG_U32: u8 = b'L';
/// `PA_TAG_U8`.
const TAG_U8: u8 = b'B';
/// `PA_TAG_U64`.
const TAG_U64: u8 = b'R';
/// `PA_TAG_S64`.
const TAG_S64: u8 = b'r';
/// `PA_TAG_SAMPLE_SPEC`.
const TAG_SAMPLE_SPEC: u8 = b'a';
/// `PA_TAG_ARBITRARY`.
const TAG_ARBITRARY: u8 = b'x';
/// `PA_TAG_BOOLEAN_TRUE`.
const TAG_TRUE: u8 = b'1';
/// `PA_TAG_BOOLEAN_FALSE`.
const TAG_FALSE: u8 = b'0';
/// `PA_TAG_TIMEVAL`.
const TAG_TIMEVAL: u8 = b'T';
/// `PA_TAG_USEC`.
const TAG_USEC: u8 = b'U';
/// `PA_TAG_CHANNEL_MAP`.
const TAG_CHANNEL_MAP: u8 = b'm';
/// `PA_TAG_CVOLUME`.
const TAG_CVOLUME: u8 = b'v';
/// `PA_TAG_PROPLIST`.
const TAG_PROPLIST: u8 = b'P';
/// `PA_TAG_VOLUME`.
const TAG_VOLUME: u8 = b'V';
/// `PA_TAG_FORMAT_INFO`.
const TAG_FORMAT_INFO: u8 = b'f';

/// `PA_CHANNELS_MAX`.
pub const CHANNELS_MAX: usize = 32;
/// The longest string this decoder will accept, excluding the terminator.
pub const STRING_MAX: usize = 1024;
/// The largest arbitrary block. The authentication cookie is 256 bytes; a
/// proplist value can legitimately be an icon, so the bound is generous and
/// still finite.
pub const ARBITRARY_MAX: usize = 64 * 1024;
/// The most properties one proplist may carry.
pub const PROPLIST_MAX: usize = 256;
/// `PA_INVALID_INDEX`.
pub const INVALID_INDEX: u32 = u32::MAX;

/// What went wrong, in terms a diagnostic can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The packet ended in the middle of a value.
    Truncated { at: usize, wanted: &'static str },
    /// A value was there, but not the one the schema asked for.
    Unexpected { at: usize, wanted: &'static str, found: u8 },
    /// A length in the packet exceeds what this decoder will allocate.
    TooLarge { at: usize, what: &'static str, len: usize },
    /// A string was not valid UTF-8, or ran past the end without a NUL.
    BadString { at: usize },
    /// The schema was satisfied but bytes remained.
    Trailing { at: usize, left: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated { at, wanted } => {
                write!(f, "packet ends at {at} in the middle of a {wanted}")
            }
            Error::Unexpected { at, wanted, found } => write!(
                f,
                "expected a {wanted} at {at}, found tag {:?}",
                *found as char
            ),
            Error::TooLarge { at, what, len } => {
                write!(f, "{what} at {at} declares {len} bytes, which is over the bound")
            }
            Error::BadString { at } => write!(f, "unterminated or non-UTF-8 string at {at}"),
            Error::Trailing { at, left } => write!(
                f,
                "the schema was satisfied at {at} but {left} bytes remain"
            ),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A sample specification: format, channels, rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpec {
    pub format: u8,
    pub channels: u8,
    pub rate: u32,
}

/// `PA_SAMPLE_S16LE`, the one format td's sink runs at.
pub const SAMPLE_S16LE: u8 = 3;

/// One property: a key and its bytes. Values are NOT strings — a proplist may
/// carry arbitrary data — so they stay bytes until something decides otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    pub key: String,
    pub value: Vec<u8>,
}

/// A walk over one tagstruct.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How far in the walk has got, for a diagnostic.
    /// Are there more values?
    /// The exhaustion check §K.3 asks for.
    ///
    /// A schema that reads the values it expects and stops has proved nothing
    /// about the rest of the packet. Requiring the packet to END where the
    /// schema does is what turns "well-formed but unexpected" into an error,
    /// and it is the difference between a decoder that is strict and one that
    /// merely does not crash.
    pub fn finish(&self) -> Result<()> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Trailing {
                at: self.at,
                left: self.bytes.len().saturating_sub(self.at),
            })
        }
    }

    fn peek(&self, wanted: &'static str) -> Result<u8> {
        self.bytes
            .get(self.at)
            .copied()
            .ok_or(Error::Truncated { at: self.at, wanted })
    }

    fn take_tag(&mut self, tag: u8, wanted: &'static str) -> Result<()> {
        let found = self.peek(wanted)?;
        if found != tag {
            return Err(Error::Unexpected {
                at: self.at,
                wanted,
                found,
            });
        }
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    fn take(&mut self, len: usize, wanted: &'static str) -> Result<&'a [u8]> {
        let end = self.at.saturating_add(len);
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(Error::Truncated { at: self.at, wanted })?;
        self.at = end;
        Ok(slice)
    }

    fn be_u32(&mut self, wanted: &'static str) -> Result<u32> {
        let bytes = self.take(4, wanted)?;
        let array: [u8; 4] = bytes.try_into().map_err(|_| Error::Truncated { at: self.at, wanted })?;
        Ok(u32::from_be_bytes(array))
    }

    fn be_u64(&mut self, wanted: &'static str) -> Result<u64> {
        let bytes = self.take(8, wanted)?;
        let array: [u8; 8] = bytes.try_into().map_err(|_| Error::Truncated { at: self.at, wanted })?;
        Ok(u64::from_be_bytes(array))
    }

    pub fn u32(&mut self) -> Result<u32> {
        self.take_tag(TAG_U32, "u32")?;
        self.be_u32("u32")
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.take_tag(TAG_U8, "u8")?;
        let byte = self.take(1, "u8")?;
        byte.first().copied().ok_or(Error::Truncated { at: self.at, wanted: "u8" })
    }

    /// Reply-side value. This server WRITES these and never reads one, so the
    /// only callers today are the tests that verify its replies against the
    /// schemas — which is exactly what makes those tests an oracle rather than
    /// a restatement of the writer. §K.5's `td-audio status`/`volume`
    /// personalities are ordinary clients of this socket and will read them for
    /// real; deleting the half of a codec that a complete implementation needs,
    /// to satisfy a lint, would be the wrong trade.
    #[allow(dead_code)]
    pub fn u64(&mut self) -> Result<u64> {
        self.take_tag(TAG_U64, "u64")?;
        self.be_u64("u64")
    }

    /// Reply-side value. This server WRITES these and never reads one, so the
    /// only callers today are the tests that verify its replies against the
    /// schemas — which is exactly what makes those tests an oracle rather than
    /// a restatement of the writer. §K.5's `td-audio status`/`volume`
    /// personalities are ordinary clients of this socket and will read them for
    /// real; deleting the half of a codec that a complete implementation needs,
    /// to satisfy a lint, would be the wrong trade.
    #[allow(dead_code)]
    pub fn s64(&mut self) -> Result<i64> {
        self.take_tag(TAG_S64, "s64")?;
        Ok(self.be_u64("s64")? as i64)
    }

    /// Reply-side value. This server WRITES these and never reads one, so the
    /// only callers today are the tests that verify its replies against the
    /// schemas — which is exactly what makes those tests an oracle rather than
    /// a restatement of the writer. §K.5's `td-audio status`/`volume`
    /// personalities are ordinary clients of this socket and will read them for
    /// real; deleting the half of a codec that a complete implementation needs,
    /// to satisfy a lint, would be the wrong trade.
    #[allow(dead_code)]
    pub fn usec(&mut self) -> Result<u64> {
        self.take_tag(TAG_USEC, "usec")?;
        self.be_u64("usec")
    }

    pub fn timeval(&mut self) -> Result<(u32, u32)> {
        self.take_tag(TAG_TIMEVAL, "timeval")?;
        Ok((self.be_u32("timeval")?, self.be_u32("timeval")?))
    }

    /// Reply-side value. This server WRITES these and never reads one, so the
    /// only callers today are the tests that verify its replies against the
    /// schemas — which is exactly what makes those tests an oracle rather than
    /// a restatement of the writer. §K.5's `td-audio status`/`volume`
    /// personalities are ordinary clients of this socket and will read them for
    /// real; deleting the half of a codec that a complete implementation needs,
    /// to satisfy a lint, would be the wrong trade.
    #[allow(dead_code)]
    pub fn volume(&mut self) -> Result<u32> {
        self.take_tag(TAG_VOLUME, "volume")?;
        self.be_u32("volume")
    }

    /// A boolean, which is its OWN pair of tags rather than a `B`.
    pub fn boolean(&mut self) -> Result<bool> {
        let found = self.peek("boolean")?;
        match found {
            TAG_TRUE => {
                self.at = self.at.saturating_add(1);
                Ok(true)
            }
            TAG_FALSE => {
                self.at = self.at.saturating_add(1);
                Ok(false)
            }
            _ => Err(Error::Unexpected {
                at: self.at,
                wanted: "boolean",
                found,
            }),
        }
    }

    /// A string, or `None` for the NULL string, which is a different value.
    pub fn string(&mut self) -> Result<Option<String>> {
        let found = self.peek("string")?;
        if found == TAG_STRING_NULL {
            self.at = self.at.saturating_add(1);
            return Ok(None);
        }
        self.take_tag(TAG_STRING, "string")?;
        let start = self.at;
        let rest = self.bytes.get(start..).unwrap_or(&[]);
        let len = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or(Error::BadString { at: start })?;
        if len > STRING_MAX {
            return Err(Error::TooLarge {
                at: start,
                what: "a string",
                len,
            });
        }
        let text = rest.get(..len).unwrap_or(&[]);
        let text = std::str::from_utf8(text).map_err(|_| Error::BadString { at: start })?;
        self.at = start.saturating_add(len).saturating_add(1);
        Ok(Some(text.to_string()))
    }

    /// An arbitrary block of bytes.
    pub fn arbitrary(&mut self) -> Result<&'a [u8]> {
        self.take_tag(TAG_ARBITRARY, "arbitrary")?;
        let at = self.at;
        let len = self.be_u32("arbitrary")? as usize;
        if len > ARBITRARY_MAX {
            return Err(Error::TooLarge {
                at,
                what: "an arbitrary block",
                len,
            });
        }
        self.take(len, "arbitrary")
    }

    pub fn sample_spec(&mut self) -> Result<SampleSpec> {
        self.take_tag(TAG_SAMPLE_SPEC, "sample spec")?;
        let bytes = self.take(2, "sample spec")?;
        let format = bytes.first().copied().unwrap_or(0);
        let channels = bytes.get(1).copied().unwrap_or(0);
        Ok(SampleSpec {
            format,
            channels,
            rate: self.be_u32("sample spec")?,
        })
    }

    pub fn channel_map(&mut self) -> Result<Vec<u8>> {
        self.take_tag(TAG_CHANNEL_MAP, "channel map")?;
        let at = self.at;
        let count = usize::from(
            self.take(1, "channel map")?
                .first()
                .copied()
                .unwrap_or(0),
        );
        if count > CHANNELS_MAX {
            return Err(Error::TooLarge {
                at,
                what: "a channel map",
                len: count,
            });
        }
        Ok(self.take(count, "channel map")?.to_vec())
    }

    pub fn cvolume(&mut self) -> Result<Vec<u32>> {
        self.take_tag(TAG_CVOLUME, "cvolume")?;
        let at = self.at;
        let count = usize::from(self.take(1, "cvolume")?.first().copied().unwrap_or(0));
        if count > CHANNELS_MAX {
            return Err(Error::TooLarge {
                at,
                what: "a cvolume",
                len: count,
            });
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(self.be_u32("cvolume")?);
        }
        Ok(out)
    }

    /// A proplist: `key`, `length`, `value` triples until the NULL string.
    ///
    /// The entries are themselves tagged, so this is a nested walk rather than
    /// a length-prefixed block — which is why the entry count needs its own
    /// bound: without one, a packet of repeated tiny entries allocates without
    /// limit while every individual value stays inside its own.
    pub fn proplist(&mut self) -> Result<Vec<Property>> {
        self.take_tag(TAG_PROPLIST, "proplist")?;
        let mut out = Vec::new();
        loop {
            let at = self.at;
            let Some(key) = self.string()? else {
                return Ok(out);
            };
            if out.len() >= PROPLIST_MAX {
                return Err(Error::TooLarge {
                    at,
                    what: "a proplist",
                    len: out.len().saturating_add(1),
                });
            }
            let declared = self.u32()? as usize;
            let value = self.arbitrary()?;
            // The declared length and the block's own length are two statements
            // about the same bytes; a client that disagrees with itself is one
            // this server does not have to serve.
            if declared != value.len() {
                return Err(Error::TooLarge {
                    at,
                    what: "a proplist value whose declared length disagrees with its block",
                    len: declared,
                });
            }
            out.push(Property {
                key,
                value: value.to_vec(),
            });
        }
    }

    /// A format info: an encoding byte and a proplist.
    pub fn format_info(&mut self) -> Result<(u8, Vec<Property>)> {
        self.take_tag(TAG_FORMAT_INFO, "format info")?;
        Ok((self.u8()?, self.proplist()?))
    }

    /// Skip one value of whatever type without knowing what it means.
    ///
    /// This is the property §K.3 says the encoding buys: a value can be walked
    /// past on tags alone. It is what lets a schema tolerate a trailing field a
    /// newer client added without the server having to model it.
    /// Walk past one value of any type without interpreting it.
    ///
    /// Not reachable from this server's schemas, which are exact by design —
    /// §K.3: "the schemas are what make a well-formed but unexpected one an
    /// error". It is here because a tagstruct walker that cannot skip cannot
    /// report WHERE a packet diverged, and the test below is what proves the
    /// span arithmetic for every tag, including the two compound ones.
    #[allow(dead_code)]
    pub fn skip(&mut self) -> Result<()> {
        let tag = self.peek("a value")?;
        match tag {
            TAG_U32 | TAG_VOLUME => {
                self.u32().map(|_| ()).or_else(|_| self.volume().map(|_| ()))
            }
            TAG_U8 => self.u8().map(|_| ()),
            TAG_U64 => self.u64().map(|_| ()),
            TAG_S64 => self.s64().map(|_| ()),
            TAG_USEC => self.usec().map(|_| ()),
            TAG_TIMEVAL => self.timeval().map(|_| ()),
            TAG_TRUE | TAG_FALSE => self.boolean().map(|_| ()),
            TAG_STRING | TAG_STRING_NULL => self.string().map(|_| ()),
            TAG_ARBITRARY => self.arbitrary().map(|_| ()),
            TAG_SAMPLE_SPEC => self.sample_spec().map(|_| ()),
            TAG_CHANNEL_MAP => self.channel_map().map(|_| ()),
            TAG_CVOLUME => self.cvolume().map(|_| ()),
            TAG_PROPLIST => self.proplist().map(|_| ()),
            TAG_FORMAT_INFO => self.format_info().map(|_| ()),
            found => Err(Error::Unexpected {
                at: self.at,
                wanted: "a value",
                found,
            }),
        }
    }
}

/// Build a tagstruct.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The bytes so far, without consuming the writer. Used where a packet is
    /// framed and then reused, which the tests do and the daemon does not.
    #[allow(dead_code)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes.push(TAG_U32);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.bytes.push(TAG_U8);
        self.bytes.push(value);
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes.push(TAG_U64);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn s64(&mut self, value: i64) -> &mut Self {
        self.bytes.push(TAG_S64);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn usec(&mut self, value: u64) -> &mut Self {
        self.bytes.push(TAG_USEC);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn timeval(&mut self, seconds: u32, micros: u32) -> &mut Self {
        self.bytes.push(TAG_TIMEVAL);
        self.bytes.extend_from_slice(&seconds.to_be_bytes());
        self.bytes.extend_from_slice(&micros.to_be_bytes());
        self
    }

    pub fn volume(&mut self, value: u32) -> &mut Self {
        self.bytes.push(TAG_VOLUME);
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.bytes.push(if value { TAG_TRUE } else { TAG_FALSE });
        self
    }

    /// A string. Interior NULs are refused rather than silently truncating the
    /// value at the first one, which is how a name becomes a different name.
    pub fn string(&mut self, value: &str) -> &mut Self {
        if value.as_bytes().contains(&0) {
            return self.null_string();
        }
        self.bytes.push(TAG_STRING);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        self
    }

    /// The NULL string, which is a different value from the empty string.
    pub fn null_string(&mut self) -> &mut Self {
        self.bytes.push(TAG_STRING_NULL);
        self
    }

    pub fn arbitrary(&mut self, value: &[u8]) -> &mut Self {
        self.bytes.push(TAG_ARBITRARY);
        let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        self.bytes.extend_from_slice(&len.to_be_bytes());
        self.bytes.extend_from_slice(value);
        self
    }

    pub fn sample_spec(&mut self, spec: SampleSpec) -> &mut Self {
        self.bytes.push(TAG_SAMPLE_SPEC);
        self.bytes.push(spec.format);
        self.bytes.push(spec.channels);
        self.bytes.extend_from_slice(&spec.rate.to_be_bytes());
        self
    }

    pub fn channel_map(&mut self, positions: &[u8]) -> &mut Self {
        self.bytes.push(TAG_CHANNEL_MAP);
        let count = positions.len().min(CHANNELS_MAX);
        self.bytes.push(count as u8);
        self.bytes
            .extend_from_slice(positions.get(..count).unwrap_or(&[]));
        self
    }

    pub fn cvolume(&mut self, volumes: &[u32]) -> &mut Self {
        self.bytes.push(TAG_CVOLUME);
        let count = volumes.len().min(CHANNELS_MAX);
        self.bytes.push(count as u8);
        for volume in volumes.iter().take(count) {
            self.bytes.extend_from_slice(&volume.to_be_bytes());
        }
        self
    }

    pub fn proplist(&mut self, properties: &[Property]) -> &mut Self {
        self.bytes.push(TAG_PROPLIST);
        for property in properties {
            self.string(&property.key);
            self.u32(u32::try_from(property.value.len()).unwrap_or(u32::MAX));
            self.arbitrary(&property.value);
        }
        self.null_string();
        self
    }

    pub fn format_info(&mut self, encoding: u8, properties: &[Property]) -> &mut Self {
        self.bytes.push(TAG_FORMAT_INFO);
        self.u8(encoding);
        self.proplist(properties);
        self
    }
}

/// A property whose value is a NUL-terminated string, which is how libpulse
/// writes text properties — the terminator is inside the counted bytes.
pub fn text_property(key: &str, value: &str) -> Property {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    Property {
        key: key.to_string(),
        value: bytes,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Hex to bytes, for the captured fixtures.
    fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .filter_map(|pair| {
                let s = std::str::from_utf8(pair).ok()?;
                u8::from_str_radix(s, 16).ok()
            })
            .collect()
    }

    /// CAPTURED, not written from memory.
    ///
    /// §K.3: "Do not write the command table from memory. Capture it: run the
    /// pinned runtime's own libpulse ... against a logging stub ... and commit
    /// the captures as golden fixtures. This is the `.filez` rule again: the
    /// bytes in tree are the oracle."
    ///
    /// These are the exact packets libpulse 16.1 sent to a logging stub over a
    /// Unix socket. The cookie bytes in `AUTH` are that run's real cookie and
    /// carry no meaning here — this server never reads one (§K.3 authenticates
    /// by `SO_PEERCRED` and parses the cookie only to consume its 256 bytes).
    mod captured {
        /// `AUTH`, tag 0. Note the version word: `0xc0000023`.
        pub const AUTH: &str = "\
4c000000084c000000004cc00000237800000100f75401b750382a5100436705420ed97d89cdd9ff\
0b417983479c5f9bb667199830a8f3b51a5fc5dc33cea7db91a4762dd367ca72236243516ff73009\
edde1e9e782eed5704e8707b06e35019a0238f501618d6f39c6d12518d55d0dd23f5341c823e0579\
81a45993b1056fe8de39f92b774919021a8fb2d8df71187982900daf76926c4d42209562e223c337\
3f5a6c3d34dda97dc92c15ce0f3647b23fd9a6007f7f66d4ea402ec30ddc0ee2685e4d3968a04895\
e0b2613f434ed18749b0756e1139aa7b879b6b2cf99afa9c3315529abb8cc619a26c7c0b30ed8d10\
f53729572f29b951922476138e139f679b8032523865ed4192343b4402926780fab24837";

        /// `SET_CLIENT_NAME`, tag 1 — one proplist and nothing else.
        pub const SET_CLIENT_NAME: &str = "\
4c000000094c0000000150746170706c69636174696f6e2e70726f636573732e6964004c00000006\
7800000006313230333500746170706c69636174696f6e2e70726f636573732e75736572004c0000\
000578000000057465737400746170706c69636174696f6e2e70726f636573732e686f7374004c00\
000007780000000774353730306700746170706c69636174696f6e2e70726f636573732e62696e61\
7279004c000000067800000006706163746c00746170706c69636174696f6e2e6e616d65004c0000\
00067800000006706163746c00746170706c69636174696f6e2e6c616e6775616765004c0000000b\
780000000b656e5f55532e7574663800746170706c69636174696f6e2e70726f636573732e6d6163\
68696e655f6964004c00000021780000002133613133666165633136653463316333313039326431\
61613631343565313565004e";

        /// `CREATE_PLAYBACK_STREAM`, tag 2 — sixteen booleans and a proplist.
        pub const CREATE_PLAYBACK_STREAM: &str = "\
4c000000034c000000026103020000bb806d0201024cffffffff4e4cffffffff304cffffffff4cff\
ffffff4cffffffff4c000000007602000100000001000030303030303030303050746d656469612e\
666f726d6174004c00000010780000001057415620284d6963726f736f66742900746170706c6963\
6174696f6e2e6e616d65004c0000000778000000077061706c617900746d656469612e6e616d6500\
4c000000067800000006742e776176004e303030303030304200";

        /// `GET_SERVER_INFO`, tag 2 — command and tag, and nothing else.
        pub const GET_SERVER_INFO: &str = "4c000000144c00000002";
        /// `GET_SINK_INFO_LIST`, tag 2.
        pub const GET_SINK_INFO_LIST: &str = "4c000000164c00000002";
        /// `GET_SOURCE_INFO_LIST`, tag 2.
        pub const GET_SOURCE_INFO_LIST: &str = "4c000000184c00000002";
        /// `SUBSCRIBE`, tag 2, mask `0x2ff`.
        pub const SUBSCRIBE: &str = "4c000000234c000000024c000002ff";
        /// `DRAIN_PLAYBACK_STREAM`, tag 3, channel 0.
        pub const DRAIN_PLAYBACK_STREAM: &str = "4c0000000c4c000000034c00000000";
        /// `DELETE_PLAYBACK_STREAM`, tag 4, channel 0.
        pub const DELETE_PLAYBACK_STREAM: &str = "4c000000044c000000044c00000000";
    }

    /// The captured `AUTH` decodes to exactly what §K.3 predicts, including the
    /// feature bits in the version word's high half.
    #[test]
    fn the_captured_auth_decodes_and_carries_feature_bits() {
        let bytes = unhex(captured::AUTH);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 8, "AUTH is command 8");
        assert_eq!(reader.u32().unwrap(), 0, "tag");
        let version = reader.u32().unwrap();
        assert_eq!(version, 0xc000_0023);
        // The trap: the raw word is over three billion, and its low half is 35.
        assert_eq!(version & 0xffff, 35);
        assert!(version > 35, "a raw min() would take this at face value");
        let cookie = reader.arbitrary().unwrap();
        assert_eq!(cookie.len(), 256, "the cookie is exactly 256 bytes");
        reader.finish().unwrap();
    }

    /// The captured `SET_CLIENT_NAME` is one proplist, and the packet ends
    /// there — which is what `finish` is for.
    #[test]
    fn the_captured_set_client_name_is_one_proplist() {
        let bytes = unhex(captured::SET_CLIENT_NAME);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 9);
        assert_eq!(reader.u32().unwrap(), 1);
        let properties = reader.proplist().unwrap();
        reader.finish().unwrap();
        let keys: Vec<&str> = properties.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"application.name"));
        assert!(keys.contains(&"application.process.binary"));
        let name = properties
            .iter()
            .find(|p| p.key == "application.name")
            .unwrap();
        // The counted bytes include the NUL libpulse writes.
        assert_eq!(name.value, b"pactl\0");
    }

    /// The whole `CREATE_PLAYBACK_STREAM` schema, read value by value against
    /// the captured bytes. This is the assertion that would fail on the two
    /// mistakes §K.3 names: sixteen booleans read as `B` would desynchronise
    /// the packet, and reading the cvolume as a `V` would too.
    #[test]
    fn the_captured_create_playback_stream_matches_the_version_35_schema() {
        let bytes = unhex(captured::CREATE_PLAYBACK_STREAM);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 3, "CREATE_PLAYBACK_STREAM");
        assert_eq!(reader.u32().unwrap(), 2, "tag");
        assert_eq!(
            reader.sample_spec().unwrap(),
            SampleSpec { format: SAMPLE_S16LE, channels: 2, rate: 48000 }
        );
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        assert_eq!(reader.u32().unwrap(), INVALID_INDEX, "sink index");
        assert_eq!(reader.string().unwrap(), None, "sink name is the NULL string");
        assert_eq!(reader.u32().unwrap(), u32::MAX, "maxlength");
        assert!(!reader.boolean().unwrap(), "corked");
        assert_eq!(reader.u32().unwrap(), u32::MAX, "tlength");
        assert_eq!(reader.u32().unwrap(), u32::MAX, "prebuf");
        assert_eq!(reader.u32().unwrap(), u32::MAX, "minreq");
        assert_eq!(reader.u32().unwrap(), 0, "syncid");
        assert_eq!(reader.cvolume().unwrap(), vec![0x10000, 0x10000]);
        // Nine booleans between the volume and the proplist.
        for index in 0..9 {
            assert!(!reader.boolean().unwrap(), "boolean {index} before the proplist");
        }
        let properties = reader.proplist().unwrap();
        assert!(properties.iter().any(|p| p.key == "media.name"));
        // Seven more after it.
        for index in 0..7 {
            assert!(!reader.boolean().unwrap(), "boolean {index} after the proplist");
        }
        assert_eq!(reader.u8().unwrap(), 0, "n_formats");
        reader.finish().unwrap();
    }

    /// Every captured packet starts with a command and a tag and is exhausted
    /// by its schema — including the three that carry nothing else.
    #[test]
    fn every_captured_packet_is_exhausted_by_its_schema() {
        for (name, hex, command, extra) in [
            ("GET_SERVER_INFO", captured::GET_SERVER_INFO, 20u32, 0usize),
            ("GET_SINK_INFO_LIST", captured::GET_SINK_INFO_LIST, 22, 0),
            ("GET_SOURCE_INFO_LIST", captured::GET_SOURCE_INFO_LIST, 24, 0),
            ("SUBSCRIBE", captured::SUBSCRIBE, 35, 1),
            ("DRAIN_PLAYBACK_STREAM", captured::DRAIN_PLAYBACK_STREAM, 12, 1),
            ("DELETE_PLAYBACK_STREAM", captured::DELETE_PLAYBACK_STREAM, 4, 1),
        ] {
            let bytes = unhex(hex);
            let mut reader = Reader::new(&bytes);
            assert_eq!(reader.u32().unwrap(), command, "{name} command number");
            reader.u32().unwrap();
            for _ in 0..extra {
                reader.u32().unwrap();
            }
            reader.finish().unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // SUBSCRIBE's argument is the event mask the client asked for.
        let bytes = unhex(captured::SUBSCRIBE);
        let mut reader = Reader::new(&bytes);
        reader.u32().unwrap();
        reader.u32().unwrap();
        assert_eq!(reader.u32().unwrap(), 0x2ff);
    }

    /// A schema that stops early is an error, not a partial success.
    #[test]
    fn a_packet_with_bytes_left_over_is_refused() {
        let bytes = unhex(captured::SUBSCRIBE);
        let mut reader = Reader::new(&bytes);
        reader.u32().unwrap();
        reader.u32().unwrap();
        // The mask was not read: three bytes short of exhaustion.
        let err = reader.finish().unwrap_err();
        assert!(matches!(err, Error::Trailing { left: 5, .. }), "{err:?}");
    }

    /// Round trip: everything the writer emits, the reader reads back.
    #[test]
    fn every_value_round_trips() {
        let mut writer = Writer::new();
        writer
            .u32(0xdead_beef)
            .u8(42)
            .u64(u64::MAX)
            .s64(-3)
            .usec(1_000_000)
            .timeval(7, 8)
            .volume(0x10000)
            .boolean(true)
            .boolean(false)
            .string("hello")
            .null_string()
            .string("")
            .arbitrary(&[1, 2, 3])
            .sample_spec(SampleSpec { format: SAMPLE_S16LE, channels: 2, rate: 48000 })
            .channel_map(&[1, 2])
            .cvolume(&[0x10000, 0x8000])
            .proplist(&[text_property("k", "v")])
            .format_info(1, &[]);
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 0xdead_beef);
        assert_eq!(reader.u8().unwrap(), 42);
        assert_eq!(reader.u64().unwrap(), u64::MAX);
        assert_eq!(reader.s64().unwrap(), -3);
        assert_eq!(reader.usec().unwrap(), 1_000_000);
        assert_eq!(reader.timeval().unwrap(), (7, 8));
        assert_eq!(reader.volume().unwrap(), 0x10000);
        assert!(reader.boolean().unwrap());
        assert!(!reader.boolean().unwrap());
        assert_eq!(reader.string().unwrap().as_deref(), Some("hello"));
        assert_eq!(reader.string().unwrap(), None);
        assert_eq!(reader.string().unwrap().as_deref(), Some(""));
        assert_eq!(reader.arbitrary().unwrap(), &[1, 2, 3]);
        assert_eq!(reader.sample_spec().unwrap().rate, 48000);
        assert_eq!(reader.channel_map().unwrap(), vec![1, 2]);
        assert_eq!(reader.cvolume().unwrap(), vec![0x10000, 0x8000]);
        assert_eq!(reader.proplist().unwrap().len(), 1);
        assert_eq!(reader.format_info().unwrap().0, 1);
        reader.finish().unwrap();
    }

    /// The NULL string and the empty string are DIFFERENT values, on the wire
    /// and after decoding. libpulse relies on it: a sink with no active port
    /// sends NULL, and a port literally named "" would be a different answer.
    #[test]
    fn the_null_string_is_not_the_empty_string() {
        let mut writer = Writer::new();
        writer.null_string().string("");
        let bytes = writer.into_bytes();
        assert_eq!(bytes, vec![b'N', b't', 0]);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.string().unwrap(), None);
        assert_eq!(reader.string().unwrap(), Some(String::new()));
    }

    /// A `B` where a boolean belongs desynchronises the packet — which is the
    /// failure §K.3 says a from-memory alphabet produces. Asserted so the
    /// distinction cannot be quietly removed.
    #[test]
    fn a_byte_is_not_a_boolean_and_a_volume_is_not_a_cvolume() {
        let mut writer = Writer::new();
        writer.u8(1).u32(7);
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        let err = reader.boolean().unwrap_err();
        assert!(matches!(err, Error::Unexpected { found: b'B', .. }), "{err:?}");

        let mut writer = Writer::new();
        writer.cvolume(&[0x10000]);
        let mut reader = Reader::new(writer.as_bytes());
        assert!(reader.volume().is_err(), "a `v` must not read as a `V`");
        let mut writer = Writer::new();
        writer.volume(0x10000);
        let mut reader = Reader::new(writer.as_bytes());
        assert!(reader.cvolume().is_err(), "a `V` must not read as a `v`");
    }

    /// The malformed corpus §K.3 asks for. Each entry is a packet that a
    /// permissive decoder would accept or hang on.
    #[test]
    fn the_malformed_corpus_is_refused_without_panicking() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", vec![]),
            ("a tag with no payload", vec![b'L']),
            ("a truncated u32", vec![b'L', 0, 0]),
            ("an unterminated string", vec![b't', b'h', b'i']),
            ("a non-UTF-8 string", vec![b't', 0xff, 0xfe, 0]),
            ("an unknown tag", vec![b'Z', 0, 0, 0, 0]),
            (
                "an arbitrary block claiming four gigabytes",
                vec![b'x', 0xff, 0xff, 0xff, 0xff],
            ),
            (
                "an arbitrary block longer than it is",
                vec![b'x', 0, 0, 0, 8, 1, 2],
            ),
            ("a cvolume with 255 channels", vec![b'v', 0xff]),
            ("a channel map with 255 channels", vec![b'm', 0xff]),
            ("a truncated sample spec", vec![b'a', 3, 2]),
            ("a proplist with no terminator", vec![b'P', b't', b'k', 0]),
            (
                "a proplist value that lies about its length",
                vec![b'P', b't', b'k', 0, b'L', 0, 0, 0, 9, b'x', 0, 0, 0, 1, 7, b'N'],
            ),
            ("a format info with no proplist", vec![b'f', b'B', 1]),
        ];
        for (name, bytes) in cases {
            let mut reader = Reader::new(&bytes);
            // Whatever the schema tried first, the walk must end in an error and
            // never in a panic or a loop.
            let outcome = reader
                .skip()
                .and_then(|()| reader.finish());
            assert!(outcome.is_err(), "{name} was accepted: {bytes:?}");
        }
    }

    /// Bounds are enforced on the DECLARED length, before anything is
    /// allocated: a client that claims four gigabytes gets an error, not an
    /// allocation attempt.
    #[test]
    fn the_bounds_are_checked_against_the_declared_length() {
        let mut over = vec![b'x'];
        over.extend_from_slice(&((ARBITRARY_MAX + 1) as u32).to_be_bytes());
        let mut reader = Reader::new(&over);
        assert!(matches!(
            reader.arbitrary().unwrap_err(),
            Error::TooLarge { what: "an arbitrary block", .. }
        ));

        // A proplist of many tiny entries: each value is inside its own bound,
        // so only the ENTRY bound stops this.
        let mut writer = Writer::new();
        let many: Vec<Property> = (0..PROPLIST_MAX + 1)
            .map(|index| text_property(&format!("k{index}"), "v"))
            .collect();
        writer.proplist(&many);
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        assert!(matches!(
            reader.proplist().unwrap_err(),
            Error::TooLarge { what: "a proplist", .. }
        ));
        // ...and exactly at the bound it is still accepted.
        let mut writer = Writer::new();
        writer.proplist(many.get(..PROPLIST_MAX).unwrap_or(&[]));
        let bytes = writer.into_bytes();
        assert_eq!(
            Reader::new(&bytes).proplist().unwrap().len(),
            PROPLIST_MAX
        );
    }

    /// A string longer than the bound is refused rather than allocated.
    #[test]
    fn an_over_long_string_is_refused() {
        let mut bytes = vec![b't'];
        bytes.extend(std::iter::repeat_n(b'a', STRING_MAX + 1));
        bytes.push(0);
        let mut reader = Reader::new(&bytes);
        assert!(matches!(
            reader.string().unwrap_err(),
            Error::TooLarge { what: "a string", .. }
        ));
    }

    /// `skip` walks past a value of any type without knowing what it means,
    /// which is the property that lets a schema tolerate a newer client's extra
    /// trailing field.
    #[test]
    fn skip_walks_past_any_value() {
        let mut writer = Writer::new();
        writer
            .proplist(&[text_property("a", "b")])
            .cvolume(&[1, 2, 3])
            .format_info(1, &[text_property("c", "d")])
            .string("last");
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        reader.skip().unwrap();
        reader.skip().unwrap();
        reader.skip().unwrap();
        assert_eq!(reader.string().unwrap().as_deref(), Some("last"));
        reader.finish().unwrap();
    }

    /// A key with an interior NUL cannot smuggle a second property past the
    /// decoder: the writer refuses to emit it as a string at all.
    #[test]
    fn an_interior_nul_does_not_split_a_string() {
        let mut writer = Writer::new();
        writer.string("a\0b");
        assert_eq!(writer.as_bytes(), b"N", "refused, not truncated");
    }
}
