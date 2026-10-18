#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    compose_text,
    ports::{self, Time},
};
use std::sync::atomic::{AtomicU64, Ordering};
use td_json::arena::{self, Token};
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
    Deadline::after(Tick(0), 100).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        deadline(),
        Charge {
            io_bytes: 1_000_000,
            output_bytes: 1_000_000,
            records: 100_000,
            unlinks: 0,
        },
    )
}
const BASE: &str = r#"{"mailboxIds":{"m02020202020202020202020202020202":true},"keywords":{"$draft":true,"$seen":true},"from":[{"name":"Élodie","email":"sender@example.test"}],"to":[{"email":"recipient@example.test"}],"bcc":[{"name":null,"email":"hidden@example.test"}],"subject":"A subject","messageId":["draft@example.test"],"sentAt":"1970-01-01T00:00:00Z","textBody":[{"partId":"text","type":"text/plain"}],"bodyValues":{"text":{"value":"hello\nworld","isEncodingProblem":false,"isTruncated":false}}}"#;
fn with_value(text: &str, run: impl FnOnce(Value<'_>)) {
    let mut bytes = text.as_bytes().to_vec();
    let mut tokens = vec![Token::EMPTY; 16384];
    let document = arena::parse(
        &mut bytes,
        &mut tokens,
        arena::ParseLimits {
            max_bytes: 1_048_576,
            max_depth: 16,
            max_tokens: 16384,
        },
    )
    .unwrap();
    run(document.root());
}
#[test]
fn td_mail_plain_text_shape_borrows_strings_and_composes_the_stored_copy() {
    with_value(BASE, |value| {
        let clock = Now(AtomicU64::new(1));
        let mut budget = meter();
        let draft = parse(value, BASE.len(), &clock, deadline(), &mut budget).unwrap();
        assert_eq!(draft.mailbox, MailboxId::from_bytes([2; 16]));
        let input = draft.message();
        assert_eq!(input.from.name, "Élodie");
        assert_eq!(input.to[0].name, "");
        assert_eq!(input.bcc[0].email, "hidden@example.test");
        assert_eq!(input.text, "hello\nworld");
        assert_eq!(input.sent_at_ms, 0);
        let body = value
            .as_object()
            .unwrap()
            .find(|(key, _)| *key == "bodyValues")
            .unwrap()
            .1
            .as_object()
            .unwrap()
            .next()
            .unwrap()
            .1
            .as_object()
            .unwrap()
            .find(|(key, _)| *key == "value")
            .unwrap()
            .1
            .as_str()
            .unwrap();
        assert!(std::ptr::eq(input.text.as_ptr(), body.as_ptr()));
        let mut output = Vec::new();
        compose_text::compose(&input, 65536, &clock, &mut budget, |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .unwrap();
        assert!(output
            .windows(b"<hidden@example.test>".len())
            .any(|part| part == b"<hidden@example.test>"));
        assert!(output.ends_with(b"aGVsbG8NCndvcmxk\r\n"));
        assert!(budget.remaining().records < meter().remaining().records);
    });
}
#[test]
fn non_client_shapes_and_incomplete_body_values_refuse() {
    for (old, new, field) in [
        (r#""$seen":true"#, r#""$seen":false"#, "keywords"),
        (r#""$draft":true,"#, "", "keywords"),
        (
            r#""isTruncated":false"#,
            r#""isTruncated":true"#,
            "bodyValues",
        ),
        (
            r#""isEncodingProblem":false"#,
            r#""isEncodingProblem":true"#,
            "bodyValues",
        ),
        (r#""isTruncated":false"#, r#""other":false"#, "bodyValues"),
        (r#""partId":"text""#, r#""partId":"other""#, "textBody"),
        (
            r#""type":"text/plain""#,
            r#""type":"text/html""#,
            "textBody",
        ),
        (
            r#""from":[{"name":"Élodie","email":"sender@example.test"}]"#,
            r#""from":[]"#,
            "from",
        ),
        (
            r#""messageId":["draft@example.test"]"#,
            r#""messageId":[]"#,
            "messageId",
        ),
        (
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:00+00:00",
            "sentAt",
        ),
        (
            r#""from":[{"name":"Élodie","email":"sender@example.test"}]"#,
            r#""from":[{"email":"a@example.test"},{"email":"b@example.test"}]"#,
            "from",
        ),
        (
            r#""messageId":["draft@example.test"]"#,
            r#""messageId":["a@example.test","b@example.test"]"#,
            "messageId",
        ),
        ("1970-01-01T00:00:00Z", "1970-01-01t00:00:00Z", "sentAt"),
        ("1970-01-01T00:00:00Z", "1970-02-30T00:00:00Z", "sentAt"),
        (
            r#""m02020202020202020202020202020202":true"#,
            r#""m02020202020202020202020202020202":false"#,
            "mailboxIds",
        ),
    ] {
        let text = BASE.replace(old, new);
        assert_ne!(text, BASE);
        with_value(&text, |value| {
            assert!(
                matches!(parse(value,text.len(),&Now(AtomicU64::new(1)),deadline(),&mut meter()),Err(Error::Property(actual)) if actual == field),
                "{field}: {text}"
            )
        });
    }
    for (extra, field) in [
        (r#", "htmlBody":[]"#, "htmlBody"),
        (r#", "attachments":[{"blobId":"b123"}]"#, "attachments"),
        (r#", "preview":"text""#, "preview"),
        (r#", "unknown":true"#, "create"),
    ] {
        let text = format!("{}{extra}}}", BASE.strip_suffix('}').unwrap());
        with_value(&text, |value| {
            assert!(matches!(
                parse(
                    value,
                    text.len(),
                    &Now(AtomicU64::new(1)),
                    deadline(),
                    &mut meter()
                ),
                Err(Error::Property(actual)) if actual == field
            ))
        });
    }
}
#[test]
fn original_work_text_capacity_and_shorter_deadline_are_enforced() {
    with_value(BASE, |value| {
        let clock = Now(AtomicU64::new(1));
        assert!(matches!(
            parse(value, 1, &clock, deadline(), &mut meter()),
            Err(Error::Capacity)
        ));
        let mut empty = Meter::new(deadline(), Charge::default());
        assert!(matches!(
            parse(value, BASE.len(), &clock, deadline(), &mut empty),
            Err(Error::Capacity)
        ));
        let short = Deadline::after(Tick(0), 50).unwrap();
        let mut original = meter();
        assert!(parse(value, BASE.len(), &clock, short, &mut original).is_ok());
        assert_eq!(original.deadline(), deadline().into());
        clock.0.store(50, Ordering::Relaxed);
        assert!(matches!(
            parse(value, BASE.len(), &clock, short, &mut original),
            Err(Error::Deadline)
        ));
    });
}
#[test]
fn descriptor_lists_accept_1000_and_refuse_1001_as_invalid_properties() {
    for count in [1000, 1001] {
        for field in ["references", "inReplyTo", "to", "cc", "bcc", "replyTo"] {
            let entry = if matches!(field, "to" | "cc" | "bcc" | "replyTo") {
                r#"{"email":"x@example.test"}"#
            } else {
                r#""id@example.test""#
            };
            let base = BASE
                .replace(r#""to":[{"email":"recipient@example.test"}],"#, "")
                .replace(
                    r#""bcc":[{"name":null,"email":"hidden@example.test"}],"#,
                    "",
                );
            let text = format!(
                "{},\"{field}\":[{}]}}",
                base.strip_suffix('}').unwrap(),
                vec![entry; count].join(",")
            );
            with_value(&text, |value| {
                let result = parse(
                    value,
                    text.len(),
                    &Now(AtomicU64::new(1)),
                    deadline(),
                    &mut meter(),
                );
                if count == 1000 {
                    assert!(result.is_ok(), "{field}/{count}");
                } else {
                    assert!(
                        matches!(result, Err(Error::Property(actual)) if actual == field),
                        "{field}/{count}"
                    );
                }
            });
        }
    }
}

#[test]
fn every_record_cutoff_withholds_the_draft_and_exact_capacity_succeeds() {
    with_value(BASE, |value| {
        let clock = Now(AtomicU64::new(1));
        let mut complete = meter();
        parse(value, BASE.len(), &clock, deadline(), &mut complete).unwrap();
        let cost = meter().remaining().records - complete.remaining().records;
        for records in 0..=cost {
            let mut budget = Meter::new(
                deadline(),
                Charge {
                    records,
                    ..meter().remaining()
                },
            );
            let result = parse(value, BASE.len(), &clock, deadline(), &mut budget);
            assert_eq!(result.is_ok(), records == cost, "records={records}");
        }
    });
}
struct Backward(AtomicU64);
impl Clock for Backward {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(u64::from(self.0.fetch_add(1, Ordering::Relaxed) == 0)),
        })
    }
}
#[test]
fn clock_regression_missing_required_fields_and_empty_attachments() {
    with_value(BASE, |value| {
        assert!(matches!(
            parse(
                value,
                BASE.len(),
                &Backward(AtomicU64::new(0)),
                deadline(),
                &mut meter()
            ),
            Err(Error::Clock)
        ))
    });
    for (old, field) in [
        (r#""subject":"A subject","#, "subject"),
        (r#""sentAt":"1970-01-01T00:00:00Z","#, "sentAt"),
        (r#""messageId":["draft@example.test"],"#, "messageId"),
    ] {
        let text = BASE.replace(old, "");
        assert_ne!(text, BASE);
        with_value(&text, |value| {
            assert!(
                matches!(parse(value,text.len(),&Now(AtomicU64::new(1)),deadline(),&mut meter()),Err(Error::Property(actual)) if actual == field)
            )
        });
    }
    let text = format!("{},\"attachments\":[]}}", BASE.strip_suffix('}').unwrap());
    with_value(&text, |value| {
        assert!(parse(
            value,
            text.len(),
            &Now(AtomicU64::new(1)),
            deadline(),
            &mut meter()
        )
        .is_ok())
    });
}

#[test]
fn date_boundaries_are_exact_and_do_not_clamp_seconds() {
    for (date, valid) in [
        ("1970-01-01T00:00:59Z", true),
        ("1970-01-01T00:00:60Z", false),
        ("1970-01-01T00:00:99Z", false),
        ("0000-01-01T00:00:00Z", false),
        ("1899-12-31T23:59:59Z", false),
        ("1900-01-01T00:00:00Z", true),
        ("9999-12-31T23:59:59Z", true),
    ] {
        let text = BASE.replace("1970-01-01T00:00:00Z", date);
        with_value(&text, |value| {
            let result = parse(
                value,
                text.len(),
                &Now(AtomicU64::new(1)),
                deadline(),
                &mut meter(),
            );
            if valid {
                let actual = result.unwrap().message().sent_at_ms;
                assert_eq!(actual, td_civil::parse_rfc3339(date).unwrap() * 1000);
            } else {
                assert!(matches!(result, Err(Error::Property("sentAt"))), "{date}");
            }
        });
    }
}
#[test]
fn caller_guards_and_mailbox_id_kind_are_checked() {
    with_value(BASE, |value| {
        let clock = Now(AtomicU64::new(1));
        assert!(matches!(
            parse(
                value,
                4 * crate::limits::MIB + 1,
                &clock,
                deadline(),
                &mut meter()
            ),
            Err(Error::Capacity)
        ));
        assert!(matches!(
            parse(
                value,
                BASE.len(),
                &clock,
                Deadline::after(Tick(0), 101).unwrap(),
                &mut meter()
            ),
            Err(Error::Invalid)
        ));
    });
    for kind in ["e", "t"] {
        let text = BASE.replace(
            "m02020202020202020202020202020202",
            &format!("{kind}02020202020202020202020202020202"),
        );
        with_value(&text, |value| {
            assert!(matches!(
                parse(
                    value,
                    text.len(),
                    &Now(AtomicU64::new(1)),
                    deadline(),
                    &mut meter()
                ),
                Err(Error::Property("mailboxIds"))
            ));
        });
    }
}

#[test]
fn attachment_descriptors_are_bounded_and_require_local_uploaded_blobs() {
    let descriptor = r#"{"blobId":"b03030303030303030303030303030303","name":"résumé.bin","type":"application/octet-stream","disposition":"attachment","header:Content-Description:asText":"A file"}"#;
    for count in [1, 64, 65] {
        let text = format!(
            "{},\"attachments\":[{}]}}",
            BASE.strip_suffix('}').unwrap(),
            vec![descriptor; count].join(",")
        );
        with_value(&text, |value| {
            let result = parse(
                value,
                text.len(),
                &Now(AtomicU64::new(1)),
                deadline(),
                &mut meter(),
            );
            if count <= 64 {
                let draft = result.unwrap();
                assert_eq!(draft.attachments.len(), count);
                assert_eq!(draft.attachments[0].name, "résumé.bin");
                assert_eq!(draft.attachments[0].blob, BlobId::from_bytes([3; 16]));
            } else {
                assert!(matches!(result, Err(Error::Property("attachments"))));
            }
        });
    }
    for (old, new) in [
        (
            "b03030303030303030303030303030303",
            "e03030303030303030303030303030303",
        ),
        ("application/octet-stream", "multipart/mixed"),
        ("attachment", "inline"),
        ("résumé.bin", ""),
        ("résumé.bin", r"bad\nname"),
    ] {
        let text = format!(
            "{},\"attachments\":[{}]}}",
            BASE.strip_suffix('}').unwrap(),
            descriptor.replace(old, new)
        );
        with_value(&text, |value| {
            assert!(matches!(
                parse(
                    value,
                    text.len(),
                    &Now(AtomicU64::new(1)),
                    deadline(),
                    &mut meter()
                ),
                Err(Error::Property("attachments"))
            ))
        });
    }
}
