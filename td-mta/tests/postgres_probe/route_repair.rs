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
    sql(fixture,"CREATE FUNCTION public.reject_repair() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=2 THEN RAISE EXCEPTION 'fixture refusal'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_repair BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_repair()");
    assert_eq!(
        repair::publish(request, trust, deadline(), &value),
        Err(Error::Database)
    );
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
    sql(fixture,"DROP TRIGGER reject_repair ON td_mta.route_health_receipts; CREATE FUNCTION public.delay_repair() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=2 THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_repair AFTER INSERT ON td_mta.route_health_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_repair()");
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
}
