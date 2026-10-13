#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::ports::{Deadline, Time};
use std::sync::atomic::{AtomicU64, Ordering};
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.fetch_add(1, Ordering::Relaxed)),
        })
    }
}
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 1000000).unwrap(),
        Charge {
            io_bytes: 1000000,
            records: 1000000,
            output_bytes: 1000000,
            unlinks: 0,
        },
    )
}
fn message(text: &str) -> Message<'_> {
    Message {
        from: Address {
            name: "hé ☃",
            email: "sender@example.com",
        },
        to: &[],
        cc: &[],
        bcc: &[],
        reply_to: &[],
        subject: "hello",
        message_id: "td-mail.123@example.com",
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text,
    }
}
fn render(message: &Message<'_>, maximum: u64) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    let length = compose(
        message,
        maximum,
        &TestClock(AtomicU64::new(0)),
        &mut meter(),
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )?;
    assert_eq!(length, output.len() as u64);
    Ok(output)
}
fn decode_body(wire: &[u8]) -> Vec<u8> {
    let offset = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let source = &wire[offset..];
    let mut decoder = td_mime::base64::Decoder::default();
    let mut position = 0;
    let mut result = Vec::new();
    let mut work = meter();
    loop {
        let mut output = [0; 61];
        let step = decoder
            .poll(&source[position..], &mut output, true, Tick(1), &mut work)
            .unwrap();
        position += step.consumed;
        result.extend_from_slice(&output[..step.written]);
        if step.status == td_mime::base64::Status::Complete {
            break;
        }
    }
    assert!(!decoder.is_encoding_problem());
    result
}
#[test]
fn exact_plain_text_message_keeps_id_date_and_unicode() {
    let wire = render(&message("hello\n"), 10000).unwrap();
    assert_eq!(
        String::from_utf8(wire).unwrap(),
        concat!(
            "From:\r\n =?UTF-8?B?aMOpIOKYgw==?=\r\n <sender@example.com>\r\n",
            "Subject:\r\n =?UTF-8?B?aGVsbG8=?=\r\n",
            "Message-ID:\r\n <td-mail.123@example.com>\r\n",
            "Date: Thu, 01 Jan 1970 00:00:00 +0000\r\n",
            "MIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naGVsbG8NCg==\r\n"
        )
    );
}
#[test]
fn body_normalizes_only_newlines_across_buffer_edges() {
    for value in [
        "".to_string(),
        "\n\r\n\r\r\na\0é".to_string(),
        format!(
            "{}\r\n{}\r{}\n",
            "a".repeat(1023),
            "☃".repeat(700),
            "z".repeat(1024)
        ),
    ] {
        let wire = render(&message(&value), 100000).unwrap();
        let expected = value
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\n', "\r\n");
        assert_eq!(decode_body(&wire), expected.as_bytes());
        assert!(wire.split(|b| *b == b'\n').all(|line| line.len() <= 999));
    }
}
#[test]
fn recipients_reply_headers_and_bcc_remain_in_stored_copy() {
    let addresses = [
        Address {
            name: "one",
            email: "one@example.com",
        },
        Address {
            name: "",
            email: "two@example.com",
        },
    ];
    let mut input = message("body");
    input.to = &addresses;
    input.cc = &addresses[..1];
    input.bcc = &addresses[1..];
    input.reply_to = &addresses[..1];
    input.in_reply_to = &["parent@example.com"];
    input.references = &["root@example.com", "parent@example.com"];
    input.sent_at_ms = -1;
    let wire = String::from_utf8(render(&input, 10000).unwrap()).unwrap();
    assert!(wire.contains("Bcc:\r\n <two@example.com>\r\n"));
    assert!(wire.contains("<one@example.com>,\r\n <two@example.com>\r\n"));
    assert!(wire.contains("References:\r\n <root@example.com>\r\n <parent@example.com>\r\n"));
    assert!(wire.contains("Date: Wed, 31 Dec 1969 23:59:59 +0000\r\n"));
}
#[test]
fn malformed_generated_fields_refuse_and_exact_size_is_enforced() {
    for value in [
        "bad\r\nInjected: x",
        "two@example.com> <three@example.com",
        "(comment)one@example.com",
        "<one@example.com>",
        "",
    ] {
        let mut input = message("");
        input.message_id = value;
        assert_eq!(render(&input, 10000), Err(Error::Invalid), "{value:?}");
    }
    for address in ["a@example.com\r\nBcc: x", "bad", "@example.com"] {
        let mut input = message("");
        input.from.email = address;
        assert_eq!(render(&input, 10000), Err(Error::Invalid));
    }
    let mut input = message("");
    input.subject = "bad\r\nheader";
    assert_eq!(render(&input, 10000), Err(Error::Invalid));
    input.subject = "okay";
    input.sent_at_ms = i64::MAX;
    assert_eq!(render(&input, 10000), Err(Error::Invalid));
    let input = message("hello");
    let reference = render(&input, 10000).unwrap();
    assert_eq!(render(&input, reference.len() as u64).unwrap(), reference);
    assert_eq!(
        render(&input, reference.len() as u64 - 1),
        Err(Error::Quota)
    );
    assert_eq!(render(&input, 0), Err(Error::Quota));
}
#[test]
fn budget_deadline_sink_failure_and_clock_regression_never_succeed() {
    let input = message("hello");
    for stop in [Stop::IoBytes, Stop::Records, Stop::OutputBytes] {
        let mut capacity = meter().remaining();
        match stop {
            Stop::IoBytes => capacity.io_bytes = 0,
            Stop::Records => capacity.records = 0,
            _ => capacity.output_bytes = 0,
        }
        let mut work = Meter::new(Deadline::after(Tick(0), 1000000).unwrap(), capacity);
        assert_eq!(
            compose(
                &input,
                10000,
                &TestClock(AtomicU64::new(0)),
                &mut work,
                |_| Ok(())
            ),
            Err(Error::Capacity)
        );
        assert_eq!(work.stopped(), Some(stop));
    }
    let mut work = Meter::new(Deadline::after(Tick(0), 5).unwrap(), meter().remaining());
    assert_eq!(
        compose(
            &input,
            10000,
            &TestClock(AtomicU64::new(0)),
            &mut work,
            |_| Ok(())
        ),
        Err(Error::Deadline)
    );
    assert_eq!(
        compose(
            &input,
            10000,
            &TestClock(AtomicU64::new(0)),
            &mut meter(),
            |_| Err(Error::Busy)
        ),
        Err(Error::Busy)
    );
    struct Backwards(AtomicU64);
    impl Clock for Backwards {
        fn sample(&self) -> Result<Time, Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    2
                } else {
                    1
                }),
            })
        }
    }
    assert_eq!(
        compose(
            &input,
            10000,
            &Backwards(AtomicU64::new(0)),
            &mut meter(),
            |_| Ok(())
        ),
        Err(Error::Invalid)
    );
}

#[test]
fn obsolete_identifier_controls_cannot_enter_any_generated_field() {
    for id in [
        "\"a\\\nBcc: victim@evil.example\"@example.com",
        "\"a\n\tb\"@example.com",
        "\"\\\r\\\n\\\0\"@example.com",
        "\"\u{1}\u{7f}\"@example.com",
        "x@[\\\nInjected: y]",
        "\"a\r\n b\"@example.com",
        "\"okay\"@example.com",
        "x@\"example\"",
        "x@[[example]]",
        "x@[bad domain]",
    ] {
        for field in 0..3 {
            let mut input = message("");
            let ids = [id];
            match field {
                0 => input.message_id = id,
                1 => input.in_reply_to = &ids,
                _ => input.references = &ids,
            }
            assert_eq!(
                render(&input, 10000),
                Err(Error::Invalid),
                "field {field}: {id:?}"
            );
        }
    }
    for id in ["x@[IPv6:::1]", "x@[(a),;\"@]", "a.b+tag@domain.example"] {
        let mut input = message("");
        input.message_id = id;
        assert!(render(&input, 10000).is_ok(), "{id}");
    }
}

#[test]
fn identifier_and_list_ceiling_edges_are_exact() {
    for length in [995, 996] {
        let id = format!("{}@b", "a".repeat(length - 2));
        for field in 0..3 {
            let mut input = message("");
            let ids = [id.as_str()];
            match field {
                0 => input.message_id = &id,
                1 => input.in_reply_to = &ids,
                _ => input.references = &ids,
            }
            let result = render(&input, 10000);
            if length == 995 {
                let wire = result.unwrap();
                assert_eq!(
                    wire.split(|b| *b == b'\n').map(|line| line.len()).max(),
                    Some(999)
                );
            } else {
                assert_eq!(result, Err(Error::Invalid));
            }
        }
    }
    for count in [1000, 1001] {
        let addresses = vec![
            Address {
                name: "",
                email: "a@example.test"
            };
            count
        ];
        for field in 0..4 {
            let mut input = message("");
            match field {
                0 => input.to = &addresses,
                1 => input.cc = &addresses,
                2 => input.bcc = &addresses,
                _ => input.reply_to = &addresses,
            }
            let result = render(&input, 100000);
            if count == 1000 {
                assert!(result.is_ok());
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
        }
        let ids = vec!["a@b"; count];
        for field in 0..2 {
            let mut input = message("");
            if field == 0 {
                input.in_reply_to = &ids;
            } else {
                input.references = &ids;
            }
            let result = render(&input, 100000);
            if count == 1000 {
                assert!(result.is_ok());
            } else {
                assert_eq!(result, Err(Error::Capacity));
            }
        }
    }
}

#[test]
fn date_year_edges_match_the_declared_range() {
    for year in [1899, 1900, 9999, 10000] {
        let mut input = message("");
        input.sent_at_ms =
            td_civil::civil_to_unix_utc(&td_civil::Civil::new(year, 1, 1, 0, 0, 0)).unwrap() * 1000;
        let result = render(&input, 10000);
        if (1900..=9999).contains(&year) {
            assert!(String::from_utf8(result.unwrap())
                .unwrap()
                .contains(&format!("Jan {year} 00:00:00 +0000")));
        } else {
            assert_eq!(result, Err(Error::Invalid));
        }
    }
}
