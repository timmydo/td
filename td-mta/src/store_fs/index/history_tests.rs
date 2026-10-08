use super::*;
fn apply(
    store: &IndexStore<'_>,
    sequence: u64,
    operations: &[Operation<'_>],
    encoded: bool,
) -> Result<Sequence, CommitError> {
    if encoded {
        commit_encoded(store, request(sequence), operations, &mut [])
    } else {
        store.commit(&td_crypto::Provider, request(sequence), operations, &mut [])
    }
}
fn uncertain() -> RecipientRow<'static> {
    RecipientRow {
        state: RecipientState::OutcomeUnknown,
        uncertain: true,
        attempt: Some(AttemptId::from_bytes([8; 16])),
        attempt_count: 3,
        last_attempt_at: Some(1),
        phase: AttemptPhase::Final,
        next_attempt_at: Some(2),
        reason: FailureReason::Network,
        ..queued()
    }
}
#[test]
fn recipient_history_survives_typed_and_encoded_updates() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create(&store, 1, &[0]).unwrap();
        let key = recipient_key(0);
        let initial = uncertain();
        let bytes = encode(Row::Recipient(initial));
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
        create(&store, 1, &[0]).unwrap();
        let key = recipient_key(0);
        let initial = uncertain();
        let original = encode(Row::Recipient(initial));
        let good = Operation::put(Table::Recipients, &key, &original).unwrap();
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
            kind: BlobKind::Message,
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
            kind: BlobKind::Message,
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
        create(&store, 1, &[0]).unwrap();
        let key = recipient_key(0);
        let mut row = uncertain();
        row.uncertain = false;
        row.state = RecipientState::RetryWait;
        let bytes = encode(Row::Recipient(row));
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
        kind: BlobKind::Message,
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
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create(&store, 1, &[0]).unwrap();
        let mut recipient = queued();
        recipient.state = RecipientState::Canceled;
        recipient.reason = FailureReason::Canceled;
        recipient.next_attempt_at = None;
        let mut sub = submission(1);
        sub.completed_at = Some(1);
        let key = recipient_key(0);
        let recipient_bytes = encode(Row::Recipient(recipient));
        let sub_bytes = encode(Row::Submission(sub));
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
        // Creation authority is outside this core's retained-history guard.
        sub.notification = NotificationState::Pending;
        let pending = encode(Row::Submission(sub));
        apply(
            &store,
            2,
            &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &pending).unwrap()],
            encoded,
        )
        .unwrap();
        let email = EmailId::from_bytes([14; 16]);
        sub.notification = NotificationState::Stored;
        sub.notification_email = Some(email);
        let stored = encode(Row::Submission(sub));
        apply(
            &store,
            3,
            &[Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &stored).unwrap()],
            encoded,
        )
        .unwrap();
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 65536];
        assert!(view.get(Key::Email(email), &mut scratch).unwrap().is_none());
    }
}
