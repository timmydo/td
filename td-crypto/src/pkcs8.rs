//! The bounded PKCS#8 subset accepted by the signing-key facade.
use crate::Error;

pub(super) const MAX_LEN: usize = 150;
const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];

struct Der<'a>(&'a [u8]);
impl<'a> Der<'a> {
    fn byte(&mut self) -> Result<u8, Error> {
        let (&byte, rest) = self.0.split_first().ok_or(Error::Invalid)?;
        self.0 = rest;
        Ok(byte)
    }

    fn element(&mut self, tag: u8) -> Result<&'a [u8], Error> {
        if self.byte()? != tag {
            return Err(Error::Invalid);
        }
        let first = self.byte()?;
        let length = match first {
            0..=127 => usize::from(first),
            0x81 => {
                let length = self.byte()?;
                if length < 128 {
                    return Err(Error::Invalid);
                }
                usize::from(length)
            }
            _ => return Err(Error::Invalid),
        };
        let value = self.0.get(..length).ok_or(Error::Invalid)?;
        self.0 = self.0.get(length..).ok_or(Error::Invalid)?;
        Ok(value)
    }

    fn exact(&mut self, tag: u8, expected: &[u8]) -> Result<(), Error> {
        if self.element(tag)? != expected {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}

/// The generated shape: the curve in the outer algorithm only and the public
/// point inside, as AWS-LC, ring and OpenSSL write it.
pub(super) const GENERATED_LEN: usize = 138;
const GENERATED_HEAD: &[u8] = &[
    0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02,
    0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30, 0x6b, 0x02,
    0x01, 0x01, 0x04, 0x20,
];
const GENERATED_POINT: &[u8] = &[0xa1, 0x44, 0x03, 0x42, 0x00];

pub(super) fn write(scalar: &[u8; 32], public: &[u8; 65]) -> [u8; GENERATED_LEN] {
    let mut out = [0; GENERATED_LEN];
    let parts: [&[u8]; 4] = [GENERATED_HEAD, scalar, GENERATED_POINT, public];
    let mut at = 0;
    for part in parts {
        for (slot, byte) in out.iter_mut().skip(at).zip(part) {
            *slot = *byte;
        }
        at += part.len();
    }
    out
}

/// The scalar and optional embedded point of an admitted document, borrowed.
pub(super) struct Parts<'a> {
    pub(super) scalar: &'a [u8; 32],
    pub(super) public: Option<&'a [u8; 65]>,
}

pub(super) fn validate(bytes: &[u8]) -> Result<(), Error> {
    parse(bytes).map(|_| ())
}

// Structural admission only: the key loader checks the scalar's range and the
// embedded point against the one it derives. No borrowed secret is copied.
pub(super) fn parse(bytes: &[u8]) -> Result<Parts<'_>, Error> {
    if bytes.len() > MAX_LEN {
        return Err(Error::Invalid);
    }
    let mut document = Der(bytes);
    let mut info = Der(document.element(0x30)?);
    document.end()?;
    info.exact(0x02, &[0])?;
    let mut algorithm = Der(info.element(0x30)?);
    algorithm.exact(0x06, EC_PUBLIC_KEY)?;
    algorithm.exact(0x06, P256)?;
    algorithm.end()?;
    let mut wrapped = Der(info.element(0x04)?);
    info.end()?;
    let mut key = Der(wrapped.element(0x30)?);
    wrapped.end()?;
    key.exact(0x02, &[1])?;
    let scalar: &[u8; 32] = key.element(0x04)?.try_into().map_err(|_| Error::Invalid)?;
    if key.0.first() == Some(&0xa0) {
        let mut parameters = Der(key.element(0xa0)?);
        parameters.exact(0x06, P256)?;
        parameters.end()?;
    }
    let mut embedded = None;
    if key.0.first() == Some(&0xa1) {
        let mut public = Der(key.element(0xa1)?);
        let bits = public.element(0x03)?;
        let point = match bits.split_first() {
            Some((0, point)) if point.first() == Some(&4) => point,
            _ => return Err(Error::Invalid),
        };
        embedded = Some(point.try_into().map_err(|_| Error::Invalid)?);
        public.end()?;
    }
    key.end()?;
    Ok(Parts {
        scalar,
        public: embedded,
    })
}
