use super::*;
#[test]
fn recipient_history_survives_typed_and_encoded_updates() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_with_recipient(&store, 1, &[0], uncertain(), encoded).unwrap();
        let key = recipient_key(0);
        let initial = uncertain();
        let bytes = encode(Row::Recipient(initial));
        // An identical PUT remains valid with retained attempt history.
        apply(
            &store,
            1,
            &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
            encoded,
        )
        .unwrap();
        for variant in 0..5 {
            let mut changed = initial;
            match variant {
                0 => changed.address = "other@example.test",
                1 => {
                    changed.uncertain = false;
                    changed.state = RecipientState::RetryWait;
                }
                2 => changed.attempt_count = 2,
                3 => changed.attempt = Some(AttemptId::from_bytes([9; 16])),
                _ => changed.last_attempt_at = Some(2),
            }
            let bytes = encode(Row::Recipient(changed));
            rejected(apply(
                &store,
                2,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded,
            ));
        }
        let mut changed = initial;
        changed.diagnostic = "retained delivery risk";
        let bytes = encode(Row::Recipient(changed));
        assert_eq!(
            apply(
                &store,
                2,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded
            ),
            Ok(Sequence::from_u64(3))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 65536];
        let (row, _) = view
            .get(Key::Recipient(SUBMISSION, 0), &mut scratch)
            .unwrap()
            .unwrap();
        assert_eq!(row, Row::Recipient(changed));
    }
}
#[test]
fn repeated_keys_compare_the_final_effect_with_the_original_history() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_with_recipient(&store, 1, &[0], uncertain(), encoded).unwrap();
        let key = recipient_key(0);
        let initial = uncertain();
        let original = encode(Row::Recipient(initial));
        let good = Operation::put(Table::Recipients, &key, &original).unwrap();
        // An identical PUT remains valid with retained attempt history.
        apply(&store, 1, &[good], encoded).unwrap();
        let mut changed = initial;
        changed.uncertain = false;
        changed.state = RecipientState::RetryWait;
        let bytes = encode(Row::Recipient(changed));
        let bad = Operation::put(Table::Recipients, &key, &bytes).unwrap();
        let delete = Operation::delete(Table::Recipients, &key).unwrap();
        assert_eq!(
            apply(&store, 2, &[bad, good], encoded),
            Ok(Sequence::from_u64(3))
        );
        rejected(apply(&store, 3, &[good, bad], encoded));
        rejected(apply(&store, 3, &[delete, bad], encoded));
        assert_eq!(
            apply(&store, 3, &[bad, delete, good], encoded),
            Ok(Sequence::from_u64(4))
        );
    }
}
#[test]
fn submission_envelope_and_creation_identity_are_immutable() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create(&store, 1, &[0]).unwrap();
        let original = encode(Row::Submission(submission(1)));
        let good = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &original).unwrap();
        apply(&store, 1, &[good], encoded).unwrap();
        let alternate = BlobId::from_bytes([10; 16]);
        let hash = td_crypto::Provider.sha256().unwrap().finish().unwrap();
        let blob = encode(Row::Blob(BlobRow {
            length: 0,
            digest: hash,
            created_at: 0,
        }));
        let mut empty = b"".as_slice();
        store
            .commit(
                &td_crypto::Provider,
                request(2),
                &[Operation::put(Table::Blobs, alternate.as_bytes(), &blob).unwrap()],
                &mut [BlobSource {
                    id: alternate,
                    source: &mut empty,
                }],
            )
            .unwrap();
        let fresh = BlobId::from_bytes([12; 16]);
        let body = b"unpublished";
        let mut hash = td_crypto::Provider.sha256().unwrap();
        hash.update(body).unwrap();
        let fresh_row = encode(Row::Blob(BlobRow {
            length: body.len() as u64,
            digest: hash.finish().unwrap(),
            created_at: 0,
        }));
        let fresh_put = Operation::put(Table::Blobs, fresh.as_bytes(), &fresh_row).unwrap();
        for variant in 0..8 {
            let mut row = submission(1);
            match variant {
                0 => row.email = EmailId::from_bytes([11; 16]),
                1 => row.thread = ThreadId::from_bytes([11; 16]),
                2 => row.identity = IdentityId::from_bytes([11; 16]),
                3 => row.transmitted_blob = alternate,
                4 => row.reverse_path = "another@example.test",
                5 => row.send_at += 1,
                6 => row.expires_at += 1,
                _ => row.recipient_count = 2,
            }
            let bytes = encode(Row::Submission(row));
            let recipient = encode(Row::Recipient(queued()));
            let extra_key = recipient_key(1);
            let mut ops = vec![
                fresh_put,
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap(),
            ];
            if variant == 7 {
                ops.push(Operation::put(Table::Recipients, &extra_key, &recipient).unwrap());
            }
            let mut input = body.as_slice();
            let mut sources = [BlobSource {
                id: fresh,
                source: &mut input,
            }];
            let result = if encoded {
                commit_encoded(&store, request(3), &ops, &mut sources)
            } else {
                store.commit(&td_crypto::Provider, request(3), &ops, &mut sources)
            };
            rejected(result);
            assert_eq!(input, body.as_slice(), "history rejection consumed body");
        }
        // Reuse proves that refusal left neither body nor permanent ID registration.
        let mut input = body.as_slice();
        store
            .commit(
                &td_crypto::Provider,
                request(3),
                &[fresh_put],
                &mut [BlobSource {
                    id: fresh,
                    source: &mut input,
                }],
            )
            .unwrap();
        let view = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(view.identity().committed_sequence, Sequence::from_u64(4));
    }
}
#[test]
fn repeated_submission_keys_preserve_original_creation_identity() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create(&store, 1, &[0]).unwrap();
        let original = encode(Row::Submission(submission(1)));
        let good = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &original).unwrap();
        let mut changed = submission(1);
        changed.identity = IdentityId::from_bytes([9; 16]);
        let bytes = encode(Row::Submission(changed));
        let bad = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap();
        let delete = Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
        assert_eq!(
            apply(&store, 1, &[bad, good], encoded),
            Ok(Sequence::from_u64(2))
        );
        rejected(apply(&store, 2, &[good, bad], encoded));
        rejected(apply(&store, 2, &[delete, bad], encoded));
        assert_eq!(
            apply(&store, 2, &[bad, delete, good], encoded),
            Ok(Sequence::from_u64(3))
        );
    }
}
#[test]
fn existing_attempt_can_gain_uncertainty_then_start_a_fresh_attempt() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let key = recipient_key(0);
        let mut row = uncertain();
        row.uncertain = false;
        row.state = RecipientState::RetryWait;
        create_with_recipient(&store, 1, &[0], row, encoded).unwrap();
        let bytes = encode(Row::Recipient(row));
        // An identical PUT remains valid with retained attempt history.
        apply(
            &store,
            1,
            &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
            encoded,
        )
        .unwrap();
        row.uncertain = true;
        row.state = RecipientState::OutcomeUnknown;
        let bytes = encode(Row::Recipient(row));
        apply(
            &store,
            2,
            &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
            encoded,
        )
        .unwrap();
        row.attempt_count += 1;
        row.attempt = Some(AttemptId::from_bytes([9; 16]));
        row.last_attempt_at = Some(2);
        row.state = RecipientState::InFlight;
        row.phase = AttemptPhase::Prepared;
        row.reason = FailureReason::None;
        row.next_attempt_at = None;
        let bytes = encode(Row::Recipient(row));
        assert_eq!(
            apply(
                &store,
                3,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded
            ),
            Ok(Sequence::from_u64(4))
        );
    }
}
#[test]
fn maximum_distinct_encoded_history_batch_fits_a_production_commit_deadline() {
    struct RealClock(std::time::Instant);
    impl Clock for RealClock {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(
                    u64::try_from(self.0.elapsed().as_millis())
                        .map_err(|_| ports::Error::Capacity)?,
                ),
            })
        }
    }
    let clock = Arc::new(RealClock(std::time::Instant::now()));
    let request = |sequence| CommitRequest {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([4; 16]),
        expected: Sequence::from_u64(sequence),
        utc_ms: 0,
        deadline: Deadline::after(clock.sample().unwrap().monotonic, 30_000).unwrap(),
    };
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([4; 16]),
        clock.clone(),
        2,
        request(0).deadline,
    )
    .unwrap();
    store.create_account(ACCOUNT, request(0).deadline).unwrap();
    let hash = td_crypto::Provider.sha256().unwrap().finish().unwrap();
    let blob = encode(Row::Blob(BlobRow {
        length: 0,
        digest: hash,
        created_at: 0,
    }));
    let mut empty = b"".as_slice();
    store
        .commit(
            &td_crypto::Provider,
            request(0),
            &[Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap()],
            &mut [BlobSource {
                id: BLOB,
                source: &mut empty,
            }],
        )
        .unwrap();
    let recipient = encode(Row::Recipient(queued()));
    let mut keys = Vec::new();
    let mut sequence = 1;
    for (group, count) in [1000u32, 1000, 1000, 1000, 96].into_iter().enumerate() {
        let id = SubmissionId::from_bytes([20 + u8::try_from(group).unwrap(); 16]);
        let sub = encode(Row::Submission(submission(count)));
        let start = keys.len();
        for ordinal in 0..count {
            let mut key = [0; 20];
            Key::Recipient(id, ordinal).encode(&mut key).unwrap();
            keys.push(key);
        }
        let mut operations = vec![Operation::put(Table::Submissions, id.as_bytes(), &sub).unwrap()];
        operations.extend(
            keys.get(start..)
                .unwrap()
                .iter()
                .map(|key| Operation::put(Table::Recipients, key, &recipient).unwrap()),
        );
        store
            .commit(
                &td_crypto::Provider,
                request(sequence),
                &operations,
                &mut [],
            )
            .unwrap();
        sequence += 1;
    }
    assert_eq!(keys.len(), 4096);
    let mut changed = queued();
    changed.diagnostic = "history retained";
    let changed = encode(Row::Recipient(changed));
    let operations: Vec<_> = keys
        .iter()
        .map(|key| Operation::put(Table::Recipients, key, &changed).unwrap())
        .collect();
    assert_eq!(
        commit_encoded(&store, request(sequence), &operations, &mut []),
        Ok(Sequence::from_u64(sequence + 1))
    );
    sequence += 1;
    let ids: Vec<_> = (0u16..2048)
        .map(|ordinal| {
            let mut bytes = [40; 16];
            bytes[..2].copy_from_slice(&ordinal.to_be_bytes());
            SubmissionId::from_bytes(bytes)
        })
        .collect();
    let keys: Vec<_> = ids
        .iter()
        .map(|id| {
            let mut key = [0; 20];
            Key::Recipient(*id, 0).encode(&mut key).unwrap();
            key
        })
        .collect();
    let initial = encode(Row::Submission(submission(1)));
    let mut setup = Vec::new();
    for (id, key) in ids.iter().zip(&keys) {
        setup.extend([
            Operation::put(Table::Submissions, id.as_bytes(), &initial).unwrap(),
            Operation::put(Table::Recipients, key, &recipient).unwrap(),
        ]);
    }
    assert_eq!(setup.len(), 4096);
    assert_eq!(
        commit_encoded(&store, request(sequence), &setup, &mut []),
        Ok(Sequence::from_u64(sequence + 1))
    );
    sequence += 1;
    let completed = encode(Row::Submission(SubmissionRow {
        completed_at: Some(2),
        notification: NotificationState::Pending,
        ..submission(1)
    }));
    let failed = encode(Row::Recipient(RecipientRow {
        state: RecipientState::Failed,
        reason: FailureReason::Expired,
        next_attempt_at: None,
        ..queued()
    }));
    let mut notices = Vec::new();
    for key in &keys {
        notices.push(Operation::put(Table::Recipients, key, &failed).unwrap());
    }
    for id in &ids {
        notices.push(Operation::put(Table::Submissions, id.as_bytes(), &completed).unwrap());
    }
    assert_eq!(notices.len(), 4096);
    let started = std::time::Instant::now();
    assert_eq!(
        commit_encoded(&store, request(sequence), &notices, &mut []),
        Ok(Sequence::from_u64(sequence + 1))
    );
    eprintln!(
        "2048 notice completions: {:?}; debug_assertions={}",
        started.elapsed(),
        cfg!(debug_assertions)
    );
}

fn failed_notice(
    store: &IndexStore<'_>,
    encoded: bool,
    stored: bool,
) -> (SubmissionRow<'static>, RecipientRow<'static>, u64) {
    create(store, 1, &[0]).unwrap();
    let mut recipient = queued();
    recipient.state = RecipientState::Failed;
    recipient.reason = FailureReason::Expired;
    recipient.next_attempt_at = None;
    let mut sub = submission(1);
    sub.completed_at = Some(1);
    sub.notification = NotificationState::Pending;
    let key = recipient_key(0);
    let recipient_bytes = encode(Row::Recipient(recipient));
    let sub_bytes = encode(Row::Submission(sub));
    apply(
        store,
        1,
        &[
            Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap(),
            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
        ],
        encoded,
    )
    .unwrap();
    let sequence = if stored {
        sub.notification = NotificationState::Stored;
        sub.notification_email = Some(EmailId::from_bytes([14; 16]));
        let bytes = encode(Row::Submission(sub));
        apply(
            store,
            2,
            &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap()],
            encoded,
        )
        .unwrap();
        3
    } else {
        2
    };
    (sub, recipient, sequence)
}

#[test]
fn cancellation_cannot_erase_a_pending_failure_notice() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let (mut sub, mut recipient, sequence) = failed_notice(&store, encoded, false);
        recipient.state = RecipientState::Canceled;
        recipient.reason = FailureReason::Canceled;
        let key = recipient_key(0);
        let recipient_bytes = encode(Row::Recipient(recipient));
        let cancel = Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap();
        let original = encode(Row::Submission(sub));
        sub.notification = NotificationState::None;
        let cleared = encode(Row::Submission(sub));
        rejected(apply(
            &store,
            sequence,
            &[
                cancel,
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &cleared).unwrap(),
            ],
            encoded,
        ));
        assert_eq!(
            apply(
                &store,
                sequence,
                &[
                    cancel,
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &original).unwrap(),
                ],
                encoded
            ),
            Ok(Sequence::from_u64(sequence + 1))
        );
        sub.notification = NotificationState::Stored;
        sub.notification_email = Some(EmailId::from_bytes([14; 16]));
        let stored = encode(Row::Submission(sub));
        assert_eq!(
            apply(
                &store,
                sequence + 1,
                &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &stored).unwrap()],
                encoded
            ),
            Ok(Sequence::from_u64(sequence + 2))
        );
    }
}

#[test]
fn a_stored_notice_cannot_regress_even_through_delete_and_reinsert() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let (sub, recipient, sequence) = failed_notice(&store, encoded, true);
        let key = recipient_key(0);
        let delete = Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
        for clear in [false, true] {
            let mut changed = sub;
            changed.notification_email = None;
            changed.notification = if clear {
                NotificationState::None
            } else {
                NotificationState::Pending
            };
            let mut final_recipient = recipient;
            if clear {
                final_recipient.state = RecipientState::Canceled;
                final_recipient.reason = FailureReason::Canceled;
            }
            let recipient_bytes = encode(Row::Recipient(final_recipient));
            let bytes = encode(Row::Submission(changed));
            rejected(apply(
                &store,
                sequence,
                &[
                    delete,
                    Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap(),
                ],
                encoded,
            ));
        }
        let mut canceled = recipient;
        canceled.state = RecipientState::Canceled;
        canceled.reason = FailureReason::Canceled;
        let bytes = encode(Row::Recipient(canceled));
        let sub_bytes = encode(Row::Submission(sub));
        assert_eq!(
            apply(
                &store,
                sequence,
                &[
                    delete,
                    Operation::put(Table::Recipients, &key, &bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                ],
                encoded
            ),
            Ok(Sequence::from_u64(sequence + 1))
        );
    }
}

#[test]
fn a_stored_notice_keeps_its_historical_email_identity() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let (sub, _, sequence) = failed_notice(&store, encoded, true);
        let original = encode(Row::Submission(sub));
        let mut changed = sub;
        changed.notification_email = Some(EmailId::from_bytes([15; 16]));
        let replacement = encode(Row::Submission(changed));
        let good = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &original).unwrap();
        let bad = Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &replacement).unwrap();
        let delete = Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
        rejected(apply(&store, sequence, &[delete, bad], encoded));
        assert_eq!(
            apply(&store, sequence, &[bad, good], encoded),
            Ok(Sequence::from_u64(sequence + 1))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 65536];
        let (row, _) = view
            .get(Key::Submission(SUBMISSION), &mut scratch)
            .unwrap()
            .unwrap();
        assert_eq!(row, Row::Submission(sub));
    }
}

#[test]
fn notice_history_does_not_prove_service_notice_creation() {
    for encoded in [false, true] {
        for state in [RecipientState::Failed, RecipientState::Canceled] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create(&store, 1, &[0]).unwrap();
            let mut recipient = queued();
            recipient.state = state;
            recipient.reason = if state == RecipientState::Canceled {
                FailureReason::Canceled
            } else {
                FailureReason::Expired
            };
            recipient.next_attempt_at = None;
            let mut sub = submission(1);
            sub.completed_at = Some(1);
            sub.notification = NotificationState::Pending;
            let key = recipient_key(0);
            let recipient_bytes = encode(Row::Recipient(recipient));
            let sub_bytes = encode(Row::Submission(sub));
            let email = EmailId::from_bytes([14; 16]);
            let mut skipped = sub;
            skipped.notification = NotificationState::Stored;
            skipped.notification_email = Some(email);
            let skipped_bytes = encode(Row::Submission(skipped));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[
                        Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap(),
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &skipped_bytes)
                            .unwrap(),
                    ],
                    encoded,
                ),
                Err(CommitError::Rejected(ports::Error::Conflict))
            );
            assert_submission(&store, submission(1), 1);
            assert_recipient(&store, queued());
            if state == RecipientState::Canceled {
                rejected(apply(
                    &store,
                    1,
                    &[
                        Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap(),
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes)
                            .unwrap(),
                    ],
                    encoded,
                ));
                assert_submission(&store, submission(1), 1);
                assert_recipient(&store, queued());
                continue;
            }
            apply(
                &store,
                1,
                &[
                    Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                ],
                encoded,
            )
            .unwrap();
            assert_submission(&store, sub, 2);
            sub.notification = NotificationState::Stored;
            sub.notification_email = Some(email);
            let stored = encode(Row::Submission(sub));
            apply(
                &store,
                2,
                &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &stored).unwrap()],
                encoded,
            )
            .unwrap();
            assert_submission(&store, sub, 3);
            let mut view = store.view(ACCOUNT, deadline()).unwrap();
            let mut scratch = [0; 65536];
            assert!(view.get(Key::Email(email), &mut scratch).unwrap().is_none());
        }
    }
}

fn terminal(state: RecipientState) -> RecipientRow<'static> {
    let mut row = queued();
    row.state = state;
    row.next_attempt_at = None;
    match state {
        RecipientState::Accepted => {
            row.attempt = Some(AttemptId::from_bytes([8; 16]));
            row.attempt_count = 1;
            row.last_attempt_at = Some(1);
            row.phase = AttemptPhase::Final;
            row.rcpt_reply = Some("250 accepted");
            row.data_reply = Some("250 accepted");
        }
        RecipientState::Failed => row.reason = FailureReason::Expired,
        RecipientState::Canceled => row.reason = FailureReason::Canceled,
        RecipientState::OutcomeUnknown => {
            row = uncertain();
            row.attempt_count = 1;
            row.state = state;
            row.next_attempt_at = None;
            row.reason = FailureReason::Expired;
        }
        _ => panic!("test requires a terminal state"),
    }
    row
}
fn seed_terminal(store: &IndexStore<'_>, state: RecipientState, encoded: bool) {
    seed_terminal_row(store, terminal(state), encoded);
}
fn seed_terminal_row(store: &IndexStore<'_>, row: RecipientRow<'_>, encoded: bool) {
    let state = row.state;
    let first = recipient_key(0);
    let second = recipient_key(1);
    let mut sub = submission(2);
    if state == RecipientState::Canceled {
        sub.completed_at = Some(1);
    }
    let other = if state == RecipientState::Canceled {
        row
    } else {
        queued()
    };
    create_group(store, sub, &[(0, row), (1, other)], encoded).unwrap();
    // Keep the existing snapshot sequence with an identical group PUT.
    let row = encode(Row::Recipient(row));
    let sub_bytes = encode(Row::Submission(sub));
    let mut operations = vec![
        Operation::put(Table::Recipients, &first, &row).unwrap(),
        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
    ];
    if state == RecipientState::Canceled {
        operations.push(Operation::put(Table::Recipients, &second, &row).unwrap());
    }
    apply(store, 1, &operations, encoded).unwrap();
}
const TERMINAL_STATES: &[RecipientState] = &[
    RecipientState::Accepted,
    RecipientState::Failed,
    RecipientState::Canceled,
    RecipientState::OutcomeUnknown,
];
#[test]
fn terminal_recipients_cannot_regain_dispatch_obligations() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for &state in TERMINAL_STATES {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            seed_terminal(&store, state, encoded);
            let mut next = terminal(state);
            match state {
                RecipientState::Accepted => {
                    next.state = RecipientState::RetryWait;
                    next.reason = FailureReason::Network;
                    next.next_attempt_at = Some(2);
                }
                RecipientState::Failed | RecipientState::Canceled => next = queued(),
                RecipientState::OutcomeUnknown => {
                    next.reason = FailureReason::Network;
                    next.next_attempt_at = Some(2);
                }
                _ => panic!("test requires a terminal transition"),
            }
            let next_bytes = encode(Row::Recipient(next));
            let first = recipient_key(0);
            let second = recipient_key(1);
            let second_bytes = encode(Row::Recipient(queued()));
            let sub_bytes = encode(Row::Submission(submission(2)));
            let result = apply(
                &store,
                2,
                &[
                    Operation::delete(Table::Recipients, &first).unwrap(),
                    Operation::put(Table::Recipients, &first, &next_bytes).unwrap(),
                    Operation::put(Table::Recipients, &second, &second_bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                ],
                encoded,
            );
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                accepted.push((encoded, state, result));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "revival was not rejected: {accepted:?}"
    );
}
#[test]
fn terminal_recipients_cannot_record_a_new_attempt() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for &state in TERMINAL_STATES {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            seed_terminal(&store, state, encoded);
            let mut next = terminal(state);
            next.attempt_count += 1;
            next.attempt = Some(AttemptId::from_bytes([9; 16]));
            next.last_attempt_at = Some(2);
            next.phase = AttemptPhase::Final;
            let next_bytes = encode(Row::Recipient(next));
            let first = recipient_key(0);
            let result = apply(
                &store,
                2,
                &[Operation::put(Table::Recipients, &first, &next_bytes).unwrap()],
                encoded,
            );
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                accepted.push((encoded, state, result));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "new terminal attempt was not rejected: {accepted:?}"
    );
}
#[test]
fn terminal_history_allows_diagnostics_and_whole_group_failure_cancellation() {
    for encoded in [false, true] {
        for &state in TERMINAL_STATES {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            seed_terminal(&store, state, encoded);
            let mut next = terminal(state);
            next.diagnostic = "retained terminal outcome";
            let next_bytes = encode(Row::Recipient(next));
            let first = recipient_key(0);
            let put = Operation::put(Table::Recipients, &first, &next_bytes).unwrap();
            assert_eq!(
                apply(
                    &store,
                    2,
                    &[Operation::delete(Table::Recipients, &first).unwrap(), put],
                    encoded,
                ),
                Ok(Sequence::from_u64(3))
            );
            if state == RecipientState::Failed {
                next.state = RecipientState::Canceled;
                next.reason = FailureReason::Canceled;
                let bytes = encode(Row::Recipient(next));
                let second = recipient_key(1);
                let mut sub = submission(2);
                sub.completed_at = Some(1);
                let sub_bytes = encode(Row::Submission(sub));
                assert_eq!(
                    apply(
                        &store,
                        3,
                        &[
                            Operation::put(Table::Recipients, &first, &bytes).unwrap(),
                            Operation::put(Table::Recipients, &second, &bytes).unwrap(),
                            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes)
                                .unwrap(),
                        ],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(4))
                );
            }
        }
    }
}

fn attempted_terminal(state: RecipientState) -> RecipientRow<'static> {
    let mut row = terminal(state);
    if row.attempt_count == 0 {
        row.attempt = Some(AttemptId::from_bytes([8; 16]));
        row.attempt_count = 1;
        row.last_attempt_at = Some(1);
        row.phase = AttemptPhase::Final;
    }
    row.rcpt_reply = Some("250 recipient accepted");
    row.data_reply = Some(if state == RecipientState::Accepted {
        "250 delivered"
    } else {
        "550 rejected"
    });
    if state == RecipientState::Failed {
        row.reason = FailureReason::SmtpPermanent;
    }
    row
}
#[test]
fn terminal_outcomes_cannot_be_reclassified() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for (before, after) in [
            (RecipientState::Accepted, RecipientState::Failed),
            (RecipientState::Accepted, RecipientState::OutcomeUnknown),
            (RecipientState::OutcomeUnknown, RecipientState::Accepted),
            (RecipientState::Failed, RecipientState::Accepted),
            (RecipientState::Failed, RecipientState::OutcomeUnknown),
            (RecipientState::Canceled, RecipientState::Failed),
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            seed_terminal_row(&store, attempted_terminal(before), encoded);
            let mut next = attempted_terminal(before);
            next.state = after;
            match after {
                RecipientState::Accepted => {
                    next.phase = AttemptPhase::Final;
                    next.reason = FailureReason::None;
                    next.rcpt_reply = Some("250 accepted");
                    next.data_reply = Some("250 accepted");
                }
                RecipientState::OutcomeUnknown => {
                    next.uncertain = true;
                    next.reason = FailureReason::Expired;
                }
                RecipientState::Failed => next.reason = FailureReason::Expired,
                _ => panic!("test requires a terminal transition"),
            }
            let bytes = encode(Row::Recipient(next));
            let key = recipient_key(0);
            let mut operations = vec![Operation::put(Table::Recipients, &key, &bytes).unwrap()];
            let second = recipient_key(1);
            let mut sub = submission(2);
            sub.completed_at = Some(1);
            sub.notification = NotificationState::Pending;
            let sub_bytes = encode(Row::Submission(sub));
            if before == RecipientState::Canceled {
                operations.push(Operation::put(Table::Recipients, &second, &bytes).unwrap());
                operations.push(
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                );
            }
            let result = apply(&store, 2, &operations, encoded);
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                accepted.push((encoded, before, after, result));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "terminal reclassification was not rejected: {accepted:?}"
    );
}
#[test]
fn terminal_reply_reason_phase_and_uncertainty_history_is_fixed() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for &state in TERMINAL_STATES {
            for field in 0..5 {
                if (field == 2
                    && !matches!(
                        state,
                        RecipientState::Failed | RecipientState::OutcomeUnknown
                    ))
                    || (field == 3 && state != RecipientState::OutcomeUnknown)
                    || (field == 4 && state != RecipientState::Accepted)
                {
                    continue;
                }
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let mut row = attempted_terminal(state);
                seed_terminal_row(&store, row, encoded);
                match field {
                    0 => row.rcpt_reply = Some("250 replacement recipient reply"),
                    1 => {
                        row.data_reply = Some(if state == RecipientState::Accepted {
                            "250 replacement data reply"
                        } else {
                            "550 replacement data reply"
                        })
                    }
                    2 => {
                        row.reason = if state == RecipientState::Failed {
                            FailureReason::Expired
                        } else {
                            FailureReason::SmtpPermanent
                        }
                    }
                    3 => row.phase = AttemptPhase::AcceptancePossible,
                    _ => row.uncertain = true,
                }
                let key = recipient_key(0);
                let bytes = encode(Row::Recipient(row));
                let result = apply(
                    &store,
                    2,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    accepted.push((encoded, state, field, result));
                    continue;
                }
                let mut retained = attempted_terminal(state);
                retained.diagnostic = "original outcome retained";
                let retained_bytes = encode(Row::Recipient(retained));
                assert_eq!(
                    apply(
                        &store,
                        2,
                        &[Operation::put(Table::Recipients, &key, &retained_bytes).unwrap()],
                        encoded
                    ),
                    Ok(Sequence::from_u64(3))
                );
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                let mut scratch = [0; 65536];
                assert_eq!(
                    view.get(Key::Recipient(SUBMISSION, 0), &mut scratch)
                        .unwrap()
                        .unwrap()
                        .0,
                    Row::Recipient(retained)
                );
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "terminal history replacement was not rejected: {accepted:?}"
    );
}
#[test]
fn terminal_final_operation_wins_and_attempted_cancellation_retains_replies() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let original = attempted_terminal(RecipientState::Failed);
        seed_terminal_row(&store, original, encoded);
        let key = recipient_key(0);
        let bytes = encode(Row::Recipient(original));
        let good = Operation::put(Table::Recipients, &key, &bytes).unwrap();
        let mut revival = original;
        revival.state = RecipientState::RetryWait;
        revival.reason = FailureReason::Network;
        revival.next_attempt_at = Some(2);
        let changed = encode(Row::Recipient(revival));
        let bad = Operation::put(Table::Recipients, &key, &changed).unwrap();
        rejected(apply(&store, 2, &[good, bad], encoded));
        assert_eq!(
            apply(&store, 2, &[bad, good], encoded),
            Ok(Sequence::from_u64(3))
        );
        let mut canceled = original;
        canceled.state = RecipientState::Canceled;
        canceled.reason = FailureReason::Canceled;
        let canceled_bytes = encode(Row::Recipient(canceled));
        let second = recipient_key(1);
        let second_bytes = encode(Row::Recipient(terminal(RecipientState::Canceled)));
        let mut sub = submission(2);
        sub.completed_at = Some(1);
        let sub_bytes = encode(Row::Submission(sub));
        assert_eq!(
            apply(
                &store,
                3,
                &[
                    Operation::put(Table::Recipients, &key, &canceled_bytes).unwrap(),
                    Operation::put(Table::Recipients, &second, &second_bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                ],
                encoded
            ),
            Ok(Sequence::from_u64(4))
        );
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 65536];
        assert_eq!(
            view.get(Key::Recipient(SUBMISSION, 0), &mut scratch)
                .unwrap()
                .unwrap()
                .0,
            Row::Recipient(canceled)
        );
    }
}

#[test]
fn a_new_attempt_advances_once_and_changes_the_previous_id() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for state in [
            RecipientState::Queued,
            RecipientState::RetryWait,
            RecipientState::OutcomeUnknown,
        ] {
            for reused in [false, true] {
                if reused && state == RecipientState::Queued {
                    continue;
                }
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = pending_attempt(state);
                create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                let mut next = next_attempt(original);
                if reused {
                    next.attempt = original.attempt;
                } else {
                    next.attempt_count += 1;
                }
                let bytes = encode(Row::Recipient(next));
                let key = recipient_key(0);
                let result = apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    accepted.push((encoded, state, reused, result));
                    continue;
                }
                let valid = next_attempt(original);
                let valid_bytes = encode(Row::Recipient(valid));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[Operation::put(Table::Recipients, &key, &valid_bytes).unwrap()],
                        encoded
                    ),
                    Ok(Sequence::from_u64(2))
                );
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                let mut scratch = [0; 65536];
                assert_eq!(
                    view.get(Key::Recipient(SUBMISSION, 0), &mut scratch)
                        .unwrap()
                        .unwrap()
                        .0,
                    Row::Recipient(valid)
                );
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "invalid attempt advance was not rejected: {accepted:?}"
    );
}
#[test]
fn final_attempt_put_controls_repeated_keys_against_the_original_row() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let original = pending_attempt(RecipientState::RetryWait);
        create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
        let key = recipient_key(0);
        let valid = next_attempt(original);
        let valid_bytes = encode(Row::Recipient(valid));
        let good = Operation::put(Table::Recipients, &key, &valid_bytes).unwrap();
        let mut skipped = valid;
        skipped.attempt_count += 1;
        let bytes = encode(Row::Recipient(skipped));
        let bad = Operation::put(Table::Recipients, &key, &bytes).unwrap();
        let delete = Operation::delete(Table::Recipients, &key).unwrap();
        rejected(apply(&store, 1, &[good, bad], encoded));
        let mut chained = skipped;
        chained.attempt = Some(AttemptId::from_bytes([10; 16]));
        chained.last_attempt_at = Some(3);
        let chained_bytes = encode(Row::Recipient(chained));
        let chained_put = Operation::put(Table::Recipients, &key, &chained_bytes).unwrap();
        let mut retry = valid;
        retry.state = RecipientState::RetryWait;
        retry.phase = AttemptPhase::Final;
        retry.reason = FailureReason::Network;
        retry.next_attempt_at = Some(3);
        let retry_bytes = encode(Row::Recipient(retry));
        let retry_put = Operation::put(Table::Recipients, &key, &retry_bytes).unwrap();
        rejected(apply(&store, 1, &[good, retry_put, chained_put], encoded));
        rejected(apply(&store, 1, &[delete, bad], encoded));
        assert_eq!(
            apply(&store, 1, &[bad, delete, good], encoded),
            Ok(Sequence::from_u64(2))
        );
        let separate_fixture = Fixture::new();
        let mut separate_root = separate_fixture.locked();
        let separate = open(&mut separate_root);
        create_with_recipient(&separate, 1, &[0], original, encoded).unwrap();
        assert_eq!(
            apply(&separate, 1, &[good], encoded),
            Ok(Sequence::from_u64(2))
        );
        assert_eq!(
            apply(&separate, 2, &[retry_put], encoded),
            Ok(Sequence::from_u64(3))
        );
        assert_eq!(
            apply(&separate, 3, &[chained_put], encoded),
            Ok(Sequence::from_u64(4))
        );
    }
}
#[test]
fn the_last_attempt_count_is_usable_without_wrapping_or_replacing_its_id() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let mut original = pending_attempt(RecipientState::RetryWait);
        original.attempt_count = u32::MAX - 1;
        create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
        let key = recipient_key(0);
        let last = next_attempt(original);
        let bytes = encode(Row::Recipient(last));
        assert_eq!(
            apply(
                &store,
                1,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded
            ),
            Ok(Sequence::from_u64(2))
        );
        let wrapped = encode(Row::Recipient(queued()));
        rejected(apply(
            &store,
            2,
            &[Operation::put(Table::Recipients, &key, &wrapped).unwrap()],
            encoded,
        ));
        let mut replacement = last;
        replacement.attempt = Some(AttemptId::from_bytes([10; 16]));
        let replacement_bytes = encode(Row::Recipient(replacement));
        rejected(apply(
            &store,
            2,
            &[Operation::put(Table::Recipients, &key, &replacement_bytes).unwrap()],
            encoded,
        ));
        let mut retained = last;
        retained.diagnostic = "last attempt remains owned";
        let retained_bytes = encode(Row::Recipient(retained));
        assert_eq!(
            apply(
                &store,
                2,
                &[Operation::put(Table::Recipients, &key, &retained_bytes).unwrap()],
                encoded
            ),
            Ok(Sequence::from_u64(3))
        );
    }
}

#[test]
fn a_new_attempt_must_first_commit_the_prepared_phase() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for state in [
            RecipientState::Queued,
            RecipientState::RetryWait,
            RecipientState::OutcomeUnknown,
        ] {
            for variant in 0..7 {
                if state == RecipientState::OutcomeUnknown && matches!(variant, 2 | 4 | 6) {
                    continue;
                }
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = pending_attempt(state);
                create_with_recipient(&store, 2, &[0, 1], original, encoded).unwrap();
                let mut next = next_attempt(original);
                match variant {
                    0 => {
                        next.phase = AttemptPhase::Body;
                        next.rcpt_reply = Some("250 recipient accepted");
                    }
                    1 => {
                        next.phase = AttemptPhase::AcceptancePossible;
                        next.rcpt_reply = Some("250 recipient accepted");
                    }
                    2 => {
                        next.state = RecipientState::RetryWait;
                        next.phase = AttemptPhase::Final;
                        next.reason = FailureReason::Network;
                        next.next_attempt_at = Some(3);
                    }
                    3 => {
                        next.state = RecipientState::Accepted;
                        next.phase = AttemptPhase::Final;
                        next.rcpt_reply = Some("250 recipient accepted");
                        next.data_reply = Some("250 delivered");
                    }
                    4 => {
                        next.state = RecipientState::Failed;
                        next.phase = AttemptPhase::Final;
                        next.reason = FailureReason::Expired;
                    }
                    5 => {
                        next.state = RecipientState::OutcomeUnknown;
                        next.phase = AttemptPhase::Final;
                        next.uncertain = true;
                        next.reason = FailureReason::Network;
                        next.next_attempt_at = Some(3);
                    }
                    _ => {
                        next.state = RecipientState::Canceled;
                        next.phase = AttemptPhase::Final;
                        next.reason = FailureReason::Canceled;
                    }
                }
                let bytes = encode(Row::Recipient(next));
                let first = recipient_key(0);
                let second = recipient_key(1);
                let mut operations =
                    vec![Operation::put(Table::Recipients, &first, &bytes).unwrap()];
                let mut sub = submission(2);
                let other = if variant == 6 {
                    sub.completed_at = Some(3);
                    next
                } else {
                    original
                };
                assert_fresh_group(sub, &[(0, next), (1, other)], encoded);
                let sub_bytes = encode(Row::Submission(sub));
                if variant == 6 {
                    operations.push(Operation::put(Table::Recipients, &second, &bytes).unwrap());
                    operations.push(
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes)
                            .unwrap(),
                    );
                }
                let result = apply(&store, 1, &operations, encoded);
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    accepted.push((encoded, state, variant, result));
                    continue;
                }
                let valid = next_attempt(original);
                let valid_bytes = encode(Row::Recipient(valid));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[Operation::put(Table::Recipients, &first, &valid_bytes).unwrap()],
                        encoded
                    ),
                    Ok(Sequence::from_u64(2))
                );
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                let mut scratch = [0; 65536];
                assert_eq!(
                    view.get(Key::Recipient(SUBMISSION, 0), &mut scratch)
                        .unwrap()
                        .unwrap()
                        .0,
                    Row::Recipient(valid)
                );
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "attempt skipped Prepared: {accepted:?}"
    );
}
#[test]
fn an_in_flight_attempt_cannot_be_replaced_by_a_fresh_attempt() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for phase in [
            AttemptPhase::Prepared,
            AttemptPhase::Body,
            AttemptPhase::AcceptancePossible,
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let mut original = pending_attempt(RecipientState::RetryWait);
            original.state = RecipientState::InFlight;
            original.phase = phase;
            original.reason = FailureReason::None;
            original.next_attempt_at = None;
            original.rcpt_reply = Some("250 recipient accepted");
            create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
            let next = next_attempt(original);
            let bytes = encode(Row::Recipient(next));
            let key = recipient_key(0);
            assert_fresh_group(submission(1), &[(0, next)], encoded);
            let result = apply(
                &store,
                1,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded,
            );
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                accepted.push((encoded, phase, result));
                continue;
            }
            let mut retained = original;
            retained.diagnostic = "current attempt still owns responsibility";
            let retained_bytes = encode(Row::Recipient(retained));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &retained_bytes).unwrap()],
                    encoded
                ),
                Ok(Sequence::from_u64(2))
            );
        }
    }
    assert!(
        accepted.is_empty(),
        "active attempt was replaced: {accepted:?}"
    );
}
#[test]
fn a_new_attempt_cannot_combine_prepared_and_body_in_one_commit() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_with(&store, 1, &[0], encoded).unwrap();
        let key = recipient_key(0);
        let prepared = next_attempt(queued());
        let prepared_bytes = encode(Row::Recipient(prepared));
        let good = Operation::put(Table::Recipients, &key, &prepared_bytes).unwrap();
        let mut body = prepared;
        body.phase = AttemptPhase::Body;
        body.rcpt_reply = Some("250 recipient accepted");
        assert_fresh_group(submission(1), &[(0, body)], encoded);
        let body_bytes = encode(Row::Recipient(body));
        let bad = Operation::put(Table::Recipients, &key, &body_bytes).unwrap();
        let delete = Operation::delete(Table::Recipients, &key).unwrap();
        rejected(apply(&store, 1, &[good, bad], encoded));
        rejected(apply(&store, 1, &[delete, bad], encoded));
        assert_eq!(
            apply(&store, 1, &[bad, delete, good], encoded),
            Ok(Sequence::from_u64(2))
        );
        assert_eq!(apply(&store, 2, &[bad], encoded), Ok(Sequence::from_u64(3)));
    }
}

#[test]
fn pending_recipients_cannot_reenter_in_flight_with_the_previous_attempt() {
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for state in [RecipientState::RetryWait, RecipientState::OutcomeUnknown] {
            for phase in [
                AttemptPhase::Prepared,
                AttemptPhase::Body,
                AttemptPhase::AcceptancePossible,
            ] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = pending_attempt(state);
                create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                let mut next = next_attempt(original);
                next.attempt_count = original.attempt_count;
                next.attempt = original.attempt;
                next.last_attempt_at = original.last_attempt_at;
                next.phase = phase;
                next.rcpt_reply = Some("250 recipient accepted");
                let key = recipient_key(0);
                let bytes = encode(Row::Recipient(next));
                assert_fresh_group(submission(1), &[(0, next)], encoded);
                let result = apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    accepted.push((encoded, state, phase, result));
                    continue;
                }
                let valid = next_attempt(original);
                let valid_bytes = encode(Row::Recipient(valid));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[Operation::put(Table::Recipients, &key, &valid_bytes).unwrap()],
                        encoded
                    ),
                    Ok(Sequence::from_u64(2))
                );
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "previous attempt was revived: {accepted:?}"
    );
}

#[test]
fn completed_timestamp_is_retained_across_terminal_updates_and_repeated_keys() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for state in [
            RecipientState::Accepted,
            RecipientState::Canceled,
            RecipientState::Failed,
            RecipientState::OutcomeUnknown,
        ] {
            for time in [i64::MIN, -1, 2, i64::MAX] {
                for repeated in [false, true] {
                    let fixture = Fixture::new();
                    let mut root = fixture.locked();
                    let store = open(&mut root);
                    let original = terminal(state);
                    let mut sub = submission(1);
                    sub.completed_at = Some(1);
                    if matches!(
                        state,
                        RecipientState::Failed | RecipientState::OutcomeUnknown
                    ) {
                        sub.notification = NotificationState::Pending;
                    }
                    create_group(&store, sub, &[(0, original)], encoded).unwrap();
                    let changed = RecipientRow {
                        diagnostic: "terminal diagnostic update",
                        ..original
                    };
                    let next = SubmissionRow {
                        completed_at: Some(time),
                        ..sub
                    };
                    assert_fresh_group(next, &[(0, changed)], encoded);
                    let key = recipient_key(0);
                    let row = encode(Row::Recipient(changed));
                    let diag = Operation::put(Table::Recipients, &key, &row).unwrap();
                    let bytes = encode(Row::Submission(next));
                    let bad =
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap();
                    let unset_bytes = encode(Row::Submission(SubmissionRow {
                        completed_at: None,
                        ..sub
                    }));
                    let unset =
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &unset_bytes)
                            .unwrap();
                    let delete =
                        Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
                    let operations = if repeated {
                        vec![unset, delete, bad, diag]
                    } else {
                        vec![diag, bad]
                    };
                    let result = apply(&store, 1, &operations, encoded);
                    if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                        unexpected.push((encoded, state, time, repeated, result));
                        continue;
                    }
                    assert_recipient(&store, original);
                    assert_submission(&store, sub, 1);
                    let good_bytes = encode(Row::Submission(sub));
                    let good =
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &good_bytes)
                            .unwrap();
                    assert_eq!(
                        apply(&store, 1, &[bad, delete, good, diag], encoded),
                        Ok(Sequence::from_u64(2))
                    );
                    assert_recipient(&store, changed);
                    assert_submission(&store, sub, 2);
                }
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "completed timestamp rewritten: {unexpected:?}"
    );
}

#[test]
fn cancellation_of_completed_failure_keeps_time_and_notice() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for stored in [false, true] {
            for time in [-6, 0] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = terminal(RecipientState::Failed);
                let mut sub = submission(1);
                sub.completed_at = Some(-5);
                sub.notification = if stored {
                    NotificationState::Stored
                } else {
                    NotificationState::Pending
                };
                sub.notification_email = stored.then_some(EmailId::from_bytes([14; 16]));
                create_group(&store, sub, &[(0, original)], encoded).unwrap();
                let canceled = RecipientRow {
                    state: RecipientState::Canceled,
                    reason: FailureReason::Canceled,
                    ..original
                };
                let next = SubmissionRow {
                    completed_at: Some(time),
                    ..sub
                };
                assert_fresh_group(next, &[(0, canceled)], encoded);
                let key = recipient_key(0);
                let row = encode(Row::Recipient(canceled));
                let cancel = Operation::put(Table::Recipients, &key, &row).unwrap();
                let bytes = encode(Row::Submission(next));
                let bad =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap();
                let result = apply(&store, 1, &[cancel, bad], encoded);
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, stored, time, result));
                    continue;
                }
                assert_recipient(&store, original);
                assert_submission(&store, sub, 1);
                let good_bytes = encode(Row::Submission(sub));
                let good =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &good_bytes).unwrap();
                assert_eq!(
                    apply(&store, 1, &[cancel, good], encoded),
                    Ok(Sequence::from_u64(2))
                );
                assert_recipient(&store, canceled);
                assert_submission(&store, sub, 2);
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "cancellation rewrote completion time: {unexpected:?}"
    );
}

#[test]
fn initial_completion_accepts_signed_clock_observation_and_retains_it() {
    for encoded in [false, true] {
        for time in [i64::MIN, -1, 0, i64::MAX] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create_group(&store, submission(1), &[(0, queued())], encoded).unwrap();
            let pending = encode(Row::Submission(submission(1)));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending)
                            .unwrap()
                    ],
                    encoded
                ),
                Ok(Sequence::from_u64(2))
            );
            let sub = SubmissionRow {
                completed_at: Some(time),
                notification: NotificationState::Pending,
                ..submission(1)
            };
            let row = encode(Row::Recipient(terminal(RecipientState::Failed)));
            let key = recipient_key(0);
            let bytes = encode(Row::Submission(sub));
            let complete =
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap();
            assert_eq!(
                apply(
                    &store,
                    2,
                    &[
                        Operation::put(Table::Recipients, &key, &row).unwrap(),
                        complete
                    ],
                    encoded
                ),
                Ok(Sequence::from_u64(3))
            );
            assert_submission(&store, sub, 3);
            assert_eq!(
                apply(&store, 3, &[complete], encoded),
                Ok(Sequence::from_u64(4))
            );
            assert_submission(&store, sub, 4);
        }
    }
}

#[test]
fn storing_a_failure_notice_keeps_the_original_completion_time() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for state in [RecipientState::Failed, RecipientState::OutcomeUnknown] {
            for time in [-1, 2] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = terminal(state);
                let sub = SubmissionRow {
                    completed_at: Some(1),
                    notification: NotificationState::Pending,
                    ..submission(1)
                };
                create_group(&store, sub, &[(0, original)], encoded).unwrap();
                let stored = SubmissionRow {
                    notification: NotificationState::Stored,
                    notification_email: Some(EmailId::from_bytes([14; 16])),
                    ..sub
                };
                let next = SubmissionRow {
                    completed_at: Some(time),
                    ..stored
                };
                // Notice Email creation remains a separate service obligation.
                assert_fresh_group(next, &[(0, original)], encoded);
                let bytes = encode(Row::Submission(next));
                let result = apply(
                    &store,
                    1,
                    &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, state, time, result));
                    continue;
                }
                assert_submission(&store, sub, 1);
                assert_recipient(&store, original);
                let bytes = encode(Row::Submission(stored));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[
                            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes)
                                .unwrap()
                        ],
                        encoded
                    ),
                    Ok(Sequence::from_u64(2))
                );
                assert_submission(&store, stored, 2);
                assert_recipient(&store, original);
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "notice rewrote completion time: {unexpected:?}"
    );
}

#[test]
fn stored_notice_requires_a_separate_pending_commit() {
    #[derive(Clone, Copy, Debug)]
    enum Intermediate {
        Direct,
        Pending,
        Delete,
    }
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        let failed = terminal(RecipientState::Failed);
        let pending = SubmissionRow {
            completed_at: Some(1),
            notification: NotificationState::Pending,
            ..submission(1)
        };
        let stored = SubmissionRow {
            notification: NotificationState::Stored,
            notification_email: Some(EmailId::from_bytes([14; 16])),
            ..pending
        };
        assert_fresh_group(stored, &[(0, failed)], encoded);
        for intermediate in [
            Intermediate::Direct,
            Intermediate::Pending,
            Intermediate::Delete,
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create_group(&store, submission(1), &[(0, queued())], encoded).unwrap();
            let key = recipient_key(0);
            let recipient_bytes = encode(Row::Recipient(failed));
            let failure = Operation::put(Table::Recipients, &key, &recipient_bytes).unwrap();
            let pending_bytes = encode(Row::Submission(pending));
            let record_pending =
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending_bytes).unwrap();
            let stored_bytes = encode(Row::Submission(stored));
            let record_stored =
                Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &stored_bytes).unwrap();
            let mut operations = vec![failure];
            match intermediate {
                Intermediate::Pending => operations.push(record_pending),
                Intermediate::Delete => operations
                    .push(Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap()),
                Intermediate::Direct => {}
            }
            operations.push(record_stored);
            let result = apply(&store, 1, &operations, encoded);
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                unexpected.push((encoded, intermediate, result));
                continue;
            }
            assert_submission(&store, submission(1), 1);
            assert_recipient(&store, queued());
            assert_eq!(
                apply(&store, 1, &[failure, record_pending], encoded),
                Ok(Sequence::from_u64(2))
            );
            assert_submission(&store, pending, 2);
            assert_recipient(&store, failed);
            assert_eq!(
                apply(&store, 2, &[record_stored], encoded),
                Ok(Sequence::from_u64(3))
            );
            assert_submission(&store, stored, 3);
        }
    }
    assert!(
        unexpected.is_empty(),
        "Stored skipped durable Pending: {unexpected:?}"
    );
}

#[test]
fn completed_cancellation_without_notice_cannot_acquire_one() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for notice in [NotificationState::Pending, NotificationState::Stored] {
            let original = terminal(RecipientState::Canceled);
            let sub = SubmissionRow {
                completed_at: Some(1),
                ..submission(1)
            };
            let changed = RecipientRow {
                diagnostic: "retained cancellation",
                ..original
            };
            let next = SubmissionRow {
                notification: notice,
                notification_email: (notice == NotificationState::Stored)
                    .then_some(EmailId::from_bytes([14; 16])),
                ..sub
            };
            assert_fresh_group(next, &[(0, changed)], encoded);
            for repeated in [false, true] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                create_group(&store, sub, &[(0, original)], encoded).unwrap();
                let key = recipient_key(0);
                let row = encode(Row::Recipient(changed));
                let diagnostic = Operation::put(Table::Recipients, &key, &row).unwrap();
                let bytes = encode(Row::Submission(next));
                let bad =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &bytes).unwrap();
                let pending_bytes = encode(Row::Submission(SubmissionRow {
                    notification: NotificationState::Pending,
                    ..sub
                }));
                let pending =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending_bytes)
                        .unwrap();
                let delete = Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
                let operations = if repeated {
                    vec![diagnostic, pending, delete, bad]
                } else {
                    vec![diagnostic, bad]
                };
                let result = apply(&store, 1, &operations, encoded);
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, notice, repeated, result));
                    continue;
                }
                assert_submission(&store, sub, 1);
                assert_recipient(&store, original);
                let good_bytes = encode(Row::Submission(sub));
                let good =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &good_bytes).unwrap();
                assert_eq!(
                    apply(&store, 1, &[bad, delete, good, diagnostic], encoded),
                    Ok(Sequence::from_u64(2))
                );
                assert_submission(&store, sub, 2);
                assert_recipient(&store, changed);
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "completed cancellation acquired notice: {unexpected:?}"
    );
}

#[test]
fn first_cancellation_cannot_create_a_pending_notice() {
    #[derive(Clone, Copy, Debug)]
    enum Intermediate {
        Direct,
        Failed,
        Delete,
    }
    let canceled = RecipientRow {
        state: RecipientState::Canceled,
        reason: FailureReason::Canceled,
        next_attempt_at: None,
        ..queued()
    };
    let failed = RecipientRow {
        state: RecipientState::Failed,
        reason: FailureReason::Expired,
        next_attempt_at: None,
        ..queued()
    };
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for count in [1, 2] {
            let pending = SubmissionRow {
                completed_at: Some(2),
                notification: NotificationState::Pending,
                ..submission(count)
            };
            let fresh: Vec<_> = (0..count).map(|ordinal| (ordinal, canceled)).collect();
            assert_fresh_group(pending, &fresh, encoded);
            for intermediate in [
                Intermediate::Direct,
                Intermediate::Failed,
                Intermediate::Delete,
            ] {
                for parent_first in [false, true] {
                    let fixture = Fixture::new();
                    let mut root = fixture.locked();
                    let store = open(&mut root);
                    let originals: Vec<_> = (0..count).map(|ordinal| (ordinal, queued())).collect();
                    create_group(&store, submission(count), &originals, encoded).unwrap();
                    let keys: Vec<_> = (0..count).map(recipient_key).collect();
                    let canceled_bytes = encode(Row::Recipient(canceled));
                    let failed_bytes = encode(Row::Recipient(failed));
                    let pending_bytes = encode(Row::Submission(pending));
                    let parent =
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending_bytes)
                            .unwrap();
                    let mut operations = Vec::new();
                    if parent_first {
                        operations.push(parent);
                    }
                    for key in &keys {
                        match intermediate {
                            Intermediate::Direct => {}
                            Intermediate::Failed => operations.push(
                                Operation::put(Table::Recipients, key, &failed_bytes).unwrap(),
                            ),
                            Intermediate::Delete => {
                                operations.push(Operation::delete(Table::Recipients, key).unwrap())
                            }
                        }
                        operations
                            .push(Operation::put(Table::Recipients, key, &canceled_bytes).unwrap());
                    }
                    if !parent_first {
                        operations.push(parent);
                    }
                    let result = apply(&store, 1, &operations, encoded);
                    if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                        unexpected.push((encoded, count, intermediate, parent_first, result));
                        continue;
                    }
                    assert_submission(&store, submission(count), 1);
                    {
                        let mut view = store.view(ACCOUNT, deadline()).unwrap();
                        let mut scratch = [0; 1024];
                        for ordinal in 0..count {
                            assert_eq!(
                                view.get(Key::Recipient(SUBMISSION, ordinal), &mut scratch)
                                    .unwrap()
                                    .unwrap()
                                    .0,
                                Row::Recipient(queued())
                            );
                        }
                    }
                    // Intermediate cancellation must not defeat a final legitimate failure.
                    let mut repaired = Vec::new();
                    if parent_first {
                        repaired.push(parent);
                    }
                    for key in &keys {
                        repaired
                            .push(Operation::put(Table::Recipients, key, &canceled_bytes).unwrap());
                        match intermediate {
                            Intermediate::Direct => {}
                            Intermediate::Failed => repaired.push(
                                Operation::put(Table::Recipients, key, &failed_bytes).unwrap(),
                            ),
                            Intermediate::Delete => {
                                repaired.push(Operation::delete(Table::Recipients, key).unwrap())
                            }
                        }
                        repaired
                            .push(Operation::put(Table::Recipients, key, &failed_bytes).unwrap());
                    }
                    if !parent_first {
                        repaired.push(parent);
                    }
                    assert_eq!(
                        apply(&store, 1, &repaired, encoded),
                        Ok(Sequence::from_u64(2))
                    );
                    assert_submission(&store, pending, 2);
                    let mut view = store.view(ACCOUNT, deadline()).unwrap();
                    let mut scratch = [0; 1024];
                    for ordinal in 0..count {
                        assert_eq!(
                            view.get(Key::Recipient(SUBMISSION, ordinal), &mut scratch)
                                .unwrap()
                                .unwrap()
                                .0,
                            Row::Recipient(failed)
                        );
                    }
                }
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "first cancellation created notice: {unexpected:?}"
    );
}

#[test]
fn cancellation_in_another_submission_does_not_block_a_failure_notice() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_group(&store, submission(1), &[(0, queued())], encoded).unwrap();
        let other = SubmissionId::from_bytes([25; 16]);
        let mut other_key = [0; 20];
        Key::Recipient(other, 0).encode(&mut other_key).unwrap();
        let queued_bytes = encode(Row::Recipient(queued()));
        let initial_bytes = encode(Row::Submission(submission(1)));
        apply(
            &store,
            1,
            &[
                Operation::put(Table::Submissions, other.as_bytes(), &initial_bytes).unwrap(),
                Operation::put(Table::Recipients, &other_key, &queued_bytes).unwrap(),
            ],
            encoded,
        )
        .unwrap();
        let completed = SubmissionRow {
            completed_at: Some(2),
            ..submission(1)
        };
        let pending = SubmissionRow {
            notification: NotificationState::Pending,
            ..completed
        };
        let failed = RecipientRow {
            state: RecipientState::Failed,
            reason: FailureReason::Expired,
            next_attempt_at: None,
            ..queued()
        };
        let canceled = RecipientRow {
            state: RecipientState::Canceled,
            reason: FailureReason::Canceled,
            next_attempt_at: None,
            ..queued()
        };
        let key = recipient_key(0);
        let pending_bytes = encode(Row::Submission(pending));
        let completed_bytes = encode(Row::Submission(completed));
        let failed_bytes = encode(Row::Recipient(failed));
        let canceled_bytes = encode(Row::Recipient(canceled));
        assert_eq!(
            apply(
                &store,
                2,
                &[
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending_bytes)
                        .unwrap(),
                    Operation::put(Table::Recipients, &other_key, &canceled_bytes).unwrap(),
                    Operation::put(Table::Recipients, &key, &failed_bytes).unwrap(),
                    Operation::put(Table::Submissions, other.as_bytes(), &completed_bytes).unwrap(),
                ],
                encoded
            ),
            Ok(Sequence::from_u64(3))
        );
        assert_submission(&store, pending, 3);
        assert_recipient(&store, failed);
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 1024];
        assert_eq!(
            view.get(Key::Submission(other), &mut scratch)
                .unwrap()
                .unwrap()
                .0,
            Row::Submission(completed)
        );
        assert_eq!(
            view.get(Key::Recipient(other, 0), &mut scratch)
                .unwrap()
                .unwrap()
                .0,
            Row::Recipient(canceled)
        );
    }
}

#[test]
fn first_cancellation_notice_check_uses_the_final_submission_put() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_group(&store, submission(1), &[(0, queued())], encoded).unwrap();
        let canceled = RecipientRow {
            state: RecipientState::Canceled,
            reason: FailureReason::Canceled,
            next_attempt_at: None,
            ..queued()
        };
        let completed = SubmissionRow {
            completed_at: Some(2),
            ..submission(1)
        };
        let pending = SubmissionRow {
            notification: NotificationState::Pending,
            ..completed
        };
        let key = recipient_key(0);
        let canceled_bytes = encode(Row::Recipient(canceled));
        let completed_bytes = encode(Row::Submission(completed));
        let pending_bytes = encode(Row::Submission(pending));
        let cancel = Operation::put(Table::Recipients, &key, &canceled_bytes).unwrap();
        let good =
            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &completed_bytes).unwrap();
        let bad =
            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending_bytes).unwrap();
        rejected(apply(&store, 1, &[good, cancel, bad], encoded));
        assert_submission(&store, submission(1), 1);
        assert_recipient(&store, queued());
        assert_eq!(
            apply(&store, 1, &[bad, cancel, good], encoded),
            Ok(Sequence::from_u64(2))
        );
        assert_submission(&store, completed, 2);
        assert_recipient(&store, canceled);
    }
}

#[test]
fn pending_submission_deletion_requires_a_prior_completion_commit() {
    #[derive(Clone, Copy, Debug)]
    enum Intermediate {
        Direct,
        Completed,
        DeleteCompleted,
        CompletedGroup,
    }
    let prepared = RecipientRow {
        state: RecipientState::InFlight,
        phase: AttemptPhase::Prepared,
        uncertain: false,
        reason: FailureReason::None,
        next_attempt_at: None,
        ..uncertain()
    };
    let body = RecipientRow {
        phase: AttemptPhase::Body,
        rcpt_reply: Some("250 recipient accepted"),
        ..prepared
    };
    let exposed = RecipientRow {
        phase: AttemptPhase::AcceptancePossible,
        ..body
    };
    let unknown_exposed = RecipientRow {
        phase: AttemptPhase::AcceptancePossible,
        ..uncertain()
    };
    let cases = &[
        ("queued", queued()),
        ("retry wait", pending_attempt(RecipientState::RetryWait)),
        ("prepared", prepared),
        ("body", body),
        ("exposed", exposed),
        ("unknown final", uncertain()),
        ("unknown exposed", unknown_exposed),
    ];
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for state in [
            RecipientState::Accepted,
            RecipientState::Canceled,
            RecipientState::Failed,
            RecipientState::OutcomeUnknown,
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let sub = SubmissionRow {
                completed_at: Some(1),
                notification: if matches!(
                    state,
                    RecipientState::Failed | RecipientState::OutcomeUnknown
                ) {
                    NotificationState::Pending
                } else {
                    NotificationState::None
                },
                ..submission(1)
            };
            // The core permits Pending here; the service must store its notice first.
            create_group(&store, sub, &[(0, terminal(state))], encoded).unwrap();
            let key = recipient_key(0);
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[
                        Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap(),
                        Operation::delete(Table::Recipients, &key).unwrap(),
                    ],
                    encoded
                ),
                Ok(Sequence::from_u64(2))
            );
        }
        for &(name, original) in cases {
            for intermediate in [
                Intermediate::Direct,
                Intermediate::Completed,
                Intermediate::DeleteCompleted,
                Intermediate::CompletedGroup,
            ] {
                for parent_first in [false, true] {
                    let fixture = Fixture::new();
                    let mut root = fixture.locked();
                    let store = open(&mut root);
                    create_group(&store, submission(1), &[(0, original)], encoded).unwrap();
                    let key = recipient_key(0);
                    let parent =
                        Operation::delete(Table::Submissions, SUBMISSION.as_bytes()).unwrap();
                    let child = Operation::delete(Table::Recipients, &key).unwrap();
                    let transient = encode(Row::Submission(SubmissionRow {
                        completed_at: Some(1),
                        ..submission(1)
                    }));
                    let completed =
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &transient)
                            .unwrap();
                    let unknown =
                        original.uncertain || original.phase == AttemptPhase::AcceptancePossible;
                    let ended = RecipientRow {
                        state: if unknown {
                            RecipientState::OutcomeUnknown
                        } else {
                            RecipientState::Failed
                        },
                        phase: if original.attempt_count == 0 {
                            AttemptPhase::None
                        } else {
                            AttemptPhase::Final
                        },
                        uncertain: unknown,
                        reason: FailureReason::Expired,
                        next_attempt_at: None,
                        ..original
                    };
                    let sub = SubmissionRow {
                        completed_at: Some(2),
                        notification: NotificationState::Pending,
                        ..submission(1)
                    };
                    let sub_bytes = encode(Row::Submission(sub));
                    let ended_bytes = encode(Row::Recipient(ended));
                    let mut operations = Vec::new();
                    if !parent_first && !matches!(intermediate, Intermediate::CompletedGroup) {
                        operations.push(child);
                    }
                    match intermediate {
                        Intermediate::Direct => {}
                        Intermediate::Completed => operations.push(completed),
                        Intermediate::DeleteCompleted => operations.extend([parent, completed]),
                        Intermediate::CompletedGroup => operations.extend([
                            Operation::put(Table::Recipients, &key, &ended_bytes).unwrap(),
                            Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes)
                                .unwrap(),
                        ]),
                    }
                    if !parent_first && matches!(intermediate, Intermediate::CompletedGroup) {
                        operations.push(child);
                    }
                    operations.push(parent);
                    if parent_first {
                        operations.push(child);
                    }
                    let result = apply(&store, 1, &operations, encoded);
                    if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                        unexpected.push((encoded, name, intermediate, parent_first, result));
                        continue;
                    }
                    assert_submission(&store, submission(1), 1);
                    assert_recipient(&store, original);
                    assert_eq!(
                        apply(
                            &store,
                            1,
                            &[
                                Operation::put(
                                    Table::Submissions,
                                    SUBMISSION.as_bytes(),
                                    &sub_bytes
                                )
                                .unwrap(),
                                Operation::put(Table::Recipients, &key, &ended_bytes).unwrap(),
                            ],
                            encoded
                        ),
                        Ok(Sequence::from_u64(2))
                    );
                    assert_submission(&store, sub, 2);
                    assert_recipient(&store, ended);
                    assert_eq!(
                        apply(&store, 2, &[parent, child], encoded),
                        Ok(Sequence::from_u64(3))
                    );
                    let mut view = store.view(ACCOUNT, deadline()).unwrap();
                    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(3));
                    let mut scratch = [0; 1024];
                    assert!(view
                        .get(Key::Submission(SUBMISSION), &mut scratch)
                        .unwrap()
                        .is_none());
                    assert!(view
                        .get(Key::Recipient(SUBMISSION, 0), &mut scratch)
                        .unwrap()
                        .is_none());
                }
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "pending responsibility deleted: {unexpected:?}"
    );
}
