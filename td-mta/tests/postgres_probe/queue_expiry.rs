use super::{deadline, sql, Fixture};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
    ports::{Clock, Tick, Time},
    postgres_probe::Request,
    postgres_queue_expiry as expiry,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
};
struct TestClock {
    start: Instant,
    expire_on_sample: AtomicU64,
    samples: AtomicU64,
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, td_mta::ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(
                300_000
                    + self.start.elapsed().as_millis() as u64
                    + if self.samples.fetch_add(1, Ordering::Relaxed) + 1
                        >= self.expire_on_sample.load(Ordering::Relaxed)
                    {
                        10_000
                    } else {
                        0
                    },
            ),
        })
    }
}
fn confirmed(fixture: &Fixture) -> (Arc<TestClock>, Arc<Mutex<Guard>>) {
    let now: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    let clock = Arc::new(TestClock {
        start: Instant::now(),
        expire_on_sample: AtomicU64::new(u64::MAX),
        samples: AtomicU64::new(0),
    });
    let mut guard = Guard::new(Time {
        utc_ms: now - 300_000,
        monotonic: Tick(0),
    })
    .unwrap();
    guard
        .confirm(Time {
            utc_ms: now,
            monotonic: Tick(300_000),
        })
        .unwrap();
    (clock, Arc::new(Mutex::new(guard)))
}
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
) {
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let value = expiry::prepare(claim, generation, submission).unwrap();
    let (clock, guard) = confirmed(fixture);
    let held = Arc::new(Mutex::new(
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
    assert_eq!(
        expiry::publish(request, trust, deadline(), &value, clock.clone(), held),
        Err(expiry::Error::Clock(td_mta::queue_clock::Error::Unstable))
    );
    assert_eq!(
        expiry::publish(
            request,
            trust,
            deadline(),
            &expiry::prepare(claim, generation + 1, submission).unwrap(),
            clock.clone(),
            guard.clone()
        ),
        Err(expiry::Error::Owner(owners::Error::Stale))
    );
    assert_eq!(
        expiry::publish(
            request,
            trust,
            deadline(),
            &value,
            clock.clone(),
            guard.clone()
        ),
        Err(expiry::Error::NotDue)
    );
    // Keep 200 pending rows and terminal rows beyond the two bounded batches.
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=send_at,completed_at=NULL,notification=0 WHERE id=decode('{submission}','hex'); UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL,diagnostic='' WHERE submission_id=decode('{submission}','hex') AND ordinal<200"));
    let previous=sql(fixture,&format!("SELECT encode(attempt_id,'hex') FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=200"));
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=2,attempt_id=decode('{previous}','hex'),attempt_count=1,last_attempt_at=0,phase=3,next_attempt_at=NULL,rcpt_reply='250 actual RCPT' WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.recipients SET state=7,uncertain=1,attempt_id=decode('{previous}','hex'),attempt_count=1,last_attempt_at=0,phase=4,reason=3,rcpt_reply='250 prior RCPT',data_reply='450 prior DATA' WHERE submission_id=decode('{submission}','hex') AND ordinal=100; UPDATE td_mta.recipients SET state=5,next_attempt_at=NULL,reason=2,data_reply='550 terminal DATA' WHERE submission_id=decode('{submission}','hex') AND ordinal=203; UPDATE td_mta.submission_attempts SET active=true WHERE attempt_id=decode('{previous}','hex')"));
    let frozen=sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,attempt_count,rcpt_reply,data_reply),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>=200"));
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    thread::sleep(Duration::from_millis(1100));
    sql(fixture,"CREATE FUNCTION public.reject_expiry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_expiry BEFORE INSERT ON td_mta.submission_expiry_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_expiry()");
    assert_eq!(
        expiry::publish(
            request,
            trust,
            deadline(),
            &value,
            clock.clone(),
            guard.clone()
        ),
        Err(expiry::Error::Database)
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
            "SELECT published_at IS NULL FROM td_mta.queue_expiry_progress"
        ),
        "t"
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<200 AND reason=6")),"0");
    sql(
        fixture,
        "DROP TRIGGER reject_expiry ON td_mta.submission_expiry_receipts",
    );
    // A token that expires during SQL work cannot publish even staged changes.
    let (late_clock, late_guard) = confirmed(fixture);
    late_clock.expire_on_sample.store(2, Ordering::Relaxed);
    assert_eq!(
        expiry::publish(request, trust, deadline(), &value, late_clock, late_guard),
        Err(expiry::Error::Clock(td_mta::queue_clock::Error::Expired))
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
            "SELECT published_at IS NULL FROM td_mta.queue_expiry_progress"
        ),
        "t"
    );

    thread::sleep(Duration::from_millis(1100));
    sql(fixture,"CREATE FUNCTION public.delay_expiry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_expiry AFTER INSERT ON td_mta.submission_expiry_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_expiry()");
    assert_eq!(
        expiry::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value,
            clock.clone(),
            guard.clone()
        ),
        Err(expiry::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let first = expiry::publish(
        request,
        trust,
        deadline(),
        &value,
        clock.clone(),
        guard.clone(),
    )
    .unwrap();
    assert_eq!(first.recipients, 100);
    assert_eq!(sql(fixture, &format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{previous}','hex')")), "f");
    assert_eq!(first.new_sequence, first.old_sequence + 1);
    assert_eq!(
        expiry::inspect(request, trust, deadline(), &value).unwrap(),
        Some(first)
    );
    assert_eq!(sql(fixture,&format!("SELECT state,uncertain,phase,reason,next_attempt_at IS NULL,rcpt_reply FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=0")),"7|1|4|6|t|250 actual RCPT");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal BETWEEN 1 AND 99 AND state=5 AND phase=0 AND attempt_count=0 AND reason=6")),"99");
    assert_eq!(sql(fixture,&format!("SELECT completed_at IS NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t|0");
    sql(
        fixture,
        "DROP TRIGGER delay_expiry ON td_mta.submission_expiry_receipts",
    );
    // A separately confirmed process still honors database-wide spacing.
    sql(fixture,"UPDATE td_mta.queue_expiry_progress SET published_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+5000");
    let second = expiry::prepare(claim, generation, submission).unwrap();
    let (other_clock, other_guard) = confirmed(fixture);
    assert_eq!(
        expiry::publish(
            request,
            trust,
            deadline(),
            &second,
            other_clock.clone(),
            other_guard.clone()
        ),
        Err(expiry::Error::RateLimited)
    );
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.queue_expiry_progress SET published_at={}",
            first.published_at
        ),
    );
    thread::sleep(Duration::from_millis(1100));
    let second = expiry::publish(
        request,
        trust,
        deadline(),
        &second,
        other_clock.clone(),
        other_guard.clone(),
    )
    .unwrap();
    assert_eq!(second.recipients, 100);
    assert_eq!(sql(fixture,&format!("SELECT state,uncertain,phase,reason,next_attempt_at IS NULL,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=100")),"7|1|4|6|t|250 prior RCPT|450 prior DATA");
    assert_eq!(sql(fixture,&format!("SELECT completed_at IS NOT NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t|1");
    assert_eq!(sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,attempt_count,rcpt_reply,data_reply),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>=200")),frozen);
    // Replays settle even after an explicit clock hold.
    let now: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    guard
        .lock()
        .unwrap()
        .primary_changed(Time {
            utc_ms: now,
            monotonic: clock.sample().unwrap().monotonic,
        })
        .unwrap();
    assert_eq!(
        expiry::publish(
            request,
            trust,
            deadline(),
            &value,
            clock.clone(),
            guard.clone()
        )
        .unwrap(),
        first
    );
    let digest=sql(fixture,&format!("SELECT encode(request_digest,'hex') FROM td_mta.submission_expiry_receipts WHERE operation=decode('{}','hex')",value.operation()));
    sql(fixture,&format!("UPDATE td_mta.submission_expiry_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation=decode('{}','hex')",value.operation()));
    assert_eq!(
        expiry::inspect(request, trust, deadline(), &value),
        Err(expiry::Error::Conflict)
    );
    sql(fixture,&format!("UPDATE td_mta.submission_expiry_receipts SET request_digest=decode('{digest}','hex') WHERE operation=decode('{}','hex')",value.operation()));
}
