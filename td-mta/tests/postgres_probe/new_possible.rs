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
