use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    generations::GenerationSet,
    ids::{BootId, SubmissionId},
    postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_health::Error,
    postgres_route_owners as owners, postgres_route_repair as repair,
    postgres_runtime_profile::{self as runtime, Binding},
    postgres_selection as selection,
    tls_policy::TlsPolicies,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    submission: SubmissionId,
) {
    let old = owners::prepare(binding, BootId::from_bytes([88; 16])).unwrap();
    let owner = owners::acquire(request, trust, deadline(), &old).unwrap();
    assert_eq!(
        repair::publish(
            request,
            trust,
            deadline(),
            &repair::prepare(&old, owner.generation).unwrap()
        ),
        Err(Error::RepairRequired)
    );
    owners::release(request, trust, deadline(), &old, owner.generation).unwrap();
    // Explicitly reselect validated configuration, then resolve a fresh profile.
    let admin = fixture.request();
    let (selected, candidate) = selection::read(&admin, trust, deadline(), binding.epoch())
        .unwrap()
        .unwrap();
    let change = selection::prepare(
        binding.epoch(),
        selected.generation,
        selected.version,
        &candidate,
    )
    .unwrap();
    let new = selection::publish(&admin, trust, deadline(), change).unwrap();
    assert!(new.generation > selected.generation);
    let domain = GenerationSet::<TlsPolicies>::at_startup();
    let loaded = runtime::load(
        request,
        trust,
        deadline(),
        binding.epoch(),
        binding.profile(),
        domain.reserve().unwrap(),
    )
    .unwrap();
    let claim = owners::prepare(loaded.binding(), BootId::from_bytes([89; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let value = repair::prepare(&claim, grant.generation).unwrap();
    assert_eq!(
        repair::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        repair::publish(
            request,
            trust,
            deadline(),
            &repair::prepare(&claim, grant.generation + 1).unwrap()
        ),
        Err(Error::Owner(owners::Error::Stale))
    );
    assert_eq!(
        super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::retry(&claim, grant.generation, submission).unwrap()
        ),
        Err(attempts::Error::RouteHealth(Error::RepairRequired))
    );
    // Repair must preserve an independently seeded, still-future retry interval.
    sql(fixture,"UPDATE td_mta.route_health SET failed_at=(extract(epoch FROM clock_timestamp())*1000)::bigint,next_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+600000");
    let snapshot = sql(
        fixture,
        "SELECT failure_count,reason,failed_at,next_at FROM td_mta.route_health",
    );
    let revision = sql(
        fixture,
        "SELECT encode(revision,'hex') FROM td_mta.route_health",
    );
    sql(
        fixture,
        "UPDATE td_mta.route_health SET revision=decode(repeat('ff',8),'hex')",
    );
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Capacity)
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.route_health SET revision=decode('{revision}','hex')"),
    );
    // A competing repair cannot spend the final receipt of an outstanding
    // attempt. A settled failure releases the reservation even while retained.
    let active = sql(
        fixture,
        "SELECT encode(attempt_id,'hex') FROM td_mta.submission_attempts WHERE active",
    );
    sql(fixture, "CREATE TABLE public.saved_fence AS SELECT attempt_id,owner_boot,owner_operation,generation FROM td_mta.submission_attempts WHERE active; UPDATE td_mta.submission_attempts a SET owner_boot=o.owner_boot,owner_operation=o.operation,generation=o.generation FROM td_mta.route_owners o WHERE a.active AND o.account=a.account AND o.route_name=a.route_name");
    sql(fixture, "CREATE TABLE public.saved_health AS SELECT * FROM td_mta.route_health_receipts; DELETE FROM td_mta.route_health_receipts");
    sql(fixture, "INSERT INTO td_mta.route_health SELECT account,'reserved-'||i,destination,revision,failure_count,reason,failed_at,next_at,repair_generation FROM td_mta.route_health CROSS JOIN generate_series(0,255) i");
    sql(fixture, "INSERT INTO td_mta.route_health_receipts SELECT h.account,decode(md5(h.route_name||':'||i),'hex'),h.destination,h.route_name,h.destination,2,int8send(i::bigint),int8send(i::bigint+1),h.failure_count,h.reason,h.failed_at,h.next_at,NULL FROM td_mta.route_health h CROSS JOIN generate_series(1,4) i WHERE h.route_name LIKE 'reserved-%' AND NOT(h.route_name='reserved-255' AND i=4)");
    sql(fixture,"CREATE FUNCTION public.reject_repair() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=2 THEN RAISE EXCEPTION 'fixture refusal'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_repair BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_repair()");
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Capacity)
    );
    sql(fixture, &format!("UPDATE td_mta.submission_attempts a SET active=false,owner_boot=f.owner_boot,owner_operation=f.owner_operation,generation=f.generation FROM public.saved_fence f WHERE a.attempt_id=f.attempt_id AND a.attempt_id=decode('{active}','hex'); DROP TABLE public.saved_fence"));
    // The accepting owner was replaced before it could reset the route;
    // its retired receipt cannot require capacity from the current owner.
    sql(fixture, &format!("INSERT INTO td_mta.attempt_acceptance_receipts SELECT account,attempt_id,4,request_digest,1,old_sequence,new_sequence FROM td_mta.submission_attempts WHERE attempt_id=decode('{active}','hex')"));
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Database)
    );
    // A still-owned acceptance on a different route occupies the last slot.
    // Only a completed reset outside the cross-route window can release it.
    sql(fixture, &format!("UPDATE td_mta.submission_attempts SET active=true WHERE attempt_id=decode('{active}','hex');
        INSERT INTO td_mta.route_owners SELECT account,'sibling',destination,generation,owner_boot,operation,last_clock,expires_at
          FROM td_mta.route_owners WHERE route_name='{}';
        INSERT INTO td_mta.submission_attempts
          SELECT a.account,decode(repeat('72',16),'hex'),a.request_digest,a.submission_id,o.route_name,o.destination,
            o.owner_boot,o.operation,o.generation,o.last_clock-1800000,a.ordinals,a.old_sequence,a.new_sequence,false
          FROM td_mta.submission_attempts a JOIN td_mta.route_owners o ON o.account=a.account
          WHERE a.attempt_id=decode('{active}','hex') AND o.route_name='sibling';
        INSERT INTO td_mta.attempt_acceptance_receipts SELECT account,attempt_id,4,request_digest,1,old_sequence,new_sequence
          FROM td_mta.submission_attempts WHERE route_name='sibling'", loaded.binding().route().name()));
    let reset = "INSERT INTO td_mta.route_health_receipts SELECT account,attempt_id,request_digest,route_name,destination,3,
        int8send(1::bigint),int8send(2::bigint),0,0,NULL,NULL,NULL FROM td_mta.submission_attempts WHERE route_name='sibling'";
    sql(fixture, reset);
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Capacity)
    );
    sql(fixture, "UPDATE td_mta.submission_attempts SET prepared_at=prepared_at-3600000 WHERE route_name='sibling';
        DELETE FROM td_mta.route_health_receipts WHERE route_name='sibling'");
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Capacity)
    );
    sql(fixture, reset);
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Database)
    );
    // The injected repair refusal rolled back its reclaim. Maintenance commits it independently.
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE route_name='sibling'"
        ),
        "1"
    );
    let now: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    let reclaimed = td_mta::postgres_submission_retention::Step::IdleAttempts
        .run(request, trust, deadline(), &claim, grant.generation, now)
        .unwrap();
    assert!(
        reclaimed > 0,
        "attempt cleanup schedules another retention round"
    );
    assert_eq!(sql(fixture, "SELECT (SELECT count(*) FROM td_mta.submission_attempts WHERE route_name='sibling'),
        (SELECT count(*) FROM td_mta.route_health_receipts WHERE route_name='sibling'),
        (SELECT count(*) FROM td_mta.attempt_acceptance_receipts WHERE attempt_id=decode(repeat('72',16),'hex'))"), "0|0|0");
    sql(
        fixture,
        "DELETE FROM td_mta.route_owners WHERE route_name='sibling'",
    );
    sql(fixture, &format!("UPDATE td_mta.submission_attempts SET active=true WHERE attempt_id=decode('{active}','hex'); DELETE FROM td_mta.attempt_acceptance_receipts WHERE attempt_id=decode('{active}','hex'); DELETE FROM td_mta.route_health WHERE route_name LIKE 'reserved-%'; DELETE FROM td_mta.route_health_receipts WHERE route_name LIKE 'reserved-%'; INSERT INTO td_mta.route_health_receipts SELECT * FROM public.saved_health; DROP TABLE public.saved_health"));
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(revision,'hex') FROM td_mta.route_health"
        ),
        revision
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT repair_generation IS NOT NULL FROM td_mta.route_health"
        ),
        "t"
    );
    sql(fixture,"DROP TRIGGER reject_repair ON td_mta.route_health_receipts; INSERT INTO td_mta.route_health_receipts SELECT account,decode('c0000000'||lpad(to_hex(n),24,'0'),'hex'),decode(repeat('00',32),'hex'),route_name,destination,2,decode(lpad(to_hex(1000000+n),16,'0'),'hex'),decode(lpad(to_hex(1000001+n),16,'0'),'hex'),1,3,0,300000,NULL FROM td_mta.route_health CROSS JOIN generate_series(1,6) n WHERE repair_generation IS NOT NULL; CREATE FUNCTION public.delay_repair() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=2 THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_repair AFTER INSERT ON td_mta.route_health_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_repair()");
    assert_eq!(
        repair::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = repair::publish(request, trust, deadline(), &value).unwrap();
    // Six older repairs of this destination were seeded: the sweep keeps the
    // newest four of them, and this repair adds its own.
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT string_agg(right(encode(operation,'hex'),2),',' ORDER BY operation) FILTER (WHERE encode(operation,'hex') LIKE 'c0000000%'),count(*) FILTER (WHERE operation=decode('{op}','hex')) FROM td_mta.route_health_receipts r WHERE action=2 AND (route_name,destination)=(SELECT route_name,destination FROM td_mta.route_health_receipts WHERE operation=decode('{op}','hex'))", op = value.operation())
        ),
        "03,04,05,06|1"
    );
    sql(
        fixture,
        "DELETE FROM td_mta.route_health_receipts WHERE encode(operation,'hex') LIKE 'c0000000%'",
    );
    assert_eq!(outcome.state.revision, outcome.old_revision + 1);
    assert_eq!(outcome.state.failure_count, 2);
    assert_eq!(outcome.state.reason, 5);
    assert_eq!(outcome.state.repair_generation, None);
    assert_eq!(
        sql(
            fixture,
            "SELECT failure_count,reason,failed_at,next_at FROM td_mta.route_health"
        ),
        snapshot
    );
    assert_eq!(
        super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::retry(&claim, grant.generation, submission).unwrap()
        ),
        Err(attempts::Error::RouteHealth(Error::Backoff))
    );
    assert_eq!(
        repair::publish(
            request,
            trust,
            deadline(),
            &repair::prepare(&claim, grant.generation).unwrap()
        ),
        Err(Error::Conflict)
    );
    assert_eq!(
        repair::inspect(request, trust, deadline(), &value).unwrap(),
        Some(outcome)
    );
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();
    assert_eq!(
        repair::publish(request, trust, deadline(), &value).unwrap(),
        outcome
    );
    sql(
        fixture,
        "DROP TRIGGER delay_repair ON td_mta.route_health_receipts",
    );
    // Restore the surrounding fixture's selected generation, as its other
    // configuration-fencing cases do; remove only this fixture's selection
    // receipt so the surrounding race can reuse its generation. Production
    // never decrements generations or deletes a receipt to enable reuse.
    sql(
        fixture,
        &format!(
            "DELETE FROM td_mta.config_selection_receipts WHERE operation=decode('{}','hex'); UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            change.operation(), selected.generation
        ),
    );
    selection_retention(fixture, trust, binding);
}

/// Publication keeps the receipts of the latest RETAINED_GENERATIONS
/// selections; an older retained request reports Expired, never absence.
fn selection_retention(fixture: &Fixture, trust: &TrustStore, binding: &Binding) {
    sql(fixture, "CREATE TABLE public.retain_selection AS TABLE td_mta.config_selection; CREATE TABLE public.retain_receipts AS TABLE td_mta.config_selection_receipts");
    let admin = fixture.request();
    let (selected, candidate) = selection::read(&admin, trust, deadline(), binding.epoch())
        .unwrap()
        .unwrap();
    let mut changes = Vec::new();
    let mut generation = selected.generation;
    for _ in 0..=selection::RETAINED_GENERATIONS {
        let change =
            selection::prepare(binding.epoch(), generation, selected.version, &candidate).unwrap();
        generation = selection::publish(&admin, trust, deadline(), change)
            .unwrap()
            .generation;
        changes.push(change);
    }
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.config_selection_receipts"
        ),
        selection::RETAINED_GENERATIONS.to_string()
    );
    let oldest = *changes.first().unwrap();
    assert_eq!(
        selection::inspect(&admin, trust, deadline(), oldest),
        Err(selection::Error::Expired)
    );
    assert_eq!(
        selection::publish(&admin, trust, deadline(), oldest),
        Err(selection::Error::Expired)
    );
    // The latest RETAINED_GENERATIONS selections still resolve.
    let kept = *changes.get(1).unwrap();
    assert!(selection::inspect(&admin, trust, deadline(), kept)
        .unwrap()
        .is_some());
    sql(fixture, "DELETE FROM td_mta.config_selection_receipts; INSERT INTO td_mta.config_selection_receipts SELECT * FROM public.retain_receipts; DELETE FROM td_mta.config_selection; INSERT INTO td_mta.config_selection SELECT * FROM public.retain_selection; DROP TABLE public.retain_selection, public.retain_receipts");
}
