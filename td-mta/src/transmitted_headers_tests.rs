#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::Time;
use std::sync::atomic::{AtomicU64, Ordering};
struct Now(AtomicU64);
impl Clock for Now {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        deadline(),
        Charge {
            io_bytes: 16_000_000,
            output_bytes: 16_000_000,
            records: 100_000,
            unlinks: 0,
        },
    )
}
fn filtered(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    let size = write(
        input,
        &Now(AtomicU64::new(1)),
        deadline(),
        &mut meter(),
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )?;
    assert_eq!(size, output.len() as u64);
    Ok(output)
}
#[test]
fn folded_duplicate_blind_headers_are_removed_without_rewriting_other_bytes() {
    for (input,expected) in [
        (b"\r\n".as_slice(),b"\r\n".as_slice()),
        (b"Bcc: hidden@example.test\r\n\r\n",b"\r\n"),
        (b"bCc : one@example.test\r\n\ttwo@example.test\r\nX: keep\r\nBcc: three@example.test\r\n\r\n",b"X: keep\r\n\r\n"),
        (b"From: sender@example.test\r\nResent-Bcc: hidden@example.test\r\n\tmore@example.test\r\nTo: recipient@example.test\r\n\r\n",b"From: sender@example.test\r\nTo: recipient@example.test\r\n\r\n"),
        (b"X-Bcc: unchanged\r\nX: Bcc: value\r\n Bcc: folded value\r\n\r\n",b"X-Bcc: unchanged\r\nX: Bcc: value\r\n Bcc: folded value\r\n\r\n"),
    ] { assert_eq!(filtered(input).unwrap(),expected); }
}
#[test]
fn exact_complete_header_extent_is_required_and_failure_keeps_output_provisional() {
    for input in [
        b"".as_slice(),
        b"X: value",
        b"X: value\r\n",
        b"X: value\n\n",
        b"X: value\r\n\r\nbody",
        b"X: value\r\nBcc: hidden\r\nmalformed\r\n\r\n",
    ] {
        assert_eq!(filtered(input), Err(Error::Invalid), "{input:?}");
    }
    let mut output = Vec::new();
    assert_eq!(
        write(
            b"X: value\r\nBcc: hidden\r\nmalformed\r\n\r\n",
            &Now(AtomicU64::new(1)),
            deadline(),
            &mut meter(),
            |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            }
        ),
        Err(Error::Invalid)
    );
    assert_eq!(output, b"X: value\r\n");
}
#[test]
fn bare_line_endings_are_refused_including_across_validation_chunks() {
    for input in [
        b"X: a\rBcc: hidden@example.test\r\n\r\n".as_slice(),
        b"X: a\nBcc: hidden@example.test\r\n\r\n",
        b"Bcc: hidden\rstill hidden\r\n\r\n",
        b"X: a\r\r\n\r\n",
    ] {
        assert_eq!(filtered(input), Err(Error::Invalid), "{input:?}");
    }
    let mut input = b"X: ".to_vec();
    input.resize(4095, b'a');
    input.extend_from_slice(b"\r\nBcc: hidden@example.test\r\n\r\n");
    let mut expected = input[..4097].to_vec();
    expected.extend_from_slice(b"\r\n");
    assert_eq!(filtered(&input).unwrap(), expected);
    input[4096] = b'x';
    assert_eq!(filtered(&input), Err(Error::Invalid));
}
#[test]
fn header_ceiling_and_output_quantum_are_exact() {
    let mut input = b"X: ".to_vec();
    input.resize(crate::limits::MIB - 2, b'a');
    input.extend_from_slice(b"\r\n\r\n");
    let mut emitted = 0;
    assert_eq!(
        write(
            &input,
            &Now(AtomicU64::new(1)),
            deadline(),
            &mut meter(),
            |bytes| {
                assert!(bytes.len() <= 4096);
                emitted += bytes.len();
                Ok(())
            }
        )
        .unwrap(),
        input.len() as u64
    );
    assert_eq!(emitted, input.len());
    input.insert(3, b'a');
    assert_eq!(filtered(&input), Err(Error::Invalid));
    let mut bcc = b"Bcc: ".to_vec();
    bcc.resize(131072, b'a');
    bcc.extend_from_slice(b"\r\nSubject: keep\r\n\r\n");
    assert_eq!(filtered(&bcc).unwrap(), b"Subject: keep\r\n\r\n");
}
#[test]
fn every_original_work_cutoff_refuses_and_exact_work_succeeds() {
    let input = b"From: x@y\r\nBcc: hidden@y\r\nTo: z@y\r\n\r\n";
    let now = Now(AtomicU64::new(1));
    let mut full = meter();
    write(input, &now, deadline(), &mut full, |_| Ok(())).unwrap();
    let used = Charge {
        io_bytes: meter().remaining().io_bytes - full.remaining().io_bytes,
        output_bytes: meter().remaining().output_bytes - full.remaining().output_bytes,
        records: meter().remaining().records - full.remaining().records,
        unlinks: 0,
    };
    for kind in 0..3 {
        let total = match kind {
            0 => used.io_bytes,
            1 => used.output_bytes,
            _ => used.records,
        };
        for available in 0..=total {
            let mut allowance = used;
            match kind {
                0 => allowance.io_bytes = available,
                1 => allowance.output_bytes = available,
                _ => allowance.records = available,
            }
            let result = write(
                input,
                &now,
                deadline(),
                &mut Meter::new(deadline(), allowance),
                |_| Ok(()),
            );
            if available == total {
                assert!(result.is_ok());
            } else {
                assert_eq!(
                    result,
                    Err(Error::Capacity),
                    "kind={kind} available={available}"
                );
            }
        }
    }
}
struct Backward(AtomicU64);
impl Clock for Backward {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 1,
            monotonic: Tick(u64::from(self.0.fetch_add(1, Ordering::Relaxed) == 0)),
        })
    }
}
#[test]
fn original_deadline_short_phase_sink_failure_and_regression_are_terminal() {
    let input = b"X: value\r\n\r\n";
    let now = Now(AtomicU64::new(1));
    let short = Deadline::after(Tick(0), 50).unwrap();
    let mut budget = meter();
    write(input, &now, short, &mut budget, |_| Ok(())).unwrap();
    assert_eq!(budget.deadline(), deadline().into());
    assert_eq!(
        write(
            input,
            &now,
            Deadline::after(Tick(0), 101).unwrap(),
            &mut meter(),
            |_| Ok(())
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        write(
            input,
            &Backward(AtomicU64::new(0)),
            deadline(),
            &mut meter(),
            |_| Ok(())
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        write(input, &now, short, &mut meter(), |_| Err(Error::Busy)),
        Err(Error::Busy)
    );
    assert_eq!(
        write(input, &now, short, &mut meter(), |_| {
            now.0.store(50, Ordering::Relaxed);
            Ok(())
        }),
        Err(Error::Deadline)
    );
    let mut called = false;
    assert_eq!(
        write(input, &now, short, &mut meter(), |_| {
            called = true;
            Ok(())
        }),
        Err(Error::Deadline)
    );
    assert!(!called);
}
#[test]
fn composed_draft_keeps_bcc_locally_while_transmitted_body_is_byte_identical() {
    use crate::compose_text::{Address, Message};
    let message = Message {
        from: Address {
            name: "Sender",
            email: "sender@example.test",
        },
        to: &[Address {
            name: "Recipient",
            email: "recipient@example.test",
        }],
        cc: &[],
        bcc: &[Address {
            name: "Hidden recipient with a folded display name",
            email: "hidden@example.test",
        }],
        reply_to: &[],
        subject: "Subject",
        message_id: "draft@example.test",
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text: "body with literal Bcc: text\nsecond line",
    };
    let mut stored = Vec::new();
    crate::compose_text::compose(
        &message,
        65536,
        &Now(AtomicU64::new(1)),
        &mut meter(),
        |bytes| {
            stored.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();
    let body = stored
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap()
        + 4;
    let mut transmitted = filtered(&stored[..body]).unwrap();
    let transmitted_body = transmitted.len();
    transmitted.extend_from_slice(&stored[body..]);
    assert_eq!(&transmitted[transmitted_body..], &stored[body..]);
    assert!(stored
        .windows(b"hidden@example.test".len())
        .any(|part| part == b"hidden@example.test"));
    assert!(!transmitted
        .windows(b"hidden@example.test".len())
        .any(|part| part == b"hidden@example.test"));
    assert!(transmitted
        .windows(b"Message-ID:\r\n <draft@example.test>".len())
        .any(|part| part == b"Message-ID:\r\n <draft@example.test>"));
}
