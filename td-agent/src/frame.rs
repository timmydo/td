//! Bounded, length-framed messages over a byte stream: a four-byte
//! big-endian length, then that many bytes. The window process and each
//! conversation process speak it over their socketpair (DESIGN.md §2);
//! a length past `MAX_FRAME` is refused before anything is allocated
//! for it, so neither side can make the other hold more than that.

use std::io::{self, Read, Write};

/// The largest payload either side sends or accepts: room for the
/// longest message text (`protocol::MAX_TEXT`) after JSON escaping.
pub const MAX_FRAME: usize = 1 << 20;

/// Why a frame could not be read.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The header named a payload past `MAX_FRAME`.
    TooLong(u64),
    /// The stream ended inside a frame.
    Torn,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::TooLong(n) => {
                write!(f, "a frame of {n} bytes is past the {MAX_FRAME}-byte bound")
            }
            Self::Torn => f.write_str("the stream ended inside a frame"),
        }
    }
}

/// Writes one frame whole: its header and payload in a single write, so
/// a reader never sees a header without the bytes it promises unless the
/// stream breaks.
pub fn write<W: Write + ?Sized>(stream: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a frame of {} bytes is past the bound", payload.len()),
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame length"))?;
    let mut bytes = Vec::with_capacity(payload.len().saturating_add(4));
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(payload);
    stream.write_all(&bytes)?;
    stream.flush()
}

/// The next frame, or `None` when the stream ends cleanly between frames.
pub fn read(stream: &mut impl Read) -> Result<Option<Vec<u8>>, Error> {
    let mut head = [0u8; 4];
    let mut got = 0usize;
    while let Some(rest) = head.get_mut(got..).filter(|rest| !rest.is_empty()) {
        match stream.read(rest) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::Torn),
            Ok(n) => got = got.saturating_add(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    let length = u32::from_be_bytes(head);
    let size = usize::try_from(length).map_err(|_| Error::TooLong(u64::from(length)))?;
    if size > MAX_FRAME {
        return Err(Error::TooLong(u64::from(length)));
    }
    let mut payload = vec![0u8; size];
    match stream.read_exact(&mut payload) {
        Ok(()) => Ok(Some(payload)),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::Torn),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn frames_round_trip_in_order_and_end_cleanly() {
        let mut wire = Vec::new();
        write(&mut wire, b"one").unwrap();
        write(&mut wire, b"").unwrap();
        write(&mut wire, &vec![7u8; MAX_FRAME]).unwrap();
        let mut reader = wire.as_slice();
        assert_eq!(read(&mut reader).unwrap().unwrap(), b"one");
        assert_eq!(read(&mut reader).unwrap().unwrap(), b"");
        assert_eq!(read(&mut reader).unwrap().unwrap().len(), MAX_FRAME);
        assert!(read(&mut reader).unwrap().is_none());
    }

    #[test]
    fn a_frame_past_the_bound_is_refused_on_both_sides() {
        let mut wire = Vec::new();
        assert!(write(&mut wire, &vec![0u8; MAX_FRAME + 1]).is_err());
        assert!(wire.is_empty(), "nothing written for a refused frame");
        let header = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        match read(&mut header.as_slice()) {
            Err(Error::TooLong(n)) => assert_eq!(n, (MAX_FRAME + 1) as u64),
            other => panic!("{other:?}"),
        }
        // The largest header is refused without allocating for it.
        assert!(matches!(
            read(&mut u32::MAX.to_be_bytes().as_slice()),
            Err(Error::TooLong(_))
        ));
    }

    #[test]
    fn a_stream_ending_inside_a_frame_is_torn() {
        let mut wire = Vec::new();
        write(&mut wire, b"payload").unwrap();
        for cut in 1..wire.len() {
            let mut reader = wire.get(..cut).unwrap();
            assert!(matches!(read(&mut reader), Err(Error::Torn)), "cut {cut}");
        }
    }

    /// A reader handed one byte at a time still assembles the frame.
    #[test]
    fn short_reads_assemble_a_frame() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let (Some(first), Some(slot)) = (self.0.first(), buf.first_mut()) else {
                    return Ok(0);
                };
                *slot = *first;
                self.0 = self.0.get(1..).unwrap_or_default();
                Ok(1)
            }
        }
        let mut wire = Vec::new();
        write(&mut wire, b"slow").unwrap();
        assert_eq!(read(&mut Trickle(&wire)).unwrap().unwrap(), b"slow");
    }
}
