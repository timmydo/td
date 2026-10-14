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
