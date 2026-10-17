use super::{deadline, sql, Fixture, PASSWORD};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use td_crypto::TrustStore;
use td_mta::postgres_dispatch::Identity;
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{self, Clock, Deadline, Time},
    postgres_attempt_phases as phases, postgres_attempt_recovery as recovery,
    postgres_dispatch::{self as dispatch, Error},
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_route_owners::{self as owners, Claim},
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
    let submission = SubmissionId::from_bytes([102; 16]);
    let sub = format!("decode('{submission}','hex')");
    let src = format!("decode('{source}','hex')");
    sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,2,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},n,n::text||'@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,1) n WHERE id={sub}"));
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
    let preparation = Request {
        mode: Mode::Preparation,
        ..runtime
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
        guard: guard.clone(),
        pending: &pending,
    };
    let forbidden = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &preparation,
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
    let capacity_pending = dispatch::Pending::default();
    let ordinary_capacity = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &runtime,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: &capacity_pending,
    };
    let settlement_capacity = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &runtime,
            trust,
            slots: &settlement,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: &capacity_pending,
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
    let input = || dispatch::Input {
        submission,
        retry: false,
        deadline: until(10_000),
    };
    for delayed in [false, true] {
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id={sub}; UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id={sub}"));
        let published = worker
            .publish_prepared(claim, generation, input(), &mut meter())
            .unwrap();
        let attempt = published.attempt();
        drop(published);
        let value =
            recovery::interrupted_with_rcpt(claim, generation, attempt, &[(0, "250 retained")])
                .unwrap();
        assert_eq!(
            forbidden.publish_recovery(value.clone(), until(10_000), &mut meter()),
            Err(Error::Admission(ports::Error::Forbidden))
        );
        assert_eq!(
            worker.publish_recovery(value.clone(), until(0), &mut meter()),
            Err(Error::Admission(ports::Error::Deadline))
        );
        let mut empty = Meter::new(until(10_000), Charge::default());
        assert_eq!(
            worker.publish_recovery(value.clone(), until(10_000), &mut empty),
            Err(Error::Admission(ports::Error::Capacity))
        );
        let occupied = worker
            .reserve_final(attempt, until(10_000), &mut meter())
            .unwrap();
        assert_eq!(
            worker.publish_recovery(value.clone(), until(10_000), &mut meter()),
            Err(Error::Admission(ports::Error::Busy))
        );
        drop(occupied);
        assert_eq!(pending.identity().unwrap(), None);
        let outcome = if delayed {
            sql(fixture,"CREATE FUNCTION public.delay_recovery_worker() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_recovery_worker AFTER INSERT ON td_mta.attempt_recovery_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_recovery_worker()");
            assert_eq!(
                worker.publish_recovery(value.clone(), until(1500), &mut meter()),
                Err(Error::Recovery(recovery::Error::Indeterminate))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            assert_eq!(
                worker.publish_recovery(value.clone(), until(10_000), &mut meter()),
                Err(Error::Admission(ports::Error::Busy))
            );
            assert_eq!(
                worker
                    .publish_prepared(claim, generation, input(), &mut meter())
                    .map(|_| ()),
                Err(Error::Admission(ports::Error::Busy))
            );
            let body = phases::body(claim, generation, attempt, &[(0, "250 retained")]).unwrap();
            assert_eq!(
                worker.publish_phase(body, until(10_000), &mut meter()),
                Err(Error::Admission(ports::Error::Busy))
            );
            assert_eq!(
                worker
                    .reserve_final(attempt, until(10_000), &mut meter())
                    .map(|_| ()),
                Err(Error::Admission(ports::Error::Busy))
            );
            thread::sleep(Duration::from_secs(4));
            sql(
                fixture,
                "DROP TRIGGER delay_recovery_worker ON td_mta.attempt_recovery_receipts",
            );
            assert_eq!(
                pending.settle(&settle, until(0), &mut meter()),
                Err(Error::Admission(ports::Error::Deadline))
            );
            let mut empty = Meter::new(until(10_000), Charge::default());
            assert_eq!(
                pending.settle(&settle, until(10_000), &mut empty),
                Err(Error::Admission(ports::Error::Capacity))
            );
            let occupied = settlement_capacity
                .reserve_final(attempt, until(10_000), &mut meter())
                .unwrap();
            assert_eq!(
                pending.settle(&settle, until(10_000), &mut meter()),
                Err(Error::Admission(ports::Error::Busy))
            );
            drop(occupied);
            let operation = value.operation();
            sql(fixture,&format!("CREATE TABLE public.recovery_worker_receipt AS SELECT * FROM td_mta.attempt_recovery_receipts WHERE operation=decode('{operation}','hex'); UPDATE td_mta.attempt_recovery_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation=decode('{operation}','hex')"));
            assert_eq!(
                pending.settle(&settle, until(10_000), &mut meter()),
                Err(Error::Recovery(recovery::Error::Conflict))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(fixture, &format!("DELETE FROM td_mta.attempt_recovery_receipts WHERE operation=decode('{operation}','hex')"));
            assert_eq!(
                pending
                    .settle(&settle, until(10_000), &mut meter())
                    .unwrap(),
                None
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(fixture,"INSERT INTO td_mta.attempt_recovery_receipts SELECT * FROM public.recovery_worker_receipt; DROP TABLE public.recovery_worker_receipt");
            let before = sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts",
            );
            let ordinary = ordinary_capacity
                .reserve_final(attempt, until(10_000), &mut meter())
                .unwrap();
            let dispatch::Settlement::Recovery(outcome) = pending
                .settle(&settle, until(10_000), &mut meter())
                .unwrap()
                .unwrap()
            else {
                panic!("wrong recovery settlement")
            };
            drop(ordinary);
            assert_eq!(
                sql(
                    fixture,
                    "SELECT encode(sequence,'hex') FROM td_mta.accounts"
                ),
                before
            );
            outcome
        } else {
            sql(fixture,"CREATE FUNCTION public.reject_recovery_worker() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_recovery_worker BEFORE INSERT ON td_mta.attempt_recovery_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_recovery_worker()");
            assert_eq!(
                worker.publish_recovery(value.clone(), until(10_000), &mut meter()),
                Err(Error::Recovery(recovery::Error::Database))
            );
            assert_eq!(pending.identity().unwrap(), None);
            sql(
                fixture,
                "DROP TRIGGER reject_recovery_worker ON td_mta.attempt_recovery_receipts",
            );
            let mut budget = meter();
            let before = budget.remaining();
            let outcome = worker
                .publish_recovery(value.clone(), until(10_000), &mut budget)
                .unwrap();
            assert_eq!(
                before.io_bytes - budget.remaining().io_bytes,
                3 * 1024 * 1024 + 128 * 1024
            );
            assert_eq!(before.records - budget.remaining().records, 4096);
            outcome
        };
        assert_eq!(outcome.attempt, attempt);
        assert_eq!(outcome.recipients, 2);
        assert_eq!(pending.identity().unwrap(), None);
        assert_eq!(
            worker
                .publish_recovery(value, until(10_000), &mut meter())
                .unwrap(),
            outcome
        );
        assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")),"3|250 retained|\n3||");
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")),"f");
    }
    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
}
