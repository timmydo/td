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
        let deadline = Instant::now() + Duration::from_secs(20);
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
    assert_eq!(receipt.version, 4);
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
        4
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
    assert!(initialized.contains("\"schema_version\":4"));
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
    assert_eq!(receipt.version, 4);
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
    assert_eq!(sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='td_mta' AND c.relkind='r'"), "20");
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
    for previous_version in [2, 3] {
        let fixture = Fixture::new();
        let trust = TrustStore::from_pem(&fixture.ca).unwrap();
        for (index, ddl) in [
            include_str!("../src/postgres_schema.sql"),
            include_str!("../src/postgres_materials.sql"),
            include_str!("../src/postgres_mail.sql"),
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
        let obstruction = if previous_version == 2 {
            "accounts"
        } else {
            "devices"
        };
        sql(
            &fixture,
            &format!("CREATE TABLE td_mta.{obstruction}(obstruction bigint)"),
        );
        assert_eq!(
            upgrade(&fixture.request(), &trust, deadline()),
            Err(SchemaError::Database)
        );
        assert_eq!(
            sql(&fixture, "SELECT count(*) FROM td_mta.schema_migrations"),
            previous_version.to_string()
        );
        sql(&fixture, &format!("DROP TABLE td_mta.{obstruction}"));
        let receipt = upgrade(&fixture.request(), &trust, deadline()).unwrap();
        assert_eq!(receipt.version, 4);
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
