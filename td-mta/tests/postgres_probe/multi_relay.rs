use super::{deadline, sql, Fixture};
use td_mta::{
    ids::BootId, postgres_route_owners as owners, postgres_runtime_profile::PublishedProfile,
};

pub(super) fn leases(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &td_crypto::TrustStore,
    profile: &PublishedProfile,
) -> (owners::Claim, owners::Grant) {
    let claims: Vec<_> = profile
        .bindings()
        .map(|binding| owners::prepare(binding, BootId::from_bytes([41; 16])).unwrap())
        .collect();
    let grants: Vec<_> = claims
        .iter()
        .map(|claim| owners::acquire(request, trust, deadline(), claim).unwrap())
        .collect();
    let mut leases: Vec<_> = claims
        .iter()
        .zip(&grants)
        .map(|(claim, grant)| (claim, grant.generation))
        .collect();
    leases.reverse();
    let observed = owners::renew_many(request, trust, deadline(), &leases).unwrap();
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.route_owners WHERE last_clock={observed}")
        ),
        "16"
    );
    assert_eq!(
        owners::renew_many(request, trust, deadline(), &[leases[0], leases[0]]),
        Err(owners::Error::Policy)
    );
    let baseline = sql(fixture, "SELECT string_agg(encode(account,'hex')||':'||expires_at::text,',' ORDER BY account) FROM td_mta.route_owners");
    leases[0].1 += 1;
    assert_eq!(
        owners::renew_many(request, trust, deadline(), &leases),
        Err(owners::Error::Stale)
    );
    assert_eq!(sql(fixture, "SELECT string_agg(encode(account,'hex')||':'||expires_at::text,',' ORDER BY account) FROM td_mta.route_owners"), baseline);
    leases[0].1 -= 1;
    sql(fixture, "CREATE FUNCTION public.delay_multi_lease() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.25); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_multi_lease AFTER UPDATE ON td_mta.route_owners DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_multi_lease()");
    assert_eq!(
        owners::renew_many(
            request,
            trust,
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            &leases
        ),
        Err(owners::Error::Indeterminate)
    );
    std::thread::sleep(std::time::Duration::from_secs(5));
    sql(
        fixture,
        "DROP TRIGGER delay_multi_lease ON td_mta.route_owners",
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(DISTINCT last_clock) FROM td_mta.route_owners"
        ),
        "1"
    );
    for (claim, grant) in claims.iter().zip(&grants) {
        owners::observe(request, trust, deadline(), claim, grant.generation).unwrap();
    }
    // Keep the first account unavailable to the process until the test releases it.
    let rest: Vec<_> = claims
        .iter()
        .zip(&grants)
        .skip(1)
        .map(|(claim, grant)| (claim, grant.generation))
        .collect();
    owners::release_many(request, trust, deadline(), &rest).unwrap();
    owners::renew(request, trust, deadline(), &claims[0], grants[0].generation).unwrap();
    (claims[0].clone(), grants[0])
}
