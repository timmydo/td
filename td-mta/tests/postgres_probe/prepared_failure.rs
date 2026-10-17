use super::{deadline, relay_wire as wire, sql, Fixture, PASSWORD};
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
    ports::{self, Clock, Deadline, IoProgress, Time, Transport},
    postgres_attempt_recovery as recovery,
    postgres_dispatch::{self as dispatch, Error},
    postgres_probe::{Mode, Request},
    postgres_queue_candidates::{self as candidates, Kind},
    postgres_read::ReadSlots,
    postgres_route_health as health,
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
    policies: &wire::Policies,
) {
    for n in 159..=164 {
        let submission = SubmissionId::from_bytes([n; 16]);
        let sub = format!("decode('{submission}','hex')");
        let src = format!("decode('{source}','hex')");
        sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},0,'failed@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
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
        let capacity_pending = dispatch::Pending::default();
        let capacity = dispatch::Dispatches {
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
        let mut occupied = None;
        let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
        let meter = || wire::meter(until(10_000));
        let smtp_cap = until(if n == 160 { 2_000 } else { 10_000 });
        let prepared = worker
            .publish_prepared(
                claim,
                generation,
                dispatch::Input {
                    submission,
                    retry: false,
                    deadline: smtp_cap,
                },
                &mut meter(),
            )
            .unwrap();
        let attempt = prepared.attempt();
        let key = format!("decode('{attempt}','hex')");
        let classification = match n {
            160 => health::Failure::Auth,
            163 => health::Failure::Tls,
            _ => health::Failure::Network,
        };
        if n == 160 {
            let (refusal, mut peer) = wire::refused(fixture, clock.clone(), smtp_cap, policies);
            assert_eq!(refusal.error, ports::Error::Forbidden);
            drop(refusal.into_buffers().unwrap());
            if let Ok(IoProgress::Bytes(count)) = peer.read(&mut [0; 512]) {
                assert_eq!(count, 0);
            }
            thread::sleep(Duration::from_millis(2_100));
        }
        let mut failed = prepared.into_failed(classification);
        assert_eq!(failed.attempt(), attempt);
        if n == 159 {
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &slots),
                Err(Error::Admission(ports::Error::Forbidden))
            );
            assert_eq!(
                failed.step(until(0), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Deadline))
            );
            let mut short = Meter::new(
                until(10_000),
                Charge {
                    io_bytes: 3 * 1024 * 1024 + 128 * 1024 - 1,
                    records: 4096,
                    ..Default::default()
                },
            );
            assert_eq!(
                failed.step(until(10_000), &mut short, &settlement),
                Err(Error::Admission(ports::Error::Capacity))
            );
            let mut short_records = Meter::new(
                until(10_000),
                Charge {
                    io_bytes: 3 * 1024 * 1024 + 128 * 1024,
                    records: 4095,
                    ..Default::default()
                },
            );
            assert_eq!(
                failed.step(until(10_000), &mut short_records, &settlement),
                Err(Error::Admission(ports::Error::Capacity))
            );
            let held = worker
                .reserve_final(attempt, until(10_000), &mut meter())
                .unwrap();
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Busy))
            );
            drop(held);
            assert_eq!(pending.identity().unwrap(), None);
            sql(fixture,"CREATE FUNCTION public.reject_failed_health() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_failed_health BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_failed_health()");
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::RouteFailure(health::Error::Database))
            );
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT count(*) FROM td_mta.route_health_receipts WHERE operation={key}"
                    )
                ),
                "0"
            );
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT active FROM td_mta.submission_attempts WHERE attempt_id={key}"
                    )
                ),
                "t"
            );
            sql(
                fixture,
                "DROP TRIGGER reject_failed_health ON td_mta.route_health_receipts",
            );
        }
        if n == 161 {
            sql(fixture,"CREATE FUNCTION public.delay_failed_health() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_failed_health AFTER INSERT ON td_mta.route_health_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_failed_health()");
            assert_eq!(
                failed.step(until(1500), &mut meter(), &settlement),
                Err(Error::RouteFailure(health::Error::Indeterminate))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            occupied = Some(
                capacity
                    .reserve_final(attempt, until(10_000), &mut meter())
                    .unwrap(),
            );
            let mut empty = Meter::new(until(10_000), Charge::default());
            assert_eq!(
                failed.step(until(10_000), &mut empty, &settlement),
                Err(Error::Admission(ports::Error::Capacity))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            let held_settlement = settlement_capacity
                .reserve_final(attempt, until(10_000), &mut meter())
                .unwrap();
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Busy))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            drop(held_settlement);
            assert_eq!(
                failed.step(until(0), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Deadline))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            assert_eq!(
                worker.publish_recovery(
                    recovery::interrupted(claim, generation, attempt).unwrap(),
                    until(10_000),
                    &mut meter()
                ),
                Err(Error::Admission(ports::Error::Busy))
            );
            assert_eq!(
                worker
                    .publish_prepared(
                        claim,
                        generation,
                        dispatch::Input {
                            submission,
                            retry: false,
                            deadline: until(10_000)
                        },
                        &mut meter()
                    )
                    .map(|_| ()),
                Err(Error::Admission(ports::Error::Busy))
            );
            thread::sleep(Duration::from_secs(4));
            sql(
                fixture,
                "DROP TRIGGER delay_failed_health ON td_mta.route_health_receipts",
            );
            sql(fixture, &format!("CREATE TABLE public.failed_health_receipt AS SELECT * FROM td_mta.route_health_receipts WHERE operation={key}; UPDATE td_mta.route_health_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation={key}"));
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::RouteFailure(health::Error::Conflict))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(
                fixture,
                &format!("DELETE FROM td_mta.route_health_receipts WHERE operation={key}"),
            );
            assert_eq!(
                failed
                    .step(until(10_000), &mut meter(), &settlement)
                    .unwrap(),
                None
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(fixture, "INSERT INTO td_mta.route_health_receipts SELECT * FROM public.failed_health_receipt; DROP TABLE public.failed_health_receipt");
            assert!(matches!(
                pending
                    .settle(&settle, until(10_000), &mut meter())
                    .unwrap(),
                Some(dispatch::Settlement::RouteFailure(_))
            ));
            assert_eq!(pending.identity().unwrap(), None);
        }
        assert_eq!(
            failed
                .step(until(10_000), &mut meter(), &settlement)
                .unwrap(),
            None
        );
        drop(occupied);
        assert_eq!(
            sql(
                fixture,
                &format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id={key}")
            ),
            "t"
        );
        assert_eq!(
            sql(
                fixture,
                &format!("SELECT phase FROM td_mta.recipients WHERE submission_id={sub}")
            ),
            "1"
        );
        assert_eq!(
            sql(
                fixture,
                &format!("SELECT count(*) FROM td_mta.route_health_receipts WHERE operation={key}")
            ),
            "1"
        );
        assert_eq!(
            sql(fixture, "SELECT failure_count FROM td_mta.route_health"),
            "1"
        );
        if n == 163 {
            sql(fixture,"CREATE FUNCTION public.reject_failed_recovery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_failed_recovery BEFORE INSERT ON td_mta.attempt_recovery_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_failed_recovery()");
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Recovery(recovery::Error::Database))
            );
            sql(
                fixture,
                "DROP TRIGGER reject_failed_recovery ON td_mta.attempt_recovery_receipts",
            );
        }
        if n == 164 {
            sql(
                fixture,
                &format!("UPDATE td_mta.submissions SET expires_at=send_at WHERE id={sub}"),
            );
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Recovery(recovery::Error::ExpiryHold))
            );
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id={sub}"));
        }
        if n == 162 {
            sql(fixture,"CREATE FUNCTION public.delay_failed_recovery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_failed_recovery AFTER INSERT ON td_mta.attempt_recovery_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_failed_recovery()");
            assert_eq!(
                failed.step(until(1500), &mut meter(), &settlement),
                Err(Error::Recovery(recovery::Error::Indeterminate))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            assert_eq!(
                failed.step(until(0), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Deadline))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            thread::sleep(Duration::from_secs(4));
            sql(
                fixture,
                "DROP TRIGGER delay_failed_recovery ON td_mta.attempt_recovery_receipts",
            );
            sql(fixture, &format!("CREATE TABLE public.failed_recovery_receipt AS SELECT * FROM td_mta.attempt_recovery_receipts WHERE attempt_id={key}; UPDATE td_mta.attempt_recovery_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE attempt_id={key}"));
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Recovery(recovery::Error::Conflict))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(
                fixture,
                &format!("DELETE FROM td_mta.attempt_recovery_receipts WHERE attempt_id={key}"),
            );
            assert_eq!(
                failed
                    .step(until(10_000), &mut meter(), &settlement)
                    .unwrap(),
                None
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(attempt))
            );
            sql(fixture, "INSERT INTO td_mta.attempt_recovery_receipts SELECT * FROM public.failed_recovery_receipt; DROP TABLE public.failed_recovery_receipt");
            assert!(matches!(
                pending
                    .settle(&settle, until(10_000), &mut meter())
                    .unwrap(),
                Some(dispatch::Settlement::Recovery(_))
            ));

            // Another operation may occupy Pending after generic settlement.
            let other = SubmissionId::from_bytes([166; 16]);
            let other_sub = format!("decode('{other}','hex')");
            sql(fixture,&format!("UPDATE td_mta.route_health SET failed_at=failed_at-1200000,next_at=next_at-1200000; INSERT INTO td_mta.submissions SELECT account,{other_sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{other_sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{other_sub},0,'other@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={other_sub}"));
            owners::renew(request, trust, deadline(), claim, generation).unwrap();
            sql(fixture,"CREATE FUNCTION public.delay_unrelated_prepared() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_unrelated_prepared AFTER INSERT ON td_mta.submission_attempts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_unrelated_prepared()");
            assert_eq!(
                worker
                    .publish_prepared(
                        claim,
                        generation,
                        dispatch::Input {
                            submission: other,
                            retry: false,
                            deadline: until(1500)
                        },
                        &mut meter()
                    )
                    .map(|_| ()),
                Err(Error::Publication(
                    td_mta::postgres_attempts::Error::Indeterminate
                ))
            );
            let Identity::Attempt(other_attempt) = pending.identity().unwrap().unwrap() else {
                panic!("expected pending attempt");
            };
            assert_ne!(other_attempt, attempt);
            assert_eq!(
                failed.step(until(10_000), &mut meter(), &settlement),
                Err(Error::Admission(ports::Error::Busy))
            );
            assert_eq!(
                pending.identity().unwrap(),
                Some(Identity::Attempt(other_attempt))
            );
            thread::sleep(Duration::from_secs(4));
            assert!(matches!(
                pending
                    .settle(&settle, until(10_000), &mut meter())
                    .unwrap(),
                Some(dispatch::Settlement::Dispatch(_))
            ));
            sql(
                fixture,
                "DROP TRIGGER delay_unrelated_prepared ON td_mta.submission_attempts",
            );
            worker
                .publish_recovery(
                    recovery::interrupted(claim, generation, other_attempt).unwrap(),
                    until(10_000),
                    &mut meter(),
                )
                .unwrap();
            sql(fixture,&format!("UPDATE td_mta.route_health SET failed_at=failed_at+1200000,next_at=next_at+1200000; DELETE FROM td_mta.recipients WHERE submission_id={other_sub}; DELETE FROM td_mta.submissions WHERE id={other_sub}"));
        }
        let outcome = failed
            .step(until(10_000), &mut meter(), &settlement)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.recovery.attempt, attempt);
        assert_eq!(outcome.recovery.recipients, 1);
        assert_eq!(
            outcome.recovery.new_sequence,
            outcome.recovery.old_sequence + 1
        );
        assert_eq!(
            sql(
                fixture,
                &format!(
                    "SELECT count(*) FROM td_mta.attempt_recovery_receipts WHERE attempt_id={key}"
                )
            ),
            "1"
        );
        assert_eq!(
            sql(fixture, "SELECT failure_count FROM td_mta.route_health"),
            "1"
        );
        assert_eq!(outcome.health.state.failure_count, 1);
        assert_eq!(
            outcome.health.state.repair_generation.is_some(),
            matches!(n, 160 | 163)
        );
        assert_eq!(pending.identity().unwrap(), None);
        let before = sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        );
        assert_eq!(
            failed
                .step(until(10_000), &mut meter(), &settlement)
                .unwrap(),
            Some(outcome)
        );
        assert_eq!(
            sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts"
            ),
            before
        );
        assert_eq!(
            sql(
                fixture,
                &format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id={key}")
            ),
            "f"
        );
        assert_eq!(
            sql(
                fixture,
                &format!(
                    "SELECT state,uncertain,phase FROM td_mta.recipients WHERE submission_id={sub}"
                )
            ),
            "3|0|4"
        );
        assert!(matches!(
            candidates::scan(
                request,
                trust,
                deadline(),
                claim,
                generation,
                Kind::Fresh,
                None
            ),
            Err(candidates::Error::RouteHealth(
                health::Error::RepairRequired
            )) | Err(candidates::Error::RouteHealth(health::Error::Backoff))
        ));
        sql(fixture,&format!("DELETE FROM td_mta.route_health; DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
}
