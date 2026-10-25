use super::{body_checks, sql, Fixture};
use std::{thread, time::Instant};
use td_mta::{
    http_auth::{Authenticator, Principal},
    ports::{Deadline, Error},
    postgres_http_auth::Authentication,
};

pub(super) fn exercise(
    fixture: &Fixture,
    auth: &Authentication<'_>,
    password: &[u8],
    principal: Principal,
    until: impl Fn() -> Deadline + Sync,
) {
    body_checks::quiet(fixture);
    let authenticate = || auth.authenticate("reader", password, until());
    let start = Instant::now();
    assert_eq!(authenticate(), Ok(Some(principal)));
    let cold = start.elapsed();
    let pid = body_checks::backend(fixture);
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT state||'|'||(xact_start IS NULL)::text FROM pg_catalog.pg_stat_activity WHERE pid={pid}")
        ),
        "idle|true"
    );
    assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; ROLLBACK", td_mta::postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "t"));
    thread::scope(|scope| {
        assert_eq!(
            scope.spawn(authenticate).join().unwrap(),
            Err(Error::Invalid)
        );
    });
    assert_eq!(body_checks::backend(fixture), pid);
    let mut warm = Vec::new();
    for _ in 1..64 {
        let start = Instant::now();
        assert_eq!(authenticate(), Ok(Some(principal)));
        warm.push(start.elapsed());
    }
    assert_eq!(body_checks::backend(fixture), pid);
    warm.sort();
    eprintln!(
        "authentication reuse: one backend for 64 jobs; cold_us={} warm_median_us={} warm_p95_us={}",
        cold.as_micros(), warm[warm.len() / 2].as_micros(), warm[warm.len() * 95 / 100].as_micros(),
    );
    assert_eq!(authenticate(), Ok(Some(principal)));
    body_checks::closed(fixture, &pid);
    assert_ne!(body_checks::backend(fixture), pid);

    // A credential refusal retires the session but closes its open read
    // transaction gracefully, rather than flooding PostgreSQL's failure log.
    let failures = || {
        std::fs::read_to_string(fixture.root.join("server.log"))
            .unwrap()
            .lines()
            .filter(|line| {
                line.contains("Connection reset by peer")
                    || line.contains("unexpected EOF on client connection with an open transaction")
            })
            .count()
    };
    let before = failures();
    let pid = body_checks::backend(fixture);
    let mut wrong = password.to_vec();
    let index = wrong.len() - 5;
    wrong[index] = if wrong[index] == b'0' { b'1' } else { b'0' };
    assert_eq!(auth.authenticate("reader", &wrong, until()), Ok(None));
    body_checks::closed(fixture, &pid);
    assert_eq!(
        failures(),
        before,
        "credential refusal must close gracefully"
    );
    assert_eq!(authenticate(), Ok(Some(principal)));
    assert_ne!(body_checks::backend(fixture), pid);

    let generation = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.config_selection",
    );
    let epoch = sql(
        fixture,
        "SELECT encode(store_epoch,'hex') FROM td_mta.schema_migrations LIMIT 1",
    );
    let checksum = sql(
        fixture,
        "SELECT encode(checksum,'hex') FROM td_mta.schema_migrations WHERE version=1",
    );
    for (change, restore) in [
        (
            "UPDATE td_mta.config_selection SET generation=decode('000000000000ffff','hex')".to_owned(),
            format!("UPDATE td_mta.config_selection SET generation=decode('{generation}','hex')"),
        ),
        (
            "UPDATE td_mta.schema_migrations SET store_epoch=decode('eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee','hex')".into(),
            format!("UPDATE td_mta.schema_migrations SET store_epoch=decode('{epoch}','hex')"),
        ),
        (
            "UPDATE td_mta.schema_migrations SET checksum=decode(repeat('00',32),'hex') WHERE version=1".into(),
            format!("UPDATE td_mta.schema_migrations SET checksum=decode('{checksum}','hex') WHERE version=1"),
        ),
        ("ALTER ROLE mail_runtime CREATEDB".into(), "ALTER ROLE mail_runtime NOCREATEDB".into()),
        ("GRANT TRUNCATE ON td_mta.blobs TO mail_runtime".into(), "REVOKE TRUNCATE ON td_mta.blobs FROM mail_runtime".into()),
        ("UPDATE td_mta.runtime_barrier SET blocked=true".into(), "UPDATE td_mta.runtime_barrier SET blocked=false".into()),
        (
            "REVOKE EXECUTE ON FUNCTION pg_catalog.pg_advisory_unlock_shared(bigint) FROM PUBLIC".into(),
            "GRANT EXECUTE ON FUNCTION pg_catalog.pg_advisory_unlock_shared(bigint) TO PUBLIC".into(),
        ),
    ] {
        let pid = body_checks::backend(fixture);
        sql(fixture, &change);
        assert_eq!(authenticate(), Err(Error::Invalid), "{change}");
        body_checks::closed(fixture, &pid);
        assert_eq!(sql(fixture, &format!(
            "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
            td_mta::postgres_schema::MIGRATION_KEY,
        )), "t");
        sql(fixture, &restore);
        assert_eq!(authenticate(), Ok(Some(principal)));
        assert_ne!(body_checks::backend(fixture), pid);
    }
    let pid = body_checks::backend(fixture);
    sql(
        fixture,
        &format!("SELECT pg_catalog.pg_terminate_backend({pid})"),
    );
    assert_eq!(authenticate(), Err(Error::Invalid));
    body_checks::closed(fixture, &pid);
    assert_eq!(authenticate(), Ok(Some(principal)));
    assert_ne!(body_checks::backend(fixture), pid);
}
