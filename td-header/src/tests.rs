#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::{cfws, delimited, Charge, Work};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    Visits,
    Records,
    Clock,
}
struct Admission {
    visits: u64,
    records: u64,
    clock: bool,
}
impl Work for Admission {
    type Error = Stop;
    fn charge(&mut self, charge: Charge) -> Result<(), Stop> {
        if self.clock {
            return Err(Stop::Clock);
        }
        if charge.visits > self.visits {
            return Err(Stop::Visits);
        }
        if charge.records > self.records {
            return Err(Stop::Records);
        }
        self.visits -= charge.visits;
        self.records -= charge.records;
        Ok(())
    }
}
fn work() -> Admission {
    Admission {
        visits: 1_000_000,
        records: 1_000_000,
        clock: false,
    }
}
fn comments(source: &[u8]) -> Result<(Vec<cfws::Comment>, cfws::End), cfws::Error<Stop>> {
    let mut cursor = cfws::Cursor::new(source, 0);
    assert!(std::mem::size_of_val(&cursor) <= 64);
    let mut work = work();
    let mut out = Vec::new();
    for _ in 0..100_000 {
        let before = (work.visits, work.records);
        let result = cursor.poll(&mut work);
        assert!(before.0 - work.visits <= 160);
        assert!(before.1 - work.records <= 32);
        match result? {
            cfws::Status::Yield => {}
            cfws::Status::Comment(value) => out.push(value),
            cfws::Status::Complete(end) => {
                work.clock = true;
                assert_eq!(cursor.poll(&mut work), Ok(cfws::Status::Complete(end)));
                return Ok((out, end));
            }
        }
    }
    panic!("comment cursor did not complete")
}
fn token(
    source: &[u8],
    kind: delimited::Kind,
) -> Result<delimited::Extent, delimited::Error<Stop>> {
    let mut cursor = delimited::Cursor::new(source, 0, kind);
    assert!(std::mem::size_of_val(&cursor) <= 64);
    let mut work = work();
    for _ in 0..100_000 {
        let before = (work.visits, work.records);
        let result = cursor.poll(&mut work);
        assert!(before.0 - work.visits <= 160);
        assert!(before.1 - work.records <= 32);
        if let delimited::Status::Complete(extent) = result? {
            work.clock = true;
            assert_eq!(
                cursor.poll(&mut work),
                Ok(delimited::Status::Complete(extent))
            );
            return Ok(extent);
        }
    }
    panic!("token cursor did not complete")
}
#[test]
fn cfws_retains_raw_comment_extents_and_stops_at_the_next_token() {
    assert_eq!(
        comments(b" (a(b)c)\r\n\t(d) token"),
        Ok((
            vec![
                cfws::Comment { start: 1, end: 8 },
                cfws::Comment { start: 11, end: 14 }
            ],
            cfws::End {
                position: 15,
                consumed: true
            }
        ))
    );
    for source in [b"".as_slice(), b"x", b"\xff", b"\0", b"\"(literal)\""] {
        assert_eq!(
            comments(source),
            Ok((
                vec![],
                cfws::End {
                    position: 0,
                    consumed: false
                }
            ))
        );
    }
    let source = format!("({}) tail", "🐈".repeat(10_000));
    assert_eq!(comments(source.as_bytes()).unwrap().1.position, 40_003);
    for source in [
        b"(a\\(b\\))".as_slice(),
        b"(a\r\n b\n\tc)",
        b"(\x01\x7f)",
        b"(\\\0\\\r\\\n)",
        "(é\\🐈)".as_bytes(),
    ] {
        let result = comments(source).unwrap();
        assert_eq!(
            result.0,
            vec![cfws::Comment {
                start: 0,
                end: source.len()
            }]
        );
    }
}
#[test]
fn comments_refuse_malformed_syntax_and_exact_excess_depth() {
    for source in [
        b"(".as_slice(),
        b"(a\\",
        b"(a\0)",
        b"(a\rb)",
        b"(a\nb)",
        b"(\xff)",
        b"(\xc0\x80)",
        b"(\xed\xa0\x80)",
        b"(\xf4\x90\x80\x80)",
        b"(\xe2\x82",
        b"() (bad",
    ] {
        assert_eq!(comments(source), Err(cfws::Error::Malformed), "{source:?}");
    }
    let source = format!("{}{}", "(".repeat(32), ")".repeat(32));
    assert!(comments(source.as_bytes()).is_ok());
    let source = format!("{}{}", "(".repeat(33), ")".repeat(33));
    assert_eq!(comments(source.as_bytes()), Err(cfws::Error::NestingLimit));
}
#[test]
fn delimiters_keep_escapes_folds_controls_unicode_and_trailing_source() {
    for kind in [
        delimited::Kind::QuotedString,
        delimited::Kind::DomainLiteral,
    ] {
        let (open, close) = if kind == delimited::Kind::QuotedString {
            ('"', '"')
        } else {
            ('[', ']')
        };
        for value in ["", "é\\🐈", "x\r\n y\n\tz", "\u{1}\u{7f}"] {
            let source = format!("{open}{value}{close}tail");
            assert_eq!(
                token(source.as_bytes(), kind),
                Ok(delimited::Extent {
                    start: 0,
                    end: source.len() - 4
                })
            );
        }
        let source = format!("{open}{}{close}", "🐈".repeat(10_000));
        assert_eq!(
            token(source.as_bytes(), kind),
            Ok(delimited::Extent {
                start: 0,
                end: 40_002
            })
        );
    }
    assert_eq!(
        token(b"\"a\\\"b\\\\c\"", delimited::Kind::QuotedString),
        Ok(delimited::Extent { start: 0, end: 9 })
    );
    assert_eq!(
        token(b"[a\\]b\\[c]", delimited::Kind::DomainLiteral),
        Ok(delimited::Extent { start: 0, end: 9 })
    );
}
#[test]
fn delimiters_refuse_incomplete_invalid_utf8_and_unfolded_endings() {
    for source in [
        b"".as_slice(),
        b"x",
        b"\"",
        b"\"a\\",
        b"\"a\0b\"",
        b"\"a\rb\"",
        b"\"a\nb\"",
        b"\"a\r\nb\"",
        b"\"\xff\"",
        b"\"\xed\xa0\x80\"",
        b"\"\xe2\x82",
        b"\"\\\xc3",
    ] {
        assert_eq!(
            token(source, delimited::Kind::QuotedString),
            Err(delimited::Error::Malformed),
            "{source:?}"
        );
    }
    for source in [
        b"".as_slice(),
        b"x",
        b"[",
        b"[a\\",
        b"[a\0b]",
        b"[a\rb]",
        b"[a\nb]",
        b"[[]]",
        b"[\xff]",
        b"[\xed\xa0\x80]",
        b"[\xe2\x82",
        b"[\\\xc3",
    ] {
        assert_eq!(
            token(source, delimited::Kind::DomainLiteral),
            Err(delimited::Error::Malformed),
            "{source:?}"
        );
    }
}
#[test]
fn caller_errors_latch_even_with_a_replacement_admission() {
    for stop in [Stop::Visits, Stop::Records, Stop::Clock] {
        let mut limited = work();
        match stop {
            Stop::Visits => limited.visits = 0,
            Stop::Records => limited.records = 0,
            Stop::Clock => limited.clock = true,
        }
        let mut cursor = cfws::Cursor::new(b"(x)", 0);
        assert_eq!(cursor.poll(&mut limited), Err(cfws::Error::Work(stop)));
        let mut fresh = work();
        assert_eq!(cursor.poll(&mut fresh), Err(cfws::Error::Work(stop)));
        assert_eq!((fresh.visits, fresh.records), (1_000_000, 1_000_000));
        let mut limited = work();
        match stop {
            Stop::Visits => limited.visits = 0,
            Stop::Records => limited.records = 0,
            Stop::Clock => limited.clock = true,
        }
        let mut cursor = delimited::Cursor::new(b"\"x\"", 0, delimited::Kind::QuotedString);
        assert_eq!(cursor.poll(&mut limited), Err(delimited::Error::Work(stop)));
        assert_eq!(cursor.poll(&mut fresh), Err(delimited::Error::Work(stop)));
        assert_eq!((fresh.visits, fresh.records), (1_000_000, 1_000_000));
    }
}

#[test]
fn eof_utf8_rereads_and_failed_fold_lookahead_have_exact_visit_costs() {
    for (source, visits, records, malformed) in [
        (b"".as_slice(), 0, 1, false),
        ("(🐈)".as_bytes(), 7, 4, false),
        (b" \r\nx", 4, 2, false),
        (b"(\xe2\x82", 4, 2, true),
    ] {
        let mut cursor = cfws::Cursor::new(source, 0);
        let mut work = work();
        let result = loop {
            match cursor.poll(&mut work) {
                Ok(cfws::Status::Yield | cfws::Status::Comment(_)) => {}
                Ok(cfws::Status::Complete(_)) => break Ok(()),
                Err(error) => break Err(error),
            }
        };
        assert_eq!(
            result,
            if malformed {
                Err(cfws::Error::Malformed)
            } else {
                Ok(())
            }
        );
        assert_eq!(1_000_000 - work.visits, visits);
        assert_eq!(1_000_000 - work.records, records);
    }
    for (source, visits, records, malformed) in [
        ("\"🐈\"".as_bytes(), 7, 3, false),
        (b"\"a\r\nb".as_slice(), 5, 3, true),
        (b"\"\\\xc3", 4, 3, true),
    ] {
        let mut cursor = delimited::Cursor::new(source, 0, delimited::Kind::QuotedString);
        let mut work = work();
        let result = cursor.poll(&mut work);
        assert_eq!(result.is_err(), malformed);
        assert_eq!(1_000_000 - work.visits, visits);
        assert_eq!(1_000_000 - work.records, records);
    }
}
#[test]
fn late_failures_retire_comments_and_latch_syntax_and_work_errors() {
    for source in [b"() (bad".as_slice(), b"() (good)"] {
        let mut cursor = cfws::Cursor::new(source, 0);
        let mut admission = work();
        assert_eq!(
            cursor.poll(&mut admission),
            Ok(cfws::Status::Comment(cfws::Comment { start: 0, end: 2 }))
        );
        if source == b"() (good)" {
            admission.clock = true;
        }
        let expected = if admission.clock {
            cfws::Error::Work(Stop::Clock)
        } else {
            cfws::Error::Malformed
        };
        assert_eq!(cursor.poll(&mut admission), Err(expected));
        let mut fresh = work();
        assert_eq!(cursor.poll(&mut fresh), Err(expected));
        assert_eq!((fresh.visits, fresh.records), (1_000_000, 1_000_000));
    }
    let source = format!("\"{}\"", "a".repeat(100));
    let mut cursor = delimited::Cursor::new(source.as_bytes(), 0, delimited::Kind::QuotedString);
    let mut admission = work();
    assert_eq!(cursor.poll(&mut admission), Ok(delimited::Status::Yield));
    admission.clock = true;
    assert_eq!(
        cursor.poll(&mut admission),
        Err(delimited::Error::Work(Stop::Clock))
    );
    assert_eq!(
        cursor.poll(&mut work()),
        Err(delimited::Error::Work(Stop::Clock))
    );
}
#[test]
fn invalid_offsets_are_terminal_without_admission_or_overflow() {
    for start in [1, usize::MAX] {
        let mut admission = work();
        let mut cfws = cfws::Cursor::new(b"", start);
        assert_eq!(cfws.poll(&mut admission), Err(cfws::Error::InvalidState));
        assert_eq!(cfws.poll(&mut admission), Err(cfws::Error::InvalidState));
        let mut token = delimited::Cursor::new(b"", start, delimited::Kind::QuotedString);
        assert_eq!(
            token.poll(&mut admission),
            Err(delimited::Error::InvalidState)
        );
        assert_eq!(
            token.poll(&mut admission),
            Err(delimited::Error::InvalidState)
        );
        assert_eq!(
            (admission.visits, admission.records),
            (1_000_000, 1_000_000)
        );
    }
}
