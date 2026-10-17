use super::{create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    postgres_probe::{self, Mode},
    postgres_runtime_access as access, postgres_schema as schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_runtime_barrier_refuses_runtime_but_preserves_owner_inspection() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let receipt = schema::initialize(&owner, &trust, deadline()).unwrap();
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    let mut runtime = fixture.request();
    runtime.user = "mail_runtime";
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()).unwrap(),
        receipt
    );
    let mut serving = fixture.request();
    serving.user = "mail_runtime";
    serving.mode = Mode::Runtime;
    assert!(postgres_probe::probe(&serving, &trust, deadline()).is_ok());
    // No live application worker is present; these fixture writes are not an
    // implementation of the future fenced owner transition protocol.
    sql(&fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Maintenance)
    );
    assert_eq!(
        postgres_probe::probe(&serving, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeMaintenance)
    );
    assert_eq!(
        schema::inspect(&owner, &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(
        sql(&fixture, "SELECT blocked FROM td_mta.runtime_barrier"),
        "t"
    );
    sql(&fixture, "DELETE FROM td_mta.runtime_barrier");
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Maintenance)
    );
    assert_eq!(
        postgres_probe::probe(&serving, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeMaintenance)
    );
    assert_eq!(
        schema::inspect(&owner, &trust, deadline()).unwrap(),
        receipt
    );
    sql(&fixture,"INSERT INTO td_mta.runtime_barrier VALUES(true,false); GRANT UPDATE(blocked) ON td_mta.runtime_barrier TO mail_runtime");
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Privileges)
    );
    sql(
        &fixture,
        "REVOKE UPDATE(blocked) ON td_mta.runtime_barrier FROM mail_runtime",
    );
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()).unwrap(),
        receipt
    );
    assert!(postgres_probe::probe(&serving, &trust, deadline()).is_ok());
    assert_eq!(
        schema::inspect(&owner, &trust, deadline()).unwrap(),
        receipt
    );
}
