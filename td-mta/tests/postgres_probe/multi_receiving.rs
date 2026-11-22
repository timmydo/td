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
    let cap = profile.binding_for(b).unwrap().quotas().body_bytes;
    assert_eq!(cap, profile.binding_for(a).unwrap().quotas().body_bytes);
    let used = |account: &str| -> u64 {
        sql(fixture, &format!("SELECT body_bytes FROM td_mta.account_usage WHERE account=decode(repeat('{account}',16),'hex')")).parse().unwrap()
    };
    // Put usage exactly at the cap, returning the real usage to restore.
    let fill = |account: &str| {
        let real = used(account);
        sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={cap} WHERE account=decode(repeat('{account}',16),'hex')"));
        real
    };
    let unfill = |account: &str, real: u64| {
        sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes=body_bytes-{cap}+{real} WHERE account=decode(repeat('{account}',16),'hex')"));
    };
    // With every account full nothing is stored, not even a notice.
    let real_a = fill("aa");
    let real_b = fill("cc");
    assert_eq!(
        receiving::publish_fanout(request, trust, deadline(), &fresh),
        Err(receiving::Error::Quota)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
    assert_eq!(sql(fixture, "SELECT string_agg(encode(id,'hex')||':'||encode(sequence,'hex'),',' ORDER BY id) FROM td_mta.accounts"), baseline);
    unfill("aa", real_a);
    // One full account takes a compact local notice; the other the message.
    let partial = prepare();
    let outcome = receiving::publish_fanout(request, trust, deadline(), &partial).unwrap();
    assert_eq!(
        outcome
            .accounts
            .iter()
            .map(|(account, _)| *account)
            .collect::<Vec<_>>(),
        vec![a]
    );
    assert_eq!(outcome.notices.len(), 1);
    let (noticed, notice) = outcome.notices[0];
    assert_eq!(noticed, b);
    assert!(notice.new_sequence > notice.old_sequence);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "4");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails e JOIN td_mta.memberships m ON m.account=e.account AND m.email_id=e.id JOIN td_mta.mailboxes x ON x.account=m.account AND x.id=m.mailbox_id AND x.role='inbox' WHERE e.account=decode(repeat('cc',16),'hex') AND e.origin=4"), "1");
    assert_eq!(sql(fixture, "SELECT encode(sequence,'hex') FROM td_mta.accounts WHERE id=decode(repeat('cc',16),'hex')"), format!("{:016x}", notice.new_sequence));
    let text = refused_notice(fixture, "cc");
    assert!(text.contains("quota is full"), "{text}");
    assert!(
        text.contains("Sender (unverified): sender@example.test\n"),
        "{text}"
    );
    assert!(text.contains("Recipient: second@example.test"), "{text}");
    assert!(!text.contains("main@example.test"), "{text}");
    // The full account was charged exactly the compact notice, past its cap.
    let charged = used("cc") - cap;
    assert!(charged > 0 && charged <= 16 * 1024, "{charged}");
    assert_eq!(sql(fixture, "SELECT b.length FROM td_mta.emails e JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id WHERE e.account=decode(repeat('cc',16),'hex') AND e.origin=4"), charged.to_string());
    unfill("cc", real_b);
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
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "6");
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
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "8");
}

pub(super) fn smtp(fixture: &Fixture, address: std::net::SocketAddr, many: bool, body_cap: u64) {
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
                "From: Owner <main@example.test>\r\nSubject: SMTP fanout\r\n\r\nBoth local accounts\r\n.\r\n",
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
        if many { "30" } else { "10" }
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.smtp_receipt_recipients WHERE (account=decode(repeat('aa',16),'hex') AND address NOT IN ('main@example.test','alias@example.test')) OR (account=decode(repeat('cc',16),'hex') AND address<>'second@example.test')"), "0");
    // All recipients retain the same trusted observation in their immutable copy.
    assert_eq!(sql(fixture, "SELECT bool_and(accounts >= 2 AND variants = 1 AND observed)
        FROM (SELECT blob_id, count(DISTINCT account) AS accounts, count(DISTINCT bytes) AS variants,
            bool_and(position(convert_to(E'\\r\\nAuthentication-Results: localhost;\\r\\n', 'UTF8') in bytes) > 0 AND
                position(convert_to(E'\\r\\n\\tdkim=none;\\r\\n\\tdmarc=fail (policy=', 'UTF8') in bytes) > 0) AS observed
        FROM td_mta.blob_chunks WHERE ordinal=0 AND position(convert_to('Subject: SMTP fanout', 'UTF8') in bytes)>0 GROUP BY blob_id) AS copies"), "t");
    // Each account's Email has its own rows stating the same results.
    assert_eq!(sql(fixture, "SELECT count(*) = count(a.email_id) AND bool_and(a.dkim_complete AND a.dmarc_result = 'fail' AND a.dmarc_author = 'example.test')
        FROM td_mta.emails e JOIN td_mta.blob_chunks c ON c.account = e.account AND c.blob_id = e.blob_id AND c.ordinal = 0
        LEFT JOIN td_mta.email_authentication a ON a.account = e.account AND a.email_id = e.id
        WHERE position(convert_to('Subject: SMTP fanout', 'UTF8') in c.bytes) > 0"), "t");
    if !many {
        let count = |account: &str, origin: u8| {
            sql(fixture, &format!("SELECT count(*) FROM td_mta.emails WHERE account=decode(repeat('{account}',16),'hex') AND origin={origin}"))
        };
        // Put B's usage exactly at its cap; restore what it really holds.
        let usage = "SELECT body_bytes FROM td_mta.account_usage WHERE account=decode(repeat('cc',16),'hex')";
        let fill = || {
            let real: u64 = sql(fixture, usage).parse().unwrap();
            sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={body_cap} WHERE account=decode(repeat('cc',16),'hex')"));
            real
        };
        let unfill = |real: u64| {
            sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes=body_bytes-{body_cap}+{real} WHERE account=decode(repeat('cc',16),'hex')"));
        };
        let mut send = |line: &str, code: &str| {
            stream.get_mut().write_all(line.as_bytes()).unwrap();
            stream.get_mut().flush().unwrap();
            reply(&mut stream, code);
        };
        let (a, b, notices) = (count("aa", 1), count("cc", 1), count("cc", 4));
        let plus = |count: &str, n: u64| (count.parse::<u64>().unwrap() + n).to_string();
        // An account already full is refused at RCPT alone.
        let real = fill();
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        send("RCPT TO:<second@example.test>\r\n", "452 4.2.2 ");
        send("RCPT TO:<main@example.test>\r\n", "250");
        send("DATA\r\n", "354");
        send("Subject: full at RCPT\r\n\r\nbody\r\n.\r\n", "250");
        assert_eq!(count("aa", 1), plus(&a, 1));
        assert_eq!(
            (count("cc", 1), count("cc", 4)),
            (b.clone(), notices.clone())
        );
        unfill(real);
        // An account that fills after its RCPT gets a local notice instead,
        // and the other account still receives the message with a 250.
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        send("RCPT TO:<second@example.test>\r\n", "250");
        let real = fill();
        send("RCPT TO:<main@example.test>\r\n", "250");
        send("DATA\r\n", "354");
        send("Subject: full at DATA\r\n\r\nbody\r\n.\r\n", "250 2.0.0 ");
        assert_eq!(count("aa", 1), plus(&a, 2));
        assert_eq!(
            (count("cc", 1), count("cc", 4)),
            (b.clone(), plus(&notices, 1))
        );
        let text = refused_notice(fixture, "cc");
        assert!(text.contains("Recipient: second@example.test"), "{text}");
        assert!(!text.contains("main@example.test"), "{text}");
        // Only the delivered copy carries the receiver's results; the notice has none.
        assert_eq!(sql(fixture, "SELECT count(*) || '/' || count(a.email_id)
            FROM td_mta.emails e JOIN td_mta.blob_chunks c ON c.account = e.account AND c.blob_id = e.blob_id AND c.ordinal = 0
            LEFT JOIN td_mta.email_authentication a ON a.account = e.account AND a.email_id = e.id
            WHERE e.origin = 1 AND position(convert_to('Subject: full at DATA', 'UTF8') in c.bytes) > 0"), "1/1");
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.email_authentication a JOIN td_mta.emails e ON e.account = a.account AND e.id = a.email_id
            WHERE e.origin = 4"), "0");
        unfill(real);
        // Past the notice margin too the whole message stays refused, and
        // the sender's retry is refused for B alone at RCPT.
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        send("RCPT TO:<second@example.test>\r\n", "250");
        let real: u64 = sql(fixture, usage).parse().unwrap();
        sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={} WHERE account=decode(repeat('cc',16),'hex')", 2 * body_cap));
        send("RCPT TO:<main@example.test>\r\n", "250");
        send("DATA\r\n", "354");
        send(
            "Subject: past margin\r\n\r\nbody\r\n.\r\n",
            "452 4.2.2 Recipient mailbox full",
        );
        assert_eq!(count("aa", 1), plus(&a, 2));
        assert_eq!(count("cc", 4), plus(&notices, 1));
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        send("RCPT TO:<second@example.test>\r\n", "452 4.2.2 ");
        send("RCPT TO:<main@example.test>\r\n", "250");
        send("DATA\r\n", "354");
        send("Subject: past margin\r\n\r\nbody\r\n.\r\n", "250");
        assert_eq!(count("aa", 1), plus(&a, 3));
        assert_eq!(count("cc", 4), plus(&notices, 1));
        // A sole recipient that fills after its RCPT gets 452 at the dot.
        sql(fixture, &format!("UPDATE td_mta.account_usage SET body_bytes={real} WHERE account=decode(repeat('cc',16),'hex')"));
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        send("RCPT TO:<second@example.test>\r\n", "250");
        let real = fill();
        send("DATA\r\n", "354");
        send(
            "Subject: sole full\r\n\r\nbody\r\n.\r\n",
            "452 4.2.2 Recipient mailbox full",
        );
        assert_eq!(
            (count("cc", 1), count("cc", 4)),
            (b.clone(), plus(&notices, 1))
        );
        unfill(real);
        // An account without its Inbox is refused at RCPT alone too.
        send("MAIL FROM:<sender@example.test>\r\n", "250");
        sql(fixture, "UPDATE td_mta.mailboxes SET role=NULL WHERE account=decode(repeat('cc',16),'hex') AND role='inbox'");
        send("RCPT TO:<second@example.test>\r\n", "450 4.2.1 ");
        send("RCPT TO:<main@example.test>\r\n", "250");
        send("RSET\r\n", "250");
        sql(
            fixture,
            "UPDATE td_mta.mailboxes SET role='inbox' WHERE account=decode(repeat('cc',16),'hex')",
        );
        assert_eq!(count("cc", 4), plus(&notices, 1));
    }
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
        // A's held COMMIT keeps its row locked; its RCPT check takes no lock.
        for (line, code) in [
            ("MAIL FROM:<sender@example.test>\r\n", "250"),
            ("RCPT TO:<main@example.test>\r\n", "250"),
            ("RSET\r\n", "250"),
        ] {
            stream.get_mut().write_all(line.as_bytes()).unwrap();
            stream.get_mut().flush().unwrap();
            reply(&mut stream, code);
        }
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "31");
        sql(fixture, "SELECT nextval('public.release_smtp_commit')");
        sql(fixture, "DROP TRIGGER hold_smtp_commit ON td_mta.emails");
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "32");
    }
    stream.get_mut().write_all(b"QUIT\r\n").unwrap();
    stream.get_mut().flush().unwrap();
    reply(&mut stream, "221");
}

/// The decoded text of the account's newest local notice.
fn refused_notice(fixture: &Fixture, account: &str) -> String {
    super::sql_bounded(fixture, &format!("SELECT convert_from(decode(split_part(convert_from(string_agg(c.bytes,''::bytea ORDER BY c.ordinal),'UTF8'),E'\\r\\n\\r\\n',2),'base64'),'UTF8') FROM td_mta.blob_chunks c WHERE c.account=decode(repeat('{account}',16),'hex') AND c.blob_id=(SELECT blob_id FROM td_mta.emails WHERE account=c.account AND origin=4 ORDER BY changed DESC LIMIT 1)"), 65536).replace("\r\n", "\n")
}
