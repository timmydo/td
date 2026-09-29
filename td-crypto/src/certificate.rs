//! Local certificate metadata; path and signature checks are separate.
use crate::{
    der::{bit_string, utc, Der},
    TlsError, VerificationFailure,
};

const SERVER_AUTH: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 3, 1];
const CLIENT_AUTH: &[u8] = &[0x2b, 6, 1, 5, 5, 7, 3, 2];
const MAX_EXTENSIONS: usize = 64;

pub(super) struct Certificate<'a> {
    pub tbs: &'a [u8],
    pub signature_algorithm: &'a [u8],
    pub signature: &'a [u8],
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    pub key_algorithm: &'a [u8],
    pub public_key: &'a [u8],
    pub not_before: u64,
    pub not_after: u64,
    pub ca: bool,
    pub path_length: Option<u64>,
    pub key_usage: Option<u16>,
    pub extended_usage: Option<u8>,
}

fn whole_sequence(bytes: &[u8]) -> Result<Der<'_>, TlsError> {
    let mut outer = Der(bytes);
    let sequence = outer.take(0x30)?;
    outer.end()?;
    Ok(Der(sequence.value))
}

fn positive(bytes: &[u8]) -> Result<u64, TlsError> {
    let first = bytes.first().copied().ok_or(TlsError::Invalid)?;
    if first & 0x80 != 0
        || (bytes.len() > 1 && first == 0 && bytes.get(1).is_some_and(|b| b & 0x80 == 0))
    {
        return Err(TlsError::Invalid);
    }
    let mut value = 0u64;
    for &byte in bytes {
        value = value
            .checked_mul(256)
            .and_then(|n| n.checked_add(u64::from(byte)))
            .ok_or(TlsError::Invalid)?;
    }
    Ok(value)
}

impl<'a> Certificate<'a> {
    pub fn rsa_size(&self) -> Result<(), TlsError> {
        if self.key_algorithm != crate::certificate_algorithms::RSA_KEY {
            return Ok(());
        }
        let mut key = whole_sequence(self.public_key)?;
        let modulus = key.take(0x02)?.value;
        let first = modulus.first().copied().ok_or(TlsError::Invalid)?;
        let modulus = if first == 0 {
            let tail = modulus.get(1..).ok_or(TlsError::Invalid)?;
            if !tail.first().is_some_and(|b| b & 0x80 != 0) {
                return Err(TlsError::Invalid);
            }
            tail
        } else {
            if first & 0x80 != 0 {
                return Err(TlsError::Invalid);
            }
            modulus
        };
        let first = modulus.first().copied().ok_or(TlsError::Invalid)?;
        let full = modulus.len().checked_sub(1).ok_or(TlsError::Invalid)?;
        let bits = full
            .checked_mul(8)
            .and_then(|n| n.checked_add(8 - first.leading_zeros() as usize))
            .ok_or(TlsError::Invalid)?;
        if !(2048..=8192).contains(&bits) || positive(key.take(0x02)?.value)? < 3 {
            return Err(TlsError::Invalid);
        }
        key.end()
    }

    pub fn parse(bytes: &'a [u8]) -> Result<Self, TlsError> {
        if bytes.len() > crate::CERTIFICATE_DER_CAPACITY {
            return Err(TlsError::Invalid);
        }
        let mut document = whole_sequence(bytes)?;
        let tbs = document.take(0x30)?;
        let signature_algorithm = document.take(0x30)?.value;
        let (signature, unused) = bit_string(document.take(0x03)?.value)?;
        if unused != 0 {
            return Err(TlsError::Invalid);
        }
        document.end()?;
        let mut body = Der(tbs.value);
        let mut version = Der(body.take(0xa0)?.value);
        if version.take(0x02)?.value != [2] {
            return Err(TlsError::Invalid);
        }
        version.end()?;
        // The backend owns serial-number compatibility and X.509 field syntax.
        body.take(0x02)?;
        if body.take(0x30)?.value != signature_algorithm {
            return Err(TlsError::Invalid);
        }
        let issuer = body.take(0x30)?.value;
        let mut validity = Der(body.take(0x30)?.value);
        let not_before = utc(validity.next()?)?;
        let not_after = utc(validity.next()?)?;
        validity.end()?;
        if not_before > not_after {
            return Err(TlsError::Invalid);
        }
        let subject = body.take(0x30)?.value;
        let mut spki = Der(body.take(0x30)?.value);
        let key_algorithm = spki.take(0x30)?.value;
        let (public_key, unused) = bit_string(spki.take(0x03)?.value)?;
        if unused != 0 {
            return Err(TlsError::Invalid);
        }
        spki.end()?;
        let mut certificate = Self {
            tbs: tbs.encoded,
            signature_algorithm,
            signature,
            issuer,
            subject,
            key_algorithm,
            public_key,
            not_before,
            not_after,
            ca: false,
            path_length: None,
            key_usage: None,
            extended_usage: None,
        };
        if !body.0.is_empty() {
            let mut extensions = whole_sequence(body.take(0xa3)?.value)?;
            let mut seen: [Option<&[u8]>; MAX_EXTENSIONS] = [None; MAX_EXTENSIONS];
            let mut count = 0usize;
            while !extensions.0.is_empty() {
                let mut extension = Der(extensions.take(0x30)?.value);
                let oid = extension.take(0x06)?.value;
                crate::der::oid(oid)?;
                if seen.iter().flatten().any(|previous| *previous == oid) {
                    return Err(TlsError::Invalid);
                }
                *seen.get_mut(count).ok_or(TlsError::Invalid)? = Some(oid);
                count = count.checked_add(1).ok_or(TlsError::Invalid)?;
                if extension.0.first() == Some(&0x01) && extension.take(0x01)?.value != [0xff] {
                    return Err(TlsError::Invalid);
                }
                let contents = extension.take(0x04)?.value;
                extension.end()?;
                match oid {
                    [0x55, 0x1d, 0x13] => certificate.basic_constraints(contents)?,
                    [0x55, 0x1d, 0x0f] => certificate.key_usage(contents)?,
                    [0x55, 0x1d, 0x25] => certificate.extended_usage(contents)?,
                    [0x55, 0x1d, 0x11] => subject_names(contents)?,
                    _ => {}
                }
            }
        }
        body.end()?;
        Ok(certificate)
    }

    fn basic_constraints(&mut self, bytes: &[u8]) -> Result<(), TlsError> {
        let mut contents = whole_sequence(bytes)?;
        if contents.0.first() == Some(&0x01) {
            if contents.take(0x01)?.value != [0xff] {
                return Err(TlsError::Invalid);
            }
            self.ca = true;
        }
        if !contents.0.is_empty() {
            if !self.ca {
                return Err(TlsError::Invalid);
            }
            self.path_length = Some(positive(contents.take(0x02)?.value)?);
        }
        contents.end()
    }

    fn key_usage(&mut self, bytes: &[u8]) -> Result<(), TlsError> {
        let mut contents = Der(bytes);
        let (bits, unused) = bit_string(contents.take(0x03)?.value)?;
        contents.end()?;
        // A DER named-bit list omits zero trailing bits and contains a set bit.
        let last = bits.last().copied().ok_or(TlsError::Invalid)?;
        if bits.len() > 2 || last == 0 || last.trailing_zeros() != u32::from(unused) {
            return Err(TlsError::Invalid);
        }
        let first = bits.first().copied().ok_or(TlsError::Invalid)?;
        let second = bits.get(1).copied().unwrap_or(0);
        if second & 0x7f != 0 {
            return Err(TlsError::Invalid);
        }
        self.key_usage = Some(u16::from_be_bytes([first, second]));
        Ok(())
    }

    fn extended_usage(&mut self, bytes: &[u8]) -> Result<(), TlsError> {
        let mut contents = whole_sequence(bytes)?;
        if contents.0.is_empty() {
            return Err(TlsError::Invalid);
        }
        let mut flags = 0;
        while !contents.0.is_empty() {
            let oid = contents.take(0x06)?.value;
            crate::der::oid(oid)?;
            if oid == SERVER_AUTH {
                flags |= 1;
            }
            if oid == CLIENT_AUTH {
                flags |= 2;
            }
        }
        self.extended_usage = Some(flags);
        Ok(())
    }

    pub fn valid_at(&self, now: u64) -> Result<(), TlsError> {
        if now < self.not_before {
            return Err(TlsError::Verification(VerificationFailure::NotYetValid));
        }
        if now > self.not_after {
            return Err(TlsError::Verification(VerificationFailure::Expired));
        }
        Ok(())
    }

    pub fn server_usage(&self, issuer: bool) -> Result<(), TlsError> {
        let required = if issuer { 0x0400 } else { 0x8000 };
        if self.ca != issuer
            || (!self.ca && self.key_usage.is_some_and(|bits| bits & 0x0400 != 0))
            || self.key_usage.is_some_and(|bits| bits & required == 0)
            || self.extended_usage.is_some_and(|flags| flags & 1 == 0)
        {
            return Err(TlsError::Verification(VerificationFailure::Usage));
        }
        Ok(())
    }
}

// Check every GeneralName envelope, even after a matching DNS entry. Backend
// matching can stop early; structured non-DNS names remain opaque here.
fn subject_names(bytes: &[u8]) -> Result<(), TlsError> {
    let mut names = whole_sequence(bytes)?;
    if names.0.is_empty() {
        return Err(TlsError::Invalid);
    }
    while !names.0.is_empty() {
        let name = names.next()?;
        match name.tag {
            0x81 | 0x82 | 0x86 if !name.value.is_empty() && name.value.is_ascii() => {}
            0x87 if matches!(name.value.len(), 4 | 16) => {}
            0x88 => crate::der::oid(name.value)?,
            0xa0 | 0xa3 | 0xa4 | 0xa5 => {}
            _ => return Err(TlsError::Invalid),
        }
    }
    Ok(())
}
