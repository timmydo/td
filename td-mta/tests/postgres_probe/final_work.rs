use super::{deadline, sql, Fixture};
use std::{thread, time::Duration};
use td_mta::{
    admission::work::{Charge, Meter},
    ids::AttemptId,
    ports::{self, Clock, Deadline},
    postgres_attempt_phases as phases,
    postgres_dispatch::{self as dispatch, Error},
    postgres_read::ReadSlots,
    postgres_route_owners::{self as owners, Claim},
};

pub(super) fn exercise(
    fixture: &Fixture,
    worker: &dispatch::Dispatches<'_, '_>,
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
) {
    let clock = &worker.connection.clock;
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let meter = || {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: 16 * 1024 * 1024,
                records: 100_000,
                ..Default::default()
            },
        )
    };
    let final_change = phases::accepted(claim, generation, attempt, &[0], "250 queued").unwrap();
    let possible = phases::acceptance_possible(claim, generation, attempt, &[0]).unwrap();
    let mut budget = meter();
    let preparation = td_mta::postgres_probe::Request {
        address: worker.connection.request.address,
        server_name: worker.connection.request.server_name,
        user: worker.connection.request.user,
        database: worker.connection.request.database,
        password: worker.connection.request.password,
        mode: td_mta::postgres_probe::Mode::Preparation,
    };
    let forbidden = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &preparation,
            trust: worker.connection.trust,
            slots: worker.connection.slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: worker.pending,
    };
    assert!(matches!(
        forbidden.reserve_final(attempt, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Forbidden))
    ));
    let before = budget.remaining();
    let held = worker
        .reserve_final(attempt, until(10_000), &mut budget)
        .unwrap();
    assert_eq!(
        before.io_bytes - budget.remaining().io_bytes,
        3 * 1024 * 1024 + 128 * 1024
    );
    assert_eq!(before.records - budget.remaining().records, 4096);
    assert!(matches!(
        worker.reserve_final(attempt, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        worker.publish_phase(possible.clone(), until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(held);
    let held = worker
        .reserve_final(attempt, until(10_000), &mut meter())
        .unwrap();
    assert_eq!(
        held.publish(possible.clone()),
        Err(Error::Admission(ports::Error::Invalid))
    );
    let held = worker
        .reserve_final(
            AttemptId::from_bytes([235; 16]),
            until(10_000),
            &mut meter(),
        )
        .unwrap();
    assert_eq!(
        held.publish(final_change.clone()),
        Err(Error::Admission(ports::Error::Invalid))
    );
    let held = worker
        .reserve_final(attempt, until(1000), &mut meter())
        .unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        held.publish(final_change.clone()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert!(matches!(
        worker.reserve_final(attempt, until(10_000), &mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    ));
    assert!(matches!(
        worker.reserve_final(attempt, until(0), &mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    ));

    owners::renew(
        worker.connection.request,
        worker.connection.trust,
        deadline(),
        claim,
        generation,
    )
    .unwrap();
    let slots = ReadSlots::new(2).unwrap();
    let roomy = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: worker.connection.request,
            trust: worker.connection.trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: worker.pending,
    };
    let expired = roomy
        .reserve_final_result(claim, generation, attempt, &[0], until(1000), &mut meter())
        .unwrap();
    thread::sleep(Duration::from_millis(1100));
    assert!(matches!(
        expired.publish_possible(&mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    ));
    let dropped = roomy
        .reserve_final_result(
            claim,
            generation,
            attempt,
            &[0],
            until(10_000),
            &mut meter(),
        )
        .unwrap();
    drop(dropped);
    let mut final_budget = meter();
    let plan = roomy
        .reserve_final_result(
            claim,
            generation,
            attempt,
            &[0],
            until(10_000),
            &mut final_budget,
        )
        .unwrap();
    let remaining = final_budget.remaining();
    final_budget
        .charge(clock.sample().unwrap().monotonic, remaining)
        .unwrap();
    let held = plan.publish_possible(&mut meter()).unwrap();
    assert_eq!(held.attempt(), attempt);
    assert!(held.check_current(&mut meter()).unwrap() > std::time::Instant::now());
    let newly_possible = *held.outcome();
    let duplicate = roomy
        .reserve_final_result(
            claim,
            generation,
            attempt,
            &[0],
            until(10_000),
            &mut meter(),
        )
        .unwrap();
    // Both ordinary slots are now held; refusal releases the duplicate's slot.
    assert!(matches!(
        duplicate.publish_possible(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        roomy.publish_new_possible(possible.clone(), until(10_000), &mut meter()),
        Err(Error::Phase(phases::Error::Conflict))
    );
    assert_eq!(
        roomy
            .publish_phase(possible, until(10_000), &mut meter())
            .unwrap(),
        newly_possible
    );

    // Occupy the remaining slot: consumption must use the one already held.
    let other = roomy
        .reserve_final(attempt, until(10_000), &mut meter())
        .unwrap();
    assert_eq!(
        held.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    sql(fixture,"CREATE FUNCTION public.reject_retained_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_retained_final BEFORE INSERT ON td_mta.attempt_acceptance_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_retained_final()");
    let failed = held.record("250 queued").unwrap_err();
    assert_eq!(failed.error, Error::Phase(phases::Error::Database));
    assert_eq!(failed.attempt(), Some(attempt));
    assert!(matches!(
        roomy.reserve_final(attempt, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    sql(
        fixture,
        "DROP TRIGGER reject_retained_final ON td_mta.attempt_acceptance_receipts",
    );
    let mut empty = Meter::new(until(10_000), Charge::default());
    let failed = failed.retry(&mut empty).unwrap_err();
    assert_eq!(failed.error, Error::Admission(ports::Error::Capacity));
    assert_eq!(failed.attempt(), Some(attempt));
    let outcome = failed.retry(&mut meter()).unwrap();
    assert_eq!(
        phases::inspect(
            worker.connection.request,
            worker.connection.trust,
            deadline(),
            &final_change
        )
        .unwrap(),
        Some(outcome)
    );
    assert_eq!(outcome.attempt, attempt);
    assert_eq!(outcome.recipients, 1);
    assert_eq!(worker.pending.attempt().unwrap(), None);
    drop(other);
    let duplicate = roomy
        .reserve_final_result(
            claim,
            generation,
            attempt,
            &[0],
            until(10_000),
            &mut meter(),
        )
        .unwrap();
    assert!(matches!(
        duplicate.publish_possible(&mut meter()),
        Err(Error::Phase(phases::Error::Conflict))
    ));
    assert!(roomy
        .reserve_final(attempt, until(10_000), &mut meter())
        .is_ok());
}
