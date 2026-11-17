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
pub(super) struct Runtime {
    pub clock: Arc<td_mta::clock::RuntimeClock>,
    pub guard: Arc<Mutex<Guard>>,
    stable_at: u64,
}
impl Runtime {
    fn sample(&self, fixture: &Fixture) -> Time {
        Time {
            utc_ms: sql(
                fixture,
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            )
            .parse()
            .unwrap(),
            monotonic: self.clock.sample().unwrap().monotonic,
        }
    }
    pub(super) fn new(fixture: &Fixture) -> Self {
        let clock = Arc::new(td_mta::clock::RuntimeClock::new());
        let sample = Time {
            utc_ms: sql(
                fixture,
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            )
            .parse()
            .unwrap(),
            monotonic: clock.sample().unwrap().monotonic,
        };
        Self {
            clock,
            guard: Arc::new(Mutex::new(Guard::new(sample).unwrap())),
            stable_at: sample.monotonic.0 + 300_000,
        }
    }
    fn confirm(
        &self,
        fixture: &Fixture,
        request: &Request<'_>,
        trust: &TrustStore,
        claim: &Claim,
        generation: u64,
    ) {
        let now = self.clock.sample().unwrap().monotonic.0;
        if now < self.stable_at {
            eprintln!(
                "waiting {}ms for the real queue clock hold",
                self.stable_at - now
            );
        }
        loop {
            let now = self.clock.sample().unwrap().monotonic.0;
            if now >= self.stable_at {
                break;
            }
            owners::renew(request, trust, deadline(), claim, generation).unwrap();
            thread::sleep(Duration::from_millis((self.stable_at - now).min(10_000)));
        }

        assert!(
            self.guard
                .lock()
                .unwrap()
                .confirm(self.sample(fixture))
                .unwrap()
                .expiry
        );
    }
}

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
    fixture
        .queue_runtime
        .as_ref()
        .unwrap()
        .confirm(fixture, request, trust, claim, generation);
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
    // Keep 200 pending rows; the reserved subset straddles ordinary batch boundaries.
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=send_at,completed_at=NULL,notification=0 WHERE id=decode('{submission}','hex'); UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL,diagnostic='' WHERE submission_id=decode('{submission}','hex') AND ordinal<200"));
    let previous=sql(fixture,&format!("SELECT encode(attempt_id,'hex') FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=200"));
    // Retry publication reclaimed that retired attempt (POSTGRES.md section
    // 218); restore a row for it to hold the reservation below.
    sql(fixture,&format!("INSERT INTO td_mta.submission_attempts SELECT (jsonb_populate_record(NULL::td_mta.submission_attempts,to_jsonb(a)||jsonb_build_object('attempt_id','\\x{previous}'))).* FROM (SELECT * FROM td_mta.submission_attempts WHERE submission_id=decode('{submission}','hex') LIMIT 1) a ON CONFLICT DO NOTHING"));
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=2,attempt_id=decode('{previous}','hex'),attempt_count=1,last_attempt_at=0,phase=3,next_attempt_at=NULL,rcpt_reply='250 actual RCPT' WHERE submission_id=decode('{submission}','hex') AND ordinal IN(0,150); UPDATE td_mta.recipients SET state=7,uncertain=1,attempt_id=decode('{previous}','hex'),attempt_count=1,last_attempt_at=0,phase=4,reason=3,rcpt_reply='250 prior RCPT',data_reply='450 prior DATA' WHERE submission_id=decode('{submission}','hex') AND ordinal=100; UPDATE td_mta.recipients SET state=5,next_attempt_at=NULL,reason=2,data_reply='550 terminal DATA' WHERE submission_id=decode('{submission}','hex') AND ordinal=203; UPDATE td_mta.submission_attempts SET active=true,ordinals=decode('0000000000000096','hex') WHERE attempt_id=decode('{previous}','hex'); INSERT INTO td_mta.attempt_final_reservations SELECT account,attempt_id,ordinals FROM td_mta.submission_attempts WHERE attempt_id=decode('{previous}','hex')"));
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
    let first = super::expiry_admission::exercise(
        fixture, request, trust, claim, generation, submission, &value,
    );
    assert_eq!(first.recipients, 2);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "0"
    );
    assert_eq!(sql(fixture,&format!("SELECT state,uncertain,phase,reason FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=150")),"7|1|4|6");
    assert_eq!(sql(fixture, &format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{previous}','hex')")), "f");
    assert_eq!(first.new_sequence, first.old_sequence + 1);
    assert_eq!(
        expiry::inspect(request, trust, deadline(), &value).unwrap(),
        Some(first)
    );
    assert_eq!(sql(fixture,&format!("SELECT state,uncertain,phase,reason,next_attempt_at IS NULL,rcpt_reply FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=0")),"7|1|4|6|t|250 actual RCPT");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal BETWEEN 1 AND 99 AND state=5 AND phase=0 AND attempt_count=0 AND reason=6")),"0");
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
    assert_eq!(sql(fixture,&format!("SELECT completed_at IS NULL FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t");
    thread::sleep(Duration::from_millis(1100));
    let third = expiry::prepare(claim, generation, submission).unwrap();
    let third = expiry::publish(
        request,
        trust,
        deadline(),
        &third,
        other_clock.clone(),
        other_guard.clone(),
    )
    .unwrap();
    assert_eq!(third.recipients, 98);
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
    ledger(
        fixture,
        request,
        trust,
        claim,
        generation,
        submission,
        &value,
        first,
        &other_clock,
        &other_guard,
    );
}

// Expiry receipts last an hour (POSTGRES.md section 120): a full ledger
// refuses only while its receipts are young, and the next expiry deletes
// only this account's expired ones. Ages are literals, so a longer
// retention fails here.
#[allow(clippy::too_many_arguments)]
fn ledger(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
    value: &expiry::Expiry,
    first: expiry::Outcome,
    clock: &Arc<TestClock>,
    guard: &Arc<Mutex<Guard>>,
) {
    const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";
    // "expiry" prefix: other fixtures repeat one byte.
    const OVERDUE: &str = "6578706972790000000000000000ee01";
    const OTHER: &str = "decode('6578706972790000000000000000ee02','hex')";
    let overdue = format!("decode('{OVERDUE}','hex')");
    let src = format!("decode('{source}','hex')");
    sql(fixture, &format!("INSERT INTO td_mta.submissions SELECT account,{overdue},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,send_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{overdue},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{overdue},0,'expiry@example.test',1,0,NULL,0,NULL,0,0,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={overdue}"));
    let account = sql(
        fixture,
        &format!("SELECT encode(account,'hex') FROM td_mta.submissions WHERE id={overdue}"),
    );
    let scope = account.clone();
    let account = format!("decode('{account}','hex')");
    // Fill the ledger with young receipts whose request digest is zero.
    sql(fixture, &format!("INSERT INTO td_mta.submission_expiry_receipts SELECT account,decode('{}'||lpad(to_hex(n),20,'0'),'hex'),decode(repeat('00',32),'hex'),submission_id,recipient_count,0,{NOW},old_sequence,new_sequence FROM td_mta.submission_expiry_receipts CROSS JOIN generate_series(1,1024) n WHERE operation=decode('{}','hex')", &OVERDUE[..12], value.operation()));
    let publish = || {
        thread::sleep(Duration::from_millis(1100));
        expiry::publish(
            request,
            trust,
            deadline(),
            &expiry::prepare(claim, generation, SubmissionId::parse(OVERDUE).unwrap()).unwrap(),
            clock.clone(),
            guard.clone(),
        )
    };
    assert_eq!(publish(), Err(expiry::Error::Capacity));
    // Backdate the filler past retention; keep a receipt 30 minutes old and
    // another account's expired one.
    sql(fixture, &format!("UPDATE td_mta.submission_expiry_receipts SET published_at=published_at-3660000 WHERE request_digest=decode(repeat('00',32),'hex'); INSERT INTO td_mta.accounts VALUES({OTHER},decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex')); INSERT INTO td_mta.account_usage VALUES({OTHER},0,0,0,0,0); INSERT INTO td_mta.submission_expiry_receipts SELECT account,decode(repeat('11',16),'hex'),decode(repeat('11',32),'hex'),submission_id,recipient_count,0,{NOW}-1800000,old_sequence,new_sequence FROM td_mta.submission_expiry_receipts WHERE operation=decode('{op}','hex'); INSERT INTO td_mta.submission_expiry_receipts SELECT {OTHER},decode(repeat('22',16),'hex'),decode(repeat('22',32),'hex'),submission_id,recipient_count,0,{NOW}-3660000,old_sequence,new_sequence FROM td_mta.submission_expiry_receipts WHERE operation=decode('{op}','hex')", op = value.operation()));
    let epoch = td_mta::ids::StoreEpoch::parse(&sql(
        fixture,
        "SELECT encode(store_epoch,'hex') FROM td_mta.schema_migrations LIMIT 1",
    ))
    .unwrap();
    let capacity = td_mta::postgres_capacity::inspect(request, trust, deadline(), epoch).unwrap();
    let used = capacity
        .get("metrics")
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .find(|r| {
            r.get("name").and_then(td_json::Json::as_str) == Some("submission_expiry_receipts")
                && r.get("scope").and_then(td_json::Json::as_str) == Some(scope.as_str())
        })
        .unwrap()
        .get("used")
        .unwrap()
        .to_string();
    assert_eq!(
        used,
        sql(
            fixture,
            &format!("SELECT (SELECT count(*) FROM td_mta.submission_expiry_receipts WHERE account={account} AND published_at>={NOW}-3600000)+(SELECT count(*) FROM td_mta.attempt_final_reservations WHERE account={account})")
        )
    );
    assert!(used.parse::<u64>().unwrap() < 1024);
    let outcome = publish().unwrap();
    assert_eq!(outcome.recipients, 1);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_expiry_receipts WHERE request_digest=decode(repeat('00',32),'hex')"
        ),
        "0"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_expiry_receipts WHERE request_digest IN (decode(repeat('11',32),'hex'),decode(repeat('22',32),'hex'))"
        ),
        "2"
    );
    // The held operation's own young receipt still settles.
    assert_eq!(
        expiry::inspect(request, trust, deadline(), value).unwrap(),
        Some(first)
    );
    sql(fixture, &format!("DELETE FROM td_mta.submission_expiry_receipts WHERE request_digest IN (decode(repeat('11',32),'hex'),decode(repeat('22',32),'hex')) OR submission_id={overdue}; DELETE FROM td_mta.account_usage WHERE account={OTHER}; DELETE FROM td_mta.accounts WHERE id={OTHER}; DELETE FROM td_mta.recipients WHERE submission_id={overdue}; DELETE FROM td_mta.submission_routes WHERE submission_id={overdue}; DELETE FROM td_mta.submissions WHERE id={overdue}"));
}
