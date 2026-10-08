use super::*;

fn in_flight(phase: AttemptPhase) -> RecipientRow<'static> {
    RecipientRow {
        state: RecipientState::InFlight,
        attempt: Some(AttemptId::from_bytes([8; 16])),
        attempt_count: 3,
        last_attempt_at: Some(1),
        phase,
        next_attempt_at: None,
        rcpt_reply: Some("250 recipient accepted"),
        ..queued()
    }
}

fn assert_recipient(store: &IndexStore<'_>, expected: RecipientRow<'_>) {
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    let mut scratch = [0; 65536];
    assert_eq!(
        view.get(Key::Recipient(SUBMISSION, 0), &mut scratch)
            .unwrap()
            .unwrap()
            .0,
        Row::Recipient(expected)
    );
}

#[test]
fn active_attempt_cannot_skip_or_regress_a_durable_phase() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    let mut accepted = Vec::new();
    for encoded in [false, true] {
        for (old, next) in [
            (Prepared, AcceptancePossible),
            (Body, Prepared),
            (AcceptancePossible, Prepared),
            (AcceptancePossible, Body),
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let original = in_flight(old);
            create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
            let next = in_flight(next);
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
                accepted.push((encoded, old, next.phase, result));
                continue;
            }
            assert_recipient(&store, original);
            let mut retained = original;
            retained.diagnostic = "same attempt remains active";
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
    assert!(accepted.is_empty(), "invalid phase commits: {accepted:?}");
}

#[test]
fn active_attempt_allows_phase_stays_and_adjacent_advances() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    for encoded in [false, true] {
        for (old, next) in [
            (Prepared, Prepared),
            (Prepared, Body),
            (Body, Body),
            (Body, AcceptancePossible),
            (AcceptancePossible, AcceptancePossible),
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create_with_recipient(&store, 1, &[0], in_flight(old), encoded).unwrap();
            let next = in_flight(next);
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                ),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, next);
        }
    }
}

#[test]
fn active_attempt_can_exit_each_phase_to_a_pending_outcome() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    for encoded in [false, true] {
        for phase in [Prepared, Body, AcceptancePossible] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let original = in_flight(phase);
            create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
            let uncertain = phase == AcceptancePossible;
            let next = RecipientRow {
                state: if uncertain {
                    RecipientState::OutcomeUnknown
                } else {
                    RecipientState::RetryWait
                },
                phase: AttemptPhase::Final,
                uncertain,
                reason: if uncertain {
                    FailureReason::Uncertain
                } else {
                    FailureReason::Network
                },
                next_attempt_at: Some(2),
                ..original
            };
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                ),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, next);
        }
    }
}

#[test]
fn repeated_keys_cannot_combine_body_and_acceptance_possible() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        create_with_recipient(&store, 1, &[0], in_flight(Prepared), encoded).unwrap();
        assert_fresh_group(
            submission(1),
            &[(0, in_flight(AcceptancePossible))],
            encoded,
        );
        let key = recipient_key(0);
        let body = encode(Row::Recipient(in_flight(Body)));
        let possible = encode(Row::Recipient(in_flight(AcceptancePossible)));
        let body = Operation::put(Table::Recipients, &key, &body).unwrap();
        let possible = Operation::put(Table::Recipients, &key, &possible).unwrap();
        let delete = Operation::delete(Table::Recipients, &key).unwrap();
        rejected(apply(&store, 1, &[body, possible], encoded));
        rejected(apply(&store, 1, &[delete, possible], encoded));
        assert_recipient(&store, in_flight(Prepared));
        assert_eq!(
            apply(&store, 1, &[possible, delete, body], encoded),
            Ok(Sequence::from_u64(2))
        );
        assert_recipient(&store, in_flight(Body));
        assert_eq!(
            apply(&store, 2, &[possible], encoded),
            Ok(Sequence::from_u64(3))
        );
        assert_recipient(&store, in_flight(AcceptancePossible));
    }
}

fn accepted(original: RecipientRow<'_>) -> RecipientRow<'_> {
    RecipientRow {
        state: RecipientState::Accepted,
        phase: AttemptPhase::Final,
        next_attempt_at: None,
        reason: FailureReason::None,
        rcpt_reply: Some("250 recipient accepted"),
        data_reply: Some("250 delivered"),
        ..original
    }
}

#[test]
fn acceptance_requires_a_committed_acceptance_possible_attempt() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for variant in 0..5 {
            let mut original = in_flight(AttemptPhase::Prepared);
            match variant {
                0 => {}
                1 => original.phase = AttemptPhase::Body,
                2..=4 => {
                    original.state = if variant == 2 {
                        RecipientState::RetryWait
                    } else {
                        RecipientState::OutcomeUnknown
                    };
                    original.phase = AttemptPhase::Final;
                    original.uncertain = variant != 2;
                    original.reason = FailureReason::Network;
                    original.next_attempt_at = Some(2);
                    if variant == 4 {
                        original.phase = AttemptPhase::AcceptancePossible;
                        original.reason = FailureReason::Uncertain;
                    }
                }
                _ => panic!("unknown fixture"),
            }
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create_group(
                &store,
                submission(2),
                &[(0, original), (1, queued())],
                encoded,
            )
            .unwrap();
            let next = accepted(original);
            assert_fresh_group(submission(2), &[(0, next), (1, queued())], encoded);
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            let result = apply(
                &store,
                1,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded,
            );
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                unexpected.push((encoded, variant, result));
                continue;
            }
            assert_recipient(&store, original);
            let mut retained = original;
            retained.diagnostic = "acceptance was not recorded";
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
    assert!(unexpected.is_empty(), "acceptance bypasses: {unexpected:?}");
}

#[test]
fn acceptance_possible_can_record_acceptance_with_retained_uncertainty() {
    for encoded in [false, true] {
        for uncertain in [false, true] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let mut original = in_flight(AttemptPhase::AcceptancePossible);
            original.uncertain = uncertain;
            create_group(
                &store,
                submission(2),
                &[(0, original), (1, queued())],
                encoded,
            )
            .unwrap();
            let next = accepted(original);
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                ),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, next);
        }
    }
}

#[test]
fn cancellation_depends_on_the_original_active_phase() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for phase in [Prepared, Body, AcceptancePossible] {
            for variant in 0..3 {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = in_flight(phase);
                create_with_recipient(&store, 1, &[0], original, encoded).unwrap();
                let next = RecipientRow {
                    state: RecipientState::Canceled,
                    phase: AttemptPhase::Final,
                    reason: FailureReason::Canceled,
                    ..original
                };
                let mut sub = submission(1);
                sub.completed_at = Some(2);
                assert_fresh_group(sub, &[(0, next)], encoded);
                let key = recipient_key(0);
                let bytes = encode(Row::Recipient(next));
                let body = encode(Row::Recipient(in_flight(Body)));
                let sub_bytes = encode(Row::Submission(sub));
                let mut operations = Vec::new();
                match variant {
                    0 => {}
                    1 => operations.push(Operation::put(Table::Recipients, &key, &body).unwrap()),
                    2 => operations.push(Operation::delete(Table::Recipients, &key).unwrap()),
                    _ => panic!("unknown fixture"),
                }
                operations.extend([
                    Operation::put(Table::Recipients, &key, &bytes).unwrap(),
                    Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes).unwrap(),
                ]);
                let result = apply(&store, 1, &operations, encoded);
                if phase != AcceptancePossible {
                    assert_eq!(result, Ok(Sequence::from_u64(2)));
                    assert_recipient(&store, next);
                    continue;
                }
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, variant, result));
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
                let mut retained = original;
                retained.diagnostic = "attempt still may have delivered";
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
    assert!(
        unexpected.is_empty(),
        "exposed cancellations: {unexpected:?}"
    );
}

#[test]
fn acceptance_possible_and_accepted_require_separate_commits() {
    for encoded in [false, true] {
        let fixture = Fixture::new();
        let mut root = fixture.locked();
        let store = open(&mut root);
        let original = in_flight(AttemptPhase::Body);
        create_group(
            &store,
            submission(2),
            &[(0, original), (1, queued())],
            encoded,
        )
        .unwrap();
        let possible = in_flight(AttemptPhase::AcceptancePossible);
        let delivered = accepted(original);
        assert_fresh_group(submission(2), &[(0, delivered), (1, queued())], encoded);
        let key = recipient_key(0);
        let possible_bytes = encode(Row::Recipient(possible));
        let delivered_bytes = encode(Row::Recipient(delivered));
        let possible_op = Operation::put(Table::Recipients, &key, &possible_bytes).unwrap();
        let delivered_op = Operation::put(Table::Recipients, &key, &delivered_bytes).unwrap();
        let delete = Operation::delete(Table::Recipients, &key).unwrap();
        rejected(apply(&store, 1, &[possible_op, delivered_op], encoded));
        rejected(apply(&store, 1, &[delete, delivered_op], encoded));
        assert_recipient(&store, original);
        assert_eq!(
            apply(&store, 1, &[delivered_op, delete, possible_op], encoded),
            Ok(Sequence::from_u64(2))
        );
        assert_recipient(&store, possible);
        assert_eq!(
            apply(&store, 2, &[delivered_op], encoded),
            Ok(Sequence::from_u64(3))
        );
        assert_recipient(&store, delivered);
    }
}

#[test]
fn acceptance_possible_can_record_definitive_data_refusals() {
    for encoded in [false, true] {
        for (state, reason, reply, retry) in [
            (
                RecipientState::RetryWait,
                FailureReason::SmtpTemporary,
                "450 deferred",
                Some(2),
            ),
            (
                RecipientState::Failed,
                FailureReason::SmtpPermanent,
                "550 refused",
                None,
            ),
            (
                RecipientState::Failed,
                FailureReason::Expired,
                "450 deferred beyond expiry",
                None,
            ),
            (
                RecipientState::Failed,
                FailureReason::Expired,
                "550 refused at expiry",
                None,
            ),
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let original = in_flight(AttemptPhase::AcceptancePossible);
            create_group(
                &store,
                submission(2),
                &[(0, original), (1, queued())],
                encoded,
            )
            .unwrap();
            let next = RecipientRow {
                state,
                phase: AttemptPhase::Final,
                next_attempt_at: retry,
                reason,
                data_reply: Some(reply),
                ..original
            };
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                ),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, next);
            let canceled = RecipientRow {
                state: RecipientState::Canceled,
                next_attempt_at: None,
                reason: FailureReason::Canceled,
                ..next
            };
            let other = RecipientRow {
                state: RecipientState::Canceled,
                next_attempt_at: None,
                reason: FailureReason::Canceled,
                ..queued()
            };
            let mut sub = submission(2);
            sub.completed_at = Some(3);
            let first_bytes = encode(Row::Recipient(canceled));
            let second_bytes = encode(Row::Recipient(other));
            let second_key = recipient_key(1);
            let sub_bytes = encode(Row::Submission(sub));
            assert_eq!(
                apply(
                    &store,
                    2,
                    &[
                        Operation::put(Table::Recipients, &key, &first_bytes).unwrap(),
                        Operation::put(Table::Recipients, &second_key, &second_bytes).unwrap(),
                        Operation::put(Table::Submissions, SUBMISSION.as_bytes(), &sub_bytes)
                            .unwrap(),
                    ],
                    encoded,
                ),
                Ok(Sequence::from_u64(3))
            );
            assert_recipient(&store, canceled);
        }
    }
}

#[test]
fn exposed_attempt_cannot_end_certain_without_a_definitive_data_refusal() {
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for variant in 0..14 {
            let original = in_flight(AttemptPhase::AcceptancePossible);
            let mut next = RecipientRow {
                state: RecipientState::RetryWait,
                phase: AttemptPhase::Final,
                next_attempt_at: Some(2),
                reason: FailureReason::Network,
                ..original
            };
            match variant {
                0..=7 => {
                    next.reason = match variant / 2 {
                        0 => FailureReason::Network,
                        1 => FailureReason::Tls,
                        2 => FailureReason::Authentication,
                        _ => FailureReason::Protocol,
                    };
                    next.data_reply = (variant % 2 != 0).then_some("450 earlier refusal");
                }
                8..=11 => {
                    next.state = RecipientState::Failed;
                    next.next_attempt_at = None;
                    next.reason = FailureReason::Expired;
                    next.data_reply = match variant {
                        8 => None,
                        9 => Some("250 accepted"),
                        10 => Some("354 send body"),
                        _ => Some("450-invalid stored separator"),
                    };
                }
                12 => {
                    next.reason = FailureReason::SmtpTemporary;
                    next.rcpt_reply = Some("450 RCPT refusal");
                }
                _ => {
                    next.state = RecipientState::Failed;
                    next.next_attempt_at = None;
                    next.reason = FailureReason::SmtpPermanent;
                    next.rcpt_reply = Some("550 RCPT refusal");
                }
            }
            assert_fresh_group(submission(2), &[(0, next), (1, queued())], encoded);
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            create_group(
                &store,
                submission(2),
                &[(0, original), (1, queued())],
                encoded,
            )
            .unwrap();
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            let result = apply(
                &store,
                1,
                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                encoded,
            );
            if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                unexpected.push((encoded, variant, result));
                continue;
            }
            assert_recipient(&store, original);
            let mut retained = original;
            retained.diagnostic = "exposure remains unresolved";
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
    assert!(
        unexpected.is_empty(),
        "exposure was cleared: {unexpected:?}"
    );
}

#[test]
fn exposed_attempt_cannot_reuse_a_retained_data_refusal() {
    use AttemptPhase::{AcceptancePossible, Body, Prepared};
    let mut unexpected = Vec::new();
    for encoded in [false, true] {
        for chained in [false, true] {
            for (state, reason, reply, retry) in [
                (
                    RecipientState::RetryWait,
                    FailureReason::SmtpTemporary,
                    "450 retained from prior attempt",
                    Some(2),
                ),
                (
                    RecipientState::Failed,
                    FailureReason::SmtpPermanent,
                    "550 retained from prior attempt",
                    None,
                ),
                (
                    RecipientState::Failed,
                    FailureReason::Expired,
                    "450 retained from prior attempt",
                    None,
                ),
                (
                    RecipientState::Failed,
                    FailureReason::Expired,
                    "550 retained from prior attempt",
                    None,
                ),
            ] {
                let fixture = Fixture::new();
                let mut root = fixture.locked();
                let store = open(&mut root);
                let original = RecipientRow {
                    data_reply: Some(reply),
                    ..in_flight(AcceptancePossible)
                };
                let mut initial = original;
                if chained {
                    initial.phase = Prepared;
                }
                create_group(
                    &store,
                    submission(2),
                    &[(0, initial), (1, queued())],
                    encoded,
                )
                .unwrap();
                let key = recipient_key(0);
                let mut sequence = 1;
                if chained {
                    for phase in [Body, AcceptancePossible] {
                        let bytes = encode(Row::Recipient(RecipientRow { phase, ..original }));
                        assert_eq!(
                            apply(
                                &store,
                                sequence,
                                &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                                encoded,
                            ),
                            Ok(Sequence::from_u64(sequence + 1))
                        );
                        sequence += 1;
                    }
                }
                let next = RecipientRow {
                    state,
                    phase: AttemptPhase::Final,
                    next_attempt_at: retry,
                    reason,
                    ..original
                };
                assert_fresh_group(submission(2), &[(0, next), (1, queued())], encoded);
                let bytes = encode(Row::Recipient(next));
                let result = apply(
                    &store,
                    sequence,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                );
                if result != Err(CommitError::Rejected(ports::Error::Conflict)) {
                    unexpected.push((encoded, chained, reason, reply, result));
                    continue;
                }
                assert_recipient(&store, original);
                let mut retained = original;
                retained.diagnostic = "retained reply cannot resolve exposure";
                let bytes = encode(Row::Recipient(retained));
                assert_eq!(
                    apply(
                        &store,
                        sequence,
                        &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(sequence + 1))
                );
                assert_recipient(&store, retained);
                let unresolved = RecipientRow {
                    state: RecipientState::OutcomeUnknown,
                    phase: AttemptPhase::Final,
                    uncertain: true,
                    reason: FailureReason::Uncertain,
                    next_attempt_at: Some(3),
                    ..retained
                };
                let bytes = encode(Row::Recipient(unresolved));
                assert_eq!(
                    apply(
                        &store,
                        sequence + 1,
                        &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                        encoded,
                    ),
                    Ok(Sequence::from_u64(sequence + 2))
                );
                assert_recipient(&store, unresolved);
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "retained reply reused: {unexpected:?}"
    );
}

#[test]
fn exposed_attempt_can_record_uncertain_recovery_and_refusal_outcomes() {
    for encoded in [false, true] {
        for (reason, reply, retry, prior_uncertainty) in [
            (FailureReason::Network, None, Some(2), false),
            (FailureReason::Uncertain, None, Some(2), false),
            (FailureReason::Expired, None, None, false),
            (
                FailureReason::SmtpTemporary,
                Some("450 deferred"),
                Some(2),
                true,
            ),
            (
                FailureReason::SmtpPermanent,
                Some("550 refused"),
                None,
                true,
            ),
        ] {
            let fixture = Fixture::new();
            let mut root = fixture.locked();
            let store = open(&mut root);
            let original = RecipientRow {
                uncertain: prior_uncertainty,
                ..in_flight(AttemptPhase::AcceptancePossible)
            };
            create_group(
                &store,
                submission(2),
                &[(0, original), (1, queued())],
                encoded,
            )
            .unwrap();
            let next = RecipientRow {
                state: RecipientState::OutcomeUnknown,
                phase: AttemptPhase::Final,
                uncertain: true,
                next_attempt_at: retry,
                data_reply: reply,
                reason,
                ..original
            };
            let key = recipient_key(0);
            let bytes = encode(Row::Recipient(next));
            assert_eq!(
                apply(
                    &store,
                    1,
                    &[Operation::put(Table::Recipients, &key, &bytes).unwrap()],
                    encoded,
                ),
                Ok(Sequence::from_u64(2))
            );
            assert_recipient(&store, next);
        }
    }
}
