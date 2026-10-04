//! Stateless unquote/fold projection; the caller admits every read and owns failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// One projected octet; no lexical or original-placement authority follows.
pub struct Octet {
    /// Logical byte after unquoting and folding.
    pub value: u8,
    /// First original-source position after this octet or fold.
    pub next: usize,
    /// Any contributing byte used a quoted pair.
    pub escaped: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Caller refusal or a projection invariant failure.
pub enum Error<E> {
    /// Read/admission failure, returned without another read.
    Read(E),
    /// A quoted-pair introducer has no following byte.
    IncompletePair,
    /// A source-position increment overflows.
    InvalidState,
}
impl<E: std::fmt::Display> std::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "projection read: {error}"),
            Self::IncompletePair => f.write_str("incomplete projected quoted pair"),
            Self::InvalidState => f.write_str("invalid projection position"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for Error<E> {}
fn unquoted<E>(
    at: usize,
    quoted: bool,
    read: &mut impl FnMut(usize) -> Result<Option<u8>, E>,
) -> Result<Option<Octet>, Error<E>> {
    let Some(mut value) = read(at).map_err(Error::Read)? else {
        return Ok(None);
    };
    let mut next = at.checked_add(1).ok_or(Error::InvalidState)?;
    let escaped = quoted && value == b'\\';
    if escaped {
        value = read(next)
            .map_err(Error::Read)?
            .ok_or(Error::IncompletePair)?;
        next = next.checked_add(1).ok_or(Error::InvalidState)?;
    }
    Ok(Some(Octet {
        value,
        next,
        escaped,
    }))
}
/// Project one logical octet from previously validated spelling.
/// The callback owns bounds, source access and admission, including EOF policy.
/// At most six callback calls occur; failures must retire the enclosing owner.
/// Logical folds drop CRLF/LF before WSP and OR escape provenance across the fold.
/// Nonfold escaped line endings remain literal. No word-placement proof follows.
pub fn atom<E>(
    at: usize,
    quoted: bool,
    mut read: impl FnMut(usize) -> Result<Option<u8>, E>,
) -> Result<Option<Octet>, Error<E>> {
    let Some(first) = unquoted(at, quoted, &mut read)? else {
        return Ok(None);
    };
    let after = match first.value {
        b'\r' => match unquoted(first.next, quoted, &mut read)? {
            Some(Octet {
                value: b'\n',
                next,
                escaped,
            }) => Some((next, first.escaped || escaped)),
            _ => None,
        },
        b'\n' => Some((first.next, first.escaped)),
        _ => None,
    };
    if let Some((after, escaped_line)) = after {
        if let Some(
            space @ Octet {
                value: b' ' | b'\t',
                ..
            },
        ) = unquoted(after, quoted, &mut read)?
        {
            return Ok(Some(Octet {
                escaped: escaped_line || space.escaped,
                ..space
            }));
        }
    }
    Ok(Some(first))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn literal_pairs_folds_and_escape_provenance_have_exact_reads() {
        for (source, quoted, expected, positions) in [
            (
                b"a".as_slice(),
                false,
                Some(Octet {
                    value: b'a',
                    next: 1,
                    escaped: false,
                }),
                vec![0],
            ),
            (
                b"\\a",
                true,
                Some(Octet {
                    value: b'a',
                    next: 2,
                    escaped: true,
                }),
                vec![0, 1],
            ),
            (
                b"\\a",
                false,
                Some(Octet {
                    value: b'\\',
                    next: 1,
                    escaped: false,
                }),
                vec![0],
            ),
            (
                b"\r\n x",
                true,
                Some(Octet {
                    value: b' ',
                    next: 3,
                    escaped: false,
                }),
                vec![0, 1, 2],
            ),
            (
                b"\r\n x",
                false,
                Some(Octet {
                    value: b' ',
                    next: 3,
                    escaped: false,
                }),
                vec![0, 1, 2],
            ),
            (
                b"\\\r\n x",
                false,
                Some(Octet {
                    value: b'\\',
                    next: 1,
                    escaped: false,
                }),
                vec![0],
            ),
            (
                b"\\\r\n x",
                true,
                Some(Octet {
                    value: b' ',
                    next: 4,
                    escaped: true,
                }),
                vec![0, 1, 2, 3],
            ),
            (
                b"\\\n x",
                true,
                Some(Octet {
                    value: b' ',
                    next: 3,
                    escaped: true,
                }),
                vec![0, 1, 2],
            ),
            (
                b"\\\r\\\n\\\tx",
                true,
                Some(Octet {
                    value: b'\t',
                    next: 6,
                    escaped: true,
                }),
                vec![0, 1, 2, 3, 4, 5],
            ),
            (
                b"\r\\\n x",
                true,
                Some(Octet {
                    value: b' ',
                    next: 4,
                    escaped: true,
                }),
                vec![0, 1, 2, 3],
            ),
            (
                b"\n\\\tx",
                true,
                Some(Octet {
                    value: b'\t',
                    next: 3,
                    escaped: true,
                }),
                vec![0, 1, 2],
            ),
            (
                b"\\\r\\\nx",
                true,
                Some(Octet {
                    value: b'\r',
                    next: 2,
                    escaped: true,
                }),
                vec![0, 1, 2, 3, 4],
            ),
            (
                b"\rx",
                true,
                Some(Octet {
                    value: b'\r',
                    next: 1,
                    escaped: false,
                }),
                vec![0, 1],
            ),
            (
                b"\n",
                true,
                Some(Octet {
                    value: b'\n',
                    next: 1,
                    escaped: false,
                }),
                vec![0, 1],
            ),
            (b"", true, None, vec![0]),
        ] {
            let mut actual = Vec::new();
            let result = atom(0, quoted, |at| {
                actual.push(at);
                Ok::<_, ()>(source.get(at).copied())
            });
            assert_eq!(result, Ok(expected), "{source:?}");
            assert_eq!(actual, positions, "{source:?}");
            assert!(actual.len() <= 6);
        }
    }
    #[test]
    fn every_read_refusal_including_eof_is_propagated_without_further_reads() {
        for source in [b"\\\r\\\n\\ ".as_slice(), b"\r\n", b"", b"\\"] {
            let mut calls = 0;
            let _ = atom(0, true, |at| {
                calls += 1;
                Ok::<_, u8>(source.get(at).copied())
            });
            for cut in 0..calls {
                let mut attempted = 0;
                let result = atom(0, true, |at| {
                    attempted += 1;
                    if attempted > cut {
                        Err(7)
                    } else {
                        Ok(source.get(at).copied())
                    }
                });
                assert_eq!(result, Err(Error::Read(7)));
                assert_eq!(attempted, cut + 1);
            }
        }
    }
    #[test]
    fn incomplete_pairs_overflow_and_octets_remain_separate_from_validation() {
        assert_eq!(
            atom(0, true, |at| Ok::<_, ()>(b"\\".get(at).copied())),
            Err(Error::IncompletePair)
        );
        assert_eq!(
            atom(usize::MAX, false, |_| Ok::<_, ()>(Some(b'a'))),
            Err(Error::InvalidState)
        );
        assert_eq!(
            atom(usize::MAX - 1, true, |_| Ok::<_, ()>(Some(b'\\'))),
            Err(Error::InvalidState)
        );
        for byte in 0..=255 {
            let source = [byte];
            if byte != b'\\' {
                let result = atom(0, true, |at| Ok::<_, ()>(source.get(at).copied()))
                    .unwrap()
                    .unwrap();
                assert_eq!(result.value, byte);
                assert_eq!(result.next, 1);
                assert!(!result.escaped);
            }
            let source = [b'\\', byte];
            let result = atom(0, true, |at| Ok::<_, ()>(source.get(at).copied()))
                .unwrap()
                .unwrap();
            assert_eq!(result.value, byte);
            assert_eq!(result.next, 2);
            assert!(result.escaped);
        }
    }
}
