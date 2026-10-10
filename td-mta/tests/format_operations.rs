#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_mta::{
    format::{
        key::Key,
        operation::{extent, Operation, Value},
        row::Row,
        Error, ObjectType, Table,
    },
    ids::{BlobId, EmailId, InstanceId, MailboxId, SubmissionId, ThreadId},
    ports::{Change, ChangeAction, Mutation, OperationKind},
};
fn hex(input: &str) -> Vec<u8> {
    let digits: String = input.split_whitespace().collect();
    let (pairs, rest) = digits.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn check(bytes: &[u8], expected: Operation<'_>) {
    assert_eq!(Operation::decode(bytes), Ok(expected));
    assert_eq!(extent(&bytes[..12]), Ok(bytes.len()));
    let mut output = vec![0xa5; bytes.len() + 7];
    assert_eq!(expected.encode(&mut output), Ok(bytes.len()));
    assert_eq!(&output[..bytes.len()], bytes);
    assert_eq!(&output[bytes.len()..], &[0xa5; 7]);
    for n in 0..bytes.len() {
        assert_eq!(Operation::decode(&bytes[..n]), Err(Error::Truncated));
        let mut output = vec![0xa5; n];
        assert_eq!(expected.encode(&mut output), Err(Error::OutputFull));
        assert!(output.iter().all(|b| *b == 0xa5));
    }
    let mut extra = bytes.to_vec();
    extra.push(0);
    assert_eq!(Operation::decode(&extra), Err(Error::TrailingBytes));
    for n in 0..12 {
        assert_eq!(extent(&bytes[..n]), Err(Error::Truncated));
    }
    assert_eq!(extent(&bytes[..13]), Err(Error::TrailingBytes));
}
#[test]
fn independent_payloads_pin_all_three_operation_encodings() {
    let put = hex(include_str!("fixtures/format-v2/operation-put-blob.hex"));
    let row = hex(include_str!("fixtures/format-v2/row-blob.hex"));
    let key = [0x44; 16];
    let operation = Operation::put(Table::Blobs, &key, &row).unwrap();
    check(&put, operation);
    assert_eq!(operation.kind(), OperationKind::Put);
    assert_eq!(operation.type_tag(), 1);
    assert_eq!(operation.key_bytes(), key);
    assert_eq!(operation.value_bytes(), row);
    assert_eq!(
        operation.value(),
        Value::Row(Mutation::Put {
            key: Key::Blob(BlobId::from_bytes(key)),
            row: Row::decode(Table::Blobs, &row).unwrap(),
        })
    );
    let pair = hex(include_str!(
        "fixtures/format-v2/operation-delete-change.hex"
    ));
    let deletion = Operation::delete(Table::Blobs, &key).unwrap();
    check(&pair[..28], deletion);
    assert_eq!(
        deletion.value(),
        Value::Row(Mutation::Delete(Key::Blob(BlobId::from_bytes(key))))
    );
    assert_eq!(deletion.kind(), OperationKind::Delete);
    let id = [0x77; 16];
    let change = Operation::change(ObjectType::Email, ChangeAction::Destroyed, &id);
    check(&pair[28..56], change);
    assert_eq!(
        change.value(),
        Value::Change(Change {
            kind: ObjectType::Email,
            action: ChangeAction::Destroyed,
            id
        })
    );
}
#[test]
fn every_change_tag_and_action_has_exact_wire_bytes_including_known_identity() {
    let id = [0x77; 16];
    for (tag, kind) in [
        (1, ObjectType::Mailbox),
        (2, ObjectType::Thread),
        (3, ObjectType::Email),
        (4, ObjectType::Identity),
        (5, ObjectType::EmailSubmission),
    ] {
        for (action, typed) in [
            (1, ChangeAction::Created),
            (2, ChangeAction::Updated),
            (3, ChangeAction::Destroyed),
        ] {
            let mut bytes = [0; 28];
            bytes[..12].copy_from_slice(&[3, action, tag, 0, 16, 0, 0, 0, 0, 0, 0, 0]);
            bytes[12..].copy_from_slice(&id);
            let op = Operation::change(kind, typed, &id);
            check(&bytes, op);
            assert_eq!(op.kind(), OperationKind::Change(typed));
            assert_eq!(op.type_tag(), u16::from(tag));
        }
    }
    // Identity is known wire syntax; this test grants no v1 transaction validity.
}
#[test]
fn deletion_validates_the_selected_tables_entire_key_grammar() {
    use td_mta::format::key::SourceKind;
    let email = EmailId::from_bytes([0x77; 16]);
    let mailbox = MailboxId::from_bytes([0x55; 16]);
    let submission = SubmissionId::from_bytes([0x88; 16]);
    for key in [
        Key::Blob(BlobId::from_bytes([0x44; 16])),
        Key::Mailbox(mailbox),
        Key::Email(email),
        Key::Membership(email, mailbox),
        Key::Keyword(email, "x"),
        Key::Thread(ThreadId::from_bytes([0x66; 16])),
        Key::ThreadAnchor("x", email),
        Key::Submission(submission),
        Key::Recipient(submission, 1),
        Key::Lease(BlobId::from_bytes([0x44; 16])),
        Key::Import {
            instance: InstanceId::from_bytes([0x11; 16]),
            kind: SourceKind::Email,
            account: b"a",
            object: b"b",
        },
    ] {
        let mut encoded = [0; 1024];
        let n = key.encode(&mut encoded).unwrap();
        let op = Operation::delete(key.table(), &encoded[..n]).unwrap();
        let mut bytes = [0; 1036];
        let m = op.encode(&mut bytes).unwrap();
        assert_eq!(&bytes[..4], &[2, 0, key.table().tag() as u8, 0]);
        assert_eq!(
            Operation::decode(&bytes[..m]).unwrap().value(),
            Value::Row(Mutation::Delete(key))
        );
        let mut extra_key = encoded[..n].to_vec();
        extra_key.push(0);
        // Only unprefixed keyword text consumes the added byte structurally.
        if key.table() != Table::Keywords {
            assert_eq!(
                Operation::delete(key.table(), &extra_key),
                Err(Error::TrailingBytes)
            );
        } else {
            assert_eq!(
                Operation::delete(key.table(), &extra_key),
                Err(Error::InvalidValue)
            );
        }
    }
    let source_id = [b'x'; 998];
    let key = Key::Import {
        instance: InstanceId::from_bytes([0x11; 16]),
        kind: SourceKind::Email,
        account: b"a",
        object: &source_id,
    };
    let mut key_bytes = [0; 1024];
    assert_eq!(key.encode(&mut key_bytes), Ok(1024));
    let op = Operation::delete(Table::Imports, &key_bytes).unwrap();
    let mut bytes = [0; 1036];
    assert_eq!(op.encode(&mut bytes), Ok(1036));
    assert_eq!(Operation::decode(&bytes), Ok(op));
    assert_eq!(Operation::delete(Table::Blobs, &[0; 15]), Err(Error::Limit));
    assert_eq!(
        Operation::delete(Table::Blobs, &[0; 1025]),
        Err(Error::Limit)
    );
    let mut bad_keyword = [0; 17];
    bad_keyword[16] = 0xff;
    assert_eq!(
        Operation::delete(Table::Keywords, &bad_keyword),
        Err(Error::InvalidUtf8)
    );
}
#[test]
fn malformed_operation_prefixes_and_rows_are_refused_without_integrity_claims() {
    let literal = hex(include_str!("fixtures/format-v2/operation-put-blob.hex"));
    let payload = &literal;
    for (offset, byte) in [(0, 0), (0, 4), (1, 1), (2, 0), (2, 12)] {
        let mut bytes = payload.to_vec();
        bytes[offset] = byte;
        assert_eq!(Operation::decode(&bytes), Err(Error::InvalidTag));
    }
    for (offset, value) in [
        (4, 15u32),
        (4, 1025),
        (4, u32::MAX),
        (8, 65537),
        (8, u32::MAX),
    ] {
        let mut bytes = payload.to_vec();
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(Operation::decode(&bytes), Err(Error::Limit));
        assert_eq!(extent(&bytes[..12]), Err(Error::Limit));
    }
    let mut invalid_row = payload.to_vec();
    invalid_row[8] = 47;
    invalid_row.pop();
    assert_eq!(Operation::decode(&invalid_row), Err(Error::Truncated));
    for action in 1..=u8::MAX {
        let bytes = [2, action, 1, 0, 16, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(extent(&bytes), Err(Error::InvalidTag));
        assert_eq!(Operation::decode(&bytes), Err(Error::InvalidTag));
    }
    let mut deletion = payload.to_vec();
    deletion[0] = 2;
    assert_eq!(Operation::decode(&deletion), Err(Error::InvalidValue));
    for (action, tag, key_len, value_len, error) in [
        (0, 3, 16, 0, Error::InvalidTag),
        (4, 3, 16, 0, Error::InvalidTag),
        (1, 0, 16, 0, Error::InvalidTag),
        (1, 6, 16, 0, Error::InvalidTag),
        (1, 3, 15, 0, Error::InvalidValue),
        (1, 3, 16, 1, Error::InvalidValue),
    ] {
        let bytes = [3, action, tag, 0, key_len, 0, 0, 0, value_len, 0, 0, 0];
        assert_eq!(extent(&bytes), Err(error));
    }
    let key = [0; 32];
    let empty = Operation::put(Table::Memberships, &key, &[]).unwrap();
    let mut bytes = [0; 44];
    empty.encode(&mut bytes).unwrap();
    assert_eq!(Operation::decode(&bytes), Ok(empty));
    assert_eq!(
        Operation::put(Table::Memberships, &key, &[0]),
        Err(Error::TrailingBytes)
    );
}

#[test]
fn delete_rejects_noncanonical_keywords_and_out_of_range_recipient_ordinals() {
    fn check_key(key: Key<'_>, error: Error) {
        let mut bytes = [0; 1024];
        let n = key.encode(&mut bytes).unwrap();
        assert_eq!(Operation::delete(key.table(), &bytes[..n]), Err(error));
        let mut encoded = vec![2, 0, key.table().tag() as u8, 0];
        encoded.extend_from_slice(&(n as u32).to_le_bytes());
        encoded.extend_from_slice(&0u32.to_le_bytes());
        encoded.extend_from_slice(&bytes[..n]);
        assert_eq!(Operation::decode(&encoded), Err(error));
    }
    let email = EmailId::from_bytes([0x77; 16]);
    for keyword in [
        "$Seen", "a b", "(", ")", "{", "]", "%", "*", "\"", "\\", "é", "\u{7f}",
    ] {
        check_key(Key::Keyword(email, keyword), Error::InvalidValue);
    }
    let submission = SubmissionId::from_bytes([0x88; 16]);
    for ordinal in [1000, u32::MAX] {
        check_key(Key::Recipient(submission, ordinal), Error::Limit);
    }
    for key in [
        Key::Keyword(email, "$seen"),
        Key::Recipient(submission, 999),
    ] {
        let mut bytes = [0; 1024];
        let n = key.encode(&mut bytes).unwrap();
        let operation = Operation::delete(key.table(), &bytes[..n]).unwrap();
        let mut output = [0; 1036];
        let m = operation.encode(&mut output).unwrap();
        assert_eq!(Operation::decode(&output[..m]), Ok(operation));
    }
}
