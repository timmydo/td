use super::*;

fn pending_with_replies(state: RecipientState) -> RecipientRow<'static> {
    let mut row = pending_attempt(state);
    row.diagnostic = "previous attempt diagnostic";
    if state != RecipientState::Queued {
        row.rcpt_reply = Some("250 previous recipient reply");
        row.data_reply = Some("450 previous DATA reply");
    }
    row
}

#[test]
fn inactive_replies_survive_pending_updates_and_new_prepared_attempts() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for (state, has_replies) in [
            (RecipientState::Queued, false),
            (RecipientState::RetryWait, true),
            (RecipientState::OutcomeUnknown, true),
            (RecipientState::RetryWait, false),
            (RecipientState::OutcomeUnknown, false),
        ] {
            for dispatch in [false, true] {
                for field in 0..4 {
                    if (state == RecipientState::Queued && !dispatch)
                        || (!has_replies && matches!(field, 0 | 2))
                    {
                        continue;
                    }
                    let fixture = Fixture::new();
                    let mut root = fixture.locked();
                    let store = open(&mut root);
                    let mut original = pending_with_replies(state);
                    if !has_replies {
                        original.rcpt_reply = None;
                        original.data_reply = None;
                    }
                    create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                    let mut next = if dispatch {
                        next_attempt(original)
                    } else {
                        original
                    };
                    match field {
                        0 => next.rcpt_reply = None,
                        1 => next.rcpt_reply = Some("250 replacement recipient reply"),
                        2 => next.data_reply = None,
                        _ => next.data_reply = Some("450 replacement DATA reply"),
                    }
                    assert_fresh_group(submission(1), &[(0, next)], encoded);
                    let key = recipient_key(0);
                    let bytes = encode(Row::Recipient(next));
                    let result = apply(
                        &store,
                        1,
                        &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                        encoded,
                    );
                    if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                        unexpected.push((encoded, state, has_replies, dispatch, field, result));
                        continue;
                    }
                    assert_recipient(&store, original);
                    let retained = if dispatch {
                        next_attempt(original)
                    } else {
                        RecipientRow {
                            next_attempt_at: Some(3),
                            diagnostic: "pending schedule updated",
                            ..original
                        }
                    };
                    let bytes = encode(Row::Recipient(retained));
                    assert_eq!(
                        apply(
                            &store,
                            1,
                            &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                            encoded,
                        ),
                        Ok(Sequence::from_u64(2))
                    );
                    assert_recipient(&store, retained);
                }
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "inactive reply rewrites: {unexpected:?}"
    );
}

#[test]
fn dispatch_clears_diagnostics_before_an_active_attempt_can_change_replies() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for state in [
            RecipientState::Queued,
            RecipientState::RetryWait,
            RecipientState::OutcomeUnknown,
        ] {
            for diagnostic in ["previous attempt diagnostic", "new local diagnostic"] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = pending_with_replies(state);
                create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                let prepared = next_attempt(original);
                let invalid = RecipientRow {
                    diagnostic,
                    ..prepared
                };
                assert_fresh_group(submission(1), &[(0, invalid)], encoded);
                let key = recipient_key(0);
                let bytes = encode(Row::Recipient(invalid));
                let result = apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, state, diagnostic, result));
                    continue;
                }
                assert_recipient(&store, original);
                let bytes = encode(Row::Recipient(prepared));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(2))
                );
                assert_recipient(&store, prepared);
                let body = RecipientRow {
                    phase: AttemptPhase::Body,
                    rcpt_reply: Some("250 current recipient reply"),
                    data_reply: None,
                    ..prepared
                };
                let bytes = encode(Row::Recipient(body));
                assert_eq!(
                    apply(
                        &store,
                        2,
                        &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(3))
                );
                assert_recipient(&store, body);
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "dispatch diagnostic retained: {unexpected:?}"
    );
}

#[test]
fn intermediate_prepared_or_delete_cannot_authorize_reply_changes() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for deleted in [false, true] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let original = pending_with_replies(RecipientState::RetryWait);
            create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
            let prepared = next_attempt(original);
            let changed = RecipientRow {
                rcpt_reply: Some("250 current recipient reply"),
                data_reply: None,
                ..prepared
            };
            assert_fresh_group(submission(1), &[(0, changed)], encoded);
            let key = recipient_key(0);
            let prepared_bytes = encode(Row::Recipient(prepared));
            let changed_bytes = encode(Row::Recipient(changed));
            let prepare = Operation::put(Table::Recipients, &key, &prepared_bytes).unwrap();
            let change = Operation::put(Table::Recipients, &key, &changed_bytes).unwrap();
            let delete = Operation::delete(Table::Recipients, &key).unwrap();
            let first = if deleted { delete } else { prepare };
            let result = apply(&store, 1, &[first, change], encoded);
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                unexpected.push((encoded, deleted, result));
                continue;
            }
            assert_recipient(&store, original);
            assert_eq!(
                apply(&store, 1, &[change, delete, prepare], encoded),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, prepared);
            assert_eq!(
                apply(&store, 2, &[change], encoded),
                Ok(Sequence::from_u64(3))
            );
            assert_recipient(&store, changed);
        }
    }
    assert!(
        unexpected.is_empty(),
        "intermediate authority: {unexpected:?}"
    );
}

#[test]
fn pending_expiry_and_cancellation_keep_replies_without_a_new_attempt() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for variant in 0..4 {
            let mutation_count = if variant == 3 { 1 } else { 2 };
            for mutation in 0..mutation_count {
                let state = if variant == 1 {
                    RecipientState::OutcomeUnknown
                } else {
                    RecipientState::RetryWait
                };
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = pending_with_replies(state);
                create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                let canceled = variant == 2;
                let retained = RecipientRow {
                    state: if canceled {
                        RecipientState::Canceled
                    } else if original.uncertain {
                        RecipientState::OutcomeUnknown
                    } else {
                        RecipientState::Failed
                    },
                    reason: if canceled {
                        FailureReason::Canceled
                    } else {
                        FailureReason::Expired
                    },
                    next_attempt_at: None,
                    ..original
                };
                let mut invalid = retained;
                if variant == 3 {
                    invalid.reason = FailureReason::SmtpPermanent;
                    invalid.data_reply = Some("550 fabricated permanent refusal");
                } else if mutation == 0 {
                    invalid.rcpt_reply = None;
                    invalid.data_reply = None;
                } else {
                    invalid.data_reply = Some("450 replacement DATA reply");
                }
                let mut sub = submission(1);
                sub.completed_at = Some(2);
                if !canceled {
                    sub.notification = NotificationState::Pending;
                }
                assert_fresh_group(sub, &[(0, invalid)], encoded);
                let key = recipient_key(0);
                let invalid_bytes = encode(Row::Recipient(invalid));
                let sub_bytes = encode(Row::Submission(sub));
                let sub_op =
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap();
                let result = apply(
                    &store,
                    1,
                    &[
                        Operation::put(Table::Recipients, &key, &invalid_bytes).unwrap(),
                        sub_op,
                    ],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, variant, mutation, result));
                    continue;
                }
                assert_recipient(&store, original);
                {
                    let mut view = store.view(ACCOUNT, deadline()).unwrap();
                    let mut scratch = [0; 65536];
                    assert_eq!(
                        view.get(Key::Submission(SUBMISSION), &mut scratch)
                            .unwrap()
                            .unwrap()
                            .0,
                        Row::Submission(submission(1))
                    );
                }
                let bytes = encode(Row::Recipient(retained));
                assert_eq!(
                    apply(
                        &store,
                        1,
                        &[
                            Operation::put(Table::Recipients, &key, &bytes).unwrap(),
                            sub_op,
                        ],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(2))
                );
                assert_recipient(&store, retained);
                let mut view = store.view(ACCOUNT, deadline()).unwrap();
                let mut scratch = [0; 65536];
                assert_eq!(
                    view.get(Key::Submission(SUBMISSION), &mut scratch)
                        .unwrap()
                        .unwrap()
                        .0,
                    Row::Submission(sub)
                );
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "terminal reply rewrites: {unexpected:?}"
    );
}
