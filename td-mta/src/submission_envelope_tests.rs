#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::Time;
use std::sync::atomic::{AtomicU64, Ordering};
struct Now(AtomicU64);
impl Clock for Now {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 1_000_000).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        deadline(),
        Charge {
            io_bytes: 10_000_000,
            output_bytes: 10_000_000,
            records: 1_000_000,
            unlinks: 0,
        },
    )
}
fn plan(headers: &[u8]) -> Result<Envelope<'_>, Error> {
    derive(
        headers,
        "user@example.test",
        1000,
        deadline(),
        &Now(AtomicU64::new(1)),
        &mut meter(),
    )
}
#[test]
fn emitted_draft_headers_derive_authorized_blind_recipients_without_rewriting() {
    use crate::compose_text::{self, Address, Message};
    let mut message = Vec::new();
    let address = |email| Address { name: "", email };
    compose_text::compose(
        &Message {
            from: address("\"user\"@EXAMPLE.test"),
            to: &[
                Address {
                    name: "A comma, é",
                    email: "first@example.test",
                },
                address("\"two words\"@example.test"),
            ],
            cc: &[address("first@EXAMPLE.test"), address("First@example.test")],
            bcc: &[
                address("hidden@example.test"),
                address("\"first\"@example.test"),
            ],
            reply_to: &[],
            subject: "Subject",
            message_id: "unique@example.test",
            in_reply_to: &[],
            references: &[],
            sent_at_ms: 0,
            text: "message body",
        },
        30000,
        &Now(AtomicU64::new(1)),
        &mut meter(),
        |bytes| {
            message.extend_from_slice(bytes);
            Ok(())
        },
    )
    .unwrap();
    let end = message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    let envelope = plan(&message[..end]).unwrap();
    assert_eq!(envelope.mail_from(), "\"user\"@EXAMPLE.test");
    assert_eq!(
        envelope.recipients().collect::<Vec<_>>(),
        [
            "first@example.test",
            "\"two words\"@example.test",
            "First@example.test",
            "hidden@example.test"
        ]
    );
    assert_eq!(format!("{envelope:?}"), "SubmissionEnvelope(<redacted>)");
}
#[test]
fn sender_and_from_must_both_match_the_selected_identity() {
    let source = b"From: user@example.test\r\nSender: \"user\"@EXAMPLE.test\r\nTo: dest@example.test\r\n\r\n";
    assert_eq!(plan(source).unwrap().mail_from(), "\"user\"@EXAMPLE.test");
    let source =
        b"From: user@example.test\r\nSender: other@example.test\r\nTo: dest@example.test\r\n\r\n";
    assert_eq!(plan(source).unwrap_err(), Error::ForbiddenFrom);
    for from in [
        "User@example.test",
        "postmaster@example.test",
        "other@example.test",
    ] {
        let source = format!("From: {from}\r\nTo: dest@example.test\r\n\r\n");
        assert_eq!(plan(source.as_bytes()).unwrap_err(), Error::ForbiddenFrom);
    }
}
#[test]
fn ambiguous_invalid_and_unsupported_addresses_refuse_without_partial_envelope() {
    for (source, expected) in [
        (
            "To: x@example.test\r\n\r\n",
            Error::InvalidEmail(Field::From),
        ),
        (
            "From: user@example.test\r\nFrom: user@example.test\r\nTo: x@example.test\r\n\r\n",
            Error::InvalidEmail(Field::From),
        ),
        (
            "From: user@example.test, other@example.test\r\nTo: x@example.test\r\n\r\n",
            Error::InvalidEmail(Field::From),
        ),
        (
            "From: user@example.test\r\nSender: a@example.test\r\nSender: b@example.test\r\nTo: x@example.test\r\n\r\n",
            Error::InvalidEmail(Field::Sender),
        ),
        (
            "From: user@example.test\r\nSender: \r\nTo: x@example.test\r\n\r\n",
            Error::InvalidEmail(Field::Sender),
        ),
        (
            "From: user@example.test\r\nTo: x@example.test, broken\r\n\r\n",
            Error::InvalidEmail(Field::To),
        ),
        (
            "From: user@example.test\r\nCc: ü@example.test\r\n\r\n",
            Error::InvalidEmail(Field::Cc),
        ),
        (
            "From: user@example.test\r\nBcc: Group: x@example.test;\r\n\r\n",
            Error::InvalidEmail(Field::Bcc),
        ),
        (
            "From: user@example.test\r\nBcc: x@[127.0.0.1]\r\n\r\n",
            Error::InvalidEmail(Field::Bcc),
        ),
        ("From: user@example.test\r\n\r\n", Error::NoRecipients),
        (
            "From: user@example.test\r\nTo: x@example.test\r\n\r\nbody",
            Error::Headers,
        ),
        (
            "From: user@example.test\r\nTo: x@example.test",
            Error::Headers,
        ),
        (
            "From: user@example.test\nTo: x@example.test\n\n",
            Error::Headers,
        ),
    ] {
        assert_eq!(plan(source.as_bytes()).unwrap_err(), expected, "{source:?}");
    }
}
#[test]
fn recipient_bound_counts_unique_addresses_and_preserves_first_source_order() {
    let mut source = String::from("From: user@example.test\r\nTo: ");
    for index in (0..1000).rev() {
        if index != 999 {
            source.push_str(", ");
        }
        source.push_str(&format!("r{index}@EXAMPLE.test"));
    }
    source.push_str("\r\nCc: r500@example.test\r\nBcc: r0@example.test\r\n\r\n");
    let envelope = plan(source.as_bytes()).unwrap();
    let addresses: Vec<_> = envelope.recipients().collect();
    assert_eq!(addresses.len(), 1000);
    assert_eq!(addresses.first(), Some(&"r999@EXAMPLE.test"));
    assert_eq!(addresses.last(), Some(&"r0@EXAMPLE.test"));
    let clock = Now(AtomicU64::new(1));
    assert_eq!(
        derive(
            source.as_bytes(),
            "user@example.test",
            999,
            deadline(),
            &clock,
            &mut meter()
        )
        .unwrap_err(),
        Error::TooManyRecipients
    );
    source.truncate(source.len() - 2);
    source.push_str("Bcc: extra@example.test\r\n\r\n");
    assert_eq!(
        plan(source.as_bytes()).unwrap_err(),
        Error::TooManyRecipients
    );
}
#[test]
fn original_work_deadline_and_caller_bounds_are_enforced() {
    let source = b"From: user@example.test\r\nTo: x@example.test\r\n\r\n";
    let clock = Now(AtomicU64::new(1));
    for maximum in [0, 1001] {
        assert_eq!(
            derive(
                source,
                "user@example.test",
                maximum,
                deadline(),
                &clock,
                &mut meter()
            )
            .unwrap_err(),
            Error::Policy(ports::Error::Invalid)
        );
    }
    for identity in ["invalid", "*@example.test", "\"*\"@example.test"] {
        assert_eq!(
            derive(source, identity, 1000, deadline(), &clock, &mut meter()).unwrap_err(),
            Error::Policy(ports::Error::Invalid)
        );
    }
    let mut used = meter();
    derive(
        source,
        "user@example.test",
        1000,
        deadline(),
        &clock,
        &mut used,
    )
    .unwrap();
    let original = meter().remaining();
    let consumed = Charge {
        io_bytes: original.io_bytes - used.remaining().io_bytes,
        records: original.records - used.remaining().records,
        output_bytes: original.output_bytes - used.remaining().output_bytes,
        unlinks: 0,
    };
    for records in [false, true] {
        let total = if records {
            consumed.records
        } else {
            consumed.io_bytes
        };
        for amount in [0, total - 1, total] {
            let mut allowance = consumed;
            if records {
                allowance.records = amount;
            } else {
                allowance.io_bytes = amount;
            }
            let result = derive(
                source,
                "user@example.test",
                1000,
                deadline(),
                &clock,
                &mut Meter::new(deadline(), allowance),
            );
            if amount == total {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err(), Error::Policy(ports::Error::Capacity));
            }
        }
    }
    clock.0.store(100, Ordering::Relaxed);
    let short = Deadline::after(Tick(0), 100).unwrap();
    let mut original = meter();
    assert_eq!(
        derive(
            source,
            "user@example.test",
            1000,
            short,
            &clock,
            &mut original
        )
        .unwrap_err(),
        Error::Policy(ports::Error::Deadline)
    );
    assert_eq!(original.deadline(), deadline().into());
    assert_eq!(
        derive(
            source,
            "user@example.test",
            1000,
            deadline(),
            &clock,
            &mut Meter::new(short, meter().remaining())
        )
        .unwrap_err(),
        Error::Policy(ports::Error::Invalid)
    );
}

#[test]
fn clock_regression_and_final_sample_expiry_withhold_the_envelope() {
    struct Jump {
        calls: AtomicU64,
        change_at: u64,
        next: u64,
    }
    impl Clock for Jump {
        fn sample(&self) -> Result<Time, ports::Error> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if call >= self.change_at { self.next } else { 1 }),
            })
        }
    }
    let source = b"From: user@example.test\r\nTo: x@example.test\r\n\r\n";
    let baseline = Jump {
        calls: AtomicU64::new(0),
        change_at: u64::MAX,
        next: 1,
    };
    derive(
        source,
        "user@example.test",
        1000,
        deadline(),
        &baseline,
        &mut meter(),
    )
    .unwrap();
    let calls = baseline.calls.load(Ordering::Relaxed);
    for at in [1, calls - 1] {
        for (next, expected) in [
            (0, ports::Error::Invalid),
            (1_000_000, ports::Error::Deadline),
        ] {
            let clock = Jump {
                calls: AtomicU64::new(0),
                change_at: at,
                next,
            };
            assert_eq!(
                derive(
                    source,
                    "user@example.test",
                    1000,
                    deadline(),
                    &clock,
                    &mut meter()
                )
                .unwrap_err(),
                Error::Policy(expected)
            );
        }
    }
}
