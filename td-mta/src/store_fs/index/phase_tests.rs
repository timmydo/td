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
