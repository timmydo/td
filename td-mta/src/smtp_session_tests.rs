#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::{
    config::{
        routing::{AliasSlot, Builder, DomainSlot},
        syntax::Location,
    },
    format::Sequence,
    ids::StoreEpoch,
};
use std::num::NonZeroU32;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
fn fixture(run: impl FnOnce(&Routing<'_>)) {
    let mut text = [0; 1024];
    let mut domains = [DomainSlot::EMPTY; 1];
    let mut aliases = [AliasSlot::EMPTY; 3];
    let at = Location {
        line: NonZeroU32::new(1).unwrap(),
        column: 1,
    };
    let mut b = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
    b.account(ACCOUNT, at).unwrap();
    b.domain("example.test", at).unwrap();
    b.alias("Alice@example.test", ACCOUNT, at).unwrap();
    b.alias("alias@example.test", ACCOUNT, at).unwrap();
    b.alias("\"a>b\"@example.test", ACCOUNT, at).unwrap();
    run(&b.finish().unwrap());
}
fn session<'a>(routes: &'a Routing<'a>) -> Session<'a> {
    let mut s = Session::new(
        routes,
        Settings {
            hostname: "mx.example.test",
            message_bytes: 4096 + 256,
            trace_bytes: 256,
            recipients: 100,
            starttls: true,
        },
    )
    .unwrap();
    assert!(reply(&s).starts_with("220 mx.example.test"));
    s.reply_sent().unwrap();
    s
}
fn reply<'s>(s: &'s Session<'_>) -> &'s str {
    match s.pending() {
        Pending::Reply { bytes, .. } => std::str::from_utf8(bytes).unwrap(),
        other => panic!("{other:?}"),
    }
}
fn command(s: &mut Session<'_>, line: &str, code: &str) {
    assert_eq!(s.feed(line.as_bytes()).unwrap(), line.len());
    assert!(reply(s).starts_with(code), "{}", reply(s));
    s.reply_sent().unwrap();
}
fn begin(s: &mut Session<'_>, body: &str) {
    command(s, "EHLO sender.test\r\n", "250-");
    command(s, &format!("MAIL FROM:<> {body}\r\n"), "250 ");
    command(s, "RCPT TO:<Alice@example.test>\r\n", "250 ");
    assert_eq!(s.feed(b"DATA\r\n").unwrap(), 6);
    assert_eq!(
        s.pending(),
        Pending::BeginData {
            maximum_bytes: s.settings.message_bytes
        }
    );
    assert_eq!(s.feed(b"ignored"), Err(Error::Conflict));
    s.data_ready(Ok(())).unwrap();
    assert!(reply(s).starts_with("354 "));
    s.reply_sent().unwrap();
}
fn receipt() -> Commit {
    Commit {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([2; 16]),
        sequence: Sequence::from_u64(1),
    }
}
#[test]
fn sequence_routes_aliases_and_reset() {
    fixture(|routes| {
        let mut s = session(routes);
        command(&mut s, "MAIL FROM:<>\r\n", "503 ");
        command(&mut s, "VRFY secret@example.test\r\n", "252 ");
        command(&mut s, "RSET\r\n", "250 ");
        command(&mut s, "EHLO sender.test\r\n", "250-");
        command(&mut s, "MAIL FROM:<>\r\n", "250 ");
        command(&mut s, "MAIL FROM:<other@example.test>\r\n", "503 ");
        for (address, code) in [
            ("alice@example.test", "550 "),
            ("alias@other.test", "550 "),
            ("Alice@EXAMPLE.TEST", "250 "),
            ("alias@example.test", "250 "),
            ("PoStMaStEr", "250 "),
            ("PoStMaStEr@example.test", "250 "),
            ("\"a>b\"@example.test", "250 "),
        ] {
            command(&mut s, &format!("RCPT TO:<{address}>\r\n"), code);
        }
        let e = s.envelope().unwrap();
        assert_eq!(e.account, ACCOUNT);
        assert_eq!(e.recipients.len(), 5);
        assert!(e.reverse_path.is_empty());
        command(&mut s, "EHLO bad_domain\r\n", "501 ");
        assert_eq!(s.envelope().unwrap().recipients.len(), 5);
        command(&mut s, "RSET\r\n", "250 ");
        assert!(s.envelope().is_none());
        command(&mut s, "DATA\r\n", "503 ");
        command(&mut s, "HELO [IPv6:::1]\r\n", "250 ");
        command(&mut s, "MAIL FROM:<> SIZE=10\r\n", "555 ");
        command(&mut s, "MAIL FROM:<x@[192.0.2.1]>\r\n", "250 ");
        command(&mut s, "EHLO again.test\r\n", "250-");
        assert!(s.envelope().is_none());
        command(&mut s, "AUTH PLAIN\r\n", "500 ");
        for bad in [
            "QUIT extra\r\n",
            "RSET extra\r\n",
            "DATA extra\r\n",
            "STARTTLS extra\r\n",
            "STARTTLS \r\n",
            "VRFY\r\n",
        ] {
            command(&mut s, bad, "501 ");
        }
        command(&mut s, "QUIT\r\n", "221 ");
        assert_eq!(s.pending(), Pending::Closed);
    });
}
#[test]
fn transcript_every_split_preserves_octets_and_waits_for_publication() {
    fixture(|routes| {
        let wire = b"Subject: example\r\n\r\n..stuffed\r\n.dot\r\n\xff\r\n.\r\n";
        let expected = b"Subject: example\r\n\r\n.stuffed\r\ndot\r\n\xff\r\n";
        for split in 0..=wire.len() {
            let mut s = session(routes);
            begin(&mut s, "BODY=8BITMIME SIZE=1");
            let mut stored = Vec::new();
            for mut input in [&wire[..split], &wire[split..]] {
                while !input.is_empty() {
                    let n = s.feed(input).unwrap();
                    input = &input[n..];
                    match s.pending() {
                        Pending::Data(bytes) => {
                            stored.extend_from_slice(bytes);
                            s.data_written(Ok(())).unwrap();
                        }
                        Pending::Commit => assert!(input.is_empty()),
                        Pending::Input => (),
                        other => panic!("{other:?}"),
                    }
                }
            }
            assert_eq!(stored, expected);
            assert_eq!(s.pending(), Pending::Commit);
            assert_eq!(s.feed(b"QUIT\r\n"), Err(Error::Conflict));
            s.committed(Ok(receipt())).unwrap();
            assert!(reply(&s).starts_with("250 2.0.0 Message accepted"));
            assert_eq!(s.feed(b"QUIT\r\n"), Err(Error::Conflict));
            s.reply_sent().unwrap();
            assert!(s.envelope().is_none());
        }
    });
}
#[test]
fn command_every_split_and_unread_tail() {
    fixture(|routes| {
        let wire = b"EHLO sender.test\r\n";
        for split in 0..=wire.len() {
            let mut s = session(routes);
            s.feed(&wire[..split]).unwrap();
            if split < wire.len() {
                s.feed(&wire[split..]).unwrap();
            }
            let response = reply(&s);
            assert!(response.contains("8BITMIME"));
            assert!(response.contains("SIZE 4096"));
            for unsupported in [
                "PIPELINING",
                "AUTH",
                "SMTPUTF8",
                "CHUNKING",
                "BINARYMIME",
                "DSN",
            ] {
                assert!(!response.contains(unsupported));
            }
        }
        let mut s = session(routes);
        assert_eq!(s.feed(b"NOOP\r\nQUIT\r\n").unwrap(), 6);
        assert_eq!(s.feed(b"QUIT\r\n"), Err(Error::Conflict));
        s.reply_sent().unwrap();
        command(&mut s, "QUIT\r\n", "221 ");
    });
}
#[test]
fn data_failures_are_terminal_and_cannot_smuggle_commands() {
    fixture(|routes| {
        for body in [
            b"a\n.\nQUIT\r\n".as_slice(),
            b"a\rb\r\n.\r\n",
            b"\0\r\n.\r\n",
            b"\xff\r\n.\r\n",
        ] {
            let mut s = session(routes);
            begin(&mut s, "BODY=7BIT");
            s.feed(body).unwrap();
            assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
            assert!(!reply(&s).starts_with("250"));
            s.reply_sent().unwrap();
            assert_eq!(s.pending(), Pending::Closed);
            assert_eq!(s.feed(b"QUIT\r\n"), Err(Error::Conflict));
        }
        let mut s = session(routes);
        begin(&mut s, "BODY=8BITMIME");
        s.feed(b"a\r\n").unwrap();
        s.data_written(Err(Error::Quota)).unwrap();
        assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
    });
}
#[test]
fn sizes_line_limits_and_recipient_exhaustion() {
    fixture(|routes| {
        let mut s = session(routes);
        command(&mut s, "EHLO sender.test\r\n", "250-");
        for (arg, code) in [
            ("SIZE=4097", "552 "),
            ("SIZE=99999999999999999999", "552 "),
            ("SIZE=1 SIZE=2", "501 "),
            ("SIZE=-1", "501 "),
            ("BODY=8BITMIME BODY=7BIT", "501 "),
            ("BODY=BINARYMIME", "501 "),
            ("SMTPUTF8", "555 "),
        ] {
            command(&mut s, &format!("MAIL FROM:<> {arg}\r\n"), code);
            assert!(s.envelope().is_none());
        }
        command(&mut s, "MAIL FROM:<>\r\n", "250 ");
        for _ in 0..100 {
            command(&mut s, "RCPT TO:<postmaster>\r\n", "250 ");
        }
        command(&mut s, "RCPT TO:<alias@example.test>\r\n", "452 ");
        for (count, dot, valid) in [
            (998, false, true),
            (999, false, false),
            (998, true, true),
            (999, true, false),
        ] {
            let mut s = session(routes);
            begin(&mut s, "");
            let mut line = Vec::new();
            if dot {
                line.push(b'.');
            }
            line.extend(vec![b'x'; count]);
            line.extend(b"\r\n");
            s.feed(&line).unwrap();
            assert_eq!(matches!(s.pending(), Pending::Data(_)), valid);
            if !valid {
                assert_eq!(reply(&s), "554 5.6.0 Message line too long\r\n");
            }
        }
        let mut s = session(routes);
        s.settings.message_bytes = s.settings.trace_bytes + 3;
        begin(&mut s, "");
        s.feed(b"x\r\n").unwrap();
        s.data_written(Ok(())).unwrap();
        s.feed(b"\r\n.\r\n").unwrap();
        assert!(reply(&s).starts_with("552 "));
        assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
    });
}
#[test]
fn commit_outcomes_and_begin_refusal() {
    fixture(|routes| {
        for outcome in [
            Err(CommitFailure::Rejected(Error::Quota)),
            Err(CommitFailure::Indeterminate(Error::Io {
                kind: std::io::ErrorKind::Other,
                os_code: None,
            })),
            Ok(Commit {
                account: AccountId::from_bytes([9; 16]),
                ..receipt()
            }),
        ] {
            let mut s = session(routes);
            begin(&mut s, "");
            s.feed(b".\r\nQUIT\r\n").unwrap();
            assert_eq!(s.pending(), Pending::Commit);
            s.committed(outcome).unwrap();
            if matches!(outcome, Err(CommitFailure::Rejected(_))) {
                assert!(reply(&s).starts_with("451 "));
                s.reply_sent().unwrap();
                assert_eq!(s.pending(), Pending::Input);
            } else {
                assert_eq!(s.pending(), Pending::Closed);
            }
        }
        let mut s = session(routes);
        command(&mut s, "EHLO sender.test\r\n", "250-");
        command(&mut s, "MAIL FROM:<>\r\n", "250 ");
        command(&mut s, "RCPT TO:<postmaster>\r\n", "250 ");
        s.feed(b"DATA\r\n").unwrap();
        s.data_ready(Err(Error::Quota)).unwrap();
        assert!(reply(&s).starts_with("451 "));
        assert!(s.envelope().is_none());
    });
}
#[test]
fn starttls_handoff_resets_and_refuses_plaintext_tail() {
    fixture(|routes| {
        let mut s = session(routes);
        command(&mut s, "STARTTLS\r\n", "503 ");
        command(&mut s, "EHLO sender.test\r\n", "250-");
        s.feed(b"STARTTLS\r\n").unwrap();
        assert_eq!(
            s.pending(),
            Pending::StartTls {
                command: b"STARTTLS\r\n"
            }
        );
        if let Pending::StartTls { command } = s.pending() {
            let mut storage = [0; crate::smtp_wire::LINE_BYTES];
            let mut reader = crate::smtp_wire::LineReader::new(&mut storage).unwrap();
            let progress = reader.feed(command).unwrap();
            assert!(progress.complete);
            assert_eq!(progress.consumed, command.len());
            assert_eq!(reader.line(), Some(b"STARTTLS".as_slice()));
        } else {
            panic!("missing upgrade");
        }
        assert_eq!(s.feed(b"EHLO attacker\r\n"), Err(Error::Conflict));
        s.tls_established().unwrap();
        command(&mut s, "MAIL FROM:<>\r\n", "503 ");
        s.feed(b"EHLO sender.test\r\n").unwrap();
        assert!(!reply(&s).contains("STARTTLS"));
        s.reply_sent().unwrap();
        command(&mut s, "STARTTLS\r\n", "502 ");
        let mut s = session(routes);
        command(&mut s, "EHLO sender.test\r\n", "250-");
        s.feed(b"STARTTLS\r\nMAIL FROM:<>\r\n").unwrap();
        assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
        assert_eq!(s.tls_established(), Err(Error::Conflict));
    });
}
#[test]
fn smtp_paths_are_not_header_addresses_or_relay_instructions() {
    for (wire, expected) in [
        ("FROM:<>", ""),
        (
            "FROM:<@first.test,@next.test:user@example.test>",
            "user@example.test",
        ),
        ("FROM:<\"\"@example.test>", "\"\"@example.test"),
        ("FROM:<\"a>b\"@example.test>", "\"a>b\"@example.test"),
        ("FROM:<x@[IPv6:::1]>", "x@[IPv6:::1]"),
    ] {
        assert_eq!(path(wire, "FROM:", true).unwrap().0, expected);
    }
    for wire in [
        "FROM:<x@example.test>junk",
        "FROM:<x@example.test>\tSIZE=1",
        "FROM:<x..y@example.test>",
        "FROM:<Name <x@example.test>>",
        "FROM:<x@[999.0.0.1]>",
        "FROM:<@bad_domain:x@example.test>",
        "FROM:<é@example.test>",
        "FROM:<@first.test:>",
        "FROM: <x@example.test>",
        "FROM :<x@example.test>",
    ] {
        assert!(path(wire, "FROM:", true).is_none(), "{wire}");
    }
}

#[test]
fn retained_recipients_fit_the_actual_receipt_codec() {
    fixture(|routes| {
        let mut s = session(routes);
        s.settings.recipients = MAX_RECIPIENTS as usize;
        command(&mut s, "EHLO sender.test\r\n", "250-");
        command(&mut s, "MAIL FROM:<>\r\n", "250 ");
        let address = r#""\P\o\S\t\M\a\S\t\E\r"@example.test"#;
        let count = (MAX_RECEIPT_BYTES - 4) / (4 + address.len());
        // Use the longest equivalent postmaster spelling this fixture accepts.
        assert!(count < MAX_RECIPIENTS as usize);
        for _ in 0..count {
            command(&mut s, &format!("RCPT TO:<{address}>\r\n"), "250 ");
        }
        command(&mut s, &format!("RCPT TO:<{address}>\r\n"), "452 ");
        let e = s.envelope().unwrap();
        let recipients: Vec<_> = e.recipients.iter().map(String::as_str).collect();
        let mut output = [0; MAX_RECEIPT_BYTES];
        let receipt =
            crate::format::row::ReceiptRecipients::encode(&recipients, &mut output).unwrap();
        assert_eq!(receipt.count() as usize, count);
    });
}

#[test]
fn trace_allowance_reduces_size_offer_but_not_the_storage_reservation() {
    fixture(|routes| {
        let settings = || Settings {
            hostname: "mx.example.test",
            message_bytes: MAX_MESSAGE_BYTES,
            trace_bytes: 1024,
            recipients: 100,
            starttls: false,
        };
        for (trace, recipients) in [
            (0, 100),
            (MAX_MESSAGE_BYTES, 100),
            (MAX_MESSAGE_BYTES + 1, 100),
            (1024, 99),
            (1024, 1001),
        ] {
            let mut config = settings();
            config.trace_bytes = trace;
            config.recipients = recipients;
            assert!(matches!(Session::new(routes, config), Err(Error::Invalid)));
        }
        let mut s = Session::new(routes, settings()).unwrap();
        s.reply_sent().unwrap();
        let incoming = MAX_MESSAGE_BYTES - 1024;
        assert_eq!(s.incoming_limit(), incoming);
        s.feed(b"EHLO sender.test\r\n").unwrap();
        assert!(reply(&s).contains(&format!("250-SIZE {incoming}\r\n")));
        s.reply_sent().unwrap();
        command(
            &mut s,
            &format!("MAIL FROM:<> SIZE={}\r\n", incoming + 1),
            "552 ",
        );
        command(&mut s, &format!("MAIL FROM:<> SIZE={incoming}\r\n"), "250 ");
        command(&mut s, "RCPT TO:<postmaster>\r\n", "250 ");
        s.feed(b"DATA\r\n").unwrap();
        assert_eq!(
            s.pending(),
            Pending::BeginData {
                maximum_bytes: MAX_MESSAGE_BYTES
            }
        );
    });
}

#[test]
fn temporary_data_refusal_precedes_the_service_close_notice() {
    fixture(|routes| {
        let mut s = session(routes);
        begin(&mut s, "");
        s.feed(b".\r\n").unwrap();
        assert_eq!(s.service_unavailable(), Err(Error::Conflict));
        s.committed(Err(CommitFailure::Rejected(Error::Forbidden)))
            .unwrap();
        assert!(reply(&s).starts_with("451 "));
        assert_eq!(s.service_unavailable(), Err(Error::Conflict));
        s.reply_sent().unwrap();
        s.service_unavailable().unwrap();
        assert!(reply(&s).starts_with("421 4.3.2 "));
        assert_eq!(s.feed(b"MAIL FROM:<>\r\n"), Err(Error::Conflict));
        s.reply_sent().unwrap();
        assert_eq!(s.pending(), Pending::Closed);
        let mut s = session(routes);
        s.feed(b"NO").unwrap();
        assert_eq!(s.service_unavailable(), Err(Error::Conflict));
        s.abort();
        assert_eq!(s.pending(), Pending::Closed);
    });
}

#[test]
fn command_lengths_follow_base_and_extended_limits() {
    fixture(|routes| {
        for (length, extended, prefix, code, closed) in [
            (512, false, "NOOP ", "250 ", false),
            (513, false, "NOOP ", "500 ", false),
            (554, true, "MAIL FROM:<> SIZE=1 ", "250 ", false),
            (554, false, "MAIL FROM:<> ", "500 ", false),
            (555, true, "MAIL FROM:<> SIZE=1 ", "500 ", true),
        ] {
            let mut s = session(routes);
            command(
                &mut s,
                if extended {
                    "EHLO sender.test\r\n"
                } else {
                    "HELO sender.test\r\n"
                },
                "250",
            );
            let line = format!("{prefix}{}\r\n", " ".repeat(length - 2 - prefix.len()));
            assert_eq!(line.len(), length);
            s.feed(line.as_bytes()).unwrap();
            assert!(reply(&s).starts_with(code), "{}", reply(&s));
            assert_eq!(
                matches!(s.pending(), Pending::Reply { close: true, .. }),
                closed
            );
        }
    });
}

#[test]
fn control_octets_default_seven_bit_and_tls_policy_refusals() {
    fixture(|routes| {
        for input in [b"NOOP\0\r\n".as_slice(), b"NOOP\xff\r\n"] {
            let mut s = session(routes);
            s.feed(input).unwrap();
            assert_eq!(reply(&s), "501 5.5.2 Invalid command framing\r\n");
            assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
        }
        let mut s = session(routes);
        begin(&mut s, "");
        s.feed(b"\xff\r\n").unwrap();
        assert!(matches!(s.pending(), Pending::Reply { close: true, .. }));
        let mut s = session(routes);
        s.settings.starttls = false;
        s.feed(b"EHLO sender.test\r\n").unwrap();
        assert!(!reply(&s).contains("STARTTLS"));
        s.reply_sent().unwrap();
        command(&mut s, "STARTTLS\r\n", "502 ");
        let mut s = session(routes);
        command(&mut s, "EHLO sender.test\r\n", "250-");
        command(&mut s, "MAIL FROM:<>\r\n", "250 ");
        command(&mut s, "STARTTLS\r\n", "503 ");
    });
}

#[test]
fn pending_debug_never_formats_the_raw_message_bytes() {
    fixture(|routes| {
        let mut s = session(routes);
        begin(&mut s, "");
        s.feed(b"private mail content\r\n").unwrap();
        assert_eq!(format!("{:?}", s.pending()), "Data(<redacted>)");
    });
}
