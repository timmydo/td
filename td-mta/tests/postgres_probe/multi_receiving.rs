use super::{deadline, sql, Fixture};
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
    ids::AccountId,
    ports::{Clock, Deadline},
    postgres_receiving as receiving,
    postgres_runtime_profile::PublishedProfile,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &td_crypto::TrustStore,
    profile: &PublishedProfile,
) {
    let a = AccountId::from_bytes([0xaa; 16]);
    let b = AccountId::from_bytes([0xcc; 16]);
    let mut all = vec![0; 4096];
    let mut a_bytes = vec![0; 4096];
    let mut b_bytes = vec![0; 4096];
    let recipients = ReceiptRecipients::encode(
        &[
            "second@example.test",
            "main@example.test",
            "alias@example.test",
        ],
        &mut all,
    )
    .unwrap();
    let a_recipients =
        ReceiptRecipients::encode(&["main@example.test", "alias@example.test"], &mut a_bytes)
            .unwrap();
    let b_recipients = ReceiptRecipients::encode(&["second@example.test"], &mut b_bytes).unwrap();
    let targets = [
        receiving::Target {
            binding: profile.binding_for(b).unwrap(),
            recipients: b_recipients,
        },
        receiving::Target {
            binding: profile.binding_for(a).unwrap(),
            recipients: a_recipients,
        },
    ];
    let clock = RuntimeClock::new();
    let message = receiving::Message {
        bytes: b"Message-ID: <fanout@example.test>\r\nSubject: Both accounts\r\n\r\nprivate delivery facts\r\n",
        receipt: SmtpReceipt { peer: "192.0.2.1".parse().unwrap(), gateway: None, tls: ReceiptTls::Plain, ehlo: "sender.example.test", reverse_path: "sender@example.test", recipients },
        hostname: "localhost", extended: true, header_bytes: 16384, received_at: 1_700_000_000_000,
    };
    let prepare = || {
        let cap = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let mut meter = Meter::new(
            cap,
            Charge {
                io_bytes: 64 * 1024 * 1024,
                output_bytes: 64 * 1024 * 1024,
                records: 1024 * 1024,
                ..Default::default()
            },
        );
        receiving::prepare_fanout(&targets, &message, deadline(), &mut meter, &clock).unwrap()
    };
    let first = prepare();
    assert_eq!(first.count(), 2);
    let left = receiving::publish_fanout(request, trust, deadline(), &first).unwrap();
    assert_eq!(
        left.accounts
            .iter()
            .map(|(account, _)| *account)
            .collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
    assert_eq!(
        sql(fixture, "SELECT count(DISTINCT blob_id) FROM td_mta.emails"),
        "1"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.smtp_receipt_recipients WHERE (account=decode(repeat('aa',16),'hex') AND address NOT IN ('main@example.test','alias@example.test')) OR (account=decode(repeat('cc',16),'hex') AND address<>'second@example.test')"), "0");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.smtp_receipt_recipients"
        ),
        "3"
    );
    let baseline = sql(fixture, "SELECT string_agg(encode(id,'hex')||':'||encode(sequence,'hex'),',' ORDER BY id) FROM td_mta.accounts");
    let fresh = prepare();
    sql(fixture, "UPDATE td_mta.mailboxes SET role=NULL WHERE account=decode(repeat('cc',16),'hex') AND role='inbox'");
    assert_eq!(
        receiving::publish_fanout(request, trust, deadline(), &fresh),
        Err(receiving::Error::MissingMailbox)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
    assert_eq!(sql(fixture, "SELECT string_agg(encode(id,'hex')||':'||encode(sequence,'hex'),',' ORDER BY id) FROM td_mta.accounts"), baseline);
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET role='inbox' WHERE account=decode(repeat('cc',16),'hex')",
    );
    let used = sql(
        fixture,
        "SELECT body_bytes FROM td_mta.account_usage WHERE account=decode(repeat('cc',16),'hex')",
    );
    sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={} WHERE account=decode(repeat('cc',16),'hex')", profile.binding_for(b).unwrap().quotas().body_bytes));
    assert_eq!(
        receiving::publish_fanout(request, trust, deadline(), &fresh),
        Err(receiving::Error::Quota)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
    sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={used} WHERE account=decode(repeat('cc',16),'hex')"));
    sql(fixture, "CREATE FUNCTION public.delay_multi_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_multi_receiving AFTER INSERT ON td_mta.emails DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_multi_receiving()");
    assert_eq!(
        receiving::publish_fanout(
            request,
            trust,
            std::time::Instant::now() + std::time::Duration::from_secs(3),
            &fresh
        ),
        Err(receiving::Error::Indeterminate)
    );
    std::thread::sleep(std::time::Duration::from_secs(9));
    sql(
        fixture,
        "DROP TRIGGER delay_multi_receiving ON td_mta.emails",
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "4");
    // Two account-scoped copies exceed the old single-upload 40-MiB wire cap.
    let mut bytes = b"Subject: Large fanout\r\n\r\n".to_vec();
    let mut line = vec![b'x'; 998];
    line.extend_from_slice(b"\r\n");
    while bytes.len() < 21 * 1024 * 1024 {
        bytes.extend_from_slice(&line);
    }
    let large = receiving::Message {
        bytes: &bytes,
        ..message
    };
    let cap = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    let mut meter = Meter::new(
        cap,
        Charge {
            io_bytes: 1024 * 1024 * 1024,
            output_bytes: 128 * 1024 * 1024,
            records: 4 * 1024 * 1024,
            ..Default::default()
        },
    );
    let large = receiving::prepare_fanout(
        &targets,
        &large,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
        &mut meter,
        &clock,
    )
    .unwrap();
    assert_eq!(
        receiving::publish_fanout(
            request,
            trust,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
            &large
        )
        .unwrap()
        .accounts
        .len(),
        2
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "6");
}

pub(super) fn smtp(fixture: &Fixture, address: std::net::SocketAddr, many: bool) {
    use std::io::{BufRead, BufReader, Write};
    let stream = std::net::TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut stream = BufReader::new(stream);
    let reply = |stream: &mut BufReader<std::net::TcpStream>, expected: &str| loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).unwrap(), 0);
        assert!(line.starts_with(expected), "{line}");
        if line.as_bytes().get(3) == Some(&b' ') {
            break;
        }
    };
    reply(&mut stream, "220");
    let mut commands: Vec<(String, &str)> = [
        ("EHLO sender.example.test\r\n", "250"),
        ("AUTH PLAIN ignored\r\n", "500"),
        ("MAIL FROM:<sender@example.test>\r\n", "250"),
        ("RCPT TO:<outside@external.test>\r\n", "550"),
        ("RCPT TO:<unknown@example.test>\r\n", "550"),
        ("RCPT TO:<second@example.test>\r\n", "250"),
        ("RCPT TO:<main@example.test>\r\n", "250"),
        ("RCPT TO:<alias@example.test>\r\n", "250"),
    ]
    .into_iter()
    .map(|(line, code)| (line.to_owned(), code))
    .collect();
    if many {
        for index in 1..=14 {
            commands.push((format!("RCPT TO:<extra{index}@example.test>\r\n"), "250"));
        }
    }
    commands.extend(
        [
            ("DATA\r\n", "354"),
            (
                "Subject: SMTP fanout\r\n\r\nBoth local accounts\r\n.\r\n",
                "250",
            ),
        ]
        .into_iter()
        .map(|(line, code)| (line.to_owned(), code)),
    );
    for (line, expected) in commands {
        stream.get_mut().write_all(line.as_bytes()).unwrap();
        stream.get_mut().flush().unwrap();
        reply(&mut stream, expected);
    }
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails"),
        if many { "24" } else { "8" }
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.smtp_receipt_recipients WHERE (account=decode(repeat('aa',16),'hex') AND address NOT IN ('main@example.test','alias@example.test')) OR (account=decode(repeat('cc',16),'hex') AND address<>'second@example.test')"), "0");
    // All recipients retain the same trusted observation in their immutable copy.
    assert_eq!(sql(fixture, "SELECT bool_and(accounts >= 2 AND variants = 1 AND observed)
        FROM (SELECT blob_id, count(DISTINCT account) AS accounts, count(DISTINCT bytes) AS variants,
            bool_and(position(convert_to(E'\\r\\nX-TD-DKIM-Results: none; receiver=localhost\\r\\n', 'UTF8') in bytes) > 0) AS observed
        FROM td_mta.blob_chunks WHERE ordinal=0 AND position(convert_to('Subject: SMTP fanout', 'UTF8') in bytes)>0 GROUP BY blob_id) AS copies"), "t");
    if many {
        // Sequence increments are visible even while A's deferred trigger holds COMMIT.
        sql(fixture, "CREATE SEQUENCE public.release_smtp_commit; GRANT SELECT ON SEQUENCE public.release_smtp_commit TO mail_runtime;
            CREATE FUNCTION public.extend_smtp_commit() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                PERFORM set_config('statement_timeout','70s',true);
                RETURN NEW;
            END $$;
            CREATE TRIGGER extend_smtp_commit BEFORE INSERT ON td_mta.emails
            FOR EACH ROW WHEN (NEW.account=decode(repeat('aa',16),'hex'))
            EXECUTE FUNCTION public.extend_smtp_commit();
            CREATE FUNCTION public.hold_smtp_commit() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                FOR i IN 1..600 LOOP
                    IF (SELECT is_called FROM public.release_smtp_commit) THEN RETURN NEW; END IF;
                    PERFORM pg_sleep(0.1);
                END LOOP;
                RAISE EXCEPTION 'SMTP fixture release timed out';
            END $$;
            CREATE CONSTRAINT TRIGGER hold_smtp_commit AFTER INSERT ON td_mta.emails
            DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.account=decode(repeat('aa',16),'hex'))
            EXECUTE FUNCTION public.hold_smtp_commit()");
        for (recipient, expected) in [("main@example.test", "451"), ("second@example.test", "250")]
        {
            for (line, code) in [
                ("MAIL FROM:<sender@example.test>\r\n".to_string(), "250"),
                (format!("RCPT TO:<{recipient}>\r\n"), "250"),
                ("DATA\r\n".to_string(), "354"),
                (
                    "Subject: independent delivery\r\n\r\nbody\r\n.\r\n".to_string(),
                    expected,
                ),
            ] {
                stream.get_mut().write_all(line.as_bytes()).unwrap();
                stream.get_mut().flush().unwrap();
                reply(&mut stream, code);
            }
        }
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "25");
        sql(fixture, "SELECT nextval('public.release_smtp_commit')");
        sql(fixture, "DROP TRIGGER hold_smtp_commit ON td_mta.emails");
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "26");
    }
    stream.get_mut().write_all(b"QUIT\r\n").unwrap();
    stream.get_mut().flush().unwrap();
    reply(&mut stream, "221");
}
