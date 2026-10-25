#![deny(unsafe_code)]
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

#[path = "support/termination_sender.rs"]
mod termination_sender;

#[path = "support/migration_archive.rs"]
mod migration_archive;

#[test]
#[ignore = "requires private trusted TMPDIR"]
fn migration_archive_cli_preserves_and_verifies_snapshots() {
    migration_archive::exercise();
}

#[path = "support/postgres_mail_client.rs"]
mod mail_client;

const PASSWORD: &[u8] = b"disposable-postgres-fixture-password";

/// The directory of PATH's `initdb`, which must also hold `postgres`,
/// `pg_ctl`, `psql`, `pg_dump` and `pg_restore`, from one installation, whose
/// `postgres` is release 17.
fn postgres_bin() -> PathBuf {
    let initdb = td_fs::executable_on_path("initdb")
        .unwrap_or_else(|| panic!("PostgreSQL 17 is not on PATH: no initdb"));
    let bin = initdb.parent().unwrap().to_path_buf();
    for name in ["postgres", "pg_ctl", "psql", "pg_dump", "pg_restore"] {
        assert!(
            td_fs::executable_in(Some(bin.as_os_str()), name).is_some(),
            "PostgreSQL on PATH: {} has initdb but no {name}",
            bin.display()
        );
    }
    let version = std::process::Command::new(bin.join("postgres"))
        .arg("--version")
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.starts_with("postgres (PostgreSQL) 17."),
        "PostgreSQL on PATH: {} is not release 17: {version}",
        bin.display()
    );
    bin
}

struct Fixture {
    root: TempDir,
    bin: PathBuf,
    server: Option<Child>,
    port: u16,
    ca: Vec<u8>,
    queue_runtime: Option<queue_expiry::Runtime>,
}
impl Fixture {
    fn new() -> Self {
        let bin = postgres_bin();
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
            queue_runtime: None,
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
        let root_der = certificate::certificate_names(&root_key, &root_key, true, 1, &[], false);
        fs::write(fixture.root.join("root.der"), &root_der).unwrap();
        fixture.ca = certificate::pem("CERTIFICATE", &root_der);
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
            mode: td_mta::postgres_probe::Mode::Preparation,
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
    let stop = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "SQL {query}: {}",
                fs::read_to_string(&path).unwrap()
            );
            break;
        }
        assert!(
            Instant::now() < stop,
            "SQL fixture command timed out: {query}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fs::metadata(&path).unwrap().len() < 4096);
    fs::read_to_string(path).unwrap().trim().to_owned()
}

fn create_runtime_login(fixture: &Fixture) {
    sql(fixture, "CREATE ROLE mail_runtime LOGIN PASSWORD 'disposable-postgres-fixture-password'; GRANT USAGE ON SCHEMA td_mta TO mail_runtime; GRANT SELECT ON ALL TABLES IN SCHEMA td_mta TO mail_runtime");
}

fn grant_runtime_mail(fixture: &Fixture) {
    sql(fixture, "GRANT INSERT,UPDATE,DELETE ON ALL TABLES IN SCHEMA td_mta TO mail_runtime; REVOKE INSERT,UPDATE,DELETE ON td_mta.schema_migrations,td_mta.runtime_barrier,td_mta.config_materials,td_mta.config_material_chunks,td_mta.config_versions,td_mta.config_version_chunks,td_mta.config_selection,td_mta.config_selection_receipts,td_mta.account_bootstraps,td_mta.leases,td_mta.imports FROM mail_runtime; GRANT UPDATE(singleton) ON td_mta.config_selection TO mail_runtime; GRANT INSERT ON td_mta.leases TO mail_runtime");
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
    assert_eq!(receipt.version, 32);
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
        32
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
    command_output_with_timeout(arguments, root, expected, Duration::from_secs(15))
}

fn command_output_with_timeout(
    arguments: &[&str],
    root: &std::path::Path,
    expected: i32,
    timeout: Duration,
) -> String {
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
    let stop = Instant::now() + timeout;
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
    assert!(initialized.contains("\"schema_version\":32"));
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
    for operation in [
        "schema-init",
        "schema-inspect",
        "schema-upgrade",
        "runtime-check",
    ] {
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
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_runtime_credentials_separate_mail_writes_from_owner_authority() {
    use td_mta::{postgres_runtime_access as access, postgres_schema as schema};
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let receipt = schema::initialize(&owner, &trust, deadline()).unwrap();
    assert_eq!(
        access::inspect(&owner, &trust, deadline()),
        Err(access::Error::Role)
    );
    create_runtime_login(&fixture);
    let mut runtime = fixture.request();
    runtime.user = "mail_runtime";
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Privileges)
    );
    grant_runtime_mail(&fixture);
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()).unwrap(),
        receipt
    );
    sql(&fixture, "ALTER DATABASE postgres OWNER TO mail_runtime; REVOKE CREATE ON DATABASE postgres FROM mail_runtime");
    assert_eq!(
        sql(
            &fixture,
            "SELECT pg_catalog.has_database_privilege('mail_runtime','postgres','CREATE')"
        ),
        "f"
    );
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()),
        Err(access::Error::Role)
    );
    sql(&fixture, "ALTER DATABASE postgres OWNER TO td_probe");
    assert_eq!(
        access::inspect(&runtime, &trust, deadline()).unwrap(),
        receipt
    );
    let mut serving = fixture.request();
    serving.mode = td_mta::postgres_probe::Mode::Runtime;
    assert_eq!(probe(&serving, &trust, deadline()), Err(Error::RuntimeRole));
    serving.user = "mail_runtime";
    assert_eq!(
        schema::inspect(&serving, &trust, deadline()).unwrap(),
        receipt
    );
    for (grant, revoke, error) in [
        (
            "GRANT CREATE ON SCHEMA td_mta TO mail_runtime",
            "REVOKE CREATE ON SCHEMA td_mta FROM mail_runtime",
            access::Error::Role,
        ),
        (
            "GRANT CREATE ON DATABASE postgres TO mail_runtime",
            "REVOKE CREATE ON DATABASE postgres FROM mail_runtime",
            access::Error::Role,
        ),
        (
            "ALTER ROLE mail_runtime CREATEDB",
            "ALTER ROLE mail_runtime NOCREATEDB",
            access::Error::Role,
        ),
        (
            "ALTER ROLE mail_runtime CREATEROLE",
            "ALTER ROLE mail_runtime NOCREATEROLE",
            access::Error::Role,
        ),
        (
            "ALTER ROLE mail_runtime REPLICATION",
            "ALTER ROLE mail_runtime NOREPLICATION",
            access::Error::Role,
        ),
        (
            "ALTER ROLE mail_runtime BYPASSRLS",
            "ALTER ROLE mail_runtime NOBYPASSRLS",
            access::Error::Role,
        ),
        (
            "GRANT td_probe TO mail_runtime",
            "REVOKE td_probe FROM mail_runtime",
            access::Error::Role,
        ),
        (
            "GRANT UPDATE(generation) ON td_mta.config_selection TO mail_runtime",
            "REVOKE UPDATE(generation) ON td_mta.config_selection FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "GRANT UPDATE(version) ON td_mta.schema_migrations TO mail_runtime",
            "REVOKE UPDATE(version) ON td_mta.schema_migrations FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE INSERT ON td_mta.leases FROM mail_runtime",
            "GRANT INSERT ON td_mta.leases TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "GRANT UPDATE(expires_at) ON td_mta.leases TO mail_runtime",
            "REVOKE UPDATE(expires_at) ON td_mta.leases FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "GRANT DELETE ON td_mta.leases TO mail_runtime",
            "REVOKE DELETE ON td_mta.leases FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE INSERT ON td_mta.attempt_final_reservations FROM mail_runtime",
            "GRANT INSERT ON td_mta.attempt_final_reservations TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE INSERT ON td_mta.submission_cancel_receipts FROM mail_runtime",
            "GRANT INSERT ON td_mta.submission_cancel_receipts TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE UPDATE ON td_mta.route_health FROM mail_runtime",
            "GRANT UPDATE ON td_mta.route_health TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE INSERT ON td_mta.route_health_receipts FROM mail_runtime",
            "GRANT INSERT ON td_mta.route_health_receipts TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE UPDATE ON td_mta.queue_expiry_progress FROM mail_runtime",
            "GRANT UPDATE ON td_mta.queue_expiry_progress TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "REVOKE INSERT ON td_mta.submission_expiry_receipts FROM mail_runtime",
            "GRANT INSERT ON td_mta.submission_expiry_receipts TO mail_runtime",
            access::Error::Privileges,
        ),
        (
            "GRANT TRUNCATE ON td_mta.emails TO mail_runtime",
            "REVOKE TRUNCATE ON td_mta.emails FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "GRANT SELECT ON td_mta.emails TO mail_runtime WITH GRANT OPTION",
            "REVOKE GRANT OPTION FOR SELECT ON td_mta.emails FROM mail_runtime",
            access::Error::Privileges,
        ),
        (
            "ALTER TABLE td_mta.emails ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE td_mta.emails DISABLE ROW LEVEL SECURITY",
            access::Error::Privileges,
        ),
    ] {
        sql(&fixture, grant);
        let expected = if error == access::Error::Role {
            Error::RuntimeRole
        } else {
            Error::RuntimePrivileges
        };
        assert_eq!(
            probe(&serving, &trust, deadline()),
            Err(expected),
            "{grant}"
        );
        assert_eq!(
            access::inspect(&runtime, &trust, deadline()),
            Err(error),
            "{grant}"
        );
        sql(&fixture, revoke);
        assert!(probe(&serving, &trust, deadline()).is_ok());
        assert_eq!(
            access::inspect(&runtime, &trust, deadline()).unwrap(),
            receipt
        );
    }
    sql(
        &fixture,
        "UPDATE td_mta.schema_migrations SET version=99 WHERE version=32",
    );
    assert_eq!(
        probe(&serving, &trust, deadline()),
        Err(Error::RuntimeSchema)
    );
    sql(
        &fixture,
        "UPDATE td_mta.schema_migrations SET version=32 WHERE version=99",
    );
    assert_eq!(
        schema::inspect(&serving, &trust, deadline()).unwrap(),
        receipt
    );
    // Column-only UPDATE permits policy locking without pointer mutation.
    let execute = |statement: &str, expected: bool| {
        let mut child = ProbeChild(
            sql_command(&fixture)
                .args(["-U", "mail_runtime", "-c", statement])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let stop = deadline();
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert_eq!(status.success(), expected, "{statement}");
                break;
            }
            assert!(Instant::now() < stop, "runtime SQL did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    };
    execute("BEGIN; SELECT singleton FROM td_mta.config_selection FOR SHARE; UPDATE td_mta.accounts SET sequence=sequence; ROLLBACK", true);
    for statement in [
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000001','hex')",
        "CREATE TABLE td_mta.forbidden(id integer)",
        "DELETE FROM td_mta.schema_migrations",
        "UPDATE td_mta.leases SET expires_at=expires_at",
        "DELETE FROM td_mta.leases",
    ] {
        execute(statement, false);
    }
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("runtime-ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("runtime-connection");
    fs::write(
        &connection,
        format!(
            "postgresql://mail_runtime:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let invoke = |status| {
        command_output(
            &[
                "postgres",
                "runtime-check",
                "--connection-file",
                connection.to_str().unwrap(),
            ],
            fixture.root.path(),
            status,
        )
    };
    let output = invoke(0);
    assert!(output.contains("\"runtime_privileges_checked\":true"));
    assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
    sql(&fixture, "REVOKE DELETE ON td_mta.emails FROM mail_runtime");
    assert!(invoke(1).contains("incorrect-data-privileges"));
}

#[test]
#[ignore = "requires PostgreSQL 17's initdb, postgres, pg_ctl and psql on PATH"]
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
    assert_eq!(receipt.version, 32);
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
    assert_eq!(sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='td_mta' AND c.relkind='r'"), "52");
    // Each expected refusal rolls back its own subtransaction. An unexpected
    // success raises a different SQLSTATE and fails psql with ON_ERROR_STOP.
    let refuse = |statement: String, state: &str| {
        sql(&fixture, &format!("DO $$ BEGIN BEGIN {statement}; SET CONSTRAINTS ALL IMMEDIATE; RAISE EXCEPTION 'unexpected success'; EXCEPTION WHEN SQLSTATE '{state}' THEN NULL; END; END $$"));
    };
    assert_eq!(
        sql(
            &fixture,
            "SELECT singleton,published_at IS NULL FROM td_mta.queue_expiry_progress"
        ),
        "t|t"
    );
    for statement in [
        "INSERT INTO td_mta.queue_expiry_progress VALUES(false,NULL)",
        "UPDATE td_mta.queue_expiry_progress SET published_at=-1",
    ] {
        refuse(statement.to_owned(), "23514");
    }
    refuse(
        "INSERT INTO td_mta.queue_expiry_progress VALUES(true,0)".to_owned(),
        "23505",
    );
    let expiry = format!("INSERT INTO td_mta.submission_expiry_receipts VALUES({a},{a},{digest},{b},100,10,11,{zero},{high})");
    sql(&fixture, &expiry);
    refuse(expiry, "23505");
    for assignment in [
        "recipient_count=0",
        "recipient_count=101",
        "cutoff_at=-1",
        "published_at=9",
        "operation=decode('00','hex')",
        "request_digest=decode('00','hex')",
        "submission_id=decode('00','hex')",
        "new_sequence=old_sequence",
    ] {
        refuse(
            format!("UPDATE td_mta.submission_expiry_receipts SET {assignment}"),
            "23514",
        );
    }
    // Receipts retain settlement independently of submission lifetime.
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.submission_expiry_receipts"
        ),
        "1"
    );
    // Cancellation settlement survives independently of the submission row.
    sql(&fixture, &format!("INSERT INTO td_mta.submission_cancel_receipts VALUES({a},{a},{digest},{a},1000,1000,-1,{high},{max}),({a},{b},{digest},{a},1000,0,-1,{high},{high})"));
    for assignment in [
        "recipient_count=0",
        "recipient_count=1001",
        "changed_count=-1",
        "changed_count=1001",
        "changed_count=0",
        "new_sequence=old_sequence",
        "old_sequence=decode(repeat('00',8),'hex')",
        "request_digest=decode(repeat('00',31),'hex')",
        "submission_id=decode(repeat('00',15),'hex')",
    ] {
        refuse(
            format!(
                "UPDATE td_mta.submission_cancel_receipts SET {assignment} WHERE operation={a}"
            ),
            "23514",
        );
    }
    refuse(
        format!("UPDATE td_mta.submission_cancel_receipts SET changed_count=1 WHERE operation={b}"),
        "23514",
    );
    refuse(
        format!(
            "UPDATE td_mta.submission_cancel_receipts SET new_sequence={zero} WHERE operation={b}"
        ),
        "23514",
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.submission_cancel_receipts WHERE completed_at=-1"
        ),
        "2"
    );
    sql(&fixture, &format!("INSERT INTO td_mta.route_health VALUES({a},'primary',{digest},{high},1,4,0,300000,{high}); INSERT INTO td_mta.route_health_receipts VALUES({a},{a},{digest},'primary',{digest},1,{zero},{high},1,4,0,300000,{high})"));
    for assignment in [
        "failure_count=4294967296",
        "next_at=299999",
        "next_at=3600001",
        "reason=2",
        "reason=3",
        "failure_count=0",
        "repair_generation=decode(repeat('00',8),'hex')",
    ] {
        for table in ["route_health", "route_health_receipts"] {
            refuse(format!("UPDATE td_mta.{table} SET {assignment}"), "23514");
        }
    }
    refuse(
        "UPDATE td_mta.route_health_receipts SET action=4".to_owned(),
        "23514",
    );
    refuse(
        "UPDATE td_mta.route_health_receipts SET new_revision=old_revision".to_owned(),
        "23514",
    );
    sql(&fixture, "UPDATE td_mta.route_health SET repair_generation=NULL; UPDATE td_mta.route_health SET failure_count=0,reason=0,failed_at=NULL,next_at=NULL");
    assert_eq!(sql(&fixture, "SELECT failure_count,reason,failed_at IS NULL,next_at IS NULL,repair_generation IS NULL FROM td_mta.route_health"), "0|0|t|t|t");
    assert_eq!(
        sql(
            &fixture,
            "SELECT failure_count,reason,next_at FROM td_mta.route_health_receipts"
        ),
        "1|4|300000"
    );
    refuse(
        format!("UPDATE td_mta.accounts SET sequence={zero} WHERE id={a}"),
        "23514",
    );
    refuse(
        format!("INSERT INTO td_mta.blobs VALUES ({b},{a},0,{digest},0,{zero},0)"),
        "23503",
    );
    refuse(
        format!("INSERT INTO td_mta.blobs VALUES ({a},{a},67108865,{digest},0,{zero},1025)"),
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
        format!("INSERT INTO td_mta.blob_chunks VALUES ({a},{b},1024,decode('42','hex'))"),
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
    for previous_version in [
        2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 31,
    ] {
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
            include_str!("../src/postgres_receiving.sql"),
            include_str!("../src/postgres_submissions.sql"),
            include_str!("../src/postgres_submission_routes.sql"),
            include_str!("../src/postgres_route_owners.sql"),
            include_str!("../src/postgres_attempts.sql"),
            include_str!("../src/postgres_attempt_recovery.sql"),
            include_str!("../src/postgres_attempt_phases.sql"),
            include_str!("../src/postgres_attempt_acceptance.sql"),
            include_str!("../src/postgres_attempt_refusal.sql"),
            include_str!("../src/postgres_attempt_rcpt.sql"),
            include_str!("../src/postgres_attempt_command.sql"),
            include_str!("../src/postgres_queue_expiry.sql"),
            include_str!("../src/postgres_route_health.sql"),
            include_str!("../src/postgres_submission_cancel.sql"),
            include_str!("../src/postgres_final_capacity.sql"),
            include_str!("../src/postgres_runtime_barrier.sql"),
            include_str!("../src/postgres_logs.sql"),
            include_str!("../src/postgres_acme.sql"),
            include_str!("../src/postgres_recovery.sql"),
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
        if matches!(previous_version, 9 | 26) {
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
            13 => "submission_receipts",
            14 => "submission_routes",
            15 => "route_owners",
            16 => "submission_attempts",
            17 => "attempt_recovery_receipts",
            18 => "attempt_phase_receipts",
            19 => "attempt_acceptance_receipts",
            20 => "attempt_refusal_receipts",
            21 => "attempt_rcpt_receipts",
            22 => "attempt_command_receipts",
            23 => "queue_expiry_progress",
            24 => "route_health_receipts",
            25 => "submission_cancel_receipts",
            26 => "attempt_final_reservations",
            27 => "runtime_barrier",
            28 => "application_log_state",
            _ => "flag_receipts",
        };
        if previous_version == 8 {
            sql(&fixture, "INSERT INTO td_mta.accounts VALUES(decode(repeat('7f',16),'hex'),decode('0000000000000001','hex'),decode('0000000000000000','hex')); INSERT INTO td_mta.flag_receipts VALUES(decode(repeat('7f',16),'hex'),decode(repeat('01',16),'hex'),decode(repeat('02',32),'hex'),decode('0000000000000001','hex'),decode('0000000000000001','hex')); ALTER TABLE td_mta.flag_receipts ADD COLUMN results bytea");
        } else if previous_version == 31 {
            sql(&fixture, "ALTER TABLE td_mta.blob_chunks RENAME CONSTRAINT blob_chunks_ordinal_check TO obstructed_ordinal_check");
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
        } else if previous_version == 31 {
            assert!(sql(&fixture, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='td_mta.blobs'::regclass AND conname='blobs_length_check'").contains("33554432"));
            sql(&fixture, "ALTER TABLE td_mta.blob_chunks RENAME CONSTRAINT obstructed_ordinal_check TO blob_chunks_ordinal_check");
        } else {
            sql(&fixture, &format!("DROP TABLE td_mta.{obstruction}"));
        }
        if previous_version == 9 {
            sql(
                &fixture,
                "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC",
            );
        }
        if previous_version == 26 {
            sql(&fixture,"UPDATE td_mta.recipients SET state=2,attempt_count=1,attempt_id=decode(repeat('03',16),'hex'),last_attempt_at=0,phase=3,rcpt_reply='250 accepted'");
            assert_eq!(
                upgrade(&fixture.request(), &trust, deadline()),
                Err(SchemaError::Database)
            );
            assert_eq!(
                sql(&fixture, "SELECT count(*) FROM td_mta.schema_migrations"),
                "26"
            );
            assert_eq!(
                sql(
                    &fixture,
                    "SELECT to_regclass('td_mta.attempt_final_reservations') IS NULL"
                ),
                "t"
            );
            sql(&fixture,"UPDATE td_mta.recipients SET state=7,uncertain=1,phase=4,reason=9,next_attempt_at=1");
        }
        let receipt = upgrade(&fixture.request(), &trust, deadline()).unwrap();
        assert_eq!(receipt.version, 32);
        assert_eq!(
            sql(
                &fixture,
                "SELECT singleton,blocked FROM td_mta.runtime_barrier"
            ),
            "t|f"
        );
        assert_eq!(
            sql(
                &fixture,
                "SELECT count(*) FROM td_mta.submission_cancel_receipts"
            ),
            "0"
        );
        assert_eq!(
            sql(&fixture, "SELECT count(*) FROM td_mta.route_health"),
            "0"
        );
        assert_eq!(
            sql(
                &fixture,
                "SELECT count(*) FROM td_mta.route_health_receipts"
            ),
            "0"
        );
        assert_eq!(
            sql(
                &fixture,
                "SELECT singleton,published_at IS NULL FROM td_mta.queue_expiry_progress"
            ),
            "t|t"
        );
        assert_eq!(
            sql(
                &fixture,
                "SELECT count(*) FROM td_mta.submission_expiry_receipts"
            ),
            "0"
        );
        assert_eq!(
            sql(&fixture, "SELECT count(*) FROM td_mta.submission_routes"),
            "0"
        );
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
#[ignore = "requires local PostgreSQL 17 tools"]
fn real_postgres_https_authentication_reuses_only_closed_qualified_sessions() {
    exercise_configuration(ConfigurationExercise::HttpAuthentication);
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

#[test]
#[ignore = "real PostgreSQL 17, td-mail and isolated td-fetch fixture executables required"]
fn real_postgres_td_mail_reads_and_downloads() {
    exercise_configuration(ConfigurationExercise::Client);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools required"]
fn real_postgres_http_drafts_create_read_and_settle() {
    exercise_configuration(ConfigurationExercise::HttpDrafts);
}

#[test]
#[ignore = "real PostgreSQL 17, td-mail and isolated td-fetch fixture executables required"]
fn real_postgres_td_mail_sends_plain_text() {
    exercise_configuration(ConfigurationExercise::SendClient);
}

#[test]
#[ignore = "real PostgreSQL 17 installation required"]
fn real_postgres_http_upload_owner_retains_and_settles() {
    exercise_configuration(ConfigurationExercise::HttpUploads);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_smtp_owner_retains_and_settles() {
    exercise_configuration(ConfigurationExercise::ReceivingOwner);
}

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_smtp_worker_receives_and_drains() {
    exercise_configuration(ConfigurationExercise::ReceivingTransport);
}

#[path = "postgres_probe/smtp_transport.rs"]
mod smtp_transport;

#[test]
#[ignore = "real local PostgreSQL 17 tools, including psql, required"]
fn real_postgres_smtp_worker_preserves_starttls_receipts() {
    exercise_configuration(ConfigurationExercise::ReceivingTls);
}

#[path = "postgres_probe/multi_account.rs"]
mod multi_account;
#[path = "postgres_probe/multi_receiving.rs"]
mod multi_receiving;
#[path = "postgres_probe/multi_relay.rs"]
mod multi_relay;

#[path = "postgres_probe/serving.rs"]
mod serving;
#[path = "postgres_probe/serving_relay.rs"]
mod serving_relay;

#[test]
#[ignore = "real local PostgreSQL tools required"]
fn real_postgres_executable_receives_and_serves_database_profiles() {
    application_logs::real_postgres_logs_bound_retention_rate_wrap_and_inspection();
    acme::real_postgres_acme_fences_ownership_and_pins_complete_pairs();
    recovery::real_postgres_restore_quarantines_pending_delivery_and_changes_epoch();
    recovery::real_postgres_recovery_uncertain_commit_keeps_retained_new_epoch();
    exercise_configuration(ConfigurationExercise::AccountAdmin);
    exercise_configuration(ConfigurationExercise::AccountRoutes);
    exercise_configuration(ConfigurationExercise::Imports);
    exercise_configuration(ConfigurationExercise::Reconcile);
    exercise_configuration(ConfigurationExercise::Unmapped);
    exercise_configuration(ConfigurationExercise::FolderReconcile);
    audit_scaling::exercise();
    exercise_configuration(ConfigurationExercise::MultiAccount);
    exercise_configuration(ConfigurationExercise::Serving);
}

#[test]
#[ignore = "real local PostgreSQL tools required"]
fn real_postgres_ipv6_receiving_profiles() {
    exercise_configuration(ConfigurationExercise::Serving);
}

#[path = "postgres_probe/performance.rs"]
mod performance;

/// On demand, never in a gate:
/// `td-builder gate-crates crypto-cargo perf --manifest-path td-mta/Cargo.toml`.
#[test]
#[ignore = "on-demand performance harness; real local PostgreSQL 17 tools required"]
fn real_postgres_performance_harness() {
    exercise_configuration(ConfigurationExercise::Performance);
}

#[path = "postgres_probe/account_routes.rs"]
mod account_routes;

#[test]
#[ignore = "requires PostgreSQL 17 tools"]
fn real_postgres_account_routes_keep_submission_and_delivery_policies_separate() {
    exercise_configuration(ConfigurationExercise::AccountRoutes);
}

enum ConfigurationExercise {
    HttpAuthentication,
    AccountRoutes,
    FolderReconcile,
    Unmapped,
    Reconcile,
    Imports,
    AccountAdmin,
    MultiAccount,
    Serving,
    Performance,
    ReceivingTls,
    ReceivingTransport,
    ReceivingOwner,
    HttpUploads,
    HttpDrafts,
    Client,
    SendClient,
    Submissions,
    SubmissionInput,
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
    let mut fixture = Fixture::new();
    if matches!(exercise, ConfigurationExercise::Submissions) {
        fixture.queue_runtime = Some(queue_expiry::Runtime::new(&fixture));
    }
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
    let serves_profiles = matches!(
        exercise,
        ConfigurationExercise::Serving | ConfigurationExercise::Performance
    );
    let mut relay_socket = serves_profiles.then(|| TcpListener::bind("127.0.0.1:0").unwrap());
    let mut relay_dns = serves_profiles.then(|| std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let desktop = format!(
        r#"[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
route = "personal-smarthost"
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
    let desktop = if let (Some(relay), Some(dns)) = (&relay_socket, &relay_dns) {
        desktop
            .replace(
                "port = 465",
                &format!("port = {}", relay.local_addr().unwrap().port()),
            )
            .replace("127.0.0.1:53", &dns.local_addr().unwrap().to_string())
    } else {
        desktop
    };
    let receiving_bind = if matches!(exercise, ConfigurationExercise::Serving) {
        "[::1]:0"
    } else {
        "127.0.0.1:0"
    };
    let mut https_socket = matches!(
        exercise,
        ConfigurationExercise::Client
            | ConfigurationExercise::SendClient
            | ConfigurationExercise::Serving
            | ConfigurationExercise::Performance
    )
    .then(|| TcpListener::bind(receiving_bind).unwrap());
    // The private test namespace resolves localhost only to IPv4. Keep an
    // explicit IPv4 listener for the exporter while driving SMTP/HTTPS over IPv6.
    let https_ipv4 = matches!(exercise, ConfigurationExercise::Serving).then(|| {
        TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            https_socket.as_ref().unwrap().local_addr().unwrap().port(),
        ))
        .unwrap()
    });
    let desktop = if let Some(socket) = &https_socket {
        let address = socket.local_addr().unwrap();
        desktop
            .replace(
                "https://localhost",
                &format!("https://localhost:{}", address.port()),
            )
            .replace("bind = \"127.0.0.1:443\"", &format!("bind = \"{address}\""))
    } else {
        desktop
    };
    let desktop = if let Some(socket) = &https_ipv4 {
        format!(
            "{desktop}[listener \"https-ipv4\"]\nkind = \"https\"\nbind = \"{}\"\ncertificate = \"public\"\n",
            socket.local_addr().unwrap()
        )
    } else {
        desktop
    };
    let desktop = if matches!(exercise, ConfigurationExercise::Submissions) {
        desktop + "[limits]\nsmtp_recipients = 1000\nindex_cache_bytes = 2097152\n"
    } else {
        desktop
    };
    let cloud_source = if serves_profiles {
        desktop
            .split("[listener \"https\"]")
            .next()
            .unwrap()
            .to_owned()
    } else {
        desktop.clone()
    };
    let cloud = cloud_source
        + r#"[listener "smtp"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
"#;
    let mut smtp_socket = matches!(
        exercise,
        ConfigurationExercise::ReceivingTransport
            | ConfigurationExercise::ReceivingTls
            | ConfigurationExercise::Serving
            | ConfigurationExercise::Performance
    )
    .then(|| TcpListener::bind(receiving_bind).unwrap());
    let cloud = if let Some(socket) = &smtp_socket {
        cloud.replace("0.0.0.0:25", &socket.local_addr().unwrap().to_string())
    } else {
        cloud
    };
    // Concurrent loopback streams share one peer address.
    let cloud = if matches!(
        exercise,
        ConfigurationExercise::Performance
            | ConfigurationExercise::ReceivingTransport
            | ConfigurationExercise::ReceivingTls
    ) {
        let limits = "session_limit = 1\nper_peer_limit = 1\n";
        assert_eq!(cloud.matches(limits).count(), 1, "cloud listener limits");
        cloud.replace(
            limits,
            &format!(
                "session_limit = {0}\nper_peer_limit = {0}\n[limits]\nsmtp_per_peer = {0}\n",
                performance::SMTP_STREAMS_MAX
            ),
        )
    } else {
        cloud
    };
    let cloud_roles = if serves_profiles {
        5
    } else if matches!(exercise, ConfigurationExercise::Submissions) {
        7
    } else {
        3
    };
    let one = Profile {
        name: "cloud",
        roles: Roles::from_bits(cloud_roles).unwrap(),
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
    assert_eq!(candidate.profile(0).unwrap().roles.bits(), cloud_roles);
    assert!(candidate.profile(2).is_none());
    assert_eq!(candidate.digest(), prepare(&[one, two]).unwrap().digest());
    assert_eq!(
        candidate.route("desktop").unwrap(),
        candidate.route("cloud").unwrap()
    );
    assert_eq!(
        candidate.route("desktop").unwrap().name(),
        "personal-smarthost"
    );
    assert!(candidate.route("unknown").is_none());
    let conflicting = cloud.replace("username = \"relay-user\"", "username = \"relay-other\"");
    assert_eq!(
        prepare(&[
            two,
            Profile {
                source: conflicting.as_bytes(),
                ..one
            }
        ])
        .err(),
        Some(configuration::Error::RouteConflict)
    );
    let separate = conflicting.replace(
        "route = \"personal-smarthost\"",
        "route = \"other-smarthost\"",
    );
    prepare(&[
        two,
        Profile {
            source: separate.as_bytes(),
            ..one
        },
    ])
    .unwrap();

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
        let override_source = if matches!(exercise, ConfigurationExercise::Submissions) {
            override_source
                .strip_prefix("[limits]\n")
                .unwrap_or(override_source)
        } else {
            override_source
        };
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
                if cloud_roles == 1 {
                    "receive"
                } else if cloud_roles == 5 {
                    "receive,relay"
                } else if cloud_roles == 7 {
                    "receive,jmap,relay"
                } else {
                    "receive,jmap"
                },
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
    let first = if matches!(exercise, ConfigurationExercise::Flags) {
        let change = selection_commands::prepare(&fixture, &connection, epoch, version);
        selection_commands::absent(&fixture, &connection, change);
        change
    } else {
        selection::prepare(epoch, 0, version, &candidate).unwrap()
    };
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
    if matches!(exercise, ConfigurationExercise::Flags) {
        selection_commands::recorded(&fixture, &connection, first, 1);
    }
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
    if matches!(exercise, ConfigurationExercise::FolderReconcile) {
        folder_reconcile::exercise(&fixture, &request, &trust, epoch, &connection);
        return;
    }
    if matches!(exercise, ConfigurationExercise::Unmapped) {
        unmapped::exercise(&fixture, &request, &trust, epoch, &connection);
        return;
    }
    if matches!(exercise, ConfigurationExercise::Reconcile) {
        reconciliation::exercise(&fixture, &request, &trust, epoch, &connection);
        return;
    }
    if matches!(exercise, ConfigurationExercise::Imports) {
        imports::exercise(&fixture, &request, &trust, epoch, &connection);
        return;
    }
    if matches!(exercise, ConfigurationExercise::AccountAdmin) {
        account_workflow::exercise(&fixture, &request, &trust, epoch, &connection);
        return;
    }
    if matches!(exercise, ConfigurationExercise::AccountRoutes) {
        account_routes::exercise(
            &fixture,
            &request,
            &trust,
            epoch,
            &shared,
            &desktop,
            &connection,
        );
        return;
    }
    if matches!(exercise, ConfigurationExercise::MultiAccount) {
        multi_account::exercise(&fixture, &request, &trust, epoch, &shared, &desktop);
        return;
    }
    let mut serving = fixture.request();
    serving.user = "mail_runtime";
    serving.mode = td_mta::postgres_probe::Mode::Runtime;
    let profile_request = if matches!(
        exercise,
        ConfigurationExercise::Flags
            | ConfigurationExercise::Read
            | ConfigurationExercise::HttpAuthentication
            | ConfigurationExercise::HttpDrafts
            | ConfigurationExercise::HttpUploads
            | ConfigurationExercise::ReceivingOwner
            | ConfigurationExercise::ReceivingTransport
            | ConfigurationExercise::ReceivingTls
            | ConfigurationExercise::Client
            | ConfigurationExercise::SendClient
    ) {
        create_runtime_login(&fixture);
        grant_runtime_mail(&fixture);
        &serving
    } else {
        &request
    };
    if serves_profiles {
        create_runtime_login(&fixture);
        grant_runtime_mail(&fixture);
        acme::publish_fixture_pair(&fixture);
        let managed = |source: &str| {
            source.replace(&format!("mode = \"files\"\nchain_file = \"/postgres-material/{}\"\nkey_file = \"/postgres-material/{}\"", Version::from_bytes([2;16]), Version::from_bytes([3;16])), "mode = \"acme\"")
                + "\n[acme]\ndirectory = \"https://acme.example.test/directory\"\ncontact = \"operator@example.test\"\nterms_accepted = true\n[listener \"http01\"]\nkind = \"http01\"\nbind = \"127.0.0.1:80\"\n"
        };
        let managed_cloud = managed(&cloud);
        let managed_desktop = managed(&desktop);
        let managed = prepare(&[
            Profile {
                source: managed_cloud.as_bytes(),
                ..one
            },
            Profile {
                source: managed_desktop.as_bytes(),
                ..two
            },
        ])
        .unwrap();
        let version = versions::Version::from_bytes([24; 16]);
        versions::publish(&request, &trust, deadline(), version, &managed).unwrap();
        let change = selection::prepare(epoch, 1, version, &managed).unwrap();
        selection::publish(&request, &trust, deadline(), change).unwrap();
        let sockets = (
            smtp_socket.take().unwrap(),
            https_socket.take().unwrap(),
            relay_socket.take().unwrap(),
            relay_dns.take().unwrap(),
        );
        if matches!(exercise, ConfigurationExercise::Performance) {
            performance::exercise(&fixture, sockets.0, sockets.1, sockets.2, sockets.3);
        } else {
            serving::exercise(
                &fixture,
                sockets.0,
                sockets.1,
                sockets.2,
                sockets.3,
                https_ipv4.unwrap(),
            );
        }
        return;
    }
    for (name, roles) in [("desktop", 2), ("cloud", cloud_roles)] {
        let mut domain = GenerationSet::<TlsPolicies>::at_startup();
        let prepared = runtime::load(
            profile_request,
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
        assert_eq!(prepared.binding().route(), candidate.route(name).unwrap());
        assert_eq!(
            prepared
                .binding()
                .serves_route(candidate.route(name).unwrap()),
            roles & 4 != 0
        );
        assert_eq!(prepared.binding().quotas(), candidate.quotas());
        assert_eq!(prepared.configuration().roles().bits(), roles);
        let limits = prepared
            .configuration()
            .candidate()
            .resources()
            .resources()
            .limits();
        assert_eq!(prepared.binding().header_maximum(), limits.header_bytes);
        assert_eq!(
            prepared.binding().recipient_maximum(),
            limits.smtp_recipients
        );
        let sending = td_mta::ids::IdentityId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let unknown = td_mta::ids::IdentityId::from_bytes([0xef; 16]);
        assert_eq!(
            prepared.binding().identity_email(sending),
            Some("main@example.test")
        );
        assert_eq!(prepared.binding().identity_email(unknown), None);
        assert_eq!(
            prepared.binding().clone().identity_email(sending),
            Some("main@example.test")
        );
        assert!(!format!("{:?}", prepared.binding()).contains("main@example.test"));
        assert_eq!(
            prepared.binding().identity_catalog().is_some(),
            roles & 2 != 0
        );
        if name == "desktop" {
            match exercise {
                ConfigurationExercise::AccountAdmin
                | ConfigurationExercise::AccountRoutes
                | ConfigurationExercise::MultiAccount
                | ConfigurationExercise::Imports
                | ConfigurationExercise::Reconcile
                | ConfigurationExercise::Unmapped
                | ConfigurationExercise::FolderReconcile => {
                    panic!("multi-account fixture must return before singleton exercises")
                }
                ConfigurationExercise::Client
                | ConfigurationExercise::SendClient
                | ConfigurationExercise::Serving
                | ConfigurationExercise::Performance => {}
                ConfigurationExercise::Submissions => {
                    exercise_submission_input(&fixture, &request, &trust, prepared.binding(), true)
                }
                ConfigurationExercise::SubmissionInput => {
                    exercise_submission_input(&fixture, &request, &trust, prepared.binding(), false)
                }
                ConfigurationExercise::Receiving
                | ConfigurationExercise::ReceivingOwner
                | ConfigurationExercise::ReceivingTransport
                | ConfigurationExercise::ReceivingTls => {}
                ConfigurationExercise::Deletion => {
                    exercise_deletion(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Moves => {
                    exercise_moves(&fixture, &request, &trust, prepared.binding())
                }
                ConfigurationExercise::Flags => {
                    exercise_flags(&fixture, profile_request, &trust, prepared.binding())
                }
                ConfigurationExercise::HttpUploads => {
                    exercise_http_upload_owner(&fixture, &request, &trust, prepared.binding())
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
                ConfigurationExercise::HttpDrafts => {
                    exercise_read(&fixture, profile_request, &trust, prepared.binding(), true)
                }
                ConfigurationExercise::Read => {
                    exercise_read(&fixture, profile_request, &trust, prepared.binding(), false)
                }
                ConfigurationExercise::HttpAuthentication => {
                    let creation = td_mta::postgres_devices::prepare_creation(
                        epoch,
                        account,
                        "auth fixture",
                        1,
                    )
                    .unwrap();
                    td_mta::postgres_devices::publish_creation(
                        &request,
                        &trust,
                        deadline(),
                        &creation,
                    )
                    .unwrap();
                    exercise_http_auth(
                        &fixture,
                        prepared.binding(),
                        creation.password(),
                        creation.device_id(),
                    );
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
        if name == "cloud" && matches!(exercise, ConfigurationExercise::ReceivingOwner) {
            exercise_smtp_receiving_owner(&fixture, prepared.binding());
            exercise_smtp_storage(&fixture, prepared.binding());
        }
        let (binding, configuration, policies) = prepared.into_parts().unwrap();
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
        if name == "desktop"
            && matches!(
                exercise,
                ConfigurationExercise::Client | ConfigurationExercise::SendClient
            )
        {
            mail_client::exercise(
                &fixture,
                profile_request,
                &trust,
                &binding,
                &configuration,
                &active,
                if matches!(exercise, ConfigurationExercise::SendClient) {
                    mail_client::Mode::Send(https_socket.take().unwrap())
                } else {
                    mail_client::Mode::Read(https_socket.take().unwrap())
                },
            );
            return;
        }
        if name == "cloud"
            && matches!(
                exercise,
                ConfigurationExercise::ReceivingTransport | ConfigurationExercise::ReceivingTls
            )
        {
            smtp_transport::exercise(
                &fixture,
                &binding,
                &configuration,
                &active,
                smtp_socket.take().unwrap(),
                matches!(exercise, ConfigurationExercise::ReceivingTls),
            );
            return;
        }
        let https = TlsPolicies::listener(&active, "https").unwrap();
        assert_eq!(
            TlsPolicies::role(&active, https).unwrap(),
            td_mta::tls_policy::PolicyRole::Https
        );
        assert_eq!(
            TlsPolicies::listener(&active, "smtp").is_ok(),
            roles & 1 != 0
        );
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
                versions::Error::Validation(
                    configuration::Error::Provider | configuration::Error::Profile
                )
            ))
        ) {
            // Global text-owner or generation-ID contention can refuse preparation.
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
    let retained_first = first.encode();
    let first = selection::Change::decode(&retained_first).unwrap();
    if matches!(exercise, ConfigurationExercise::Flags) {
        selection_commands::recorded(&fixture, &connection, first, 1);
    }
    assert_eq!(
        selection::publish(&request, &trust, deadline(), first).unwrap(),
        selected
    );
    let delay = if matches!(exercise, ConfigurationExercise::Flags) {
        11
    } else {
        3
    };
    sql(&fixture, &format!("CREATE FUNCTION public.delay_selection_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep({delay}); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_selection AFTER INSERT ON td_mta.config_selection_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_selection_commit()"));
    let uncertain = selection::prepare(epoch, 2, version, &candidate).unwrap();
    let retained_uncertain = uncertain.encode();
    if matches!(exercise, ConfigurationExercise::Flags) {
        selection_commands::uncertain(&fixture, &connection, uncertain);
    } else {
        assert_eq!(
            selection::publish(
                &request,
                &trust,
                Instant::now() + Duration::from_secs(2),
                uncertain
            ),
            Err(selection::Error::Indeterminate)
        );
    }
    thread::sleep(Duration::from_secs(5));
    sql(
        &fixture,
        "DROP TRIGGER delay_selection ON td_mta.config_selection_receipts",
    );
    let uncertain = selection::Change::decode(&retained_uncertain).unwrap();
    let landed = selection::inspect(&request, &trust, deadline(), uncertain)
        .unwrap()
        .unwrap();
    assert_eq!(landed.generation, 3);
    if matches!(exercise, ConfigurationExercise::Flags) {
        selection_commands::recorded(&fixture, &connection, uncertain, 3);
    }
    let mut altered = retained_uncertain;
    altered[57] ^= 1;
    let altered = selection::Change::decode(&altered).unwrap();
    assert_eq!(
        selection::inspect(&request, &trust, deadline(), altered),
        Err(selection::Error::Conflict)
    );
    assert_eq!(
        selection::publish(&request, &trust, deadline(), altered),
        Err(selection::Error::Conflict)
    );
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
    let first = selection::Change::decode(&retained_first).unwrap();
    if matches!(exercise, ConfigurationExercise::Flags) {
        selection_commands::recorded(&fixture, &connection, first, 1);
    }
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
#[ignore = "requires PostgreSQL 17's initdb, postgres, pg_ctl and psql on PATH"]
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
            identity: listing.identity,
            mailbox: creation.inbox(),
            total_emails: 5,
            unread_emails: 2,
            total_threads: 2,
            unread_threads: 2
        }
    );
    sql(&fixture, &format!("BEGIN;
        INSERT INTO td_mta.mailboxes SELECT decode('{account}','hex'),decode(repeat(n,16),'hex'),'Shared subset',NULL,NULL,0,1,decode('0000000000000001','hex') FROM (VALUES ('44'),('45')) v(n);
        INSERT INTO td_mta.mailboxes VALUES (decode('{other}','hex'),decode(repeat('44',16),'hex'),'Other account',NULL,NULL,0,1,decode('0000000000000002','hex'));
        INSERT INTO td_mta.memberships SELECT account,id,decode(repeat('44',16),'hex'),changed FROM td_mta.emails WHERE account=decode('{account}','hex') AND id IN (decode(lpad('2',32,'0'),'hex'),decode(lpad('3',32,'0'),'hex'),decode(lpad('4',32,'0'),'hex'));
        COMMIT"));
    let grouped = read().unwrap();
    assert_eq!(grouped.mailboxes.len(), 3);
    let subset = grouped
        .mailboxes
        .iter()
        .find(|row| row.id.as_bytes() == &[0x44; 16])
        .unwrap();
    assert_eq!(
        (
            subset.counts.total_emails,
            subset.counts.unread_emails,
            subset.counts.total_threads,
            subset.counts.unread_threads
        ),
        (3, 1, 2, 1)
    );
    let empty = grouped
        .mailboxes
        .iter()
        .find(|row| row.id.as_bytes() == &[0x45; 16])
        .unwrap();
    assert_eq!(
        (
            empty.counts.total_emails,
            empty.counts.unread_emails,
            empty.counts.total_threads,
            empty.counts.unread_threads
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        grouped
            .mailboxes
            .iter()
            .find(|row| row.id == creation.inbox())
            .unwrap()
            .counts,
        listing.mailboxes[0].counts
    );
    sql(&fixture, "DELETE FROM td_mta.memberships WHERE mailbox_id=decode(repeat('44',16),'hex'); DELETE FROM td_mta.mailboxes WHERE id IN (decode(repeat('44',16),'hex'),decode(repeat('45',16),'hex'))");
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
#[ignore = "requires PostgreSQL 17's initdb, postgres, pg_ctl and psql on PATH"]
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
            Instant::now() + Duration::from_secs(30),
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
    let mut runtime = fixture.request();
    runtime.mode = td_mta::postgres_probe::Mode::Runtime;
    assert_eq!(
        body::with_email(
            &runtime,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Connection(Error::RuntimeRole))
    );
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    runtime.user = "mail_runtime";
    sql(&fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        body::with_email(
            &runtime,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Connection(Error::RuntimeMaintenance))
    );
    sql(&fixture, "UPDATE td_mta.runtime_barrier SET blocked=false; REVOKE SELECT ON td_mta.emails FROM mail_runtime");
    assert_eq!(
        body::with_email(
            &runtime,
            &trust,
            deadline(),
            selected,
            device.password(),
            unused
        ),
        Err(body::Error::Connection(Error::RuntimePrivileges))
    );
    grant_runtime_mail(&fixture);
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
        ports::BlobReader, postgres_body as bodies, postgres_devices as devices,
        postgres_uploads as uploads,
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
        bodies::with_pinned_blob(
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
                assert_eq!(body.id(), blob);
                assert_eq!(body.digest(), &metadata.body.digest);
                body.check_deadline()?;
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
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
    };
    let clock = RuntimeClock::new();
    let allowance = || {
        let cap = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        Meter::new(
            cap,
            Charge {
                io_bytes: 3 * uploads::MAX_BYTES as u64,
                output_bytes: uploads::MAX_BYTES as u64,
                records: 4096,
                ..Charge::default()
            },
        )
    };
    let prepare = |bytes: &[u8]| {
        let mut meter = allowance();
        let before = meter.remaining();
        let upload =
            uploads::prepare_with_meter(binding, device, bytes, deadline(), &mut meter, &clock)
                .unwrap();
        assert!(before.io_bytes - meter.remaining().io_bytes >= 2 * bytes.len() as u64);
        assert_eq!(
            before.output_bytes - meter.remaining().output_bytes,
            bytes.len() as u64
        );
        upload
    };
    for budget in [
        Charge {
            io_bytes: 0,
            output_bytes: 1,
            records: 100,
            ..Charge::default()
        },
        Charge {
            io_bytes: 1000,
            output_bytes: 0,
            records: 100,
            ..Charge::default()
        },
        Charge {
            io_bytes: 1000,
            output_bytes: 1,
            records: 0,
            ..Charge::default()
        },
    ] {
        let mut meter = Meter::new(
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
            budget,
        );
        assert!(matches!(
            uploads::prepare_with_meter(binding, device, b"x", deadline(), &mut meter, &clock),
            Err(uploads::Error::Capacity)
        ));
        assert!(meter.stopped().is_some());
        assert!(matches!(
            uploads::prepare_with_meter(binding, device, b"", deadline(), &mut meter, &clock),
            Err(uploads::Error::Capacity)
        ));
    }
    let mut expired = Meter::new(
        Deadline::after(clock.sample().unwrap().monotonic, 0).unwrap(),
        allowance().remaining(),
    );
    assert!(matches!(
        uploads::prepare_with_meter(binding, device, b"x", deadline(), &mut expired, &clock),
        Err(uploads::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    ));

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
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
    };
    let clock = RuntimeClock::new();
    let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    let input = message("first@example.test", &[]);
    let draft_request = || drafts::DraftRequest {
        binding,
        device,
        mailbox,
        expected: Some(1),
    };
    for budget in [
        Charge::default(),
        Charge {
            io_bytes: 65536,
            records: 10000,
            output_bytes: 1,
            ..Default::default()
        },
    ] {
        let mut exhausted = Meter::new(until, budget);
        assert!(matches!(
            drafts::prepare_with_meter(draft_request(), &input, deadline(), &mut exhausted, &clock),
            Err(drafts::Error::Compose(td_mta::ports::Error::Capacity))
        ));
        assert!(exhausted.stopped().is_some());
    }
    let mut expired = Meter::new(
        Deadline::after(clock.sample().unwrap().monotonic, 0).unwrap(),
        Charge {
            io_bytes: 65536,
            records: 10000,
            output_bytes: 65536,
            ..Default::default()
        },
    );
    assert!(matches!(
        drafts::prepare_with_meter(draft_request(), &input, deadline(), &mut expired, &clock),
        Err(drafts::Error::Compose(td_mta::ports::Error::Deadline))
    ));
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    let mut meter = Meter::new(
        until,
        Charge {
            io_bytes: 65536,
            records: 10000,
            output_bytes: 65536,
            ..Default::default()
        },
    );
    meter
        .charge(
            clock.sample().unwrap().monotonic,
            Charge {
                records: 7,
                ..Default::default()
            },
        )
        .unwrap();
    let before = meter.remaining();
    assert!(matches!(
        drafts::prepare_with_meter(draft_request(), &input, Instant::now(), &mut meter, &clock),
        Err(drafts::Error::Connection(Error::Deadline))
    ));
    let first = drafts::prepare_with_meter(draft_request(), &input, deadline(), &mut meter, &clock)
        .unwrap();
    let after = meter.remaining();
    assert!(
        after.records < before.records
            && after.io_bytes < before.io_bytes
            && after.output_bytes < before.output_bytes
    );

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

#[path = "postgres_probe/authentication_reuse.rs"]
mod authentication_reuse;

fn exercise_http_auth(
    fixture: &Fixture,
    binding: &td_mta::postgres_runtime_profile::Binding,
    password: &[u8],
    device: td_mta::ids::DeviceId,
) {
    use td_mta::{
        http_auth::Authenticator,
        ports::{Clock, Deadline, Error, Tick},
    };
    let loaded = |user| td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://{user}:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let connection = loaded("mail_runtime");
    let clock = td_mta::clock::RuntimeClock::new();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let accounts = [("reader", binding)];
    let principal = td_mta::http_auth::Principal {
        account: binding.account(),
        device,
    };
    let auth =
        td_mta::postgres_http_auth::Authentication::new(&connection, executable, &accounts, &clock)
            .unwrap();
    let until = || Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    assert_eq!(
        auth.authenticate("reader", password, until()),
        Ok(Some(principal))
    );
    assert_eq!(auth.authenticate("Reader", password, until()), Ok(None));
    assert_eq!(auth.authenticate("unknown", password, until()), Ok(None));
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
    writeln!(
        input,
        "BEGIN; LOCK TABLE td_mta.config_selection IN ACCESS EXCLUSIVE MODE;"
    )
    .unwrap();
    let stop = deadline();
    while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%ACCESS EXCLUSIVE MODE%'") != "1" {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(5));
    }
    {
        // The production adapter belongs to one fixed authentication thread.
        let concurrent = td_mta::postgres_http_auth::Authentication::new(
            &connection,
            executable,
            &accounts,
            &clock,
        )
        .unwrap();
        thread::scope(|scope| {
            let worker = scope.spawn(|| concurrent.authenticate("reader", password, until()));
            let stop = deadline();
            while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND wait_event_type='Lock'") != "1" {
            assert!(!worker.is_finished(), "authentication ended before lock contention");
            assert!(Instant::now() < stop);
            thread::sleep(Duration::from_millis(5));
        }
            assert_eq!(
                concurrent.authenticate("reader", password, until()),
                Err(Error::Busy)
            );
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
                        td_mta::postgres_schema::MIGRATION_KEY,
                    )
                ),
                "f"
            );
            assert_eq!(worker.join().unwrap(), Err(Error::Invalid));
        });
    }
    for _ in 0..3 {
        let short = Deadline::after(clock.sample().unwrap().monotonic, 500).unwrap();
        assert_eq!(
            auth.authenticate("reader", password, short),
            Err(Error::Deadline)
        );
    }
    writeln!(input, "COMMIT;").unwrap();
    drop(input);
    wait_command(&mut blocker);
    assert_eq!(
        auth.authenticate("reader", password, until()),
        Ok(Some(principal))
    );
    assert_eq!(auth.authenticate("reader", b"malformed", until()), Ok(None));
    let mut wrong = password.to_vec();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    assert_eq!(auth.authenticate("reader", &wrong, until()), Ok(None));
    assert_eq!(
        auth.authenticate("reader", password, Deadline::after(Tick(0), 0).unwrap()),
        Err(Error::Deadline)
    );
    let owner = loaded("td_probe");
    let unqualified =
        td_mta::postgres_http_auth::Authentication::new(&owner, executable, &accounts, &clock)
            .unwrap();
    assert_eq!(
        unqualified.authenticate("reader", password, until()),
        Err(Error::Invalid)
    );
    let revoked = td_mta::postgres_devices::prepare_creation(
        binding.epoch(),
        binding.account(),
        "auth revocation",
        1,
    )
    .unwrap();
    td_mta::postgres_devices::publish_creation(
        &fixture.request(),
        &connection.trust,
        deadline(),
        &revoked,
    )
    .unwrap();
    assert_eq!(
        auth.authenticate("reader", revoked.password(), until()),
        Ok(Some(td_mta::http_auth::Principal {
            account: binding.account(),
            device: revoked.device_id()
        }))
    );
    let revoke = td_mta::postgres_devices::prepare_revocation(
        binding.epoch(),
        binding.account(),
        revoked.device_id(),
        2,
    )
    .unwrap();
    td_mta::postgres_devices::publish_revocation(
        &fixture.request(),
        &connection.trust,
        deadline(),
        &revoke,
    )
    .unwrap();
    assert_eq!(
        auth.authenticate("reader", revoked.password(), until()),
        Ok(None)
    );
    assert_eq!(
        auth.authenticate("reader", password, until()),
        Ok(Some(principal))
    );
    drop(auth);
    let auth =
        td_mta::postgres_http_auth::Authentication::new(&connection, executable, &accounts, &clock)
            .unwrap();
    authentication_reuse::exercise(fixture, &auth, password, principal, |ms| {
        Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap()
    });
}

fn exercise_read_dispatch(
    fixture: &Fixture,
    snapshot: &mut td_mta::postgres_read::Snapshot<'_>,
    mailbox: td_mta::ids::MailboxId,
    emails: [td_mta::ids::EmailId; 2],
    blocked: bool,
) {
    use std::sync::Arc;
    use td_mta::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        jmap_request::{dispatch, response, OwnedValidation, CORE, MAIL, SUBMISSION},
        limits::Limits,
        ports::{Clock, Deadline, Tick},
        store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    };
    let mut id_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(snapshot.identity().account, &mut id_bytes)
        .unwrap()
        .to_owned();
    let mailbox = td_mta::wire::encode_id(mailbox, &mut id_bytes)
        .unwrap()
        .to_owned();
    let missing = td_mta::wire::encode_id(
        td_mta::ids::MailboxId::from_bytes([0xfe; 16]),
        &mut id_bytes,
    )
    .unwrap()
    .to_owned();
    let [first_id, second_id] = emails;
    let first = td_mta::wire::encode_id(first_id, &mut id_bytes)
        .unwrap()
        .to_owned();
    let second = td_mta::wire::encode_id(second_id, &mut id_bytes)
        .unwrap()
        .to_owned();
    let absent_email =
        td_mta::wire::encode_id(td_mta::ids::EmailId::from_bytes([0xfe; 16]), &mut id_bytes)
            .unwrap()
            .to_owned();
    let (low, high) = if first_id.as_bytes() < second_id.as_bytes() {
        (&first, &second)
    } else {
        (&second, &first)
    };
    let window_missing: Vec<_> = (0..33_u128)
        .map(|n| format!("e{:032x}", 0xed00 + n))
        .collect();
    let window_not_found = window_missing
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(",");
    let window_ids = format!("\"{second}\",{window_not_found},\"{first}\",\"{second}\"");
    let input = format!(
        r##"{{"using":["{CORE}","{MAIL}","{SUBMISSION}"],"methodCalls":[
        ["Mailbox/get",{{"accountId":"{account}","ids":["{mailbox}","{mailbox}","{missing}"],"properties":["name","totalEmails","myRights"]}},"a"],
        ["Mailbox/get",{{"accountId":"{account}","#ids":{{"resultOf":"a","name":"Mailbox/get","path":"/list/*/id"}},"properties":["id"]}},"b"],
        ["Mailbox/get",{{"accountId":"{account}","ids":null,"properties":["name"]}},"c"],
        ["Mailbox/get",{{"accountId":"{account}","properties":["unsupported"]}},"d"],
        ["Thread/get",{{"accountId":"{account}"}},"e"],
        ["Core/echo",{{"after":true}},"f"],
        ["Email/get",{{"accountId":"{account}","ids":["{second}","{first}","{second}","{absent_email}"],"fetchTextBodyValues":true}},"g"],
        ["Email/get",{{"accountId":"{account}","#ids":{{"resultOf":"g","name":"Email/get","path":"/list/*/id"}},"properties":["id"]}},"h"],
        ["Email/get",{{"accountId":"{account}","ids":null,"properties":["id"]}},"i"],
        ["Email/query",{{"accountId":"{account}","filter":{{"inMailbox":"{mailbox}"}},"limit":1,"calculateTotal":true}},"j"],
        ["Email/get",{{"accountId":"{account}","#ids":{{"resultOf":"j","name":"Email/query","path":"/ids"}},"properties":["id"]}},"k"],
        ["Email/query",{{"accountId":"{account}","sort":[{{"property":"receivedAt","isAscending":true}}],"position":1,"limit":1}},"l"],
        ["Email/query",{{"accountId":"{account}","filter":{{"text":"unsupported"}}}},"m"],
        ["Email/query",{{"accountId":"{account}","limit":0,"calculateTotal":true}},"n"],
        ["Identity/get",{{"accountId":"{account}"}},"o"],
        ["Identity/get",{{"accountId":"{account}","ids":["ibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","ifefefefefefefefefefefefefefefefe"]}},"p"],
        ["Identity/get",{{"accountId":"{account}","#ids":{{"resultOf":"o","name":"Identity/get","path":"/list/*/id"}}}},"q"],
        ["Identity/get",{{"accountId":"afefefefefefefefefefefefefefefefe"}},"r"],
        ["Email/query",{{"accountId":"{account}","filter":{{"operator":"AND","conditions":[{{"inMailbox":"{mailbox}"}},{{"after":"1900-01-01T00:00:00Z"}},{{"before":"9999-01-01T00:00:00Z"}}]}},"sort":[{{"property":"receivedAt","isAscending":false}}],"collapseThreads":false,"limit":1,"position":1,"calculateTotal":true}},"w1"],
        ["Email/query",{{"accountId":"{account}","filter":{{"after":"9999-01-01T00:00:00Z","before":"1900-01-01T00:00:00Z"}},"calculateTotal":true}},"w2"],
        ["Email/query",{{"accountId":"{account}","filter":{{"operator":"AND","conditions":[{{"inMailbox":"{mailbox}"}},{{"inMailbox":"{missing}"}}]}},"calculateTotal":true}},"w3"],
        ["Email/query",{{"accountId":"{account}","filter":{{"after":"2024-01-01T00:00:0 Z"}}}},"w4"],
        ["Email/query",{{"accountId":"{account}","filter":{{"operator":"OR","conditions":[]}}}},"w5"],
        ["Email/query",{{"accountId":"{account}","filter":{{"operator":"AND"}}}},"w6"],
        ["Email/query",{{"accountId":"{account}","filter":{{"minSize":0}},"calculateTotal":true,"limit":0}},"w7"],
        ["Email/query",{{"accountId":"{account}","filter":{{"minSize":1}}}},"w8"],
        ["Email/query",{{"accountId":"{account}","filter":{{"minSize":-1}}}},"w9"],
        ["Email/get",{{"accountId":"{account}","ids":[{window_ids}],"properties":["id"]}},"window"]
    ]}}"##
    );
    let smallest_body = [first_id, second_id]
        .into_iter()
        .map(|id| snapshot.email(id).unwrap().unwrap().body.length as usize)
        .min()
        .unwrap();
    // These bodies predate a stricter incoming-message policy.
    let limits = Limits {
        json_methods: 28,
        message_bytes: smallest_body - 1,
        header_bytes: smallest_body - 1,
        ..Limits::default()
    };
    let mut bytes = vec![0; limits.json_bytes].into_boxed_slice();
    bytes[..input.len()].copy_from_slice(input.as_bytes());
    let tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
    let mut parser = td_json::arena::OwnedCursor::with_input_length(
        bytes,
        tokens,
        input.len(),
        td_json::arena::ParseLimits {
            max_bytes: limits.json_bytes,
            max_depth: limits.json_depth,
            max_tokens: limits.json_tokens,
        },
    )
    .unwrap();
    while !parser.step().unwrap() {}
    let resources = limits.plan().unwrap();
    let mut validation = OwnedValidation::new(
        parser.finish().unwrap(),
        &resources,
        &[CORE, MAIL, SUBMISSION],
    )
    .unwrap();
    while !validation.step().unwrap() {}
    let request = validation.finish().unwrap();
    let directory = fixture.root.join(if blocked {
        "jmap-blocked-responses"
    } else {
        "jmap-responses"
    });
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let mut root = PrivateRoot::open(directory.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let clock = Arc::new(td_mta::clock::RuntimeClock::new());
    let deadline = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    let spool = RequestSpool::open(&mut root, clock.clone(), deadline).unwrap();
    let disk = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
    let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
    let owner =
        ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
    let retention = owner.retain_response(deadline).unwrap();
    let mut preflight = response::Preflight::new(request, "session", retention.maximum()).unwrap();
    while !preflight.step(&mut [0; 4096]).unwrap() {}
    let mut blocker = None;
    if blocked {
        use std::io::Write;
        let mut child = ProbeChild(
            sql_command(fixture)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut input = child.0.stdin.take().unwrap();
        writeln!(
            input,
            "BEGIN; LOCK TABLE td_mta.mailboxes IN ACCESS EXCLUSIVE MODE;"
        )
        .unwrap();
        let stop = self::deadline();
        while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%ACCESS EXCLUSIVE MODE%'") != "1" {
            assert!(Instant::now() < stop);
            thread::sleep(Duration::from_millis(5));
        }
        blocker = Some((child, input));
    }
    let execution = if blocked {
        Deadline::after(clock.sample().unwrap().monotonic, 250).unwrap()
    } else {
        Deadline::after(Tick(0), deadline.tick().0 - 1).unwrap()
    };
    let result = dispatch::run_with_postgres_read(
        preflight.finish().unwrap(),
        retention,
        execution,
        snapshot,
        &clock,
        false,
    );
    if let Some((mut child, mut input)) = blocker {
        use std::io::Write;
        assert!(matches!(result, Err(dispatch::Error::ExecutionDeadline)));
        let expired =
            td_mta::postgres_read::Error::Connection(td_mta::postgres_probe::Error::Deadline);
        assert_eq!(snapshot.check(), Err(expired));
        assert_eq!(snapshot.constrain_deadline(self::deadline()), Err(expired));
        drop(result);
        assert_eq!(owner.used().unwrap(), 0);
        writeln!(input, "COMMIT;").unwrap();
        drop(input);
        wait_command(&mut child);
        return;
    }
    let mut complete = result.unwrap();
    assert_eq!(complete.responses().len(), 28);
    let mut encoder = complete.encoder().unwrap();
    while !encoder.count_step(&mut [0; 4096]).unwrap() {}
    let length = encoder.begin_output().unwrap();
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let progress = encoder.poll_output(&mut chunk).unwrap();
        output.extend_from_slice(&chunk[..progress.written]);
        if progress.complete {
            break;
        }
    }
    assert_eq!(encoder.finish().unwrap(), length);
    assert_eq!(output.len() as u64, length);
    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains(r#""name":"Drafts","totalEmails":2"#),
        "{output}"
    );
    assert!(output.contains(r#""mayReadItems":true,"mayAddItems":false"#));
    assert!(output.contains(&format!(r#""notFound":["{missing}"]"#)));
    assert!(output.contains(&format!(
        r#""list":[{{"id":"{mailbox}"}}],"notFound":[]}},"b""#
    )));
    assert!(output.contains(r#"["error",{"type":"invalidArguments"},"d"]"#));
    assert!(output.contains(r#"["error",{"type":"invalidArguments"},"e"]"#));
    assert!(output.contains(r#"["Core/echo",{"after":true},"f"]"#));
    assert_eq!(output.matches(r#""name":"Drafts""#).count(), 2);
    assert!(
        output.contains(&format!(r#""notFound":["{absent_email}"]}},"g""#)),
        "{output}"
    );
    assert!(output.contains(&format!(
        r#""list":[{{"id":"{second}"}},{{"id":"{first}"}}],"notFound":[]}},"h""#
    )));
    assert_eq!(output.matches(r#"["Email/get","#).count(), 5);
    assert!(output.contains(&format!(r#""list":[{{"id":"{second}"}},{{"id":"{first}"}}],"notFound":[{window_not_found}]}},"window""#)));
    assert_eq!(output.matches(r#"["Email/query","#).count(), 7, "{output}");
    assert!(output.contains(r#""position":0,"total":2,"ids":[]},"w7""#));
    assert!(output.contains(r#"["error",{"type":"unsupportedFilter"},"w8"]"#));
    assert!(output.contains(r#"["error",{"type":"invalidArguments"},"w9"]"#));
    assert!(output.contains(&format!(r#""position":0,"total":2,"ids":["{low}"]}},"j""#)));
    assert!(output.contains(&format!(r#""list":[{{"id":"{low}"}}],"notFound":[]}},"k""#)));
    assert!(output.contains(&format!(r#""position":1,"ids":["{high}"]}},"l""#)));
    assert!(output.contains(r#""position":0,"limit":256,"ids":[]},"m""#));
    assert!(output.contains(r#""position":0,"total":2,"ids":[]},"n""#));
    assert_eq!(output.matches(r#""subject":"Snapshot""#).count(), 2);
    assert_eq!(output.matches(r#""preview":"snapshot text""#).count(), 2);
    assert_eq!(output.matches(r#""attachments":[]"#).count(), 2);
    assert_eq!(output.matches(r#""value":"snapshot text""#).count(), 2);
    assert_eq!(
        output
            .matches(r#""keywords":{"$draft":true,"$seen":true}"#)
            .count(),
        2
    );
    assert!(output.contains(r#""email":"sender@example.test""#));

    assert_eq!(output.matches(r#"["Identity/get","#).count(), 3);
    assert_eq!(output.matches(r#""email":"main@example.test""#).count(), 3);
    assert_eq!(
        output.matches(r#""textSignature":"signature\r\n""#).count(),
        3
    );
    assert!(output.contains(r#""notFound":["ifefefefefefefefefefefefefefefefe"]},"p""#));
    assert!(output.contains(r#"["error",{"type":"accountNotFound"},"r"]"#));
    assert!(
        output.contains(&format!(
            r#""position":1,"total":2,"ids":["{high}"]}},"w1""#
        )),
        "{output}"
    );
    assert!(output.contains(r#""total":0,"ids":[]},"w2""#), "{output}");
    assert!(
        output.contains(r#"["error",{"type":"unsupportedFilter"},"w3"]"#),
        "{output}"
    );
    assert!(
        output.contains(r#"["error",{"type":"invalidArguments"},"w4"]"#),
        "{output}"
    );
    assert!(
        output.contains(r#"["error",{"type":"unsupportedFilter"},"w5"]"#),
        "{output}"
    );
    assert!(
        output.contains(r#"["error",{"type":"invalidArguments"},"w6"]"#),
        "{output}"
    );
    drop(complete);
    assert_eq!(owner.used().unwrap(), 0);
    assert!(!owner.admission_stopped());
}

mod http_fixture {
    use td_mta::ports::{Error, FlushProgress, IoProgress, Transport};
    pub(super) struct Wire {
        pub(super) input: Vec<u8>,
        pub(super) position: usize,
        pub(super) output: Vec<u8>,
        pub(super) fail_write: bool,
    }
    pub(super) struct Observed<'a> {
        pub(super) wire: Wire,
        pub(super) before_write: &'a (dyn Fn() + Sync),
    }
    impl Transport for Observed<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> Result<IoProgress, Error> {
            self.wire.read(bytes)
        }
        fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, Error> {
            (self.before_write)();
            self.wire.write(bytes)
        }
        fn flush(&mut self) -> Result<FlushProgress, Error> {
            self.wire.flush()
        }
        fn close(&mut self) -> Result<FlushProgress, Error> {
            self.wire.close()
        }
        fn abort(&mut self) {
            self.wire.abort();
        }
    }
    impl Transport for Wire {
        fn read(&mut self, bytes: &mut [u8]) -> Result<IoProgress, Error> {
            let count = bytes.len().min(17).min(self.input.len() - self.position);
            if count == 0 {
                return Ok(IoProgress::Closed);
            }
            bytes[..count].copy_from_slice(&self.input[self.position..self.position + count]);
            self.position += count;
            Ok(IoProgress::Bytes(count))
        }
        fn write(&mut self, bytes: &[u8]) -> Result<IoProgress, Error> {
            if self.fail_write {
                return Err(Error::Invalid);
            }
            let count = bytes.len().min(13);
            if count == 0 {
                return Ok(IoProgress::Pending);
            }
            self.output.extend_from_slice(&bytes[..count]);
            Ok(IoProgress::Bytes(count))
        }
        fn flush(&mut self) -> Result<FlushProgress, Error> {
            Ok(FlushProgress::Complete)
        }
        fn close(&mut self) -> Result<FlushProgress, Error> {
            Ok(FlushProgress::Complete)
        }
        fn abort(&mut self) {}
    }
    pub(super) fn credentials(password: &[u8]) -> td_mta::http_auth::BasicCredentials {
        let bytes = [b"reader:".as_slice(), password].concat();
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::from("Basic ");
        for group in bytes.chunks(3) {
            let bits = (u32::from(group[0]) << 16)
                | (u32::from(group.get(1).copied().unwrap_or(0)) << 8)
                | u32::from(group.get(2).copied().unwrap_or(0));
            encoded.push(char::from(alphabet[(bits >> 18) as usize]));
            encoded.push(char::from(alphabet[((bits >> 12) & 63) as usize]));
            encoded.push(if group.len() > 1 {
                char::from(alphabet[((bits >> 6) & 63) as usize])
            } else {
                '='
            });
            encoded.push(if group.len() > 2 {
                char::from(alphabet[(bits & 63) as usize])
            } else {
                '='
            });
        }
        td_mta::http_auth::parse_basic(Some(&encoded)).unwrap()
    }
}

fn exercise_http_read(
    fixture: &Fixture,
    binding: &td_mta::postgres_runtime_profile::Binding,
    password: &[u8],
    device: td_mta::ids::DeviceId,
    drafts: bool,
) {
    use http_fixture::{credentials, Wire};
    use std::sync::Arc;
    use td_mta::{
        admission::{timers::NetworkLimits, DiskLimits, ViewMode, WorkLimits},
        http_request::Framing,
        https_upload::Backend,
        ports::{Clock, Deadline, Error},
        store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    };
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let resources = td_mta::limits::Limits::default().plan().unwrap();
    let disk = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&disk).unwrap();
    let clock = Arc::new(td_mta::clock::RuntimeClock::new());
    let milliseconds = std::cell::Cell::new(10_000);
    let until = || Deadline::after(clock.sample().unwrap().monotonic, milliseconds.get()).unwrap();
    let directory = fixture.root.join("http-read-responses");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let mut root = PrivateRoot::open(directory.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let spool = RequestSpool::open(&mut root, clock.clone(), until()).unwrap();
    let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
    let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
    let responses =
        ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
    let slots = td_mta::postgres_read::ReadSlots::new(2).unwrap();
    let prepare_session = if drafts {
        td_mta::jmap_session::Session::mail_submission
    } else {
        td_mta::jmap_session::Session::mail_access
    };
    let session = prepare_session(
        td_mta::config::endpoint::Origin::parse("https://localhost").unwrap(),
        td_mta::config::identities::AccountView {
            id: binding.account(),
            username: "reader",
            name: "Reader",
        },
        &resources,
        td_mta::jmap_session::Concurrency {
            uploads: 1,
            requests: 1,
        },
    )
    .unwrap();
    let pending = td_mta::postgres_http_updates::Pending::default();
    let service = td_mta::postgres_http_api::Service {
        loaded: &loaded,
        executable: std::path::Path::new(env!("CARGO_BIN_EXE_td-mta")),
        binding,
        slots: &slots,
        pending: &pending,
        responses: &responses,
        resources: &resources,
        session: &session,
        response_maximum: disk.disk().response_bytes,
        admission: &disk,
        clock: &clock,
        timeouts: &timeouts,
    };
    let mut id = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(binding.account(), &mut id).unwrap();
    let body = format!(
        r##"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":[["Mailbox/get",{{"accountId":"{account}","properties":["name"]}},"m"],["Email/query",{{"accountId":"{account}","calculateTotal":true}},"q"],["Email/get",{{"accountId":"{account}","#ids":{{"resultOf":"q","name":"Email/query","path":"/ids"}},"properties":["id","blobId","threadId","subject","textBody","bodyValues"],"fetchTextBodyValues":true}},"g"]]}}"##
    );
    td_json::parse(&body).unwrap();
    let control = td_mta::smtp_receiving::Control::default();
    let api_worker = std::cell::RefCell::new(service.api_worker());
    let inspect_retirement = std::sync::atomic::AtomicBool::new(false);
    let call = |body: &str, password: &[u8], device, chunked, fail_write, expired| {
        let credentials = credentials(password);
        let input = if chunked {
            format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len()).into_bytes()
        } else {
            body.as_bytes().to_vec()
        };
        let wire = Wire {
            input,
            position: 0,
            output: Vec::new(),
            fail_write,
        };
        let inspect =
            || {
                if inspect_retirement.swap(false, std::sync::atomic::Ordering::AcqRel) {
                    let until = Instant::now() + Duration::from_secs(2);
                    while sql(
                        fixture,
                        "SELECT count(*) FROM pg_stat_activity WHERE usename='mail_runtime'",
                    ) != "0"
                    {
                        assert!(Instant::now() < until,
                        "exhausted reader must close during HTTP output, not after the exchange");
                        thread::sleep(Duration::from_millis(5));
                    }
                    let open = || {
                        td_mta::postgres_read::Snapshot::open_resolved(
                            &slots,
                            &loaded,
                            service.executable,
                            deadline(),
                            binding,
                            password,
                        )
                        .unwrap()
                    };
                    let first = open();
                    let second = open();
                    drop((first, second));
                }
            };
        let mut wire = http_fixture::Observed {
            wire,
            before_write: &inspect,
        };
        let request = td_mta::https_api::Request {
            keep_alive: false,
            device,
            credentials: &credentials,
            framing: if chunked {
                Framing::Chunked
            } else {
                Framing::Length(body.len() as u64)
            },
            prefix: &[],
            expect_continue: chunked,
            deadline: if expired {
                Deadline::after(td_mta::ports::Tick(0), 0).unwrap()
            } else {
                until()
            },
        };
        let mut worker = api_worker.borrow_mut();
        if worker.is_none() {
            *worker = service.api_worker();
        }
        let result = match worker.as_mut() {
            Some(worker) => worker.api(request, &mut wire, &control),
            None => service.api(request, &mut wire, &control),
        };
        assert_eq!(responses.used().unwrap(), 0);
        assert!(!responses.admission_stopped());
        (result, String::from_utf8(wire.wire.output).unwrap())
    };
    let rights_body = td_json::json!({"using":["urn:ietf:params:jmap:mail"], "methodCalls":[["Mailbox/get",{"accountId":account,"properties":["myRights"]},"rights"]]}).to_string();
    let (result, wire) = call(&rights_body, password, device, false, false, false);
    result.unwrap();
    let retained_pid = || {
        sql(
            fixture,
            "SELECT pid FROM pg_stat_activity WHERE usename='mail_runtime' AND state='idle'",
        )
        .parse::<u32>()
        .unwrap()
    };
    let first_pid = retained_pid();
    call(&rights_body, password, device, false, false, false)
        .0
        .unwrap();
    assert_eq!(
        retained_pid(),
        first_pid,
        "read job must reuse a clean idle connection"
    );
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT pg_terminate_backend({first_pid},1000)")
        ),
        "t"
    );
    call(&rights_body, password, device, false, false, false)
        .0
        .unwrap();
    assert_ne!(
        retained_pid(),
        first_pid,
        "idle disconnect must reconnect before submitting a job"
    );
    let second_pid = retained_pid();
    for _ in 1..7 {
        call(&rights_body, password, device, false, false, false)
            .0
            .unwrap();
        assert_eq!(retained_pid(), second_pid);
    }
    inspect_retirement.store(true, std::sync::atomic::Ordering::Release);
    call(&rights_body, password, device, false, false, false)
        .0
        .unwrap();
    assert!(
        !inspect_retirement.load(std::sync::atomic::Ordering::Acquire),
        "response output must inspect cleanup while the API worker is occupied"
    );
    call(&rights_body, password, device, false, false, false)
        .0
        .unwrap();
    assert_ne!(
        retained_pid(),
        second_pid,
        "eight jobs must exhaust the retained read session"
    );
    let fresh = td_mta::postgres_devices::prepare_creation(
        binding.epoch(),
        binding.account(),
        "cached read revocation",
        1,
    )
    .unwrap();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    td_mta::postgres_devices::publish_creation(&fixture.request(), &trust, deadline(), &fresh)
        .unwrap();
    call(
        &rights_body,
        fresh.password(),
        fresh.device_id(),
        false,
        false,
        false,
    )
    .0
    .unwrap();
    let revocation = td_mta::postgres_devices::prepare_revocation(
        binding.epoch(),
        binding.account(),
        fresh.device_id(),
        2,
    )
    .unwrap();
    td_mta::postgres_devices::publish_revocation(
        &fixture.request(),
        &trust,
        deadline(),
        &revocation,
    )
    .unwrap();
    assert!(matches!(
        call(
            &rights_body,
            fresh.password(),
            fresh.device_id(),
            false,
            false,
            false
        )
        .0,
        Err(td_mta::https_download::Failure::Before(Error::Forbidden))
    ));
    call(&rights_body, password, device, false, false, false)
        .0
        .unwrap();
    let rights = td_json::parse(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    let rights = &rights["methodResponses"][0][1]["list"][0]["myRights"];
    assert!(rights["maySetSeen"].is_true());
    assert!(rights["maySetKeywords"].is_true());
    assert!(rights["mayAddItems"].is_true());
    assert!(rights["mayRemoveItems"].is_true());
    for chunked in [false, true] {
        let (result, wire) = call(&body, password, device, chunked, false, false);
        result.unwrap();
        let wire = wire
            .strip_prefix("HTTP/1.1 100 Continue\r\n\r\n")
            .unwrap_or(&wire);
        let (headers, json) = wire.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
        assert!(headers.contains(&format!("Content-Length: {}\r\n", json.len())));
        assert!(json.contains("\"total\":2"));
        assert_eq!(
            json.matches("\"subject\":\"Snapshot\"").count(),
            2,
            "{json}"
        );
        assert!(json.contains("snapshot text"));
    }
    let (_, wire) = call(&body, password, device, false, false, false);
    let response = td_json::parse(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    let emails = response["methodResponses"][2][1]["list"]
        .as_array()
        .unwrap();
    let thread_id = emails[0]["threadId"].as_str().unwrap();
    let thread_body = format!(
        r##"{{"using":["urn:ietf:params:jmap:mail"],"methodCalls":[["Thread/get",{{"accountId":"{account}","ids":["{thread_id}","{thread_id}"]}},"t"],["Thread/get",{{"accountId":"{account}","#ids":{{"resultOf":"t","name":"Thread/get","path":"/list/*/id"}},"properties":["id"]}},"p"],["Thread/get",{{"accountId":"{account}","ids":null}},"n"],["Thread/get",{{"accountId":"{account}","ids":[]}},"e"],["EmailSubmission/get",{{"accountId":"{account}"}},"u"]]}}"##
    );
    let (result, wire) = call(&thread_body, password, device, false, false, false);
    result.unwrap();
    let response = td_json::parse(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    let replies = response["methodResponses"].as_array().unwrap();
    assert_eq!(replies[0][0].as_str(), Some("Thread/get"), "{response}");
    assert_eq!(replies[0][1]["list"].as_array().unwrap().len(), 1);
    assert!(replies[0][1]["list"][0]["emailIds"]
        .as_array()
        .unwrap()
        .contains(&emails[0]["id"]));
    assert_eq!(replies[1][1]["list"][0]["id"].as_str(), Some(thread_id));
    assert!(replies[1][1]["list"][0]["emailIds"].is_null());
    assert_eq!(replies[2][1]["type"].as_str(), Some("invalidArguments"));
    assert_eq!(replies[3][1]["list"].as_array().unwrap().len(), 0);
    assert_eq!(replies[4][1]["type"].as_str(), Some("unknownMethod"));
    let current = td_mta::sync::DataState::decode(
        response["methodResponses"][0][1]["state"].as_str().unwrap(),
    )
    .unwrap();
    let mut states = Vec::new();
    for kind in [
        td_mta::sync::DataType::Mailbox,
        td_mta::sync::DataType::Email,
        td_mta::sync::DataType::Thread,
    ] {
        let state = td_mta::sync::DataState {
            kind,
            sequence: td_mta::format::Sequence::from_u64(0),
            ..current
        };
        let mut encoded = [0; td_mta::sync::DataState::WIRE_BYTES];
        states.push(state.encode(&mut encoded).unwrap().to_owned());
    }
    let changes_body = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"], "methodCalls":[
        ["Mailbox/changes", {"accountId":account,"sinceState":states[0]},"m"],
        ["Email/changes", {"accountId":account,"sinceState":states[1]},"e"],
        ["Thread/changes", {"accountId":account,"sinceState":states[2]},"t"],
        ["Email/changes", {"accountId":account,"sinceState":states[0]},"wrong"],
        ["Core/echo", {"after":true},"echo"]
    ]}).to_string();
    let (result, wire) = call(&changes_body, password, device, false, false, false);
    result.unwrap();
    let changes = td_json::parse(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    let replies = changes["methodResponses"].as_array().unwrap();
    for (i, name) in ["Mailbox/changes", "Email/changes", "Thread/changes"]
        .iter()
        .enumerate()
    {
        assert_eq!(replies[i][0].as_str(), Some(*name), "{changes}");
        assert_eq!(replies[i][1]["oldState"].as_str(), Some(states[i].as_str()));
        assert!(!replies[i][1]["hasMoreChanges"].is_true());
        assert_eq!(replies[i][1]["destroyed"].as_array().unwrap().len(), 0);
    }
    assert_eq!(replies[1][1]["created"].as_array().unwrap().len(), 2);
    assert_eq!(replies[2][1]["created"].as_array().unwrap().len(), 2);
    assert_eq!(
        replies[3][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
    assert!(replies[4][1]["after"].is_true());
    let mut exhausted = td_mta::postgres_read::Snapshot::open_resolved(
        &slots,
        &loaded,
        service.executable,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    let since = td_mta::sync::DataState::decode(&states[1]).unwrap();
    assert!(matches!(
        exhausted.changes(td_mta::sync::DataType::Email, since, 256, &mut || Err(
            Error::Capacity
        )),
        Err(td_mta::postgres_read::Error::Changes(
            td_mta::jmap_changes::Error::Store(Error::Capacity)
        ))
    ));
    assert!(matches!(
        exhausted.check(),
        Err(td_mta::postgres_read::Error::Changes(
            td_mta::jmap_changes::Error::Store(Error::Capacity)
        ))
    ));
    drop(exhausted);

    let (result, wire) = call("{", password, device, false, false, false);
    result.unwrap();
    assert!(wire.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(wire.contains("notJSON"));
    let mut wrong = password.to_vec();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    for (secret, id) in [
        (&wrong[..], device),
        (password, td_mta::ids::DeviceId::from_bytes([0xfe; 16])),
    ] {
        let (result, wire) = call(&body, secret, id, false, false, false);
        assert!(matches!(
            result,
            Err(td_mta::https_download::Failure::Before(Error::Forbidden))
        ));
        assert!(wire.is_empty());
    }
    let (result, wire) = call(&body, password, device, false, false, true);
    assert!(matches!(
        result,
        Err(td_mta::https_download::Failure::Before(Error::Deadline))
    ));
    assert!(wire.is_empty());
    assert!(call(&body, password, device, false, true, false).0.is_err());
    call(&body, password, device, false, false, false)
        .0
        .unwrap();
    let (_, output) = call(&body, password, device, false, false, false);
    let json = td_json::parse(output.split_once("\r\n\r\n").unwrap().1).unwrap();
    let email = &json["methodResponses"][2][1]["list"][0];
    let raw = email["blobId"].as_str().unwrap();
    let part = email["textBody"][0]["blobId"].as_str().unwrap();
    let download = |blob: &str, secret: &[u8], fail_write| {
        api_worker.borrow_mut().take();
        let target = td_mta::https_download::Target::parse(&format!(
            "/jmap/download/{account}/{blob}/snapshot.txt?type=text%2Fplain"
        ))
        .unwrap();
        let credentials = credentials(secret);
        let mut wire = Wire {
            input: Vec::new(),
            position: 0,
            output: Vec::new(),
            fail_write,
        };
        let result = service.download(
            td_mta::https_download::Request {
                keep_alive: false,
                target: &target,
                device,
                credentials: &credentials,
                deadline: until(),
            },
            &mut wire,
            &control,
        );
        (result, String::from_utf8(wire.output).unwrap())
    };
    let (result, raw_wire) = download(raw, password, false);
    result.unwrap();
    let (headers, message) = raw_wire.split_once("\r\n\r\n").unwrap();
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.contains(&format!("Content-Length: {}\r\n", message.len())));
    assert!(
        message.contains("Subject:\r\n =?UTF-8?B?U25hcHNob3Q=?=\r\n"),
        "{message}"
    );
    let (result, part_wire) = download(part, password, false);
    result.unwrap();
    assert_eq!(part_wire.split_once("\r\n\r\n").unwrap().1, "snapshot text");
    let mut locator = td_mta::wire::PartLocator::decode(part).unwrap();
    locator.offset += 1;
    let mut locator_bytes = [0; 255];
    let forged = locator.encode(&mut locator_bytes).unwrap();
    let (result, wire) = download(forged, password, false);
    assert!(matches!(
        result,
        Err(td_mta::https_download::Failure::Before(Error::NotFound))
    ));
    assert!(wire.is_empty());
    let (result, wire) = download(raw, &wrong, false);
    assert!(matches!(
        result,
        Err(td_mta::https_download::Failure::Before(Error::Forbidden))
    ));
    assert!(wire.is_empty());
    assert!(download(raw, password, true).0.is_err());
    download(raw, password, false).0.unwrap();
    // One held read leaves only one slot: refuse before opening either of the
    // download's two connections, then release its partial reservation.
    let held = td_mta::postgres_read::Snapshot::open_resolved(
        &slots,
        &loaded,
        service.executable,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    let (result, wire) = download(raw, password, false);
    assert!(matches!(
        result,
        Err(td_mta::https_download::Failure::Before(Error::Busy))
    ));
    assert!(wire.is_empty());
    drop(held);
    download(raw, password, false).0.unwrap();
    if drafts {
        let mailbox = sql(
            fixture,
            "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE name='Drafts'",
        );
        let mailbox = td_mta::ids::MailboxId::parse(&mailbox).unwrap();
        let mut encoded = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let mailbox = td_mta::wire::encode_id(mailbox, &mut encoded).unwrap();
        let draft = td_json::json!({
            "mailboxIds":{mailbox:true},"keywords":{"$draft":true,"$seen":true},
            "from":[{"name":"Sender","email":"sender@example.test"}],
            "to":[{"name":null,"email":"recipient@example.test"}],
            "subject":"HTTP PostgreSQL draft", "messageId":["http-postgres@example.test"],
            "sentAt":"2026-10-13T12:00:59Z",
            "textBody":[{"partId":"text","type":"text/plain"}],
            "bodyValues":{"text":{"value":"HTTP draft body","isEncodingProblem":false,"isTruncated":false}}
        });
        let body = td_json::json!({"using":["urn:ietf:params:jmap:mail"], "methodCalls":[["Email/set", {"accountId":account,"create":{"draft":draft.clone()}},"c"]]}).to_string();
        let (result, output) = call(&body, password, device, false, false, false);
        result.unwrap();
        let created = td_json::parse(output.split_once("\r\n\r\n").unwrap().1).unwrap();
        let result = &created["methodResponses"][0][1];
        let email = result["created"]["draft"]["id"].as_str().unwrap();
        let blob = result["created"]["draft"]["blobId"].as_str().unwrap();
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "3");
        let stale = td_json::json!({"using":["urn:ietf:params:jmap:mail"], "methodCalls":[["Email/set", {"accountId":account,"ifInState":result["oldState"].clone(),"create":{"draft":draft.clone()}},"c"]]}).to_string();
        let (result, output) = call(&stale, password, device, false, false, false);
        result.unwrap();
        assert!(output.contains("stateMismatch"), "{output}");
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "3");
        let get = td_json::json!({"using":["urn:ietf:params:jmap:mail"], "methodCalls":[["Email/get", {"accountId":account,"ids":[email],"properties":["subject","bodyValues","textBody"],"fetchTextBodyValues":true},"g"]]}).to_string();
        let (result, output) = call(&get, password, device, false, false, false);
        result.unwrap();
        assert!(output.contains("HTTP draft body"), "{output}");
        let (result, output) = download(blob, password, false);
        result.unwrap();
        assert!(output.contains("http-postgres@example.test"), "{output}");
        sql(fixture, "CREATE FUNCTION public.delay_http_draft() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_http_draft AFTER INSERT ON td_mta.draft_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_http_draft()");
        milliseconds.set(2_000);
        let _ = call(&body, password, device, false, false, false);
        let operation = pending.drafts.operation().unwrap().unwrap();
        milliseconds.set(10_000);
        thread::sleep(Duration::from_secs(5));
        sql(fixture, "DROP TRIGGER delay_http_draft ON td_mta.draft_receipts; DROP FUNCTION public.delay_http_draft()");
        let settlement = td_mta::postgres_read::ReadSlots::new(1).unwrap();
        let settled = pending
            .drafts
            .settle(&loaded, service.executable, &settlement, &clock, until())
            .unwrap()
            .unwrap();
        assert_eq!(settled.operation, operation);
        assert_eq!(pending.drafts.operation().unwrap(), None);
        assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "4");
        let catalog = binding.identity_catalog().unwrap();
        let identity_id = catalog.ids().next().unwrap();
        let mut identity_bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
        let identity = td_mta::wire::encode_id(identity_id, &mut identity_bytes).unwrap();
        let mut send_draft = draft;
        let _ = send_draft.insert(
            "from",
            td_json::json!([{"email":binding.identity_email(identity_id).unwrap()}]),
        );
        let send = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail","urn:ietf:params:jmap:submission"],"methodCalls":[
            ["Email/set",{"accountId":account,"create":{"draft":send_draft}},"0"],
            ["EmailSubmission/set",{"accountId":account,"create":{"submission":{"emailId":"#draft","identityId":identity}},"onSuccessUpdateEmail":{"#submission":{"keywords/$draft":null}}},"1"]
        ]}).to_string();
        let before = sql(fixture, "SELECT count(*) FROM td_mta.submissions")
            .parse::<usize>()
            .unwrap();
        let (result, output) = call(&send, password, device, true, false, false);
        result.unwrap();
        let output = output
            .strip_prefix("HTTP/1.1 100 Continue\r\n\r\n")
            .unwrap();
        let output = td_json::parse(output.split_once("\r\n\r\n").unwrap().1).unwrap();
        let replies = output["methodResponses"].as_array().unwrap();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0][0].as_str(), Some("Email/set"));
        assert_eq!(replies[1][0].as_str(), Some("EmailSubmission/set"));
        assert_eq!(replies[2][0].as_str(), Some("Email/set"));
        assert_eq!(replies[1][2], replies[2][2]);
        let email = replies[0][1]["created"]["draft"]["id"].as_str().unwrap();
        assert!(replies[2][1]["updated"].get(email).unwrap().is_null());
        let submitted = replies[1][1]["created"]["submission"]["id"]
            .as_str()
            .unwrap();
        let read = td_json::json!({"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],"methodCalls":[
            ["Identity/get",{"accountId":account},"i"],
            ["EmailSubmission/get",{"accountId":account,"ids":[submitted],"properties":["id","emailId"]},"s"]
        ]}).to_string();
        let (result, output) = call(&read, password, device, false, false, false);
        result.unwrap();
        let output = td_json::parse(output.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            output["methodResponses"][0][0].as_str(),
            Some("Identity/get")
        );
        assert_eq!(
            output["methodResponses"][1][1]["list"][0]["emailId"].as_str(),
            Some(email)
        );
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.submissions")
                .parse::<usize>()
                .unwrap(),
            before + 1
        );
        // A lost final HTTP response must not erase accepted responsibility.
        let (result, _) = call(&send, password, device, false, true, false);
        assert!(result.is_err());
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.submissions")
                .parse::<usize>()
                .unwrap(),
            before + 2
        );
        let upload = |body: &[u8], framing, expect_continue, secret: &[u8]| {
            api_worker.borrow_mut().take();
            let credentials = credentials(secret);
            let mut wire = Wire {
                input: body.to_vec(),
                position: 0,
                output: Vec::new(),
                fail_write: false,
            };
            let until = until();
            let result = service.receive(
                td_mta::https_upload::Request {
                    account: binding.account(),
                    device,
                    credentials: &credentials,
                    framing,
                    content_type: "text/plain",
                    prefix: &[],
                    deadline: until,
                    body_deadline: until,
                    expect_continue,
                },
                &mut wire,
                &control,
            );
            (result, wire.output)
        };
        let uploads_before = sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts")
            .parse::<usize>()
            .unwrap();
        for (bytes, framing, interim) in [
            (b"uploaded body".as_slice(), Framing::Length(13), false),
            (
                b"d\r\nuploaded body\r\n0\r\n\r\n".as_slice(),
                Framing::Chunked,
                true,
            ),
        ] {
            let (result, wire) = upload(bytes, framing, interim, password);
            let response = result.unwrap();
            assert!(!response.stop);
            let response = td_json::parse_slice(response.bytes().unwrap()).unwrap();
            assert_eq!(response["size"].as_u64(), Some(13));
            assert_eq!(response["type"].as_str(), Some("text/plain"));
            if interim {
                assert_eq!(wire, b"HTTP/1.1 100 Continue\r\n\r\n");
            } else {
                assert!(wire.is_empty());
            }
            let (result, wire) = download(response["blobId"].as_str().unwrap(), password, false);
            result.unwrap();
            assert_eq!(wire.split_once("\r\n\r\n").unwrap().1, "uploaded body");
        }
        assert_eq!(pending.uploads.operation().unwrap(), None);
        for (bytes, framing, expected) in [
            (b"short".as_slice(), Framing::Length(6), Error::Invalid),
            (b"z\r\n".as_slice(), Framing::Chunked, Error::Invalid),
            (
                b"".as_slice(),
                Framing::Length(33 * 1024 * 1024),
                Error::Capacity,
            ),
        ] {
            let (result, _) = upload(bytes, framing, false, password);
            assert!(matches!(result, Err(error) if error == expected));
        }
        assert!(matches!(
            upload(b"x", Framing::Length(1), false, &wrong).0,
            Err(Error::Forbidden)
        ));
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts")
                .parse::<usize>()
                .unwrap(),
            uploads_before + 2
        );
        sql(fixture, "CREATE FUNCTION public.delay_http_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_http_upload AFTER INSERT ON td_mta.upload_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_http_upload()");
        milliseconds.set(3000);
        let (result, _) = upload(b"uncertain", Framing::Length(9), false, password);
        let response = result.unwrap();
        assert_eq!(response.bytes(), Err(Error::Conflict));
        assert!(!response.stop);
        let operation = pending.uploads.operation().unwrap().unwrap();
        milliseconds.set(10_000);
        let (result, wire) = upload(b"x", Framing::Length(1), true, password);
        assert!(matches!(result, Err(Error::Busy)));
        assert!(wire.is_empty());
        thread::sleep(Duration::from_secs(5));
        sql(
            fixture,
            "DROP TRIGGER delay_http_upload ON td_mta.upload_receipts",
        );
        let settled = pending
            .uploads
            .settle(&loaded, service.executable, &settlement, &clock, until())
            .unwrap()
            .unwrap();
        assert_eq!(settled.operation, operation);
        assert_eq!(settled.outcome.length, 9);
        assert_eq!(pending.uploads.operation().unwrap(), None);
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts")
                .parse::<usize>()
                .unwrap(),
            uploads_before + 3
        );
        assert_eq!(pending.submissions.operation().unwrap(), None);
        assert_eq!(pending.drafts.operation().unwrap(), None);
        assert_eq!(pending.operation().unwrap(), None);
        assert_eq!(responses.used().unwrap(), 0);
    }
}

fn exercise_resolved_read(
    fixture: &Fixture,
    binding: &td_mta::postgres_runtime_profile::Binding,
    password: &[u8],
) {
    use td_mta::postgres_read::{Error, ReadSlots, Snapshot};
    let loaded = |user| td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://{user}:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let loaded_runtime = loaded("mail_runtime");
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let missing = fixture.root.join("missing-td-mta");
    let slots = ReadSlots::new(1).unwrap();
    let mut view = Snapshot::open_resolved(
        &slots,
        &loaded_runtime,
        executable,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    assert_eq!(view.identity().account, binding.account());
    assert!(!view.mailbox_listing().unwrap().is_empty());
    // Capacity refusal must happen before trying to launch this missing helper.
    assert_eq!(
        Snapshot::open_resolved(
            &slots,
            &loaded_runtime,
            &missing,
            deadline(),
            binding,
            password
        )
        .err(),
        Some(Error::Busy)
    );
    drop(view);
    assert_eq!(
        Snapshot::open_resolved(
            &slots,
            &loaded_runtime,
            &missing,
            deadline(),
            binding,
            password
        )
        .err(),
        Some(Error::Resolution(td_mta::postgres_resolver::Error::Spawn))
    );
    assert_eq!(
        Snapshot::open_resolved(
            &slots,
            &loaded_runtime,
            executable,
            Instant::now(),
            binding,
            password
        )
        .err(),
        Some(Error::Connection(td_mta::postgres_probe::Error::Deadline))
    );
    assert_eq!(
        Snapshot::open_resolved(
            &slots,
            &loaded_runtime,
            &missing,
            deadline(),
            binding,
            b"bad"
        )
        .err(),
        Some(Error::Unauthenticated)
    );
    let mut wrong = password.to_vec();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    assert_eq!(
        Snapshot::open_resolved(
            &slots,
            &loaded_runtime,
            executable,
            deadline(),
            binding,
            &wrong
        )
        .err(),
        Some(Error::Unauthenticated)
    );
    let owner = loaded("td_probe");
    assert_eq!(
        Snapshot::open_resolved(&slots, &owner, executable, deadline(), binding, password).err(),
        Some(Error::Connection(
            td_mta::postgres_probe::Error::RuntimeRole
        ))
    );
    let replacement = Snapshot::open_resolved(
        &slots,
        &loaded_runtime,
        executable,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    drop(replacement);
}

#[path = "postgres_probe/metadata_prefetch.rs"]
mod metadata_prefetch;

fn exercise_read(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    http_drafts: bool,
) {
    use td_mta::{
        compose_text::{Address, Message},
        ids::{EmailId, MailboxId},
        ports::BlobReader,
        postgres_body, postgres_devices as devices, postgres_drafts as drafts,
        postgres_read as read,
    };
    let account = binding.account();
    let creation = devices::prepare_creation(binding.epoch(), account, "read fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    // Healthy one-shot operations and dropped read snapshots must send
    // Terminate rather than reset sockets with open transactions.
    let log_offset = fs::metadata(fixture.root.join("server.log")).unwrap().len() as usize;
    let shutdown_slots = read::ReadSlots::new(1).unwrap();
    for _ in 0..4 {
        probe(request, trust, deadline()).unwrap();
        let snapshot = read::Snapshot::open_runtime(
            &shutdown_slots,
            request,
            trust,
            deadline(),
            binding,
            password,
        )
        .unwrap();
        drop(snapshot);
    }
    let closed_by = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'");
        if remaining.trim() == "0" {
            break;
        }
        assert!(
            Instant::now() < closed_by,
            "database sessions remain: {remaining}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let log = fs::read_to_string(fixture.root.join("server.log")).unwrap();
    let teardown = &log[log_offset..];
    assert!(!teardown.contains("Connection reset by peer"), "{teardown}");
    assert!(!teardown.contains("unexpected EOF"), "{teardown}");
    exercise_http_auth(fixture, binding, password, creation.device_id());
    exercise_resolved_read(fixture, binding, password);
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
    exercise_http_read(
        fixture,
        binding,
        password,
        creation.device_id(),
        http_drafts,
    );
    if http_drafts {
        return;
    }
    // Pin equal dates so both sort directions must use ascending-ID ties.
    sql(
        fixture,
        &format!("UPDATE td_mta.emails SET received_at=0 WHERE account=decode('{account}','hex')"),
    );
    metadata_prefetch::exercise(
        fixture,
        request,
        trust,
        binding,
        password,
        [first.email, second.email],
    );
    let slots = read::ReadSlots::new(1).unwrap();
    let mut owned =
        read::Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password)
            .unwrap();
    exercise_read_dispatch(
        fixture,
        &mut owned,
        mailbox,
        [first.email, second.email],
        false,
    );
    assert_eq!(owned.identity().committed_sequence.number(), 3);
    assert!(owned.email(first.email).unwrap().is_some());
    assert_eq!(
        read::Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password).err(),
        Some(read::Error::Busy)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'"), "1");
    drop(owned);
    let mut blocked =
        read::Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password)
            .unwrap();
    exercise_read_dispatch(
        fixture,
        &mut blocked,
        mailbox,
        [first.email, second.email],
        true,
    );
    drop(blocked);
    let closed = deadline();
    while sql(
        fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(
            Instant::now() < closed,
            "owned snapshot connection survived drop"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let mut wrong = password.to_vec();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    assert_eq!(
        read::Snapshot::open_runtime(&slots, request, trust, deadline(), binding, &wrong).err(),
        Some(read::Error::Unauthenticated)
    );
    // Authentication failure after connection setup must return its slot too.
    let replacement =
        read::Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password)
            .unwrap();
    drop(replacement);
    assert_eq!(
        read::Snapshot::open_runtime(
            &slots,
            &fixture.request(),
            trust,
            deadline(),
            binding,
            password
        )
        .err(),
        Some(read::Error::Invalid)
    );
    let snapshot = |callback: fn(&mut read::Snapshot<'_>) -> Result<(), read::Error>| {
        read::with_snapshot(request, trust, deadline(), binding, password, callback)
    };
    assert!(matches!(
        read::with_snapshot(request, trust, deadline(), binding, b"bad", |_| Ok(())),
        Err(read::Error::Unauthenticated)
    ));
    let stored = |view: &mut read::Snapshot<'_>, email| -> Result<Option<String>, read::Error> {
        let mut output = Vec::new();
        let found = td_mta::email_response::write_stored(
            view,
            email,
            td_mta::email_response::Stored::all(),
            &mut [],
            |chunk| {
                assert!(chunk.len() <= td_mta::email_response::MAX_CHUNK_BYTES);
                assert!(output.len() + chunk.len() <= 8192);
                output.extend_from_slice(chunk);
                Ok(())
            },
        )?;
        if found {
            Ok(Some(String::from_utf8(output).unwrap()))
        } else {
            assert!(output.is_empty());
            Ok(None)
        }
    };
    let check_mailbox = |view: &mut read::Snapshot<'_>, expected: u64| -> Result<(), read::Error> {
        use td_mta::mailbox_response::{self, Object, Projection, Rights};
        let listing = view.mailbox_listing()?;
        let row = listing.iter().find(|row| row.id == mailbox).unwrap();
        assert_eq!(row.counts.identity, view.identity());
        assert_eq!(row.counts.total_emails, expected);
        assert_eq!(row.counts.unread_emails, 0);
        let rights = Rights {
            may_read_items: true,
            may_add_items: true,
            may_remove_items: true,
            may_set_seen: true,
            may_set_keywords: true,
            may_create_child: false,
            may_rename: false,
            may_delete: false,
            may_submit: false,
        };
        let mut bytes = [0; mailbox_response::MAX_OBJECT_BYTES];
        let object = Object {
            identity: view.identity(),
            id: row.id,
            row: row.row(),
            counts: Some(&row.counts),
        };
        let length =
            mailbox_response::encode_object(object, Projection::all(), rights, &mut bytes).unwrap();
        let text = std::str::from_utf8(&bytes[..length]).unwrap();
        assert!(text.contains("\"name\":\"Drafts\""));
        assert!(text.contains(&format!("\"totalEmails\":{expected},")));
        assert!(text.contains("\"unreadEmails\":0,"));
        assert!(text.contains("\"maySubmit\":false"));
        // Passive counts from another snapshot or mailbox must not mix.
        for mismatch in [
            td_mta::mailbox_response::MailboxCounts {
                mailbox: MailboxId::from_bytes([0xee; 16]),
                ..row.counts
            },
            td_mta::mailbox_response::MailboxCounts {
                identity: td_mta::ports::ViewIdentity {
                    committed_sequence: td_mta::format::Sequence::from_u64(999),
                    ..view.identity()
                },
                ..row.counts
            },
        ] {
            let object = Object {
                identity: view.identity(),
                id: row.id,
                row: row.row(),
                counts: Some(&mismatch),
            };
            assert_eq!(
                mailbox_response::encode_object(object, Projection::all(), rights, &mut bytes),
                Err(td_mta::ports::Error::Invalid)
            );
        }
        view.check()
    };
    read::with_snapshot(request,trust,deadline(),binding,password,|view| {
        assert_eq!(view.identity().committed_sequence.number(),3);
        view.prefetch_metadata(&[first.email, second.email])?;
        view.prefetch_values(&[first.email, second.email], true, true)?;
        check_mailbox(view, 2)?;
        let original = stored(view, first.email)?.unwrap();
        assert!(original.contains("\"keywords\":{\"$draft\":true,\"$seen\":true}"));
        assert!(original.contains("\"mailboxIds\":{"));
        assert!(original.contains("\"blobId\":"));
        assert!(original.contains("\"threadId\":"));
        assert!(original.contains("\"receivedAt\":"));

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
        let bytes=view.with_pinned_body(first.email,|metadata,body| {
            use td_mta::{admission::work::{Charge, Meter}, ports::{Deadline, Tick}, root_headers};
            assert_eq!(body.id(), metadata.blob);
            assert_eq!(body.digest(), &metadata.body.digest);
            assert!(!body.is_failed());
            body.check_deadline()?;
            let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge {
                io_bytes: 65536, records: 65536, output_bytes: 65536, unlinks: 0,
            });
            let mut headers = vec![0; 65536];
            let mut reader = root_headers::Cursor::new(body, 65534, &mut headers, &mut work, Tick(1)).unwrap();
            while reader.poll(Tick(1)).unwrap() != root_headers::Status::Complete {}
            let headers = reader.finish(Tick(1)).unwrap();
            assert!(std::str::from_utf8(headers.bytes).unwrap().contains("Content-Transfer-Encoding: base64"));
            use td_mta::jmap_request::dispatch::{self, EmailOutput, EmailProjection};
            struct Clock;
            impl td_mta::ports::Clock for Clock {
                fn sample(&self) -> Result<td_mta::ports::Time, td_mta::ports::Error> {
                    Ok(td_mta::ports::Time { utc_ms: 0, monotonic: Tick(1) })
                }
            }
            struct Output { bytes: Vec<u8>, refuse: bool }
            impl EmailOutput for Output {
                fn check(&mut self) -> Result<(), dispatch::Error> { Ok(()) }
                fn append(&mut self, bytes: &[u8]) -> Result<(), dispatch::Error> {
                    if self.refuse || self.bytes.len() + bytes.len() > 32768 {
                        return Err(dispatch::Error::ResponseCapacity);
                    }
                    self.bytes.extend_from_slice(bytes);
                    Ok(())
                }
            }
            let mut selected = EmailProjection::new();
            for field in ["from", "subject", "textBody", "htmlBody", "attachments", "preview", "bodyValues"] {
                selected.include(field, &mut work, &Clock).unwrap();
            }
            let options = td_mta::body_value_options::Options { fetch_text: true, ..Default::default() };
            let mut output = Output { bytes: Vec::new(), refuse: false };
            // A later ingress policy must not hide an already stored body.
            let read_limits = td_mta::limits::Limits { message_bytes: body.len() as usize - 1, header_bytes: body.len() as usize - 1, ..Default::default() };
            read_limits.plan().unwrap();
            assert!(body.len() > read_limits.message_bytes as u64);
            dispatch::write_verified_email(&mut output, body, &read_limits, (&selected, options), &Clock, &mut work).unwrap();
            let text = std::str::from_utf8(&output.bytes).unwrap();
            assert!(text.contains("\"subject\":\"Snapshot\""));
            assert!(text.contains("sender@example.test"));
            assert!(text.contains("\"preview\":\"snapshot text\""));
            assert!(text.contains("\"value\":\"snapshot text\""));
            assert!(text.contains("\"attachments\":[]"));
            let body_list = |name: &str| text.split_once(name).unwrap().1.split_once(']').unwrap().0;
            let plain = body_list("\"textBody\":[");
            assert!(plain.contains("\"type\":\"text/plain\""));
            assert_eq!(body_list("\"htmlBody\":["), plain);
            assert!(text.contains("\"blobId\":"));
            output.refuse = true;
            assert_eq!(dispatch::write_verified_email(&mut output, body, &td_mta::limits::Limits::default(), (&selected, options), &Clock, &mut work), Err(dispatch::Error::ResponseCapacity));
            assert_eq!(metadata.identity.committed_sequence.number(),3);
            let mut bytes=vec![0;metadata.body.length as usize];
            let mut position=0;
            while position<bytes.len() { let n=body.read_at(position as u64,&mut bytes[position..])?;assert!(n>0);position+=n; }
            Ok(bytes)
        })?;
        assert!(String::from_utf8(bytes).unwrap().ends_with("c25hcHNob3QgdGV4dA==\r\n"));
        assert!(view.email(second.email)?.is_some());
        assert_eq!(stored(view, first.email)?, Some(original));
        check_mailbox(view, 2)?;
        Ok(())
    }).unwrap();
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert_eq!(view.identity().committed_sequence.number(), 4);
        check_mailbox(view, 1)?;
        assert_eq!(stored(view, first.email)?, None);
        assert_eq!(
            td_mta::email_response::write_stored(
                view,
                second.email,
                td_mta::email_response::Stored::all(),
                &mut [],
                |_| Err(td_mta::ports::Error::Capacity)
            ),
            Err(read::Error::Output(td_mta::ports::Error::Capacity))
        );
        assert_eq!(view.email(first.email)?, None);
        assert!(view.mailboxes(first.email, None, 1)?.values.is_empty());
        Ok(())
    })
    .unwrap();
    sql(fixture,&format!("INSERT INTO td_mta.keywords SELECT decode('{account}','hex'),decode('{}','hex'),'k'||lpad(n::text,3,'0'),decode('0000000000000004','hex') FROM generate_series(0,256) n",second.email));
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let encoded = stored(view, second.email)?.unwrap();
        assert!(encoded.contains("\"k000\":true"));
        assert!(encoded.contains("\"k256\":true"));
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
    for interpreted in [false, true] {
        let expires = Instant::now() + Duration::from_secs(3);
        let expired_pin = read::with_snapshot(request, trust, expires, binding, password, |view| {
            let read_pin =
                |_: postgres_body::Metadata, pin: &mut td_mta::store_fs::PinnedBlob<'_, '_>| {
                    pin.check_deadline()?;
                    thread::sleep(expires.saturating_duration_since(Instant::now()));
                    assert_eq!(
                        pin.read_at(0, &mut [0; 1]),
                        Err(td_mta::ports::Error::Deadline)
                    );
                    assert!(pin.is_failed());
                    assert_eq!(pin.check_deadline(), Err(td_mta::ports::Error::Deadline));
                    Ok(())
                };
            if interpreted {
                view.with_interpreted_body(second.email, read_pin)
            } else {
                view.with_pinned_body(second.email, read_pin)
            }
        });
        assert_eq!(
            expired_pin,
            Err(read::Error::Body(postgres_body::Error::Read(
                td_mta::ports::Error::Deadline
            )))
        );
    }
    let disconnected_pin = read::with_snapshot(
        request,
        trust,
        deadline(),
        binding,
        password,
        |view| {
            view.with_interpreted_body(second.email, |_, pin| {
            pin.check_deadline()?;
            sql(fixture,"SELECT pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND pid<>pg_backend_pid()");
            thread::sleep(Duration::from_millis(100));
            let mut provisional_reads = 0;
            let failure = (0..16).find_map(|_| match pin.read_at(0, &mut [0; 1]) {
                Ok(1) => { provisional_reads += 1; None },
                Ok(_) => panic!("cached byte missing"),
                Err(error) => Some(error),
            });
            assert!(provisional_reads > 0, "cached reads still poll the disconnected socket immediately");
            assert!(failure.is_some(), "resident pin hid a disconnected snapshot beyond its bounded checks");
            assert!(pin.is_failed());
            assert_eq!(pin.check_deadline().err(), failure);
            Ok(())
        })
        },
    );
    assert!(disconnected_pin.is_err());
    let disconnected_at_completion = read::with_snapshot(
        request,
        trust,
        deadline(),
        binding,
        password,
        |view| {
            view.with_interpreted_body(second.email, |_, pin| {
            pin.check_deadline()?;
            sql(fixture,"SELECT pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND pid<>pg_backend_pid()");
            thread::sleep(Duration::from_millis(100));
            Ok(())
        })
        },
    );
    assert!(
        matches!(
            disconnected_at_completion,
            Err(read::Error::Body(postgres_body::Error::Read(_)))
        ),
        "callback completion must always poll connection health"
    );
    sql(fixture,&format!("UPDATE td_mta.blob_chunks SET bytes=set_byte(bytes,0,0) WHERE blob_id=decode('{}','hex')",second.blob));
    let corrupt = read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let error = view
            .with_pinned_body::<()>(second.email, |_, _| -> Result<(), td_mta::ports::Error> {
                panic!("corrupt body admitted")
            })
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
    exercise_read_boundaries(fixture, request, trust, binding, second.email);
}

fn exercise_read_boundaries(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    email: td_mta::ids::EmailId,
) {
    use td_mta::{
        postgres_devices as devices,
        postgres_read::{Error, ReadSlots, Snapshot},
    };
    let creation =
        devices::prepare_creation(binding.epoch(), binding.account(), "method boundary", 1)
            .unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let slots = ReadSlots::new(1).unwrap();
    let open =
        || Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password).unwrap();
    let mut view = open();
    assert_eq!(
        view.keywords(email, Some("$draft"), 1).unwrap().values,
        ["$seen"]
    );
    let next = view.identity().committed_sequence.number() + 1;
    let between = view.finish_method().unwrap();
    assert!(matches!(
        Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password),
        Err(Error::Busy)
    ));
    let stop_at = Instant::now() + Duration::from_secs(5);
    while sql(
        fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(
            Instant::now() < stop_at,
            "closed method retains PostgreSQL connection"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let account = binding.account();
    sql(fixture, &format!("BEGIN; INSERT INTO td_mta.keywords VALUES(decode('{account}','hex'),decode('{email}','hex'),'$flagged',decode('{next:016x}','hex')); UPDATE td_mta.accounts SET sequence=decode('{next:016x}','hex'); COMMIT"));
    let mut view = between.reopen(&loaded, executable, password).unwrap();
    assert_eq!(view.identity().committed_sequence.number(), next);
    assert_eq!(
        view.keywords(email, Some("$draft"), 1).unwrap().values,
        ["$flagged"]
    );
    let between = view.finish_method().unwrap();
    let mut wrong = password.to_vec();
    let last = wrong.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    assert!(matches!(
        between.reopen(&loaded, executable, &wrong),
        Err(Error::Unauthenticated)
    ));
    let between = open().finish_method().unwrap();
    let failed = between.reopen(&loaded, &fixture.root.join("missing-helper"), password);
    assert!(matches!(failed, Err(Error::Resolution(_))));
    let view = Snapshot::open_runtime(
        &slots,
        request,
        trust,
        Instant::now() + Duration::from_secs(2),
        binding,
        password,
    )
    .unwrap();
    let between = view.finish_method().unwrap();
    thread::sleep(Duration::from_secs(2));
    assert!(matches!(
        between.reopen(&loaded, executable, password),
        Err(Error::Connection(td_mta::postgres_probe::Error::Deadline))
    ));
    let between = open().finish_method().unwrap();
    let revoke =
        devices::prepare_revocation(binding.epoch(), account, creation.device_id(), 2).unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert!(matches!(
        between.reopen(&loaded, executable, password),
        Err(Error::Unauthenticated)
    ));
    assert!(matches!(
        Snapshot::open_runtime(&slots, request, trust, deadline(), binding, password),
        Err(Error::Unauthenticated)
    ));
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
        after: None,
        before: None,
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
        let window=view.query_emails(read::EmailQuery {after:Some(-1),before:Some(5),..query})?;
        assert_eq!(window.ids,vec![first.email]);assert_eq!(window.total,Some(1));assert!(!window.has_more);
        let window=view.query_emails(read::EmailQuery {after:Some(5),before:Some(6),scope:read::EmailScope::Mailbox(mailbox),..query})?;
        assert_eq!(window.ids,vec![second.email]);assert_eq!(window.total,Some(1));
        let window=view.query_emails(read::EmailQuery {after:Some(5),before:Some(5),..query})?;
        assert!(window.ids.is_empty());assert_eq!(window.total,Some(0));assert!(!window.has_more);
        let window=view.query_emails(read::EmailQuery {after:Some(5),position:1,limit:1,..query})?;
        assert_eq!(window.ids,vec![ties[1]]);assert_eq!(window.total,Some(2));assert!(!window.has_more);
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
    use td_mta::{postgres_devices as devices, postgres_mailbox_set as folders};
    let account = binding.account();
    let device = devices::prepare_creation(binding.epoch(), account, "folder fixture", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let password = device.password();
    let prepare = |names: &[&str], expected| {
        folders::prepare(binding, device.device_id(), expected, names, deadline()).unwrap()
    };
    let prepare_delete = |mailbox, expected| {
        use td_mta::{
            admission::work::{Charge, Meter},
            clock::RuntimeClock,
            ports::{Clock, Deadline},
        };
        let clock = RuntimeClock::new();
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let mut meter = Meter::new(
            until,
            Charge {
                records: 10000,
                io_bytes: 65536,
                output_bytes: 65536,
                ..Default::default()
            },
        );
        folders::prepare_delete_with_meter(
            binding,
            device.device_id(),
            expected,
            mailbox,
            deadline(),
            &mut meter,
            &clock,
        )
        .unwrap()
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
    let target = td_mta::ids::MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE name='uncertain'",
    ))
    .unwrap();
    let deletion = prepare_delete(Some(target), None);
    assert_eq!(
        folders::publish(request, trust, deadline(), &deletion, b"bad"),
        Err(Error::Unauthenticated)
    );
    assert_eq!(
        folders::publish(
            request,
            trust,
            deadline(),
            &prepare_delete(Some(target), Some(1)),
            password
        ),
        Err(Error::State)
    );
    sql(fixture, "CREATE TRIGGER reject_folder_delete BEFORE INSERT ON td_mta.mailbox_create_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_folder()");
    assert_eq!(
        folders::publish(request, trust, deadline(), &deletion, password),
        Err(Error::Database)
    );
    assert_eq!(
        folders::inspect(request, trust, deadline(), &deletion).unwrap(),
        None
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE name='uncertain'"
        ),
        "1"
    );
    sql(
        fixture,
        "DROP TRIGGER reject_folder_delete ON td_mta.mailbox_create_receipts",
    );
    let deleted = folders::publish(request, trust, deadline(), &deletion, password).unwrap();
    assert_eq!(deleted.results, [ResultEntry::Destroyed(target)]);
    assert_eq!(deleted.new_sequence, deleted.old_sequence + 1);
    assert_eq!(
        folders::publish(request, trust, deadline(), &deletion, b"bad").unwrap(),
        deleted
    );
    assert_eq!(
        folders::inspect(request, trust, deadline(), &deletion).unwrap(),
        Some(deleted.clone())
    );
    let absent = folders::publish(
        request,
        trust,
        deadline(),
        &prepare_delete(Some(target), None),
        password,
    )
    .unwrap();
    assert_eq!(absent.results, [ResultEntry::NotFound]);
    assert_eq!(absent.old_sequence, absent.new_sequence);
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE kind=1 AND action=3 AND object=decode('{target}','hex')")), "1");
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
        folders::publish(
            request,
            trust,
            deadline(),
            &prepare_delete(None, None),
            password
        ),
        Err(Error::Unauthenticated)
    );
    assert_eq!(
        folders::publish(request, trust, deadline(), &deletion, password).unwrap(),
        deleted
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
    let clock = td_mta::clock::RuntimeClock::new();
    fn message<'a>(bytes: &'a [u8], receipt: SmtpReceipt<'a>) -> receiving::Message<'a> {
        receiving::Message {
            bytes,
            receipt,
            hostname: "mail.example.test",
            extended: true,
            header_bytes: 65536,
            received_at: 1_700_000_000_000,
        }
    }
    let capacity = td_mta::admission::work::Charge {
        io_bytes: binding.work().foreground_io_bytes,
        records: binding.work().foreground_records,
        output_bytes: 33 * 1024 * 1024,
        ..Default::default()
    };
    let cap = || {
        use td_mta::ports::Clock;
        td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 30_000).unwrap()
    };
    let prepare = |bytes: &[u8]| {
        let mut meter = td_mta::admission::work::Meter::new(cap(), capacity);
        receiving::prepare_with_meter(
            binding,
            &message(bytes, receipt),
            deadline(),
            &mut meter,
            &clock,
        )
    };
    for limited in [
        td_mta::admission::work::Charge {
            io_bytes: 0,
            ..capacity
        },
        td_mta::admission::work::Charge {
            records: 0,
            ..capacity
        },
        td_mta::admission::work::Charge {
            output_bytes: 0,
            ..capacity
        },
    ] {
        let mut meter = td_mta::admission::work::Meter::new(cap(), limited);
        for bytes in [b"Subject: refusal\r\n\r\nbody\r\n".as_slice(), b"\r\n"] {
            assert!(matches!(
                receiving::prepare_with_meter(
                    binding,
                    &message(bytes, receipt),
                    deadline(),
                    &mut meter,
                    &clock
                ),
                Err(receiving::Error::Input(td_mta::ports::Error::Capacity))
            ));
        }
        assert!(meter.stopped().is_some());
    }
    let expired = td_mta::ports::Deadline::after(td_mta::ports::Tick(0), 0).unwrap();
    let mut expired = td_mta::admission::work::Meter::new(expired, capacity);
    assert!(matches!(
        receiving::prepare_with_meter(
            binding,
            &message(b"\r\n", receipt),
            deadline(),
            &mut expired,
            &clock
        ),
        Err(receiving::Error::Input(td_mta::ports::Error::Deadline))
    ));
    let mut consumed = td_mta::admission::work::Meter::new(cap(), capacity);
    receiving::prepare_with_meter(
        binding,
        &message(b"Subject: paid\r\n\r\nbody\r\n", receipt),
        deadline(),
        &mut consumed,
        &clock,
    )
    .unwrap();
    assert!(consumed.remaining().io_bytes < capacity.io_bytes);
    assert!(consumed.remaining().output_bytes < capacity.output_bytes);
    assert!(consumed.remaining().records < capacity.records);
    receiving::prepare(binding, &message(b"\r\n", receipt), deadline()).unwrap();
    let mut bounded = td_mta::admission::work::Meter::new(cap(), capacity);
    let long_line = vec![b'x'; 1024 * 1024];
    assert!(matches!(
        receiving::prepare_with_meter(
            binding,
            &message(&long_line, receipt),
            deadline(),
            &mut bounded,
            &clock
        ),
        Err(receiving::Error::Input(td_mta::ports::Error::Invalid))
    ));
    assert_eq!(
        capacity.io_bytes - bounded.remaining().io_bytes,
        4 * td_mta::format::MAX_VALUE_BYTES as u64 + 1001
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "0"
    );
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
    let allowance = td_mta::smtp_storage::trace_allowance("mail.example.test").unwrap();
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

#[test]
#[ignore = "requires local PostgreSQL binaries"]
fn real_postgres_submission_input_freezes_verified_bytes_and_identity_envelope() {
    exercise_configuration(ConfigurationExercise::SubmissionInput);
}

fn exercise_submission_input(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    publish: bool,
) {
    use td_mta::{
        compose_text::{Address, Message},
        ids::{IdentityId, MailboxId},
        postgres_devices as devices, postgres_drafts as drafts, postgres_submission_input as input,
    };
    let creation = devices::prepare_creation(
        binding.epoch(),
        binding.account(),
        "submission input fixture",
        1,
    )
    .unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let password = creation.password();
    let mailbox = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    ))
    .unwrap();
    let identity = IdentityId::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let visible = [Address {
        name: "",
        email: "one@example.test",
    }];
    let blind = [Address {
        name: "",
        email: "hidden@example.test",
    }];
    let message = Message {
        from: Address {
            name: "Sender",
            email: "main@example.test",
        },
        to: &visible,
        cc: &[],
        bcc: &blind,
        reply_to: &[],
        subject: "outgoing",
        message_id: "outgoing@example.test",
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text: "body
text",
    };
    let draft = drafts::prepare(
        drafts::DraftRequest {
            binding,
            device: creation.device_id(),
            mailbox,
            expected: Some(1),
        },
        &message,
        deadline(),
    )
    .unwrap();
    let published = drafts::publish(request, trust, deadline(), &draft, password).unwrap();
    let prepare = |identity, expected, password| {
        input::prepare(
            request,
            trust,
            deadline(),
            input::Input {
                binding,
                email: published.email,
                identity,
                expected,
            },
            password,
        )
    };
    assert_eq!(
        prepare(identity, Some(1), password).err(),
        Some(input::Error::State)
    );
    assert_eq!(
        prepare(IdentityId::from_bytes([0xef; 16]), None, password).err(),
        Some(input::Error::Identity)
    );
    assert_eq!(
        prepare(identity, None, b"bad").err(),
        Some(input::Error::Unauthenticated)
    );
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
    };
    let clock = RuntimeClock::new();
    let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    let selection = || input::Input {
        binding,
        email: published.email,
        identity,
        expected: Some(2),
    };
    let mut exhausted = Meter::new(until, Charge::default());
    assert!(matches!(
        input::prepare_with_meter(
            request,
            trust,
            deadline(),
            selection(),
            password,
            &mut exhausted,
            &clock
        ),
        Err(input::Error::Work(td_mta::ports::Error::Capacity))
    ));
    assert!(exhausted.stopped().is_some());
    let capacity = Charge {
        io_bytes: 65536,
        records: 10000,
        output_bytes: 65536,
        ..Default::default()
    };
    let mut expired = Meter::new(
        Deadline::after(clock.sample().unwrap().monotonic, 0).unwrap(),
        capacity,
    );
    assert!(matches!(
        input::prepare_with_meter(
            request,
            trust,
            deadline(),
            selection(),
            password,
            &mut expired,
            &clock
        ),
        Err(input::Error::Work(td_mta::ports::Error::Deadline))
    ));
    let mut meter = Meter::new(until, capacity);
    meter
        .charge(
            clock.sample().unwrap().monotonic,
            Charge {
                records: 7,
                ..Default::default()
            },
        )
        .unwrap();
    let before = meter.remaining();
    assert!(matches!(
        input::prepare_with_meter(
            request,
            trust,
            Instant::now(),
            selection(),
            password,
            &mut meter,
            &clock
        ),
        Err(input::Error::Work(td_mta::ports::Error::Deadline))
    ));
    let prepared = input::prepare_with_meter(
        request,
        trust,
        deadline(),
        selection(),
        password,
        &mut meter,
        &clock,
    )
    .unwrap();
    let after = meter.remaining();
    assert!(
        after.io_bytes < before.io_bytes
            && after.records < before.records
            && after.output_bytes < before.output_bytes
    );

    assert_eq!(prepared.binding(), binding);
    assert_eq!(prepared.device(), creation.device_id());
    assert_eq!(prepared.identity(), identity);
    assert_eq!(prepared.expected(), Some(2));
    assert_eq!(prepared.source().email, published.email);
    assert_eq!(prepared.source().blob, published.blob);
    assert_eq!(prepared.reverse_path(), "main@example.test");
    assert_eq!(
        prepared.recipients().collect::<Vec<_>>(),
        ["one@example.test", "hidden@example.test"]
    );
    assert!(!String::from_utf8_lossy(prepared.bytes()).contains("hidden@example.test"));
    assert!(!format!("{prepared:?}").contains("main@example.test"));
    let original = td_mta::postgres_body::with_email(
        request,
        trust,
        deadline(),
        td_mta::postgres_body::ReadRequest {
            epoch: binding.epoch(),
            account: binding.account(),
            email: published.email,
            maximum: 32 * 1024 * 1024,
        },
        password,
        |metadata, reader| {
            let mut bytes = vec![0; metadata.body.length as usize];
            let mut offset = 0;
            while offset < bytes.len() {
                let count = reader.read_at(offset as u64, &mut bytes[offset..])?;
                assert_ne!(count, 0);
                offset += count;
            }
            Ok(bytes)
        },
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&original).contains("hidden@example.test"));
    let body_start = |bytes: &[u8]| {
        bytes
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap()
            + 4
    };
    assert_eq!(
        &original[body_start(&original)..],
        &prepared.bytes()[body_start(prepared.bytes())..]
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "0");
    assert_eq!(
        sql(
            fixture,
            "SELECT queue_bytes||','||queue_submissions FROM td_mta.account_usage"
        ),
        "0,0"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000002"
    );
    let retained = prepared.clone();
    drop(prepared);
    assert_eq!(retained.recipients().len(), 2);
    if publish {
        exercise_submission_publication(
            fixture,
            request,
            trust,
            binding,
            retained.clone(),
            password,
        );
    }
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        prepare(identity, None, password).err(),
        Some(input::Error::Read(td_mta::postgres_read::Error::Policy))
    );
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            binding.selection().generation
        ),
    );
    let revoke =
        devices::prepare_revocation(binding.epoch(), binding.account(), creation.device_id(), 1)
            .unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert_eq!(
        prepare(identity, None, password).err(),
        Some(input::Error::Read(
            td_mta::postgres_read::Error::Unauthenticated
        ))
    );
    assert!(retained
        .bytes()
        .ends_with(&original[body_start(&original)..]));
}

#[test]
#[ignore = "requires local PostgreSQL binaries"]
fn real_postgres_submissions_publish_queue_and_retained_outcomes() {
    exercise_configuration(ConfigurationExercise::Submissions);
}

fn exercise_submission_publication(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    input: td_mta::postgres_submission_input::Prepared,
    password: &[u8],
) {
    use td_mta::{
        postgres_devices as devices, postgres_submission_input as inputs,
        postgres_submissions as submissions,
    };
    let account = binding.account();
    let make = || {
        let prepared = inputs::prepare(
            request,
            trust,
            deadline(),
            inputs::Input {
                binding,
                email: input.source().email,
                identity: input.identity(),
                expected: None,
            },
            password,
        )
        .unwrap();
        submissions::prepare(prepared, deadline()).unwrap()
    };
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline},
    };
    let clock = RuntimeClock::new();
    let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
    let capacity = Charge {
        io_bytes: 65536,
        records: 10000,
        ..Default::default()
    };
    let mut exhausted = Meter::new(until, Charge::default());
    assert!(matches!(
        submissions::prepare_with_meter(input.clone(), deadline(), &mut exhausted, &clock),
        Err(submissions::Error::Work(td_mta::ports::Error::Capacity))
    ));
    assert!(exhausted.stopped().is_some());
    let mut short = Meter::new(
        until,
        Charge {
            io_bytes: input.bytes().len() as u64 - 1,
            ..capacity
        },
    );
    assert!(matches!(
        submissions::prepare_with_meter(input.clone(), deadline(), &mut short, &clock),
        Err(submissions::Error::Work(td_mta::ports::Error::Capacity))
    ));
    assert!(short.stopped().is_some());
    let mut expired = Meter::new(
        Deadline::after(clock.sample().unwrap().monotonic, 0).unwrap(),
        capacity,
    );
    assert!(matches!(
        submissions::prepare_with_meter(input.clone(), deadline(), &mut expired, &clock),
        Err(submissions::Error::Work(td_mta::ports::Error::Deadline))
    ));
    let mut meter = Meter::new(until, capacity);
    meter
        .charge(
            clock.sample().unwrap().monotonic,
            Charge {
                records: 7,
                ..Default::default()
            },
        )
        .unwrap();
    let before = meter.remaining();
    assert!(matches!(
        submissions::prepare_with_meter(input.clone(), Instant::now(), &mut meter, &clock),
        Err(submissions::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    ));
    let first =
        submissions::prepare_with_meter(input.clone(), deadline(), &mut meter, &clock).unwrap();
    let after = meter.remaining();
    assert!(after.io_bytes < before.io_bytes && after.records < before.records);
    assert_eq!(after.output_bytes, before.output_bytes);
    assert_eq!(
        submissions::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    assert_eq!(
        submissions::publish(request, trust, deadline(), &first, b"bad"),
        Err(submissions::Error::Unauthenticated)
    );
    let (a, b) = thread::scope(|scope| {
        let a = scope.spawn(|| submissions::publish(request, trust, deadline(), &first, password));
        let b = scope.spawn(|| submissions::publish(request, trust, deadline(), &first, password));
        (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
    });
    assert_eq!(a, b);
    assert_eq!(a.submission, first.id());
    assert_eq!(a.blob, first.blob());
    assert_eq!(a.email, input.source().email);
    assert_eq!((a.old_sequence, a.new_sequence), (2, 3));
    assert_eq!(a.expires_at - a.send_at, 5 * 24 * 60 * 60 * 1000);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "1");
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(address,',' ORDER BY ordinal) FROM td_mta.recipients"
        ),
        "one@example.test,hidden@example.test"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.recipients WHERE state=1 AND uncertain=0 AND attempt_id IS NULL AND attempt_count=0 AND last_attempt_at IS NULL AND phase=0 AND rcpt_reply IS NULL AND data_reply IS NULL AND reason=0 AND diagnostic=''"), "2");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.recipients r JOIN td_mta.submissions s ON s.account=r.account AND s.id=r.submission_id WHERE r.next_attempt_at=s.send_at"), "2");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.changes WHERE kind=5 AND action=1"
        ),
        "1"
    );
    assert_eq!(first.route(), binding.route());
    use td_mta::{
        generations::GenerationSet, postgres_delivery_body as delivery_body,
        postgres_runtime_profile as runtime, tls_policy::TlsPolicies,
    };
    let mut domain = GenerationSet::<TlsPolicies>::at_startup();
    let relay = runtime::load(
        request,
        trust,
        deadline(),
        binding.epoch(),
        "cloud",
        domain.reserve().unwrap(),
    )
    .unwrap();
    let (relay_binding, _relay_configuration, relay_prepared) = relay.into_parts().unwrap();
    drop(domain.publish(relay_prepared).unwrap());
    let relay_policies = domain.current().unwrap();
    assert!(relay_binding.serves_route(first.route()));
    exercise_route_owners(fixture, request, trust, &relay_binding, binding);
    assert_eq!(
        delivery_body::with_submission(
            request,
            trust,
            deadline(),
            delivery_body::ReadRequest {
                binding,
                submission: a.submission,
                maximum: a.length
            },
            |_, _| Ok(())
        ),
        Err(delivery_body::Error::Policy)
    );
    let read_outgoing = || {
        delivery_body::with_submission(
            request,
            trust,
            deadline(),
            delivery_body::ReadRequest {
                binding: &relay_binding,
                submission: a.submission,
                maximum: a.length,
            },
            |metadata, body| {
                assert_eq!(metadata.blob, a.blob);
                assert_eq!(metadata.body.length, a.length);
                let mut bytes = vec![0; a.length as usize];
                assert_eq!(body.read_at(0, &mut bytes)?, bytes.len());
                assert_eq!(bytes, input.bytes());
                Ok(())
            },
        )
    };
    read_outgoing().unwrap();
    assert_eq!(
        delivery_body::with_submission(
            request,
            trust,
            deadline(),
            delivery_body::ReadRequest {
                binding: &relay_binding,
                submission: a.submission,
                maximum: a.length - 1
            },
            |_, _| -> Result<(), td_mta::ports::Error> { panic!("oversized body admitted") }
        ),
        Err(delivery_body::Error::Capacity)
    );
    for assignment in [
        "route_name='other-route'",
        "destination=decode(repeat('00',32),'hex')",
    ] {
        sql(
            fixture,
            &format!("UPDATE td_mta.submission_routes SET {assignment}"),
        );
        assert_eq!(read_outgoing(), Err(delivery_body::Error::Missing));
        let destination: String = binding
            .route()
            .destination()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        sql(fixture, &format!("UPDATE td_mta.submission_routes SET route_name='personal-smarthost', destination=decode('{destination}','hex')"));
    }
    sql(fixture, "DELETE FROM td_mta.submission_routes");
    assert_eq!(read_outgoing(), Err(delivery_body::Error::Missing));
    let route_digest: String = binding
        .route()
        .destination()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sql(fixture, &format!("INSERT INTO td_mta.submission_routes VALUES(decode('{}','hex'),decode('{}','hex'),'desktop','personal-smarthost',decode('{route_digest}','hex'))", binding.account(), a.submission));
    delivery_body::with_submission(
        request,
        trust,
        deadline(),
        delivery_body::ReadRequest {
            binding: &relay_binding,
            submission: a.submission,
            maximum: a.length,
        },
        |_, body| {
            sql(
                fixture,
                "UPDATE td_mta.submission_routes SET route_name='other-route'",
            );
            let mut bytes = vec![0; a.length as usize];
            assert_eq!(body.read_at(0, &mut bytes)?, bytes.len());
            assert_eq!(bytes, input.bytes());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(read_outgoing(), Err(delivery_body::Error::Missing));
    sql(
        fixture,
        "UPDATE td_mta.submission_routes SET route_name='personal-smarthost'",
    );
    let original = sql(fixture, &format!("SELECT encode(bytes,'hex') FROM td_mta.blob_chunks WHERE blob_id=decode('{}','hex') AND ordinal=0", a.blob));
    sql(fixture, &format!("UPDATE td_mta.blob_chunks SET bytes=convert_to(repeat('x',octet_length(bytes)),'UTF8') WHERE blob_id=decode('{}','hex') AND ordinal=0", a.blob));
    assert!(matches!(
        delivery_body::with_submission(
            request,
            trust,
            deadline(),
            delivery_body::ReadRequest {
                binding: &relay_binding,
                submission: a.submission,
                maximum: a.length
            },
            |_, _| -> Result<(), td_mta::ports::Error> { panic!("corrupt body admitted") }
        ),
        Err(delivery_body::Error::Body(_))
    ));
    sql(fixture, &format!("UPDATE td_mta.blob_chunks SET bytes=decode('{original}','hex') WHERE blob_id=decode('{}','hex') AND ordinal=0", a.blob));
    read_outgoing().unwrap();

    let destination: String = binding
        .route()
        .destination()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let route_row = format!("desktop|personal-smarthost|{destination}");
    assert_eq!(sql(fixture, "SELECT submitting_profile,route_name,encode(destination,'hex') FROM td_mta.submission_routes"), route_row);
    for assignment in [
        "route_name='Other'",
        "submitting_profile=''",
        "route_name=repeat('a',65)",
        "destination=decode('00','hex')",
    ] {
        sql(fixture, &format!("DO $$ BEGIN BEGIN UPDATE td_mta.submission_routes SET {assignment}; RAISE EXCEPTION 'unexpected success'; EXCEPTION WHEN check_violation THEN NULL; END; END $$"));
    }
    let usage = sql(fixture, "SELECT body_bytes||','||blob_count||','||queue_bytes||','||queue_submissions FROM td_mta.account_usage");
    assert_eq!(
        usage,
        format!("{},2,{},1", input.source().body.length + a.length, a.length)
    );
    assert_eq!(
        submissions::publish(request, trust, deadline(), &first, b"bad").unwrap(),
        a
    );
    let stale = submissions::prepare(input.clone(), deadline()).unwrap();
    assert_eq!(
        submissions::publish(request, trust, deadline(), &stale, password),
        Err(submissions::Error::State)
    );

    let refused = make();
    sql(fixture, "CREATE FUNCTION public.reject_submission() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refuse'; END $$; CREATE TRIGGER reject_submission BEFORE INSERT ON td_mta.submission_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_submission()");
    assert_eq!(
        submissions::publish(request, trust, deadline(), &refused, password),
        Err(submissions::Error::Database)
    );
    assert_eq!(
        submissions::inspect(request, trust, deadline(), &refused).unwrap(),
        None
    );
    assert_eq!(sql(fixture, "SELECT body_bytes||','||blob_count||','||queue_bytes||','||queue_submissions FROM td_mta.account_usage"), usage);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blobs"), "2");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "1");
    sql(
        fixture,
        "DROP TRIGGER reject_submission ON td_mta.submission_receipts",
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submission_routes"),
        "1"
    );
    sql(fixture, "CREATE TRIGGER reject_route BEFORE INSERT ON td_mta.submission_routes FOR EACH ROW EXECUTE FUNCTION public.reject_submission()");
    assert_eq!(
        submissions::publish(request, trust, deadline(), &refused, password),
        Err(submissions::Error::Database)
    );
    assert_eq!(
        submissions::inspect(request, trust, deadline(), &refused).unwrap(),
        None
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.submissions"), "1");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.blobs"), "2");
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submission_routes"),
        "1"
    );
    sql(
        fixture,
        "DROP TRIGGER reject_route ON td_mta.submission_routes",
    );

    for (column, cap, old) in [
        ("queue_bytes", binding.quotas().queue_bytes, a.length),
        ("queue_submissions", binding.quotas().queue_submissions, 1),
        (
            "body_bytes",
            binding.quotas().body_bytes,
            input.source().body.length + a.length,
        ),
    ] {
        let body_bytes = input.source().body.length + a.length;
        if column == "queue_bytes" {
            sql(
                fixture,
                &format!("UPDATE td_mta.account_usage SET body_bytes={cap}"),
            );
        }
        sql(
            fixture,
            &format!("UPDATE td_mta.account_usage SET {column}={cap}"),
        );
        assert_eq!(
            submissions::publish(request, trust, deadline(), &refused, password),
            Err(submissions::Error::Quota)
        );
        assert_eq!(
            submissions::inspect(request, trust, deadline(), &refused).unwrap(),
            None
        );
        sql(
            fixture,
            &format!("UPDATE td_mta.account_usage SET {column}={old}"),
        );
        if column == "queue_bytes" {
            sql(
                fixture,
                &format!("UPDATE td_mta.account_usage SET body_bytes={body_bytes}"),
            );
        }
    }
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        submissions::publish(request, trust, deadline(), &refused, password),
        Err(submissions::Error::Policy)
    );
    assert_eq!(read_outgoing(), Err(delivery_body::Error::Policy));
    assert_eq!(
        submissions::publish(request, trust, deadline(), &first, b"bad").unwrap(),
        a
    );
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            binding.selection().generation
        ),
    );

    let second_device =
        devices::prepare_creation(binding.epoch(), account, "revoked queue", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &second_device).unwrap();
    let revoked = inputs::prepare(
        request,
        trust,
        deadline(),
        inputs::Input {
            binding,
            email: input.source().email,
            identity: input.identity(),
            expected: None,
        },
        second_device.password(),
    )
    .unwrap();
    let revoked = submissions::prepare(revoked, deadline()).unwrap();
    let revocation =
        devices::prepare_revocation(binding.epoch(), account, second_device.device_id(), 1)
            .unwrap();
    devices::publish_revocation(request, trust, deadline(), &revocation).unwrap();
    assert_eq!(
        submissions::publish(
            request,
            trust,
            deadline(),
            &revoked,
            second_device.password()
        ),
        Err(submissions::Error::Unauthenticated)
    );

    sql(fixture, "CREATE FUNCTION public.delay_submission() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_submission AFTER INSERT ON td_mta.submission_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_submission()");
    assert_eq!(
        submissions::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &refused,
            password
        ),
        Err(submissions::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let recovered = submissions::inspect(request, trust, deadline(), &refused)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.new_sequence, 4);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submission_routes"),
        "2"
    );

    assert_eq!(
        submissions::publish(request, trust, deadline(), &refused, password).unwrap(),
        recovered
    );
    sql(
        fixture,
        "DROP TRIGGER delay_submission ON td_mta.submission_receipts",
    );

    let digest = sql(fixture,&format!("SELECT encode(request_digest,'hex') FROM td_mta.submission_receipts WHERE operation=decode('{}','hex')",first.operation()));
    sql(fixture,&format!("UPDATE td_mta.submission_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation=decode('{}','hex')",first.operation()));
    assert_eq!(
        submissions::inspect(request, trust, deadline(), &first),
        Err(submissions::Error::Conflict)
    );
    sql(fixture,&format!("UPDATE td_mta.submission_receipts SET request_digest=decode('{digest}','hex') WHERE operation=decode('{}','hex')",first.operation()));

    use td_mta::{
        compose_text::{Address, Message},
        ids::MailboxId,
        postgres_drafts as drafts,
    };
    let mailboxes = sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    );
    let mailbox = MailboxId::parse(&mailboxes).unwrap();
    let addresses: Vec<_> = (0..1000).map(|i| format!("r{i}@example.test")).collect();
    let recipients: Vec<_> = addresses
        .iter()
        .map(|email| Address { name: "", email })
        .collect();
    let message = Message {
        from: Address {
            name: "",
            email: "main@example.test",
        },
        to: &recipients,
        cc: &[],
        bcc: &[],
        reply_to: &[],
        subject: "full envelope",
        message_id: "full-envelope@example.test",
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text: "body",
    };
    let draft = drafts::prepare(
        drafts::DraftRequest {
            binding,
            device: input.device(),
            mailbox,
            expected: None,
        },
        &message,
        deadline(),
    )
    .unwrap();
    let draft = drafts::publish(request, trust, deadline(), &draft, password).unwrap();
    let full = inputs::prepare(
        request,
        trust,
        deadline(),
        inputs::Input {
            binding,
            email: draft.email,
            identity: input.identity(),
            expected: None,
        },
        password,
    )
    .unwrap();
    assert_eq!(full.recipients().len(), 1000);
    let full = submissions::prepare(full, deadline()).unwrap();
    let full_outcome = submissions::publish(request, trust, deadline(), &full, password).unwrap();
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{}','hex')",
                full_outcome.submission
            )
        ),
        "1000"
    );

    exercise_success_filing(
        fixture,
        request,
        trust,
        binding,
        &input,
        draft.email,
        password,
    );
    exercise_http_submission_owner(fixture, binding, &input, password);
    let capacity = make();
    sql(fixture,"INSERT INTO td_mta.submission_receipts SELECT account,decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),submission_id,blob_id,length,send_at,expires_at,old_sequence,new_sequence FROM (SELECT * FROM td_mta.submission_receipts LIMIT 1) r CROSS JOIN generate_series(1,1024) n");
    assert_eq!(
        submissions::publish(request, trust, deadline(), &capacity, password),
        Err(submissions::Error::Capacity)
    );
    sql(
        fixture,
        "DELETE FROM td_mta.submission_receipts WHERE request_digest=decode(repeat('00',32),'hex')",
    );

    use td_mta::postgres_email_set::deletion;
    let destroy =
        deletion::prepare(binding, input.device(), &[input.source().email], None).unwrap();
    deletion::publish(request, trust, deadline(), &destroy, password).unwrap();
    assert_eq!(
        submissions::publish(request, trust, deadline(), &capacity, password),
        Err(submissions::Error::Source)
    );
    assert_eq!(
        submissions::publish(request, trust, deadline(), &first, b"bad").unwrap(),
        a
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.blobs WHERE id=decode('{}','hex')",
                a.blob
            )
        ),
        "1"
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.blobs WHERE id=decode('{}','hex')",
                input.source().blob
            )
        ),
        "0"
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{}','hex') AND state=1",a.submission)),"2");
    assert_eq!(sql(fixture, &format!("SELECT submitting_profile,route_name,encode(destination,'hex') FROM td_mta.submission_routes WHERE submission_id=decode('{}','hex')", a.submission)), route_row);
    read_outgoing().unwrap();
    exercise_dispatch(
        fixture,
        request,
        trust,
        &relay_binding,
        a.submission,
        &relay_policies,
    );
    native_relay_connect::exercise(fixture, request, trust, binding.epoch(), a.submission);
}

fn exercise_http_submission_owner(
    fixture: &Fixture,
    binding: &td_mta::postgres_runtime_profile::Binding,
    input: &td_mta::postgres_submission_input::Prepared,
    password: &[u8],
) {
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{self, Clock, Deadline},
        postgres_http_submissions::{self as http, Error},
        postgres_read::ReadSlots,
        postgres_submissions as submissions,
    };
    create_runtime_login(fixture);
    grant_runtime_mail(fixture);
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let clock = RuntimeClock::new();
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let meter = || {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: 1_000_000,
                records: 100_000,
                output_bytes: 1_000_000,
                ..Default::default()
            },
        )
    };
    let pending = http::Pending::default();
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let worker = http::Submissions {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &pending,
        password,
    };
    let request = |ms| http::Input {
        account: binding.account(),
        device: input.device(),
        email: input.source().email,
        identity: input.identity(),
        expected: None,
        deadline: until(ms),
    };
    let count = || {
        sql(fixture, "SELECT count(*) FROM td_mta.submissions")
            .parse::<u64>()
            .unwrap()
    };
    let before = count();
    let mut exhausted = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker.create(request(10_000), &mut exhausted),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert!(exhausted.stopped().is_some());
    assert_eq!(
        worker.create(request(0), &mut meter()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut wrong = request(10_000);
    wrong.device = td_mta::ids::DeviceId::from_bytes([0xee; 16]);
    assert_eq!(
        worker.create(wrong, &mut meter()),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    assert_eq!(count(), before);
    let success = worker.create(request(10_000), &mut meter()).unwrap();
    assert_eq!(count(), before + 1);
    assert_eq!(success.email, input.source().email);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(sql(fixture,&format!("SELECT submitting_profile||':'||route_name FROM td_mta.submission_routes WHERE submission_id=decode('{}','hex')",success.submission)),"desktop:personal-smarthost");
    sql(fixture, "CREATE TRIGGER refuse_http_submission BEFORE INSERT ON td_mta.submission_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_submission()");
    assert_eq!(
        worker.create(request(10_000), &mut meter()),
        Err(Error::Publication(submissions::Error::Database))
    );
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(count(), before + 1);
    sql(fixture, "DROP TRIGGER refuse_http_submission ON td_mta.submission_receipts; CREATE FUNCTION public.delay_http_submission() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_http_submission AFTER INSERT ON td_mta.submission_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_http_submission()");
    assert_eq!(
        worker.create(request(3000), &mut meter()),
        Err(Error::Publication(submissions::Error::Indeterminate))
    );
    let operation = pending.operation().unwrap().unwrap();
    assert_eq!(
        worker.create(request(10_000), &mut meter()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert_eq!(pending.operation().unwrap(), Some(operation));
    thread::sleep(Duration::from_secs(5));
    sql(
        fixture,
        "DROP TRIGGER delay_http_submission ON td_mta.submission_receipts",
    );
    let mut connection = loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let foreground = td_mta::postgres_read::Snapshot::open_runtime(
        &slots,
        &connection,
        &loaded.trust,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    let settled = pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(settled.outcome.email, input.source().email);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(count(), before + 2);
    assert_eq!(sql(fixture,&format!("SELECT encode(submission_id,'hex') FROM td_mta.submission_receipts WHERE operation=decode('{operation}','hex')")), settled.outcome.submission.to_string());
    drop(foreground);
    assert!(pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .is_none());
    exercise_http_filing_owner(fixture, &loaded, binding, input, password);
    exercise_submission_reads(fixture, &loaded, binding, input, password);
    exercise_submission_set_dispatch(fixture, &loaded, binding, input, password);
}

fn exercise_submission_reads(
    fixture: &Fixture,
    loaded: &td_mta::postgres_input::Loaded,
    binding: &td_mta::postgres_runtime_profile::Binding,
    input: &td_mta::postgres_submission_input::Prepared,
    password: &[u8],
) {
    use td_mta::{
        ids::{IdentityId, SubmissionId},
        postgres_read::{Error, ReadSlots, Snapshot, SubmissionQuery, MAX_SUBMISSION_PAGE},
        sync::QueryType,
    };
    let slots = ReadSlots::new(1).unwrap();
    let mut connection = loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let mut view = Snapshot::open_runtime(
        &slots,
        &connection,
        &loaded.trust,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    let query = SubmissionQuery {
        identity: input.identity(),
        after: i64::MIN,
        before: i64::MAX,
        ascending: true,
        position: 0,
        limit: MAX_SUBMISSION_PAGE,
        calculate_total: true,
    };
    let all = view.query_submissions(query).unwrap();
    exercise_submission_query_dispatch(fixture, &mut view, input.identity(), &all.ids);
    assert!(all.ids.len() >= 2);
    assert_eq!(all.total, Some(all.ids.len() as u64));
    assert!(!all.has_more);
    assert!(all
        .state
        .matches_current(view.identity(), QueryType::Submission));
    let expected = sql(fixture, &format!("SELECT encode(id,'hex') FROM td_mta.submissions WHERE account=decode('{}','hex') AND identity_id=decode('{}','hex') ORDER BY send_at,id", binding.account(), input.identity()));
    assert_eq!(
        all.ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        expected
    );
    let first = *all.ids.first().unwrap();
    for (position, expected) in all.ids.iter().enumerate() {
        let page = view
            .query_submissions(SubmissionQuery {
                position: position as u64,
                limit: 1,
                ..query
            })
            .unwrap();
        assert_eq!(page.ids, vec![*expected]);
        assert_eq!(page.state, all.state);
        assert_eq!(page.total, all.total);
        assert_eq!(page.position, position as u64);
        assert_eq!(page.has_more, position + 1 < all.ids.len());
    }
    let record = view.submission(first).unwrap().unwrap();
    assert_eq!(record.view, view.identity());
    assert_eq!(record.identity, input.identity());
    assert_eq!(record.submission, first);
    assert_eq!(record.email.to_string(), sql(fixture, &format!("SELECT encode(email_id,'hex') FROM td_mta.submissions WHERE id=decode('{first}','hex')")));
    exercise_submission_get_dispatch(fixture, &mut view, first, record.email);
    let absent = SubmissionId::from_bytes([0xfe; 16]);
    assert!(view.submission(absent).unwrap().is_none());
    let same_time = view
        .query_submissions(SubmissionQuery {
            after: record.send_at,
            before: record.send_at + 1,
            ..query
        })
        .unwrap();
    assert!(same_time.ids.contains(&first));
    let excluded = view
        .query_submissions(SubmissionQuery {
            before: record.send_at,
            ..query
        })
        .unwrap();
    assert!(!excluded.ids.contains(&first));
    let descending = view
        .query_submissions(SubmissionQuery {
            ascending: false,
            ..query
        })
        .unwrap();
    let expected = sql(fixture, &format!("SELECT encode(id,'hex') FROM td_mta.submissions WHERE account=decode('{}','hex') AND identity_id=decode('{}','hex') ORDER BY send_at DESC,id", binding.account(), input.identity()));
    assert_eq!(
        descending
            .ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        expected
    );
    let empty = view
        .query_submissions(SubmissionQuery {
            identity: IdentityId::from_bytes([0xfe; 16]),
            ..query
        })
        .unwrap();
    assert_eq!(empty.total, Some(0));
    assert!(empty.ids.is_empty());
    let zero = view
        .query_submissions(SubmissionQuery { limit: 0, ..query })
        .unwrap();
    assert!(zero.ids.is_empty() && zero.has_more);
    let beyond = view
        .query_submissions(SubmissionQuery {
            position: u64::MAX,
            ..query
        })
        .unwrap();
    assert!(beyond.ids.is_empty() && !beyond.has_more);
    assert_eq!(beyond.total, all.total);
    let inverted = view
        .query_submissions(SubmissionQuery {
            after: 1,
            before: 0,
            calculate_total: false,
            ..query
        })
        .unwrap();
    assert!(inverted.ids.is_empty());
    assert_eq!(inverted.total, None);
    assert!(matches!(
        view.query_submissions(SubmissionQuery {
            limit: MAX_SUBMISSION_PAGE + 1,
            ..query
        }),
        Err(Error::Capacity)
    ));
    assert_eq!(view.submission(first), Err(Error::Capacity));
}

fn exercise_submission_get_dispatch(
    fixture: &Fixture,
    snapshot: &mut td_mta::postgres_read::Snapshot<'_>,
    submission: td_mta::ids::SubmissionId,
    email: td_mta::ids::EmailId,
) {
    use std::sync::Arc;
    use td_mta::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        jmap_request::{dispatch, response, OwnedValidation, CORE, SUBMISSION},
        limits::Limits,
        ports::{Clock, Deadline, Tick},
        store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    };
    let mut bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(snapshot.identity().account, &mut bytes)
        .unwrap()
        .to_owned();
    let submission = td_mta::wire::encode_id(submission, &mut bytes)
        .unwrap()
        .to_owned();
    let email = td_mta::wire::encode_id(email, &mut bytes)
        .unwrap()
        .to_owned();
    let missing = td_mta::wire::encode_id(
        td_mta::ids::SubmissionId::from_bytes([0xfe; 16]),
        &mut bytes,
    )
    .unwrap()
    .to_owned();
    for capability in [true, false] {
        let using = if capability {
            format!("\"{CORE}\",\"{SUBMISSION}\"")
        } else {
            format!("\"{CORE}\"")
        };
        let input = format!(
            r##"{{"using":[{using}],"methodCalls":[
            ["EmailSubmission/get",{{"accountId":"{account}","ids":["{submission}","{missing}","foreign","{submission}"],"properties":["id","emailId"]}},"a"],
            ["EmailSubmission/get",{{"accountId":"{account}","ids":["{submission}"],"properties":[]}},"b"],
            ["EmailSubmission/get",{{"accountId":"{account}","#ids":{{"resultOf":"a","name":"EmailSubmission/get","path":"/list/*/id"}},"properties":["emailId"]}},"c"],
            ["EmailSubmission/get",{{"accountId":"afefefefefefefefefefefefefefefefe","ids":[],"properties":["id"]}},"d"],
            ["EmailSubmission/get",{{"accountId":"{account}","ids":[]}},"e"],
            ["EmailSubmission/get",{{"accountId":"{account}","ids":null,"properties":["id"]}},"f"],
            ["EmailSubmission/get",{{"accountId":"{account}","ids":[],"properties":["undoStatus"]}},"g"],
            ["Core/echo",{{"after":true}},"h"]
        ]}}"##
        );
        let limits = Limits::default();
        let mut input_bytes = vec![0; limits.json_bytes].into_boxed_slice();
        input_bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let mut parser = td_json::arena::OwnedCursor::with_input_length(
            input_bytes,
            tokens,
            input.len(),
            td_json::arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        let resources = limits.plan().unwrap();
        let mut validation =
            OwnedValidation::new(parser.finish().unwrap(), &resources, &[CORE, SUBMISSION])
                .unwrap();
        while !validation.step().unwrap() {}
        let request = validation.finish().unwrap();
        let directory = fixture.root.join(if capability {
            "submission-get-responses"
        } else {
            "submission-get-no-using"
        });
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut root = PrivateRoot::open(directory.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock = Arc::new(td_mta::clock::RuntimeClock::new());
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let spool = RequestSpool::open(&mut root, clock.clone(), until).unwrap();
        let disk = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
        let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
        let owner =
            ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
        let retention = owner.retain_response(until).unwrap();
        let mut preflight =
            response::Preflight::new(request, "session", retention.maximum()).unwrap();
        while !preflight.step(&mut [0; 4096]).unwrap() {}
        let execution = Deadline::after(Tick(0), until.tick().0 - 1).unwrap();
        let mut complete = dispatch::run_with_postgres_read(
            preflight.finish().unwrap(),
            retention,
            execution,
            snapshot,
            &clock,
            false,
        )
        .unwrap();
        assert_eq!(complete.responses().len(), 8);
        let mut encoder = complete.encoder().unwrap();
        while !encoder.count_step(&mut [0; 4096]).unwrap() {}
        let length = encoder.begin_output().unwrap();
        let mut output = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let progress = encoder.poll_output(&mut chunk).unwrap();
            output.extend_from_slice(&chunk[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(encoder.finish().unwrap(), length);
        assert_eq!(output.len() as u64, length);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains(r#"["Core/echo",{"after":true},"h"]"#),
            "{output}"
        );
        if capability {
            assert_eq!(
                output.matches(r#"["EmailSubmission/get","#).count(),
                3,
                "{output}"
            );
            assert_eq!(
                output
                    .matches(&format!(r#""id":"{submission}","emailId":"{email}""#))
                    .count(),
                2,
                "{output}"
            );
            assert!(
                output.contains(&format!(r#""notFound":["{missing}","foreign"]"#)),
                "{output}"
            );
            assert!(
                output.contains(&format!(
                    r#""list":[{{"id":"{submission}"}}],"notFound":[]}},"b""#
                )),
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"accountNotFound"},"d"]"#),
                "{output}"
            );
            for call in ["e", "f", "g"] {
                assert!(
                    output.contains(&format!(
                        r#"["error",{{"type":"invalidArguments"}},"{call}"]"#
                    )),
                    "{output}"
                );
            }
        } else {
            assert_eq!(
                output.matches(r#""type":"unknownMethod""#).count(),
                6,
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"invalidResultReference"},"c"]"#),
                "{output}"
            );
        }
    }
}

fn exercise_http_filing_owner(
    fixture: &Fixture,
    loaded: &td_mta::postgres_input::Loaded,
    binding: &td_mta::postgres_runtime_profile::Binding,
    input: &td_mta::postgres_submission_input::Prepared,
    password: &[u8],
) {
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ids::MailboxId,
        ports::{self, Clock, Deadline},
        postgres_email_set::UpdateResult,
        postgres_http_updates::{FilingRequest, Pending, Updates},
        postgres_read::{ReadSlots, Snapshot},
    };
    let clock = RuntimeClock::new();
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let meter = || {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: 1_000_000,
                records: 100_000,
                output_bytes: 1_000_000,
                ..Default::default()
            },
        )
    };
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let pending = Pending::default();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let worker = Updates {
        loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &pending,
        password,
    };
    let source = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    ))
    .unwrap();
    let sent = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE name='Filing Sent'",
    ))
    .unwrap();
    let request = |ms| FilingRequest {
        account: binding.account(),
        device: input.device(),
        email: input.source().email,
        movement: Some((source, sent)),
        deadline: until(ms),
    };
    let email = input.source().email;
    let queued = sql(fixture, "SELECT count(*) FROM td_mta.submissions");
    let exhausted_request = request(10_000);
    let mut exhausted = Meter::new(exhausted_request.deadline, Charge::default());
    assert!(matches!(
        worker.file(exhausted_request, &mut exhausted),
        Err(td_mta::store_fs::UploadError::Store(ports::Error::Capacity))
    ));
    assert!(exhausted.stopped().is_some());
    sql(fixture, "CREATE TRIGGER reject_http_filing BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_filing()");
    assert!(worker.file(request(10_000), &mut meter()).is_err());
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submissions"),
        queued
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.keywords WHERE email_id=decode('{email}','hex') AND keyword='$draft'")), "1");
    sql(fixture, "DROP TRIGGER reject_http_filing ON td_mta.flag_receipts; CREATE FUNCTION public.delay_http_filing() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_http_filing AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_http_filing()");
    let result = worker.file(request(3000), &mut meter()).unwrap();
    assert!(matches!(
        result.outcome,
        Err(ports::CommitFailure::Indeterminate(_))
    ));
    assert!(result.admission_stopped);
    let operation = pending.operation().unwrap().unwrap();
    assert!(matches!(
        worker.file(request(10_000), &mut meter()),
        Err(td_mta::store_fs::UploadError::Store(ports::Error::Busy))
    ));
    assert_eq!(pending.operation().unwrap(), Some(operation));
    thread::sleep(Duration::from_secs(5));
    sql(
        fixture,
        "DROP TRIGGER delay_http_filing ON td_mta.flag_receipts",
    );
    let mut connection = loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let foreground = Snapshot::open_runtime(
        &slots,
        &connection,
        &loaded.trust,
        deadline(),
        binding,
        password,
    )
    .unwrap();
    let settled = pending
        .settle(loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(settled.outcome.results, [UpdateResult::Updated]);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submissions"),
        queued
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.keywords WHERE email_id=decode('{email}','hex') AND keyword='$draft'")), "0");
    assert_eq!(sql(fixture,&format!("SELECT encode(mailbox_id,'hex') FROM td_mta.memberships WHERE email_id=decode('{email}','hex')")), sent.to_string());
    drop(foreground);
    let missing_source = worker.file(request(10_000), &mut meter()).unwrap();
    assert!(missing_source.outcome.is_ok());
    assert_eq!(
        missing_source.results,
        [td_mta::store_fs::EmailUpdateResult::InvalidMailbox]
    );
}

fn exercise_success_filing(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    input: &td_mta::postgres_submission_input::Prepared,
    email: td_mta::ids::EmailId,
    password: &[u8],
) {
    use td_mta::{
        ids::{EmailId, MailboxId},
        postgres_email_set::{filing, Error, UpdateResult},
        postgres_mailbox_set as folders,
    };
    let creation = folders::prepare(
        binding,
        input.device(),
        None,
        &["Filing Sent", "Unrelated"],
        deadline(),
    )
    .unwrap();
    let created = folders::publish(request, trust, deadline(), &creation, password).unwrap();
    let [folders::ResultEntry::Created(sent), folders::ResultEntry::Created(other)] =
        created.results.as_slice()
    else {
        panic!("folders not created")
    };
    let source = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    ))
    .unwrap();
    let account = binding.account();
    let make = |movement| filing::prepare(binding, input.device(), email, movement).unwrap();
    let patch = make(Some((source, *sent)));
    assert!(matches!(
        filing::prepare(binding, input.device(), email, Some((source, source))),
        Err(Error::Invalid)
    ));
    assert_eq!(
        filing::publish(request, trust, deadline(), &patch, b"bad"),
        Err(Error::Unauthenticated)
    );
    let state = || {
        sql(fixture, &format!("SELECT encode(changed,'hex')||':'||(SELECT count(*) FROM td_mta.keywords WHERE email_id=e.id AND keyword='$draft') FROM td_mta.emails e WHERE id=decode('{email}','hex')"))
    };
    let memberships = || {
        sql(fixture, &format!("SELECT string_agg(encode(mailbox_id,'hex'),',' ORDER BY mailbox_id) FROM td_mta.memberships WHERE email_id=decode('{email}','hex')"))
    };
    let queued = sql(fixture, "SELECT count(*) FROM td_mta.submissions");
    sql(fixture, &format!("DELETE FROM td_mta.keywords WHERE email_id=decode('{email}','hex') AND keyword='$seen'"));
    let before = state();
    sql(fixture, "CREATE FUNCTION public.reject_filing() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refuse filing'; END $$; CREATE TRIGGER reject_filing BEFORE INSERT ON td_mta.flag_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_filing()");
    assert_eq!(
        filing::publish(request, trust, deadline(), &patch, password),
        Err(Error::Database)
    );
    assert_eq!(
        filing::inspect(request, trust, deadline(), &patch).unwrap(),
        None
    );
    assert_eq!(state(), before);
    assert_eq!(memberships(), source.to_string());
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submissions"),
        queued
    );
    sql(
        fixture,
        "DROP TRIGGER reject_filing ON td_mta.flag_receipts",
    );
    let missing = make(Some((source, MailboxId::from_bytes([0xee; 16]))));
    let refused = filing::publish(request, trust, deadline(), &missing, password).unwrap();
    assert_eq!(refused.results, [UpdateResult::InvalidMailbox]);
    assert_eq!(refused.state.old_sequence, refused.state.new_sequence);
    assert_eq!(state(), before);
    // A successful patch must retain unrelated memberships added meanwhile.
    sql(fixture, &format!("INSERT INTO td_mta.memberships SELECT id,decode('{email}','hex'),decode('{other}','hex'),sequence FROM td_mta.accounts WHERE id=decode('{account}','hex')"));
    sql(fixture, "CREATE FUNCTION public.delay_filing() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_filing AFTER INSERT ON td_mta.flag_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_filing()");
    assert_eq!(
        filing::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &patch,
            password
        ),
        Err(Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    sql(fixture, "DROP TRIGGER delay_filing ON td_mta.flag_receipts");
    let recovered = filing::inspect(request, trust, deadline(), &patch)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.results, [UpdateResult::Updated]);
    assert_eq!(
        recovered.state.new_sequence,
        recovered.state.old_sequence + 1
    );
    assert!(state().ends_with(":0"));
    let mut expected = [sent.to_string(), other.to_string()];
    expected.sort();
    assert_eq!(memberships(), expected.join(","));
    assert_eq!(
        filing::publish(request, trust, deadline(), &patch, b"bad").unwrap(),
        recovered
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submissions"),
        queued
    );
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE sequence=decode('{:016x}','hex') AND kind=1", recovered.state.new_sequence)), "3");
    // An absent source refuses without clearing draft or overwriting a move.
    sql(fixture, &format!("INSERT INTO td_mta.keywords SELECT id,decode('{email}','hex'),'$draft',sequence FROM td_mta.accounts WHERE id=decode('{account}','hex')"));
    let before = state();
    let moved = make(Some((source, *sent)));
    assert_eq!(
        filing::publish(request, trust, deadline(), &moved, password)
            .unwrap()
            .results,
        [UpdateResult::InvalidMailbox]
    );
    assert_eq!(state(), before);
    assert_eq!(memberships(), expected.join(","));
    let clear = make(None);
    let cleared = filing::publish(request, trust, deadline(), &clear, password).unwrap();
    assert_eq!(cleared.state.new_sequence, cleared.state.old_sequence + 1);
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE sequence=decode('{:016x}','hex') AND kind=1", cleared.state.new_sequence)), "2");
    let no_op = filing::publish(request, trust, deadline(), &make(None), password).unwrap();
    assert_eq!(no_op.state.old_sequence, no_op.state.new_sequence);
    assert_eq!(memberships(), expected.join(","));
    // An existing destination still changes unread counts when draft clears.
    sql(fixture, &format!("INSERT INTO td_mta.memberships SELECT id,decode('{email}','hex'),decode('{source}','hex'),sequence FROM td_mta.accounts WHERE id=decode('{account}','hex'); INSERT INTO td_mta.keywords SELECT id,decode('{email}','hex'),'$draft',sequence FROM td_mta.accounts WHERE id=decode('{account}','hex')"));
    let existing = filing::publish(
        request,
        trust,
        deadline(),
        &make(Some((source, *sent))),
        password,
    )
    .unwrap();
    assert_eq!(memberships(), expected.join(","));
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE sequence=decode('{:016x}','hex') AND kind=1", existing.state.new_sequence)), "3");
    // Seen clear-only filing changes Email history but no mailbox counts.
    sql(fixture, &format!("INSERT INTO td_mta.keywords SELECT id,decode('{email}','hex'),keyword,sequence FROM td_mta.accounts CROSS JOIN (VALUES ('$seen'),('$draft')) k(keyword) WHERE id=decode('{account}','hex')"));
    let seen = filing::publish(request, trust, deadline(), &make(None), password).unwrap();
    assert_eq!(seen.state.new_sequence, seen.state.old_sequence + 1);
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.changes WHERE sequence=decode('{:016x}','hex') AND kind=1", seen.state.new_sequence)), "0");
    let absent = filing::prepare(
        binding,
        input.device(),
        EmailId::from_bytes([0xee; 16]),
        None,
    )
    .unwrap();
    assert_eq!(
        filing::publish(request, trust, deadline(), &absent, password)
            .unwrap()
            .results,
        [UpdateResult::NotFound]
    );
    sql(fixture, &format!("UPDATE td_mta.flag_receipts SET request_digest=decode(repeat('00',32),'hex') WHERE operation=decode('{}','hex')", patch.operation()));
    assert_eq!(
        filing::inspect(request, trust, deadline(), &patch),
        Err(Error::Conflict)
    );
}

fn exercise_route_owners(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    relay: &td_mta::postgres_runtime_profile::Binding,
    jmap: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{ids::BootId, postgres_route_owners as owners};
    let boot = BootId::from_bytes([1; 16]);
    assert!(matches!(
        owners::prepare(jmap, boot),
        Err(owners::Error::Policy)
    ));
    let first = owners::prepare(relay, boot).unwrap();
    assert_eq!(
        owners::inspect(request, trust, deadline(), &first).unwrap(),
        None
    );
    let (one, two) = thread::scope(|scope| {
        let one = scope.spawn(|| owners::acquire(request, trust, deadline(), &first));
        let two = scope.spawn(|| owners::acquire(request, trust, deadline(), &first));
        (one.join().unwrap().unwrap(), two.join().unwrap().unwrap())
    });
    assert_eq!(one, two);
    assert_eq!(one.generation, 1);
    assert_eq!(one.expires_at - one.claimed_at, owners::LEASE_MS);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.route_owner_receipts"),
        "1"
    );
    // Stage a receipt appearing after inspection while policy changes.
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
    writeln!(input, "CREATE TEMP TABLE saved_owner_receipt AS SELECT * FROM td_mta.route_owner_receipts WHERE operation=decode('{}','hex'); DELETE FROM td_mta.route_owner_receipts WHERE operation=decode('{}','hex'); BEGIN; SELECT singleton FROM td_mta.config_selection WHERE singleton=true FOR UPDATE;", first.operation(), first.operation()).unwrap();
    let stop = deadline();
    while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%FROM td_mta.config_selection%'") != "1" {
        assert!(Instant::now() < stop, "owner replay barrier not held");
        thread::sleep(Duration::from_millis(5));
    }
    thread::scope(|scope| {
        let pending = scope.spawn(|| owners::acquire(request, trust, deadline(), &first));
        let stop = deadline();
        while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND wait_event_type='Lock' AND query LIKE 'SELECT generation,version_id FROM td_mta.config_selection%'") != "1" {
            assert!(Instant::now() < stop, "owner replay did not reach barrier");
            thread::sleep(Duration::from_millis(5));
        }
        writeln!(input, "INSERT INTO td_mta.route_owner_receipts SELECT * FROM saved_owner_receipt; UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex'); COMMIT;").unwrap();
        drop(input);
        wait_command(&mut blocker);
        assert_eq!(pending.join().unwrap().unwrap(), one);
    });
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            relay.selection().generation
        ),
    );
    let second = owners::prepare(relay, BootId::from_bytes([2; 16])).unwrap();
    assert_eq!(
        owners::acquire(request, trust, deadline(), &second),
        Err(owners::Error::Busy)
    );
    assert_eq!(
        owners::observe(request, trust, deadline(), &first, one.generation).unwrap(),
        one.expires_at
    );
    let renewed = owners::renew(request, trust, deadline(), &first, one.generation).unwrap();
    assert!(renewed >= one.expires_at);
    assert_eq!(
        owners::observe(request, trust, deadline(), &first, one.generation).unwrap(),
        renewed
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        owners::renew(request, trust, deadline(), &first, one.generation),
        Err(owners::Error::Policy)
    );
    assert_eq!(
        owners::acquire(request, trust, deadline(), &first).unwrap(),
        one
    );
    owners::release(request, trust, deadline(), &first, one.generation).unwrap();
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            relay.selection().generation
        ),
    );
    assert_eq!(
        owners::observe(request, trust, deadline(), &first, one.generation),
        Err(owners::Error::Stale)
    );
    let next = owners::acquire(request, trust, deadline(), &second).unwrap();
    assert_eq!(next.generation, 2);
    assert_eq!(
        owners::renew(request, trust, deadline(), &first, one.generation),
        Err(owners::Error::Stale)
    );
    assert_eq!(
        owners::release(request, trust, deadline(), &first, one.generation),
        Err(owners::Error::Stale)
    );
    let clock = sql(fixture, "SELECT last_clock FROM td_mta.route_owners");
    sql(
        fixture,
        "UPDATE td_mta.route_owners SET last_clock=last_clock+60000,expires_at=expires_at+60000",
    );
    assert_eq!(
        owners::renew(request, trust, deadline(), &second, next.generation),
        Err(owners::Error::Clock)
    );
    sql(fixture,&format!("UPDATE td_mta.route_owners SET last_clock={clock},expires_at={clock}+1; SELECT pg_sleep(0.02)"));
    assert_eq!(
        owners::renew(request, trust, deadline(), &second, next.generation),
        Err(owners::Error::Stale)
    );
    let third = owners::prepare(relay, BootId::from_bytes([3; 16])).unwrap();
    let takeover = owners::acquire(request, trust, deadline(), &third).unwrap();
    assert_eq!(takeover.generation, 3);
    assert_eq!(
        owners::release(request, trust, deadline(), &second, next.generation),
        Err(owners::Error::Stale)
    );
    owners::release(request, trust, deadline(), &third, takeover.generation).unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_owner() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_owner AFTER INSERT ON td_mta.route_owner_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_owner()");
    let uncertain = owners::prepare(relay, boot).unwrap();
    let recovered = route_acquisition::exercise(fixture, request, trust, relay, &first, &uncertain);
    assert_eq!(recovered.generation, 4);
    assert_eq!(
        owners::acquire(request, trust, deadline(), &uncertain).unwrap(),
        recovered
    );
    owners::release(request, trust, deadline(), &uncertain, recovered.generation).unwrap();
    sql(
        fixture,
        "DROP TRIGGER delay_owner ON td_mta.route_owner_receipts",
    );
    let pending = owners::prepare(relay, boot).unwrap();
    sql(
        fixture,
        "UPDATE td_mta.route_owners SET generation=decode(repeat('ff',8),'hex')",
    );
    assert_eq!(
        owners::acquire(request, trust, deadline(), &pending),
        Err(owners::Error::Capacity)
    );
    sql(
        fixture,
        "UPDATE td_mta.route_owners SET generation=decode('0000000000000004','hex')",
    );
    sql(fixture,"INSERT INTO td_mta.route_owner_receipts SELECT account,decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('00',32),'hex'),generation,claimed_at,expires_at FROM (SELECT * FROM td_mta.route_owner_receipts LIMIT 1) r CROSS JOIN generate_series(1,1024) n");
    assert_eq!(
        owners::acquire(request, trust, deadline(), &pending),
        Err(owners::Error::Capacity)
    );
    sql(fixture,"DELETE FROM td_mta.route_owner_receipts WHERE request_digest=decode(repeat('00',32),'hex')");
    let last = owners::acquire(request, trust, deadline(), &pending).unwrap();
    assert_eq!(last.generation, 5);
    owners::release(request, trust, deadline(), &pending, last.generation).unwrap();
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.route_owners"),
        "1"
    );
}

#[path = "postgres_probe/attempt_acceptance.rs"]
mod attempt_acceptance;
#[path = "postgres_probe/attempt_command.rs"]
mod attempt_command;
#[path = "postgres_probe/attempt_interrupted.rs"]
mod attempt_interrupted;
#[path = "postgres_probe/attempt_phases.rs"]
mod attempt_phases;
#[path = "postgres_probe/attempt_rcpt.rs"]
mod attempt_rcpt;
#[path = "postgres_probe/attempt_refusal.rs"]
mod attempt_refusal;
#[path = "postgres_probe/attempt_retry.rs"]
mod attempt_retry;
#[path = "postgres_probe/body_checks.rs"]
mod body_checks;
#[path = "postgres_probe/envelope_checks.rs"]
mod envelope_checks;
#[path = "postgres_probe/final_capacity.rs"]
mod final_capacity;
#[path = "postgres_probe/final_work.rs"]
mod final_work;
#[path = "postgres_probe/new_possible.rs"]
mod new_possible;

fn exercise_dispatch(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    submission: td_mta::ids::SubmissionId,
    policies: &relay_wire::Policies,
) {
    use td_mta::{ids::BootId, postgres_attempts as attempts, postgres_route_owners as owners};
    let claim = owners::prepare(binding, BootId::from_bytes([9; 16])).unwrap();
    let owner = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let value = attempts::prepare(&claim, owner.generation, submission).unwrap();
    attempt_clock::exercise(
        fixture,
        request,
        trust,
        &claim,
        owner.generation,
        submission,
    );
    let bad = attempts::prepare(&claim, owner.generation + 1, submission).unwrap();
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &bad),
        Err(attempts::Error::Owner(owners::Error::Stale))
    );
    assert_eq!(
        attempts::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Policy)
    );
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            binding.selection().generation
        ),
    );
    let destination = sql(fixture, &format!("SELECT encode(destination,'hex') FROM td_mta.submission_routes WHERE submission_id=decode('{submission}','hex')"));
    sql(fixture, &format!("UPDATE td_mta.submission_routes SET destination=decode(repeat('00',32),'hex') WHERE submission_id=decode('{submission}','hex')"));
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Missing)
    );
    sql(fixture, &format!("UPDATE td_mta.submission_routes SET destination=decode('{destination}','hex') WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.recipients SET next_attempt_at=next_attempt_at+86400000 WHERE submission_id=decode('{submission}','hex')"));
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::NotDue)
    );
    sql(fixture, &format!("UPDATE td_mta.recipients SET next_attempt_at=next_attempt_at-86400000,uncertain=1 WHERE submission_id=decode('{submission}','hex')"));
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Integrity)
    );
    sql(fixture, &format!("UPDATE td_mta.recipients SET uncertain=0 WHERE submission_id=decode('{submission}','hex')"));
    // Expand one fresh submission to exercise the bounded ordinal batch.
    sql(fixture, &format!("INSERT INTO td_mta.recipients(account,submission_id,ordinal,address,state,uncertain,attempt_count,phase,next_attempt_at,reason,diagnostic,changed) SELECT account,submission_id,n,'recipient'||n||'@example.test',state,uncertain,attempt_count,phase,next_attempt_at,reason,diagnostic,changed FROM td_mta.recipients CROSS JOIN generate_series(2,999) n WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.submissions SET recipient_count=1000 WHERE id=decode('{submission}','hex')"));
    sql(fixture, "CREATE FUNCTION public.reject_attempt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_attempt BEFORE INSERT ON td_mta.submission_attempts FOR EACH ROW EXECUTE FUNCTION public.reject_attempt()");
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Database)
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.submission_attempts"),
        "0"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(sql(fixture, &format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=1")),"1000");
    sql(fixture,"DROP TRIGGER reject_attempt ON td_mta.submission_attempts; CREATE FUNCTION public.delay_attempt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_attempt AFTER INSERT ON td_mta.submission_attempts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_attempt()");
    assert_eq!(
        attempt_clock::publish(
            fixture,
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(attempts::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = attempts::inspect(request, trust, deadline(), &value)
        .unwrap()
        .unwrap();
    assert_eq!(outcome.ordinals, (0..100).collect::<Vec<_>>());
    assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
    attempt_clock::replay(fixture, request, trust, &value, &outcome);
    assert_eq!(outcome.attempt, value.attempt());
    assert_eq!(outcome.submission, submission);
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=2 AND phase=1 AND attempt_count=1 AND next_attempt_at IS NULL AND attempt_id=decode('{}','hex')",value.attempt())),"100");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=1")),"900");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.changes WHERE kind=5 AND action=2 AND object=decode('{submission}','hex') AND sequence=decode('{:016x}','hex')",outcome.new_sequence)),"1");
    let current_recovery =
        td_mta::postgres_attempt_recovery::prepare(&claim, owner.generation, value.attempt())
            .unwrap();
    assert_eq!(
        td_mta::postgres_attempt_recovery::publish(request, trust, deadline(), &current_recovery),
        Err(td_mta::postgres_attempt_recovery::Error::CurrentOwner)
    );
    owners::renew(request, trust, deadline(), &claim, owner.generation).unwrap();
    envelope_checks::exercise(fixture, request, trust, &value);
    attempt_phases::exercise(
        fixture,
        request,
        trust,
        binding,
        &claim,
        owner.generation,
        value.attempt(),
    );
    let other = attempts::prepare(&claim, owner.generation, submission).unwrap();
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &other),
        Err(attempts::Error::Busy)
    );
    owners::release(request, trust, deadline(), &claim, owner.generation).unwrap();
    let replacement = owners::prepare(binding, BootId::from_bytes([10; 16])).unwrap();
    let replacement_owner = owners::acquire(request, trust, deadline(), &replacement).unwrap();
    let replacement_attempt =
        attempts::prepare(&replacement, replacement_owner.generation, submission).unwrap();
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &replacement_attempt),
        Err(attempts::Error::Busy)
    );
    owners::release(
        request,
        trust,
        deadline(),
        &replacement,
        replacement_owner.generation,
    )
    .unwrap();
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        attempt_clock::publish(fixture, request, trust, deadline(), &value).unwrap(),
        outcome
    );
    sql(fixture,&format!("UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex'); DROP TRIGGER delay_attempt ON td_mta.submission_attempts",binding.selection().generation));
    exercise_attempt_recovery(
        fixture,
        request,
        trust,
        binding,
        submission,
        value.attempt(),
        policies,
    );
}

fn exercise_attempt_recovery(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    submission: td_mta::ids::SubmissionId,
    attempt: td_mta::ids::AttemptId,
    policies: &relay_wire::Policies,
) {
    use td_mta::{
        ids::BootId, postgres_attempt_recovery as recovery, postgres_route_owners as owners,
    };
    let claim = owners::prepare(binding, BootId::from_bytes([11; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let value = recovery::prepare(&claim, grant.generation, attempt).unwrap();
    assert_eq!(
        recovery::publish(
            request,
            trust,
            deadline(),
            &recovery::interrupted(&claim, grant.generation, attempt).unwrap()
        ),
        Err(recovery::Error::StaleAttempt)
    );
    assert_eq!(
        recovery::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    // Independently seed all recovery phase cases after phase-publication checks.
    sql(fixture, &format!("UPDATE td_mta.recipients SET phase=1,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex') AND state=2"));
    sql(fixture,&format!("UPDATE td_mta.recipients SET phase=3,rcpt_reply='250 accepted' WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.recipients SET phase=2,rcpt_reply='250 accepted' WHERE submission_id=decode('{submission}','hex') AND ordinal=1; UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal=2"));
    sql(fixture,&format!("UPDATE td_mta.attempt_final_reservations SET ordinals=decode('00000000','hex') WHERE attempt_id=decode('{attempt}','hex')"));
    let expiry = sql(
        fixture,
        &format!("SELECT expires_at FROM td_mta.submissions WHERE id=decode('{submission}','hex')"),
    );
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+1000 WHERE id=decode('{submission}','hex')"));
    expiry_wait_snapshot(fixture, || {
        recovery::publish(request, trust, deadline(), &value).unwrap();
    });
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at={expiry} WHERE id=decode('{submission}','hex')"));
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture,"CREATE FUNCTION public.reject_recovery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_recovery BEFORE INSERT ON td_mta.attempt_recovery_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_recovery()");
    assert_eq!(
        recovery::publish(request, trust, deadline(), &value),
        Err(recovery::Error::Database)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=2")),"100");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "1"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_recovery_receipts"
        ),
        "0"
    );
    sql(fixture,"DROP TRIGGER reject_recovery ON td_mta.attempt_recovery_receipts; CREATE FUNCTION public.delay_recovery() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_recovery AFTER INSERT ON td_mta.attempt_recovery_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_recovery()");
    let clock_before: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    assert_eq!(
        recovery::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(recovery::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = recovery::inspect(request, trust, deadline(), &value)
        .unwrap()
        .unwrap();
    let clock_after: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "0"
    );
    assert_eq!(outcome.attempt, attempt);
    assert_eq!(outcome.recipients, 100);
    assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=3 AND uncertain=0 AND phase=4 AND reason=3")),"98");
    assert_eq!(sql(fixture,&format!("SELECT string_agg(ordinal||':'||reason,',' ORDER BY ordinal) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=7 AND uncertain=1 AND phase=4")),"0:9,2:3");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<100 AND attempt_count=1 AND attempt_id=decode('{attempt}','hex') AND next_attempt_at BETWEEN {} AND {}",clock_before+300_000,clock_after+330_000)),"100");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal IN(0,1) AND rcpt_reply='250 accepted'")),"2");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "0"
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(
        recovery::publish(request, trust, deadline(), &value).unwrap(),
        outcome
    );
    sql(fixture,&format!("UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex'); DROP TRIGGER delay_recovery ON td_mta.attempt_recovery_receipts",binding.selection().generation));
    let next = td_mta::postgres_attempts::prepare(&claim, grant.generation, submission).unwrap();
    let next = attempt_clock::publish(fixture, request, trust, deadline(), &next).unwrap();
    assert_eq!(next.ordinals, (100..200).collect::<Vec<_>>());
    attempt_acceptance::exercise(
        fixture,
        request,
        trust,
        &claim,
        grant.generation,
        submission,
        next.attempt,
    );
    route_health::exercise(
        fixture,
        request,
        trust,
        &claim,
        binding,
        grant.generation,
        submission,
    );
    route_repair::exercise(fixture, request, trust, binding, submission);
    route_success::exercise(fixture, request, trust, binding, submission);
    submission_cancel::exercise(fixture, request, trust, binding, submission, policies);
}

fn exercise_submission_query_dispatch(
    fixture: &Fixture,
    snapshot: &mut td_mta::postgres_read::Snapshot<'_>,
    identity: td_mta::ids::IdentityId,
    ids: &[td_mta::ids::SubmissionId],
) {
    use std::sync::Arc;
    use td_mta::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        jmap_request::{dispatch, response, OwnedValidation, CORE, SUBMISSION},
        limits::Limits,
        ports::{Clock, Deadline, Tick},
        store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    };
    let mut bytes = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(snapshot.identity().account, &mut bytes)
        .unwrap()
        .to_owned();
    let identity = td_mta::wire::encode_id(identity, &mut bytes)
        .unwrap()
        .to_owned();
    let first = td_mta::wire::encode_id(ids[0], &mut bytes)
        .unwrap()
        .to_owned();
    let second = td_mta::wire::encode_id(ids[1], &mut bytes)
        .unwrap()
        .to_owned();
    let total = ids.len();
    let filter = format!(
        r#"{{"identityIds":["{identity}"],"after":"1900-01-01T00:00:00Z","before":"9999-01-01T00:00:00Z"}}"#
    );
    for capability in [true, false] {
        let using = if capability {
            format!("\"{CORE}\",\"{SUBMISSION}\"")
        } else {
            format!("\"{CORE}\"")
        };
        let input = format!(
            r##"{{"using":[{using}],"methodCalls":[
            ["EmailSubmission/query",{{"accountId":"{account}","filter":{filter},"sort":[{{"property":"sendAt","isAscending":true}}],"position":0,"limit":1,"calculateTotal":true}},"a"],
            ["EmailSubmission/query",{{"accountId":"{account}","filter":{filter},"sort":[{{"property":"sendAt","isAscending":true}}],"position":1,"limit":1,"calculateTotal":true}},"b"],
            ["EmailSubmission/get",{{"accountId":"{account}","#ids":{{"resultOf":"a","name":"EmailSubmission/query","path":"/ids"}},"properties":["id"]}},"c"],
            ["EmailSubmission/query",{{"accountId":"foreign","filter":{filter}}},"d"],
            ["EmailSubmission/query",{{"accountId":"{account}","filter":{{"emailIds":[]}}}},"e"],
            ["EmailSubmission/query",{{"accountId":"{account}","filter":{{"identityIds":["{identity}"],"after":"2025-02-29T00:00:00Z","before":"9999-01-01T00:00:00Z"}}}},"f"],
            ["EmailSubmission/query",{{"accountId":"{account}","filter":{{"identityIds":["foreign"],"after":"1900-01-01T00:00:00Z","before":"9999-01-01T00:00:00Z"}},"calculateTotal":true}},"g"],
            ["Core/echo",{{"after":true}},"h"]
        ]}}"##
        );
        let limits = Limits::default();
        let mut input_bytes = vec![0; limits.json_bytes].into_boxed_slice();
        input_bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let mut parser = td_json::arena::OwnedCursor::with_input_length(
            input_bytes,
            tokens,
            input.len(),
            td_json::arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        let resources = limits.plan().unwrap();
        let mut validation =
            OwnedValidation::new(parser.finish().unwrap(), &resources, &[CORE, SUBMISSION])
                .unwrap();
        while !validation.step().unwrap() {}
        let request = validation.finish().unwrap();
        let directory = fixture.root.join(if capability {
            "submission-query-responses"
        } else {
            "submission-query-no-using"
        });
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut root = PrivateRoot::open(directory.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock = Arc::new(td_mta::clock::RuntimeClock::new());
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let spool = RequestSpool::open(&mut root, clock.clone(), until).unwrap();
        let disk = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
        let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
        let owner =
            ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
        let retention = owner.retain_response(until).unwrap();
        let mut preflight =
            response::Preflight::new(request, "session", retention.maximum()).unwrap();
        while !preflight.step(&mut [0; 4096]).unwrap() {}
        let execution = Deadline::after(Tick(0), until.tick().0 - 1).unwrap();
        let mut complete = dispatch::run_with_postgres_read(
            preflight.finish().unwrap(),
            retention,
            execution,
            snapshot,
            &clock,
            false,
        )
        .unwrap();
        assert_eq!(complete.responses().len(), 8);
        let mut encoder = complete.encoder().unwrap();
        while !encoder.count_step(&mut [0; 4096]).unwrap() {}
        let length = encoder.begin_output().unwrap();
        let mut output = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let progress = encoder.poll_output(&mut chunk).unwrap();
            output.extend_from_slice(&chunk[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(encoder.finish().unwrap(), length);
        assert_eq!(output.len() as u64, length);
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains(r#"["Core/echo",{"after":true},"h"]"#),
            "{output}"
        );
        if capability {
            assert_eq!(
                output.matches(r#"["EmailSubmission/query","#).count(),
                3,
                "{output}"
            );
            assert!(
                output.contains(&format!(
                    r#""position":0, "total":{total}, "ids":["{first}"]}},"a""#
                )),
                "{output}"
            );
            assert!(
                output.contains(&format!(
                    r#""position":1, "total":{total}, "ids":["{second}"]}},"b""#
                )),
                "{output}"
            );
            assert!(
                output.contains(&format!(
                    r#""list":[{{"id":"{first}"}}],"notFound":[]}},"c""#
                )),
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"accountNotFound"},"d"]"#),
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"unsupportedFilter"},"e"]"#),
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"invalidArguments"},"f"]"#),
                "{output}"
            );
            assert!(output.contains(r#""total":0, "ids":[]},"g""#), "{output}");
        } else {
            assert_eq!(
                output.matches(r#""type":"unknownMethod""#).count(),
                6,
                "{output}"
            );
            assert!(
                output.contains(r#"["error",{"type":"invalidResultReference"},"c"]"#),
                "{output}"
            );
        }
    }
}

fn exercise_submission_set_dispatch(
    fixture: &Fixture,
    loaded: &td_mta::postgres_input::Loaded,
    binding: &td_mta::postgres_runtime_profile::Binding,
    source: &td_mta::postgres_submission_input::Prepared,
    password: &[u8],
) {
    use std::sync::Arc;
    use td_mta::{
        admission::{DiskLimits, ViewMode, WorkLimits},
        clock::RuntimeClock,
        jmap_request::{dispatch, response, OwnedValidation, CORE, MAIL, SUBMISSION},
        limits::Limits,
        ports::{Clock, Deadline, Tick},
        postgres_http_submissions as sending, postgres_http_updates as filing,
        postgres_read::{ReadSlots, Snapshot},
        store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
        sync::{DataState, DataType},
    };
    let slots = ReadSlots::new(1).unwrap();
    let pending = sending::Pending::default();
    let draft_pending = td_mta::postgres_http_drafts::Pending::default();
    let filing_pending = filing::Pending::default();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let mut connection = loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let mut buffer = [0; td_mta::wire::IdKind::WIRE_BYTES];
    let account = td_mta::wire::encode_id(binding.account(), &mut buffer)
        .unwrap()
        .to_owned();
    let email = td_mta::wire::encode_id(source.source().email, &mut buffer)
        .unwrap()
        .to_owned();
    let identity = td_mta::wire::encode_id(source.identity(), &mut buffer)
        .unwrap()
        .to_owned();
    for (refused, batch) in [(false, false), (true, false), (false, true), (true, true)] {
        let snapshot = Snapshot::open_runtime(
            &slots,
            &connection,
            &loaded.trust,
            deadline(),
            binding,
            password,
        )
        .unwrap();
        let view = snapshot.identity();
        let state = DataState {
            account: view.account,
            epoch: view.epoch,
            kind: DataType::Submission,
            sequence: view.committed_sequence,
        };
        drop(snapshot);
        let before = sql(fixture, "SELECT count(*) FROM td_mta.submissions")
            .parse::<usize>()
            .unwrap();
        let patch = if refused {
            td_json::json!({"keywords/$draft":null,"mailboxIds/mfefefefefefefefefefefefefefefefe":null,"mailboxIds/mfdfdfdfdfdfdfdfdfdfdfdfdfdfdfdfd":true})
        } else {
            td_json::json!({"keywords/$draft":null})
        };
        let mut input = td_json::json!({"using":[CORE,MAIL,SUBMISSION],"createdIds":{"draft":email},"methodCalls":[["EmailSubmission/set",{
            "accountId":account,"ifInState":state.encode(&mut [0;DataState::WIRE_BYTES]).unwrap(),
            "create":{"send":{"emailId":"#draft","identityId":identity}},
            "onSuccessUpdateEmail":{"#send":patch}
        },"send\n☃"]]});
        if batch {
            let _ = input.insert("createdIds", td_json::json!({}));
            let mailbox = td_mta::ids::MailboxId::parse(&sql(
                fixture,
                "SELECT encode(id,'hex') FROM td_mta.mailboxes ORDER BY id LIMIT 1",
            ))
            .unwrap();
            let mailbox = td_mta::wire::encode_id(mailbox, &mut buffer).unwrap();
            let draft = td_json::json!({
                "mailboxIds":{mailbox:true},"keywords":{"$draft":true,"$seen":true},
                "from":[{"email":binding.identity_email(source.identity()).unwrap()}],
                "to":[{"email":"recipient@example.test"}],
                "subject":"PostgreSQL send batch", "messageId":["postgres-send-batch@example.test"],
                "sentAt":"2026-10-15T12:00:00Z",
                "textBody":[{"partId":"text","type":"text/plain"}],
                "bodyValues":{"text":{"value":"Send batch body","isEncodingProblem":false,"isTruncated":false}}
            });
            let td_json::Json::Arr(calls) = input.get_mut("methodCalls").unwrap() else {
                panic!("calls");
            };
            let td_json::Json::Arr(call) = &mut calls[0] else {
                panic!("call");
            };
            let _ = call[1].insert("ifInState", td_json::Json::Null);
            calls.insert(0, td_json::json!(["Email/set", {"accountId":account,"create":{"draft":draft}}, "draft"]));
        }
        let input = input.to_string();
        let limits = Limits::default();
        let mut bytes = vec![0; limits.json_bytes].into_boxed_slice();
        bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let mut parser = td_json::arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            td_json::arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        let resources = limits.plan().unwrap();
        let mut validation = OwnedValidation::new(
            parser.finish().unwrap(),
            &resources,
            &[CORE, MAIL, SUBMISSION],
        )
        .unwrap();
        while !validation.step().unwrap() {}
        let request = validation.finish().unwrap();
        let directory = fixture
            .root
            .join(format!("submission-set-{refused}-{batch}"));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let mut root = PrivateRoot::open(directory.to_str().unwrap())
            .unwrap()
            .try_lock()
            .unwrap();
        let clock = Arc::new(RuntimeClock::new());
        let until = Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap();
        let execution = Deadline::after(Tick(0), until.tick().0 - 1).unwrap();
        let spool = RequestSpool::open(&mut root, clock.clone(), until).unwrap();
        let disk = DiskLimits::default()
            .plan(
                &resources,
                WorkLimits::default(),
                ViewMode::OnlineBackground,
            )
            .unwrap();
        let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
        let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
        let owner =
            ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
        let retention = owner.retain_response(until).unwrap();
        let mut preflight =
            response::Preflight::new(request, "session", retention.maximum()).unwrap();
        while !preflight.step(&mut [0; 4096]).unwrap() {}
        let creator = sending::Submissions {
            loaded,
            executable,
            binding,
            slots: &slots,
            clock: &clock,
            pending: &pending,
            password,
        };
        let filer = filing::Updates {
            loaded,
            executable,
            binding,
            slots: &slots,
            clock: &clock,
            pending: &filing_pending,
            password,
        };
        let access = dispatch::SubmissionAccess {
            creator: &creator,
            canceler: None,
            filer: &filer,
            clock: &*clock,
            device: source.device(),
            state,
        };
        let drafts = td_mta::postgres_http_drafts::Drafts {
            loaded,
            executable,
            binding,
            slots: &slots,
            clock: &clock,
            pending: &draft_pending,
            password,
            maximum_text: 1024 * 1024,
        };
        let plan = preflight.finish().unwrap();
        let mut completed = if batch {
            dispatch::run_with_postgres_send(plan, retention, execution, access, &drafts, &filer)
        } else {
            dispatch::run_with_postgres_submissions(plan, retention, execution, access)
        }
        .unwrap();
        assert_eq!(completed.responses().len(), if batch { 3 } else { 2 });
        let mut encoder = completed.encoder().unwrap();
        while !encoder.count_step(&mut [0; 4096]).unwrap() {}
        let length = encoder.begin_output().unwrap();
        let mut output = Vec::new();
        loop {
            let mut bytes = [0; 4096];
            let progress = encoder.poll_output(&mut bytes).unwrap();
            output.extend_from_slice(&bytes[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(encoder.finish().unwrap(), length);
        let output = td_json::parse_slice(&output).unwrap();
        let responses = output["methodResponses"].as_array().unwrap();
        assert_eq!(responses.len(), if batch { 3 } else { 2 });
        let email = if batch {
            assert_eq!(responses[0][0].as_str(), Some("Email/set"));
            responses[0][1]["created"]["draft"]["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{output}"))
        } else {
            email.as_str()
        };
        let responses = &responses[usize::from(batch)..];
        assert_eq!(responses[0][0].as_str(), Some("EmailSubmission/set"));
        let submitted: td_mta::ids::SubmissionId =
            td_mta::wire::Id::parse(responses[0][1]["created"]["send"]["id"].as_str().unwrap())
                .unwrap()
                .local()
                .unwrap();
        assert_eq!(responses[1][0].as_str(), Some("Email/set"));
        assert_eq!(responses[0][2].as_str(), Some("send\n☃"));
        assert_eq!(responses[0][2], responses[1][2]);
        if refused {
            assert_eq!(
                responses[1][1]["notUpdated"][email]["type"].as_str(),
                Some("invalidProperties")
            );
        } else {
            assert!(responses[1][1]["updated"].get(email).unwrap().is_null());
        }
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.submissions")
                .parse::<usize>()
                .unwrap(),
            before + 1
        );
        assert_eq!(sql(fixture,&format!("SELECT submitting_profile||':'||route_name FROM td_mta.submission_routes WHERE submission_id=decode('{submitted}','hex')")),"desktop:personal-smarthost");
        assert_eq!(pending.operation().unwrap(), None);
        assert_eq!(filing_pending.operation().unwrap(), None);
        assert_eq!(draft_pending.operation().unwrap(), None);
        drop(completed);
        assert_eq!(owner.used().unwrap(), 0);
    }
}

fn exercise_http_upload_owner(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        ports::{Clock, Deadline, Error},
        postgres_devices as devices, postgres_http_uploads as owner,
        postgres_read::{ReadSlots, Snapshot},
        postgres_uploads as changes,
    };
    let creation =
        devices::prepare_creation(binding.epoch(), binding.account(), "upload owner", 1).unwrap();
    devices::publish_creation(request, trust, deadline(), &creation).unwrap();
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let clock = RuntimeClock::new();
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let meter = || {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: 1024 * 1024,
                output_bytes: 1024,
                records: 4096,
                ..Charge::default()
            },
        )
    };
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let pending = owner::Pending::default();
    let worker = owner::Uploads {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &pending,
        password: creation.password(),
    };
    let input = |ms| owner::Input {
        account: binding.account(),
        device: creation.device_id(),
        bytes: b"bounded uploaded bytes",
        deadline: until(ms),
    };
    let mut exhausted = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker.create(input(10_000), &mut exhausted),
        Err(owner::Error::Admission(Error::Capacity))
    );
    assert!(exhausted.stopped().is_some());
    assert_eq!(pending.operation().unwrap(), None);
    let first = worker.create(input(10_000), &mut meter()).unwrap();
    assert_eq!(first.length, b"bounded uploaded bytes".len() as u64);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts"),
        "1"
    );
    sql(fixture, "CREATE FUNCTION public.reject_owned_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject upload'; END $$; CREATE TRIGGER reject_owned_upload BEFORE INSERT ON td_mta.upload_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_owned_upload()");
    assert_eq!(
        worker.create(input(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Database))
    );
    assert_eq!(pending.operation().unwrap(), None);
    sql(fixture, "DROP TRIGGER reject_owned_upload ON td_mta.upload_receipts; CREATE FUNCTION public.delay_owned_upload() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_owned_upload AFTER INSERT ON td_mta.upload_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_owned_upload()");
    assert_eq!(
        worker.create(input(3000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Indeterminate))
    );
    let operation = pending.operation().unwrap().unwrap();
    assert_eq!(
        worker.create(input(10_000), &mut meter()),
        Err(owner::Error::Admission(Error::Busy))
    );
    assert_eq!(pending.operation().unwrap(), Some(operation));
    thread::sleep(Duration::from_secs(5));
    sql(
        fixture,
        "DROP TRIGGER delay_owned_upload ON td_mta.upload_receipts",
    );
    let mut connection = loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let foreground = Snapshot::open_runtime(
        &slots,
        &connection,
        &loaded.trust,
        deadline(),
        binding,
        creation.password(),
    )
    .unwrap();
    let settled = pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(settled.outcome.length, first.length);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts"),
        "2"
    );
    drop(foreground);
    sql(fixture, "CREATE CONSTRAINT TRIGGER reject_owned_upload AFTER INSERT ON td_mta.upload_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.reject_owned_upload()");
    assert_eq!(
        worker.create(input(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Indeterminate))
    );
    let absent = pending.operation().unwrap().unwrap();
    assert!(pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .is_none());
    assert_eq!(pending.operation().unwrap(), Some(absent));
    assert_eq!(
        worker.create(input(10_000), &mut meter()),
        Err(owner::Error::Admission(Error::Busy))
    );
    sql(
        fixture,
        "DROP TRIGGER reject_owned_upload ON td_mta.upload_receipts",
    );
    let revoked =
        devices::prepare_revocation(binding.epoch(), binding.account(), creation.device_id(), 2)
            .unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoked).unwrap();
    let fresh = owner::Pending::default();
    let worker = owner::Uploads {
        pending: &fresh,
        ..worker
    };
    assert_eq!(
        worker.create(input(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Unauthenticated))
    );
    assert_eq!(fresh.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.upload_receipts"),
        "2"
    );
}

fn exercise_smtp_storage(fixture: &Fixture, binding: &td_mta::postgres_runtime_profile::Binding) {
    use std::{num::NonZeroU32, sync::Arc};
    use td_mta::{
        clock::RuntimeClock,
        config::{
            routing::{AliasSlot, Builder, DomainSlot},
            syntax::Location,
        },
        format::row::ReceiptTls,
        ports::{Clock, CommitFailure, Deadline, Error},
        postgres_read::ReadSlots,
        postgres_smtp_receiving::{Pending, Receiving},
        postgres_smtp_storage::Backend,
        smtp_session::{Pending as Action, Session, Settings},
        smtp_storage::{Begin, Failure, Job, Storage},
    };
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let clock = Arc::new(RuntimeClock::new());
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let slots = ReadSlots::new(1).unwrap();
    let pending = Pending::default();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let storage = Backend::new(
        Receiving {
            loaded: &loaded,
            executable,
            binding,
            additional: &[],
            slots: &slots,
            clock: &clock,
            pending: &pending,
        },
        clock.clone(),
        7,
    )
    .unwrap();
    let mut text = [0; 1024];
    let mut domains = [DomainSlot::EMPTY; 1];
    let mut aliases = [AliasSlot::EMPTY; 1];
    let at = Location {
        line: NonZeroU32::new(1).unwrap(),
        column: 1,
    };
    let mut builder = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
    builder.account(binding.account(), at).unwrap();
    builder.domain("example.test", at).unwrap();
    builder
        .alias("alice@example.test", binding.account(), at)
        .unwrap();
    let routes = builder.finish().unwrap();
    let initial: u64 = sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts")
        .parse()
        .unwrap();
    let mut random = td_crypto::SystemEntropy::try_new().unwrap();
    let mut prior_pid = None;
    for case in 0..4 {
        let mut session = Session::new(
            &routes,
            Settings {
                hostname: "mail.example.test",
                message_bytes: 32768,
                trace_bytes: 4096,
                recipients: 100,
                starttls: false,
            },
        )
        .unwrap();
        session.reply_sent().unwrap();
        for line in [
            "EHLO sender.example.test\r\n",
            "MAIL FROM:<sender@example.test>\r\n",
            "RCPT TO:<alice@example.test>\r\n",
        ] {
            assert_eq!(session.feed(line.as_bytes()).unwrap(), line.len());
            assert!(
                matches!(session.pending(), Action::Reply { bytes, .. } if bytes.starts_with(b"250"))
            );
            session.reply_sent().unwrap();
        }
        session.feed(b"DATA\r\n").unwrap();
        let deadline = until(if case == 3 { 3000 } else { 10_000 });
        let begin = |clock: Arc<dyn Clock>, generation| Begin {
            config_generation: generation,
            peer: "192.0.2.2".parse().unwrap(),
            tls: ReceiptTls::Plain,
            clock,
            session: &session,
            header_bytes: 16384,
            deadline,
        };
        assert!(matches!(
            storage.begin(begin(Arc::new(RuntimeClock::new()), 7), &mut random),
            Err(Failure::Refused(Error::Forbidden))
        ));
        assert!(matches!(
            storage.begin(begin(clock.clone(), 8), &mut random),
            Err(Failure::Refused(Error::Forbidden))
        ));
        for _ in 0..td_mta::limits::SMTP_DATA_JOBS {
            let mut expired = begin(clock.clone(), 7);
            expired.deadline = until(0);
            assert!(matches!(
                storage.begin(expired, &mut random),
                Err(Failure::Refused(Error::Deadline))
            ));
        }
        let mut job = storage.begin(begin(clock.clone(), 7), &mut random).unwrap();
        let pid: i32 = sql(fixture, "SELECT pid FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND usename='mail_runtime' AND state='idle'").parse().unwrap();
        assert_eq!(sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND usename='mail_runtime' AND state LIKE 'idle in transaction%'"), "0");
        if case == 1 {
            assert_eq!(Some(pid), prior_pid);
        }
        if case == 2 {
            assert_ne!(Some(pid), prior_pid);
        }
        if case == 0 {
            prior_pid = Some(pid);
        }
        std::thread::scope(|scope| {
            assert_eq!(
                scope.spawn(|| storage.maintain()).join().unwrap(),
                Err(Error::Invalid)
            );
        });

        if case == 2 {
            let generation = sql(
                fixture,
                "SELECT encode(generation,'hex') FROM td_mta.config_selection WHERE singleton=true",
            );
            sql(fixture, "UPDATE td_mta.config_selection SET generation=decode('000000000000ffff','hex') WHERE singleton=true");
            assert!(matches!(
                storage.begin(begin(clock.clone(), 7), &mut random),
                Err(Failure::Refused(Error::Forbidden))
            ));
            sql(fixture, &format!("UPDATE td_mta.config_selection SET generation=decode('{generation}','hex') WHERE singleton=true"));
        }
        let mut others = Vec::new();
        for _ in 1..td_mta::limits::SMTP_DATA_JOBS {
            others.push(storage.begin(begin(clock.clone(), 7), &mut random).unwrap());
        }
        assert!(matches!(
            storage.begin(begin(clock.clone(), 7), &mut random),
            Err(Failure::Retry)
        ));
        drop(others.pop());
        others.push(storage.begin(begin(clock.clone(), 7), &mut random).unwrap());
        if case == 0 {
            drop(job);
            assert_eq!(
                sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
                initial.to_string()
            );
            continue;
        }
        if case == 1 {
            assert_eq!(job.write(b"invalid"), Err(Failure::Refused(Error::Invalid)));
            assert_eq!(
                job.write(b"valid\r\n"),
                Err(Failure::Refused(Error::Invalid))
            );
            assert_eq!(
                job.prepare(until(10_000)),
                Err(Failure::Refused(Error::Invalid))
            );
            drop(job);
            assert_eq!(
                sql(
                    fixture,
                    &format!("SELECT pg_catalog.pg_terminate_backend({pid})")
                ),
                "t"
            );
            continue;
        }
        session.data_ready(Ok(())).unwrap();
        assert!(
            matches!(session.pending(), Action::Reply { bytes, .. } if bytes.starts_with(b"354"))
        );
        session.reply_sent().unwrap();
        for line in [
            b"Subject: SMTP PostgreSQL\r\n".as_slice(),
            b"\r\n",
            b"body\r\n",
        ] {
            assert_eq!(session.feed(line).unwrap(), line.len());
            let Action::Data(bytes) = session.pending() else {
                panic!("expected DATA")
            };
            job.write(bytes).unwrap();
            session.data_written(Ok(())).unwrap();
        }
        assert_eq!(
            sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
            (initial + u64::from(case == 3)).to_string()
        );
        assert_eq!(sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres' AND usename='mail_runtime' AND state LIKE 'idle in transaction%'"), "0");
        session.feed(b".\r\n").unwrap();
        assert!(matches!(session.pending(), Action::Commit));
        job.prepare(until(10_000)).unwrap();
        if case == 3 {
            // A separately admitted and prepared job cannot publish after an
            // uncertain sibling, even when historical settlement later succeeds.
            let other = others.first_mut().unwrap();
            other.write(b"Subject: fenced sibling\r\n").unwrap();
            other.write(b"\r\n").unwrap();
            other.write(b"body\r\n").unwrap();
            other.prepare(until(10_000)).unwrap();
            sql(fixture, "CREATE FUNCTION public.delay_storage_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_storage_receiving AFTER INSERT ON td_mta.receiving_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_storage_receiving()");
        }
        let completed = job.commit().unwrap();
        assert!(matches!(
            job.commit(),
            Err(Failure::Refused(Error::Conflict))
        ));
        drop(job);
        if case == 2 {
            assert!(!completed.stop);
            let commit = completed.outcome.unwrap();
            assert_eq!(commit.iter().count(), 1);
            assert_eq!(commit.iter().next().unwrap().account, binding.account());
            assert_eq!(commit.iter().next().unwrap().epoch, binding.epoch());
            session.committed(Ok(commit)).unwrap();
            assert!(
                matches!(session.pending(), Action::Reply { bytes, .. } if bytes.starts_with(b"250"))
            );
            assert_eq!(
                sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
                (initial + 1).to_string()
            );
        } else {
            assert!(completed.stop && storage.stopped());
            assert!(matches!(
                completed.outcome,
                Err(CommitFailure::Indeterminate(_))
            ));
            session.committed(completed.outcome).unwrap();
            assert!(matches!(session.pending(), Action::Closed));
            assert_eq!(
                storage.ready(binding.account(), until(10_000)),
                Err(Error::WriterStopped)
            );
            assert!(pending.operation().unwrap().is_some());
            let other = others.first_mut().unwrap();
            assert!(matches!(
                other.commit(),
                Err(Failure::Refused(Error::WriterStopped))
            ));
            assert_eq!(
                others.get_mut(1).unwrap().write(b"body\r\n"),
                Err(Failure::Refused(Error::WriterStopped))
            );
            assert_eq!(
                others.get_mut(2).unwrap().prepare(until(10_000)),
                Err(Failure::Refused(Error::WriterStopped))
            );
            thread::sleep(Duration::from_secs(5));
            sql(
                fixture,
                "DROP TRIGGER delay_storage_receiving ON td_mta.receiving_receipts",
            );
            let settlement = ReadSlots::new(1).unwrap();
            assert!(pending
                .settle(&loaded, executable, &settlement, &clock, until(10_000))
                .unwrap()
                .is_some());
            assert_eq!(
                sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
                (initial + 2).to_string()
            );
            assert!(storage.stopped());
            assert!(matches!(
                others.first_mut().unwrap().commit(),
                Err(Failure::Refused(Error::WriterStopped))
            ));
        }
    }
    storage.close().unwrap();
    Receiving {
        loaded: &loaded,
        executable,
        binding,
        additional: &[],
        slots: &slots,
        clock: &clock,
        pending: &pending,
    }
    .ready(until(10_000))
    .unwrap();
}

fn exercise_smtp_receiving_owner(
    fixture: &Fixture,
    binding: &td_mta::postgres_runtime_profile::Binding,
) {
    use td_mta::{
        admission::work::{Charge, Meter},
        clock::RuntimeClock,
        format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
        ports::{Clock, Deadline, Error},
        postgres_read::ReadSlots,
        postgres_receiving as changes, postgres_smtp_receiving as owner,
    };
    let loaded = td_mta::postgres_input::Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let clock = RuntimeClock::new();
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let meter = || {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: 1024 * 1024,
                output_bytes: 1024 * 1024,
                records: 4096,
                ..Charge::default()
            },
        )
    };
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let pending = owner::Pending::default();
    let worker = owner::Receiving {
        loaded: &loaded,
        executable,
        binding,
        additional: &[],
        slots: &slots,
        clock: &clock,
        pending: &pending,
    };
    worker.ready(until(10_000)).unwrap();
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "0"
    );
    let inbox = sql(
        fixture,
        "SELECT encode(id,'hex') FROM td_mta.mailboxes WHERE role='inbox'",
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET role=NULL WHERE role='inbox'",
    );
    assert_eq!(
        worker.ready(until(10_000)),
        Err(owner::Error::Publication(changes::Error::MissingMailbox))
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.mailboxes SET role='inbox' WHERE id=decode('{inbox}','hex')"),
    );
    let state = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let changed = sql(
        fixture,
        &format!(
            "SELECT encode(changed,'hex') FROM td_mta.mailboxes WHERE id=decode('{inbox}','hex')"
        ),
    );
    sql(fixture, &format!("UPDATE td_mta.mailboxes SET changed=decode('ffffffffffffffff','hex') WHERE id=decode('{inbox}','hex')"));
    assert_eq!(
        worker.ready(until(10_000)),
        Err(owner::Error::Publication(changes::Error::Integrity))
    );
    sql(fixture, &format!("UPDATE td_mta.mailboxes SET changed=decode('{changed}','hex') WHERE id=decode('{inbox}','hex')"));
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        state
    );
    worker.ready(until(10_000)).unwrap();
    let mut recipient_bytes = [0; 1024];
    let recipients =
        ReceiptRecipients::encode(&["alice@example.test"], &mut recipient_bytes).unwrap();
    let message = changes::Message {
        bytes: b"Subject: retained receiving\r\n\r\nbody\r\n",
        receipt: SmtpReceipt {
            peer: "192.0.2.1".parse().unwrap(),
            gateway: None,
            tls: ReceiptTls::Tls13,
            ehlo: "sender.example.test",
            reverse_path: "sender@example.test",
            recipients,
        },
        hostname: "mail.example.test",
        extended: true,
        header_bytes: binding.header_maximum(),
        received_at: 1_700_000_000_000,
    };
    let mut exhausted = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker.create(&message, until(10_000), &mut exhausted),
        Err(owner::Error::Admission(Error::Capacity))
    );
    assert!(exhausted.stopped().is_some());
    assert_eq!(pending.operation().unwrap(), None);
    let first = worker
        .create(&message, until(10_000), &mut meter())
        .unwrap();
    assert!(first.length > message.bytes.len() as u64);
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "1"
    );
    sql(fixture, "CREATE FUNCTION public.reject_owned_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'reject receiving'; END $$; CREATE TRIGGER reject_owned_receiving BEFORE INSERT ON td_mta.receiving_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_owned_receiving()");
    assert_eq!(
        worker.create(&message, until(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Database))
    );
    assert_eq!(pending.operation().unwrap(), None);
    sql(fixture, "DROP TRIGGER reject_owned_receiving ON td_mta.receiving_receipts; CREATE FUNCTION public.delay_owned_receiving() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(4); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_owned_receiving AFTER INSERT ON td_mta.receiving_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_owned_receiving()");
    assert_eq!(
        worker.create(&message, until(3000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Indeterminate))
    );
    let operation = pending.operation().unwrap().unwrap();
    assert_eq!(
        worker.ready(until(10_000)),
        Err(owner::Error::Admission(Error::Busy))
    );
    assert_eq!(
        worker.create(&message, until(10_000), &mut meter()),
        Err(owner::Error::Admission(Error::Busy))
    );
    assert_eq!(pending.operation().unwrap(), Some(operation));
    thread::sleep(Duration::from_secs(5));
    sql(
        fixture,
        "DROP TRIGGER delay_owned_receiving ON td_mta.receiving_receipts",
    );
    let settled = pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(
        settled.outcome.accounts.first().unwrap().1.length,
        first.length
    );
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "2"
    );
    sql(fixture, "CREATE CONSTRAINT TRIGGER reject_owned_receiving AFTER INSERT ON td_mta.receiving_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.reject_owned_receiving()");
    assert_eq!(
        worker.create(&message, until(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Indeterminate))
    );
    let absent = pending.operation().unwrap().unwrap();
    assert!(pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .is_none());
    assert_eq!(pending.operation().unwrap(), Some(absent));
    assert_eq!(
        worker.create(&message, until(10_000), &mut meter()),
        Err(owner::Error::Admission(Error::Busy))
    );
    sql(
        fixture,
        "DROP TRIGGER reject_owned_receiving ON td_mta.receiving_receipts",
    );
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    let fresh = owner::Pending::default();
    let worker = owner::Receiving {
        pending: &fresh,
        ..worker
    };
    assert_eq!(
        worker.ready(until(10_000)),
        Err(owner::Error::Publication(changes::Error::Policy))
    );
    assert_eq!(
        worker.create(&message, until(10_000), &mut meter()),
        Err(owner::Error::Publication(changes::Error::Policy))
    );
    assert_eq!(fresh.operation().unwrap(), None);
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.receiving_receipts"),
        "2"
    );
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.config_selection SET generation=decode('{:016x}','hex')",
            binding.selection().generation
        ),
    );
}

#[path = "postgres_probe/queue_expiry.rs"]
mod queue_expiry;

#[path = "postgres_probe/route_health.rs"]
mod route_health;

#[path = "postgres_probe/route_repair.rs"]
mod route_repair;

#[path = "postgres_probe/route_success.rs"]
mod route_success;

#[path = "postgres_probe/submission_cancel.rs"]
mod submission_cancel;

#[path = "postgres_probe/cancel_worker.rs"]
mod cancel_worker;

#[path = "postgres_probe/cancel_jmap.rs"]
mod cancel_jmap;

#[path = "postgres_probe/prepared_failure.rs"]
mod prepared_failure;
#[path = "postgres_probe/queue_candidates.rs"]
mod queue_candidates;

#[path = "postgres_probe/attempt_body.rs"]
mod attempt_body;

#[path = "postgres_probe/attempt_envelope.rs"]
mod attempt_envelope;

#[path = "postgres_probe/attempt_clock.rs"]
mod attempt_clock;

#[path = "postgres_probe/dispatch_worker.rs"]
mod dispatch_worker;

#[path = "postgres_probe/native_final_handoff.rs"]
mod native_final_handoff;
#[path = "postgres_probe/native_relay_attempt.rs"]
mod native_relay_attempt;
#[path = "postgres_probe/native_relay_connect.rs"]
mod native_relay_connect;
#[path = "postgres_probe/relay_wire.rs"]
mod relay_wire;

#[path = "postgres_probe/dispatch_recovery.rs"]
mod dispatch_recovery;

#[path = "postgres_probe/native_mail_exchange.rs"]
mod native_mail_exchange;

#[path = "postgres_probe/native_rcpt_exchange.rs"]
mod native_rcpt_exchange;

#[path = "postgres_probe/native_data_exchange.rs"]
mod native_data_exchange;

#[path = "postgres_probe/route_acquisition.rs"]
mod route_acquisition;

#[path = "postgres_probe/expiry_admission.rs"]
mod expiry_admission;

#[path = "postgres_probe/device_listing.rs"]
mod device_listing;

#[path = "postgres_probe/device_revocation.rs"]
mod device_revocation;

#[path = "postgres_probe/device_creation.rs"]
mod device_creation;

#[path = "postgres_probe/account_admin.rs"]
mod account_admin;

#[path = "postgres_probe/selection_commands.rs"]
mod selection_commands;

#[path = "postgres_probe/usage_audit.rs"]
mod usage_audit;

#[path = "postgres_probe/usage_commands.rs"]
mod usage_commands;

#[path = "postgres_probe/blob_audit.rs"]
mod blob_audit;

#[path = "postgres_probe/metadata_audit.rs"]
mod metadata_audit;

#[path = "postgres_probe/account_audit.rs"]
mod account_audit;

#[path = "postgres_probe/account_audit_commands.rs"]
mod account_audit_commands;

#[path = "postgres_probe/runtime_barrier.rs"]
mod runtime_barrier;

#[path = "postgres_probe/maintenance.rs"]
mod maintenance;

#[path = "postgres_probe/maintenance_commands.rs"]
mod maintenance_commands;

#[path = "postgres_probe/blob_audit_commands.rs"]
mod blob_audit_commands;

#[path = "postgres_probe/metadata_audit_commands.rs"]
mod metadata_audit_commands;

#[path = "postgres_probe/history_audit.rs"]
mod history_audit;

// Exercise a committed expiry-wait result, then restore this isolated fixture
// so subsequent rollback/unknown-commit cases can use their original attempt.
fn expiry_wait_snapshot(fixture: &Fixture, publish: impl FnOnce()) {
    let tables = sql(fixture,"SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname='td_mta' AND (tablename IN ('accounts','changes','submissions','recipients','submission_attempts') OR tablename LIKE 'attempt_%') ORDER BY tablename");
    for table in tables.lines() {
        sql(
            fixture,
            &format!("CREATE TABLE public.expiry_snapshot_{table} AS TABLE td_mta.{table}"),
        );
    }
    publish();
    assert_ne!(sql(fixture,"SELECT count(*) FROM td_mta.recipients r JOIN td_mta.submissions s ON s.account=r.account AND s.id=r.submission_id WHERE r.state IN (3,7) AND r.phase=4 AND r.next_attempt_at=s.expires_at"),"0");
    // Restore one consistent snapshot without transient parent/child FK conflicts.
    // This is the isolated superuser fixture, never the runtime connection.
    let mut restore = String::from("BEGIN; SET LOCAL session_replication_role=replica;");
    for table in tables.lines() {
        restore.push_str(&format!("DELETE FROM td_mta.{table}; INSERT INTO td_mta.{table} SELECT * FROM public.expiry_snapshot_{table}; DROP TABLE public.expiry_snapshot_{table};"));
    }
    restore.push_str("COMMIT;");
    sql(fixture, &restore);
}

#[path = "postgres_probe/application_logs.rs"]
mod application_logs;

#[path = "postgres_probe/acme.rs"]
mod acme;

#[path = "postgres_probe/recovery.rs"]
mod recovery;

#[path = "postgres_probe/account_workflow.rs"]
mod account_workflow;

#[test]
#[ignore = "real local PostgreSQL 17 tools required"]
fn real_postgres_account_administration_resumes_and_preserves_other_accounts() {
    exercise_configuration(ConfigurationExercise::AccountAdmin);
}

#[path = "postgres_probe/imports.rs"]
mod imports;
#[path = "postgres_probe/reconcile.rs"]
mod reconciliation;

#[test]
#[ignore = "real local PostgreSQL tools required"]
fn real_postgres_import_reconciliation() {
    exercise_configuration(ConfigurationExercise::Reconcile);
}
#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_imports_preserve_source_snapshots() {
    exercise_configuration(ConfigurationExercise::Imports);
    exercise_configuration(ConfigurationExercise::Reconcile);
}

#[path = "postgres_probe/unmapped.rs"]
mod unmapped;
#[test]
#[ignore = "real local PostgreSQL tools required"]
fn real_postgres_import_unmapped() {
    exercise_configuration(ConfigurationExercise::Unmapped);
}

#[path = "postgres_probe/folder_reconcile.rs"]
mod folder_reconcile;
#[test]
#[ignore = "real local PostgreSQL tools required"]
fn real_postgres_folder_reconciliation() {
    exercise_configuration(ConfigurationExercise::FolderReconcile);
}

#[path = "postgres_probe/audit_scaling.rs"]
mod audit_scaling;
