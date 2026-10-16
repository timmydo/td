use super::{deadline, sql, Fixture, PASSWORD};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use td_crypto::TrustStore;
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{self, Clock, Deadline, Time},
    postgres_attempt_phases as phases, postgres_attempts as attempts,
    postgres_dispatch::{self as dispatch, Error},
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_route_owners::{self as owners, Claim},
    postgres_submission_cancel as cancel,
    queue_clock::Guard,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
) {
    let submission = SubmissionId::from_bytes([94; 16]);
    let sub = format!("decode('{submission}','hex')");
    let src = format!("decode('{source}','hex')");
    sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,2,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},n,n::text||'@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,1) n WHERE id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let clock = Arc::new(RuntimeClock::new());
    let guard = Arc::new(Mutex::new(
        Guard::new(Time {
            utc_ms: sql(
                fixture,
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            )
            .parse()
            .unwrap(),
            monotonic: clock.sample().unwrap().monotonic,
        })
        .unwrap(),
    ));
    let runtime = Request {
        address: request.address,
        server_name: request.server_name,
        user: "mail_runtime",
        database: request.database,
        password: PASSWORD,
        mode: Mode::Runtime,
    };
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let pending = dispatch::Pending::default();
    let worker = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &runtime,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard,
        pending: &pending,
    };
    let settle = dispatch::Connection {
        request: &runtime,
        trust,
        slots: &settlement,
        clock: clock.clone(),
    };
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
    let input = |ms, retry| dispatch::Input {
        submission,
        retry,
        deadline: until(ms),
    };
    let preparation = Request {
        address: runtime.address,
        server_name: runtime.server_name,
        user: runtime.user,
        database: runtime.database,
        password: runtime.password,
        mode: Mode::Preparation,
    };
    let forbidden = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &preparation,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: &pending,
    };
    assert_eq!(
        forbidden
            .publish_prepared(claim, generation, input(10_000, false), &mut meter())
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(10_000, false), &mut empty)
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert!(empty.stopped().is_some());
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(0, false), &mut meter())
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut expired = Meter::new(
        until(0),
        Charge {
            io_bytes: u64::MAX,
            records: u64::MAX,
            ..Default::default()
        },
    );
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(10_000, false), &mut expired)
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(10_000, true), &mut meter())
            .map(|_| ()),
        Err(Error::Publication(attempts::Error::Clock(
            td_mta::queue_clock::Error::Unstable
        )))
    );
    assert_eq!(pending.attempt().unwrap(), None);
    let mut success = worker
        .publish_prepared(claim, generation, input(5000, false), &mut meter())
        .unwrap()
        .admit_envelope(&mut meter())
        .unwrap();
    assert_eq!(success.outcome().ordinals, vec![0, 1]);
    assert_eq!(success.attempt(), success.outcome().attempt);
    assert_eq!(success.envelope().recipients.len(), 2);
    assert_eq!(pending.attempt().unwrap(), None);
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        success.check_current(&mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    );
    let occupied = worker
        .reserve_final(success.attempt(), until(10_000), &mut meter())
        .unwrap();
    assert_eq!(
        success.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(occupied);
    let mut observed_budget = meter();
    let before = observed_budget.remaining();
    assert!(success.check_current(&mut observed_budget).is_ok());
    assert_eq!(
        before.io_bytes - observed_budget.remaining().io_bytes,
        2 * 1024 * 1024 + 192 * 1024
    );
    assert_eq!(before.records - observed_budget.remaining().records, 4096);
    sql(fixture, &format!("UPDATE td_mta.recipients SET address='changed@example.test' WHERE submission_id={sub} AND ordinal=0"));
    assert_eq!(
        success.check_current(&mut meter()),
        Err(Error::Publication(attempts::Error::Integrity))
    );
    assert_eq!(
        success.envelope().recipients.first().unwrap().address,
        "0@example.test"
    );
    sql(fixture, &format!("UPDATE td_mta.recipients SET address='0@example.test' WHERE submission_id={sub} AND ordinal=0"));
    let mut short = Meter::new(
        until(2000),
        Charge {
            io_bytes: 16 * 1024 * 1024,
            records: 100_000,
            ..Default::default()
        },
    );
    let short_limit = success.check_current(&mut short).unwrap();
    assert!(success.check_current(&mut meter()).unwrap() <= short_limit);

    let body = phases::body(
        claim,
        generation,
        success.outcome().attempt,
        &[(0, "250 first"), (1, "250 second")],
    )
    .unwrap();
    assert_eq!(
        forbidden.publish_phase(body.clone(), until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    assert_eq!(
        worker.publish_phase(body.clone(), until(0), &mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker.publish_phase(body.clone(), until(10_000), &mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert_eq!(pending.attempt().unwrap(), None);
    let stale = phases::body(
        claim,
        generation + 1,
        success.outcome().attempt,
        &[(0, "250 first"), (1, "250 second")],
    )
    .unwrap();
    assert_eq!(
        worker.publish_phase(stale, until(10_000), &mut meter()),
        Err(Error::Phase(phases::Error::Owner(owners::Error::Stale)))
    );
    assert_eq!(pending.attempt().unwrap(), None);
    let accepted = &[(0, "250 first"), (1, "250 second")];
    assert!(matches!(
        forbidden.publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(10_000),
            &mut meter()
        ),
        Err(Error::Admission(ports::Error::Forbidden))
    ));
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(0),
            &mut meter()
        ),
        Err(Error::Admission(ports::Error::Deadline))
    ));
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(10_000),
            &mut empty
        ),
        Err(Error::Admission(ports::Error::Capacity))
    ));
    let slot = worker
        .reserve_final(success.outcome().attempt, until(10_000), &mut meter())
        .unwrap();
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(10_000),
            &mut meter()
        ),
        Err(Error::Admission(ports::Error::Busy))
    ));
    drop(slot);
    let mut budget = meter();
    let before = budget.remaining();
    let owner = worker
        .publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(2000),
            &mut budget,
        )
        .unwrap();
    assert_eq!(
        before.io_bytes - budget.remaining().io_bytes,
        3 * 1024 * 1024 + 128 * 1024
    );
    assert_eq!(before.records - budget.remaining().records, 4096);
    assert_eq!(owner.attempt(), success.outcome().attempt);
    let published = *owner.outcome();
    let before = budget.remaining();
    assert!(owner.check_current(&mut budget).is_ok());
    assert_eq!(
        before.io_bytes - budget.remaining().io_bytes,
        1024 * 1024 + 128 * 1024
    );
    assert_eq!(before.records - budget.remaining().records, 4096);
    let body_io = 1024 * 1024 + 128 * 1024;
    let mut short = Meter::new(
        until(10_000),
        Charge {
            io_bytes: body_io - 1,
            records: 4096,
            ..Charge::default()
        },
    );
    assert_eq!(
        owner.check_current(&mut short),
        Err(Error::Admission(ports::Error::Capacity))
    );
    let mut exact = Meter::new(
        until(10_000),
        Charge {
            io_bytes: body_io,
            records: 4096,
            ..Charge::default()
        },
    );
    owner.check_current(&mut exact).unwrap();
    assert_eq!(exact.remaining().io_bytes, 0);
    assert_eq!(exact.remaining().records, 0);
    assert_eq!(
        owner.check_current(&mut exact),
        Err(Error::Admission(ports::Error::Capacity))
    );
    let slot = worker
        .reserve_final(success.outcome().attempt, until(10_000), &mut meter())
        .unwrap();
    assert_eq!(
        owner.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(slot);
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        owner.check_current(&mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            success.outcome().attempt,
            accepted,
            until(10_000),
            &mut meter()
        ),
        Err(Error::Phase(phases::Error::Conflict))
    ));
    let plan = owner.reserve_final(&mut meter()).unwrap();
    assert_eq!(plan.attempt(), success.outcome().attempt);
    assert_eq!(
        plan.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert!(matches!(
        worker.reserve_final(success.outcome().attempt, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    thread::sleep(Duration::from_millis(2100));
    assert_eq!(
        plan.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert!(matches!(
        plan.publish_possible(&mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    ));
    let released = worker
        .reserve_final(success.outcome().attempt, until(10_000), &mut meter())
        .unwrap();
    drop(released);
    assert_eq!(published.attempt, success.outcome().attempt);
    assert_eq!(published.recipients, 2);
    assert_eq!(pending.attempt().unwrap(), None);
    assert_eq!(
        worker
            .publish_phase(body, until(10_000), &mut meter())
            .unwrap(),
        published
    );
    cancel::publish(
        request,
        trust,
        deadline(),
        &cancel::prepare(claim, generation, submission).unwrap(),
        |_| Ok(()),
    )
    .unwrap();
    sql(fixture,&format!("UPDATE td_mta.submissions SET completed_at=NULL,notification=0 WHERE id={sub}; UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL,diagnostic='' WHERE submission_id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_dispatch_worker() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_dispatch_worker AFTER INSERT ON td_mta.submission_attempts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_dispatch_worker()");
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(1500, false), &mut meter())
            .map(|_| ()),
        Err(Error::Publication(attempts::Error::Indeterminate))
    );
    let attempt = pending.attempt().unwrap().unwrap();
    assert_eq!(
        success.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert!(matches!(
        worker.reserve_final(attempt, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        worker.publish_phase(
            phases::body(
                claim,
                generation,
                attempt,
                &[(0, "250 first"), (1, "250 second")]
            )
            .unwrap(),
            until(10_000),
            &mut meter()
        ),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(10_000, false), &mut meter())
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Busy))
    );
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_dispatch_worker ON td_mta.submission_attempts",
    );
    assert_eq!(
        pending.settle(&settle, until(0), &mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    // Absence is not proof that the original commit cannot still complete.
    sql(fixture,&format!("CREATE TABLE public.dispatch_worker_receipt AS SELECT * FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex'); DELETE FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')"));
    assert_eq!(
        pending
            .settle(&settle, until(10_000), &mut meter())
            .unwrap(),
        None
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    sql(fixture,"INSERT INTO td_mta.submission_attempts SELECT * FROM public.dispatch_worker_receipt; DROP TABLE public.dispatch_worker_receipt");
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let dispatch::Settlement::Dispatch(outcome) = pending
        .settle(&settle, until(10_000), &mut meter())
        .unwrap()
        .unwrap()
    else {
        panic!("wrong retained operation");
    };
    assert_eq!(outcome.attempt, attempt);
    assert_eq!(outcome.ordinals, vec![0, 1]);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    assert_eq!(pending.attempt().unwrap(), None);
    assert_eq!(
        pending
            .settle(&settle, until(10_000), &mut meter())
            .unwrap(),
        None
    );
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let body = phases::body(claim, generation, attempt, &[(0, "250 first")]).unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_phase_worker() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_phase_worker AFTER INSERT ON td_mta.attempt_phase_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_phase_worker()");
    assert_eq!(
        worker
            .publish_body(
                claim,
                generation,
                attempt,
                &[(0, "250 first")],
                until(1500),
                &mut meter()
            )
            .map(|_| ()),
        Err(Error::Phase(phases::Error::Indeterminate))
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            attempt,
            &[(0, "250 first")],
            until(10_000),
            &mut meter()
        ),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        worker.publish_phase(body, until(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert_eq!(
        worker
            .publish_prepared(claim, generation, input(10_000, false), &mut meter())
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Busy))
    );
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_phase_worker ON td_mta.attempt_phase_receipts",
    );
    assert_eq!(
        pending.settle(&settle, until(0), &mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut empty),
        Err(Error::Admission(ports::Error::Capacity))
    );
    sql(fixture,&format!("CREATE TABLE public.phase_worker_receipt AS SELECT * FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex'); DELETE FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex')"));
    assert_eq!(
        pending
            .settle(&settle, until(10_000), &mut meter())
            .unwrap(),
        None
    );
    assert_eq!(pending.attempt().unwrap(), Some(attempt));
    sql(fixture,"INSERT INTO td_mta.attempt_phase_receipts SELECT * FROM public.phase_worker_receipt; DROP TABLE public.phase_worker_receipt");
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let dispatch::Settlement::Phase(outcome) = pending
        .settle(&settle, until(10_000), &mut meter())
        .unwrap()
        .unwrap()
    else {
        panic!("wrong phase settlement");
    };
    assert_eq!(outcome.attempt, attempt);
    assert_eq!(outcome.recipients, 1);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    assert_eq!(pending.attempt().unwrap(), None);
    assert!(matches!(
        worker.publish_body(
            claim,
            generation,
            attempt,
            &[(0, "250 first")],
            until(10_000),
            &mut meter()
        ),
        Err(Error::Phase(phases::Error::Conflict))
    ));
    let historical = phases::body(claim, generation, attempt, &[(0, "250 first")]).unwrap();
    assert!(phases::check_current_body(
        &runtime,
        trust,
        deadline(),
        &historical,
        clock.clone(),
        worker.guard.clone()
    )
    .is_ok());
    // Existing delayed COMMIT cases exceed the original five-second owner
    // limit. A fresh meter cannot revive the first Prepared owner.
    assert_eq!(
        success.check_current(&mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert_eq!(
        success
            .publish_body(&[(0, "250 first")], &mut meter())
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    super::final_work::exercise(fixture, &worker, claim, generation, attempt);
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=1 AND state=2 AND phase=1")),"1");
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let recovery =
        td_mta::postgres_attempt_recovery::interrupted(claim, generation, attempt).unwrap();
    let recovered = worker
        .publish_recovery(recovery, until(10_000), &mut meter())
        .unwrap();
    assert_eq!(recovered.attempt, attempt);
    assert_eq!(recovered.recipients, 1);
    assert_eq!(sql(fixture, &format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
}
