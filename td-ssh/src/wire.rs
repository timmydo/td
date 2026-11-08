//! RFC 4251 section 5 encodings, bounded and panic-free.

use crate::error::{Error, Result};

/// Cursor over untrusted bytes. Every read that runs out reports it.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.buf.len() {
            return Err(Error::protocol("truncated message"));
        }
        let (head, tail) = self.buf.split_at(len);
        self.buf = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| Error::protocol("truncated message"))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        let array: [u8; 4] = bytes
            .try_into()
            .map_err(|_| Error::protocol("truncated message"))?;
        Ok(u32::from_be_bytes(array))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let array: [u8; 8] = bytes
            .try_into()
            .map_err(|_| Error::protocol("truncated message"))?;
        Ok(u64::from_be_bytes(array))
    }

    pub fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn str(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.string()?).map_err(|_| Error::protocol("string is not UTF-8"))
    }

    /// A fixed-size binary string, as key and signature material is carried.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.string()?
            .try_into()
            .map_err(|_| Error::protocol("field has the wrong length"))
    }

    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        self.take(len)
    }

    pub fn name_list(&mut self) -> Result<Vec<&'a str>> {
        let text = self.str()?;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        Ok(text.split(',').collect())
    }

    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn finish(&self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(Error::protocol("trailing bytes in message"))
        }
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    /// For what holds a secret: growing would leave the old copies behind, unwiped.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    pub fn message(number: u8) -> Self {
        let mut writer = Self::new();
        writer.u8(number);
        writer
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Strings in this protocol never approach 4 GiB: packets are bounded far
    /// below it, so the length saturates rather than failing a builder chain.
    pub fn string(&mut self, bytes: &[u8]) -> &mut Self {
        self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        self.raw(bytes)
    }

    pub fn str(&mut self, text: &str) -> &mut Self {
        self.string(text.as_bytes())
    }

    pub fn name_list(&mut self, names: &[&str]) -> &mut Self {
        self.str(&names.join(","))
    }

    /// An unsigned big-endian value as an `mpint`: minimal, with a zero byte
    /// when the top bit would read as a sign.
    pub fn mpint(&mut self, value: &[u8]) -> &mut Self {
        let start = value.iter().position(|b| *b != 0).unwrap_or(value.len());
        let minimal = value.get(start..).unwrap_or_default();
        match minimal.first() {
            Some(first) if first & 0x80 != 0 => {
                self.u32(u32::try_from(minimal.len() + 1).unwrap_or(u32::MAX));
                self.u8(0);
                self.raw(minimal)
            }
            _ => self.string(minimal),
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// The bytes, to be wiped however their scope ends.
    pub fn into_wiped(self) -> Wiped {
        Wiped(self.buf)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// Secret bytes on the heap, wiped however their scope ends. Only what the vector
/// holds now: a copy it left behind by growing is not, so what fills one is sized
/// first (`Writer::with_capacity`).
pub struct Wiped(pub(crate) Vec<u8>);

impl Drop for Wiped {
    fn drop(&mut self) {
        td_primitives::ct::zero(&mut self.0);
    }
}

impl std::fmt::Debug for Wiped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wiped({} bytes)", self.0.len())
    }
}

impl std::ops::Deref for Wiped {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    impl super::Writer {
        pub(crate) fn capacity(&self) -> usize {
            self.buf.capacity()
        }
    }

    #[test]
    fn mpint_follows_rfc4251_examples() {
        let encode = |value: &[u8]| {
            let mut writer = Writer::new();
            writer.mpint(value);
            writer.into_vec()
        };
        assert_eq!(encode(&[0]), [0, 0, 0, 0]);
        assert_eq!(encode(&[0, 0x09, 0xa3]), [0, 0, 0, 2, 0x09, 0xa3]);
        assert_eq!(encode(&[0x80]), [0, 0, 0, 2, 0, 0x80]);
    }

    #[test]
    fn reads_refuse_a_length_past_the_end() {
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 1, 2]);
        assert!(reader.string().is_err());
        let mut reader = Reader::new(&[0, 0]);
        assert!(reader.u32().is_err());
        assert!(Reader::new(&[]).u8().is_err());
    }

    #[test]
    fn array_refuses_a_wrong_length() {
        let mut writer = Writer::new();
        writer.string(&[1, 2, 3]);
        let bytes = writer.into_vec();
        assert!(Reader::new(&bytes).array::<4>().is_err());
        assert_eq!(Reader::new(&bytes).array::<3>().unwrap(), [1, 2, 3]);
    }

    #[test]
    fn name_lists_round_trip_and_empty_is_empty() {
        let mut writer = Writer::new();
        writer.name_list(&["a", "b"]).name_list(&[]);
        let bytes = writer.into_vec();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.name_list().unwrap(), ["a", "b"]);
        assert!(reader.name_list().unwrap().is_empty());
        assert!(reader.finish().is_ok());
    }
}
