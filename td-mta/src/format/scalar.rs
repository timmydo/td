//! Borrowed bounded fields; discard the cursor/output after any error.
use super::Error;

pub struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    pub const fn remaining(&self) -> &'a [u8] {
        self.remaining
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let (value, rest) = self
            .remaining
            .split_at_checked(len)
            .ok_or(Error::Truncated)?;
        self.remaining = rest;
        Ok(value)
    }
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?.try_into().map_err(|_| Error::Truncated)
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        let [value] = self.fixed()?;
        Ok(value)
    }
    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.fixed()?))
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }
    pub fn u32_key(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.fixed()?))
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.fixed()?))
    }
    pub fn i64(&mut self) -> Result<i64, Error> {
        Ok(i64::from_le_bytes(self.fixed()?))
    }
    pub fn boolean(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidTag),
        }
    }
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], Error> {
        let len = usize::try_from(self.u32()?).map_err(|_| Error::Overflow)?;
        if len > max {
            return Err(Error::Limit);
        }
        self.take(len)
    }
    pub fn text(&mut self, max: usize) -> Result<&'a str, Error> {
        std::str::from_utf8(self.bytes(max)?).map_err(|_| Error::InvalidUtf8)
    }
    pub fn finish(self) -> Result<(), Error> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(Error::TrailingBytes)
        }
    }
}

pub struct Writer<'a> {
    bytes: Option<&'a mut [u8]>,
    used: usize,
}

impl<'a> Writer<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self {
            bytes: Some(bytes),
            used: 0,
        }
    }
    pub(super) fn measuring() -> Self {
        Self {
            bytes: None,
            used: 0,
        }
    }
    fn check_end(&self, end: usize) -> Result<(), Error> {
        if let Some(bytes) = self.bytes.as_ref() {
            bytes.get(self.used..end).ok_or(Error::OutputFull)?;
        }
        Ok(())
    }
    pub fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self.used.checked_add(bytes.len()).ok_or(Error::Overflow)?;
        self.check_end(end)?;
        if let Some(output) = self.bytes.as_mut() {
            let destination = output.get_mut(self.used..end).ok_or(Error::OutputFull)?;
            destination.copy_from_slice(bytes);
        }
        self.used = end;
        Ok(())
    }
    pub fn u8(&mut self, value: u8) -> Result<(), Error> {
        self.put(&[value])
    }
    pub fn u16(&mut self, value: u16) -> Result<(), Error> {
        self.put(&value.to_le_bytes())
    }
    pub fn u32(&mut self, value: u32) -> Result<(), Error> {
        self.put(&value.to_le_bytes())
    }
    pub fn u32_key(&mut self, value: u32) -> Result<(), Error> {
        self.put(&value.to_be_bytes())
    }
    pub fn u64(&mut self, value: u64) -> Result<(), Error> {
        self.put(&value.to_le_bytes())
    }
    pub fn i64(&mut self, value: i64) -> Result<(), Error> {
        self.put(&value.to_le_bytes())
    }
    pub fn boolean(&mut self, value: bool) -> Result<(), Error> {
        self.u8(u8::from(value))
    }
    pub fn bytes(&mut self, value: &[u8], max: usize) -> Result<(), Error> {
        if value.len() > max {
            return Err(Error::Limit);
        }
        let len = u32::try_from(value.len()).map_err(|_| Error::Overflow)?;
        let end = self
            .used
            .checked_add(4)
            .and_then(|n| n.checked_add(value.len()))
            .ok_or(Error::Overflow)?;
        self.check_end(end)?;
        self.u32(len)?;
        self.put(value)
    }
    pub fn text(&mut self, value: &str, max: usize) -> Result<(), Error> {
        self.bytes(value.as_bytes(), max)
    }
    pub const fn written(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn scalar_golden_bytes_are_architecture_independent() -> Result<(), Error> {
        // u16 0x1234, u32 0x12345678, u64 1, i64 -2, bool true, UTF-8 e-acute.
        let golden = [
            0x34, 0x12, 0x78, 0x56, 0x34, 0x12, 1, 0, 0, 0, 0, 0, 0, 0, 0xfe, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 1, 2, 0, 0, 0, 0xc3, 0xa9,
        ];
        let mut output = [0; 29];
        let mut writer = Writer::new(&mut output);
        writer.u16(0x1234)?;
        writer.u32(0x12345678)?;
        writer.u64(1)?;
        writer.i64(-2)?;
        writer.boolean(true)?;
        writer.text("é", 2)?;
        assert_eq!(writer.written(), golden.len());
        assert_eq!(output, golden);
        let mut reader = Reader::new(&golden);
        assert_eq!(reader.u16()?, 0x1234);
        assert_eq!(reader.u32()?, 0x12345678);
        assert_eq!(reader.u64()?, 1);
        assert_eq!(reader.i64()?, -2);
        assert!(reader.boolean()?);
        assert_eq!(reader.text(2)?, "é");
        reader.finish()
    }

    #[test]
    fn malformed_scalars_and_capacity_fail_without_panicking() {
        for len in 0..8 {
            assert_eq!(Reader::new(&[0; 8][..len]).u64(), Err(Error::Truncated));
        }
        assert_eq!(Reader::new(&[2]).boolean(), Err(Error::InvalidTag));
        assert_eq!(Reader::new(&[0xff; 4]).bytes(1024), Err(Error::Limit));
        assert_eq!(
            Reader::new(&[2, 0, 0, 0, 0xff, 0xff]).text(2),
            Err(Error::InvalidUtf8)
        );
        assert_eq!(Reader::new(&[1]).finish(), Err(Error::TrailingBytes));
        assert_eq!(Reader::new(&[]).take(usize::MAX), Err(Error::Truncated));
        let mut output = [7; 1];
        assert_eq!(Writer::new(&mut output).u64(1), Err(Error::OutputFull));
        assert_eq!(output, [7]);
        let mut output = [7; 5];
        let mut writer = Writer::new(&mut output);
        assert_eq!(writer.bytes(b"ab", 2), Err(Error::OutputFull));
        assert_eq!(writer.written(), 0);
        assert_eq!(output, [7; 5]);
    }
}
