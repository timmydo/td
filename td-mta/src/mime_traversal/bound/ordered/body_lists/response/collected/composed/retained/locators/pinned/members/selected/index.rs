//! Index only td-generated fixed metadata; no request/source JSON parser.
use super::Error;
#[derive(Clone, Copy, Default)]
struct Span {
    start: u32,
    end: u32,
}
pub(in super::super) struct Index {
    spans: [Span; 9],
    mask: u16,
    position: u32,
    start: u32,
    field: u8,
    quoted: bool,
    escaped: bool,
    depth: u8,
    ready: bool,
}
const KEYS: [&[u8]; 9] = [
    b"\"partId\":",
    b"\"size\":",
    b"\"type\":",
    b"\"charset\":",
    b"\"name\":",
    b"\"disposition\":",
    b"\"cid\":",
    b"\"language\":",
    b"\"location\":",
];
impl Index {
    pub(in super::super) fn new(mask: u16, fragment: &[u8]) -> Result<Self, Error> {
        u32::try_from(fragment.len()).map_err(|_| Error::InvalidState)?;
        Ok(Self {
            spans: [Span::default(); 9],
            mask,
            position: 0,
            start: 0,
            field: 0,
            quoted: false,
            escaped: false,
            depth: 0,
            ready: false,
        })
    }
    pub(in super::super) fn ready(&self) -> bool {
        self.ready
    }
    pub(in super::super) fn remaining(&self, fragment: &[u8]) -> Result<usize, Error> {
        fragment
            .len()
            .checked_sub(usize::try_from(self.position).map_err(|_| Error::InvalidState)?)
            .ok_or(Error::InvalidState)
    }
    fn close(&mut self, fragment: &[u8]) -> Result<(), Error> {
        let field = usize::from(self.field);
        let key = KEYS.get(field).ok_or(Error::InvalidState)?;
        let start = usize::try_from(self.start).map_err(|_| Error::InvalidState)?;
        let end = usize::try_from(self.position).map_err(|_| Error::InvalidState)?;
        if !fragment
            .get(start..end)
            .ok_or(Error::InvalidState)?
            .starts_with(key)
        {
            return Err(Error::InvalidState);
        }
        *self.spans.get_mut(field).ok_or(Error::InvalidState)? = Span {
            start: self.start,
            end: self.position,
        };
        self.field = self.field.checked_add(1).ok_or(Error::InvalidState)?;
        Ok(())
    }
    pub(in super::super) fn scan(
        &mut self,
        segments: [&[u8]; 5],
        count: usize,
    ) -> Result<(), Error> {
        let fragment = segments.into_iter().next().ok_or(Error::InvalidState)?;
        if self.ready || count == 0 || count > 64 || count > self.remaining(fragment)? {
            return Err(Error::InvalidState);
        }
        for _ in 0..count {
            let byte = *fragment
                .get(usize::try_from(self.position).map_err(|_| Error::InvalidState)?)
                .ok_or(Error::InvalidState)?;
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.quoted = false;
                }
            } else {
                match byte {
                    b'"' => self.quoted = true,
                    b'[' if self.depth == 0 => self.depth = 1,
                    b']' if self.depth == 1 => self.depth = 0,
                    b',' if self.depth == 0 => {
                        self.close(fragment)?;
                        self.start = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                    }
                    b'[' | b']' | b'{' | b'}' => return Err(Error::InvalidState),
                    _ => {}
                }
            }
            self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
        }
        if self.remaining(fragment)? == 0 {
            if self.quoted || self.escaped || self.depth != 0 {
                return Err(Error::InvalidState);
            }
            self.close(fragment)?;
            if self.field != 9 {
                return Err(Error::InvalidState);
            }
            self.ready = true;
        }
        Ok(())
    }
    fn metadata<'s>(&self, field: usize, segments: [&'s [u8]; 5]) -> Result<&'s [u8], Error> {
        let span = self.spans.get(field).ok_or(Error::InvalidState)?;
        let start = usize::try_from(span.start).map_err(|_| Error::InvalidState)?;
        let end = usize::try_from(span.end).map_err(|_| Error::InvalidState)?;
        segments
            .into_iter()
            .next()
            .ok_or(Error::InvalidState)?
            .get(start..end)
            .ok_or(Error::InvalidState)
    }
    fn length(&self, field: usize, segments: [&[u8]; 5]) -> Result<usize, Error> {
        if field < 9 {
            return Ok(self.metadata(field, segments)?.len());
        }
        let mut total = 0usize;
        for (number, segment) in segments.into_iter().enumerate().skip(1) {
            let length = if number == 1 {
                segment.len().checked_sub(1).ok_or(Error::InvalidState)?
            } else {
                segment.len()
            };
            total = total.checked_add(length).ok_or(Error::InvalidState)?;
        }
        Ok(total)
    }
    pub(in super::super) fn total(&self, segments: [&[u8]; 5]) -> Result<usize, Error> {
        if !self.ready {
            return Err(Error::InvalidState);
        }
        let mut total = 0usize;
        let mut first = true;
        for field in 0..10 {
            if self.mask & (1u16 << field) == 0 {
                continue;
            }
            total = total
                .checked_add(usize::from(!first))
                .and_then(|sum| sum.checked_add(self.length(field, segments).ok()?))
                .ok_or(Error::InvalidState)?;
            first = false;
        }
        Ok(total)
    }
    pub(in super::super) fn byte_at(
        &self,
        mut offset: usize,
        segments: [&[u8]; 5],
    ) -> Result<u8, Error> {
        if !self.ready {
            return Err(Error::InvalidState);
        }
        let mut first = true;
        for field in 0..10 {
            if self.mask & (1u16 << field) == 0 {
                continue;
            }
            if !first {
                if offset == 0 {
                    return Ok(b',');
                }
                offset = offset.checked_sub(1).ok_or(Error::InvalidState)?;
            }
            first = false;
            let length = self.length(field, segments)?;
            if offset >= length {
                offset = offset.checked_sub(length).ok_or(Error::InvalidState)?;
                continue;
            }
            if field < 9 {
                return self
                    .metadata(field, segments)?
                    .get(offset)
                    .copied()
                    .ok_or(Error::InvalidState);
            }
            for (number, mut segment) in segments.into_iter().enumerate().skip(1) {
                if number == 1 {
                    segment = segment.get(1..).ok_or(Error::InvalidState)?;
                }
                if let Some(byte) = segment.get(offset) {
                    return Ok(*byte);
                }
                offset = offset
                    .checked_sub(segment.len())
                    .ok_or(Error::InvalidState)?;
            }
            return Err(Error::InvalidState);
        }
        Err(Error::InvalidState)
    }
}
