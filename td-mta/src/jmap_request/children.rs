//! Bounded direct-child walks over retained, producer-validated I-JSON.
use super::dispatch::Error;
use std::ops::Range;
use td_json::scan;

/// Trusted retained reader. Reads must fill the whole slice or fail, and both
/// methods charge the original work/deadline budget. Never renew that budget.
pub(crate) trait Source {
    fn check(&mut self) -> Result<(), Error>;
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error>;
}

// Keep one bounded key inline instead of allocating for every child.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Progress {
    Pending,
    Item(Item),
    Complete,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Name {
    bytes: [u8; 256],
    length: usize,
}
impl Default for Name {
    fn default() -> Self {
        Self::new()
    }
}
impl Name {
    pub(crate) fn new() -> Self {
        Self {
            bytes: [0; 256],
            length: 0,
        }
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes.get(..self.length).unwrap_or_default()
    }
    pub(crate) fn push(&mut self, value: &[u8]) -> Result<(), Error> {
        let end = self
            .length
            .checked_add(value.len())
            .ok_or(Error::InvalidArguments)?;
        self.bytes
            .get_mut(self.length..end)
            .ok_or(Error::InvalidArguments)?
            .copy_from_slice(value);
        self.length = end;
        Ok(())
    }
    pub(crate) fn wire_id(&self) -> Result<crate::wire::Id<'_>, Error> {
        let text = std::str::from_utf8(self.bytes()).map_err(|_| Error::InvalidArguments)?;
        crate::wire::Id::parse(text).map_err(|_| Error::InvalidArguments)
    }
    pub(crate) fn id(&self) -> Result<(), Error> {
        self.wire_id().map(|_| ())
    }
}
pub(crate) struct Item {
    pub(crate) key: Name,
    pub(crate) kind: scan::Kind,
    pub(crate) range: Range<u64>,
}
/// Walk only direct children, retaining no subtree. The producer already
/// guarantees unique I-JSON; a malformed retained source is an internal error.
pub(crate) struct Children {
    range: Range<u64>,
    scanner: scan::Scanner,
    buffer: [u8; 4096],
    begin: usize,
    end: usize,
    read: u64,
    depth: usize,
    expected: scan::Kind,
    key: Name,
    child: Option<(scan::Kind, u64)>,
    complete: bool,
    failed: Option<Error>,
}
impl Children {
    pub(crate) fn new(range: Range<u64>, expected: scan::Kind) -> Result<Self, Error> {
        if !matches!(expected, scan::Kind::Object | scan::Kind::Array) {
            return Err(Error::State);
        }
        let length = range.end.checked_sub(range.start).ok_or(Error::State)?;
        Ok(Self {
            scanner: scan::Scanner::new(length, scan::MAX_DEPTH).map_err(|_| Error::State)?,
            read: range.start,
            range,
            buffer: [0; 4096],
            begin: 0,
            end: 0,
            depth: 0,
            expected,
            key: Name::new(),
            child: None,
            complete: false,
            failed: None,
        })
    }
    /// Consume at most one scanner turn (4096 bytes), with at most one read.
    /// Pending is progress, never the end of a container. Failures are sticky.
    pub(crate) fn poll(&mut self, source: &mut impl Source) -> Result<Progress, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.step(source);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    /// Convenience for consumers that schedule/check within the source calls.
    pub(crate) fn next(&mut self, source: &mut impl Source) -> Result<Option<Item>, Error> {
        loop {
            match self.poll(source)? {
                Progress::Pending => (),
                Progress::Item(item) => return Ok(Some(item)),
                Progress::Complete => return Ok(None),
            }
        }
    }
    fn step(&mut self, source: &mut impl Source) -> Result<Progress, Error> {
        source.check()?;
        if self.complete {
            return Ok(Progress::Complete);
        }
        if self.begin == self.end && self.read < self.range.end {
            self.end = (self.range.end - self.read).min(self.buffer.len() as u64) as usize;
            self.begin = 0;
            source.read(
                self.read,
                self.buffer.get_mut(..self.end).ok_or(Error::State)?,
            )?;
            self.read = self.read.checked_add(self.end as u64).ok_or(Error::State)?;
        }
        let input = self.buffer.get(self.begin..self.end).ok_or(Error::State)?;
        let next = self
            .scanner
            .poll(input, self.read == self.range.end)
            .map_err(|_| Error::State)?;
        if next.consumed == 0 && next.event.is_none() && !next.complete {
            return Err(Error::State);
        }
        self.begin = self.begin.checked_add(next.consumed).ok_or(Error::State)?;
        self.complete = next.complete;
        let mut result = Progress::Pending;
        match next.event {
            Some(scan::Event::Start { kind, offset }) => {
                if self.depth == 0 && kind != self.expected {
                    return Err(Error::InvalidArguments);
                }
                if self.depth == 1 {
                    self.child = Some((kind, offset));
                }
                self.depth = self.depth.checked_add(1).ok_or(Error::State)?;
            }
            Some(scan::Event::End { kind, offset }) => {
                self.depth = self.depth.checked_sub(1).ok_or(Error::State)?;
                if self.depth == 1 {
                    let (first, start) = self.child.take().ok_or(Error::State)?;
                    if first != kind {
                        return Err(Error::State);
                    }
                    result = Progress::Item(Item {
                        key: std::mem::take(&mut self.key),
                        kind,
                        range: self.range.start.checked_add(start).ok_or(Error::State)?
                            ..self.range.start.checked_add(offset).ok_or(Error::State)?,
                    });
                }
            }
            Some(scan::Event::KeyStart) if self.depth == 1 => self.key = Name::new(),
            Some(scan::Event::KeyChar(value)) if self.depth == 1 => {
                let mut bytes = [0; 4];
                self.key.push(value.encode_utf8(&mut bytes).as_bytes())?;
            }
            _ => (),
        }
        source.check()?;
        if self.complete {
            if matches!(result, Progress::Item(_)) {
                return Err(Error::State);
            }
            Ok(Progress::Complete)
        } else {
            Ok(result)
        }
    }
}

#[cfg(test)]
#[path = "children_tests.rs"]
mod tests;
