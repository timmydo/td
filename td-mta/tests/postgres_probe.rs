#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::{
    fs::{self, File},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use td_crypto::{Crypto, Provider, TrustStore, P256_PKCS8_CAPACITY};
use td_fs::TempDir;
use td_mta::postgres_probe::{probe, Error, Request};

#[path = "support/tls_certificate_fixture.rs"]
mod certificate;

const PASSWORD: &[u8] = b"disposable-postgres-fixture-password";

struct Fixture {
    root: TempDir,
    bin: PathBuf,
    server: Option<Child>,
    port: u16,
    ca: Vec<u8>,
}
impl Fixture {
    fn new() -> Self {
        let bin = PathBuf::from(std::env::var_os("TD_MTA_TEST_POSTGRES_BIN").unwrap());
        assert!(bin.is_absolute());
        let root = TempDir::new("td-postgres-probe").unwrap();
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut fixture = Self {
            root,
            bin,
            server: None,
            port,
            ca: Vec::new(),
        };
        fs::write(fixture.root.join("password"), PASSWORD).unwrap();
        fixture.run(
            "initdb",
            &[
                "-D",
                fixture.data().to_str().unwrap(),
                "-U",
                "td_probe",
                "--pwfile",
                fixture.root.join("password").to_str().unwrap(),
                "--auth=scram-sha-256",
                "--no-locale",
                "-E",
                "UTF8",
            ],
        );
        let provider = Provider;
        let mut raw = [0; P256_PKCS8_CAPACITY];
        let count = provider.generate_p256(&mut raw).unwrap();
        let root_key = provider.load_p256(&raw[..count]).unwrap();
        fixture.ca = certificate::pem(
            "CERTIFICATE",
            &certificate::certificate_names(&root_key, &root_key, true, 1, &[], false),
        );
        let count = provider.generate_p256(&mut raw).unwrap();
        let key = provider.load_p256(&raw[..count]).unwrap();
        let leaf = certificate::pem(
            "CERTIFICATE",
            &certificate::certificate_names(&key, &root_key, false, 2, &["localhost"], false),
        );
        fs::write(fixture.root.join("server.crt"), leaf).unwrap();
        fs::write(
            fixture.root.join("server.key"),
            certificate::pem("PRIVATE KEY", &raw[..count]),
        )
        .unwrap();
        fs::set_permissions(
            fixture.root.join("server.key"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(
            fixture.data().join("pg_hba.conf"),
            "host all all 127.0.0.1/32 scram-sha-256\n",
        )
        .unwrap();
        fixture.start(true, false);
        fixture
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(self.bin.join(program));
        command
            .env_clear()
            .env("PATH", &self.bin)
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        command
    }

    fn run(&self, program: &str, arguments: &[&str]) {
        let log = File::create(self.root.join("command.log")).unwrap();
        let mut child = self
            .command(program)
            .args(arguments)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        // initdb synchronizes a fresh cluster; concurrent host builds can
        // delay that filesystem work without changing any client deadline.
        let seconds = if program == "initdb" { 60 } else { 20 };
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{program}: {:?}", status.code());
                return;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{program} timed out");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn start(&mut self, tls: bool, read_only: bool) {
        assert!(self.server.is_none());
        fs::write(
            self.data().join("postgresql.conf"),
            format!(
                "listen_addresses='127.0.0.1'\nport={}\n\
                 unix_socket_directories=''\nssl={}\n\
                 ssl_cert_file='{}'\nssl_key_file='{}'\n\
                 default_transaction_read_only={}\n",
                self.port,
                if tls { "on" } else { "off" },
                self.root.join("server.crt").display(),
                self.root.join("server.key").display(),
                if read_only { "on" } else { "off" },
            ),
        )
        .unwrap();
        let log_path = self.root.join("server.log");
        let log = File::create(&log_path).unwrap();
        self.server = Some(
            self.command("postgres")
                .arg("-D")
                .arg(self.data())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(self.server.as_mut().unwrap().try_wait().unwrap().is_none());
            if fs::read_to_string(&log_path)
                .unwrap()
                .contains("database system is ready to accept connections")
            {
                break;
            }
            assert!(Instant::now() < deadline, "PostgreSQL startup timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(&mut self) {
        if self.server.is_some() {
            self.run(
                "pg_ctl",
                &[
                    "-D",
                    self.data().to_str().unwrap(),
                    "-m",
                    "immediate",
                    "-w",
                    "stop",
                ],
            );
            self.server.take().unwrap().wait().unwrap();
        }
    }

    fn request(&self) -> Request<'_> {
        Request {
            address: ([127, 0, 0, 1], self.port).into(),
            server_name: "localhost",
            user: "td_probe",
            database: "postgres",
            password: PASSWORD,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            // Only our retained child is signalled on a failed test.
            let _ = server.kill();
            let _ = server.wait();
        }
    }
}

struct ProbeChild(Child);
impl Drop for ProbeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sql_command(fixture: &Fixture) -> Command {
    let ca = fixture.root.join("sql-ca.pem");
    if !ca.exists() {
        fs::write(&ca, &fixture.ca).unwrap();
    }
    let mut command = fixture.command("psql");
    command
        .env("PGPASSWORD", std::str::from_utf8(PASSWORD).unwrap())
        .env("PGSSLMODE", "verify-full")
        .env("PGSSLROOTCERT", ca)
        .env("PGAPPNAME", "td-mta-schema-fixture")
        .args([
            "-X",
            "-A",
            "-t",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            "localhost",
            "-U",
            "td_probe",
            "-d",
            "postgres",
            "-p",
        ])
        .arg(fixture.port.to_string());
    command
}

fn wait_command(child: &mut ProbeChild) {
    let stop = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            return;
        }
        assert!(Instant::now() < stop, "SQL fixture command timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn sql(fixture: &Fixture, query: &str) -> String {
    let path = fixture.root.join("sql-result");
    let output = File::create(&path).unwrap();
    let mut child = ProbeChild(
        sql_command(fixture)
            .args(["-c", query])
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    wait_command(&mut child);
    assert!(fs::metadata(&path).unwrap().len() < 4096);
    fs::read_to_string(path).unwrap().trim().to_owned()
}

fn hold_schema_lock(fixture: &Fixture, shared: bool) -> ProbeChild {
    let function = if shared {
        "pg_advisory_lock_shared"
    } else {
        "pg_advisory_lock"
    };
    let query = format!(
        "SELECT pg_catalog.{function}({}::bigint)",
        td_mta::postgres_schema::MIGRATION_KEY
    );
    let mut child = ProbeChild(
        sql_command(fixture)
            .args(["-c", &query, "-c", "SELECT pg_catalog.pg_sleep(3)"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stop = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(child.0.try_wait().unwrap().is_none());
        if sql(fixture, "SELECT count(*) FROM pg_catalog.pg_locks l JOIN pg_catalog.pg_stat_activity a ON l.pid = a.pid WHERE a.application_name = 'td-mta-schema-fixture' AND l.locktype = 'advisory' AND l.granted") == "1" { return child; }
        assert!(Instant::now() < stop, "schema lock was not acquired");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_schema_initialization_is_atomic_and_locked() {
    use td_mta::postgres_schema::{initialize, inspect, Error as SchemaError};
    let mut fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    sql(&fixture, "CREATE SCHEMA td_mta");
    assert_eq!(
        initialize(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Exists)
    );
    sql(&fixture, "DROP SCHEMA td_mta; CREATE ROLE outsider; ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO outsider");
    assert_eq!(
        initialize(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Invalid)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM pg_catalog.pg_namespace WHERE nspname = 'td_mta'"
        ),
        "0"
    );
    sql(
        &fixture,
        "ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM outsider",
    );
    let mut shared = hold_schema_lock(&fixture, true);
    assert_eq!(
        initialize(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Busy)
    );
    wait_command(&mut shared);
    let receipt = initialize(&fixture.request(), &trust, deadline()).unwrap();
    assert_eq!(receipt.version, 13);
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(
        initialize(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Exists)
    );
    let mut shared = hold_schema_lock(&fixture, true);
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    wait_command(&mut shared);
    let mut exclusive = hold_schema_lock(&fixture, false);
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Busy)
    );
    wait_command(&mut exclusive);
    fixture.stop();
    fixture.start(true, false);
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    sql(
        &fixture,
        "UPDATE td_mta.schema_migrations SET version = 99 WHERE version = 2",
    );
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Version)
    );
    sql(&fixture, "UPDATE td_mta.schema_migrations SET version = 2, checksum = pg_catalog.decode(pg_catalog.repeat('00',32), 'hex') WHERE version = 99");
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Version)
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_schema_commit_timeout_reports_uncertainty() {
    use td_mta::postgres_schema::{initialize, inspect, Error as SchemaError};
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    sql(&fixture, "CREATE FUNCTION public.delay_schema_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.version = 1 THEN PERFORM pg_catalog.pg_sleep(6); END IF; RETURN NEW; END $$;
CREATE FUNCTION public.install_schema_delay() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger WHERE tgname = 'td_delay') THEN EXECUTE 'CREATE CONSTRAINT TRIGGER td_delay AFTER INSERT ON td_mta.schema_migrations DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_schema_commit()'; END IF; END $$;
CREATE EVENT TRIGGER td_schema_delay ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION public.install_schema_delay()");
    assert_eq!(
        initialize(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(2)
        ),
        Err(SchemaError::Indeterminate)
    );
    // COMMIT can finish after the client deadline/disconnect. Wait for the
    // original session to end, then inspect rather than assuming rollback.
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name = 'td-mta-postgres'") != "0" {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM pg_catalog.pg_namespace WHERE nspname = 'td_mta'"
        ),
        "1"
    );
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline())
            .unwrap()
            .version,
        13
    );
    assert_eq!(
        initialize(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Exists)
    );
}

fn command_probe(fixture: &Fixture, expected: bool) -> String {
    let connection = fixture.root.join("connection");
    command_output(
        &[
            "postgres",
            "probe",
            "--connection-file",
            connection.to_str().unwrap(),
            "--profile",
            "desktop",
        ],
        fixture.root.path(),
        if expected { 0 } else { 1 },
    )
}

fn command_output(arguments: &[&str], root: &std::path::Path, expected: i32) -> String {
    let output_path = root.join("probe-output");
    let output = File::create(&output_path).unwrap();
    let mut child = ProbeChild(
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .env_clear()
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let stop = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(expected));
            break;
        }
        assert!(Instant::now() < stop, "probe command timed out");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fs::metadata(&output_path).unwrap().len() < 1024);
    let text = fs::read_to_string(output_path).unwrap();
    assert!(!text.contains(std::str::from_utf8(PASSWORD).unwrap()));
    assert!(!text.contains("postgresql://"));
    text
}

#[test]
fn command_refusals_have_fixed_output_without_argument_contents() {
    let root = TempDir::new("pg-command-refusal").unwrap();
    assert_eq!(command_output(&["postgres", "probe", "--password", "not-for-output"], root.path(), 2),
        "{\"schema\":1,\"command\":\"postgres-probe\",\"status\":\"error\",\"stage\":\"arguments\",\"error\":\"invalid-arguments\"}\n");
    assert_eq!(command_output(&["postgres", "probe", "--connection-file", "not-for-output", "--profile", "desktop"], root.path(), 1),
        "{\"schema\":1,\"command\":\"postgres-probe\",\"status\":\"error\",\"stage\":\"bootstrap\",\"error\":\"postgres-bootstrap-root\"}\n");
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_probe_command_loads_resolves_and_authenticates() {
    let mut fixture = Fixture::new();
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let path = fixture.root.join("connection");
    let uri = format!(
        "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
        std::str::from_utf8(PASSWORD).unwrap(),
        fixture.port,
        ca.display()
    );
    fs::write(&path, &uri).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let output = command_probe(&fixture, true);
    assert!(output.contains("\"schema_checked\":false,\"profile_checked\":false"));
    assert!(output.contains("\"server_version\":"));
    let wrong = uri.replace(std::str::from_utf8(PASSWORD).unwrap(), "wrong-password");
    fs::write(&path, wrong).unwrap();
    assert!(command_probe(&fixture, false).contains("connection-or-authentication"));
    fs::write(&path, &uri).unwrap();
    fixture.stop();
    fixture.start(true, true);
    assert!(command_probe(&fixture, false).contains("primary-or-durability-settings"));
    fixture.stop();
    fixture.start(true, false);
    assert!(command_probe(&fixture, true).contains("\"status\":\"ok\""));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(command_probe(&fixture, false).contains("input-private-mode"));
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_schema_commands_prepare_and_inspect_one_epoch() {
    let fixture = Fixture::new();
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("connection");
    let uri = format!(
        "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
        std::str::from_utf8(PASSWORD).unwrap(),
        fixture.port,
        ca.display()
    );
    fs::write(&connection, uri).unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let invoke = |operation, status| {
        command_output(
            &[
                "postgres",
                operation,
                "--connection-file",
                connection.to_str().unwrap(),
            ],
            fixture.root.path(),
            status,
        )
    };
    assert!(invoke("schema-inspect", 1).contains("schema-operation-failed"));
    let initialized = invoke("schema-init", 0);
    assert!(initialized.contains("\"schema_version\":13"));
    assert!(initialized.contains(
        "\"ledger_checked\":true,\"serving_schema_checked\":false,\"profile_checked\":false"
    ));
    let inspected = invoke("schema-inspect", 0);
    assert_eq!(
        initialized.replace("postgres-schema-init", "postgres-schema-inspect"),
        inspected
    );
    assert_eq!(
        invoke("schema-upgrade", 0),
        initialized.replace("postgres-schema-init", "postgres-schema-upgrade")
    );
    assert!(invoke("schema-init", 1).contains("namespace-exists"));
    let mut held = hold_schema_lock(&fixture, false);
    assert!(invoke("schema-inspect", 1).contains("migration-busy"));
    wait_command(&mut held);
    sql(
        &fixture,
        "UPDATE td_mta.schema_migrations SET version = 99 WHERE version = 2",
    );
    assert!(invoke("schema-inspect", 1).contains("unsupported-version-or-checksum"));
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(invoke("schema-init", 1).contains("input-private-mode"));
}

#[test]
fn schema_commands_refuse_profiles_and_secret_arguments_without_echoing_them() {
    let root = TempDir::new("pg-schema-command-refusal").unwrap();
    for operation in ["schema-init", "schema-inspect", "schema-upgrade"] {
        for flags in [
            vec!["--password", "private-secret"],
            vec![
                "--connection-file",
                "private-path",
                "--profile",
                "private-profile",
            ],
            vec![
                "--connection-file",
                "private-path",
                "--connection-file",
                "other-path",
            ],
        ] {
            let mut args = vec!["postgres", operation];
            args.extend(flags);
            let text = command_output(&args, root.path(), 2);
            assert!(text.contains("invalid-arguments"));
            assert!(text.contains(&format!("postgres-{operation}")));
            assert!(!text.contains("private-"));
        }
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
#[ignore = "requires TD_MTA_TEST_POSTGRES_BIN pointing to an independently installed PostgreSQL"]
fn real_postgres_requires_tls_scram_primary_settings_and_reconnects() {
    let mut fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let status = probe(&fixture.request(), &trust, deadline()).unwrap();
    assert!(status.server_version >= 170000);

    let mut wrong = fixture.request();
    wrong.server_name = "wrong.invalid";
    assert_eq!(probe(&wrong, &trust, deadline()), Err(Error::Database));
    wrong = fixture.request();
    wrong.password = b"wrong";
    assert_eq!(probe(&wrong, &trust, deadline()), Err(Error::Database));
    assert_eq!(
        probe(
            &fixture.request(),
            &TrustStore::public_roots().unwrap(),
            deadline()
        ),
        Err(Error::Database)
    );
    assert_eq!(
        probe(&fixture.request(), &trust, Instant::now()),
        Err(Error::Deadline)
    );

    fixture.stop();
    assert_eq!(
        probe(&fixture.request(), &trust, deadline()),
        Err(Error::Transport)
    );
    fixture.start(false, false);
    assert_eq!(
        probe(&fixture.request(), &trust, deadline()),
        Err(Error::Database)
    );
    fixture.stop();
    fixture.start(true, true);
    assert_eq!(
        probe(&fixture.request(), &trust, deadline()),
        Err(Error::Settings)
    );
    fixture.stop();
    for method in ["trust", "password"] {
        fs::write(
            fixture.data().join("pg_hba.conf"),
            format!("host all all 127.0.0.1/32 {method}\n"),
        )
        .unwrap();
        fixture.start(true, false);
        assert_eq!(
            probe(&fixture.request(), &trust, deadline()),
            Err(Error::Database)
        );
        fixture.stop();
    }
    fs::write(
        fixture.data().join("pg_hba.conf"),
        "host all all 127.0.0.1/32 scram-sha-256\n",
    )
    .unwrap();
    fixture.start(true, false);
    assert_eq!(
        probe(&fixture.request(), &trust, deadline()).unwrap(),
        status
    );
    fixture.stop();
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_material_schema_upgrade_preserves_epoch_and_rolls_back() {
    use td_crypto::Digest;
    use td_mta::postgres_schema::{inspect, upgrade, Error as SchemaError};
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let ddl = include_str!("../src/postgres_schema.sql");
    let mut hash = Provider.sha256().unwrap();
    hash.update(ddl.as_bytes()).unwrap();
    let digest: String = hash
        .finish()
        .unwrap()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    sql(&fixture, &format!("{ddl} INSERT INTO td_mta.schema_migrations VALUES (1, pg_catalog.decode('{digest}','hex'), pg_catalog.decode(pg_catalog.repeat('01',16),'hex'))"));
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Version)
    );
    sql(&fixture, "CREATE ROLE material_outsider; ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO material_outsider");
    assert_eq!(
        upgrade(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Invalid)
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.schema_migrations"),
        "1"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'td_mta' AND c.relname = 'config_materials'"), "0");
    sql(
        &fixture,
        "ALTER DEFAULT PRIVILEGES REVOKE SELECT ON TABLES FROM material_outsider",
    );
    let mut held = hold_schema_lock(&fixture, true);
    assert_eq!(
        upgrade(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Busy)
    );
    wait_command(&mut held);
    let receipt = upgrade(&fixture.request(), &trust, deadline()).unwrap();
    assert_eq!(receipt.version, 13);
    assert_eq!(receipt.store_epoch.as_bytes(), &[1; 16]);
    assert_eq!(
        upgrade(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()).unwrap(),
        receipt
    );
    sql(&fixture, "UPDATE td_mta.schema_migrations SET store_epoch = pg_catalog.decode(pg_catalog.repeat('02',16),'hex') WHERE version = 2");
    assert_eq!(
        inspect(&fixture.request(), &trust, deadline()),
        Err(SchemaError::Version)
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_materials_round_trip_bounds_immutability_and_integrity() {
    use td_mta::{
        ids::StoreEpoch,
        postgres_materials::{self as materials, Kind, Version},
        postgres_schema,
    };
    let mut fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    for kind in [
        Kind::RelayPassword,
        Kind::CertificateChain,
        Kind::PrivateKey,
        Kind::TrustBundle,
        Kind::TextSignature,
        Kind::HtmlSignature,
    ] {
        let bytes: Vec<u8> = (0..kind.max_bytes()).map(|i| (i % 251) as u8).collect();
        let version = Version::from_bytes([kind as u8; 16]);
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            version,
            kind,
            &bytes,
        )
        .unwrap();
        let value =
            materials::read(&fixture.request(), &trust, deadline(), epoch, version, kind).unwrap();
        assert_eq!(value.kind(), kind);
        assert_eq!(value.bytes(), bytes);
        assert_eq!(
            materials::insert(
                &fixture.request(),
                &trust,
                deadline(),
                epoch,
                version,
                kind,
                b"replacement"
            ),
            Err(materials::Error::Database)
        );
        assert!(matches!(
            materials::read(
                &fixture.request(),
                &trust,
                deadline(),
                epoch,
                version,
                if kind == Kind::PrivateKey {
                    Kind::TrustBundle
                } else {
                    Kind::PrivateKey
                }
            ),
            Err(materials::Error::Kind)
        ));
        assert_eq!(
            materials::read(&fixture.request(), &trust, deadline(), epoch, version, kind)
                .unwrap()
                .bytes(),
            bytes
        );
    }
    let empty = materials::fresh_version().unwrap();
    materials::insert(
        &fixture.request(),
        &trust,
        deadline(),
        epoch,
        empty,
        Kind::TextSignature,
        b"",
    )
    .unwrap();
    assert!(materials::read(
        &fixture.request(),
        &trust,
        deadline(),
        epoch,
        empty,
        Kind::TextSignature
    )
    .unwrap()
    .bytes()
    .is_empty());
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            empty,
            Kind::PrivateKey,
            b""
        ),
        Err(materials::Error::Invalid)
    );
    let absent = materials::fresh_version().unwrap();
    assert!(matches!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            absent,
            Kind::PrivateKey
        ),
        Err(materials::Error::Missing)
    ));
    let stale = StoreEpoch::from_bytes([42; 16]);
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            stale,
            absent,
            Kind::PrivateKey,
            b"private"
        ),
        Err(materials::Error::Epoch)
    );
    assert!(matches!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            stale,
            empty,
            Kind::TextSignature
        ),
        Err(materials::Error::Epoch)
    ));
    fixture.stop();
    fixture.start(true, false);
    let trust_version = Version::from_bytes([Kind::TrustBundle as u8; 16]);
    assert_eq!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            trust_version,
            Kind::TrustBundle
        )
        .unwrap()
        .bytes()
        .len(),
        131072
    );
    sql(&fixture, "UPDATE td_mta.config_material_chunks SET bytes = pg_catalog.set_byte(bytes,0,255) WHERE material_id = pg_catalog.decode(pg_catalog.repeat('04',16),'hex') AND ordinal = 1");
    assert!(matches!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            trust_version,
            Kind::TrustBundle
        ),
        Err(materials::Error::Integrity)
    ));
    sql(&fixture, "DELETE FROM td_mta.config_material_chunks WHERE material_id = pg_catalog.decode(pg_catalog.repeat('02',16),'hex')");
    assert!(matches!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            Version::from_bytes([2; 16]),
            Kind::CertificateChain
        ),
        Err(materials::Error::Integrity)
    ));
    let mut held = hold_schema_lock(&fixture, false);
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            absent,
            Kind::PrivateKey,
            b"private"
        ),
        Err(materials::Error::Schema(postgres_schema::Error::Busy))
    );
    wait_command(&mut held);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_materials_roll_back_chunks_and_preserve_uncertain_version() {
    use td_mta::{
        postgres_materials::{self as materials, Kind, Version},
        postgres_schema,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    sql(&fixture, "CREATE FUNCTION public.reject_second_chunk() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.ordinal = 1 THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_chunk BEFORE INSERT ON td_mta.config_material_chunks FOR EACH ROW EXECUTE FUNCTION public.reject_second_chunk()");
    let failed = Version::from_bytes([7; 16]);
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            failed,
            Kind::TrustBundle,
            &vec![42; 131072]
        ),
        Err(materials::Error::Database)
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_materials"),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.config_material_chunks"
        ),
        "0"
    );
    sql(&fixture, "DROP TRIGGER reject_chunk ON td_mta.config_material_chunks; CREATE FUNCTION public.delay_material_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_material AFTER INSERT ON td_mta.config_materials DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_material_commit()");
    let uncertain = Version::from_bytes([8; 16]);
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(2),
            epoch,
            uncertain,
            Kind::PrivateKey,
            b"opaque private material"
        ),
        Err(materials::Error::Indeterminate)
    );
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name = 'td-mta-postgres'") != "0" {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        materials::read(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            uncertain,
            Kind::PrivateKey
        )
        .unwrap()
        .bytes(),
        b"opaque private material"
    );
    assert_eq!(
        materials::insert(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            uncertain,
            Kind::PrivateKey,
            b"different"
        ),
        Err(materials::Error::Database)
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_materials"),
        "1"
    );
}

#[test]
fn material_commands_keep_versions_explicit_and_refusals_redacted() {
    use td_mta::postgres_materials::Version;
    let root = TempDir::new("pg-material-arguments").unwrap();
    let text = command_output(&["postgres", "material-version"], root.path(), 0);
    let version = text
        .split("\"version\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(Version::parse(version).unwrap().to_string(), version);
    for operation in ["material-version", "material-import", "material-check"] {
        let output = command_output(
            &["postgres", operation, "--password", "private-secret"],
            root.path(),
            2,
        );
        assert!(output.contains("invalid-arguments"));
        assert!(!output.contains("private-secret"));
    }
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_material_commands_import_verify_and_resolve_retained_version() {
    use td_mta::{ids::StoreEpoch, postgres_schema};
    let fixture = Fixture::new();
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("connection");
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let input = fixture.root.join("material");
    fs::write(&input, b"private material bytes").unwrap();
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();
    let invoke = |operation, kind, path: &std::path::Path, epoch: StoreEpoch, version, status| {
        let text = command_output(
            &[
                "postgres",
                operation,
                "--connection-file",
                connection.to_str().unwrap(),
                "--input-file",
                path.to_str().unwrap(),
                "--kind",
                kind,
                "--epoch",
                &epoch.to_string(),
                "--version",
                version,
            ],
            fixture.root.path(),
            status,
        );
        assert!(!text.contains("private material bytes"));
        text
    };
    const VERSION: &str = "01010101010101010101010101010101";
    let imported = invoke("material-import", "private-key", &input, epoch, VERSION, 0);
    assert!(imported.contains("\"content_validated\":false,\"profile_activated\":false"));
    assert!(invoke("material-check", "private-key", &input, epoch, VERSION, 0).contains(VERSION));
    assert!(
        invoke("material-import", "private-key", &input, epoch, VERSION, 1)
            .contains("material-operation-failed")
    );
    fs::write(&input, b"different bytes").unwrap();
    assert!(
        invoke("material-check", "private-key", &input, epoch, VERSION, 1)
            .contains("material-integrity-or-content-mismatch")
    );
    assert!(invoke(
        "material-check",
        "relay-password",
        &input,
        epoch,
        VERSION,
        1
    )
    .contains("material-kind-mismatch"));
    assert!(invoke(
        "material-check",
        "private-key",
        &input,
        StoreEpoch::from_bytes([0; 16]),
        VERSION,
        1
    )
    .contains("store-epoch-mismatch"));
    assert!(invoke(
        "material-check",
        "private-key",
        &input,
        epoch,
        "02020202020202020202020202020202",
        1
    )
    .contains("material-missing"));
    fs::set_permissions(&input, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        invoke("material-import", "private-key", &input, epoch, VERSION, 1)
            .contains("input-private-mode")
    );
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(invoke(
        "material-import",
        "trust-bundle",
        &connection,
        epoch,
        VERSION,
        1
    )
    .contains("input-secret-alias"));
    let alias = fixture.root.join("alias");
    fs::hard_link(&connection, &alias).unwrap();
    assert!(
        invoke("material-import", "trust-bundle", &alias, epoch, VERSION, 1)
            .contains("input-secret-alias")
    );
    fs::remove_file(&alias).unwrap();
    fs::write(&input, vec![b'x'; 131072]).unwrap();
    const TRUST: &str = "03030303030303030303030303030303";
    invoke("material-import", "trust-bundle", &input, epoch, TRUST, 0);
    invoke("material-check", "trust-bundle", &input, epoch, TRUST, 0);
    fs::write(&input, vec![b'x'; 131073]).unwrap();
    assert!(
        invoke("material-import", "trust-bundle", &input, epoch, TRUST, 1)
            .contains("postgres-bootstrap-capacity")
    );
    fs::write(&input, b"").unwrap();
    const EMPTY: &str = "04040404040404040404040404040404";
    invoke("material-import", "text-signature", &input, epoch, EMPTY, 0);
    invoke("material-check", "text-signature", &input, epoch, EMPTY, 0);
    assert!(
        invoke("material-import", "private-key", &input, epoch, EMPTY, 1)
            .contains("invalid-material")
    );
    // The ten-second client budget expires while the server commits. The
    // caller already owns the identity and resolves it after backend exit.
    sql(&fixture, "CREATE FUNCTION public.delay_material_command() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(12); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER material_command_delay AFTER INSERT ON td_mta.config_materials DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_material_command()");
    fs::write(&input, b"private material bytes").unwrap();
    const UNCERTAIN: &str = "05050505050505050505050505050505";
    assert!(invoke(
        "material-import",
        "private-key",
        &input,
        epoch,
        UNCERTAIN,
        1
    )
    .contains("indeterminate-do-not-retry"));
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name = 'td-mta-postgres'") != "0" {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    invoke("material-check", "private-key", &input, epoch, UNCERTAIN, 0);
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_materials"),
        "4"
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_mail_schema_bounds_references_and_rollback() {
    use td_mta::postgres_schema;
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    postgres_schema::initialize(&fixture.request(), &trust, deadline()).unwrap();
    sql(&fixture, "CREATE DATABASE non_utf8 TEMPLATE template0 ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C'");
    let mut non_utf8 = fixture.request();
    non_utf8.database = "non_utf8";
    assert_eq!(
        postgres_schema::initialize(&non_utf8, &trust, deadline()),
        Err(postgres_schema::Error::Version)
    );
    let a = "decode(repeat('01',16),'hex')";
    let b = "decode(repeat('02',16),'hex')";
    let zero = "decode(repeat('00',8),'hex')";
    let high = "decode('8000000000000000','hex')";
    let max = "decode(repeat('ff',8),'hex')";
    let digest = "decode(repeat('00',32),'hex')";
    sql(&fixture, &format!("INSERT INTO td_mta.accounts VALUES ({a},{max},{high}),({b},{zero},{zero}); INSERT INTO td_mta.blob_ids VALUES ({a},{a}),({a},{b})"));
    assert_eq!(sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='td_mta' AND c.relkind='r' AND c.relpersistence<>'p'"), "0");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='td_mta' AND c.relkind='r'"), "31");
    // Each expected refusal rolls back its own subtransaction. An unexpected
    // success raises a different SQLSTATE and fails psql with ON_ERROR_STOP.
    let refuse = |statement: String, state: &str| {
        sql(&fixture, &format!("DO $$ BEGIN BEGIN {statement}; SET CONSTRAINTS ALL IMMEDIATE; RAISE EXCEPTION 'unexpected success'; EXCEPTION WHEN SQLSTATE '{state}' THEN NULL; END; END $$"));
    };
    refuse(
        format!("UPDATE td_mta.accounts SET sequence={zero} WHERE id={a}"),
        "23514",
    );
    refuse(
        format!("INSERT INTO td_mta.blobs VALUES ({b},{a},0,{digest},0,{zero},0)"),
        "23503",
    );
    refuse(
        format!("INSERT INTO td_mta.blobs VALUES ({a},{a},33554433,{digest},0,{zero},513)"),
        "23514",
    );
    refuse(
        format!("INSERT INTO td_mta.blobs VALUES ({a},{a},65537,{digest},0,{zero},1)"),
        "23514",
    );
    sql(&fixture, &format!("INSERT INTO td_mta.blobs VALUES ({a},{a},33554432,{digest},0,{zero},512),({a},{b},0,{digest},0,{zero},0)"));
    sql(&fixture, &format!("INSERT INTO td_mta.blob_chunks SELECT {a},{a},n,decode(repeat('42',65536),'hex') FROM generate_series(0,511) n"));
    assert_eq!(sql(&fixture, "SELECT count(*),sum(octet_length(bytes)),max(octet_length(bytes)) FROM td_mta.blob_chunks"), "512|33554432|65536");
    refuse(
        format!("INSERT INTO td_mta.blob_chunks VALUES ({a},{b},512,decode('42','hex'))"),
        "23514",
    );
    refuse(
        format!("INSERT INTO td_mta.blob_chunks VALUES ({a},{b},0,decode('','hex'))"),
        "23514",
    );
    refuse(
        format!(
            "INSERT INTO td_mta.blob_chunks VALUES ({a},{b},0,decode(repeat('42',65537),'hex'))"
        ),
        "23514",
    );
    refuse(
        format!("INSERT INTO td_mta.blob_chunks VALUES ({b},{a},0,decode('42','hex'))"),
        "23503",
    );
    refuse(
        format!("INSERT INTO td_mta.blob_chunks VALUES ({a},{a},0,decode('42','hex'))"),
        "23505",
    );
    // Build children before parents, then force all deferred checks.
    sql(&fixture, &format!("BEGIN; INSERT INTO td_mta.memberships VALUES ({a},{a},{a},{zero}); INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES ({a},{a},{a},{a},0,2,{zero}); INSERT INTO td_mta.mailboxes VALUES ({a},{a},'Inbox',NULL,'inbox',0,1,{zero}); INSERT INTO td_mta.threads VALUES ({a},{a},{zero}); SET CONSTRAINTS ALL IMMEDIATE; COMMIT"));
    refuse(
        format!("INSERT INTO td_mta.memberships VALUES ({b},{a},{a},{zero})"),
        "23503",
    );
    refuse(format!("INSERT INTO td_mta.mailboxes VALUES ({b},{b},'bad',NULL,NULL,0,1,{zero}); INSERT INTO td_mta.memberships VALUES ({b},{b},{b},{zero})"), "23503");
    assert_eq!(
        sql(
            &fixture,
            &format!("SELECT count(*) FROM td_mta.mailboxes WHERE account={b}")
        ),
        "0"
    );
    refuse(
        format!("INSERT INTO td_mta.keywords VALUES ({a},{a},repeat('é',128),{zero})"),
        "23514",
    );
    sql(&fixture, &format!("INSERT INTO td_mta.keywords VALUES ({a},{a},'é',{zero}),({a},{a},'z',{zero}),({a},{a},'A',{zero})"));
    assert_eq!(
        sql(
            &fixture,
            "SELECT string_agg(keyword,',' ORDER BY keyword) FROM td_mta.keywords"
        ),
        "A,z,é"
    );
    refuse(
        format!("DELETE FROM td_mta.blobs WHERE account={a} AND id={a}"),
        "23503",
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.blob_chunks"),
        "512"
    );
    sql(&fixture, "BEGIN; DELETE FROM td_mta.keywords; DELETE FROM td_mta.memberships; DELETE FROM td_mta.emails; DELETE FROM td_mta.blobs; COMMIT");
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.blob_chunks"),
        "0"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blob_ids"), "2");
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_mail_schema_upgrade_retains_materials_and_epoch() {
    use td_crypto::Digest;
    use td_mta::postgres_schema::{inspect, upgrade, Error as SchemaError};
    for previous_version in [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] {
        let fixture = Fixture::new();
        let trust = TrustStore::from_pem(&fixture.ca).unwrap();
        for (index, ddl) in [
            include_str!("../src/postgres_schema.sql"),
            include_str!("../src/postgres_materials.sql"),
            include_str!("../src/postgres_mail.sql"),
            include_str!("../src/postgres_devices.sql"),
            include_str!("../src/postgres_configuration.sql"),
            include_str!("../src/postgres_accounts.sql"),
            include_str!("../src/postgres_selection.sql"),
            include_str!("../src/postgres_flags.sql"),
            include_str!("../src/postgres_flag_batches.sql"),
            include_str!("../src/postgres_uploads.sql"),
            include_str!("../src/postgres_drafts.sql"),
            include_str!("../src/postgres_mailbox_create.sql"),
        ]
        .iter()
        .take(previous_version)
        .enumerate()
        {
            let mut hash = Provider.sha256().unwrap();
            hash.update(ddl.as_bytes()).unwrap();
            let digest: String = hash
                .finish()
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            sql(&fixture, &format!("{ddl} INSERT INTO td_mta.schema_migrations VALUES ({},decode('{digest}','hex'),decode(repeat('42',16),'hex'))", index + 1));
        }
        // Owner-inserted raw material remains byte-identical through migration.
        sql(&fixture, "INSERT INTO td_mta.config_materials VALUES (decode(repeat('01',16),'hex'),5,0,decode(repeat('00',32),'hex'),0)");
        assert_eq!(
            inspect(&fixture.request(), &trust, deadline()),
            Err(SchemaError::Version)
        );
        if previous_version == 9 {
            sql(&fixture,"INSERT INTO td_mta.accounts VALUES(decode(repeat('7e',16),'hex'),decode('0000000000000001','hex'),decode('0000000000000000','hex')); INSERT INTO td_mta.blob_ids SELECT decode(repeat('7e',16),'hex'),decode(repeat(n,16),'hex') FROM unnest(ARRAY['01','02']) n; INSERT INTO td_mta.blobs SELECT account,id,CASE WHEN id=decode(repeat('01',16),'hex') THEN 3 ELSE 0 END,decode(repeat('00',32),'hex'),0,decode('0000000000000001','hex'),CASE WHEN id=decode(repeat('01',16),'hex') THEN 1 ELSE 0 END FROM td_mta.blob_ids; INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('7e',16),'hex'),decode(repeat('01',16),'hex'),0,decode('616263','hex')); INSERT INTO td_mta.leases VALUES(decode(repeat('7e',16),'hex'),decode(repeat('01',16),'hex'),decode(repeat('01',16),'hex'),0,3,decode('0000000000000001','hex')); INSERT INTO td_mta.submissions SELECT account,id,id,id,id,decode(repeat('01',16),'hex'),'a@example.test',0,1,1,NULL,0,NULL,decode('0000000000000001','hex') FROM td_mta.blob_ids; INSERT INTO td_mta.recipients(account,submission_id,ordinal,address,state,uncertain,attempt_count,phase,reason,diagnostic,changed) SELECT account,id,0,'a@example.test',1,0,0,0,0,'',changed FROM td_mta.submissions");
        }
        let obstruction = match previous_version {
            2 => "accounts",
            3 => "devices",
            4 => "config_versions",
            5 => "account_bootstraps",
            6 => "config_selection",
            9 => "account_usage",
            10 => "draft_receipts",
            11 => "mailbox_create_receipts",
            12 => "receiving_receipts",
            _ => "flag_receipts",
        };
        if previous_version == 8 {
            sql(&fixture, "INSERT INTO td_mta.accounts VALUES(decode(repeat('7f',16),'hex'),decode('0000000000000001','hex'),decode('0000000000000000','hex')); INSERT INTO td_mta.flag_receipts VALUES(decode(repeat('7f',16),'hex'),decode(repeat('01',16),'hex'),decode(repeat('02',32),'hex'),decode('0000000000000001','hex'),decode('0000000000000001','hex')); ALTER TABLE td_mta.flag_receipts ADD COLUMN results bytea");
        } else {
            sql(
                &fixture,
                &format!("CREATE TABLE td_mta.{obstruction}(obstruction bigint)"),
            );
        }
        assert_eq!(
            upgrade(&fixture.request(), &trust, deadline()),
            Err(SchemaError::Database)
        );
        assert_eq!(
            sql(&fixture, "SELECT count(*) FROM td_mta.schema_migrations"),
            previous_version.to_string()
        );
        if previous_version == 8 {
            sql(
                &fixture,
                "ALTER TABLE td_mta.flag_receipts DROP COLUMN results",
            );
        } else {
            sql(&fixture, &format!("DROP TABLE td_mta.{obstruction}"));
        }
        if previous_version == 9 {
            sql(
                &fixture,
                "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
            );
        }
        let receipt = upgrade(&fixture.request(), &trust, deadline()).unwrap();
        assert_eq!(receipt.version, 13);
        if previous_version == 9 {
            assert_eq!(sql(&fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),"3|2|3|3|2");
        }
        if previous_version == 8 {
            assert_eq!(sql(&fixture, "SELECT encode(results,'hex'),encode(old_sequence,'hex'),encode(new_sequence,'hex') FROM td_mta.flag_receipts"), "01|0000000000000001|0000000000000001");
        }
        assert_eq!(receipt.store_epoch.as_bytes(), &[0x42; 16]);
        assert_eq!(
        sql(
            &fixture,
            "SELECT kind,length,chunk_count,encode(checksum,'hex') FROM td_mta.config_materials"
        ),
        format!("5|0|0|{}", "00".repeat(32))
    );
        assert_eq!(
            inspect(&fixture.request(), &trust, deadline()).unwrap(),
            receipt
        );
    }
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_devices_use_current_primary_state_and_retained_receipts() {
    use td_mta::{
        ids::{AccountId, StoreEpoch},
        postgres_devices as devices, postgres_schema,
    };
    let mut fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let other = AccountId::from_bytes([2; 16]);
    sql(&fixture, "INSERT INTO td_mta.accounts VALUES (decode(repeat('01',16),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex')),(decode(repeat('02',16),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex'))");
    sql(&fixture, "CREATE FUNCTION public.require_device_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('synchronous_commit') <> 'on' OR current_setting('idle_in_transaction_session_timeout') <> '10s' OR current_setting('transaction_isolation') <> 'read committed' THEN RAISE EXCEPTION 'device settings'; END IF; RETURN NEW; END $$; CREATE TRIGGER require_device_settings BEFORE INSERT OR UPDATE ON td_mta.devices FOR EACH ROW EXECUTE FUNCTION public.require_device_settings()");
    // The database owner's default must not weaken write serialization.
    sql(
        &fixture,
        "ALTER ROLE td_probe SET default_transaction_isolation='repeatable read'",
    );
    let creation = devices::prepare_creation(epoch, account, "laptop α", 10).unwrap();
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(None)
    );
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(Some(creation.device_id()))
    );
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            other,
            creation.password()
        ),
        Ok(None)
    );
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account,
            creation.password()
        ),
        Err(devices::Error::Epoch)
    );
    let mut wrong = creation.password().to_vec();
    wrong[96] = if wrong[96] == b'0' { b'1' } else { b'0' };
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            &wrong
        ),
        Ok(None)
    );
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            b"bad"
        ),
        Ok(None)
    );
    let revoke = devices::prepare_revocation(epoch, account, creation.device_id(), 20).unwrap();
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke),
        Ok(devices::RevocationOutcome::Recorded)
    );
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke),
        Ok(devices::RevocationOutcome::Recorded)
    );
    let second = devices::prepare_revocation(epoch, account, creation.device_id(), 30).unwrap();
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &second),
        Ok(devices::RevocationOutcome::AlreadyRevoked)
    );
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(None)
    );
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*),min(revoked_at) FROM td_mta.devices"
        ),
        "1|20"
    );
    fixture.stop();
    assert!(devices::check(
        &fixture.request(),
        &trust,
        deadline(),
        epoch,
        account,
        creation.password()
    )
    .is_err());
    fixture.start(true, false);
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(None)
    );
    sql(
        &fixture,
        "DROP TRIGGER require_device_settings ON td_mta.devices",
    );
    sql(
        &fixture,
        "UPDATE td_mta.devices SET creation_digest=decode(repeat('00',32),'hex')",
    );
    assert_eq!(
        devices::publish_creation(&fixture.request(), &trust, deadline(), &creation),
        Err(devices::Error::Conflict)
    );
    sql(
        &fixture,
        "UPDATE td_mta.devices SET revocation_digest=decode(repeat('00',32),'hex')",
    );
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke),
        Err(devices::Error::Conflict)
    );
    let missing =
        devices::prepare_creation(epoch, AccountId::from_bytes([3; 16]), "missing", 1).unwrap();
    assert_eq!(
        devices::publish_creation(&fixture.request(), &trust, deadline(), &missing),
        Err(devices::Error::Missing)
    );
    for label in ["", "bad\nlabel", &"é".repeat(65)] {
        assert!(matches!(
            devices::prepare_creation(epoch, account, label, 0),
            Err(devices::Error::Invalid)
        ));
    }
    assert!(matches!(
        devices::prepare_creation(epoch, account, "overflow", u64::MAX),
        Err(devices::Error::Invalid)
    ));
    // Competing independent connections cannot both consume the final slot.
    sql(&fixture, "INSERT INTO td_mta.devices(account,id,salt,verifier,label,created_at,creation_digest) SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',32),'hex'),'fixture',0,decode(repeat('00',32),'hex') FROM generate_series(1,254) n");
    let first = devices::prepare_creation(epoch, account, "first", 1).unwrap();
    let second = devices::prepare_creation(epoch, account, "second", 1).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = thread::scope(|scope| {
        let one = scope.spawn(|| {
            barrier.wait();
            devices::publish_creation(&fixture.request(), &trust, deadline(), &first)
        });
        let two = scope.spawn(|| {
            barrier.wait();
            devices::publish_creation(&fixture.request(), &trust, deadline(), &second)
        });
        [one.join().unwrap(), two.join().unwrap()]
    });
    // A loaded host may exhaust the bounded account-lock wait. Retry the
    // same retained operation after both initial writers have completed.
    let results = results
        .into_iter()
        .zip([&first, &second])
        .map(|(result, creation)| {
            if result == Err(devices::Error::Database) {
                devices::publish_creation(&fixture.request(), &trust, deadline(), creation)
            } else {
                result
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(devices::Error::Capacity))
            .count(),
        1
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "256");
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_device_commit_uncertainty_resolves_without_duplicate_or_unrevocation() {
    use td_mta::{ids::AccountId, postgres_devices as devices, postgres_schema};
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    sql(&fixture, "INSERT INTO td_mta.accounts VALUES (decode(repeat('01',16),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex'))");
    sql(&fixture, "CREATE FUNCTION public.delay_device_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_device AFTER INSERT OR UPDATE ON td_mta.devices DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_device_commit()");
    let creation = devices::prepare_creation(epoch, account, "retained", 1).unwrap();
    assert_eq!(
        devices::publish_creation(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(2),
            &creation
        ),
        Err(devices::Error::Indeterminate)
    );
    // A retry either waits behind the original fence or finds its receipt.
    let retry = devices::publish_creation(&fixture.request(), &trust, deadline(), &creation);
    assert!(matches!(retry, Ok(()) | Err(devices::Error::Database)));
    let settled = || {
        let stop = Instant::now() + Duration::from_secs(8);
        while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'") != "0" {
            assert!(Instant::now()<stop);
            thread::sleep(Duration::from_millis(50));
        }
    };
    settled();
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "1");
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(Some(creation.device_id()))
    );
    let revoke = devices::prepare_revocation(epoch, account, creation.device_id(), 2).unwrap();
    assert_eq!(
        devices::publish_revocation(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(2),
            &revoke
        ),
        Err(devices::Error::Indeterminate)
    );
    settled();
    assert_eq!(
        devices::publish_revocation(&fixture.request(), &trust, deadline(), &revoke),
        Ok(devices::RevocationOutcome::Recorded)
    );
    devices::publish_creation(&fixture.request(), &trust, deadline(), &creation).unwrap();
    assert_eq!(
        devices::check(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            creation.password()
        ),
        Ok(None)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*),min(revoked_at) FROM td_mta.devices"
        ),
        "1|2"
    );
    sql(&fixture, "DROP TRIGGER delay_device ON td_mta.devices; CREATE FUNCTION public.reject_device() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END $$; CREATE TRIGGER reject_device BEFORE INSERT ON td_mta.devices FOR EACH ROW EXECUTE FUNCTION public.reject_device()");
    let rejected = devices::prepare_creation(epoch, account, "rejected", 3).unwrap();
    assert_eq!(
        devices::publish_creation(&fixture.request(), &trust, deadline(), &rejected),
        Err(devices::Error::Database)
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "1");
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_profile_materials_decode_text_and_compile_tls_without_files() {
    use std::sync::Arc;
    use td_mta::{
        clock::{RuntimeClock, TlsClockSource},
        config::{load, materialize, profile::Roles, stanza::Pending, storage::Storage, stream},
        generations::GenerationSet,
        postgres_materials::{self as materials, Kind, Version},
        postgres_profile_materials::Loader,
        postgres_schema,
        tls_policy::TlsPolicies,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let chain = fs::read(fixture.root.join("server.crt")).unwrap();
    let key = fs::read(fixture.root.join("server.key")).unwrap();
    for (n, kind, bytes) in [
        (1, Kind::RelayPassword, b"database-password\n".as_slice()),
        (2, Kind::CertificateChain, chain.as_slice()),
        (3, Kind::PrivateKey, key.as_slice()),
        (4, Kind::TrustBundle, fixture.ca.as_slice()),
        (5, Kind::TextSignature, b"signature\r\n".as_slice()),
    ] {
        materials::insert(
            &request,
            &trust,
            deadline(),
            epoch,
            Version::from_bytes([n; 16]),
            kind,
            bytes,
        )
        .unwrap();
    }
    let source = format!(
        r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "user"
[domain "example.test"]
mx_host = "localhost"
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
text_signature_file = "/postgres-material/{signature}"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "relay-user"
password_file = "/postgres-material/{password}"
ca_file = "/postgres-material/{ca}"
[certificate "public"]
mode = "files"
chain_file = "/postgres-material/{chain}"
key_file = "/postgres-material/{key}"
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#,
        password = Version::from_bytes([1; 16]),
        chain = Version::from_bytes([2; 16]),
        key = Version::from_bytes([3; 16]),
        ca = Version::from_bytes([4; 16]),
        signature = Version::from_bytes([5; 16])
    );
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    let parse = |source: &str, scratch: &mut [u8]| {
        load::read_profile(
            Storage::try_new().unwrap(),
            &mut Pending::new(),
            scratch,
            &mut source.as_bytes(),
            Roles::from_bits(2).unwrap(),
        )
        .unwrap()
    };
    let profile = parse(&source, &mut scratch);
    let mut loader = Loader::new(&request, &trust, epoch, deadline());
    let resolved = materialize::read_profile_text(profile, &mut scratch, |reference| {
        loader.open_input(reference)
    })
    .unwrap();
    assert_eq!(resolved.relay_password().unwrap(), "database-password");
    assert_eq!(
        resolved.signatures(0).unwrap().unwrap().text,
        "signature\r\n"
    );
    let set = GenerationSet::at_startup();
    let clock = Arc::new(td_crypto::ClockHandle::new(TlsClockSource::new(Arc::new(
        RuntimeClock::new(),
    ))));
    let prepared =
        TlsPolicies::prepare_profile(set.reserve().unwrap(), &resolved, clock, |reference| {
            loader
                .open_tls(reference)
                .map_err(|_| td_mta::ports::Error::Invalid)
        })
        .unwrap();
    drop(prepared);
    // Kind-confused references cannot reinterpret public bytes as a secret.
    let wrong = source.replace(
        &Version::from_bytes([1; 16]).to_string(),
        &Version::from_bytes([2; 16]).to_string(),
    );
    let profile = parse(&wrong, &mut scratch);
    let mut loader = Loader::new(&request, &trust, epoch, deadline());
    let failure = materialize::read_profile_text(profile, &mut scratch, |reference| {
        loader.open_input(reference)
    })
    .unwrap_err();
    assert!(matches!(
        failure.error(),
        materialize::Error::Input(td_mta::postgres_profile_materials::Error::Database(
            materials::Error::Kind
        ))
    ));
    let profile = parse(&source, &mut scratch);
    assert!(
        materialize::read_profile_text(profile, &mut scratch, |reference| loader
            .open_input(reference))
        .is_err()
    );
    // A valid profile cannot fall back to a real existing local file.
    let local = source.replace(
        &format!("/postgres-material/{}", Version::from_bytes([1; 16])),
        fixture.root.join("password").to_str().unwrap(),
    );
    let profile = parse(&local, &mut scratch);
    let mut loader = Loader::new(&request, &trust, epoch, deadline());
    assert!(
        materialize::read_profile_text(profile, &mut scratch, |reference| loader
            .open_input(reference))
        .is_err()
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_configuration_validates_all_profiles_and_binds_canonical_sources() {
    exercise_configuration(ConfigurationExercise::Flags);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_uploads_publish_chunks_quotas_and_retained_outcomes() {
    exercise_configuration(ConfigurationExercise::Uploads);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_blob_downloads_enforce_device_leases_and_verify_snapshots() {
    exercise_configuration(ConfigurationExercise::Downloads);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_drafts_publish_body_threads_history_and_retained_outcomes() {
    exercise_configuration(ConfigurationExercise::Drafts);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_changes_preserve_transaction_pages_and_snapshot_state() {
    exercise_configuration(ConfigurationExercise::Changes);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_email_views_keep_metadata_values_and_bodies_on_one_snapshot() {
    exercise_configuration(ConfigurationExercise::Read);
}

#[test]
#[ignore = "requires local PostgreSQL binaries"]
fn real_postgres_email_queries_bind_order_pages_and_totals_to_one_snapshot() {
    exercise_configuration(ConfigurationExercise::Query);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_thread_windows_retain_order_membership_and_snapshot() {
    exercise_configuration(ConfigurationExercise::Threads);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_folders_publish_names_history_and_retained_outcomes() {
    exercise_configuration(ConfigurationExercise::Folders);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_moves_commit_memberships_flags_history_and_receipts() {
    exercise_configuration(ConfigurationExercise::Moves);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_deletion_releases_last_owner_bodies_and_retains_outcomes() {
    exercise_configuration(ConfigurationExercise::Deletion);
}

enum ConfigurationExercise {
    Receiving,
    Deletion,
    Moves,
    Folders,
    Threads,
    Query,
    Flags,
    Uploads,
    Downloads,
    Drafts,
    Changes,
    Read,
}

fn exercise_configuration(exercise: ConfigurationExercise) {
    use td_mta::{
        config::profile::Roles,
        postgres_configuration::{self as configuration, Profile},
        postgres_materials::{self as materials, Kind, Version},
        postgres_schema,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let chain = fs::read(fixture.root.join("server.crt")).unwrap();
    let key = fs::read(fixture.root.join("server.key")).unwrap();
    for (n, kind, bytes) in [
        (1, Kind::RelayPassword, b"database-password\n".as_slice()),
        (2, Kind::CertificateChain, chain.as_slice()),
        (3, Kind::PrivateKey, key.as_slice()),
        (4, Kind::TrustBundle, fixture.ca.as_slice()),
        (5, Kind::TextSignature, b"signature\r\n".as_slice()),
        (6, Kind::CertificateChain, b"invalid-certificate".as_slice()),
    ] {
        materials::insert(
            &request,
            &trust,
            deadline(),
            epoch,
            Version::from_bytes([n; 16]),
            kind,
            bytes,
        )
        .unwrap();
    }
    let shared = format!(
        r#"version = 1
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "user"
[domain "example.test"]
mx_host = "localhost"
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
text_signature_file = "/postgres-material/{}"
"#,
        Version::from_bytes([5; 16])
    );
    let desktop = format!(
        r#"[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "relay-user"
password_file = "/postgres-material/{password}"
ca_file = "/postgres-material/{ca}"
[certificate "public"]
mode = "files"
chain_file = "/postgres-material/{chain}"
key_file = "/postgres-material/{key}"
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
"#,
        password = Version::from_bytes([1; 16]),
        chain = Version::from_bytes([2; 16]),
        key = Version::from_bytes([3; 16]),
        ca = Version::from_bytes([4; 16])
    );
    let cloud = desktop.clone()
        + r#"[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
"#;
    let one = Profile {
        name: "cloud",
        roles: Roles::from_bits(3).unwrap(),
        source: cloud.as_bytes(),
    };
    let two = Profile {
        name: "desktop",
        roles: Roles::from_bits(2).unwrap(),
        source: desktop.as_bytes(),
    };
    let prepare = |profiles: &[Profile<'_>]| {
        configuration::prepare(
            &request,
            &trust,
            deadline(),
            epoch,
            shared.as_bytes(),
            profiles,
        )
    };
    let candidate = prepare(&[two, one]).unwrap();
    assert_eq!(candidate.epoch(), epoch);
    assert_eq!(candidate.shared(), shared.as_bytes());
    assert_eq!(candidate.profile_count(), 2);
    assert_eq!(candidate.profile(0).unwrap().name, "cloud");
    assert_eq!(candidate.profile(1).unwrap().name, "desktop");
    assert_eq!(candidate.profile(0).unwrap().roles.bits(), 3);
    assert!(candidate.profile(2).is_none());
    assert_eq!(candidate.digest(), prepare(&[one, two]).unwrap().digest());
    assert_eq!(
        candidate.quotas(),
        configuration::SharedQuotas {
            body_bytes: 4 * 1024 * 1024 * 1024,
            blob_count: 250000,
            upload_bytes: 128 * 1024 * 1024,
            queue_bytes: 256 * 1024 * 1024,
            queue_submissions: 1000,
        }
    );
    for override_source in [
        "[disk]\nbody_bytes=2147483648\n",
        "[disk]\nblob_count=200000\n",
        "[limits]\nupload_disk_bytes=268435456\n",
        "[limits]\nqueue_disk_bytes=536870912\n",
        "[limits]\nqueue_submissions=2000\n",
    ] {
        let altered = format!("{desktop}{override_source}");
        let changed_profile = Profile {
            source: altered.as_bytes(),
            ..two
        };
        assert_eq!(
            prepare(&[one, changed_profile]).err(),
            Some(configuration::Error::SharedQuotas)
        );
        assert_ne!(
            prepare(&[changed_profile]).unwrap().quotas(),
            candidate.quotas()
        );
    }
    let local = format!("{desktop}[disk]\ncache_bytes=67108864\n[work]\nrequest_seconds=600\n");
    assert_eq!(
        prepare(&[
            one,
            Profile {
                source: local.as_bytes(),
                ..two
            }
        ])
        .unwrap()
        .quotas(),
        candidate.quotas()
    );

    let changed = format!("# candidate revision\n{desktop}");
    let changed = prepare(&[
        one,
        Profile {
            source: changed.as_bytes(),
            ..two
        },
    ])
    .unwrap();
    assert_ne!(candidate.digest(), changed.digest());
    let bad = desktop.replace(
        &Version::from_bytes([2; 16]).to_string(),
        &Version::from_bytes([6; 16]).to_string(),
    );
    assert_eq!(
        prepare(&[
            one,
            Profile {
                source: bad.as_bytes(),
                ..two
            }
        ])
        .err(),
        Some(configuration::Error::Provider)
    );
    let missing = desktop.replace(
        &Version::from_bytes([2; 16]).to_string(),
        &Version::from_bytes([7; 16]).to_string(),
    );
    assert_eq!(
        prepare(&[
            one,
            Profile {
                source: missing.as_bytes(),
                ..two
            }
        ])
        .err(),
        Some(configuration::Error::Material(
            td_mta::postgres_profile_materials::Error::Database(materials::Error::Missing)
        ))
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_materials"),
        "6"
    );
    use td_mta::postgres_configuration_store as versions;
    let version = versions::Version::from_bytes([11; 16]);
    assert_eq!(
        versions::read(&request, &trust, deadline(), epoch, version).err(),
        Some(versions::Error::Missing)
    );
    sql(&fixture, "ALTER ROLE td_probe SET default_transaction_isolation = 'repeatable read'; CREATE FUNCTION public.require_config_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('synchronous_commit') <> 'on' OR current_setting('idle_in_transaction_session_timeout') <> '10s' OR current_setting('transaction_isolation') <> 'read committed' THEN RAISE EXCEPTION 'config settings'; END IF; RETURN NEW; END $$; CREATE TRIGGER require_config_settings BEFORE INSERT ON td_mta.config_versions FOR EACH ROW EXECUTE FUNCTION public.require_config_settings()");
    versions::publish(&request, &trust, deadline(), version, &candidate).unwrap();
    versions::publish(&request, &trust, deadline(), version, &candidate).unwrap();
    assert_eq!(
        versions::publish(&request, &trust, deadline(), version, &changed),
        Err(versions::Error::Conflict)
    );
    let loaded = versions::read(&request, &trust, deadline(), epoch, version).unwrap();
    assert_eq!(loaded.digest(), candidate.digest());
    assert_eq!(loaded.shared(), candidate.shared());
    assert_eq!(loaded.quotas(), candidate.quotas());
    assert_eq!(
        loaded.profile(1).unwrap().source,
        candidate.profile(1).unwrap().source
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_versions"),
        "1"
    );
    // Exercise the complete 2 MiB source ceiling and all 33 wire chunks.
    let mut large = desktop.clone();
    let target = configuration::MAX_SOURCE_BYTES - shared.len() - two.name.len() - 1;
    while large.len() < target {
        let count = (target - large.len()).min(4096);
        if count == 1 {
            large.push('\n');
        } else {
            large.push('#');
            large.extend(std::iter::repeat_n('x', count - 2));
            large.push('\n');
        }
    }
    let large = prepare(&[Profile {
        source: large.as_bytes(),
        ..two
    }])
    .unwrap();
    let large_id = versions::Version::from_bytes([12; 16]);
    versions::publish(&request, &trust, deadline(), large_id, &large).unwrap();
    assert_eq!(
        sql(
            &fixture,
            "SELECT chunk_count FROM td_mta.config_versions WHERE id=decode(repeat('0c',16),'hex')"
        ),
        "33"
    );
    assert_eq!(
        versions::read(&request, &trust, deadline(), epoch, large_id)
            .unwrap()
            .digest(),
        large.digest()
    );
    // A mid-publication failure leaves neither manifest nor prefix chunks.
    sql(&fixture, "CREATE FUNCTION public.reject_config_chunk() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.ordinal = 1 THEN RAISE EXCEPTION 'fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_config_chunk BEFORE INSERT ON td_mta.config_version_chunks FOR EACH ROW EXECUTE FUNCTION public.reject_config_chunk()");
    let failed = versions::Version::from_bytes([13; 16]);
    assert_eq!(
        versions::publish(&request, &trust, deadline(), failed, &large),
        Err(versions::Error::Database)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.config_versions WHERE id=decode(repeat('0d',16),'hex')"
        ),
        "0"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.config_version_chunks WHERE version_id=decode(repeat('0d',16),'hex')"), "0");
    sql(&fixture, "DROP TRIGGER reject_config_chunk ON td_mta.config_version_chunks; CREATE FUNCTION public.delay_config_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_config AFTER INSERT ON td_mta.config_versions DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_config_commit()");
    let uncertain = versions::Version::from_bytes([14; 16]);
    assert_eq!(
        versions::publish(
            &request,
            &trust,
            Instant::now() + Duration::from_secs(2),
            uncertain,
            &candidate
        ),
        Err(versions::Error::Indeterminate)
    );
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(
        &fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    versions::publish(&request, &trust, deadline(), uncertain, &candidate).unwrap();
    assert_eq!(
        versions::read(&request, &trust, deadline(), epoch, uncertain)
            .unwrap()
            .digest(),
        candidate.digest()
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_versions"),
        "3"
    );
    sql(
        &fixture,
        "DROP TRIGGER delay_config ON td_mta.config_versions",
    );
    // Competing different contents serialize on the retained version ID, even
    // with the database role's default isolation set to REPEATABLE READ.
    let raced = versions::Version::from_bytes([15; 16]);
    let (first, second) = thread::scope(|scope| {
        let first =
            scope.spawn(|| versions::publish(&request, &trust, deadline(), raced, &candidate));
        let second =
            scope.spawn(|| versions::publish(&request, &trust, deadline(), raced, &changed));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(first.is_ok(), second.is_ok());
    let (winner, loser, refused) = if first.is_ok() {
        (&candidate, &changed, second)
    } else {
        (&changed, &candidate, first)
    };
    assert!(matches!(
        refused,
        Err(versions::Error::Conflict | versions::Error::Database)
    ));
    versions::publish(&request, &trust, deadline(), raced, winner).unwrap();
    assert_eq!(
        versions::publish(&request, &trust, deadline(), raced, loser),
        Err(versions::Error::Conflict)
    );
    assert_eq!(
        versions::read(&request, &trust, deadline(), epoch, raced)
            .unwrap()
            .digest(),
        winner.digest()
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.config_versions"),
        "4"
    );
    // Exercise the real owner commands through protected bootstrap/source files.
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("config-ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("config-connection");
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let shared_file = fixture.root.join("config-shared");
    let cloud_file = fixture.root.join("config-cloud");
    let desktop_file = fixture.root.join("config-desktop");
    fs::write(&shared_file, &shared).unwrap();
    fs::write(&cloud_file, &cloud).unwrap();
    fs::write(&desktop_file, &desktop).unwrap();
    let command_version = versions::Version::from_bytes([16; 16]).to_string();
    let invoke = |operation, shared_path: &std::path::Path, status| {
        let epoch_text = epoch.to_string();
        let mut args = vec![
            "postgres",
            operation,
            "--connection-file",
            connection.to_str().unwrap(),
            "--epoch",
            &epoch_text,
            "--version",
            &command_version,
        ];
        if operation == "config-import" {
            args.extend([
                "--shared-file",
                shared_path.to_str().unwrap(),
                "--instance",
                "desktop",
                "jmap",
                desktop_file.to_str().unwrap(),
                "--instance",
                "cloud",
                "receive,jmap",
                cloud_file.to_str().unwrap(),
            ]);
        }
        let output = command_output(&args, fixture.root.path(), status);
        assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
        assert!(!output.contains(connection.to_str().unwrap()));
        assert!(!output.contains("relay-user"));
        output
    };
    assert!(invoke("config-check", &shared_file, 1).contains("configuration-missing"));
    let imported = invoke("config-import", &shared_file, 0);
    assert!(
        imported.contains("\"profiles\":2,\"content_validated\":true,\"profile_activated\":false")
    );
    let expected_digest: String = candidate
        .digest()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert!(imported.contains(&expected_digest));
    assert!(invoke("config-import", &shared_file, 0).contains(&expected_digest));
    assert!(invoke("config-check", &shared_file, 0).contains(&expected_digest));
    fs::write(&desktop_file, format!("# changed\n{desktop}")).unwrap();
    assert!(invoke("config-import", &shared_file, 1).contains("version-content-conflict"));
    assert!(invoke("config-check", &shared_file, 0).contains(&expected_digest));
    assert!(invoke("config-import", &connection, 1).contains("input-secret-alias"));
    fs::write(
        &shared_file,
        vec![b'x'; configuration::MAX_SOURCE_BYTES + 1],
    )
    .unwrap();
    assert!(invoke("config-import", &shared_file, 1).contains("capacity"));
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(invoke("config-check", &shared_file, 1).contains("input-private-mode"));
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    use td_mta::postgres_selection as selection;
    assert!(selection::read(&request, &trust, deadline(), epoch)
        .unwrap()
        .is_none());
    use td_mta::{
        generations::GenerationSet, postgres_runtime_profile as runtime, tls_policy::TlsPolicies,
    };
    let empty_domain = GenerationSet::<TlsPolicies>::at_startup();
    assert_eq!(
        runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            "desktop",
            empty_domain.reserve().unwrap()
        )
        .err(),
        Some(runtime::Error::Unselected)
    );
    assert_eq!(empty_domain.available(), 2);
    let first = selection::prepare(epoch, 0, version, &candidate).unwrap();
    assert_eq!(
        selection::inspect(&request, &trust, deadline(), first).unwrap(),
        None
    );
    // Receipt resolution must wait for a publication holding the policy row.
    use std::io::Write;
    let mut blocker = ProbeChild(
        sql_command(&fixture)
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
    while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%FROM td_mta.config_selection%'") != "1" {
        assert!(Instant::now() < stop, "selection writer did not acquire barrier");
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        selection::inspect(&request, &trust, deadline(), first),
        Err(selection::Error::Database)
    );
    writeln!(input, "COMMIT;").unwrap();
    drop(input);
    wait_command(&mut blocker);
    let selected = selection::publish(&request, &trust, deadline(), first).unwrap();
    assert_eq!(selected.generation, 1);
    assert_eq!(selected.version, version);
    assert_eq!(
        runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            "desktop",
            empty_domain.reserve().unwrap()
        )
        .err(),
        Some(runtime::Error::Bootstrap(
            td_mta::postgres_accounts::Error::Missing
        ))
    );
    assert_eq!(empty_domain.available(), 2);
    let account = td_mta::ids::AccountId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    td_mta::postgres_accounts::publish(
        &request,
        &trust,
        deadline(),
        td_mta::postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    for (name, roles) in [("desktop", 2), ("cloud", 3)] {
        let mut domain = GenerationSet::<TlsPolicies>::at_startup();
        let prepared = runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            name,
            domain.reserve().unwrap(),
        )
        .unwrap();
        assert_eq!(prepared.binding().epoch(), epoch);
        assert_eq!(prepared.binding().selection(), selected);
        assert_eq!(prepared.binding().account(), account);
        assert_eq!(prepared.binding().profile(), name);
        assert_eq!(prepared.binding().roles().bits(), roles);
        assert_eq!(prepared.binding().quotas(), candidate.quotas());
        assert_eq!(prepared.configuration().roles().bits(), roles);
        if name == "desktop" {
            match exercise {
                ConfigurationExercise::Receiving => {}
                ConfigurationExercise::Deletion => {
                    exercise_deletion(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Moves => {
                    exercise_moves(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Flags => {
                    exercise_flags(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Uploads => {
                    exercise_uploads(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Folders => {
                    exercise_folders(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Threads => {
                    exercise_threads(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Query => {
                    exercise_query(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Read => {
                    exercise_read(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Changes => {
                    exercise_changes(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Drafts => {
                    exercise_drafts(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Downloads => {
                    exercise_downloads(&fixture, &request, &trust, prepared.binding())
                }
            }
        }
        if name == "cloud" && matches!(exercise, ConfigurationExercise::Receiving) {
            exercise_receiving(&fixture, &request, &trust, prepared.binding());
        }
        let (_, configuration, policies) = prepared.into_parts();
        assert_eq!(
            configuration
                .candidate()
                .identities()
                .unwrap()
                .account()
                .unwrap()
                .id,
            account
        );
        drop(domain.publish(policies).unwrap());
        let active = domain.current().unwrap();
        let https = TlsPolicies::listener(&active, "https").unwrap();
        assert_eq!(
            TlsPolicies::role(&active, https).unwrap(),
            td_mta::tls_policy::PolicyRole::Https
        );
        assert_eq!(TlsPolicies::listener(&active, "smtp").is_ok(), roles == 3);
    }
    assert_eq!(
        runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            "missing",
            empty_domain.reserve().unwrap()
        )
        .err(),
        Some(runtime::Error::MissingProfile)
    );
    assert_eq!(
        runtime::load(
            &request,
            &trust,
            Instant::now(),
            epoch,
            "desktop",
            empty_domain.reserve().unwrap()
        )
        .err(),
        Some(runtime::Error::Deadline)
    );
    assert_eq!(
        runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            "Desktop",
            empty_domain.reserve().unwrap()
        )
        .err(),
        Some(runtime::Error::Name)
    );
    assert_eq!(empty_domain.available(), 2);

    assert_eq!(
        selection::publish(&request, &trust, deadline(), first).unwrap(),
        selected
    );
    let (observed, loaded) = selection::read(&request, &trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    assert_eq!(observed, selected);
    assert_eq!(loaded.digest(), candidate.digest());
    let stale = selection::prepare(epoch, 0, version, &candidate).unwrap();
    assert_eq!(
        selection::publish(&request, &trust, deadline(), stale),
        Err(selection::Error::Conflict)
    );
    let next = selection::prepare(epoch, 1, version, &candidate).unwrap();
    let contender = selection::prepare(epoch, 1, version, &candidate).unwrap();
    // Revalidation opens several material connections under this one budget.
    let race_deadline = || Instant::now() + Duration::from_secs(10);
    let results = thread::scope(|scope| {
        let a = scope.spawn(|| selection::publish(&request, &trust, race_deadline(), next));
        let b = scope.spawn(|| selection::publish(&request, &trust, race_deadline(), contender));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(results.0.is_ok(), results.1.is_ok(), "{results:?}");
    for (result, change) in [(results.0, next), (results.1, contender)] {
        if matches!(
            result,
            Err(selection::Error::Configuration(
                versions::Error::Validation(configuration::Error::Provider)
            ))
        ) {
            // Cross-domain generation-ID contention can refuse preparation.
            // Revalidate once after the winner; this operation must stay stale.
            assert_eq!(
                selection::publish(&request, &trust, race_deadline(), change),
                Err(selection::Error::Conflict),
                "initial race: {results:?}"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Ok(_) | Err(selection::Error::Conflict) | Err(selection::Error::Database)
                ),
                "{results:?}"
            );
        }
    }
    assert_eq!(
        selection::read(&request, &trust, deadline(), epoch)
            .unwrap()
            .unwrap()
            .0
            .generation,
        2
    );
    // A historical receipt is not overwritten by a later selected generation.
    assert_eq!(
        selection::publish(&request, &trust, deadline(), first).unwrap(),
        selected
    );
    sql(&fixture, "CREATE FUNCTION public.delay_selection_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_selection AFTER INSERT ON td_mta.config_selection_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_selection_commit()");
    let uncertain = selection::prepare(epoch, 2, version, &candidate).unwrap();
    assert_eq!(
        selection::publish(
            &request,
            &trust,
            Instant::now() + Duration::from_secs(2),
            uncertain
        ),
        Err(selection::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(5));
    sql(
        &fixture,
        "DROP TRIGGER delay_selection ON td_mta.config_selection_receipts",
    );
    let landed = selection::inspect(&request, &trust, deadline(), uncertain)
        .unwrap()
        .unwrap();
    assert_eq!(landed.generation, 3);
    let settled = selection::publish(&request, &trust, deadline(), uncertain).unwrap();
    assert_eq!(settled, landed);
    assert_eq!(settled.generation, 3);
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.config_selection_receipts"
        ),
        "3"
    );
    // A failed receipt insertion must roll back the pointer as well.
    sql(&fixture, "CREATE FUNCTION public.reject_selection() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_selection BEFORE INSERT ON td_mta.config_selection_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_selection()");
    let refused = selection::prepare(epoch, 3, version, &candidate).unwrap();
    assert_eq!(
        selection::publish(&request, &trust, deadline(), refused),
        Err(selection::Error::Database)
    );
    assert_eq!(
        selection::read(&request, &trust, deadline(), epoch)
            .unwrap()
            .unwrap()
            .0,
        settled
    );
    sql(
        &fixture,
        "DROP TRIGGER reject_selection ON td_mta.config_selection_receipts",
    );
    assert_eq!(
        selection::prepare(epoch, u64::MAX, version, &candidate).err(),
        Some(selection::Error::Capacity)
    );
    let mismatched = selection::prepare(epoch, 3, version, &changed).unwrap();
    assert_eq!(
        selection::publish(&request, &trust, deadline(), mismatched),
        Err(selection::Error::Conflict)
    );
    sql(&fixture, &format!("INSERT INTO td_mta.config_selection_receipts SELECT decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),pg_catalog.int8send(n),decode('{version}','hex') FROM generate_series(4::bigint,1024::bigint) n"));
    assert_eq!(
        selection::publish(&request, &trust, deadline(), refused),
        Err(selection::Error::Capacity)
    );
    assert_eq!(
        selection::read(&request, &trust, deadline(), epoch)
            .unwrap()
            .unwrap()
            .0,
        settled
    );
    sql(&fixture, "DELETE FROM td_mta.config_selection_receipts WHERE generation > decode('0000000000000003','hex')");
    // A valid digest is not permission to skip current material validation.
    sql(&fixture, "DELETE FROM td_mta.config_material_chunks WHERE material_id=decode(repeat('02',16),'hex'); DELETE FROM td_mta.config_materials WHERE id=decode(repeat('02',16),'hex')");
    assert!(invoke("config-check", &shared_file, 1).contains("material-missing"));
    assert!(matches!(
        runtime::load(
            &request,
            &trust,
            deadline(),
            epoch,
            "desktop",
            empty_domain.reserve().unwrap()
        ),
        Err(runtime::Error::Selection(_))
    ));
    assert_eq!(empty_domain.available(), 2);

    assert!(matches!(
        selection::publish(&request, &trust, deadline(), refused),
        Err(selection::Error::Configuration(_))
    ));
    assert_eq!(
        selection::inspect(&request, &trust, deadline(), first).unwrap(),
        Some(selected)
    );
    assert_eq!(
        selection::publish(&request, &trust, deadline(), first).unwrap(),
        selected
    );

    assert!(matches!(
        versions::read(&request, &trust, deadline(), epoch, version),
        Err(versions::Error::Validation(configuration::Error::Material(
            _
        )))
    ));
    sql(&fixture, "UPDATE td_mta.config_version_chunks SET bytes=pg_catalog.set_byte(bytes,0,0) WHERE version_id=decode(repeat('0b',16),'hex') AND ordinal=0");
    assert_eq!(
        versions::read(&request, &trust, deadline(), epoch, version).err(),
        Some(versions::Error::Integrity)
    );
}

#[test]
fn configuration_commands_keep_versions_explicit_and_refusals_redacted() {
    use td_mta::postgres_configuration_store::Version;
    let root = TempDir::new("pg-config-arguments").unwrap();
    let text = command_output(&["postgres", "config-version"], root.path(), 0);
    let version = text
        .split("\"version\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(Version::parse(version).unwrap().to_string(), version);
    for operation in ["config-version", "config-import", "config-check"] {
        let text = command_output(
            &["postgres", operation, "--password", "private-secret"],
            root.path(),
            2,
        );
        assert!(text.contains("invalid-arguments"));
        assert!(!text.contains("private-secret"));
    }
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_account_bootstrap_is_atomic_retained_and_preserves_later_state() {
    use td_mta::{
        ids::{AccountId, StoreEpoch},
        postgres_accounts as accounts, postgres_schema,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let creation = accounts::prepare(epoch, account).unwrap();
    assert_eq!(creation.epoch(), epoch);
    assert_eq!(creation.account(), account);
    assert_eq!(
        accounts::inspect(&request, &trust, deadline(), epoch, account),
        Err(accounts::Error::Missing)
    );
    sql(&fixture, "ALTER ROLE td_probe SET default_transaction_isolation='repeatable read'; CREATE FUNCTION public.require_bootstrap_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('synchronous_commit') <> 'on' OR current_setting('idle_in_transaction_session_timeout') <> '10s' OR current_setting('transaction_isolation') <> 'read committed' THEN RAISE EXCEPTION 'bootstrap settings'; END IF; RETURN NEW; END $$; CREATE TRIGGER require_bootstrap_settings BEFORE INSERT ON td_mta.accounts FOR EACH ROW EXECUTE FUNCTION public.require_bootstrap_settings()");
    accounts::publish(&request, &trust, deadline(), creation).unwrap();
    accounts::publish(&request, &trust, deadline(), creation).unwrap();
    assert_eq!(
        accounts::inspect(&request, &trust, deadline(), epoch, account).unwrap(),
        creation
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex'),encode(floor,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001|0000000000000000"
    );
    assert_eq!(sql(&fixture, "SELECT encode(id,'hex'),name,parent_id IS NULL,role,sort_order,subscribed,encode(changed,'hex') FROM td_mta.mailboxes"), format!("{}|Inbox|t|inbox|0|1|0000000000000001", creation.inbox()));
    assert_eq!(sql(&fixture, "SELECT encode(sequence,'hex'),operation,kind,action,encode(object,'hex') FROM td_mta.changes"), format!("0000000000000001|1|1|1|{}", creation.inbox()));
    let other = accounts::prepare(epoch, account).unwrap();
    assert_ne!(creation.inbox(), other.inbox());
    assert_eq!(
        accounts::publish(&request, &trust, deadline(), other),
        Err(accounts::Error::Conflict)
    );
    sql(&fixture, "UPDATE td_mta.accounts SET sequence=decode(repeat('ff',8),'hex'); UPDATE td_mta.mailboxes SET name='Renamed Inbox'");
    accounts::publish(&request, &trust, deadline(), creation).unwrap();
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "ffffffffffffffff"
    );
    assert_eq!(
        sql(&fixture, "SELECT name FROM td_mta.mailboxes"),
        "Renamed Inbox"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.changes"), "1");
    let wrong_epoch = accounts::prepare(
        StoreEpoch::from_bytes([99; 16]),
        AccountId::from_bytes([2; 16]),
    )
    .unwrap();
    assert_eq!(
        accounts::publish(&request, &trust, deadline(), wrong_epoch),
        Err(accounts::Error::Epoch)
    );
    // An unreceipted owner-created account is not silently reset or repaired.
    sql(&fixture, "DROP TRIGGER require_bootstrap_settings ON td_mta.accounts; INSERT INTO td_mta.accounts VALUES (decode(repeat('02',16),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex'))");
    let existing = accounts::prepare(epoch, AccountId::from_bytes([2; 16])).unwrap();
    assert_eq!(
        accounts::publish(&request, &trust, deadline(), existing),
        Err(accounts::Error::Exists)
    );
    // Failure after account and Inbox inserts must remove every provisional row.
    sql(&fixture, "CREATE FUNCTION public.reject_bootstrap_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END $$; CREATE TRIGGER reject_bootstrap_receipt BEFORE INSERT ON td_mta.account_bootstraps FOR EACH ROW EXECUTE FUNCTION public.reject_bootstrap_receipt()");
    let failed = accounts::prepare(epoch, AccountId::from_bytes([3; 16])).unwrap();
    assert_eq!(
        accounts::publish(&request, &trust, deadline(), failed),
        Err(accounts::Error::Database)
    );
    for table in ["accounts", "mailboxes", "changes", "account_bootstraps"] {
        let column = if table == "accounts" { "id" } else { "account" };
        assert_eq!(sql(&fixture, &format!("SELECT count(*) FROM td_mta.{table} WHERE {column}=decode(repeat('03',16),'hex')")), "0");
    }
    sql(&fixture, "DROP TRIGGER reject_bootstrap_receipt ON td_mta.account_bootstraps; CREATE FUNCTION public.delay_bootstrap_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_bootstrap AFTER INSERT ON td_mta.account_bootstraps DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_bootstrap_commit()");
    let uncertain = accounts::prepare(epoch, AccountId::from_bytes([4; 16])).unwrap();
    assert_eq!(
        accounts::publish(
            &request,
            &trust,
            Instant::now() + Duration::from_secs(2),
            uncertain
        ),
        Err(accounts::Error::Indeterminate)
    );
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(
        &fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        accounts::inspect(&request, &trust, deadline(), epoch, uncertain.account()).unwrap(),
        uncertain
    );
    accounts::publish(&request, &trust, deadline(), uncertain).unwrap();
    sql(
        &fixture,
        "DROP TRIGGER delay_bootstrap ON td_mta.account_bootstraps",
    );
    let raced_account = AccountId::from_bytes([5; 16]);
    let first = accounts::prepare(epoch, raced_account).unwrap();
    let second = accounts::prepare(epoch, raced_account).unwrap();
    let (one, two) = thread::scope(|scope| {
        let one = scope.spawn(|| accounts::publish(&request, &trust, deadline(), first));
        let two = scope.spawn(|| accounts::publish(&request, &trust, deadline(), second));
        (one.join().unwrap(), two.join().unwrap())
    });
    assert_ne!(one.is_ok(), two.is_ok());
    let (winner, loser, refused) = if one.is_ok() {
        (first, second, two)
    } else {
        (second, first, one)
    };
    assert!(matches!(
        refused,
        Err(accounts::Error::Conflict | accounts::Error::Database)
    ));
    assert_eq!(
        accounts::inspect(&request, &trust, deadline(), epoch, raced_account).unwrap(),
        winner
    );
    accounts::publish(&request, &trust, deadline(), winner).unwrap();
    assert_eq!(
        accounts::publish(&request, &trust, deadline(), loser),
        Err(accounts::Error::Conflict)
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.accounts"), "4");
    for table in ["mailboxes", "changes", "account_bootstraps"] {
        assert_eq!(
            sql(&fixture, &format!("SELECT count(*) FROM td_mta.{table}")),
            "3"
        );
    }
}

#[test]
#[ignore = "requires TD_MTA_TEST_POSTGRES_BIN naming an independently installed PostgreSQL 17"]
fn real_postgres_mailbox_reads_bind_authentication_counts_and_state() {
    use td_mta::{
        ids::{AccountId, StoreEpoch},
        postgres_accounts as accounts, postgres_devices as devices,
        postgres_mailboxes as mailboxes, postgres_schema,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let creation = accounts::prepare(epoch, account).unwrap();
    accounts::publish(&request, &trust, deadline(), creation).unwrap();
    let device = devices::prepare_creation(epoch, account, "mailbox reader", 1).unwrap();
    devices::publish_creation(&request, &trust, deadline(), &device).unwrap();
    let read = || {
        mailboxes::list(
            &request,
            &trust,
            deadline(),
            epoch,
            account,
            device.password(),
        )
    };
    let first = read().unwrap();
    assert_eq!(first.identity.committed_sequence.number(), 1);
    assert_eq!(first.identity.history_floor.number(), 0);
    assert_eq!(first.device, device.device_id());
    assert_eq!(first.mailboxes.len(), 1);
    assert_eq!(first.mailboxes[0].id, creation.inbox());
    assert_eq!(first.mailboxes[0].name, "Inbox");
    assert_eq!(first.mailboxes[0].counts.total_emails, 0);
    assert_eq!(
        mailboxes::list(&request, &trust, deadline(), epoch, account, b"bad"),
        Err(mailboxes::Error::Unauthenticated)
    );
    assert_eq!(
        mailboxes::list(
            &request,
            &trust,
            deadline(),
            StoreEpoch::from_bytes([8; 16]),
            account,
            device.password()
        ),
        Err(mailboxes::Error::Epoch)
    );
    let other = AccountId::from_bytes([2; 16]);
    accounts::publish(
        &request,
        &trust,
        deadline(),
        accounts::prepare(epoch, other).unwrap(),
    )
    .unwrap();
    assert_eq!(
        mailboxes::list(
            &request,
            &trust,
            deadline(),
            epoch,
            other,
            device.password()
        ),
        Err(mailboxes::Error::Unauthenticated)
    );
    sql(&fixture, &format!("BEGIN;
        INSERT INTO td_mta.blob_ids VALUES (decode('{account}','hex'),decode(repeat('03',16),'hex'));
        INSERT INTO td_mta.blobs VALUES (decode('{account}','hex'),decode(repeat('03',16),'hex'),0,decode(repeat('00',32),'hex'),0,decode('0000000000000001','hex'),0);
        INSERT INTO td_mta.threads SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode('0000000000000001','hex') FROM generate_series(1,2) n;
        INSERT INTO td_mta.emails (account,id,blob_id,thread_id,received_at,origin,changed) SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('03',16),'hex'),decode(lpad(to_hex(CASE WHEN n<=3 THEN 1 ELSE 2 END),32,'0'),'hex'),0,2,decode('0000000000000001','hex') FROM generate_series(1,5) n;
        INSERT INTO td_mta.memberships SELECT account,id,decode('{}','hex'),changed FROM td_mta.emails;
        INSERT INTO td_mta.keywords SELECT account,id,CASE WHEN id=decode(lpad('3',32,'0'),'hex') THEN '$draft' ELSE '$seen' END,changed FROM td_mta.emails WHERE id IN (decode(lpad('1',32,'0'),'hex'),decode(lpad('3',32,'0'),'hex'),decode(lpad('4',32,'0'),'hex'));
        COMMIT", creation.inbox()));
    let listing = read().unwrap();
    assert_eq!(
        listing.mailboxes[0].counts,
        mailboxes::Counts {
            total_emails: 5,
            unread_emails: 2,
            total_threads: 2,
            unread_threads: 2
        }
    );
    sql(
        &fixture,
        "UPDATE td_mta.memberships SET changed=decode('0000000000000002','hex')",
    );
    assert_eq!(read(), Err(mailboxes::Error::Integrity));
    sql(&fixture, "UPDATE td_mta.memberships SET changed=decode('0000000000000001','hex'); UPDATE td_mta.mailboxes SET role='INVALID' WHERE role='inbox'");
    assert_eq!(read(), Err(mailboxes::Error::Integrity));
    sql(&fixture, "UPDATE td_mta.mailboxes SET role='inbox'; UPDATE td_mta.accounts SET sequence=decode(repeat('ff',8),'hex')");
    assert_eq!(
        read().unwrap().identity.committed_sequence.number(),
        u64::MAX
    );
    sql(&fixture, &format!("UPDATE td_mta.accounts SET sequence=decode('0000000000000001','hex'); INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(lpad(to_hex(n+100),32,'0'),'hex'),repeat('x',1024),NULL,NULL,0,1,decode('0000000000000001','hex') FROM generate_series(1,255) n"));
    assert_eq!(read().unwrap().mailboxes.len(), 256);
    sql(&fixture, &format!("INSERT INTO td_mta.mailboxes VALUES (decode('{account}','hex'),decode(repeat('ee',16),'hex'),'overflow',NULL,NULL,0,1,decode('0000000000000001','hex'))"));
    assert_eq!(read(), Err(mailboxes::Error::Capacity));
    sql(&fixture, &format!("DELETE FROM td_mta.mailboxes WHERE account=decode('{account}','hex') AND id<>decode('{}','hex'); INSERT INTO td_mta.mailboxes VALUES (decode('{account}','hex'),decode(repeat('ee',16),'hex'),'child',decode('{}','hex'),NULL,0,1,decode('0000000000000001','hex')); UPDATE td_mta.mailboxes SET parent_id=decode(repeat('ee',16),'hex') WHERE account=decode('{account}','hex') AND id=decode('{}','hex')", creation.inbox(), creation.inbox(), creation.inbox()));
    assert_eq!(read(), Err(mailboxes::Error::Integrity));
    sql(&fixture, &format!("UPDATE td_mta.mailboxes SET parent_id=NULL WHERE account=decode('{account}','hex') AND id=decode('{}','hex')", creation.inbox()));
    assert_eq!(read().unwrap().mailboxes.len(), 2);
    let revoked = devices::prepare_revocation(epoch, account, device.device_id(), 2).unwrap();
    devices::publish_revocation(&request, &trust, deadline(), &revoked).unwrap();
    assert_eq!(read(), Err(mailboxes::Error::Unauthenticated));
    assert_eq!(
        mailboxes::list(
            &request,
            &trust,
            Instant::now(),
            epoch,
            account,
            device.password()
        ),
        Err(mailboxes::Error::Connection(Error::Deadline))
    );

    // Hold only the mailbox relation, so authentication/account state can finish.
    // Release it after committing a revocation and a new state in that same writer.
    use std::io::Write;
    let admitted = devices::prepare_creation(epoch, account, "snapshot race", 3).unwrap();
    devices::publish_creation(&request, &trust, deadline(), &admitted).unwrap();
    let mut blocker = ProbeChild(
        sql_command(&fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = blocker.0.stdin.take().unwrap();
    writeln!(
        input,
        "BEGIN; LOCK TABLE td_mta.mailboxes IN ACCESS EXCLUSIVE MODE;"
    )
    .unwrap();
    let stop = Instant::now() + Duration::from_secs(5);
    while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_locks WHERE relation='td_mta.mailboxes'::regclass AND mode='AccessExclusiveLock' AND granted") != "1" {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(5));
    }
    thread::scope(|scope| {
        let pending = scope.spawn(|| {
            mailboxes::list(
                &request,
                &trust,
                deadline(),
                epoch,
                account,
                admitted.password(),
            )
        });
        let stop = deadline();
        while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND wait_event_type='Lock' AND query LIKE 'WITH selected%'") != "1" {
            assert!(Instant::now() < stop, "mailbox observation did not reach barrier");
            thread::sleep(Duration::from_millis(5));
        }
        writeln!(input, "UPDATE td_mta.devices SET revoked_at=4,revocation_id=decode(repeat('dd',16),'hex'),revocation_digest=decode(repeat('00',32),'hex') WHERE account=decode('{account}','hex') AND id=decode('{}','hex'); UPDATE td_mta.accounts SET sequence=decode('0000000000000002','hex') WHERE id=decode('{account}','hex'); UPDATE td_mta.mailboxes SET name='After snapshot',changed=decode('0000000000000002','hex') WHERE account=decode('{account}','hex'); COMMIT;", admitted.device_id()).unwrap();
        drop(input);
        wait_command(&mut blocker);
        let old = pending.join().unwrap().unwrap();
        assert_eq!(old.identity.committed_sequence.number(), 1);
        assert!(old
            .mailboxes
            .iter()
            .all(|mailbox| mailbox.changed == 1 && mailbox.name != "After snapshot"));
    });
    assert_eq!(
        mailboxes::list(
            &request,
            &trust,
            deadline(),
            epoch,
            account,
            admitted.password()
        ),
        Err(mailboxes::Error::Unauthenticated)
    );
    let fresh = devices::prepare_creation(epoch, account, "after snapshot", 5).unwrap();
    devices::publish_creation(&request, &trust, deadline(), &fresh).unwrap();
    let new = mailboxes::list(
        &request,
        &trust,
        deadline(),
        epoch,
        account,
        fresh.password(),
    )
    .unwrap();
    assert_eq!(new.identity.committed_sequence.number(), 2);
    assert!(new
        .mailboxes
        .iter()
        .all(|mailbox| mailbox.changed == 2 && mailbox.name == "After snapshot"));
}

#[test]
#[ignore = "requires TD_MTA_TEST_POSTGRES_BIN naming an independently installed PostgreSQL 17"]
fn real_postgres_email_bodies_are_verified_bounded_and_snapshot_pinned() {
    use td_crypto::Digest;
    use td_mta::{
        ids::{AccountId, EmailId, StoreEpoch},
        ports::{self, BlobReader},
        postgres_accounts as accounts, postgres_body as body, postgres_devices as devices,
        postgres_schema,
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    accounts::publish(
        &request,
        &trust,
        deadline(),
        accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let device = devices::prepare_creation(epoch, account, "body reader", 1).unwrap();
    devices::publish_creation(&request, &trust, deadline(), &device).unwrap();
    let email = EmailId::from_bytes([2; 16]);
    let selected = body::ReadRequest {
        epoch,
        account,
        email,
        maximum: body::MAX_BODY_BYTES,
    };
    let check = |length: usize| {
        let mut hash = Provider.sha256().unwrap();
        for n in 0..length.div_ceil(65536) {
            let bytes = vec![b'A' + (n % 26) as u8; (length - n * 65536).min(65536)];
            hash.update(&bytes).unwrap();
        }
        let digest = hash
            .finish()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        sql(&fixture, &format!("BEGIN; DELETE FROM td_mta.emails; DELETE FROM td_mta.blobs; DELETE FROM td_mta.blob_ids; DELETE FROM td_mta.threads;
            INSERT INTO td_mta.blob_ids VALUES (decode('{account}','hex'),decode(repeat('03',16),'hex'));
            INSERT INTO td_mta.blobs VALUES (decode('{account}','hex'),decode(repeat('03',16),'hex'),{length},decode('{digest}','hex'),7,decode('0000000000000001','hex'),({length}+65535)/65536);
            INSERT INTO td_mta.blob_chunks SELECT decode('{account}','hex'),decode(repeat('03',16),'hex'),n,convert_to(repeat(chr(65+(n%26)),LEAST(65536,{length}-n*65536)),'UTF8') FROM generate_series(0,({length}+65535)/65536-1) n;
            INSERT INTO td_mta.threads VALUES (decode('{account}','hex'),decode(repeat('04',16),'hex'),decode('0000000000000001','hex'));
            INSERT INTO td_mta.emails (account,id,blob_id,thread_id,received_at,origin,changed) VALUES (decode('{account}','hex'),decode('{email}','hex'),decode(repeat('03',16),'hex'),decode(repeat('04',16),'hex'),9,2,decode('0000000000000001','hex')); COMMIT"));
        let started = Instant::now();
        let result = body::with_email(
            &request,
            &trust,
            Instant::now() + Duration::from_secs(10),
            selected,
            device.password(),
            |metadata, reader| {
                eprintln!("body length={length}: verified at {:?}", started.elapsed());
                assert_eq!(metadata.identity.committed_sequence.number(), 1);
                assert_eq!(metadata.email, email);
                assert_eq!(metadata.received_at, 9);
                assert_eq!(metadata.body.created_at, 7);
                assert_eq!(reader.len(), length as u64);
                let mut output = [0; 4096];
                let mut position = 0;
                while position < reader.len() {
                    let count = reader.read_at(position, &mut output)?;
                    assert!(count > 0);
                    for (offset, &byte) in output[..count].iter().enumerate() {
                        assert_eq!(
                            byte,
                            b'A' + (((position + offset as u64) / 65536) % 26) as u8
                        );
                    }
                    position += count as u64;
                }
                assert_eq!(reader.read_at(reader.len(), &mut output)?, 0);
                for offset in [0, 65535, 65536, 7 * 65536 + 65535, 8 * 65536, 16 * 65536] {
                    if offset < reader.len() {
                        let count = reader.read_at(offset, &mut output)?;
                        for (at, &byte) in output[..count].iter().enumerate() {
                            assert_eq!(byte, b'A' + (((offset + at as u64) / 65536) % 26) as u8);
                        }
                    }
                }
                Ok(position)
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "body length={length} failed after {:?}: {error:?}",
                started.elapsed()
            )
        });
        eprintln!("body length={length}: complete at {:?}", started.elapsed());
        assert_eq!(result, length as u64);
    };
    for length in [
        0,
        65535,
        65536,
        65537,
        body::MAX_BODY_BYTES as usize,
        17 * 65536 + 13,
    ] {
        check(length);
    }
    let unused = |_: body::Metadata, _: &mut dyn BlobReader| -> Result<(), ports::Error> {
        panic!("unverified or unauthorized callback");
    };
    assert_eq!(
        body::with_email(&request, &trust, deadline(), selected, b"bad", unused),
        Err(body::Error::Unauthenticated)
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            body::ReadRequest {
                epoch: StoreEpoch::from_bytes([9; 16]),
                ..selected
            },
            device.password(),
            unused
        ),
        Err(body::Error::Epoch)
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            body::ReadRequest {
                email: EmailId::from_bytes([9; 16]),
                ..selected
            },
            device.password(),
            unused
        ),
        Err(body::Error::Missing)
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            body::ReadRequest {
                maximum: 1,
                ..selected
            },
            device.password(),
            unused
        ),
        Err(body::Error::Capacity)
    );
    let other = AccountId::from_bytes([8; 16]);
    accounts::publish(
        &request,
        &trust,
        deadline(),
        accounts::prepare(epoch, other).unwrap(),
    )
    .unwrap();
    let other_device = devices::prepare_creation(epoch, other, "other body reader", 1).unwrap();
    devices::publish_creation(&request, &trust, deadline(), &other_device).unwrap();
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            body::ReadRequest {
                account: other,
                ..selected
            },
            device.password(),
            unused
        ),
        Err(body::Error::Unauthenticated)
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            body::ReadRequest {
                account: other,
                ..selected
            },
            other_device.password(),
            unused
        ),
        Err(body::Error::Missing)
    );
    sql(
        &fixture,
        "UPDATE td_mta.blobs SET changed=decode('0000000000000002','hex')",
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Integrity)
    );
    sql(
        &fixture,
        "UPDATE td_mta.blobs SET changed=decode('0000000000000001','hex')",
    );
    sql(
        &fixture,
        "UPDATE td_mta.blob_chunks SET bytes=convert_to(repeat('Z',65536),'UTF8') WHERE ordinal=0",
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Read(ports::Error::Corrupt))
    );
    sql(&fixture, "UPDATE td_mta.blob_chunks SET bytes=convert_to(repeat('A',65536),'UTF8') WHERE ordinal=0; DELETE FROM td_mta.blob_chunks WHERE ordinal=1");
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Integrity)
    );
    check(17 * 65536 + 13);
    sql(
        &fixture,
        "UPDATE td_mta.blob_chunks SET ordinal=100 WHERE ordinal=1",
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Read(ports::Error::Corrupt))
    );
    sql(
        &fixture,
        "UPDATE td_mta.blob_chunks SET ordinal=1 WHERE ordinal=100",
    );
    let disconnected = ports::Error::Io {
        kind: std::io::ErrorKind::ConnectionAborted,
        os_code: None,
    };
    let terminate = || {
        assert_eq!(sql(&fixture, "SELECT pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND state='idle in transaction'"), "t");
        let stop = deadline();
        while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'") != "0" {
            assert!(Instant::now() < stop, "body backend did not terminate");
            thread::sleep(Duration::from_millis(5));
        }
    };
    let broken = body::with_email(
        &request,
        &trust,
        deadline(),
        selected,
        device.password(),
        |_, reader| {
            terminate();
            let mut byte = [0];
            let failure = reader.read_at(0, &mut byte).unwrap_err();
            assert_eq!(reader.read_at(0, &mut byte), Err(failure));
            Ok(())
        },
    );
    assert_eq!(broken, Err(body::Error::Read(disconnected)));
    check(65537);
    // Both chunks remain cached after verification: no query can detect loss.
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            |_, reader| {
                terminate();
                let mut byte = [0];
                assert_eq!(reader.read_at(0, &mut byte), Err(disconnected));
                assert_eq!(reader.read_at(0, &mut byte), Err(disconnected));
                Ok(())
            }
        ),
        Err(body::Error::Read(disconnected))
    );
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            |_, _| {
                terminate();
                Ok(())
            }
        ),
        Err(body::Error::Read(disconnected))
    );
    check(17 * 65536 + 13);
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            |_, reader| {
                let mut byte = [0];
                assert_eq!(
                    reader.read_at(reader.len() + 1, &mut byte),
                    Err(ports::Error::Invalid)
                );
                assert_eq!(reader.read_at(0, &mut byte), Err(ports::Error::Invalid));
                Ok(())
            }
        ),
        Err(body::Error::Read(ports::Error::Invalid))
    );
    let end = Instant::now() + Duration::from_secs(3);
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            end,
            selected,
            device.password(),
            |_, _| {
                thread::sleep(
                    end.saturating_duration_since(Instant::now()) + Duration::from_millis(20),
                );
                Ok(())
            }
        ),
        Err(body::Error::Read(ports::Error::Deadline))
    );
    body::with_email(&request, &trust, deadline(), selected, device.password(), |_, reader| {
        assert_eq!(sql(&fixture, &format!("SELECT pg_try_advisory_lock({}::bigint)", postgres_schema::MIGRATION_KEY)), "f");
        sql(&fixture, &format!("UPDATE td_mta.devices SET revoked_at=4,revocation_id=decode(repeat('dd',16),'hex'),revocation_digest=decode(repeat('00',32),'hex') WHERE account=decode('{account}','hex'); UPDATE td_mta.blob_chunks SET bytes=convert_to(repeat('Z',65536),'UTF8') WHERE ordinal=0"));
        let mut bytes = [0;100];
        assert_eq!(reader.read_at(0, &mut bytes)?, 100);
        assert_eq!(bytes, [b'A';100]);
        Ok(())
    }).unwrap();
    assert_eq!(
        body::with_email(
            &request,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Unauthenticated)
    );
}

fn exercise_flags(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{ids::EmailId, postgres_devices as devices, postgres_email_set as flags};
    let account = binding.account();
    let email = EmailId::from_bytes([0xe1; 16]);
    let creation = devices::prepare_creation(binding.epoch(), account, "flag fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let device = creation.device_id();
    let password = creation.password();
    sql(fixture, &format!("BEGIN; INSERT INTO td_mta.blob_ids VALUES(decode('{account}','hex'),decode('{email}','hex')); INSERT INTO td_mta.blobs VALUES(decode('{account}','hex'),decode('{email}','hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0); INSERT INTO td_mta.threads VALUES(decode('{account}','hex'),decode('{email}','hex'),decode('0000000000000001','hex')); INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES(decode('{account}','hex'),decode('{email}','hex'),decode('{email}','hex'),decode('{email}','hex'),0,2,decode('0000000000000001','hex')); INSERT INTO td_mta.memberships SELECT account,decode('{email}','hex'),id,changed FROM td_mta.mailboxes WHERE account=decode('{account}','hex'); COMMIT"));
    let prepare = |expected, seen, starred| {
        flags::prepare(binding, device, email, expected, seen, starred).unwrap()
    };
    let publish =
        |change: &flags::Change| flags::publish(request, trust, deadline(), change, password);
    let first = prepare(Some(1), Some(true), Some(true));
    assert_eq!(
        flags::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    let first_result = publish(&first).unwrap();
    assert_eq!(
        first_result,
        flags::Outcome {
            old_sequence: 1,
            new_sequence: 2
        }
    );
    assert_eq!(publish(&first).unwrap(), first_result);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "2");
    assert_eq!(sql(fixture, "SELECT kind FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex') ORDER BY operation"), "3\n1");
    let listing = td_mta::postgres_mailboxes::list(
        request,
        trust,
        deadline(),
        binding.epoch(),
        account,
        password,
    )
    .unwrap();
    assert_eq!(listing.mailboxes[0].counts.unread_emails, 0);
    let noop = prepare(Some(2), Some(true), Some(true));
    assert_eq!(
        publish(&noop).unwrap(),
        flags::Outcome {
            old_sequence: 2,
            new_sequence: 2
        }
    );
    let stale = prepare(Some(1), Some(false), None);
    assert_eq!(publish(&stale), Err(flags::Error::State));
    let star = prepare(Some(2), None, Some(false));
    assert_eq!(publish(&star).unwrap().new_sequence, 3);
    assert_eq!(
        sql(
            fixture,
            "SELECT kind FROM td_mta.changes WHERE sequence=decode('0000000000000003','hex')"
        ),
        "3"
    );
    let unread = prepare(Some(3), Some(false), None);
    assert_eq!(publish(&unread).unwrap().new_sequence, 4);
    let listing = td_mta::postgres_mailboxes::list(
        request,
        trust,
        deadline(),
        binding.epoch(),
        account,
        password,
    )
    .unwrap();
    assert_eq!(listing.mailboxes[0].counts.unread_emails, 1);
    let missing = flags::prepare(
        binding,
        device,
        EmailId::from_bytes([0xee; 16]),
        None,
        Some(true),
        None,
    )
    .unwrap();
    assert_eq!(publish(&missing), Err(flags::Error::Missing));
    let fresh = prepare(None, Some(true), None);
    assert_eq!(
        flags::publish(request, trust, deadline(), &fresh, b"invalid"),
        Err(flags::Error::Unauthenticated)
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(publish(&fresh), Err(flags::Error::Policy));
    assert_eq!(publish(&first).unwrap(), first_result);
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    sql(fixture, "CREATE FUNCTION public.reject_flags() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_flags BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_flags()");
    assert_eq!(publish(&fresh), Err(flags::Error::Database));
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000004"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "0");
    sql(fixture, "DROP TRIGGER reject_flags ON td_mta.flag_receipts");
    sql(fixture, "CREATE FUNCTION public.delay_flags() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_flags AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_flags()");
    assert_eq!(
        flags::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(2),
            &fresh,
            password
        ),
        Err(flags::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(fixture, "DROP TRIGGER delay_flags ON td_mta.flag_receipts");
    assert_eq!(
        flags::inspect(request, trust, deadline(), &fresh)
            .unwrap()
            .unwrap()
            .new_sequence,
        5
    );
    assert_eq!(publish(&fresh).unwrap().new_sequence, 5);
    let left = prepare(Some(5), None, Some(true));
    let right = prepare(Some(5), Some(false), None);
    let (left_result, right_result) = thread::scope(|scope| {
        let left = scope.spawn(|| publish(&left));
        let right = scope.spawn(|| publish(&right));
        (left.join().unwrap(), right.join().unwrap())
    });
    assert_eq!(
        usize::from(left_result.is_ok()) + usize::from(right_result.is_ok()),
        1
    );
    let loser = if left_result.is_err() {
        left_result
    } else {
        right_result
    };
    assert!(matches!(
        loser,
        Err(flags::Error::State | flags::Error::Database)
    ));
    sql(fixture, &format!("INSERT INTO td_mta.flag_receipts SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode('0000000000000001','hex'),decode('0000000000000001','hex'),decode('01','hex') FROM generate_series(1,1024) n"));
    assert_eq!(
        publish(&prepare(None, Some(true), None)),
        Err(flags::Error::Capacity)
    );
    assert_eq!(publish(&first).unwrap(), first_result);
    sql(
        fixture,
        "DELETE FROM td_mta.flag_receipts WHERE request_digest=decode(repeat('00',32),'hex')",
    );
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),'extra',NULL,NULL,0,1,decode('0000000000000001','hex') FROM generate_series(1,256) n; INSERT INTO td_mta.memberships SELECT account,decode('{email}','hex'),id,changed FROM td_mta.mailboxes WHERE name='extra'"));
    let seen = sql(
        fixture,
        "SELECT count(*) FROM td_mta.keywords WHERE keyword='$seen'",
    ) == "1";
    assert_eq!(
        publish(&prepare(None, Some(!seen), None)),
        Err(flags::Error::Capacity)
    );
    sql(fixture, "DELETE FROM td_mta.memberships WHERE mailbox_id IN (SELECT id FROM td_mta.mailboxes WHERE name='extra'); DELETE FROM td_mta.mailboxes WHERE name='extra'");
    exercise_flag_batches(fixture, request, trust, binding, device, password, email);
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('fffffffffffffffe','hex')",
    );
    let high = prepare(Some(u64::MAX - 1), Some(true), Some(false));
    // Ensure the high-sequence assignment changes a keyword whichever race won.
    sql(fixture, &format!("DELETE FROM td_mta.keywords WHERE account=decode('{account}','hex') AND keyword='$seen'"));
    assert_eq!(publish(&high).unwrap().new_sequence, u64::MAX);
    assert_eq!(
        publish(&prepare(None, Some(false), None)),
        Err(flags::Error::Capacity)
    );
    assert_eq!(
        publish(&prepare(None, Some(true), None))
            .unwrap()
            .new_sequence,
        u64::MAX
    );
    let revoke = devices::prepare_revocation(binding.epoch(), account, device, 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(
        publish(&prepare(None, Some(false), None)),
        Err(flags::Error::Unauthenticated)
    );
    assert_eq!(publish(&first).unwrap(), first_result);
}

fn exercise_flag_batches(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    device: td_mta::ids::DeviceId,
    password: &[u8],
    email: td_mta::ids::EmailId,
) {
    use td_mta::{ids::EmailId, postgres_email_set as flags};
    let publish =
        |change: &flags::Change| flags::publish_batch(request, trust, deadline(), change, password);
    let account = binding.account();
    let update = flags::Update {
        mailbox: None,
        email,
        seen: None,
        flagged: None,
    };
    assert!(matches!(
        flags::prepare_batch(binding, device, &[update; 2], None),
        Err(flags::Error::Invalid)
    ));
    assert!(matches!(
        flags::prepare_batch(binding, device, &[update; 257], None),
        Err(flags::Error::Capacity)
    ));
    let empty = flags::prepare_batch(binding, device, &[], Some(6)).unwrap();
    let empty_result = publish(&empty).unwrap();
    assert_eq!(
        empty_result.state,
        flags::Outcome {
            old_sequence: 6,
            new_sequence: 6
        }
    );
    assert!(empty_result.results.is_empty());
    assert_eq!(publish(&empty).unwrap(), empty_result);
    assert_eq!(
        flags::publish(request, trust, deadline(), &empty, password),
        Err(flags::Error::Invalid)
    );
    let mixed = flags::prepare_batch(
        binding,
        device,
        &[
            update,
            flags::Update {
                email: EmailId::from_bytes([0xee; 16]),
                ..update
            },
        ],
        Some(6),
    )
    .unwrap();
    let mixed_result = publish(&mixed).unwrap();
    assert_eq!(
        mixed_result.results,
        [flags::UpdateResult::Updated, flags::UpdateResult::NotFound]
    );
    assert_eq!(mixed_result.state.new_sequence, 6);
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &mixed).unwrap(),
        Some(mixed_result)
    );
    sql(fixture, &format!("BEGIN; INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(lpad(to_hex(n+2048),32,'0'),'hex'),'bulk',NULL,NULL,0,1,decode('0000000000000001','hex') FROM generate_series(1,255) n; INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),decode('{email}','hex'),decode('{email}','hex'),0,2,decode('0000000000000001','hex') FROM generate_series(1,256) n; INSERT INTO td_mta.memberships SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),CASE WHEN n=256 THEN (SELECT id FROM td_mta.mailboxes WHERE account=decode('{account}','hex') AND role='inbox') ELSE decode(lpad(to_hex(n+2048),32,'0'),'hex') END,decode('0000000000000001','hex') FROM generate_series(1,256) n; COMMIT"));
    let updates: Vec<_> = (1..=256)
        .map(|n| flags::Update {
            mailbox: None,
            email: EmailId::parse(&format!("{:032x}", n + 1024)).unwrap(),
            seen: Some(true),
            flagged: Some(true),
        })
        .collect();
    let full = flags::prepare_batch(binding, device, &updates, Some(6)).unwrap();
    sql(fixture, &format!("INSERT INTO td_mta.memberships SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),id,changed FROM td_mta.mailboxes CROSS JOIN generate_series(1,255) n WHERE account=decode('{account}','hex') AND role='inbox'"));
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes VALUES(decode('{account}','hex'),decode(repeat('ab',16),'hex'),'overflow',NULL,NULL,0,1,decode('0000000000000001','hex')); INSERT INTO td_mta.memberships VALUES(decode('{account}','hex'),decode(lpad(to_hex(1025),32,'0'),'hex'),decode(repeat('ab',16),'hex'),decode('0000000000000001','hex'))"));
    assert_eq!(publish(&full), Err(flags::Error::Capacity));
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &full).unwrap(),
        None
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000006"
    );
    sql(fixture, "DELETE FROM td_mta.memberships WHERE mailbox_id=decode(repeat('ab',16),'hex'); DELETE FROM td_mta.mailboxes WHERE id=decode(repeat('ab',16),'hex')");
    let full_result = publish(&full).unwrap();
    assert_eq!(
        full_result.state,
        flags::Outcome {
            old_sequence: 6,
            new_sequence: 7
        }
    );
    assert_eq!(full_result.results, vec![flags::UpdateResult::Updated; 256]);
    assert_eq!(publish(&full).unwrap(), full_result);
    assert_eq!(sql(fixture, "SELECT kind,count(*) FROM td_mta.changes WHERE sequence=decode('0000000000000007','hex') GROUP BY kind ORDER BY kind"), "1|256\n3|256");
    let noop = flags::prepare_batch(binding, device, &updates, Some(7)).unwrap();
    assert_eq!(
        publish(&noop).unwrap().state,
        flags::Outcome {
            old_sequence: 7,
            new_sequence: 7
        }
    );
    let mut clear = updates.clone();
    for update in &mut clear {
        update.seen = Some(false);
        update.flagged = Some(false);
    }
    let uncertain = flags::prepare_batch(binding, device, &clear, Some(7)).unwrap();
    assert_eq!(
        publish(&flags::prepare_batch(binding, device, &clear, Some(6)).unwrap()),
        Err(flags::Error::State)
    );
    sql(fixture, "CREATE TRIGGER reject_flags BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_flags()");
    assert_eq!(publish(&uncertain), Err(flags::Error::Database));
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000007"
    );
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.keywords WHERE email_id IN (SELECT id FROM td_mta.emails WHERE id<>decode('{email}','hex'))")), "512");
    sql(fixture, "DROP TRIGGER reject_flags ON td_mta.flag_receipts");

    sql(fixture, "CREATE CONSTRAINT TRIGGER delay_flags AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_flags()");
    assert_eq!(
        flags::publish_batch(
            request,
            trust,
            Instant::now() + Duration::from_secs(2),
            &uncertain,
            password
        ),
        Err(flags::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(fixture, "DROP TRIGGER delay_flags ON td_mta.flag_receipts");
    let recovered = flags::inspect_batch(request, trust, deadline(), &uncertain)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.state,
        flags::Outcome {
            old_sequence: 7,
            new_sequence: 8
        }
    );
    assert_eq!(recovered.results, vec![flags::UpdateResult::Updated; 256]);
    assert_eq!(publish(&uncertain).unwrap(), recovered);
    let partial = flags::prepare_batch(
        binding,
        device,
        &[
            updates[0],
            flags::Update {
                email: EmailId::from_bytes([0xee; 16]),
                ..update
            },
        ],
        Some(8),
    )
    .unwrap();
    let partial_result = publish(&partial).unwrap();
    assert_eq!(
        partial_result.state,
        flags::Outcome {
            old_sequence: 8,
            new_sequence: 9
        }
    );
    assert_eq!(
        partial_result.results,
        [flags::UpdateResult::Updated, flags::UpdateResult::NotFound]
    );
    assert_eq!(publish(&partial).unwrap(), partial_result);
    sql(fixture, &format!("UPDATE td_mta.flag_receipts SET results=decode('0103','hex') WHERE operation=decode('{}','hex')", mixed.operation()));
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &mixed),
        Err(flags::Error::Integrity)
    );

    let corrupt = format!("UPDATE td_mta.flag_receipts SET results=decode('03','hex') WHERE operation=decode('{}','hex')", empty.operation());
    sql(fixture, &corrupt);
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &empty),
        Err(flags::Error::Integrity)
    );
}

fn exercise_downloads(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        postgres_body as bodies, postgres_devices as devices, postgres_uploads as uploads,
    };
    let account = binding.account();
    let owner = devices::prepare_creation(binding.epoch(), account, "download owner", 1).unwrap();
    let other = devices::prepare_creation(binding.epoch(), account, "other device", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &owner).unwrap();
    devices::publish_creation(request, trust, deadline(), &other).unwrap();
    let upload = |bytes: &[u8]| {
        let prepared = uploads::prepare(binding, owner.device_id(), bytes, deadline()).unwrap();
        uploads::publish(request, trust, deadline(), &prepared, owner.password()).unwrap()
    };
    // Verification and replay share the same ten-second maximum as Email reads.
    let read_deadline = || Instant::now() + Duration::from_secs(10);
    let verify = |blob, maximum, password: &[u8]| {
        bodies::with_blob(
            request,
            trust,
            read_deadline(),
            bodies::BlobRequest {
                binding,
                blob,
                maximum,
            },
            password,
            |metadata, body| {
                assert_eq!(metadata.identity.account, account);
                assert_eq!(metadata.identity.epoch, binding.epoch());
                assert_eq!(metadata.blob, blob);
                let mut buffer = [0; 65536];
                let mut offset = 0;
                while offset < body.len() {
                    let count = body.read_at(offset, &mut buffer)?;
                    assert!(count > 0);
                    assert!(buffer[..count].iter().all(|byte| *byte == 0x42));
                    offset += count as u64;
                }
                Ok(metadata.body.length)
            },
        )
    };
    let mut retained = None;
    for length in [0, 65537, uploads::MAX_BYTES] {
        let published = upload(&vec![0x42; length]);
        let start = Instant::now();
        assert_eq!(
            verify(published.blob, length as u64, owner.password()).unwrap(),
            length as u64
        );
        eprintln!(
            "blob download length={length}: verified and replayed at {:?}",
            start.elapsed()
        );
        assert_eq!(
            verify(published.blob, length as u64, other.password()),
            Err(bodies::Error::Missing)
        );
        if length > 0 {
            assert_eq!(
                verify(published.blob, length as u64 - 1, owner.password()),
                Err(bodies::Error::Capacity)
            );
        }
        retained = Some(published);
    }
    let retained = retained.unwrap();
    assert_eq!(
        verify(retained.blob, bodies::MAX_BODY_BYTES, b"invalid"),
        Err(bodies::Error::Unauthenticated)
    );
    assert_eq!(
        verify(retained.blob, bodies::MAX_BODY_BYTES + 1, owner.password()),
        Err(bodies::Error::Capacity)
    );
    assert_eq!(
        verify(
            td_mta::ids::BlobId::from_bytes([0; 16]),
            1,
            owner.password()
        ),
        Err(bodies::Error::Missing)
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(
        verify(retained.blob, bodies::MAX_BODY_BYTES, owner.password()),
        Err(bodies::Error::Policy)
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let small = upload(&vec![0x42; 1024 * 1024 + 1]);
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.leases SET expires_at=0 WHERE blob_id=decode('{}','hex')",
            small.blob
        ),
    );
    assert_eq!(
        verify(small.blob, bodies::MAX_BODY_BYTES, owner.password()),
        Err(bodies::Error::Missing)
    );
    // A live Email reference admits all current account devices despite expiry.
    sql(fixture,&format!("INSERT INTO td_mta.threads VALUES(decode('{account}','hex'),decode(repeat('81',16),'hex'),decode('{:016x}','hex')); INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES(decode('{account}','hex'),decode(repeat('82',16),'hex'),decode('{}','hex'),decode(repeat('81',16),'hex'),0,2,decode('{:016x}','hex'))",small.new_sequence,small.blob,small.new_sequence));
    assert_eq!(
        verify(small.blob, bodies::MAX_BODY_BYTES, other.password()).unwrap(),
        small.length
    );
    // A retained transmission reference independently admits the same body.
    sql(fixture,&format!("INSERT INTO td_mta.submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,recipient_count,notification,changed) VALUES(decode('{account}','hex'),decode(repeat('83',16),'hex'),decode(repeat('82',16),'hex'),decode(repeat('81',16),'hex'),decode(repeat('84',16),'hex'),decode('{}','hex'),'sender@example.test',0,1,1,0,decode('{:016x}','hex')); DELETE FROM td_mta.emails",small.blob,small.new_sequence));
    assert_eq!(
        verify(small.blob, bodies::MAX_BODY_BYTES, other.password()).unwrap(),
        small.length
    );
    sql(fixture,&format!("UPDATE td_mta.blob_chunks SET bytes=decode(repeat('43',octet_length(bytes)),'hex') WHERE blob_id=decode('{}','hex') AND ordinal=0",small.blob));
    assert_eq!(
        bodies::with_blob(
            request,
            trust,
            read_deadline(),
            bodies::BlobRequest {
                binding,
                blob: small.blob,
                maximum: bodies::MAX_BODY_BYTES
            },
            owner.password(),
            |_, _| panic!("corrupt bytes reached callback")
        ),
        Err::<(), _>(bodies::Error::Read(td_mta::ports::Error::Corrupt))
    );
    sql(fixture,&format!("UPDATE td_mta.blob_chunks SET bytes=decode(repeat('42',octet_length(bytes)),'hex') WHERE blob_id=decode('{}','hex') AND ordinal=0",small.blob));
    // An admitted callback keeps its snapshot across physical row retirement.
    bodies::with_blob(request,trust,read_deadline(),bodies::BlobRequest { binding,blob:small.blob,maximum:bodies::MAX_BODY_BYTES },owner.password(),|_,body| {
        sql(fixture,&format!("BEGIN; DELETE FROM td_mta.submissions; DELETE FROM td_mta.leases WHERE blob_id=decode('{}','hex'); DELETE FROM td_mta.blobs WHERE id=decode('{}','hex'); COMMIT",small.blob,small.blob));
        let mut bytes=[0;32];
        assert_eq!(body.read_at(0,&mut bytes)?,32);
        assert_eq!(bytes,[0x42;32]);
        Ok(())
    }).unwrap();
    assert_eq!(
        verify(small.blob, bodies::MAX_BODY_BYTES, owner.password()),
        Err(bodies::Error::Missing)
    );
    let revoked =
        devices::prepare_revocation(binding.epoch(), account, owner.device_id(), 2).unwrap();
    // Hold only the body relation: authentication establishes the old snapshot
    // before the paused manifest query, then revocation commits independently.
    use std::io::Write;
    let mut blocker = ProbeChild(
        sql_command(fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = blocker.0.stdin.take().unwrap();
    input
        .write_all(b"BEGIN; LOCK td_mta.blob_chunks IN ACCESS EXCLUSIVE MODE;\n")
        .unwrap();
    input.flush().unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while sql(fixture,"SELECT count(*) FROM pg_locks WHERE relation='td_mta.blob_chunks'::regclass AND mode='AccessExclusiveLock' AND granted")!="1" {
        assert!(Instant::now()<until);
        thread::sleep(Duration::from_millis(10));
    }
    let race_started = Instant::now();
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            bodies::with_blob(
                request,
                trust,
                read_deadline(),
                bodies::BlobRequest {
                    binding,
                    blob: retained.blob,
                    maximum: bodies::MAX_BODY_BYTES,
                },
                owner.password(),
                |_, _| panic!("revoked snapshot reached callback"),
            )
        });
        let until = Instant::now() + Duration::from_secs(2);
        while sql(fixture,"SELECT count(*) FROM pg_stat_activity WHERE application_name='td-mta-postgres' AND wait_event_type='Lock' AND query LIKE 'SELECT a.sequence,a.floor,b.length%'")!="1" {
            assert!(Instant::now()<until);
            thread::sleep(Duration::from_millis(10));
        }
        devices::publish_revocation(request, trust, deadline(), &revoked).unwrap();
        input.write_all(b"COMMIT;\n\\q\n").unwrap();
        input.flush().unwrap();
        eprintln!(
            "revocation barrier released after {:?} (including reader setup)",
            race_started.elapsed()
        );
        assert_eq!(
            reader.join().unwrap(),
            Err::<(), _>(bodies::Error::Unauthenticated)
        );
    });
    drop(input);
    wait_command(&mut blocker);
    assert_eq!(
        verify(retained.blob, bodies::MAX_BODY_BYTES, owner.password()),
        Err(bodies::Error::Unauthenticated)
    );
}

fn exercise_uploads(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_crypto::Digest;
    use td_mta::{postgres_devices as devices, postgres_uploads as uploads};
    let account = binding.account();
    let creation =
        devices::prepare_creation(binding.epoch(), account, "upload fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let device = creation.device_id();
    let password = creation.password();
    let prepare = |bytes: &[u8]| uploads::prepare(binding, device, bytes, deadline()).unwrap();
    let publish =
        |upload: &uploads::Upload| uploads::publish(request, trust, deadline(), upload, password);
    assert_eq!(binding.upload_maximum(), uploads::MAX_BYTES);
    assert_eq!(binding.upload_expiry_seconds(), 86400);
    assert!(matches!(
        uploads::prepare(
            binding,
            device,
            &vec![0; uploads::MAX_BYTES + 1],
            deadline()
        ),
        Err(uploads::Error::Capacity)
    ));
    assert!(matches!(
        uploads::prepare(binding, device, b"x", Instant::now()),
        Err(uploads::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    ));
    let first = prepare(b"");
    assert_eq!(
        uploads::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    assert_eq!(
        uploads::publish(request, trust, deadline(), &first, b"invalid"),
        Err(uploads::Error::Unauthenticated)
    );
    let first_outcome = publish(&first).unwrap();
    assert_eq!(first_outcome.old_sequence, 1);
    assert_eq!(first_outcome.new_sequence, 2);
    assert_eq!(publish(&first).unwrap(), first_outcome);
    let mut total = 0usize;
    let mut sequence = 2;
    for length in [1, 65536, 65537, uploads::MAX_BYTES] {
        let bytes = vec![0x42; length];
        let upload = prepare(&bytes);
        let mut digest = Provider.sha256().unwrap();
        digest.update(&bytes).unwrap();
        let digest = digest
            .finish()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let start = Instant::now();
        let outcome = publish(&upload).unwrap();
        eprintln!("upload length={length}: committed at {:?}", start.elapsed());
        assert_eq!(outcome.old_sequence, sequence);
        sequence += 1;
        assert_eq!(outcome.new_sequence, sequence);
        assert_eq!(outcome.blob, upload.blob());
        assert_eq!(sql(fixture,&format!("SELECT encode(digest,'hex') FROM td_mta.blobs WHERE account=decode('{account}','hex') AND id=decode('{}','hex')",upload.blob())),digest);
        assert_eq!(outcome.length, length as u64);
        assert_eq!(outcome.expires_at - outcome.created_at, 86400000);
        assert_eq!(publish(&upload).unwrap(), outcome);
        assert_eq!(sql(fixture,&format!("SELECT count(*),coalesce(sum(octet_length(bytes)),0),coalesce(bool_and(bytes=decode(repeat('42',octet_length(bytes)),'hex')),true) FROM td_mta.blob_chunks WHERE account=decode('{account}','hex') AND blob_id=decode('{}','hex')",upload.blob())),format!("{}|{length}|t",length.div_ceil(65536)));
        total += length;
    }
    assert_eq!(sql(fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),format!("{total}|5|{total}|0|0"));
    let tiny = prepare(b"x");
    let restore = format!("UPDATE td_mta.account_usage SET body_bytes=(SELECT coalesce(sum(length),0) FROM td_mta.blobs),blob_count=(SELECT count(*) FROM td_mta.blobs),upload_bytes=(SELECT coalesce(sum(length),0) FROM td_mta.blobs),queue_bytes=0,queue_submissions=0 WHERE account=decode('{account}','hex')");
    let caps = binding.quotas();
    for assignments in [
        format!("body_bytes={}", caps.body_bytes),
        format!("blob_count={}", caps.blob_count),
        format!(
            "body_bytes={},upload_bytes={}",
            caps.upload_bytes, caps.upload_bytes
        ),
        format!(
            "body_bytes={},queue_bytes={}",
            caps.queue_bytes + 1,
            caps.queue_bytes + 1
        ),
        format!("queue_submissions={}", caps.queue_submissions + 1),
    ] {
        sql(
            fixture,
            &format!("UPDATE td_mta.account_usage SET {assignments}"),
        );
        assert_eq!(publish(&tiny), Err(uploads::Error::Quota));
        assert_eq!(
            uploads::inspect(request, trust, deadline(), &tiny).unwrap(),
            None
        );
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blobs"), "5");
        sql(fixture, &restore);
    }
    sql(fixture,"CREATE FUNCTION public.reject_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject upload'; END $$; CREATE TRIGGER reject_upload BEFORE INSERT ON td_mta.upload_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_upload()");
    assert_eq!(publish(&tiny), Err(uploads::Error::Database));
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes,blob_count,upload_bytes FROM td_mta.account_usage"
        ),
        format!("{total}|5|{total}")
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_ids"), "5");
    sql(
        fixture,
        "DROP TRIGGER reject_upload ON td_mta.upload_receipts",
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(publish(&tiny), Err(uploads::Error::Policy));
    assert_eq!(publish(&first).unwrap(), first_outcome);
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    sql(fixture,"CREATE FUNCTION public.delay_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_upload AFTER INSERT ON td_mta.upload_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_upload()");
    assert_eq!(
        uploads::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(2),
            &tiny,
            password
        ),
        Err(uploads::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_upload ON td_mta.upload_receipts",
    );
    let recovered = uploads::inspect(request, trust, deadline(), &tiny)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.old_sequence, sequence);
    sequence += 1;
    assert_eq!(recovered.new_sequence, sequence);
    assert_eq!(publish(&tiny).unwrap(), recovered);
    total += 1;
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET blob_count={}",
            caps.blob_count - 1
        ),
    );
    let a = prepare(b"a");
    let b = prepare(b"b");
    let outcomes = thread::scope(|scope| {
        let a = scope.spawn(|| publish(&a));
        let b = scope.spawn(|| publish(&b));
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == Err(uploads::Error::Quota))
            .count(),
        1
    );
    sequence += 1;
    total += 1;
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes,blob_count,upload_bytes FROM td_mta.account_usage"
        ),
        format!("{total}|{}|{total}", caps.blob_count)
    );
    sql(fixture, &restore);
    let capacity = prepare(b"capacity");
    sql(fixture,&format!("INSERT INTO td_mta.upload_receipts SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',16),'hex'),0,0,1,decode('0000000000000000','hex'),decode('0000000000000001','hex') FROM generate_series(1,1024) n"));
    assert_eq!(publish(&capacity), Err(uploads::Error::Capacity));
    assert_eq!(publish(&first).unwrap(), first_outcome);
    sql(
        fixture,
        "DELETE FROM td_mta.upload_receipts WHERE request_digest=decode(repeat('00',32),'hex')",
    );
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('ffffffffffffffff','hex')",
    );
    assert_eq!(publish(&capacity), Err(uploads::Error::Capacity));
    assert_eq!(publish(&first).unwrap(), first_outcome);
    sql(
        fixture,
        &format!("UPDATE td_mta.accounts SET sequence=decode('{sequence:016x}','hex')"),
    );
    let revoke = devices::prepare_revocation(binding.epoch(), account, device, 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(publish(&capacity), Err(uploads::Error::Unauthenticated));
    assert_eq!(publish(&first).unwrap(), first_outcome);
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.upload_receipts SET length=1 WHERE operation=decode('{}','hex')",
            first.operation()
        ),
    );
    assert_eq!(
        uploads::inspect(request, trust, deadline(), &first),
        Err(uploads::Error::Integrity)
    );
}

fn exercise_drafts(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        compose_text::{Address, Message},
        ids::MailboxId,
        postgres_body as body, postgres_devices as devices, postgres_drafts as drafts,
    };
    let account = binding.account();
    let creation = devices::prepare_creation(binding.epoch(), account, "draft fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let device = creation.device_id();
    let password = creation.password();
    let mailbox = MailboxId::from_bytes([0x21; 16]);
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes SELECT id,decode('{mailbox}','hex'),'Drafts',NULL,NULL,0,1,sequence FROM td_mta.accounts"));
    let message = |id, refs| Message {
        from: Address {
            name: "Sender",
            email: "sender@example.test",
        },
        to: &[],
        cc: &[],
        bcc: &[],
        reply_to: &[],
        subject: "Draft subject",
        message_id: id,
        in_reply_to: &[],
        references: refs,
        sent_at_ms: 0,
        text: "hello\nworld",
    };
    let prepare = |message: &Message<'_>, expected| {
        drafts::prepare(
            drafts::DraftRequest {
                binding,
                device,
                mailbox,
                expected,
            },
            message,
            deadline(),
        )
        .unwrap()
    };
    let publish =
        |draft: &drafts::Draft| drafts::publish(request, trust, deadline(), draft, password);
    let first = prepare(&message("first@example.test", &[]), Some(1));
    assert_eq!(
        drafts::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    assert_eq!(
        drafts::publish(request, trust, deadline(), &first, b"invalid"),
        Err(drafts::Error::Unauthenticated)
    );
    let outcome = publish(&first).unwrap();
    assert_eq!((outcome.old_sequence, outcome.new_sequence), (1, 2));
    assert_eq!(publish(&first).unwrap(), outcome);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.draft_receipts"),
        "1"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(keyword,',' ORDER BY keyword) FROM td_mta.keywords"
        ),
        "$draft,$seen"
    );
    assert_eq!(sql(fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),format!("{}|1|0|0|0",outcome.length));
    assert_eq!(sql(fixture,"SELECT string_agg(kind::text || ':' || action::text,',' ORDER BY operation) FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex')"),"3:1,2:1,1:2");
    let bytes = body::with_email(
        request,
        trust,
        deadline(),
        body::ReadRequest {
            epoch: binding.epoch(),
            account,
            email: outcome.email,
            maximum: 32 * 1024 * 1024,
        },
        password,
        |metadata, reader| {
            assert_eq!(metadata.blob, outcome.blob);
            assert_eq!(metadata.thread, outcome.thread);
            let mut bytes = vec![0; outcome.length as usize];
            let mut position = 0;
            while position < bytes.len() {
                let n = reader.read_at(position as u64, &mut bytes[position..])?;
                assert!(n > 0);
                position += n;
            }
            Ok(bytes)
        },
    )
    .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("Message-ID:\r\n <first@example.test>"));
    assert!(text.ends_with("aGVsbG8NCndvcmxk\r\n"));
    let reply_refs = ["first@example.test"];
    let reply = prepare(&message("reply@example.test", &reply_refs), Some(2));
    let reply_outcome = publish(&reply).unwrap();
    assert_eq!(reply_outcome.thread, outcome.thread);
    assert_eq!(sql(fixture,"SELECT string_agg(kind::text || ':' || action::text,',' ORDER BY operation) FROM td_mta.changes WHERE sequence=decode('0000000000000003','hex')"),"3:1,2:2,1:2");
    let other = prepare(&message("other@example.test", &[]), None);
    let other_outcome = publish(&other).unwrap();
    assert_ne!(other_outcome.thread, outcome.thread);
    let nearest_refs = ["first@example.test", "other@example.test"];
    let nearest = prepare(&message("nearest@example.test", &nearest_refs), None);
    assert_eq!(publish(&nearest).unwrap().thread, other_outcome.thread);
    let duplicate = prepare(&message("first@example.test", &[]), None);
    assert_eq!(publish(&duplicate).unwrap().thread, outcome.thread);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.threads"), "2");
    let stale = prepare(&message("stale@example.test", &[]), Some(1));
    assert_eq!(publish(&stale), Err(drafts::Error::State));
    let missing = drafts::prepare(
        drafts::DraftRequest {
            binding,
            device,
            mailbox: MailboxId::from_bytes([0x22; 16]),
            expected: None,
        },
        &message("missing@example.test", &[]),
        deadline(),
    )
    .unwrap();
    assert_eq!(publish(&missing), Err(drafts::Error::MissingMailbox));
    assert!(matches!(
        drafts::prepare(
            drafts::DraftRequest {
                binding,
                device,
                mailbox,
                expected: None
            },
            &message("bad\r\nid@example.test", &[]),
            deadline()
        ),
        Err(drafts::Error::Compose(_))
    ));
    let pending = prepare(&message("pending@example.test", &[]), None);
    let usage=sql(fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage");
    let restore="UPDATE td_mta.account_usage SET body_bytes=(SELECT sum(length) FROM td_mta.blobs),blob_count=(SELECT count(*) FROM td_mta.blobs)";
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET blob_count={}",
            binding.quotas().blob_count
        ),
    );
    assert_eq!(publish(&pending), Err(drafts::Error::Quota));
    sql(fixture, restore);
    sql(fixture,"CREATE FUNCTION public.reject_draft() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject draft'; END $$; CREATE TRIGGER reject_draft BEFORE INSERT ON td_mta.draft_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_draft()");
    assert_eq!(publish(&pending), Err(drafts::Error::Database));
    assert_eq!(sql(fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),usage);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_ids"), "5");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "5");
    assert_eq!(
        drafts::inspect(request, trust, deadline(), &pending).unwrap(),
        None
    );
    sql(fixture,"DROP TRIGGER reject_draft ON td_mta.draft_receipts; UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')");
    assert_eq!(publish(&pending), Err(drafts::Error::Policy));
    assert_eq!(publish(&first).unwrap(), outcome);
    sql(fixture,"UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex'); CREATE FUNCTION public.delay_draft() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_draft AFTER INSERT ON td_mta.draft_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_draft()");
    assert_eq!(
        drafts::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(2),
            &pending,
            password
        ),
        Err(drafts::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(fixture, "DROP TRIGGER delay_draft ON td_mta.draft_receipts");
    let recovered = drafts::inspect(request, trust, deadline(), &pending)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.new_sequence, 7);
    assert_eq!(publish(&pending).unwrap(), recovered);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "6");
    let capacity = prepare(&message("capacity@example.test", &[]), None);
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('ffffffffffffffff','hex')",
    );
    assert_eq!(publish(&capacity), Err(drafts::Error::Capacity));
    assert_eq!(publish(&first).unwrap(), outcome);
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('0000000000000007','hex')",
    );
    sql(fixture,&format!("INSERT INTO td_mta.draft_receipts SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',16),'hex'),decode(repeat('00',16),'hex'),decode(repeat('00',16),'hex'),decode(repeat('00',16),'hex'),0,0,decode('0000000000000000','hex'),decode('0000000000000001','hex') FROM generate_series(1,1024) n"));
    assert_eq!(publish(&capacity), Err(drafts::Error::Capacity));
    assert_eq!(publish(&first).unwrap(), outcome);
    sql(
        fixture,
        "DELETE FROM td_mta.draft_receipts WHERE request_digest=decode(repeat('00',32),'hex')",
    );
    let revoke = devices::prepare_revocation(binding.epoch(), account, device, 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(publish(&capacity), Err(drafts::Error::Unauthenticated));
    assert_eq!(publish(&first).unwrap(), outcome);
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.draft_receipts SET length=length+1 WHERE operation=decode('{}','hex')",
            first.operation()
        ),
    );
    assert_eq!(publish(&first), Err(drafts::Error::Integrity));
}

fn exercise_changes(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        format::Sequence,
        jmap_changes,
        ports::ChangeAction,
        postgres_changes as changes, postgres_devices as devices,
        sync::{DataState, DataType},
    };
    let account = binding.account();
    let creation =
        devices::prepare_creation(binding.epoch(), account, "change fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let state = |kind, sequence| DataState {
        account,
        epoch: binding.epoch(),
        kind,
        sequence: Sequence::from_u64(sequence),
    };
    let read = |kind, since, maximum| {
        changes::read(
            request,
            trust,
            deadline(),
            binding,
            password,
            state(kind, since),
            maximum,
        )
    };
    let empty = read(DataType::Email, 1, 10).unwrap();
    assert!(empty.changes.is_empty());
    assert_eq!(empty.new_state.sequence.number(), 1);
    assert!(!empty.has_more);
    let bootstrap = read(DataType::Mailbox, 0, 10).unwrap();
    assert_eq!(bootstrap.changes.len(), 1);
    assert_eq!(bootstrap.changes[0].action, ChangeAction::Created);
    sql(fixture,&format!("INSERT INTO td_mta.changes VALUES (decode('{account}','hex'),decode('0000000000000002','hex'),0,3,1,decode(repeat('a1',16),'hex')),(decode('{account}','hex'),decode('0000000000000003','hex'),0,3,2,decode(repeat('a1',16),'hex')),(decode('{account}','hex'),decode('0000000000000004','hex'),0,3,1,decode(repeat('b1',16),'hex')),(decode('{account}','hex'),decode('0000000000000005','hex'),0,3,3,decode(repeat('b1',16),'hex')); UPDATE td_mta.accounts SET sequence=decode('0000000000000006','hex')"));
    let folded = read(DataType::Email, 1, 10).unwrap();
    assert_eq!(folded.changes.len(), 1);
    assert_eq!(folded.changes[0].id, [0xa1; 16]);
    assert_eq!(folded.changes[0].action, ChangeAction::Created);
    assert_eq!(folded.new_state.sequence.number(), 6);
    let first = read(DataType::Email, 1, 1).unwrap();
    assert!(first.has_more);
    assert_eq!(first.new_state.sequence.number(), 3);
    let second = read(DataType::Email, 3, 1).unwrap();
    assert!(!second.has_more);
    assert!(second.changes.is_empty());
    assert_eq!(second.new_state.sequence.number(), 6);
    assert!(matches!(
        read(DataType::Email, 7, 10),
        Err(changes::Error::Page(
            jmap_changes::Error::CannotCalculateChanges
        ))
    ));
    let mut wrong = state(DataType::Email, 1);
    wrong.epoch = td_mta::ids::StoreEpoch::from_bytes([0x99; 16]);
    assert!(matches!(
        changes::read(request, trust, deadline(), binding, password, wrong, 10),
        Err(changes::Error::Page(
            jmap_changes::Error::CannotCalculateChanges
        ))
    ));
    sql(fixture,&format!("INSERT INTO td_mta.changes SELECT decode('{account}','hex'),pg_catalog.int8send(10+n),0,3,1,decode(lpad(to_hex(n),32,'0'),'hex') FROM generate_series(1::bigint,1025::bigint) n; UPDATE td_mta.accounts SET sequence=pg_catalog.int8send(1035::bigint)"));
    let page = read(DataType::Email, 10, 1024).unwrap();
    assert_eq!(page.changes.len(), 1024);
    assert!(page.has_more);
    assert_eq!(page.new_state.sequence.number(), 1034);
    let final_page = read(DataType::Email, 1034, 1024).unwrap();
    assert_eq!(final_page.changes.len(), 1);
    assert!(!final_page.has_more);
    assert_eq!(final_page.new_state.sequence.number(), 1035);
    sql(fixture,&format!("INSERT INTO td_mta.changes SELECT decode('{account}','hex'),pg_catalog.int8send(2000::bigint),n,3,1,decode(repeat(CASE WHEN n=0 THEN 'c1' ELSE 'c2' END,16),'hex') FROM generate_series(0,1) n; UPDATE td_mta.accounts SET sequence=pg_catalog.int8send(2000::bigint)"));
    assert!(matches!(
        read(DataType::Email, 1035, 1),
        Err(changes::Error::Page(
            jmap_changes::Error::CannotCalculateChanges
        ))
    ));
    let group = read(DataType::Email, 1035, 2).unwrap();
    assert_eq!(group.changes.len(), 2);
    assert_eq!(group.new_state.sequence.number(), 2000);
    sql(fixture,&format!("INSERT INTO td_mta.changes SELECT decode('{account}','hex'),pg_catalog.int8send(2000+n),0,1,2,(SELECT id FROM td_mta.mailboxes ORDER BY id LIMIT 1) FROM generate_series(1::bigint,9000::bigint) n; UPDATE td_mta.accounts SET sequence=pg_catalog.int8send(11000::bigint)"));
    assert!(matches!(
        read(DataType::Mailbox, 2000, 10),
        Err(changes::Error::Page(
            jmap_changes::Error::CannotCalculateChanges
        ))
    ));
    sql(
        fixture,
        "UPDATE td_mta.accounts SET floor=pg_catalog.int8send(1500::bigint)",
    );
    assert!(matches!(
        read(DataType::Email, 1035, 10),
        Err(changes::Error::Page(
            jmap_changes::Error::CannotCalculateChanges
        ))
    ));
    sql(fixture,&format!("INSERT INTO td_mta.changes VALUES(decode('{account}','hex'),decode('fffffffffffffffe','hex'),0,3,1,decode(repeat('fe',16),'hex')); UPDATE td_mta.accounts SET sequence=decode('ffffffffffffffff','hex')"));
    let high = read(DataType::Email, u64::MAX - 2, 10).unwrap();
    assert_eq!(high.changes.len(), 1);
    assert_eq!(high.new_state.sequence.number(), u64::MAX);
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('fffffffffffffffd','hex')",
    );
    assert!(matches!(
        read(DataType::Email, u64::MAX - 2, 10),
        Err(changes::Error::Page(jmap_changes::Error::Store(
            td_mta::ports::Error::Corrupt
        )))
    ));
    sql(fixture,"UPDATE td_mta.accounts SET sequence=decode('ffffffffffffffff','hex'); UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')");
    assert!(matches!(
        read(DataType::Email, u64::MAX - 2, 10),
        Err(changes::Error::Policy)
    ));
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let revoke =
        devices::prepare_revocation(binding.epoch(), account, creation.device_id(), 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert!(matches!(
        read(DataType::Email, u64::MAX - 2, 10),
        Err(changes::Error::Unauthenticated)
    ));
}

fn exercise_read(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        compose_text::{Address, Message},
        ids::{EmailId, MailboxId},
        postgres_body, postgres_devices as devices, postgres_drafts as drafts,
        postgres_read as read,
    };
    let account = binding.account();
    let creation = devices::prepare_creation(binding.epoch(), account, "read fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let mailbox = MailboxId::from_bytes([0x33; 16]);
    sql(fixture,&format!("INSERT INTO td_mta.mailboxes SELECT id,decode('{mailbox}','hex'),'Drafts',NULL,NULL,0,1,sequence FROM td_mta.accounts"));
    let create = |id| {
        let message = Message {
            from: Address {
                name: "Sender",
                email: "sender@example.test",
            },
            to: &[],
            cc: &[],
            bcc: &[],
            reply_to: &[],
            subject: "Snapshot",
            message_id: id,
            in_reply_to: &[],
            references: &[],
            sent_at_ms: 0,
            text: "snapshot text",
        };
        let draft = drafts::prepare(
            drafts::DraftRequest {
                binding,
                device: creation.device_id(),
                mailbox,
                expected: None,
            },
            &message,
            deadline(),
        )
        .unwrap();
        drafts::publish(request, trust, deadline(), &draft, password).unwrap()
    };
    let first = create("first@example.test");
    let second = create("second@example.test");
    let snapshot = |callback: fn(&mut read::Snapshot<'_>) -> Result<(), read::Error>| {
        read::with_snapshot(request, trust, deadline(), binding, password, callback)
    };
    assert!(matches!(
        read::with_snapshot(request, trust, deadline(), binding, b"bad", |_| Ok(())),
        Err(read::Error::Unauthenticated)
    ));
    read::with_snapshot(request,trust,deadline(),binding,password,|view| {
        assert_eq!(view.identity().committed_sequence.number(),3);
        let metadata=view.email(first.email)?.unwrap();
        assert_eq!(metadata.blob,first.blob);
        assert_eq!(view.email(EmailId::from_bytes([0xee;16]))?,None);
        let boxes=view.mailboxes(first.email,None,1)?;
        assert_eq!(boxes.values,vec![mailbox]);
        assert!(!boxes.has_more);
        assert!(view.mailboxes(first.email,Some(mailbox),1)?.values.is_empty());
        let first_keyword=view.keywords(first.email,None,1)?;
        assert_eq!(first_keyword.values,vec!["$draft"]);
        assert!(first_keyword.has_more);
        let last_keyword=view.keywords(first.email,Some("$draft"),1)?;
        assert_eq!(last_keyword.values,vec!["$seen"]);
        assert!(!last_keyword.has_more);
        sql(fixture,&format!("INSERT INTO td_mta.keywords VALUES(decode('{account}','hex'),decode('{}','hex'),'new',decode('0000000000000004','hex')); UPDATE td_mta.accounts SET sequence=decode('0000000000000004','hex')",first.email));
        assert_eq!(view.keywords(first.email,None,256)?.values,vec!["$draft","$seen"]);
        sql(fixture,&format!("BEGIN; DELETE FROM td_mta.memberships WHERE email_id=decode('{}','hex'); DELETE FROM td_mta.keywords WHERE email_id=decode('{}','hex'); DELETE FROM td_mta.thread_anchors WHERE email_id=decode('{}','hex'); DELETE FROM td_mta.emails WHERE id=decode('{}','hex'); DELETE FROM td_mta.blobs WHERE id=decode('{}','hex'); DELETE FROM td_mta.threads WHERE id=decode('{}','hex'); COMMIT",first.email,first.email,first.email,first.email,first.blob,first.thread));
        let bytes=view.with_body(first.email,|metadata,body| {
            assert_eq!(metadata.identity.committed_sequence.number(),3);
            let mut bytes=vec![0;metadata.body.length as usize];
            let mut position=0;
            while position<bytes.len() { let n=body.read_at(position as u64,&mut bytes[position..])?;assert!(n>0);position+=n; }
            Ok(bytes)
        })?;
        assert!(String::from_utf8(bytes).unwrap().ends_with("c25hcHNob3QgdGV4dA==\r\n"));
        assert!(view.email(second.email)?.is_some());
        Ok(())
    }).unwrap();
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert_eq!(view.identity().committed_sequence.number(), 4);
        assert_eq!(view.email(first.email)?, None);
        assert!(view.mailboxes(first.email, None, 1)?.values.is_empty());
        Ok(())
    })
    .unwrap();
    sql(fixture,&format!("INSERT INTO td_mta.keywords SELECT decode('{account}','hex'),decode('{}','hex'),'k'||lpad(n::text,3,'0'),decode('0000000000000004','hex') FROM generate_series(0,256) n",second.email));
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let page = view.keywords(second.email, Some("$seen"), 256)?;
        assert_eq!(page.values.len(), 256);
        assert!(page.has_more);
        assert_eq!(page.values.last().unwrap(), "k255");
        let last = view.keywords(second.email, Some("k255"), 256)?;
        assert_eq!(last.values, vec!["k256"]);
        assert!(!last.has_more);
        Ok(())
    })
    .unwrap();
    let invalid = read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert_eq!(
            view.keywords(second.email, Some(""), 1).unwrap_err(),
            read::Error::Invalid
        );
        Ok(())
    });
    assert_eq!(invalid, Err(read::Error::Invalid));
    sql(fixture,&format!("UPDATE td_mta.blob_chunks SET bytes=set_byte(bytes,0,0) WHERE blob_id=decode('{}','hex')",second.blob));
    let corrupt = read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let error = view
            .with_body::<()>(second.email, |_, _| panic!("corrupt body admitted"))
            .unwrap_err();
        assert!(matches!(
            error,
            read::Error::Body(postgres_body::Error::Read(_))
        ));
        Ok(())
    });
    assert!(matches!(
        corrupt,
        Err(read::Error::Body(postgres_body::Error::Read(_)))
    ));
    let killed = read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert!(view.email(second.email)?.is_some());
        sql(fixture,"SELECT pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND pid<>pg_backend_pid()");
        thread::sleep(Duration::from_millis(100));
        let _ = view.check();
        Ok(())
    });
    assert!(killed.is_err());
    let late = read::with_snapshot(
        request,
        trust,
        Instant::now() + Duration::from_secs(2),
        binding,
        password,
        |_| {
            thread::sleep(Duration::from_secs(2));
            Ok(())
        },
    );
    assert_eq!(
        late,
        Err(read::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(snapshot(|_| Ok(())), Err(read::Error::Policy));
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let revoke =
        devices::prepare_revocation(binding.epoch(), account, creation.device_id(), 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(snapshot(|_| Ok(())), Err(read::Error::Unauthenticated));
}

fn exercise_query(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        compose_text::{Address, Message},
        ids::MailboxId,
        postgres_devices as devices, postgres_drafts as drafts, postgres_read as read,
    };
    let account = binding.account();
    let creation = devices::prepare_creation(binding.epoch(), account, "query fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let mailbox = MailboxId::from_bytes([0x33; 16]);
    sql(fixture,&format!("INSERT INTO td_mta.mailboxes SELECT id,decode('{mailbox}','hex'),'Drafts',NULL,NULL,0,1,sequence FROM td_mta.accounts"));
    let create = |id| {
        let message = Message {
            from: Address {
                name: "Sender",
                email: "sender@example.test",
            },
            to: &[],
            cc: &[],
            bcc: &[],
            reply_to: &[],
            subject: "Query",
            message_id: id,
            in_reply_to: &[],
            references: &[],
            sent_at_ms: 0,
            text: "query text",
        };
        let draft = drafts::prepare(
            drafts::DraftRequest {
                binding,
                device: creation.device_id(),
                mailbox,
                expected: None,
            },
            &message,
            deadline(),
        )
        .unwrap();
        drafts::publish(request, trust, deadline(), &draft, password).unwrap()
    };
    let first = create("first@example.test");
    let second = create("second@example.test");
    let third = create("third@example.test");
    sql(fixture,&format!("UPDATE td_mta.emails SET received_at=5; UPDATE td_mta.emails SET received_at=-1 WHERE id=decode('{}','hex'); DELETE FROM td_mta.memberships WHERE email_id=decode('{}','hex')",first.email,third.email));
    let mut ties = vec![second.email, third.email];
    ties.sort_by_key(|id| *id.as_bytes());
    let mut expected = ties.clone();
    expected.push(first.email);
    let query = read::EmailQuery {
        scope: read::EmailScope::All,
        ascending: false,
        position: 0,
        limit: 2,
        calculate_total: true,
    };
    read::with_snapshot(request,trust,deadline(),binding,password,|view| {
        let page=view.query_emails(query)?;
        assert_eq!(page.ids,ties);assert_eq!(page.total,Some(3));assert!(page.has_more);
        assert_eq!(page.state.sequence.number(),4);
        let last=view.query_emails(read::EmailQuery {position:2,..query})?;
        assert_eq!(last.ids,vec![first.email]);assert!(!last.has_more);assert_eq!(last.position,2);
        let mut ascending=vec![first.email];ascending.extend_from_slice(&ties);
        assert_eq!(view.query_emails(read::EmailQuery {ascending:true,limit:3,..query})?.ids,ascending);
        let selected=view.query_emails(read::EmailQuery {scope:read::EmailScope::Mailbox(mailbox),..query})?;
        assert_eq!(selected.ids,vec![second.email,first.email]);assert_eq!(selected.total,Some(2));assert!(!selected.has_more);
        for scope in [read::EmailScope::Empty,read::EmailScope::Mailbox(MailboxId::from_bytes([0xfe;16]))] {
            let empty=view.query_emails(read::EmailQuery {scope,..query})?;
            assert!(empty.ids.is_empty());assert_eq!(empty.total,Some(0));assert!(!empty.has_more);
        }
        let zero=view.query_emails(read::EmailQuery {limit:0,..query})?;
        assert!(zero.ids.is_empty());assert!(zero.has_more);assert_eq!(zero.total,Some(3));
        let huge=view.query_emails(read::EmailQuery {position:u64::MAX,..query})?;
        assert!(huge.ids.is_empty());assert!(!huge.has_more);assert_eq!(huge.total,Some(3));
        assert_eq!(view.query_emails(read::EmailQuery {calculate_total:false,..query})?.total,None);
        sql(fixture,&format!("UPDATE td_mta.emails SET received_at=99,changed=decode('0000000000000005','hex') WHERE id=decode('{}','hex'); UPDATE td_mta.accounts SET sequence=decode('0000000000000005','hex')",first.email));
        assert_eq!(view.query_emails(read::EmailQuery {limit:3,..query})?.ids,expected);
        assert_eq!(view.email(first.email)?.unwrap().identity.committed_sequence.number(),4);
        Ok(())
    }).unwrap();
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let page = view.query_emails(query)?;
        assert_eq!(page.ids.first(), Some(&first.email));
        assert_eq!(page.state.sequence.number(), 5);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        read::with_snapshot(request, trust, deadline(), binding, password, |view| {
            let _ = view.query_emails(read::EmailQuery {
                limit: 1025,
                ..query
            });
            Ok(())
        }),
        Err(read::Error::Capacity)
    );
    sql(fixture,&format!("UPDATE td_mta.emails SET changed=decode('0000000000000006','hex') WHERE id=decode('{}','hex')",first.email));
    assert_eq!(
        read::with_snapshot(request, trust, deadline(), binding, password, |view| {
            let _ = view.query_emails(query);
            Ok(())
        }),
        Err(read::Error::Integrity)
    );
}

fn exercise_threads(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        ids::{EmailId, ThreadId},
        postgres_devices as devices, postgres_read as read,
    };
    let account = binding.account();
    let creation =
        devices::prepare_creation(binding.epoch(), account, "thread fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let thread = ThreadId::from_bytes([0x44; 16]);
    let empty = ThreadId::from_bytes([0x55; 16]);
    // Empty blobs suffice: this fixture exercises member ordering, not bodies.
    sql(fixture,&format!("BEGIN; INSERT INTO td_mta.threads SELECT id,decode('{thread}','hex'),sequence FROM td_mta.accounts; INSERT INTO td_mta.threads SELECT id,decode('{empty}','hex'),sequence FROM td_mta.accounts; INSERT INTO td_mta.blob_ids SELECT id,decode(repeat('66',16),'hex') FROM td_mta.accounts; INSERT INTO td_mta.blobs SELECT id,decode(repeat('66',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,sequence,0 FROM td_mta.accounts; INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT a.id,decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('66',16),'hex'),decode('{thread}','hex'),CASE WHEN n=0 THEN -9223372036854775808 WHEN n=257 THEN 9223372036854775807 ELSE 0 END,2,a.sequence FROM td_mta.accounts a CROSS JOIN generate_series(0,257) n; COMMIT"));
    let unknown = ThreadId::from_bytes([0xee; 16]);
    read::with_snapshot(request,trust,deadline(),binding,password,|view| {
        assert!(view.thread_emails(unknown,None,1)?.is_none());
        let empty=view.thread_emails(empty,None,1)?.unwrap();assert!(empty.emails.is_empty());assert!(!empty.has_more);
        let page=view.thread_emails(thread,None,256)?.unwrap();
        assert_eq!(page.emails.len(),256);assert!(page.has_more);
        assert_eq!(page.emails.first().unwrap().received_at,i64::MIN);
        assert_eq!(page.emails.last().unwrap().email,EmailId::from_bytes(255_u128.to_be_bytes()));
        let last=view.thread_emails(thread,page.emails.last().copied(),256)?.unwrap();
        assert_eq!(last.emails.len(),2);assert!(!last.has_more);
        assert_eq!(last.emails.last().unwrap().received_at,i64::MAX);
        assert!(view.thread_emails(thread,last.emails.last().copied(),1)?.unwrap().emails.is_empty());
        sql(fixture,&format!("BEGIN; DELETE FROM td_mta.emails WHERE account=decode('{account}','hex') AND thread_id=decode('{thread}','hex'); DELETE FROM td_mta.threads WHERE account=decode('{account}','hex') AND id=decode('{thread}','hex'); UPDATE td_mta.accounts SET sequence=decode('0000000000000002','hex'); COMMIT"));
        let again=view.thread_emails(thread,None,256)?.unwrap();
        assert_eq!(again.emails,page.emails);assert_eq!(again.identity,page.identity);
        Ok(())
    }).unwrap();
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert!(view.thread_emails(thread, None, 1)?.is_none());
        assert_eq!(view.identity().committed_sequence.number(), 2);
        Ok(())
    })
    .unwrap();
    for (maximum, error) in [(0, read::Error::Invalid), (257, read::Error::Capacity)] {
        assert_eq!(
            read::with_snapshot(request, trust, deadline(), binding, password, |view| {
                let _ = view.thread_emails(empty, None, maximum);
                Ok(())
            }),
            Err(error)
        );
    }
    sql(fixture,&format!("UPDATE td_mta.threads SET changed=decode('0000000000000003','hex') WHERE id=decode('{empty}','hex')"));
    assert_eq!(
        read::with_snapshot(request, trust, deadline(), binding, password, |view| {
            let _ = view.thread_emails(empty, None, 1);
            Ok(())
        }),
        Err(read::Error::Integrity)
    );
}

fn exercise_folders(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use folders::{Error, ResultEntry};
    use td_mta::{postgres_devices as devices, postgres_mailbox_create as folders};
    let account = binding.account();
    let device = devices::prepare_creation(binding.epoch(), account, "folder fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let password = device.password();
    let prepare = |names: &[&str], expected| {
        folders::prepare(binding, device.device_id(), expected, names, deadline()).unwrap()
    };
    let mixed = prepare(
        &[
            "Inbox", "Projects", "Projects", "", "e\u{301}", "é", "bad\n",
        ],
        Some(1),
    );
    assert_eq!(
        folders::inspect(request, trust, deadline(), &mixed).unwrap(),
        None
    );
    assert_eq!(
        folders::publish(request, trust, deadline(), &mixed, b"bad"),
        Err(Error::Unauthenticated)
    );
    let result = folders::publish(request, trust, deadline(), &mixed, password).unwrap();
    assert_eq!((result.old_sequence, result.new_sequence), (1, 2));
    assert!(matches!(
        result.results.as_slice(),
        [
            ResultEntry::AlreadyExists,
            ResultEntry::Created(_),
            ResultEntry::AlreadyExists,
            ResultEntry::InvalidName,
            ResultEntry::InvalidName,
            ResultEntry::Created(_),
            ResultEntry::InvalidName
        ]
    ));
    assert_eq!(
        folders::publish(request, trust, deadline(), &mixed, b"bad").unwrap(),
        result
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "3");
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex') AND kind=1 AND action=1"),"2");
    let noop = prepare(&["Projects", ""], Some(2));
    let noop = folders::publish(request, trust, deadline(), &noop, password).unwrap();
    assert_eq!((noop.old_sequence, noop.new_sequence), (2, 2));
    assert_eq!(
        folders::publish(
            request,
            trust,
            deadline(),
            &prepare(&["stale"], Some(1)),
            password
        ),
        Err(Error::State)
    );
    let left = prepare(&["Concurrent"], None);
    let right = prepare(&["Concurrent"], None);
    let (left, right) = thread::scope(|scope| {
        let a = scope.spawn(|| folders::publish(request, trust, deadline(), &left, password));
        let b = scope.spawn(|| folders::publish(request, trust, deadline(), &right, password));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    let created =
        |result: &folders::Outcome| matches!(result.results.as_slice(), [ResultEntry::Created(_)]);
    assert_ne!(created(&left), created(&right));
    assert!(
        left.results == [ResultEntry::AlreadyExists]
            || right.results == [ResultEntry::AlreadyExists]
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='Concurrent'"
        ),
        "1"
    );
    let names: Vec<String> = (0..256)
        .map(|n| format!("f{n:03}-{}", "x".repeat(1019)))
        .collect();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let oversized = prepare(&borrowed, None);
    let overflow = folders::publish(request, trust, deadline(), &oversized, password);
    assert!(
        matches!(overflow, Err(Error::Capacity)),
        "oversized folder publication: {overflow:?}"
    );
    assert_eq!(
        folders::inspect(request, trust, deadline(), &oversized).unwrap(),
        None
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "4");
    assert!(matches!(
        folders::prepare(
            binding,
            device.device_id(),
            None,
            &vec!["x"; 257],
            deadline()
        ),
        Err(Error::Capacity)
    ));
    let rejected = prepare(&["rollback"], None);
    sql(fixture,"CREATE FUNCTION public.reject_folder() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject folder'; END $$; CREATE TRIGGER reject_folder BEFORE INSERT ON td_mta.mailbox_create_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_folder()");
    assert_eq!(
        folders::publish(request, trust, deadline(), &rejected, password),
        Err(Error::Database)
    );
    assert_eq!(
        folders::inspect(request, trust, deadline(), &rejected).unwrap(),
        None
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='rollback'"
        ),
        "0"
    );
    sql(fixture,"DROP TRIGGER reject_folder ON td_mta.mailbox_create_receipts; CREATE FUNCTION public.delay_folder() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_folder AFTER INSERT ON td_mta.mailbox_create_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_folder()");
    let uncertain = prepare(&["uncertain"], None);
    assert_eq!(
        folders::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(1),
            &uncertain,
            password
        ),
        Err(Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(3));
    let retained = folders::inspect(request, trust, deadline(), &uncertain)
        .unwrap()
        .unwrap();
    sql(
        fixture,
        "DROP TRIGGER delay_folder ON td_mta.mailbox_create_receipts",
    );
    assert_eq!(
        folders::publish(request, trust, deadline(), &uncertain, password).unwrap(),
        retained
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='uncertain'"
        ),
        "1"
    );
    let count: usize = sql(fixture, "SELECT count(*) FROM td_mta.mailboxes")
        .parse()
        .unwrap();
    let available = td_mta::postgres_mailboxes::MAX_MAILBOXES - count;
    let names: Vec<String> = (0..256)
        .map(|n| format!("f{:03}-{}", n % available, "x".repeat(1019)))
        .collect();
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let maximum = prepare(&borrowed, None);
    let maximum = folders::publish(request, trust, deadline(), &maximum, password).unwrap();
    assert_eq!(maximum.results.len(), 256);
    assert_eq!(
        maximum
            .results
            .iter()
            .filter(|entry| matches!(entry, ResultEntry::Created(_)))
            .count(),
        available
    );
    assert_eq!(
        td_mta::postgres_mailboxes::list(
            request,
            trust,
            deadline(),
            binding.epoch(),
            account,
            password
        )
        .unwrap()
        .mailboxes
        .len(),
        256
    );
    assert_eq!(
        folders::publish(
            request,
            trust,
            deadline(),
            &prepare(&["one more"], None),
            password
        ),
        Err(Error::Capacity)
    );
    sql(fixture,&format!("INSERT INTO td_mta.mailbox_create_receipts SELECT decode('{account}','hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex'),decode('','hex') FROM generate_series(1,1024) n"));
    assert_eq!(
        folders::publish(request, trust, deadline(), &prepare(&[], None), password),
        Err(Error::Capacity)
    );
    sql(fixture,"DELETE FROM td_mta.mailbox_create_receipts WHERE request_digest=decode(repeat('00',32),'hex'); UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')");
    assert_eq!(
        folders::publish(request, trust, deadline(), &prepare(&[], None), password),
        Err(Error::Policy)
    );
    sql(fixture,"UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex'); UPDATE td_mta.accounts SET sequence=decode('ffffffffffffffff','hex')");
    assert_eq!(
        folders::publish(request, trust, deadline(), &prepare(&[], None), password)
            .unwrap()
            .new_sequence,
        u64::MAX
    );
    assert_eq!(
        folders::publish(
            request,
            trust,
            deadline(),
            &prepare(&["overflow"], None),
            password
        ),
        Err(Error::Capacity)
    );
    let revoke =
        devices::prepare_revocation(binding.epoch(), account, device.device_id(), 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(
        folders::publish(request, trust, deadline(), &prepare(&[], None), password),
        Err(Error::Unauthenticated)
    );
    assert_eq!(
        folders::publish(request, trust, deadline(), &mixed, password).unwrap(),
        result
    );
}

fn exercise_moves(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        ids::{EmailId, MailboxId},
        postgres_devices as devices, postgres_email_set as flags,
    };
    let account = binding.account();
    let email = EmailId::from_bytes([0xe1; 16]);
    let creation = devices::prepare_creation(binding.epoch(), account, "move fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let device = creation.device_id();
    let password = creation.password();
    sql(fixture, &format!("BEGIN; INSERT INTO td_mta.blob_ids VALUES(decode('{account}','hex'),decode('{email}','hex')); INSERT INTO td_mta.blobs VALUES(decode('{account}','hex'),decode('{email}','hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0); INSERT INTO td_mta.threads VALUES(decode('{account}','hex'),decode('{email}','hex'),decode('0000000000000001','hex')); INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES(decode('{account}','hex'),decode('{email}','hex'),decode('{email}','hex'),decode('{email}','hex'),0,2,decode('0000000000000001','hex')); INSERT INTO td_mta.memberships SELECT account,decode('{email}','hex'),id,changed FROM td_mta.mailboxes WHERE account=decode('{account}','hex'); COMMIT"));
    let inbox = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    ))
    .unwrap();
    let destination = MailboxId::from_bytes([0xd1; 16]);
    let absent = MailboxId::from_bytes([0xd2; 16]);
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes VALUES(decode('{account}','hex'),decode('{destination}','hex'),'Destination',NULL,NULL,0,1,decode('0000000000000001','hex'))"));
    let update = flags::Update {
        email,
        mailbox: Some(destination),
        seen: Some(true),
        flagged: Some(true),
    };
    let prepare = |updates: &[flags::Update], expected| {
        flags::prepare_batch(binding, device, updates, expected).unwrap()
    };
    let publish =
        |change: &flags::Change| flags::publish_batch(request, trust, deadline(), change, password);
    let first = prepare(&[update], Some(1));
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &first).unwrap(),
        None
    );
    assert_eq!(
        flags::publish_batch(request, trust, deadline(), &first, b"bad"),
        Err(flags::Error::Unauthenticated)
    );
    let result = publish(&first).unwrap();
    assert_eq!(
        result.state,
        flags::Outcome {
            old_sequence: 1,
            new_sequence: 2
        }
    );
    assert_eq!(result.results, [flags::UpdateResult::Updated]);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships"
        ),
        destination.to_string()
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "2");
    assert_eq!(
        sql(
            fixture,
            "SELECT kind FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex') ORDER BY operation"
        ),
        "3\n1\n1"
    );
    let listed = td_mta::postgres_mailboxes::list(
        request,
        trust,
        deadline(),
        binding.epoch(),
        account,
        password,
    )
    .unwrap();
    assert_eq!(
        listed
            .mailboxes
            .iter()
            .find(|m| m.id == inbox)
            .unwrap()
            .counts
            .total_emails,
        0
    );
    assert_eq!(
        listed
            .mailboxes
            .iter()
            .find(|m| m.id == destination)
            .unwrap()
            .counts
            .unread_emails,
        0
    );
    assert_eq!(
        publish(&prepare(&[update], Some(2)))
            .unwrap()
            .state
            .new_sequence,
        2
    );
    assert_eq!(
        publish(&prepare(&[update], Some(1))),
        Err(flags::Error::State)
    );
    let mixed = prepare(
        &[
            flags::Update {
                mailbox: Some(absent),
                seen: Some(false),
                flagged: Some(false),
                ..update
            },
            flags::Update {
                email: EmailId::from_bytes([0xee; 16]),
                mailbox: Some(absent),
                ..update
            },
        ],
        Some(2),
    );
    let mixed_result = publish(&mixed).unwrap();
    assert_eq!(mixed_result.state.new_sequence, 2);
    assert_eq!(
        mixed_result.results,
        [
            flags::UpdateResult::InvalidMailbox,
            flags::UpdateResult::NotFound
        ]
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "2");
    assert_eq!(publish(&mixed).unwrap(), mixed_result);
    // Moving from two memberships to an already-present target retains its row.
    sql(fixture, &format!("INSERT INTO td_mta.memberships VALUES(decode('{account}','hex'),decode('{email}','hex'),decode('{inbox}','hex'),decode('0000000000000002','hex'))"));
    let collapse = publish(&prepare(&[update], Some(2))).unwrap();
    assert_eq!(collapse.state.new_sequence, 3);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(changed,'hex') FROM td_mta.memberships"
        ),
        "0000000000000002"
    );
    assert_eq!(sql(fixture, "SELECT kind FROM td_mta.changes WHERE sequence=decode('0000000000000003','hex') ORDER BY operation"), "3\n1");
    let back = prepare(
        &[flags::Update {
            mailbox: Some(inbox),
            seen: Some(false),
            ..update
        }],
        Some(3),
    );
    sql(fixture, "CREATE FUNCTION public.reject_moves() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_moves BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_moves()");
    assert_eq!(publish(&back), Err(flags::Error::Database));
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &back).unwrap(),
        None
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships"
        ),
        destination.to_string()
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "2");
    sql(fixture, "DROP TRIGGER reject_moves ON td_mta.flag_receipts; CREATE FUNCTION public.delay_moves() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_moves AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_moves()");
    assert_eq!(
        flags::publish_batch(
            request,
            trust,
            Instant::now() + Duration::from_secs(1),
            &back,
            password
        ),
        Err(flags::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(fixture, "DROP TRIGGER delay_moves ON td_mta.flag_receipts");
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &back)
            .unwrap()
            .unwrap()
            .state
            .new_sequence,
        4
    );
    assert_eq!(publish(&back).unwrap().state.new_sequence, 4);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(mailbox_id,'hex') FROM td_mta.memberships"
        ),
        inbox.to_string()
    );
    // Competing expected-state moves publish exactly one operation.
    let left = prepare(&[update], Some(4));
    let right = prepare(
        &[flags::Update {
            seen: Some(true),
            mailbox: None,
            ..update
        }],
        Some(4),
    );
    let (a, b) = thread::scope(|scope| {
        let a = scope.spawn(|| publish(&left));
        let b = scope.spawn(|| publish(&right));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        if a.is_err() { a } else { b },
        Err(flags::Error::State | flags::Error::Database)
    ));
    // All 256 updates can replace all 256 memberships without transferring them.
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(lpad(to_hex(n+2048),32,'0'),'hex'),'bulk',NULL,NULL,0,1,decode('0000000000000001','hex') FROM generate_series(1,254) n; INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),decode('{email}','hex'),decode('{email}','hex'),0,2,decode('0000000000000001','hex') FROM generate_series(1,256) n; INSERT INTO td_mta.memberships SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),id,changed FROM td_mta.mailboxes CROSS JOIN generate_series(1,256) n WHERE account=decode('{account}','hex')"));
    let updates: Vec<_> = (1..=256)
        .map(|n| flags::Update {
            email: EmailId::parse(&format!("{:032x}", n + 1024)).unwrap(),
            ..update
        })
        .collect();
    let bulk = prepare(&updates, Some(5));
    sql(fixture, &format!("INSERT INTO td_mta.mailboxes VALUES(decode('{account}','hex'),decode(repeat('ab',16),'hex'),'overflow',NULL,NULL,0,1,decode('0000000000000001','hex')); INSERT INTO td_mta.memberships VALUES(decode('{account}','hex'),decode(lpad(to_hex(1025),32,'0'),'hex'),decode(repeat('ab',16),'hex'),decode('0000000000000001','hex'))"));
    assert_eq!(publish(&bulk), Err(flags::Error::Capacity));
    assert_eq!(
        flags::inspect_batch(request, trust, deadline(), &bulk).unwrap(),
        None
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.memberships"),
        "65538"
    );
    sql(fixture, "DELETE FROM td_mta.memberships WHERE mailbox_id=decode(repeat('ab',16),'hex'); DELETE FROM td_mta.mailboxes WHERE id=decode(repeat('ab',16),'hex')");
    let outcome = flags::publish_batch(
        request,
        trust,
        Instant::now() + Duration::from_secs(10),
        &bulk,
        password,
    )
    .unwrap();
    assert_eq!(outcome.results, vec![flags::UpdateResult::Updated; 256]);
    assert_eq!(outcome.state.new_sequence, 6);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.memberships"),
        "257"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.changes WHERE sequence=decode('0000000000000006','hex')"
        ),
        "512"
    );
    // A corrupt future target refuses the complete transaction, even a no-op.
    sql(fixture, &format!("UPDATE td_mta.mailboxes SET changed=decode('0000000000000007','hex') WHERE id=decode('{destination}','hex')"));
    assert_eq!(
        publish(&prepare(&[update], None)),
        Err(flags::Error::Integrity)
    );
    sql(fixture, &format!("UPDATE td_mta.mailboxes SET changed=decode('0000000000000006','hex') WHERE id=decode('{destination}','hex'); UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')"));
    assert_eq!(
        publish(&prepare(&[update], None)),
        Err(flags::Error::Policy)
    );
    assert_eq!(publish(&first).unwrap(), result);
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let revoke = devices::prepare_revocation(binding.epoch(), account, device, 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(
        publish(&prepare(&[update], None)),
        Err(flags::Error::Unauthenticated)
    );
    assert_eq!(
        flags::publish_batch(request, trust, deadline(), &first, b"bad").unwrap(),
        result
    );
}

fn exercise_deletion(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        ids::EmailId,
        postgres_devices as devices,
        postgres_email_set::{self as mutations, deletion},
        postgres_read as read,
    };
    let account = binding.account();
    let creation =
        devices::prepare_creation(binding.epoch(), account, "delete fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let device = creation.device_id();
    let password = creation.password();
    let id = |n| EmailId::parse(&format!("{:032x}", n + 1024)).unwrap();
    sql(fixture, &format!("BEGIN;
        INSERT INTO td_mta.blob_ids SELECT decode('{account}','hex'),decode(lpad(to_hex(n+256),32,'0'),'hex') FROM generate_series(1,3) n;
        INSERT INTO td_mta.blobs SELECT account,id,5,decode('2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824','hex'),0,decode('0000000000000001','hex'),1 FROM td_mta.blob_ids;
        INSERT INTO td_mta.blob_chunks SELECT account,id,0,decode('68656c6c6f','hex') FROM td_mta.blobs;
        INSERT INTO td_mta.threads SELECT decode('{account}','hex'),decode(lpad(to_hex(n+768),32,'0'),'hex'),decode('0000000000000001','hex') FROM generate_series(1,3) n;
        INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT decode('{account}','hex'),decode(lpad(to_hex(n+1024),32,'0'),'hex'),decode(lpad(to_hex((CASE WHEN n<3 THEN 1 ELSE n-1 END)+256),32,'0'),'hex'),decode(lpad(to_hex((CASE WHEN n<3 THEN 1 ELSE n-1 END)+768),32,'0'),'hex'),0,2,decode('0000000000000001','hex') FROM generate_series(1,4) n;
        INSERT INTO td_mta.memberships SELECT e.account,e.id,b.id,e.changed FROM td_mta.emails e JOIN td_mta.mailboxes b ON b.account=e.account;
        INSERT INTO td_mta.thread_anchors SELECT account,encode(id,'hex')||'@example.test',id,changed FROM td_mta.emails;
        INSERT INTO td_mta.keywords SELECT account,id,'$seen',changed FROM td_mta.emails;
        INSERT INTO td_mta.leases VALUES(decode('{account}','hex'),decode(lpad(to_hex(258),32,'0'),'hex'),decode('{device}','hex'),0,1,decode('0000000000000001','hex'));
        INSERT INTO td_mta.submissions VALUES(decode('{account}','hex'),decode(repeat('99',16),'hex'),decode(lpad(to_hex(1028),32,'0'),'hex'),decode(lpad(to_hex(771),32,'0'),'hex'),decode(repeat('bb',16),'hex'),decode(lpad(to_hex(259),32,'0'),'hex'),'',0,1,1,0,0,NULL,decode('0000000000000001','hex'));
        UPDATE td_mta.account_usage SET body_bytes=15,blob_count=3,upload_bytes=5,queue_bytes=5,queue_submissions=1;
        COMMIT"));
    let prepare = |emails: &[EmailId], expected| {
        deletion::prepare(binding, device, emails, expected).unwrap()
    };
    let publish = |change: &deletion::Deletion| {
        deletion::publish(request, trust, deadline(), change, password)
    };
    assert!(matches!(
        deletion::prepare(binding, device, &[id(1); 2], None),
        Err(mutations::Error::Invalid)
    ));
    assert!(matches!(
        deletion::prepare(binding, device, &[id(1); 257], None),
        Err(mutations::Error::Capacity)
    ));
    let empty = publish(&prepare(&[], Some(1))).unwrap();
    assert_eq!(empty.state.new_sequence, 1);
    let first = prepare(&[id(1), id(99)], Some(1));
    assert_eq!(
        deletion::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    assert_eq!(
        deletion::publish(request, trust, deadline(), &first, b"bad"),
        Err(mutations::Error::Unauthenticated)
    );
    let result = publish(&first).unwrap();
    assert_eq!(
        result.state,
        mutations::Outcome {
            old_sequence: 1,
            new_sequence: 2
        }
    );
    assert_eq!(
        result.results,
        [
            mutations::UpdateResult::Destroyed,
            mutations::UpdateResult::NotFound
        ]
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes::text||':'||blob_count::text FROM td_mta.account_usage"
        ),
        "15:3"
    );
    assert_eq!(sql(fixture,"SELECT kind::text||':'||action::text FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex') ORDER BY operation"),"3:3\n1:2\n2:2");
    assert_eq!(publish(&first).unwrap(), result);
    assert_eq!(
        publish(&prepare(&[id(1)], Some(2)))
            .unwrap()
            .state
            .new_sequence,
        2
    );
    assert_eq!(
        publish(&prepare(&[id(2)], Some(1))),
        Err(mutations::Error::State)
    );
    let second = prepare(&[id(2)], Some(2));
    sql(fixture, "UPDATE td_mta.account_usage SET body_bytes=5");
    assert_eq!(publish(&second), Err(mutations::Error::Integrity));
    assert_eq!(
        deletion::inspect(request, trust, deadline(), &second).unwrap(),
        None
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "3");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_chunks"), "3");
    sql(fixture,"UPDATE td_mta.account_usage SET body_bytes=15; CREATE FUNCTION public.reject_deletion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_deletion BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_deletion()");
    assert_eq!(publish(&second), Err(mutations::Error::Database));
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "3");
    assert_eq!(
        sql(fixture, "SELECT body_bytes FROM td_mta.account_usage"),
        "15"
    );
    sql(
        fixture,
        "DROP TRIGGER reject_deletion ON td_mta.flag_receipts",
    );
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert!(view.email(id(2))?.is_some());
        assert_eq!(publish(&second).unwrap().state.new_sequence, 3);
        let bytes = view.with_body(id(2), |_, body| {
            let mut bytes = [0; 5];
            assert_eq!(body.read_at(0, &mut bytes)?, 5);
            Ok(bytes)
        })?;
        assert_eq!(&bytes, b"hello");
        assert!(view.email(id(2))?.is_some());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes::text||':'||blob_count::text FROM td_mta.account_usage"
        ),
        "10:2"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_chunks"), "2");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_ids"), "3");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.threads"), "2");
    let retained = prepare(&[id(3), id(4)], Some(3));
    sql(fixture,"CREATE FUNCTION public.delay_deletion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_deletion AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_deletion()");
    assert_eq!(
        deletion::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(1),
            &retained,
            password
        ),
        Err(mutations::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_deletion ON td_mta.flag_receipts",
    );
    assert_eq!(
        deletion::inspect(request, trust, deadline(), &retained)
            .unwrap()
            .unwrap()
            .state
            .new_sequence,
        4
    );
    assert_eq!(publish(&retained).unwrap().state.new_sequence, 4);
    for table in [
        "emails",
        "memberships",
        "keywords",
        "thread_anchors",
        "threads",
    ] {
        assert_eq!(
            sql(fixture, &format!("SELECT count(*) FROM td_mta.{table}")),
            "0"
        );
    }
    assert_eq!(sql(fixture,"SELECT body_bytes::text||':'||blob_count::text||':'||upload_bytes::text||':'||queue_bytes::text||':'||queue_submissions::text FROM td_mta.account_usage"),"10:2:5:5:1");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blob_chunks"), "2");
    // Maximum batch, affected-mailbox and thread sets fit the closed worker.
    sql(fixture,&format!("BEGIN;
        INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(lpad(to_hex(n+2048),32,'0'),'hex'),'bulk',NULL,NULL,0,1,decode('0000000000000001','hex') FROM generate_series(1,255) n;
        INSERT INTO td_mta.blob_ids SELECT decode('{account}','hex'),decode(lpad(to_hex(n+11000),32,'0'),'hex') FROM generate_series(1,256) n;
        INSERT INTO td_mta.blobs SELECT decode('{account}','hex'),decode(lpad(to_hex(n+11000),32,'0'),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000004','hex'),0 FROM generate_series(1,256) n;
        INSERT INTO td_mta.threads SELECT decode('{account}','hex'),decode(lpad(to_hex(n+10000),32,'0'),'hex'),decode('0000000000000004','hex') FROM generate_series(1,256) n;
        INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT decode('{account}','hex'),decode(lpad(to_hex(n+12000),32,'0'),'hex'),decode(lpad(to_hex(n+11000),32,'0'),'hex'),decode(lpad(to_hex(n+10000),32,'0'),'hex'),0,2,decode('0000000000000004','hex') FROM generate_series(1,256) n;
        INSERT INTO td_mta.memberships SELECT decode('{account}','hex'),decode(lpad(to_hex(n+12000),32,'0'),'hex'),id,changed FROM td_mta.mailboxes CROSS JOIN generate_series(1,256) n WHERE account=decode('{account}','hex');
        UPDATE td_mta.account_usage SET blob_count=258; COMMIT"));
    let ids: Vec<_> = (1..=256)
        .map(|n| EmailId::parse(&format!("{:032x}", n + 12000)).unwrap())
        .collect();
    let bulk = prepare(&ids, Some(4));
    sql(fixture,&format!("INSERT INTO td_mta.mailboxes VALUES(decode('{account}','hex'),decode(repeat('ab',16),'hex'),'overflow',NULL,NULL,0,1,decode('0000000000000001','hex')); INSERT INTO td_mta.memberships VALUES(decode('{account}','hex'),decode(lpad(to_hex(12001),32,'0'),'hex'),decode(repeat('ab',16),'hex'),decode('0000000000000001','hex'))"));
    assert_eq!(publish(&bulk), Err(mutations::Error::Capacity));
    assert_eq!(
        deletion::inspect(request, trust, deadline(), &bulk).unwrap(),
        None
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.memberships"),
        "65537"
    );
    sql(fixture,"DELETE FROM td_mta.memberships WHERE mailbox_id=decode(repeat('ab',16),'hex'); DELETE FROM td_mta.mailboxes WHERE id=decode(repeat('ab',16),'hex')");
    let outcome = deletion::publish(
        request,
        trust,
        Instant::now() + Duration::from_secs(10),
        &bulk,
        password,
    )
    .unwrap();
    assert_eq!(
        outcome.results,
        vec![mutations::UpdateResult::Destroyed; 256]
    );
    assert_eq!(outcome.state.new_sequence, 5);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.changes WHERE sequence=decode('0000000000000005','hex')"
        ),
        "768"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT body_bytes::text||':'||blob_count::text FROM td_mta.account_usage"
        ),
        "10:2"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    sql(fixture,&format!("UPDATE td_mta.flag_receipts SET results=decode('0102','hex') WHERE operation=decode('{}','hex')",first.operation()));
    assert_eq!(
        deletion::inspect(request, trust, deadline(), &first),
        Err(mutations::Error::Integrity)
    );
    sql(fixture,&format!("UPDATE td_mta.flag_receipts SET results=decode('0402','hex') WHERE operation=decode('{}','hex'); UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",first.operation()));
    assert_eq!(publish(&prepare(&[], None)), Err(mutations::Error::Policy));
    assert_eq!(publish(&first).unwrap(), result);
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let revoke = devices::prepare_revocation(binding.epoch(), account, device, 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(
        publish(&prepare(&[], None)),
        Err(mutations::Error::Unauthenticated)
    );
    assert_eq!(
        deletion::publish(request, trust, deadline(), &first, b"bad").unwrap(),
        result
    );
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_receiving_publishes_inbox_provenance_and_retained_outcomes() {
    exercise_configuration(ConfigurationExercise::Receiving);
}

fn exercise_receiving(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
        postgres_body as body, postgres_devices as devices, postgres_receiving as receiving,
    };
    let mut recipient_bytes = vec![0; td_mta::format::row::MAX_RECEIPT_BYTES];
    let recipients = ReceiptRecipients::encode(
        &["alice@example.test", "alias@example.test"],
        &mut recipient_bytes,
    )
    .unwrap();
    let receipt = SmtpReceipt {
        peer: "192.0.2.1".parse().unwrap(),
        gateway: None,
        tls: ReceiptTls::Tls13,
        ehlo: "sender.example.test",
        reverse_path: "sender@example.test",
        recipients,
    };
    let prepare = |bytes: &[u8]| {
        receiving::prepare(
            binding,
            &receiving::Message {
                bytes,
                receipt,
                hostname: "mail.example.test",
                extended: true,
                header_bytes: 65536,
                received_at: 1_700_000_000_000,
            },
            deadline(),
        )
    };
    assert!(prepare(b"broken\n").is_err());
    assert!(prepare(&[b'x'; 1001]).is_err());
    let first = prepare(b"Return-Path: <forged@example.test>\r\nMessage-ID: <root@example.test>\r\nSubject: Inbox\r\n\r\nhello\r\n").unwrap();
    assert_eq!(
        receiving::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    let (left, right) = thread::scope(|scope| {
        let left = scope.spawn(|| receiving::publish(request, trust, deadline(), &first));
        let right = scope.spawn(|| receiving::publish(request, trust, deadline(), &first));
        (
            left.join().unwrap().unwrap(),
            right.join().unwrap().unwrap(),
        )
    });
    assert_eq!(left, right);
    let outcome = left;
    assert_eq!((outcome.old_sequence, outcome.new_sequence), (1, 2));
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "1"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords"), "0");
    assert_eq!(sql(fixture,"SELECT origin,peer_family,encode(peer_octets,'hex'),tls,ehlo,reverse_path,receipt_count FROM td_mta.emails"),"1|4|c0000201|2|sender.example.test|sender@example.test|2");
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(address,',' ORDER BY ordinal) FROM td_mta.smtp_receipt_recipients"
        ),
        "alice@example.test,alias@example.test"
    );
    assert_eq!(sql(fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),format!("{}|1|0|0|0",outcome.length));
    assert_eq!(sql(fixture,"SELECT string_agg(kind::text || ':' || action::text,',' ORDER BY operation) FROM td_mta.changes WHERE sequence=decode('0000000000000002','hex')"),"3:1,2:1,1:2");
    let credential =
        devices::prepare_creation(binding.epoch(), binding.account(), "receiving reader", 1)
            .unwrap();
    devices::publish_creation(request, trust, deadline(), &credential).unwrap();
    let text = body::with_email(
        request,
        trust,
        deadline(),
        body::ReadRequest {
            epoch: binding.epoch(),
            account: binding.account(),
            email: outcome.email,
            maximum: 32 * 1024 * 1024,
        },
        credential.password(),
        |metadata, reader| {
            assert_eq!(metadata.received_at, 1_700_000_000_000);
            let mut bytes = vec![0; outcome.length as usize];
            let mut offset = 0;
            while offset < bytes.len() {
                let n = reader.read_at(offset as u64, &mut bytes[offset..])?;
                assert!(n > 0);
                offset += n;
            }
            Ok(String::from_utf8(bytes).unwrap())
        },
    )
    .unwrap();
    assert!(text.starts_with(
        "Return-Path: <sender@example.test>\r\nReceived: from sender.example.test ([192.0.2.1])"
    ));
    assert!(!text.contains("forged@example.test"));
    assert!(text.contains("with ESMTPS (TLSv1.3)"));
    assert!(text.ends_with("\r\n\r\nhello\r\n"));
    let reply = prepare(
        b"Message-ID: <reply@example.test>\r\nReferences: <root@example.test>\r\n\r\nreply\r\n",
    )
    .unwrap();
    let reply_outcome = receiving::publish(request, trust, deadline(), &reply).unwrap();
    assert_eq!(reply_outcome.thread, outcome.thread);
    assert_eq!(reply_outcome.new_sequence, 3);
    let pending = prepare(b"\r\nnew\r\n").unwrap();
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET role=NULL WHERE role='inbox'",
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &pending),
        Err(receiving::Error::MissingMailbox)
    );
    sql(fixture, "UPDATE td_mta.mailboxes SET role='inbox'");
    let used = sql(fixture, "SELECT body_bytes FROM td_mta.account_usage");
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET body_bytes={}",
            binding.quotas().body_bytes
        ),
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &pending),
        Err(receiving::Error::Quota)
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.account_usage SET body_bytes={used}"),
    );
    sql(fixture,"CREATE FUNCTION public.reject_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refuse'; END $$; CREATE TRIGGER reject_receiving BEFORE INSERT ON td_mta.receiving_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_receiving()");
    assert_eq!(
        receiving::publish(request, trust, deadline(), &pending),
        Err(receiving::Error::Database)
    );
    assert_eq!(
        receiving::inspect(request, trust, deadline(), &pending).unwrap(),
        None
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "2");
    assert_eq!(
        sql(fixture, "SELECT body_bytes FROM td_mta.account_usage"),
        used
    );
    sql(fixture,"DROP TRIGGER reject_receiving ON td_mta.receiving_receipts; CREATE FUNCTION public.delay_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_receiving AFTER INSERT ON td_mta.receiving_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_receiving()");
    assert_eq!(
        receiving::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(1),
            &pending
        ),
        Err(receiving::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let recovered = receiving::inspect(request, trust, deadline(), &pending)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.new_sequence, 4);
    sql(
        fixture,
        "DROP TRIGGER delay_receiving ON td_mta.receiving_receipts",
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &pending).unwrap(),
        recovered
    );
    let fresh = prepare(b"\r\nfresh\r\n").unwrap();
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &fresh),
        Err(receiving::Error::Policy)
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &first).unwrap(),
        outcome
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
    );
    let original=sql(fixture,&format!("SELECT encode(request_digest,'hex') FROM td_mta.receiving_receipts WHERE operation=decode('{}','hex')",first.operation()));
    sql(fixture,&format!("UPDATE td_mta.receiving_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation=decode('{}','hex')",first.operation()));
    assert_eq!(
        receiving::inspect(request, trust, deadline(), &first),
        Err(receiving::Error::Conflict)
    );
    sql(fixture,&format!("UPDATE td_mta.receiving_receipts SET request_digest=decode('{original}','hex') WHERE operation=decode('{}','hex')",first.operation()));
    // Exercise a near-ceiling message and the full envelope recipient count.
    let addresses = vec!["alice@example.test"; 1000];
    let mut packed = vec![0; td_mta::format::row::MAX_RECEIPT_BYTES];
    let many = ReceiptRecipients::encode(&addresses, &mut packed).unwrap();
    let mut large = b"\r\n".to_vec();
    let allowance = td_mta::store_fs::smtp_trace_allowance("mail.example.test").unwrap();
    let limit = binding.upload_maximum().min(32 * 1024 * 1024) - allowance;
    let mut line = vec![b'x'; 998];
    line.extend_from_slice(b"\r\n");
    while large.len() + line.len() <= limit {
        large.extend_from_slice(&line);
    }
    let bulk = receiving::prepare(
        binding,
        &receiving::Message {
            bytes: &large,
            receipt: SmtpReceipt {
                recipients: many,
                ..receipt
            },
            hostname: "mail.example.test",
            extended: true,
            header_bytes: 65536,
            received_at: 1_700_000_000_000,
        },
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap();
    let bulk_outcome = receiving::publish(
        request,
        trust,
        Instant::now() + Duration::from_secs(10),
        &bulk,
    )
    .unwrap();
    assert!(bulk_outcome.length > large.len() as u64);
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.smtp_receipt_recipients WHERE email_id=decode('{}','hex')",bulk_outcome.email)),"1000");
    assert_eq!(sql(fixture,&format!("SELECT sum(octet_length(bytes)) FROM td_mta.blob_chunks WHERE blob_id=decode('{}','hex')",bulk_outcome.blob)),bulk_outcome.length.to_string());
    sql(fixture,"INSERT INTO td_mta.receiving_receipts SELECT account,decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),email_id,blob_id,thread_id,mailbox_id,length,created_at,old_sequence,new_sequence FROM (SELECT * FROM td_mta.receiving_receipts LIMIT 1) r CROSS JOIN generate_series(1,1024) n");
    assert_eq!(
        receiving::publish(request, trust, deadline(), &fresh),
        Err(receiving::Error::Capacity)
    );
    assert_eq!(
        receiving::inspect(request, trust, deadline(), &fresh).unwrap(),
        None
    );
    sql(
        fixture,
        "DELETE FROM td_mta.receiving_receipts WHERE request_digest=decode(repeat('00',32),'hex')",
    );
}
