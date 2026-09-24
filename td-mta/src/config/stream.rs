//! Trusted reader to bounded syntax statements. Success means reader EOF only.
use super::syntax::{self, Diagnostic, Framer, Statement};
use std::{fmt, io};

/// Input read window within the control-worker scratch reservation.
pub const INPUT_BYTES: usize = 16 * 1024;
/// Input, physical line and decoded string storage, excluding builder state.
pub const SCRATCH_BYTES: usize = INPUT_BYTES + syntax::MAX_LINE_BYTES + syntax::MAX_STRING_BYTES;
/// Total retried Interrupted errors; progress does not reset the allowance.
pub const MAX_INTERRUPTED_READS: u32 = 32;

/// Driver refusal. Handler errors are exposed only by typed inspection.
pub enum Error<E> {
    Capacity,
    Syntax(Diagnostic),
    Read(io::ErrorKind),
    InterruptedLimit,
    InvalidReadCount,
    Invariant,
    Handler(E),
}
impl<E> Error<E> {
    /// Stable driver code, or the unchanged syntax code.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Capacity => "config_stream_capacity",
            Self::Syntax(e) => e.code.name(),
            Self::Read(_) => "config_stream_read",
            Self::InterruptedLimit => "config_stream_interrupted_limit",
            Self::InvalidReadCount => "config_stream_invalid_read_count",
            Self::Invariant => "config_stream_invariant",
            Self::Handler(_) => "config_stream_handler",
        }
    }
}
impl<E> fmt::Debug for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(e) => fmt::Debug::fmt(e, f),
            Self::Read(kind) => f.debug_tuple("Read").field(kind).finish(),
            _ => f.write_str(self.name()),
        }
    }
}
impl<E> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(e) => fmt::Display::fmt(e, f),
            _ => f.write_str(self.name()),
        }
    }
}
impl<E> std::error::Error for Error<E> {}
/// Observed reader completion, not schema validity or publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Summary {
    bytes: usize,
    lines: u32,
    statements: u32,
}
impl Summary {
    /// Physical bytes consumed, including comments and line endings.
    pub fn bytes(self) -> usize {
        self.bytes
    }
    /// Physical lines, including blank/comment and unterminated final lines.
    pub fn lines(self) -> u32 {
        self.lines
    }
    /// Nonempty statements successfully dispatched to the handler.
    pub fn statements(self) -> u32 {
        self.statements
    }
}
fn dispatch<E>(
    framer: &mut Framer<'_>,
    decoded: &mut [u8],
    handler: &mut impl FnMut(Statement<'_>) -> Result<(), E>,
    summary: &mut Summary,
) -> Result<(), Error<E>> {
    if let Some((number, bytes)) = framer.line().map_err(Error::Syntax)? {
        let statement = syntax::parse_line(number, bytes, decoded).map_err(Error::Syntax)?;
        summary.lines = summary.lines.checked_add(1).ok_or(Error::Invariant)?;
        if !matches!(statement, Statement::Empty) {
            handler(statement).map_err(Error::Handler)?;
            summary.statements = summary.statements.checked_add(1).ok_or(Error::Invariant)?;
        }
        framer.advance().map_err(Error::Syntax)?;
    }
    Ok(())
}
/// The handler must stage changes only. The caller must discard that entire
/// candidate whenever this returns Err, including failures after its last
/// callback. This does not validate schema/permissions or authorize publication. The
/// trusted Read implementation owns blocking, allocation and truthful EOF.
pub fn read<R: io::Read + ?Sized, E>(
    reader: &mut R,
    scratch: &mut [u8],
    mut handler: impl FnMut(Statement<'_>) -> Result<(), E>,
) -> Result<Summary, Error<E>> {
    let scratch = scratch.get_mut(..SCRATCH_BYTES).ok_or(Error::Capacity)?;
    let (input, rest) = scratch
        .split_at_mut_checked(INPUT_BYTES)
        .ok_or(Error::Capacity)?;
    let (line, decoded) = rest
        .split_at_mut_checked(syntax::MAX_LINE_BYTES)
        .ok_or(Error::Capacity)?;
    let mut framer = Framer::new(line).map_err(Error::Syntax)?;
    let mut summary = Summary {
        bytes: 0,
        lines: 0,
        statements: 0,
    };
    let mut interrupted = 0u32;
    loop {
        let count = match reader.read(input) {
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
        let mut remaining = input.get(..count).ok_or(Error::InvalidReadCount)?;
        if count == 0 {
            framer.finish().map_err(Error::Syntax)?;
            dispatch(&mut framer, decoded, &mut handler, &mut summary)?;
            if !framer.is_finished() {
                return Err(Error::Invariant);
            }
            summary.bytes = framer.consumed_bytes();
            return Ok(summary);
        }
        while !remaining.is_empty() {
            let consumed = framer.feed(remaining).map_err(Error::Syntax)?;
            if consumed == 0 {
                return Err(Error::Invariant);
            }
            remaining = remaining.get(consumed..).ok_or(Error::Invariant)?;
            dispatch(&mut framer, decoded, &mut handler, &mut summary)?;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use syntax::{Code, Value};
    struct Chunked<'a> {
        bytes: &'a [u8],
        chunk: usize,
        reads: usize,
        end: Option<io::ErrorKind>,
    }
    impl io::Read for Chunked<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.bytes.is_empty() {
                return match self.end {
                    Some(kind) => Err(kind.into()),
                    None => Ok(0),
                };
            }
            let n = out.len().min(self.chunk).min(self.bytes.len());
            out[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }
    fn input(bytes: &[u8], chunk: usize) -> Chunked<'_> {
        Chunked {
            bytes,
            chunk,
            reads: 0,
            end: None,
        }
    }
    #[test]
    fn every_chunk_size_preserves_statements_locations_and_real_eof() {
        let bytes = b"# comment\r\n[account \"caf\xc3\xa9\"]\nname = \"a\\\"b\"\r\ncount = 42";
        for chunk in 1..=bytes.len() {
            let mut reader = input(bytes, chunk);
            let mut scratch = vec![0xaa; SCRATCH_BYTES + 16];
            let mut seen = Vec::new();
            let result = read(&mut reader, &mut scratch, |statement| {
                match statement {
                    Statement::Section {
                        location,
                        name,
                        label,
                    } => seen.push((
                        location.line.get(),
                        name.to_owned(),
                        label.unwrap().to_owned(),
                    )),
                    Statement::Assignment {
                        location,
                        key,
                        value,
                        ..
                    } => seen.push((
                        location.line.get(),
                        key.to_owned(),
                        match value {
                            Value::Text(v) => v.to_owned(),
                            Value::Integer(v) => v.to_string(),
                            _ => String::new(),
                        },
                    )),
                    Statement::Empty => return Err(()),
                }
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(result.bytes(), bytes.len());
            assert_eq!(result.lines(), 4);
            assert_eq!(result.statements(), 3);
            assert_eq!(reader.reads, bytes.len().div_ceil(chunk) + 1);
            assert_eq!(
                seen,
                [
                    (2, "account".into(), "café".into()),
                    (3, "name".into(), "a\"b".into()),
                    (4, "count".into(), "42".into())
                ]
            );
            assert_eq!(&scratch[SCRATCH_BYTES..], &[0xaa; 16]);
        }
    }
    #[test]
    fn eof_is_required_after_valid_prefix_and_final_line() {
        for bytes in [b"key = 1\n".as_slice(), b"key = 1"] {
            for kind in [
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::WouldBlock,
                io::ErrorKind::UnexpectedEof,
            ] {
                let mut scratch = vec![0; SCRATCH_BYTES];
                let mut reader = input(bytes, INPUT_BYTES);
                // A real read error, even UnexpectedEof, is not reader EOF.
                reader.end = Some(kind);
                let mut calls = 0;
                let result = read(&mut reader, &mut scratch, |_| {
                    calls += 1;
                    Ok::<_, ()>(())
                });
                assert!(matches!(result, Err(Error::Read(k)) if k == kind));
                assert_eq!(calls, usize::from(bytes.ends_with(b"\n")));
                assert_eq!(reader.reads, 2);
            }
        }
        for (bytes, lines) in [(b"".as_slice(), 0), (b"\n", 1), (b"# x\n", 1), (b"# x", 1)] {
            let mut scratch = vec![0; SCRATCH_BYTES];
            let summary = read(&mut input(bytes, 1), &mut scratch, |_| Err(())).unwrap();
            assert_eq!(summary.lines(), lines);
            assert_eq!(summary.statements(), 0);
        }
    }
    #[test]
    fn handler_and_late_syntax_errors_cannot_complete() {
        let mut scratch = vec![0; SCRATCH_BYTES];
        let mut reader = input(b"a = 1\nb = 2\n", 6);
        let mut calls = 0;
        let error = read(&mut reader, &mut scratch, |_| {
            calls += 1;
            Err(17)
        })
        .unwrap_err();
        assert!(matches!(error, Error::Handler(17)));
        assert_eq!(reader.reads, 1);
        assert_eq!(calls, 1);
        let mut reader = input(b"a = 1\nb = \"unfinished", INPUT_BYTES);
        let result = read(&mut reader, &mut scratch, |_| Ok::<_, ()>(()));
        assert!(
            matches!(result, Err(Error::Syntax(e)) if e.code == Code::UnterminatedString && e.location.line.get() == 2)
        );
        assert_eq!(reader.reads, 2);
    }
    #[test]
    fn interrupted_budget_is_total_and_all_other_read_failures_stop() {
        struct Interrupted {
            remaining: u32,
            progress: bool,
            turn: bool,
            reads: usize,
        }
        impl io::Read for Interrupted {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.reads += 1;
                if self.remaining == 0 {
                    return Ok(0);
                }
                self.turn = !self.turn;
                if self.progress && !self.turn {
                    out[0] = b'\n';
                    return Ok(1);
                }
                self.remaining -= 1;
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        for progress in [false, true] {
            for count in [MAX_INTERRUPTED_READS, MAX_INTERRUPTED_READS + 1] {
                let mut reader = Interrupted {
                    remaining: count,
                    progress,
                    turn: false,
                    reads: 0,
                };
                let mut scratch = vec![0; SCRATCH_BYTES];
                let result = read(&mut reader, &mut scratch, |_| Ok::<_, ()>(()));
                if count == MAX_INTERRUPTED_READS {
                    let summary = result.unwrap();
                    let lines = if progress {
                        MAX_INTERRUPTED_READS - 1
                    } else {
                        0
                    };
                    assert_eq!(summary.lines(), lines);
                    assert_eq!(summary.bytes(), lines as usize);
                    assert_eq!(summary.statements(), 0);
                    assert_eq!(reader.reads, if progress { 64 } else { 33 });
                } else {
                    assert!(matches!(result, Err(Error::InterruptedLimit)));
                    assert_eq!(reader.reads, if progress { 65 } else { 33 });
                }
            }
        }
    }
    #[test]
    fn late_handler_and_mid_chunk_syntax_failure_stop_all_callbacks() {
        let mut scratch = vec![0; SCRATCH_BYTES];
        for (bytes, reads) in [
            (b"a = 1\nb = 2\nc = 3\n".as_slice(), 1),
            (b"a = 1\nb = 2", 2),
        ] {
            let mut reader = input(bytes, INPUT_BYTES);
            let mut calls = 0;
            let result = read(&mut reader, &mut scratch, |_| {
                calls += 1;
                if calls == 2 {
                    Err(17)
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(Error::Handler(17))));
            assert_eq!(calls, 2);
            assert_eq!(reader.reads, reads);
        }
        let mut reader = input(b"a = 1\ninvalid syntax\nc = 3\n", INPUT_BYTES);
        let mut calls = 0;
        let result = read(&mut reader, &mut scratch, |_| {
            calls += 1;
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(Error::Syntax(_))));
        assert_eq!(calls, 1);
        assert_eq!(reader.reads, 1);
    }
    #[test]
    fn one_byte_reads_and_interruptions_reach_exact_total_call_bound() {
        struct Counted<'a> {
            input: Chunked<'a>,
            interruptions: u32,
            calls: usize,
        }
        impl io::Read for Counted<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.interruptions != 0 {
                    self.interruptions -= 1;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.input.read(out)
            }
        }
        let mut line = vec![b' '; syntax::MAX_LINE_BYTES];
        line[0] = b'#';
        line[syntax::MAX_LINE_BYTES - 1] = b'\n';
        let mut bytes = line.repeat(syntax::MAX_INPUT_BYTES / syntax::MAX_LINE_BYTES);
        let mut scratch = vec![0; SCRATCH_BYTES];
        for oversized in [false, true] {
            if oversized {
                bytes.extend_from_slice(b"extra");
            }
            let mut reader = Counted {
                input: input(&bytes, 1),
                interruptions: MAX_INTERRUPTED_READS,
                calls: 0,
            };
            let result = read(&mut reader, &mut scratch, |_| Err(()));
            if oversized {
                assert!(matches!(result, Err(Error::Syntax(e)) if e.code == Code::InputTooLarge));
            } else {
                assert_eq!(result.unwrap().bytes(), syntax::MAX_INPUT_BYTES);
            }
            assert_eq!(reader.calls, 2_097_185);
        }
    }
    #[test]
    fn capacity_bad_reader_counts_and_error_text_are_bounded() {
        struct Bad;
        impl io::Read for Bad {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                Ok(out.len() + 1)
            }
        }
        let mut scratch = vec![0; SCRATCH_BYTES];
        assert!(matches!(
            read(&mut Bad, &mut scratch, |_| Ok::<_, ()>(())),
            Err(Error::InvalidReadCount)
        ));
        let mut reader = input(b"a = 1", 1);
        assert!(matches!(
            read(&mut reader, &mut scratch[..SCRATCH_BYTES - 1], |_| Ok::<
                _,
                (),
            >(
                ()
            )),
            Err(Error::Capacity)
        ));
        assert_eq!(reader.reads, 0);
        let marker = "private_fixture";
        let error = Error::Handler(marker);
        assert!(!format!("{error:?} {error}").contains(marker));
        let chained = Error::Handler(io::Error::other(marker));
        assert!(std::error::Error::source(&chained).is_none());
        struct Private;
        impl io::Read for Private {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("private_fixture"))
            }
        }
        let error = read(&mut Private, &mut scratch, |_| Ok::<_, ()>(())).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(marker));
    }
    #[test]
    fn diagnostic_names_are_fixed_and_syntax_locations_survive() {
        for (error, name) in [
            (Error::<()>::Capacity, "config_stream_capacity"),
            (
                Error::Read(io::ErrorKind::PermissionDenied),
                "config_stream_read",
            ),
            (Error::InterruptedLimit, "config_stream_interrupted_limit"),
            (Error::InvalidReadCount, "config_stream_invalid_read_count"),
            (Error::Invariant, "config_stream_invariant"),
            (Error::Handler(()), "config_stream_handler"),
        ] {
            assert_eq!(error.name(), name);
            assert_eq!(error.to_string(), name);
        }
        let mut scratch = vec![0; SCRATCH_BYTES];
        let error = read(&mut input(b"# x\n  a = \"x\n", 1), &mut scratch, |_| {
            Ok::<_, ()>(())
        })
        .unwrap_err();
        assert_eq!(error.name(), "config_unterminated_string");
        assert!(error.to_string().contains("line 2, byte column 9"));
    }
    #[test]
    fn aggregate_and_physical_limits_apply_across_reads() {
        let mut scratch = vec![0; SCRATCH_BYTES];
        let mut full_line = vec![b' '; syntax::MAX_LINE_BYTES];
        full_line[0] = b'#';
        full_line[syntax::MAX_LINE_BYTES - 1] = b'\n';
        let mut bytes = full_line.repeat(syntax::MAX_INPUT_BYTES / syntax::MAX_LINE_BYTES);
        assert_eq!(
            read(&mut input(&bytes, 4093), &mut scratch, |_| Ok::<_, ()>(()))
                .unwrap()
                .bytes(),
            syntax::MAX_INPUT_BYTES
        );
        bytes.push(b'\n');
        assert!(
            matches!(read(&mut input(&bytes, 4093), &mut scratch, |_| Ok::<_, ()>(())), Err(Error::Syntax(e)) if e.code == Code::InputTooLarge)
        );
        let lines = vec![b'\n'; syntax::MAX_LINES as usize + 1];
        assert!(
            matches!(read(&mut input(&lines, INPUT_BYTES), &mut scratch, |_| Ok::<_, ()>(())), Err(Error::Syntax(e)) if e.code == Code::TooManyLines)
        );
        let long = vec![b' '; syntax::MAX_LINE_BYTES + 1];
        assert!(
            matches!(read(&mut input(&long, 127), &mut scratch, |_| Ok::<_, ()>(())), Err(Error::Syntax(e)) if e.code == Code::LineTooLong)
        );
    }
}
