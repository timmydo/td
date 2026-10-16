use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::AttemptId,
    postgres_attempt_phases as phases,
    postgres_probe::Request,
    postgres_route_owners::{self as owners, Claim},
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    claim: &Claim,
    generation: u64,
    attempt: AttemptId,
) {
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    for replies in [
        vec![],
        vec![(0, "450 no")],
        vec![(0, "250 yes\r\n250 injected")],
        vec![(0, "250 yes"), (0, "250 duplicate")],
        vec![(1000, "250 no")],
    ] {
        assert!(phases::body(claim, generation, attempt, &replies).is_err());
    }
    let selection = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.config_selection",
    );
    let replies: Vec<_> = (0..100)
        .map(|n| format!("250 {n:03} {}", "x".repeat(4088)))
        .collect();
    assert!(replies.iter().all(|s| s.len() == 4096));
    let accepted: Vec<_> = replies
        .iter()
        .enumerate()
        .map(|(n, s)| (n as u32, s.as_str()))
        .collect();
    let ordinals: Vec<_> = (0..100).collect();
    let body = phases::body(claim, generation, attempt, &accepted).unwrap();
    let final_phase = phases::acceptance_possible(claim, generation, attempt, &ordinals).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &final_phase),
        Err(phases::Error::Conflict)
    );
    let stale = phases::body(claim, generation + 1, attempt, &[(0, "250 accepted")]).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &stale),
        Err(phases::Error::Owner(owners::Error::Stale))
    );
    let foreign = phases::body(claim, generation, attempt, &[(999, "250 accepted")]).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &foreign),
        Err(phases::Error::Integrity)
    );
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture,"CREATE FUNCTION public.reject_phase() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_phase BEFORE INSERT ON td_mta.attempt_phase_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_phase()");
    assert_eq!(
        phases::publish(request, trust, deadline(), &body),
        Err(phases::Error::Database)
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
            "SELECT count(*) FROM td_mta.recipients WHERE phase=2"
        ),
        "0"
    );
    sql(
        fixture,
        "DROP TRIGGER reject_phase ON td_mta.attempt_phase_receipts",
    );
    let outcome = phases::publish(request, trust, deadline(), &body).unwrap();
    assert_eq!(outcome.recipients, 100);
    assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
    assert_eq!(
        phases::publish(request, trust, deadline(), &body).unwrap(),
        outcome
    );
    assert_eq!(sql(fixture,"SELECT count(DISTINCT rcpt_reply) FROM td_mta.recipients WHERE state=2 AND phase=2 AND octet_length(rcpt_reply)=4096 AND data_reply IS NULL"),"100");
    let changed = phases::body(claim, generation, attempt, &[(0, "250 different")]).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &changed),
        Err(phases::Error::Conflict)
    );
    let other = phases::acceptance_possible(claim, generation, attempt, &[0, 1]).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &other),
        Err(phases::Error::Conflict)
    );
    super::final_capacity::before_possible(fixture, request, trust, attempt, &final_phase);
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_phase() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_phase AFTER INSERT ON td_mta.attempt_phase_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_phase()");
    assert_eq!(
        phases::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &final_phase
        ),
        Err(phases::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "1"
    );
    let settled = phases::inspect(request, trust, deadline(), &final_phase)
        .unwrap()
        .unwrap();
    assert_eq!(settled.old_sequence, outcome.new_sequence);
    assert_eq!(sql(fixture,"SELECT count(DISTINCT rcpt_reply) FROM td_mta.recipients WHERE state=2 AND phase=3 AND octet_length(rcpt_reply)=4096 AND data_reply IS NULL"),"100");
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        phases::publish(request, trust, deadline(), &final_phase).unwrap(),
        settled
    );
    assert_eq!(
        phases::inspect(request, trust, deadline(), &body).unwrap(),
        Some(outcome)
    );
    sql(fixture,&format!("UPDATE td_mta.config_selection SET generation=decode('{selection}','hex'); DROP TRIGGER delay_phase ON td_mta.attempt_phase_receipts"));
    super::final_capacity::competing_writer(fixture, request, trust, binding);
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
}
