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

// Structural admission only: the provider checks scalar range, curve membership
// and consistency of the embedded public point. No borrowed secret is copied.
pub(super) fn validate(bytes: &[u8]) -> Result<(), Error> {
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
    if key.element(0x04)?.len() != 32 {
        return Err(Error::Invalid);
    }
    if key.0.first() == Some(&0xa0) {
        let mut parameters = Der(key.element(0xa0)?);
        parameters.exact(0x06, P256)?;
        parameters.end()?;
    }
    if key.0.first() == Some(&0xa1) {
        let mut public = Der(key.element(0xa1)?);
        let bits = public.element(0x03)?;
        if bits.len() != 66 || bits.get(..2) != Some(&[0, 4]) {
            return Err(Error::Invalid);
        }
        public.end()?;
    }
    key.end()
}
