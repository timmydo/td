//! A small binary encoding for a program's own messages: fixed-width
//! big-endian integers and length-prefixed bytes. The reader holds every
//! length to a bound its caller names, so a message from another machine
//! can make it hold no more than that, and refuses trailing bytes.

use crate::Error;

#[derive(Default)]
pub struct Writer(Vec<u8>);

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    /// A list's length, ahead of its items.
    pub fn count(&mut self, value: usize) {
        self.u64(u64::try_from(value).unwrap_or(u64::MAX));
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.count(value.len());
        self.0.extend_from_slice(value);
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn take(&mut self, size: usize) -> Result<&'a [u8], Error> {
        if size > self.0.len() {
            return Err(Error::Protocol("a message ended early".into()));
        }
        let (head, rest) = self.0.split_at(size);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Protocol("a message ended early".into()))
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64, Error> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// A list's length, at most `max`.
    pub fn count(&mut self, max: usize) -> Result<usize, Error> {
        let value = self.u64()?;
        usize::try_from(value)
            .ok()
            .filter(|value| *value <= max)
            .ok_or_else(|| Error::Protocol(format!("a length of {value} is past {max}")))
    }

    /// Length-prefixed bytes, at most `max`.
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], Error> {
        let size = self.count(max)?;
        self.take(size)
    }

    /// Length-prefixed UTF-8, at most `max` bytes.
    pub fn text(&mut self, max: usize) -> Result<&'a str, Error> {
        std::str::from_utf8(self.bytes(max)?)
            .map_err(|_| Error::Protocol("text that is not UTF-8".into()))
    }

    /// Refuses a message with bytes left over.
    pub fn finish(self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::Protocol("a message ran on past its end".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn values_round_trip_and_trailing_bytes_are_refused() {
        let mut w = Writer::new();
        w.u8(7);
        w.u32(70_000);
        w.u64(u64::MAX);
        w.i64(-5);
        w.count(2);
        w.bytes(b"\xffraw");
        w.bytes("text".as_bytes());
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u32().unwrap(), 70_000);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.i64().unwrap(), -5);
        assert_eq!(r.count(2).unwrap(), 2);
        assert_eq!(r.bytes(16).unwrap(), b"\xffraw");
        assert_eq!(r.text(16).unwrap(), "text");
        r.finish().unwrap();
        let mut extra = bytes.clone();
        extra.push(0);
        let mut r = Reader::new(&extra);
        r.u8().unwrap();
        r.u32().unwrap();
        r.u64().unwrap();
        r.i64().unwrap();
        r.count(2).unwrap();
        r.bytes(16).unwrap();
        r.text(16).unwrap();
        assert!(r.finish().is_err());
    }

    #[test]
    fn a_length_past_its_bound_or_the_message_is_refused_before_it_is_used() {
        let mut w = Writer::new();
        w.bytes(&[0u8; 8]);
        let bytes = w.into_bytes();
        assert!(Reader::new(&bytes).bytes(7).is_err());
        assert_eq!(Reader::new(&bytes).bytes(8).unwrap().len(), 8);
        // A length the message does not hold, however large a bound allows.
        let mut w = Writer::new();
        w.u64(u64::MAX);
        let huge = w.into_bytes();
        assert!(Reader::new(&huge).bytes(usize::MAX).is_err());
        let mut short = bytes.clone();
        short.pop();
        assert!(Reader::new(&short).bytes(8).is_err());
        assert!(Reader::new(b"\x01").u32().is_err());
    }

    #[test]
    fn text_must_be_utf8() {
        let mut w = Writer::new();
        w.bytes(b"\xff");
        let bytes = w.into_bytes();
        assert!(Reader::new(&bytes).text(8).is_err());
    }
}
