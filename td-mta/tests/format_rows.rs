use td_mta::format::{
    key::{Key, SourceKind},
    row::*,
    Error, Table,
};
use td_mta::ids::*;

// Shared by the independent structure and real-provider digest checks.
const MAILBOX_SOURCE_PREIMAGE: &str =
    "010800000050726f6a656374730108000000706172656e742d31000100000001";
const EMAIL_SOURCE_PREIMAGE: &str = "03ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad0300000000000000ffffffffffffffff02000000020000006161010000007a0200000005000000247365656e0700000070726f6a656374";

fn hex(input: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let digits: String = input.split_whitespace().collect();
    let (pairs, rest) = digits.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
        .collect()
}
macro_rules! fixture {
    ($name:literal) => {
        hex(include_str!(concat!("fixtures/format-v2/", $name, ".hex")))?
    };
}

fn check(row: Row<'_>, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(row.encoded_len()?, bytes.len());
    let mut output = vec![0xa5; bytes.len()];
    assert_eq!(row.encode(&mut output)?, bytes.len());
    assert_eq!(output, bytes);
    assert_eq!(Row::decode(row.table(), bytes)?, row);
    for end in 0..bytes.len() {
        assert!(
            Row::decode(row.table(), bytes.get(..end).ok_or("prefix")?).is_err(),
            "{} prefix {end}",
            row.table().tag()
        );
        let mut short = vec![0xa5; end];
        assert_eq!(row.encode(&mut short), Err(Error::OutputFull));
        assert!(short.iter().all(|&b| b == 0xa5));
    }
    let mut excess = bytes.to_vec();
    excess.push(0);
    assert_eq!(Row::decode(row.table(), &excess), Err(Error::TrailingBytes));
    Ok(())
}

#[test]
fn every_row_has_an_independent_literal_oracle() -> Result<(), Box<dyn std::error::Error>> {
    let blob = BlobId::from_bytes([0x44; 16]);
    let mailbox = MailboxId::from_bytes([0x55; 16]);
    let thread = ThreadId::from_bytes([0x66; 16]);
    let email = EmailId::from_bytes([0x77; 16]);
    let digest: [u8; 32] = hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")?
        .try_into()
        .map_err(|_| "digest length")?;
    check(
        Row::Blob(BlobRow {
            length: 3,
            digest,
            created_at: 0,
        }),
        &fixture!("row-blob"),
    )?;
    check(
        Row::Mailbox(MailboxRow {
            name: "Inbox",
            parent: None,
            role: Some("inbox"),
            sort_order: 0,
            subscribed: true,
        }),
        &fixture!("row-mailbox"),
    )?;
    check(
        Row::Mailbox(MailboxRow {
            name: "Archive",
            parent: Some(mailbox),
            role: None,
            sort_order: 1,
            subscribed: false,
        }),
        &fixture!("row-mailbox-child"),
    )?;
    let mut receipt_buffer = [0; 128];
    let recipients =
        ReceiptRecipients::encode(&["a@example.test", "b@example.test"], &mut receipt_buffer)?;
    assert_eq!(
        recipients.iter()?.collect::<Result<Vec<_>, _>>()?,
        ["a@example.test", "b@example.test"]
    );
    let receipt = SmtpReceipt {
        peer: "192.0.2.1".parse()?,
        gateway: None,
        tls: ReceiptTls::Tls13,
        ehlo: "mx.example.test",
        reverse_path: "",
        recipients,
    };
    let row = EmailRow {
        blob,
        thread,
        received_at: -1,
        origin: EmailOrigin::Smtp(receipt),
    };
    check(Row::Email(row), &fixture!("row-email-smtp4"))?;
    check(
        Row::Email(EmailRow {
            origin: EmailOrigin::Smtp(SmtpReceipt {
                peer: "2001:db8::1".parse()?,
                gateway: Some("gateway-1"),
                tls: ReceiptTls::Tls12,
                reverse_path: "sender@example.test",
                ..receipt
            }),
            ..row
        }),
        &fixture!("row-email-smtp6"),
    )?;
    for (origin, bytes) in [
        (EmailOrigin::Jmap, fixture!("row-email-jmap")),
        (EmailOrigin::Import, fixture!("row-email-import")),
        (EmailOrigin::FailureNotice, fixture!("row-email-notice")),
    ] {
        check(Row::Email(EmailRow { origin, ..row }), &bytes)?;
    }
    for (row, bytes) in [
        (Row::Membership, fixture!("row-membership")),
        (Row::Keyword, fixture!("row-keyword")),
        (Row::Thread, fixture!("row-thread")),
        (Row::ThreadAnchor, fixture!("row-thread-anchor")),
    ] {
        check(row, &bytes)?;
    }
    let submission = SubmissionRow {
        email,
        thread,
        identity: IdentityId::from_bytes([0x88; 16]),
        transmitted_blob: blob,
        reverse_path: "sender@example.test",
        send_at: 0,
        expires_at: 1000,
        recipient_count: 2,
        completed_at: None,
        notification: NotificationState::None,
        notification_email: None,
    };
    check(Row::Submission(submission), &fixture!("row-submission"))?;
    check(
        Row::Submission(SubmissionRow {
            completed_at: Some(900),
            notification: NotificationState::Stored,
            notification_email: Some(EmailId::from_bytes([0x99; 16])),
            ..submission
        }),
        &fixture!("row-submission-notified"),
    )?;
    let recipient = RecipientRow {
        address: "a@example.test",
        state: RecipientState::Queued,
        uncertain: false,
        attempt: None,
        attempt_count: 0,
        last_attempt_at: None,
        phase: AttemptPhase::None,
        next_attempt_at: Some(0),
        rcpt_reply: None,
        data_reply: None,
        reason: FailureReason::None,
        diagnostic: "",
    };
    check(Row::Recipient(recipient), &fixture!("row-recipient-queued"))?;
    check(
        Row::Recipient(RecipientRow {
            state: RecipientState::OutcomeUnknown,
            uncertain: true,
            attempt: Some(AttemptId::from_bytes([0xaa; 16])),
            attempt_count: 1,
            last_attempt_at: Some(10),
            phase: AttemptPhase::AcceptancePossible,
            next_attempt_at: Some(100),
            rcpt_reply: Some("250 2.1.5 OK"),
            reason: FailureReason::Uncertain,
            diagnostic: "connection lost",
            ..recipient
        }),
        &fixture!("row-recipient-unknown"),
    )?;
    check(
        Row::Lease(LeaseRow {
            account: AccountId::from_bytes([0x33; 16]),
            device: DeviceId::from_bytes([0xbb; 16]),
            expires_at: 1000,
            uses: LeaseUse::Both,
        }),
        &fixture!("row-lease"),
    )?;
    check(
        Row::Import(ImportRow {
            local_object: [0x55; 16],
            historical_blob: None,
            source_digest: [0xcc; 32],
        }),
        &fixture!("row-import-mailbox"),
    )?;
    check(
        Row::Import(ImportRow {
            local_object: [0x77; 16],
            historical_blob: Some(blob),
            source_digest: [0xcc; 32],
        }),
        &fixture!("row-import-email"),
    )?;
    Ok(())
}

#[test]
fn invalid_local_fields_refuse_without_touching_output() -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = fixture!("row-recipient-unknown");
    // State, uncertainty and presence tags follow text(14).
    for (offset, value) in [(18, 255), (19, 2), (20, 2), (50, 255)] {
        let old = *bytes.get(offset).ok_or("tag offset")?;
        *bytes.get_mut(offset).ok_or("tag offset")? = value;
        assert_eq!(
            Row::decode(Table::Recipients, &bytes),
            Err(Error::InvalidTag)
        );
        *bytes.get_mut(offset).ok_or("tag offset")? = old;
    }
    let Row::Recipient(base) = Row::decode(Table::Recipients, &bytes)? else {
        return Err("row kind".into());
    };
    let oversized = "x".repeat(MAX_SMTP_REPLY + 1);
    for (index, row) in [
        RecipientRow {
            uncertain: false,
            ..base
        },
        RecipientRow {
            state: RecipientState::Canceled,
            ..base
        },
        RecipientRow {
            attempt_count: 0,
            ..base
        },
        RecipientRow {
            rcpt_reply: Some(&oversized),
            ..base
        },
        RecipientRow {
            address: "a\r\nb",
            ..base
        },
        RecipientRow {
            diagnostic: "bad\0text",
            ..base
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut output = [0xa5; 1024];
        let expected = if index == 3 {
            Error::Limit
        } else {
            Error::InvalidValue
        };
        assert_eq!(Row::Recipient(row).encode(&mut output), Err(expected));
        assert_eq!(output, [0xa5; 1024]);
    }
    let mut bytes = fixture!("row-blob");
    bytes.pop();
    assert_eq!(Row::decode(Table::Blobs, &bytes), Err(Error::Truncated));
    let mut bytes = fixture!("row-mailbox");
    *bytes.get_mut(4).ok_or("name")? = 0xff;
    assert_eq!(
        Row::decode(Table::Mailboxes, &bytes),
        Err(Error::InvalidUtf8)
    );
    assert_eq!(
        Row::decode(Table::Blobs, &vec![0; 65537]),
        Err(Error::Limit)
    );
    Ok(())
}

#[test]
fn receipt_count_and_bytes_are_separate_limits() -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = vec![0xa5; MAX_RECEIPT_BYTES];
    assert!(ReceiptRecipients::encode(&[], &mut buffer).is_err());
    assert!(ReceiptRecipients::encode(&vec!["a"; 1001], &mut buffer).is_err());
    let exact = ReceiptRecipients::encode(&vec!["a"; 1000], &mut buffer)?;
    assert_eq!(exact.count(), 1000);
    assert_eq!(exact.iter()?.count(), 1000);
    let long = "a".repeat(254);
    assert!(ReceiptRecipients::encode(&vec![long.as_str(); 128], &mut buffer).is_err());
    assert!(ReceiptRecipients::encode(&["a".repeat(255).as_str()], &mut buffer).is_err());
    assert!(ReceiptRecipients::decode(&u32::MAX.to_le_bytes()).is_err());
    assert!(ReceiptRecipients::decode(&0u32.to_le_bytes()).is_err());
    let mut short = [0xa5; 4];
    assert_eq!(
        ReceiptRecipients::encode(&["a"], &mut short),
        Err(Error::OutputFull)
    );
    assert_eq!(short, [0xa5; 4]);
    // 126 * 258 + 256 + 4 = 32768 (last address has 252 bytes).
    let last = "b".repeat(252);
    let mut addresses = vec![long.as_str(); 126];
    addresses.push(&last);
    let exact = ReceiptRecipients::encode(&addresses, &mut buffer)?;
    assert_eq!(exact.encoded().len(), MAX_RECEIPT_BYTES);
    assert_eq!(ReceiptRecipients::decode(exact.encoded())?, exact);
    Ok(())
}

#[test]
fn key_value_checks_do_not_confuse_framing_with_semantics() -> Result<(), Box<dyn std::error::Error>>
{
    let email = EmailId::from_bytes([1; 16]);
    assert_eq!(
        Row::Keyword.validate_key(Key::Keyword(email, "")),
        Err(Error::Limit)
    );
    Row::Keyword.validate_key(Key::Keyword(email, &"a".repeat(255)))?;
    assert_eq!(
        Row::Keyword.validate_key(Key::Keyword(email, &"a".repeat(256))),
        Err(Error::Limit)
    );

    for keyword in ["$seen", "foo", "$draft", "x/y"] {
        Row::Keyword.validate_key(Key::Keyword(email, keyword))?;
    }
    for keyword in ["$Seen", "é", "foo bar", "a]", "a\\", "a*"] {
        Key::Keyword(email, keyword).encoded_len()?;
        assert!(Row::Keyword
            .validate_key(Key::Keyword(email, keyword))
            .is_err());
    }
    assert!(Row::Thread.validate_key(Key::Email(email)).is_err());
    let import = Row::Import(ImportRow {
        local_object: [2; 16],
        historical_blob: None,
        source_digest: [3; 32],
    });
    let key = Key::Import {
        instance: InstanceId::from_bytes([4; 16]),
        kind: SourceKind::Mailbox,
        account: b"a",
        object: b"b",
    };
    import.validate_key(key)?;
    if let Key::Import {
        instance,
        account,
        object,
        ..
    } = key
    {
        assert!(import
            .validate_key(Key::Import {
                instance,
                kind: SourceKind::Email,
                account,
                object
            })
            .is_err());
    }
    Ok(())
}

#[test]
fn submission_count_uses_the_collection_limit_error() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fixture!("row-submission");
    let Row::Submission(base) = Row::decode(Table::Submissions, &bytes)? else {
        return Err("row kind".into());
    };
    for recipient_count in [0, MAX_RECIPIENTS + 1] {
        assert_eq!(
            Row::Submission(SubmissionRow {
                recipient_count,
                ..base
            })
            .encoded_len(),
            Err(Error::Limit)
        );
    }
    Ok(())
}

fn replace(
    bytes: &[u8],
    start: usize,
    end: usize,
    replacement: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok([
        bytes.get(..start).ok_or("replacement start")?,
        replacement,
        bytes.get(end..).ok_or("replacement end")?,
    ]
    .concat())
}
fn string(value: &str) -> Vec<u8> {
    [
        (value.len() as u32).to_le_bytes().as_slice(),
        value.as_bytes(),
    ]
    .concat()
}

#[test]
fn structurally_valid_fields_still_require_semantic_validation(
) -> Result<(), Box<dyn std::error::Error>> {
    let mailbox = fixture!("row-mailbox");
    let submission = fixture!("row-submission");
    let recipient = fixture!("row-recipient-unknown");
    let smtp = fixture!("row-email-smtp4");
    let gateway = fixture!("row-email-smtp6");
    // Independent offsets in literal layouts, not offsets from the decoder.
    let cases = [
        (
            Table::Mailboxes,
            replace(&mailbox, 0, 9, &string(""))?,
            Error::InvalidValue,
        ),
        (
            Table::Mailboxes,
            replace(&mailbox, 15, 20, b"INBOX")?,
            Error::InvalidValue,
        ),
        (
            Table::Mailboxes,
            replace(&mailbox, 20, 24, &(1u32 << 31).to_le_bytes())?,
            Error::InvalidValue,
        ),
        (
            Table::Submissions,
            replace(&submission, 95, 103, &(-1i64).to_le_bytes())?,
            Error::InvalidValue,
        ),
        (
            Table::Submissions,
            replace(&submission, 103, 107, &0u32.to_le_bytes())?,
            Error::Limit,
        ),
        (
            Table::Submissions,
            replace(&submission, 108, 109, &[2])?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 0, 18, &string("é"))?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 19, 20, &[0])?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 18, 19, &[6])?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 37, 41, &0u32.to_le_bytes())?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 41, 50, &[0])?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 50, 51, &[0])?,
            Error::InvalidValue,
        ),
        (
            Table::Recipients,
            replace(&recipient, 61, 77, &string(""))?,
            Error::InvalidValue,
        ),
        (
            Table::Emails,
            replace(&smtp, 48, 67, &string(""))?,
            Error::InvalidValue,
        ),
        (
            Table::Emails,
            replace(&gateway, 59, 72, &string(""))?,
            Error::InvalidValue,
        ),
    ];
    for (index, (table, bytes, error)) in cases.iter().enumerate() {
        assert_eq!(Row::decode(*table, bytes), Err(*error), "case {index}");
    }
    // The receipt list is the suffix after the SMTP4 origin's null sender.
    let mut list = 128u32.to_le_bytes().to_vec();
    for _ in 0..128 {
        list.extend(string(&"a".repeat(254)));
    }
    assert_eq!(
        Row::decode(Table::Emails, &replace(&smtp, 71, smtp.len(), &list)?),
        Err(Error::Limit)
    );
    let list = [1u32.to_le_bytes().as_slice(), string("").as_slice()].concat();
    assert_eq!(
        Row::decode(Table::Emails, &replace(&smtp, 71, smtp.len(), &list)?),
        Err(Error::InvalidValue)
    );
    let mailbox_id = MailboxId::from_bytes([0x55; 16]);
    let bytes = fixture!("row-mailbox-child");
    assert_eq!(
        decode_record(Table::Mailboxes, mailbox_id.as_bytes(), &bytes),
        Err(Error::InvalidValue)
    );
    let submission_id = SubmissionId::from_bytes([0xdd; 16]);
    let mut key = [0; 20];
    Key::Recipient(submission_id, 999).encode(&mut key)?;
    decode_record(Table::Recipients, &key, &recipient)?;
    Key::Recipient(submission_id, 1000).encode(&mut key)?;
    assert_eq!(
        decode_record(Table::Recipients, &key, &recipient),
        Err(Error::Limit)
    );
    let limits = td_mta::limits::Limits {
        smtp_recipients: MAX_RECIPIENTS as usize,
        memory_budget_bytes: 128 * 1024 * 1024,
        ..Default::default()
    };
    limits.plan()?;
    assert!(td_mta::limits::Limits {
        smtp_recipients: MAX_RECIPIENTS as usize + 1,
        ..limits
    }
    .plan()
    .is_err());
    Ok(())
}

#[test]
fn source_preimage_literals_pin_raw_id_sort_and_optional_lengths(
) -> Result<(), Box<dyn std::error::Error>> {
    use td_mta::format::scalar::Reader;
    let mailbox = hex(MAILBOX_SOURCE_PREIMAGE)?;
    let mut r = Reader::new(&mailbox);
    assert_eq!(r.u8()?, 1);
    assert_eq!(r.text(1024)?, "Projects");
    assert!(r.boolean()?);
    assert_eq!(r.bytes(998)?, b"parent-1");
    assert!(!r.boolean()?);
    assert_eq!(r.u32()?, 1);
    assert!(r.boolean()?);
    r.finish()?;
    let email = hex(EMAIL_SOURCE_PREIMAGE)?;
    let mut r = Reader::new(&email);
    assert_eq!(r.u8()?, 3);
    assert_eq!(
        r.take(32)?,
        hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")?
    );
    assert_eq!(r.u64()?, 3);
    assert_eq!(r.i64()?, -1);
    assert_eq!(r.u32()?, 2);
    assert_eq!(r.bytes(998)?, b"aa");
    assert_eq!(r.bytes(998)?, b"z");
    assert_eq!(r.u32()?, 2);
    assert_eq!(r.text(255)?, "$seen");
    assert_eq!(r.text(255)?, "project");
    r.finish()?;
    Ok(())
}

// FORMAT.md offsets and independent checked-in artifacts are the oracle.
// This qualifies hash coverage, not a production container verifier.
fn provider_hash(input: &[u8], chunk: usize) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    use td_crypto::Digest;
    let mut digest = td_crypto::Sha256::try_new()?;
    for part in input.chunks(chunk) {
        digest.update(part)?;
    }
    Ok(digest.finish()?)
}

fn hash_matches(input: &[u8], expected: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(expected.len(), 32);
    for chunk in [1, 31, 64, 65] {
        assert_eq!(provider_hash(input, chunk)?.as_slice(), expected);
    }
    if !input.is_empty() {
        let mut changed = input.to_vec();
        for index in [0, input.len() / 2, input.len() - 1] {
            let byte = changed.get_mut(index).ok_or("mutation index")?;
            *byte ^= 1;
            assert_ne!(provider_hash(&changed, 31)?.as_slice(), expected);
            *changed.get_mut(index).ok_or("mutation restore")? ^= 1;
        }
    }
    Ok(())
}

#[test]
fn provider_hashes_import_snapshot_fixtures() -> Result<(), Box<dyn std::error::Error>> {
    for (preimage, digest) in [
        (
            MAILBOX_SOURCE_PREIMAGE,
            "2ebfa0d8f9aaecd36e1288487ab8419eeba45d7e22cbb079f6f5ea1a53a2cb05",
        ),
        (
            EMAIL_SOURCE_PREIMAGE,
            "ecc93dd30f2e7c6e9f14ba4cc6d910377772dc7ad3386bc801578d9801a82175",
        ),
    ] {
        hash_matches(&hex(preimage)?, &hex(digest)?)?;
    }
    Ok(())
}

#[test]
fn provider_hashes_blob_fixture() -> Result<(), Box<dyn std::error::Error>> {
    hash_matches(
        b"abc",
        &hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")?,
    )?;
    Ok(())
}
