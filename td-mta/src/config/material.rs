//! Bounded operator-file bytes. No filesystem trust or runtime authority.
use std::{fmt, io};

pub use super::identity::MAX_SIGNATURE_BYTES;
pub const MAX_PASSWORD_BYTES: usize = 1024;
/// Includes room for one terminal CRLF and the over-limit observation byte.
pub const PASSWORD_SCRATCH_BYTES: usize = MAX_PASSWORD_BYTES + 3;
pub const SIGNATURE_SCRATCH_BYTES: usize = MAX_SIGNATURE_BYTES + 1;
pub const MAX_INTERRUPTED_READS: u32 = super::stream::MAX_INTERRUPTED_READS;
const _: [(); 1] = [(); (SIGNATURE_SCRATCH_BYTES <= super::stream::SCRATCH_BYTES) as usize];
const _: [(); 1] = [(); (PASSWORD_SCRATCH_BYTES <= super::stream::SCRATCH_BYTES) as usize];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Signature,
    RelayPassword,
}
impl Kind {
    pub const fn scratch_bytes(self) -> usize {
        match self {
            Self::Signature => SIGNATURE_SCRATCH_BYTES,
            Self::RelayPassword => PASSWORD_SCRATCH_BYTES,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Capacity,
    TooLong,
    Utf8,
    Nul,
    Password,
    Read(io::ErrorKind),
    InterruptedLimit,
    InvalidReadCount,
    Invariant,
}
impl Error {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capacity => "config_material_capacity",
            Self::TooLong => "config_material_too_long",
            Self::Utf8 => "config_material_utf8",
            Self::Nul => "config_material_nul",
            Self::Password => "config_material_password",
            Self::Read(_) => "config_material_read",
            Self::InterruptedLimit => "config_material_interrupted_limit",
            Self::InvalidReadCount => "config_material_invalid_read_count",
            Self::Invariant => "config_material_invariant",
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
impl std::error::Error for Error {}

/// Decoded bytes following EOF, not proof of protected-file permission checks.
pub struct Value<'a> {
    kind: Kind,
    text: &'a str,
}
impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConfigurationMaterial(<redacted>)")
    }
}
impl<'a> Value<'a> {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    /// Explicit access for trusted finalization; never use in diagnostics.
    pub fn text(&self) -> &'a str {
        self.text
    }
}

/// Reads through EOF before returning a value. Short scratch refuses before I/O.
/// The trusted reader must truthfully report EOF/counts and initialize every
/// returned byte; it also owns blocking and allocation behavior.
/// No file is opened here. Scratch may retain input on success or failure;
/// this API does not promise zeroization or authorize authentication/publication.
pub fn read<'a, R: io::Read + ?Sized>(
    kind: Kind,
    reader: &mut R,
    scratch: &'a mut [u8],
) -> Result<Value<'a>, Error> {
    let scratch = scratch
        .get_mut(..kind.scratch_bytes())
        .ok_or(Error::Capacity)?;
    // A faulty reader must not turn a prior password into signature text.
    scratch.fill(0);
    let limit = scratch.len().checked_sub(1).ok_or(Error::Invariant)?;
    let mut used = 0usize;
    let mut interrupted = 0u32;
    loop {
        let window = scratch.get_mut(used..).ok_or(Error::Invariant)?;
        let count = match reader.read(window) {
            Ok(count) => count,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                interrupted = interrupted.checked_add(1).ok_or(Error::InterruptedLimit)?;
                if interrupted > MAX_INTERRUPTED_READS {
                    return Err(Error::InterruptedLimit);
                }
                continue;
            }
            Err(e) => return Err(Error::Read(e.kind())),
        };
        if count > window.len() {
            return Err(Error::InvalidReadCount);
        }
        if count == 0 {
            break;
        }
        used = used.checked_add(count).ok_or(Error::Invariant)?;
        if used > limit {
            return Err(Error::TooLong);
        }
    }
    let bytes = scratch.get(..used).ok_or(Error::Invariant)?;
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Utf8)?;
    if text.as_bytes().contains(&0) {
        return Err(Error::Nul);
    }
    let text = match kind {
        Kind::Signature => text,
        Kind::RelayPassword => {
            let text = text
                .strip_suffix("\r\n")
                .or_else(|| text.strip_suffix('\n'))
                .unwrap_or(text);
            if text.len() > MAX_PASSWORD_BYTES {
                return Err(Error::TooLong);
            }
            if text.is_empty() || !text.bytes().all(|byte| (32..=126).contains(&byte)) {
                return Err(Error::Password);
            }
            text
        }
    };
    Ok(Value { kind, text })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    struct Reader<'a> {
        bytes: &'a [u8],
        chunk: usize,
        calls: usize,
        ends: usize,
        end_error: Option<io::ErrorKind>,
    }
    impl io::Read for Reader<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            assert!(!out.is_empty());
            if self.bytes.is_empty() {
                self.ends += 1;
                return self.end_error.map_or(Ok(0), |kind| Err(kind.into()));
            }
            let count = self.bytes.len().min(self.chunk).min(out.len());
            out[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            Ok(count)
        }
    }
    fn reader(bytes: &[u8], chunk: usize) -> Reader<'_> {
        Reader {
            bytes,
            chunk,
            calls: 0,
            ends: 0,
            end_error: None,
        }
    }
    #[test]
    fn signatures_preserve_exact_text_through_eof_at_every_chunk_size() {
        let source = "\u{feff}é\r\n$HOME\n<html>\ttext</html>\r";
        let mut scratch = vec![0xa5; SIGNATURE_SCRATCH_BYTES + 8];
        for bytes in [source.as_bytes(), b"".as_slice()] {
            for chunk in 1..=source.len() + 1 {
                let mut input = reader(bytes, chunk);
                let value = read(Kind::Signature, &mut input, &mut scratch).unwrap();
                assert_eq!(value.kind(), Kind::Signature);
                assert_eq!(value.text().as_bytes(), bytes);
                assert_eq!(input.ends, 1);
                assert_eq!(&scratch[SIGNATURE_SCRATCH_BYTES..], &[0xa5; 8]);
            }
        }
    }
    #[test]
    fn signature_limits_invalid_encoding_and_nul_refuse_without_partial_value() {
        let mut scratch = vec![0; SIGNATURE_SCRATCH_BYTES];
        let max = "é".repeat(MAX_SIGNATURE_BYTES / 2);
        let value = read(
            Kind::Signature,
            &mut reader(max.as_bytes(), 1),
            &mut scratch,
        )
        .unwrap();
        assert_eq!(value.text(), max);
        for (bytes, error) in [
            (vec![b'x'; MAX_SIGNATURE_BYTES + 1], Error::TooLong),
            (vec![b'x'; MAX_SIGNATURE_BYTES + 500], Error::TooLong),
            (vec![b'a', 0, b'b'], Error::Nul),
            (vec![0xff], Error::Utf8),
            (vec![0xc3], Error::Utf8),
        ] {
            let mut input = reader(&bytes, 13);
            assert_eq!(
                read(Kind::Signature, &mut input, &mut scratch).unwrap_err(),
                error
            );
            assert!(bytes.len() - input.bytes.len() <= MAX_SIGNATURE_BYTES + 1);
            assert!(input.calls <= MAX_SIGNATURE_BYTES + 2);
        }
    }
    #[test]
    fn passwords_remove_only_one_optional_line_ending_and_preserve_spaces() {
        let mut scratch = vec![0xa5; PASSWORD_SCRATCH_BYTES + 8];
        for source in [" pass word ", " pass word \n", " pass word \r\n"] {
            for chunk in 1..=source.len() {
                let mut input = reader(source.as_bytes(), chunk);
                let value = read(Kind::RelayPassword, &mut input, &mut scratch).unwrap();
                assert_eq!(value.kind(), Kind::RelayPassword);
                assert_eq!(value.text(), " pass word ");
                assert_eq!(input.ends, 1);
                assert_eq!(&scratch[PASSWORD_SCRATCH_BYTES..], &[0xa5; 8]);
            }
        }
        let max = "P".repeat(MAX_PASSWORD_BYTES);
        for suffix in ["", "\n", "\r\n"] {
            let raw = format!("{max}{suffix}");
            assert_eq!(
                read(Kind::RelayPassword, &mut raw.as_bytes(), &mut scratch)
                    .unwrap()
                    .text(),
                max
            );
        }
        for raw in [
            "", "\n", "\r\n", "p\n\n", "p\r", "p\rp", "p\np", "p\tp", "é", "\u{7f}",
        ] {
            assert_eq!(
                read(Kind::RelayPassword, &mut raw.as_bytes(), &mut scratch).unwrap_err(),
                Error::Password,
                "{raw:?}"
            );
        }
        for (raw, error) in [
            (b"p\0p".as_slice(), Error::Nul),
            (b"\xff".as_slice(), Error::Utf8),
        ] {
            assert_eq!(
                read(Kind::RelayPassword, &mut reader(raw, 1), &mut scratch).unwrap_err(),
                error
            );
        }
        for count in [
            MAX_PASSWORD_BYTES + 1,
            MAX_PASSWORD_BYTES + 2,
            MAX_PASSWORD_BYTES + 3,
        ] {
            let raw = "P".repeat(count);
            assert_eq!(
                read(Kind::RelayPassword, &mut raw.as_bytes(), &mut scratch).unwrap_err(),
                Error::TooLong
            );
        }
    }
    #[test]
    fn short_scratch_late_error_and_reuse_never_return_unfinished_material() {
        for kind in [Kind::Signature, Kind::RelayPassword] {
            let mut scratch = vec![0x55; kind.scratch_bytes()];
            let mut input = reader(b"private-value", 1);
            let short = scratch.len() - 1;
            assert_eq!(
                read(kind, &mut input, &mut scratch[..short]).unwrap_err(),
                Error::Capacity
            );
            assert_eq!(input.calls, 0);
            assert!(scratch.iter().all(|b| *b == 0x55));
            input.end_error = Some(io::ErrorKind::PermissionDenied);
            assert_eq!(
                read(kind, &mut input, &mut scratch).unwrap_err(),
                Error::Read(io::ErrorKind::PermissionDenied)
            );
            assert_eq!(input.ends, 1);
            let mut maximum = vec![b'P'; kind.scratch_bytes() - 1];
            if kind == Kind::RelayPassword {
                let len = maximum.len();
                maximum[len - 2..].copy_from_slice(b"\r\n");
            }
            let mut full = reader(&maximum, maximum.len());
            full.end_error = Some(io::ErrorKind::PermissionDenied);
            assert_eq!(
                read(kind, &mut full, &mut scratch).unwrap_err(),
                Error::Read(io::ErrorKind::PermissionDenied)
            );
            assert_eq!(full.ends, 1);
            let value = read(kind, &mut b"new".as_slice(), &mut scratch).unwrap();
            assert_eq!(value.text(), "new");
            assert_eq!(format!("{value:?}"), "ConfigurationMaterial(<redacted>)");
        }
    }
    #[test]
    fn interruptions_are_bounded_across_progress_and_bad_read_counts_refuse() {
        struct Interrupted {
            calls: usize,
            before: usize,
        }
        impl io::Read for Interrupted {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls > self.before * 2 {
                    return Ok(0);
                }
                if self.calls.is_multiple_of(2) {
                    out[0] = b'x';
                    return Ok(1);
                }
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        let mut scratch = vec![0; SIGNATURE_SCRATCH_BYTES];
        let mut input = Interrupted {
            calls: 0,
            before: MAX_INTERRUPTED_READS as usize,
        };
        assert_eq!(
            read(Kind::Signature, &mut input, &mut scratch)
                .unwrap()
                .text()
                .len(),
            MAX_INTERRUPTED_READS as usize
        );
        let mut input = Interrupted {
            calls: 0,
            before: usize::MAX / 2,
        };
        assert_eq!(
            read(Kind::Signature, &mut input, &mut scratch).unwrap_err(),
            Error::InterruptedLimit
        );
        assert_eq!(input.calls, MAX_INTERRUPTED_READS as usize * 2 + 1);
        struct Bad(bool);
        impl io::Read for Bad {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                Ok(if self.0 { usize::MAX } else { out.len() + 1 })
            }
        }
        for maximum in [false, true] {
            assert_eq!(
                read(Kind::Signature, &mut Bad(maximum), &mut scratch).unwrap_err(),
                Error::InvalidReadCount
            );
        }
    }
    #[test]
    fn reused_password_bytes_are_not_returned_by_a_reader_that_writes_nothing() {
        struct Unwritten(bool);
        impl io::Read for Unwritten {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Ok(if std::mem::replace(&mut self.0, false) {
                    7
                } else {
                    0
                })
            }
        }
        let mut scratch = vec![0xa5; SIGNATURE_SCRATCH_BYTES];
        assert_eq!(
            read(
                Kind::RelayPassword,
                &mut b"hunter2".as_slice(),
                &mut scratch
            )
            .unwrap()
            .text(),
            "hunter2"
        );
        assert_eq!(
            read(Kind::Signature, &mut Unwritten(true), &mut scratch).unwrap_err(),
            Error::Nul
        );
        assert!(scratch.iter().all(|byte| *byte == 0));
    }
    #[test]
    fn final_eof_interruptions_obey_the_retry_bound_at_full_capacity() {
        struct FinalInterrupted<'a> {
            inner: Reader<'a>,
            remaining: u32,
            attempts: u32,
        }
        impl io::Read for FinalInterrupted<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.inner.bytes.is_empty() {
                    self.attempts += 1;
                    if self.remaining > 0 {
                        self.remaining -= 1;
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                }
                self.inner.read(out)
            }
        }
        for kind in [Kind::Signature, Kind::RelayPassword] {
            let mut scratch = vec![0; kind.scratch_bytes()];
            let mut raw = vec![b'P'; kind.scratch_bytes() - 1];
            if kind == Kind::RelayPassword {
                let len = raw.len();
                raw[len - 2..].copy_from_slice(b"\r\n");
            }
            for remaining in [MAX_INTERRUPTED_READS, MAX_INTERRUPTED_READS + 1] {
                let mut input = FinalInterrupted {
                    inner: reader(&raw, raw.len()),
                    remaining,
                    attempts: 0,
                };
                let result = read(kind, &mut input, &mut scratch);
                if remaining == MAX_INTERRUPTED_READS {
                    assert!(result.is_ok());
                    assert_eq!(input.inner.ends, 1);
                } else {
                    assert_eq!(result.unwrap_err(), Error::InterruptedLimit);
                    assert_eq!(input.inner.ends, 0);
                }
                assert_eq!(input.attempts, MAX_INTERRUPTED_READS + 1);
            }
        }
    }
    #[test]
    fn injected_error_messages_do_not_reach_debug_display_or_source() {
        struct Sensitive;
        impl io::Read for Sensitive {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("private-password /private-path"))
            }
        }
        let mut scratch = vec![0; PASSWORD_SCRATCH_BYTES];
        let error = read(Kind::RelayPassword, &mut Sensitive, &mut scratch).unwrap_err();
        assert_eq!(error, Error::Read(io::ErrorKind::Other));
        assert_eq!(
            format!("{error:?} {error}"),
            "Read(Other) config_material_read"
        );
        assert!(std::error::Error::source(&error).is_none());
    }
}
