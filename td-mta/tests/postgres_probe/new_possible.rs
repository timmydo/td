use super::{deadline, sql, sql_command, wait_command, Fixture, ProbeChild};
use std::{
    io::Write,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{ids::AttemptId, postgres_attempt_phases as phases, postgres_probe::Request};

pub(super) fn receipt_race(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    attempt: AttemptId,
    change: &phases::Change,
) {
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture, &format!("CREATE TABLE public.new_possible_receipt AS SELECT * FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex') AND phase=3; DELETE FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex') AND phase=3"));
    let mut blocker = ProbeChild(
        sql_command(fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = blocker.0.stdin.take().unwrap();
    writeln!(
        input,
        "BEGIN; SELECT singleton FROM td_mta.config_selection WHERE singleton=true FOR UPDATE;"
    )
    .unwrap();
    let stop = deadline();
    while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%FROM td_mta.config_selection%'") != "1" {
        assert!(Instant::now() < stop, "policy barrier was not acquired");
        thread::sleep(Duration::from_millis(10));
    }
    thread::scope(|scope| {
        let writer =
            scope.spawn(|| phases::publish_new_possible(request, trust, deadline(), change));
        let stop = deadline();
        while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND wait_event_type='Lock' AND query LIKE 'SELECT generation,version_id FROM td_mta.config_selection%'") != "1" {
            assert!(Instant::now() < stop, "new publication did not reach policy barrier");
            thread::sleep(Duration::from_millis(10));
        }
        writeln!(input, "INSERT INTO td_mta.attempt_phase_receipts SELECT * FROM public.new_possible_receipt; COMMIT;\n\\q").unwrap();
        assert_eq!(writer.join().unwrap(), Err(phases::Error::Conflict));
    });
    drop(input);
    wait_command(&mut blocker);
    sql(fixture, "DROP TABLE public.new_possible_receipt");
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "1"
    );
}

pub(super) fn live_state(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    attempt: AttemptId,
    change: &phases::Change,
    body: bool,
) {
    use std::sync::{Arc, Mutex};
    use td_mta::{
        clock::RuntimeClock,
        ports::{Clock, Time},
        queue_clock::Guard,
    };
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
    let current = if body {
        phases::check_current_body
    } else {
        phases::check_current_possible
    };
    let wrong = if body {
        phases::check_current_possible
    } else {
        phases::check_current_body
    };
    assert_eq!(
        wrong(
            request,
            trust,
            deadline(),
            change,
            clock.clone(),
            guard.clone()
        ),
        Err(phases::Error::Integrity)
    );
    let check = || {
        current(
            request,
            trust,
            deadline(),
            change,
            clock.clone(),
            guard.clone(),
        )
    };
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    super::attempt_clock::current(fixture, request, trust, change, body).unwrap();
    let limit = check().unwrap();
    if body {
        let runtime = Request {
            address: request.address,
            server_name: request.server_name,
            user: "mail_runtime",
            database: request.database,
            password: super::PASSWORD,
            mode: td_mta::postgres_probe::Mode::Runtime,
        };
        current(
            &runtime,
            trust,
            deadline(),
            change,
            clock.clone(),
            guard.clone(),
        )
        .unwrap();
    }
    assert!(limit > Instant::now());
    assert!(limit <= deadline());
    let selection = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.config_selection",
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(check(), Err(phases::Error::Policy));
    sql(
        fixture,
        &format!("UPDATE td_mta.config_selection SET generation=decode('{selection}','hex')"),
    );
    sql(fixture,&format!("UPDATE td_mta.submission_attempts SET active=false WHERE attempt_id=decode('{attempt}','hex')"));
    assert_eq!(check(), Err(phases::Error::Missing));
    sql(fixture,&format!("UPDATE td_mta.submission_attempts SET active=true WHERE attempt_id=decode('{attempt}','hex')"));
    if !body {
        sql(fixture,&format!("CREATE TABLE public.live_claim AS SELECT * FROM td_mta.attempt_final_reservations WHERE attempt_id=decode('{attempt}','hex'); DELETE FROM td_mta.attempt_final_reservations WHERE attempt_id=decode('{attempt}','hex')"));
        assert_eq!(check(), Err(phases::Error::Integrity));
        sql(fixture,"INSERT INTO td_mta.attempt_final_reservations SELECT * FROM public.live_claim; DROP TABLE public.live_claim");
    } else {
        assert_eq!(
            sql(
                fixture,
                "SELECT count(*) FROM td_mta.attempt_final_reservations"
            ),
            "0"
        );
        sql(fixture,&format!("CREATE TABLE public.live_reply AS SELECT ordinal,rcpt_reply FROM td_mta.recipients WHERE attempt_id=decode('{attempt}','hex'); UPDATE td_mta.recipients SET rcpt_reply=overlay(rcpt_reply placing 'y' from char_length(rcpt_reply) for 1) WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
        assert_eq!(check(), Err(phases::Error::Integrity));
        sql(fixture,&format!("UPDATE td_mta.recipients SET rcpt_reply=NULL WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
        assert_eq!(check(), Err(phases::Error::Integrity));
        sql(fixture,&format!("UPDATE td_mta.recipients r SET rcpt_reply=b.rcpt_reply FROM public.live_reply b WHERE r.ordinal=b.ordinal AND r.attempt_id=decode('{attempt}','hex'); DROP TABLE public.live_reply"));
        sql(fixture,&format!("UPDATE td_mta.recipients SET data_reply='250 '||repeat('x',4092) WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
        assert_eq!(check(), Err(phases::Error::Integrity));
        sql(fixture,&format!("UPDATE td_mta.recipients SET data_reply=NULL WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
    }
    let phase = if body { 2 } else { 3 };
    let other = if body { 3 } else { 2 };
    sql(fixture,&format!("UPDATE td_mta.recipients SET phase={other} WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
    assert_eq!(check(), Err(phases::Error::Conflict));
    sql(fixture,&format!("UPDATE td_mta.recipients SET phase={phase} WHERE attempt_id=decode('{attempt}','hex') AND ordinal=0"));
    sql(fixture,&format!("CREATE TABLE public.live_receipt AS SELECT * FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex') AND phase={phase}; DELETE FROM td_mta.attempt_phase_receipts WHERE attempt_id=decode('{attempt}','hex') AND phase={phase}"));
    assert_eq!(check(), Err(phases::Error::Missing));
    sql(fixture,"INSERT INTO td_mta.attempt_phase_receipts SELECT * FROM public.live_receipt; DROP TABLE public.live_receipt");
    sql(fixture,"CREATE TABLE public.live_health AS SELECT * FROM td_mta.route_health; DELETE FROM td_mta.route_health; INSERT INTO td_mta.route_health SELECT account,route_name,destination,decode('0000000000000001','hex'),1,3,t,t+300000,NULL FROM td_mta.route_owners CROSS JOIN (SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint t) x");
    assert_eq!(
        check(),
        Err(phases::Error::Health(
            td_mta::postgres_route_health::Error::Backoff
        ))
    );
    sql(fixture,"UPDATE td_mta.route_health SET reason=4,repair_generation=decode('0000000000000001','hex')");
    assert_eq!(
        check(),
        Err(phases::Error::Health(
            td_mta::postgres_route_health::Error::RepairRequired
        ))
    );
    sql(fixture,"DELETE FROM td_mta.route_health; INSERT INTO td_mta.route_health SELECT * FROM public.live_health; DROP TABLE public.live_health");
    let owner_generation = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.route_owners",
    );
    sql(
        fixture,
        "UPDATE td_mta.route_owners SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        check(),
        Err(phases::Error::Owner(
            td_mta::postgres_route_owners::Error::Stale
        ))
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.route_owners SET generation=decode('{owner_generation}','hex')"),
    );
    let expires=sql(fixture,&format!("SELECT expires_at FROM td_mta.submissions WHERE id=(SELECT submission_id FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex'))"));
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=send_at WHERE id=(SELECT submission_id FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex'))"));
    assert_eq!(check(), Err(phases::Error::ExpiryHold));
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at={expires} WHERE id=(SELECT submission_id FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex'))"));
    assert!(check().is_ok());
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
}
