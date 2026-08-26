pub const HEADER_SIZE: usize = 8;
pub const MAX_MESSAGE: usize = u16::MAX as usize;

#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub payload: Vec<u8>,
}

fn read_u32(bytes: &[u8]) -> Result<u32, String> {
    let raw: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated Wayland word".to_string())?
        .try_into()
        .map_err(|_| "truncated Wayland word".to_string())?;
    Ok(u32::from_ne_bytes(raw))
}

pub fn header(bytes: &[u8]) -> Result<Option<(u32, u16, usize)>, String> {
    if bytes.len() < HEADER_SIZE {
        return Ok(None);
    }
    let object = read_u32(
        bytes
            .get(..4)
            .ok_or_else(|| "truncated Wayland object id".to_string())?,
    )?;
    let word = read_u32(
        bytes
            .get(4..8)
            .ok_or_else(|| "truncated Wayland header".to_string())?,
    )?;
    let opcode = (word & 0xffff) as u16;
    let size = (word >> 16) as usize;
    if object == 0 {
        return Err("Wayland object id 0 is invalid".into());
    }
    if !(HEADER_SIZE..=MAX_MESSAGE).contains(&size) || !size.is_multiple_of(4) {
        return Err(format!("invalid Wayland message size {size}"));
    }
    Ok(Some((object, opcode, size)))
}

pub fn take(bytes: &mut Vec<u8>) -> Result<Option<Message>, String> {
    let Some((object, opcode, size)) = header(bytes)? else {
        return Ok(None);
    };
    if bytes.len() < size {
        return Ok(None);
    }
    let payload = bytes
        .get(HEADER_SIZE..size)
        .ok_or_else(|| "Wayland payload escaped receive buffer".to_string())?
        .to_vec();
    bytes.drain(..size);
    Ok(Some(Message {
        object,
        opcode,
        payload,
    }))
}

pub struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, offset: 0 }
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        let end = self
            .offset
            .checked_add(4)
            .ok_or_else(|| "Wayland cursor overflow".to_string())?;
        let value = read_u32(
            self.bytes
                .get(self.offset..end)
                .ok_or_else(|| "truncated Wayland u32".to_string())?,
        )?;
        self.offset = end;
        Ok(value)
    }

    pub fn i32(&mut self) -> Result<i32, String> {
        let value = self.u32()?;
        Ok(i32::from_ne_bytes(value.to_ne_bytes()))
    }

    pub fn string(&mut self) -> Result<String, String> {
        let length = self.u32()? as usize;
        if length == 0 {
            return Err("Wayland string has zero length".into());
        }
        self.string_of_length(length)
    }

    pub fn optional_string(&mut self) -> Result<Option<String>, String> {
        let length = self.u32()? as usize;
        if length == 0 {
            return Ok(None);
        }
        self.string_of_length(length).map(Some)
    }

    fn string_of_length(&mut self, length: usize) -> Result<String, String> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "Wayland string length overflow".to_string())?;
        let raw = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| "truncated Wayland string".to_string())?;
        if raw.last().copied() != Some(0) {
            return Err("Wayland string lacks its NUL terminator".into());
        }
        let text = raw
            .get(..raw.len().saturating_sub(1))
            .ok_or_else(|| "Wayland string underflow".to_string())?;
        let value =
            std::str::from_utf8(text).map_err(|e| format!("Wayland string is not UTF-8: {e}"))?;
        let padded = length
            .checked_add(3)
            .map(|sum| sum & !3)
            .ok_or_else(|| "Wayland string padding overflow".to_string())?;
        self.offset = self
            .offset
            .checked_add(padded)
            .ok_or_else(|| "Wayland cursor overflow".to_string())?;
        if self.offset > self.bytes.len() {
            return Err("truncated Wayland string padding".into());
        }
        Ok(value.to_string())
    }

    pub fn finish(self) -> Result<(), String> {
        if self.offset != self.bytes.len() {
            return Err(format!(
                "Wayland request has {} trailing bytes",
                self.bytes.len().saturating_sub(self.offset)
            ));
        }
        Ok(())
    }
}

pub struct Builder {
    bytes: Vec<u8>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder { bytes: Vec::new() }
    }

    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_ne_bytes());
    }

    pub fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_ne_bytes());
    }

    pub fn string(&mut self, value: &str) -> Result<(), String> {
        let length = value
            .len()
            .checked_add(1)
            .ok_or_else(|| "Wayland string length overflow".to_string())?;
        self.u32(u32::try_from(length).map_err(|_| "Wayland string is too long".to_string())?);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        Ok(())
    }

    pub fn array(&mut self, value: &[u8]) -> Result<(), String> {
        self.u32(u32::try_from(value.len()).map_err(|_| "Wayland array is too long".to_string())?);
        self.bytes.extend_from_slice(value);
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        Ok(())
    }

    pub fn message(self, object: u32, opcode: u16) -> Result<Vec<u8>, String> {
        if object == 0 {
            return Err("Wayland event object id 0 is invalid".into());
        }
        let size = self
            .bytes
            .len()
            .checked_add(HEADER_SIZE)
            .ok_or_else(|| "Wayland event size overflow".to_string())?;
        if size > MAX_MESSAGE || size % 4 != 0 {
            return Err(format!("Wayland event size {size} is invalid"));
        }
        let mut message = Vec::with_capacity(size);
        message.extend_from_slice(&object.to_ne_bytes());
        let header = (u32::try_from(size).map_err(|_| "Wayland event is too large".to_string())?
            << 16)
            | u32::from(opcode);
        message.extend_from_slice(&header.to_ne_bytes());
        message.extend_from_slice(&self.bytes);
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip_preserves_header_and_payload() {
        let mut builder = Builder::new();
        builder.u32(17);
        builder.string("wl_compositor").unwrap();
        let encoded = builder.message(2, 0).unwrap();
        let mut input = encoded.clone();
        let message = take(&mut input).unwrap().unwrap();
        assert_eq!(message.object, 2);
        assert_eq!(message.opcode, 0);
        let mut cursor = Cursor::new(&message.payload);
        assert_eq!(cursor.u32().unwrap(), 17);
        assert_eq!(cursor.string().unwrap(), "wl_compositor");
        cursor.finish().unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn parser_waits_for_a_complete_message() {
        let mut builder = Builder::new();
        builder.u32(1);
        let encoded = builder.message(1, 0).unwrap();
        let mut partial = encoded.get(..6).unwrap().to_vec();
        assert!(take(&mut partial).unwrap().is_none());
    }

    #[test]
    fn parser_rejects_invalid_sizes_and_strings() {
        let mut bad = Vec::new();
        bad.extend_from_slice(&1u32.to_ne_bytes());
        bad.extend_from_slice(&((7u32 << 16) | 1).to_ne_bytes());
        assert!(header(&bad).is_err());

        let mut cursor = Cursor::new(&[2, 0, 0, 0, b'x', b'y', 0, 0]);
        assert!(cursor.string().is_err());
    }

    #[test]
    fn nullable_strings_distinguish_null_from_empty_payloads() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&2u32.to_ne_bytes());
        bytes.extend_from_slice(b"x\0\0\0");
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(cursor.optional_string().unwrap(), None);
        assert_eq!(cursor.optional_string().unwrap(), Some("x".to_string()));
        cursor.finish().unwrap();
    }
}
