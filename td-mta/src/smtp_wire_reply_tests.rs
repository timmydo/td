#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use crate::smtp_wire::{LINE_BYTES, REPLY_BYTES};

#[test]
fn multiline_normalization_is_fragment_independent_and_keeps_the_tail() {
    let wire = b"550-5.7.1 Message refused by policy\r\n550 5.7.1 choose another route.\r\n";
    let expected = "550 5.7.1 Message refused by policy choose another route.";
    for split in 0..=wire.len() {
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        let first = reader.feed(&wire[..split]).unwrap();
        assert_eq!(first.consumed, split);
        if split < wire.len() {
            assert_eq!(reader.value(), None);
            assert_eq!(reader.code(), None);
        }
        let mut second = wire[split..].to_vec();
        second.extend_from_slice(b"250 another reply\r\n");
        let last = reader.feed(&second).unwrap();
        assert_eq!(last.consumed, wire.len() - split);
        assert!(last.complete);
        assert_eq!(reader.code(), Some(550));
        assert_eq!(reader.value(), Some(expected));
        assert_eq!(reader.finish().unwrap(), expected);
        assert_eq!(
            reader.feed(b"tail").unwrap(),
            Progress {
                consumed: 0,
                complete: true
            }
        );
    }
}
#[test]
fn first_line_is_the_reference_for_every_later_prefix() {
    for (wire, expected) in [
        (b"250\r\n".as_slice(), "250"),
        (b"250 \r\n", "250 "),
        (b"250-abc\r\n250-abd\r\n250 abef\r\n", "250 abc d ef"),
        (b"250-one\r\n250-two\r\n250 two\r\n", "250 one two two"),
        (b"250-same\r\n250 same\r\n", "250 same "),
        (b"250-\tfirst\r\n250 \tlast\r\n", "250 \tfirst last"),
    ] {
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        for byte in wire {
            reader.feed(std::slice::from_ref(byte)).unwrap();
        }
        assert_eq!(reader.finish().unwrap(), expected);
    }
}
fn sized_reply(last: usize) -> Vec<u8> {
    let mut wire = Vec::new();
    wire.extend_from_slice(b"250-");
    wire.extend_from_slice(&[b'A'; 506]);
    wire.extend_from_slice(b"\r\n");
    for _ in 0..7 {
        wire.extend_from_slice(b"250-");
        wire.extend_from_slice(&[b'B'; 506]);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"250 ");
    wire.extend(std::iter::repeat_n(b'B', last));
    wire.extend_from_slice(b"\r\n");
    wire
}
#[test]
fn exact_reply_cell_fits_but_one_more_byte_never_exposes_a_success() {
    for (last, fits) in [(36, true), (37, false)] {
        let wire = sized_reply(last);
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        let result = reader.feed(&wire);
        if fits {
            assert_eq!(
                result.unwrap(),
                Progress {
                    consumed: wire.len(),
                    complete: true
                }
            );
            assert_eq!(reader.finish().unwrap().len(), MAX_SMTP_REPLY);
        } else {
            assert_eq!(result, Err(Error::Capacity));
            assert_eq!(reader.value(), None);
            assert_eq!(reader.code(), None);
            assert_eq!(reader.feed(b"250 accepted\r\n"), Err(Error::Capacity));
            assert_eq!(reader.finish(), Err(Error::Capacity));
        }
    }
}
#[test]
fn wire_limit_still_bounds_repeated_lines_with_tiny_normalized_growth() {
    for lines in [32, 33] {
        let mut wire = Vec::new();
        for n in 0..lines {
            wire.extend_from_slice(if n + 1 == lines { b"250 " } else { b"250-" });
            wire.extend_from_slice(&[b'X'; 506]);
            wire.extend_from_slice(b"\r\n");
        }
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        if lines == 32 {
            assert_eq!(wire.len(), REPLY_BYTES);
            assert!(reader.feed(&wire).unwrap().complete);
            assert_eq!(reader.value().unwrap().len(), 541);
        } else {
            assert_eq!(reader.feed(&wire), Err(Error::Capacity));
            assert_eq!(reader.value(), None);
        }
    }
}
#[test]
fn incomplete_malformed_and_mismatched_replies_are_sticky_refusals() {
    for wire in [
        b"250-first\r\n550 final\r\n".as_slice(),
        b"250 ok\n",
        b"250 ok\rX",
        b"250 \xff\r\n",
        b"250\tbad\r\n",
    ] {
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        assert_eq!(reader.feed(wire), Err(Error::Invalid));
        assert_eq!(reader.value(), None);
        assert_eq!(reader.code(), None);
        assert_eq!(reader.feed(b"250 accepted\r\n"), Err(Error::Invalid));
    }
    for wire in [
        b"".as_slice(),
        b"250 accepted",
        b"250 accepted\r",
        b"250-partial\r\n",
    ] {
        let mut line = [0; LINE_BYTES];
        let mut output = [0; MAX_SMTP_REPLY];
        let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
        assert!(!reader.feed(wire).unwrap().complete);
        assert_eq!(reader.finish(), Err(Error::Invalid));
        assert_eq!(reader.feed(b"250 accepted\r\n"), Err(Error::Invalid));
        assert_eq!(reader.value(), None);
    }
}
#[test]
fn storage_and_line_limits_are_independent() {
    let mut line = [0; LINE_BYTES];
    let mut output = [0; MAX_SMTP_REPLY];
    assert!(matches!(
        NormalizedReplyReader::new(&mut line[..511], &mut output),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        NormalizedReplyReader::new(&mut line, &mut output[..4095]),
        Err(Error::Capacity)
    ));
    let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
    let mut wire = b"250 ".to_vec();
    wire.extend_from_slice(&[b'X'; 507]);
    wire.extend_from_slice(b"\r\n");
    assert_eq!(reader.feed(&wire), Err(Error::Capacity));
    assert_eq!(reader.value(), None);
    assert!(std::mem::size_of::<NormalizedReplyReader<'_>>() <= 256);
}

#[test]
fn normalized_tabs_round_trip_through_recipient_rows_only() {
    use crate::format::{row::*, Table};
    use crate::ids::AttemptId;
    let mut line = [0; LINE_BYTES];
    let mut output = [0; MAX_SMTP_REPLY];
    let mut reader = NormalizedReplyReader::new(&mut line, &mut output).unwrap();
    reader.feed(b"250-\taccepted\r\n250 \tqueued\r\n").unwrap();
    let reply = reader.finish().unwrap();
    assert_eq!(reply, "250 \taccepted queued");
    let recipient = RecipientRow {
        address: "user@example.test",
        state: RecipientState::Accepted,
        uncertain: false,
        attempt: Some(AttemptId::from_bytes([1; 16])),
        attempt_count: 1,
        last_attempt_at: Some(1),
        phase: AttemptPhase::Final,
        next_attempt_at: None,
        rcpt_reply: Some(reply),
        data_reply: Some(reply),
        reason: FailureReason::None,
        diagnostic: "",
    };
    let row = Row::Recipient(recipient);
    let mut bytes = vec![0; row.encoded_len().unwrap()];
    row.encode(&mut bytes).unwrap();
    assert_eq!(Row::decode(Table::Recipients, &bytes).unwrap(), row);
    for control in ['\0', '\r', '\n', '\u{7f}', '\u{85}'] {
        let bad = format!("250 {control}accepted");
        for (rcpt_reply, data_reply) in [
            (Some(bad.as_str()), Some(reply)),
            (Some(reply), Some(bad.as_str())),
        ] {
            assert!(Row::Recipient(RecipientRow {
                rcpt_reply,
                data_reply,
                ..recipient
            })
            .encoded_len()
            .is_err());
        }
    }
    assert!(Row::Recipient(RecipientRow {
        diagnostic: "local\tdetail",
        ..recipient
    })
    .encoded_len()
    .is_err());
    assert!(Row::Recipient(RecipientRow {
        address: "user\t@example.test",
        ..recipient
    })
    .encoded_len()
    .is_err());
}
