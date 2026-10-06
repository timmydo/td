//! Reusable caller-owned storage. Constructors and operations allocate nothing.
//! Callers budget storage and the work of copying/formatting; capacity is fixed.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    InvalidCount,
    Formatting,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Capacity => "bounded storage exhausted",
            Self::InvalidCount => "invalid bounded storage count",
            Self::Formatting => "bounded formatting failed",
        })
    }
}

impl std::error::Error for Error {}

/// Disjoint byte regions borrowed from an existing arena.
///
/// Allocations remain valid across later allocations. There is deliberately no
/// reset: drop all returned borrows, then borrow the backing storage again.
/// This is byte storage, with no alignment or typed-object allocation promise.
pub struct Arena<'a> {
    remaining: &'a mut [u8],
}

impl<'a> Arena<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        Self { remaining: storage }
    }

    pub fn remaining(&self) -> usize {
        self.remaining.len()
    }

    /// Returned bytes retain their previous contents; initialize before use.
    pub fn allocate(&mut self, length: usize) -> Result<&'a mut [u8], Error> {
        if length > self.remaining.len() {
            return Err(Error::Capacity);
        }
        let (allocated, tail) = std::mem::take(&mut self.remaining)
            .split_at_mut_checked(length)
            .ok_or(Error::InvalidCount)?;
        self.remaining = tail;
        Ok(allocated)
    }

    pub fn copy(&mut self, bytes: &[u8]) -> Result<&'a [u8], Error> {
        let allocated = self.allocate(bytes.len())?;
        allocated.copy_from_slice(bytes);
        Ok(allocated)
    }
}

/// Contiguous input/output window over initialized storage.
///
/// Consuming does not move bytes. Compact explicitly before a refill if needed;
/// append does not secretly compact. Clear/consume do not erase secret data.
pub struct WireBuffer<'a> {
    storage: &'a mut [u8],
    start: usize,
    end: usize,
}

impl<'a> WireBuffer<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        Self {
            storage,
            start: 0,
            end: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    pub fn readable(&self) -> Result<&[u8], Error> {
        self.storage
            .get(self.start..self.end)
            .ok_or(Error::InvalidCount)
    }

    /// After a read into this slice, produce exactly the count returned by I/O.
    pub fn spare_mut(&mut self) -> Result<&mut [u8], Error> {
        self.storage.get_mut(self.end..).ok_or(Error::InvalidCount)
    }

    pub fn produce(&mut self, count: usize) -> Result<(), Error> {
        let end = self.end.checked_add(count).ok_or(Error::InvalidCount)?;
        self.storage.get(self.end..end).ok_or(Error::InvalidCount)?;
        self.end = end;
        Ok(())
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self.end.checked_add(bytes.len()).ok_or(Error::Capacity)?;
        let output = self.storage.get_mut(self.end..end).ok_or(Error::Capacity)?;
        output.copy_from_slice(bytes);
        self.end = end;
        Ok(())
    }

    pub fn consume(&mut self, count: usize) -> Result<(), Error> {
        let start = self.start.checked_add(count).ok_or(Error::InvalidCount)?;
        if start > self.end {
            return Err(Error::InvalidCount);
        }
        self.start = start;
        if start == self.end {
            self.clear();
        }
        Ok(())
    }

    /// Moves unread bytes to the beginning. Returns bytes moved for work accounting.
    pub fn compact(&mut self) -> Result<usize, Error> {
        let length = self
            .end
            .checked_sub(self.start)
            .ok_or(Error::InvalidCount)?;
        if self.start == 0 {
            return Ok(0);
        }
        for destination in 0..length {
            let source = self
                .start
                .checked_add(destination)
                .ok_or(Error::InvalidCount)?;
            let byte = *self.storage.get(source).ok_or(Error::InvalidCount)?;
            *self
                .storage
                .get_mut(destination)
                .ok_or(Error::InvalidCount)? = byte;
        }
        self.start = 0;
        self.end = length;
        Ok(length)
    }

    pub fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }
}

/// UTF-8 output with atomic append/format operations and no truncation.
///
/// Failed formatting restores the visible length, not the overwritten tail.
/// Use only bounded, trusted formatting implementations: a user-defined Display
/// may itself allocate, loop, or panic outside this writer's control.
pub struct TextBuffer<'a> {
    wire: WireBuffer<'a>,
}

impl<'a> TextBuffer<'a> {
    pub fn new(storage: &'a mut [u8]) -> Self {
        Self {
            wire: WireBuffer::new(storage),
        }
    }

    pub fn as_str(&self) -> Result<&str, Error> {
        std::str::from_utf8(self.wire.readable()?).map_err(|_| Error::Formatting)
    }

    /// Visible output bytes, not a work counter for failed formatting attempts.
    pub fn len(&self) -> usize {
        self.wire.end
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_bytes(&self) -> Result<&[u8], Error> {
        self.wire.readable()
    }

    pub fn append(&mut self, text: &str) -> Result<(), Error> {
        self.wire.append(text.as_bytes())
    }

    pub fn format(&mut self, arguments: fmt::Arguments<'_>) -> Result<(), Error> {
        let previous = self.wire.end;
        let mut sink = FormatSink {
            output: &mut self.wire,
            capacity_error: false,
        };
        let result = fmt::write(&mut sink, arguments);
        let capacity_error = sink.capacity_error;
        if result.is_err() || capacity_error {
            self.wire.end = previous;
            return Err(if capacity_error {
                Error::Capacity
            } else {
                Error::Formatting
            });
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.wire.clear();
    }
}

struct FormatSink<'a, 'b> {
    output: &'a mut WireBuffer<'b>,
    capacity_error: bool,
}

impl fmt::Write for FormatSink<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.capacity_error || self.output.append(text.as_bytes()).is_err() {
            self.capacity_error = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_regions_coexist_and_failed_allocation_preserves_tail() -> Result<(), Error> {
        let mut storage = [0; 7];
        let mut arena = Arena::new(&mut storage);
        let first = arena.allocate(2)?;
        first.copy_from_slice(b"ab");
        assert_eq!(arena.allocate(usize::MAX), Err(Error::Capacity));
        let second = arena.copy(b"cdefg")?;
        assert_eq!(first, b"ab");
        assert_eq!(second, b"cdefg");
        assert_eq!(arena.remaining(), 0);
        assert!(arena.allocate(0)?.is_empty());
        assert_eq!(arena.allocate(1), Err(Error::Capacity));
        Ok(())
    }

    #[test]
    fn compaction_handles_every_overlap_and_exact_capacity() -> Result<(), Error> {
        for capacity in 0..32 {
            for consumed in 0..=capacity {
                let input: Vec<u8> = (0..capacity).map(|i| i as u8).collect();
                let mut storage = vec![0; capacity];
                let pointer = storage.as_ptr();
                let mut buffer = WireBuffer::new(&mut storage);
                buffer.append(&input)?;
                assert_eq!(buffer.append(b"x"), Err(Error::Capacity));
                assert_eq!(buffer.consume(usize::MAX), Err(Error::InvalidCount));
                buffer.consume(consumed)?;
                let moved = buffer.compact()?;
                assert_eq!(
                    moved,
                    if consumed == 0 {
                        0
                    } else {
                        capacity - consumed
                    }
                );
                assert_eq!(
                    buffer.readable()?,
                    input.get(consumed..).ok_or(Error::InvalidCount)?
                );
                buffer.append(&vec![b'x'; consumed])?;
                assert_eq!(buffer.readable()?.len(), capacity);
                buffer.consume(capacity)?;
                assert_eq!(buffer.spare_mut()?.len(), capacity);
                assert_eq!(buffer.capacity(), capacity);
                assert_eq!(buffer.storage.as_ptr(), pointer);
            }
        }
        Ok(())
    }

    #[test]
    fn direct_io_counts_and_refill_are_checked() -> Result<(), Error> {
        let mut bytes = [0; 4];
        let mut buffer = WireBuffer::new(&mut bytes);
        buffer.spare_mut()?.copy_from_slice(b"abcd");
        assert_eq!(buffer.produce(5), Err(Error::InvalidCount));
        buffer.produce(4)?;
        assert_eq!(buffer.produce(usize::MAX), Err(Error::InvalidCount));
        buffer.consume(2)?;
        assert_eq!(buffer.append(b"ef"), Err(Error::Capacity));
        buffer.compact()?;
        buffer.append(b"ef")?;
        assert_eq!(buffer.readable()?, b"cdef");
        Ok(())
    }

    struct FailedDisplay;
    impl fmt::Display for FailedDisplay {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("partial")?;
            Err(fmt::Error)
        }
    }

    struct SwallowedError;
    impl fmt::Display for SwallowedError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let _ = f.write_str("too long for the entire buffer");
            Ok(())
        }
    }

    #[test]
    fn utf8_and_format_failures_are_atomic() -> Result<(), Error> {
        let mut storage = [0xff; 16];
        let mut output = TextBuffer::new(&mut storage);
        output.append("é:")?;
        assert_eq!(output.len(), 3);
        assert!(!output.is_empty());
        assert_eq!(output.as_bytes()?, b"\xc3\xa9:");
        let too_long = "1234567890123456";
        assert_eq!(
            output.format(format_args!("{too_long}")),
            Err(Error::Capacity)
        );
        assert_eq!(output.as_str()?, "é:");
        assert_eq!(
            output.format(format_args!("prefix:{too_long}")),
            Err(Error::Capacity)
        );
        assert_eq!(output.as_str()?, "é:");
        assert_eq!(
            output.format(format_args!("{FailedDisplay}")),
            Err(Error::Formatting)
        );
        assert_eq!(output.as_str()?, "é:");
        assert_eq!(
            output.format(format_args!("{SwallowedError}")),
            Err(Error::Capacity)
        );
        assert_eq!(output.as_str()?, "é:");
        output.format(format_args!("{} {}", 42, "🦀"))?;
        assert_eq!(output.as_str()?, "é:42 🦀");
        assert_eq!(output.append("🦀🦀"), Err(Error::Capacity));
        assert_eq!(output.as_str()?, "é:42 🦀");
        output.clear();
        assert!(output.is_empty());
        assert_eq!(output.as_bytes()?, b"");
        output.append("1234567890123456")?;
        assert_eq!(output.as_str()?, "1234567890123456");
        Ok(())
    }
}
