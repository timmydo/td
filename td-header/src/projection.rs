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
/// One strict UTF-8 character assembled from logical octets.
/// Escape provenance belongs to the first logical octet, including its fold.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Character {
    pub value: char,
    pub next: usize,
    pub escaped: bool,
}
/// Projection/admission refusal or invalid logical UTF-8 spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharacterError<E> {
    Projection(Error<E>),
    /// The separate local UTF-8 inspection was refused.
    Verify(E),
    InvalidUtf8,
}
impl<E: std::fmt::Display> std::fmt::Display for CharacterError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Projection(error) => error.fmt(f),
            Self::Verify(error) => write!(f, "projection verification: {error}"),
            Self::InvalidUtf8 => f.write_str("invalid projected UTF-8"),
        }
    }
}
impl<E: std::error::Error> std::error::Error for CharacterError<E> {}
/// Assemble at most four logical octets without retaining source or admission.
/// `read` admits each source access as for `atom`. After assembly, `verify`
/// admits the separate local UTF-8 inspection of one to four buffered bytes.
/// Both callbacks borrow the same caller-owned context sequentially; neither
/// receives a copied allowance. At most 24 read calls and one verification call
/// occur. Refusals return immediately and must retire the enclosing owner.
/// EOF before the first octet returns None; truncation after it is InvalidUtf8.
/// No control filtering, normalization or word-placement authority follows.
pub fn character<C: ?Sized, E>(
    at: usize,
    quoted: bool,
    context: &mut C,
    mut read: impl FnMut(&mut C, usize) -> Result<Option<u8>, E>,
    mut verify: impl FnMut(&mut C, usize) -> Result<(), E>,
) -> Result<Option<Character>, CharacterError<E>> {
    let Some(first) =
        atom(at, quoted, |at| read(context, at)).map_err(CharacterError::Projection)?
    else {
        return Ok(None);
    };
    let width = match first.value {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Err(CharacterError::InvalidUtf8),
    };
    let mut bytes = [first.value, 0, 0, 0];
    let mut next = first.next;
    for cell in bytes.get_mut(1..width).ok_or(CharacterError::InvalidUtf8)? {
        let octet = atom(next, quoted, |at| read(context, at))
            .map_err(CharacterError::Projection)?
            .ok_or(CharacterError::InvalidUtf8)?;
        *cell = octet.value;
        next = octet.next;
    }
    verify(context, width).map_err(CharacterError::Verify)?;
    let value = std::str::from_utf8(bytes.get(..width).ok_or(CharacterError::InvalidUtf8)?)
        .map_err(|_| CharacterError::InvalidUtf8)?
        .chars()
        .next()
        .ok_or(CharacterError::InvalidUtf8)?;
    Ok(Some(Character {
        value,
        next,
        escaped: first.escaped,
    }))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn characters_preserve_first_octet_provenance_and_admit_local_verification() {
        for (source, quoted, value, next, escaped, positions, width) in [
            (b"a".as_slice(), false, 'a', 1, false, vec![0], 1),
            ("é".as_bytes(), true, 'é', 2, false, vec![0, 1], 2),
            ("例".as_bytes(), true, '例', 3, false, vec![0, 1, 2], 3),
            ("🐈".as_bytes(), true, '🐈', 4, false, vec![0, 1, 2, 3], 4),
            (b"\\\xc3\\\xa9", true, 'é', 4, true, vec![0, 1, 2, 3], 2),
            (b"\xc3\\\xa9", true, 'é', 3, false, vec![0, 1, 2], 2),
            (
                b"\\\r\\\n\\\t",
                true,
                '\t',
                6,
                true,
                vec![0, 1, 2, 3, 4, 5],
                1,
            ),
            (b"\r\n ", false, ' ', 3, false, vec![0, 1, 2], 1),
            (b"\\\0", true, '\0', 2, true, vec![0, 1], 1),
            (
                "\u{10ffff}".as_bytes(),
                false,
                '\u{10ffff}',
                4,
                false,
                vec![0, 1, 2, 3],
                4,
            ),
        ] {
            let mut calls = Vec::new();
            let actual = character(
                0,
                quoted,
                &mut calls,
                |calls, at| {
                    calls.push((false, at));
                    Ok::<_, u8>(source.get(at).copied())
                },
                |calls, width| {
                    calls.push((true, width));
                    Ok(())
                },
            );
            assert_eq!(
                actual,
                Ok(Some(Character {
                    value,
                    next,
                    escaped
                })),
                "{source:?}"
            );
            let mut expected: Vec<_> = positions.into_iter().map(|at| (false, at)).collect();
            expected.push((true, width));
            assert_eq!(calls, expected);
            assert!(calls.len() <= 25);
            // Every source and verification refusal stops at that callback.
            for cut in 0..calls.len() {
                let mut attempted = 0;
                let actual = character(
                    0,
                    quoted,
                    &mut attempted,
                    |n, at| {
                        *n += 1;
                        if *n > cut {
                            Err(7)
                        } else {
                            Ok(source.get(at).copied())
                        }
                    },
                    |n, _| {
                        *n += 1;
                        if *n > cut {
                            Err(7)
                        } else {
                            Ok(())
                        }
                    },
                );
                assert_eq!(
                    actual,
                    Err(if calls.get(cut).unwrap().0 {
                        CharacterError::Verify(7)
                    } else {
                        CharacterError::Projection(Error::Read(7))
                    })
                );
                assert_eq!(attempted, cut + 1);
            }
        }
    }
    #[test]
    fn maximal_escaped_scalar_and_malformed_folds_have_bounded_callback_counts() {
        let malformed = [
            b'\\', 0xf0, b'\\', b'\r', b'\\', b'\n', b'\\', b' ', b'\\', b'\r', b'\\', b'\n',
            b'\\', b' ', b'\\', b'\r', b'\\', b'\n', b'\\', b' ',
        ];
        for (source, visits, expected) in [
            (
                b"\\\xf0\\\x9f\\\x90\\\x88".as_slice(),
                8,
                Ok(Some(Character {
                    value: '🐈',
                    next: 8,
                    escaped: true,
                })),
            ),
            (malformed.as_slice(), 20, Err(CharacterError::InvalidUtf8)),
        ] {
            let mut calls = Vec::new();
            assert_eq!(
                character(
                    0,
                    true,
                    &mut calls,
                    |calls, at| {
                        calls.push((false, at));
                        Ok::<_, u8>(source.get(at).copied())
                    },
                    |calls, width| {
                        calls.push((true, width));
                        Ok(())
                    }
                ),
                expected
            );
            let mut expected_calls: Vec<_> = (0..visits).map(|at| (false, at)).collect();
            expected_calls.push((true, 4));
            assert_eq!(calls, expected_calls);
            assert!(visits <= 24);
        }
    }
    #[test]
    fn character_errors_do_not_grant_validation_or_recover_bad_utf8() {
        for (source, positions, verify_width) in [
            (b"\x80".as_slice(), vec![0], None),
            (b"\xc0\x80", vec![0], None),
            (b"\xc3", vec![0, 1], None),
            (b"\xc3a", vec![0, 1], Some(2)),
            (b"\xe0\x80\x80", vec![0, 1, 2], Some(3)),
            (b"\xed\xa0\x80", vec![0, 1, 2], Some(3)),
            (b"\xf4\x90\x80\x80", vec![0, 1, 2, 3], Some(4)),
        ] {
            let mut calls = Vec::new();
            assert_eq!(
                character(
                    0,
                    true,
                    &mut calls,
                    |calls, at| {
                        calls.push((false, at));
                        Ok::<_, u8>(source.get(at).copied())
                    },
                    |calls, width| {
                        calls.push((true, width));
                        Ok(())
                    }
                ),
                Err(CharacterError::InvalidUtf8),
                "{source:?}"
            );
            let mut expected: Vec<_> = positions.into_iter().map(|at| (false, at)).collect();
            if let Some(width) = verify_width {
                expected.push((true, width));
            }
            assert_eq!(calls, expected, "{source:?}");
            for cut in 0..calls.len() {
                let mut attempted = 0;
                let result = character(
                    0,
                    true,
                    &mut attempted,
                    |n, at| {
                        *n += 1;
                        if *n > cut {
                            Err(7)
                        } else {
                            Ok(source.get(at).copied())
                        }
                    },
                    |n, _| {
                        *n += 1;
                        if *n > cut {
                            Err(7)
                        } else {
                            Ok(())
                        }
                    },
                );
                assert_eq!(
                    result,
                    Err(if expected.get(cut).unwrap().0 {
                        CharacterError::Verify(7)
                    } else {
                        CharacterError::Projection(Error::Read(7))
                    })
                );
                assert_eq!(attempted, cut + 1);
            }
        }
        assert_eq!(
            character(
                0,
                true,
                &mut (),
                |_, at| Ok::<_, ()>(b"\\".get(at).copied()),
                |_, _| Ok(())
            ),
            Err(CharacterError::Projection(Error::IncompletePair))
        );
        assert_eq!(
            character(
                usize::MAX,
                false,
                &mut (),
                |_, _| Ok::<_, ()>(Some(b'a')),
                |_, _| Ok(())
            ),
            Err(CharacterError::Projection(Error::InvalidState))
        );
        let mut calls = 0;
        assert_eq!(
            character(
                0,
                false,
                &mut calls,
                |n, _| {
                    *n += 1;
                    Ok::<_, ()>(None)
                },
                |_, _| panic!("verification after EOF")
            ),
            Ok(None)
        );
        assert_eq!(calls, 1);
        assert_eq!(
            character(
                0,
                false,
                &mut (),
                |_, _| Err::<Option<u8>, _>(7),
                |_, _| panic!("verification after refusal")
            ),
            Err(CharacterError::Projection(Error::Read(7)))
        );
    }
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
