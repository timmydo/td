use super::{create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::StoreEpoch,
    postgres_probe::{self, Mode},
    postgres_replica_source::{self as replica, Error, Source},
    postgres_runtime_access as access, postgres_schema as schema,
};

pub(super) fn exercise() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let receipt = schema::initialize(&owner, &trust, deadline()).unwrap();
    let epoch = receipt.store_epoch;
    let source = Source::new(
        "https://SOURCE.example.test:443/",
        StoreEpoch::from_bytes([7; 16]),
    )
    .unwrap();
    assert_eq!(
        replica::inspect(&owner, &trust, deadline(), epoch).unwrap(),
        None
    );
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    let mut runtime = fixture.request();
    runtime.user = "mail_runtime";
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(
        replica::initialize(&runtime, &trust, deadline(), epoch, &source),
        Err(Error::Owner)
    );
    sql(&fixture, "CREATE ROLE mail_reader LOGIN PASSWORD 'disposable-postgres-fixture-password'; GRANT USAGE ON SCHEMA td_mta TO mail_reader; GRANT SELECT ON ALL TABLES IN SCHEMA td_mta TO mail_reader; GRANT UPDATE(singleton) ON td_mta.config_selection TO mail_reader; GRANT UPDATE(id) ON td_mta.accounts TO mail_reader; GRANT INSERT ON td_mta.leases TO mail_reader; GRANT INSERT,UPDATE,DELETE ON td_mta.application_logs,td_mta.application_log_state TO mail_reader");
    let mut reader = fixture.request();
    reader.user = "mail_reader";
    reader.mode = Mode::ReplicaRead;
    assert_eq!(
        postgres_probe::probe(&reader, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeReplica)
    );
    let same_epoch = Source::new(source.origin(), epoch).unwrap();
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &same_epoch),
        Err(Error::Epoch)
    );
    assert_eq!(
        replica::initialize(
            &owner,
            &trust,
            deadline(),
            StoreEpoch::from_bytes([9; 16]),
            &source
        ),
        Err(Error::Epoch)
    );
    sql(&fixture, "INSERT INTO td_mta.accounts VALUES(decode(repeat('01',16),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex'))");
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &source),
        Err(Error::Occupied)
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.replica_source"),
        "0"
    );
    sql(&fixture, "DELETE FROM td_mta.accounts");
    sql(&fixture, "INSERT INTO td_mta.config_versions VALUES(decode(repeat('01',16),'hex'),1,decode(repeat('00',32),'hex'),1); UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex'),version_id=decode(repeat('01',16),'hex')");
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &source),
        Err(Error::Occupied)
    );
    sql(&fixture, "UPDATE td_mta.config_selection SET generation=decode(repeat('00',8),'hex'),version_id=NULL; DELETE FROM td_mta.config_versions");
    sql(&fixture, "INSERT INTO td_mta.acme_profiles(slot,profile,settings_hash,revision,owner,lease_until,state) VALUES(0,'existing',decode(repeat('00',32),'hex'),1,decode(repeat('00',16),'hex'),clock_timestamp(),decode('00','hex'))");
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &source),
        Err(Error::Occupied)
    );
    sql(&fixture, "DELETE FROM td_mta.acme_profiles");
    let mut shared = super::hold_schema_lock(&fixture, true);
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &source),
        Err(Error::Schema(schema::Error::Busy))
    );
    super::wait_command(&mut shared);
    replica::initialize(&owner, &trust, deadline(), epoch, &source).unwrap();
    replica::initialize(&owner, &trust, deadline(), epoch, &source).unwrap();
    let observed = replica::inspect(&owner, &trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    assert_eq!(observed, source);
    assert_eq!(observed.origin(), "https://source.example.test");
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.replica_source"),
        "1"
    );
    let different = Source::new("https://another.example.test", source.epoch()).unwrap();
    assert_eq!(
        replica::initialize(&owner, &trust, deadline(), epoch, &different),
        Err(Error::Conflict)
    );
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Replica)
    );
    runtime.mode = Mode::Runtime;
    assert_eq!(
        postgres_probe::probe(&runtime, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeReplica)
    );
    assert_eq!(
        replica::inspect(&runtime, &trust, deadline(), epoch),
        Err(Error::Owner)
    );
    assert_eq!(
        schema::inspect(&owner, &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(sql(&fixture, "SELECT has_table_privilege('mail_runtime','td_mta.replica_source','SELECT'),has_any_column_privilege('mail_runtime','td_mta.replica_source','INSERT,UPDATE'),has_table_privilege('mail_runtime','td_mta.replica_source','DELETE')"), "t|f|f");
    assert!(postgres_probe::probe(&reader, &trust, deadline()).is_ok());
    assert_eq!(
        schema::inspect(&reader, &trust, deadline()).unwrap(),
        receipt
    );
    // A reader login cannot substitute for a primary runtime, nor vice versa.
    reader.mode = Mode::Runtime;
    assert_eq!(
        postgres_probe::probe(&reader, &trust, deadline()),
        Err(postgres_probe::Error::RuntimePrivileges)
    );
    reader.mode = Mode::ReplicaRead;
    runtime.mode = Mode::ReplicaRead;
    assert_eq!(
        postgres_probe::probe(&runtime, &trust, deadline()),
        Err(postgres_probe::Error::RuntimePrivileges)
    );
    runtime.mode = Mode::Runtime;
    for table in ["emails", "blob_chunks", "replica_progress", "submissions"] {
        sql(
            &fixture,
            &format!("GRANT UPDATE ON td_mta.{table} TO mail_reader"),
        );
        assert_eq!(
            postgres_probe::probe(&reader, &trust, deadline()),
            Err(postgres_probe::Error::RuntimePrivileges)
        );
        sql(
            &fixture,
            &format!("REVOKE UPDATE ON td_mta.{table} FROM mail_reader"),
        );
    }
    sql(
        &fixture,
        "GRANT UPDATE(sequence) ON td_mta.accounts TO mail_reader",
    );
    assert_eq!(
        postgres_probe::probe(&reader, &trust, deadline()),
        Err(postgres_probe::Error::RuntimePrivileges)
    );
    sql(&fixture, "REVOKE UPDATE(sequence) ON td_mta.accounts FROM mail_reader; UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        postgres_probe::probe(&reader, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeMaintenance)
    );
    sql(&fixture, "UPDATE td_mta.runtime_barrier SET blocked=false");
    assert!(postgres_probe::probe(&reader, &trust, deadline()).is_ok());
    sql(&fixture, "UPDATE td_mta.replica_source SET source_epoch=(SELECT store_epoch FROM td_mta.schema_migrations ORDER BY version LIMIT 1)");
    assert_eq!(
        postgres_probe::probe(&reader, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeReplica)
    );
    sql(
        &fixture,
        "UPDATE td_mta.replica_source SET source_epoch=decode(repeat('07',16),'hex')",
    );
    assert!(postgres_probe::probe(&reader, &trust, deadline()).is_ok());
    executable_refuses_service(&fixture, epoch);
    // Corrupt spelling is not normalized into a successful persisted identity.
    sql(
        &fixture,
        "UPDATE td_mta.replica_source SET origin='https://SOURCE.example.test'",
    );
    assert_eq!(
        replica::inspect(&owner, &trust, deadline(), epoch),
        Err(Error::Integrity)
    );
    runtime.mode = Mode::Preparation;
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Database)
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.accounts"), "0");
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_replica_source_mode() {
    exercise();
}

fn executable_refuses_service(fixture: &Fixture, epoch: StoreEpoch) {
    use std::{fs, net::TcpListener, os::unix::fs::PermissionsExt, process::Command};
    let ca = fixture.root.join("replica-ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("replica-runtime");
    fs::write(
        &connection,
        format!(
            "postgresql://mail_runtime:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(super::PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let serve = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["serve", "--connection-file"])
        .arg(&connection)
        .args(["--profile", "cloud", "--runtime"])
        .arg(fixture.root.join("unused-runtime"))
        .arg("--scratch")
        .arg(fixture.root.join("unused-scratch"))
        .output()
        .unwrap();
    assert!(!serve.status.success());
    assert!(
        String::from_utf8_lossy(&serve.stdout).contains("runtime-qualification"),
        "{}",
        String::from_utf8_lossy(&serve.stdout)
    );
    // An occupied port would produce http01-bind if listener startup ran first.
    let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let acme = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["acme", "--connection-file"])
        .arg(&connection)
        .args([
            "--certificate-profile",
            "replica",
            "--directory",
            "https://ca.example.test/directory",
            "--contact",
            "owner@example.test",
            "--name",
            "mail.example.test",
            "--bind",
        ])
        .arg(occupied.local_addr().unwrap().to_string())
        .arg("--accept-terms")
        .output()
        .unwrap();
    assert!(!acme.status.success());
    assert!(
        String::from_utf8_lossy(&acme.stdout).contains("runtime-claim"),
        "{}",
        String::from_utf8_lossy(&acme.stdout)
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.acme_profiles"),
        "0"
    );
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(super::PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    let inspect = Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["postgres", "replica-source-inspect", "--connection-file"])
        .arg(&connection)
        .arg("--epoch")
        .arg(epoch.to_string())
        .output()
        .unwrap();
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stdout)
    );
    let report = String::from_utf8(inspect.stdout).unwrap();
    assert!(report.contains("https://source.example.test"));
    assert!(report.contains("\"copy_complete\":false"));
}
