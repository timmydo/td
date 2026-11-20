//! Exercise the production provisioning commands, HBA overlay and scoped SQL on PostgreSQL 17.
use super::{add_hba, create_logins, grant_sql, hba_rules, private_hba, Passwords, ROLES};
use crate::{
    common::{random_hex, Host},
    postgres_fixture::{checked, run, wait, PgTools},
    R,
};
use std::{
    fs,
    net::TcpListener,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const PLAN: &str = r#"{
    "frontend":{"mail":"SELECT, UPDATE (label)"},
    "receive":{"mail":"INSERT"},
    "relay":{"mail":"SELECT (id,label)"},
    "migadu":{"mail":"SELECT (id)"},
    "acme":{"certificates":"SELECT, INSERT, UPDATE (der)"}
}"#;

struct Fixture {
    tools: PgTools,
    port: String,
    ca: PathBuf,
    admin: String,
}

impl Fixture {
    fn query(&self, database: &str, role: &str, password: &str, sql: &str) -> Command {
        let mut cmd = self.tools.cmd("psql");
        cmd.env("PGHOST", "localhost")
            .env("PGHOSTADDR", "127.0.0.1")
            .env("PGSSLMODE", "verify-full")
            .env("PGSSLROOTCERT", &self.ca)
            .env("PGPASSWORD", password)
            .args([
                "-XwqAt",
                "-p",
                &self.port,
                "-d",
                database,
                "-U",
                role,
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                sql,
            ]);
        cmd
    }

    fn owner(&self, sql: &str) -> Command {
        self.query("td_mta", "fixture_owner", &self.admin, sql)
    }

    fn service(&self, values: &Passwords, role: &str, sql: &str) -> R<Command> {
        Ok(self.query(
            "td_mta",
            &format!("td_mta_{role}"),
            values.get(role).ok_or("role")?,
            sql,
        ))
    }
}

fn denied(output: Output, expected: &str) {
    assert!(!output.status.success(), "query unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn candidate_hba_preserves_metadata_and_other_rules_on_reapplication() -> R<()> {
    let root = td_fs::TempDir::new("mta-hba")?;
    let path = root.path().join("pg_hba.conf");
    let original =
        "hostssl all fixture_owner 127.0.0.1/32 scram-sha-256\nhost all all 127.0.0.1/32 reject\n";
    fs::write(&path, original)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
    let before = fs::metadata(&path)?;
    add_hba(&path)?;
    let installed = fs::metadata(&path)?;
    assert_eq!(
        (installed.mode(), installed.uid(), installed.gid()),
        (before.mode(), before.uid(), before.gid())
    );
    let expected = format!("{}{original}", hba_rules()?);
    assert_eq!(fs::read_to_string(&path)?, expected);
    add_hba(&path)?;
    assert_eq!(fs::metadata(&path)?.ino(), installed.ino());
    assert_eq!(fs::read_to_string(&path)?, expected);
    Ok(())
}

fn conninfo_path(path: &Path) -> R<String> {
    let path = path.to_str().ok_or("non-UTF-8 fixture path")?;
    Ok(format!(
        "'{}'",
        path.replace('\\', "\\\\").replace('\'', "\\'")
    ))
}

#[test]
#[ignore = "requires PostgreSQL 17 tools, loopback TCP and a non-root process"]
fn real_postgres_provisions_five_scram_roles_and_atomically_replaces_grants() -> R<()> {
    if fs::File::open("/dev/tty").is_ok() {
        // Keep production psql directly supervised. Interactive test runners
        // can re-execute just this test with the same detached environment.
        let case = "service_database::postgres_test::real_postgres_provisions_five_scram_roles_and_atomically_replaces_grants";
        let report = checked(
            Command::new("setsid")
                .arg("--wait")
                .arg(std::env::current_exe()?)
                .args(["--exact", case, "--ignored", "--nocapture"])
                .stdin(std::process::Stdio::null()),
        )?;
        assert!(report
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed;")));
        return Ok(());
    }
    let tools = PgTools::new()?;
    let psql = tools.bin.join("psql");
    let temp = td_fs::TempDir::new("mta-scram")?;
    let root = temp.path();
    let data = root.join("pgdata");
    let pems = crate::tls_fixture::ServerPems::new("localhost")?;
    let passfile = root.join("pgpass");
    let initpass = root.join("initpass");
    let fixture = Fixture {
        tools,
        port: TcpListener::bind(("127.0.0.1", 0))?
            .local_addr()?
            .port()
            .to_string(),
        ca: root.join("ca.pem"),
        admin: random_hex(32)?,
    };
    fs::write(&initpass, &fixture.admin)?;
    fs::set_permissions(&initpass, fs::Permissions::from_mode(0o600))?;
    checked(
        fixture
            .tools
            .cmd("initdb")
            .args(["--no-sync", "--no-locale", "-E", "UTF8", "-D"])
            .arg(&data)
            .args(["-U", "fixture_owner", "--auth=scram-sha-256", "--pwfile"])
            .arg(&initpass),
    )?;
    fs::remove_file(initpass)?;
    fs::write(&fixture.ca, pems.root)?;
    fs::write(data.join("server.crt"), pems.chain)?;
    fs::write(data.join("server.key"), pems.key)?;
    fs::set_permissions(data.join("server.key"), fs::Permissions::from_mode(0o600))?;
    let persistent = data.join("pg_hba.conf");
    fs::write(
        &persistent,
        "hostssl all fixture_owner 127.0.0.1/32 scram-sha-256\nhost all all 127.0.0.1/32 reject\n",
    )?;
    add_hba(&persistent)?;
    let overlay = root.join("candidate-hba");
    fs::write(&overlay, private_hba(&persistent)?)?;
    let _server = run(
        fixture.tools.cmd("postgres").arg("-D").arg(&data).args([
            "-h",
            "127.0.0.1",
            "-p",
            &fixture.port,
            "-c",
            "unix_socket_directories=",
            "-c",
            "ssl=on",
            "-c",
            "log_statement=all",
            "-c",
            &format!("hba_file={}", overlay.display()),
        ]),
        &root.join("server.log"),
    )?;
    wait("TLS startup", || {
        checked(&mut fixture.query("postgres", "fixture_owner", &fixture.admin, "SELECT 1")).is_ok()
    })?;
    checked(&mut fixture.query(
        "postgres",
        "fixture_owner",
        &fixture.admin,
        "CREATE DATABASE td_mta",
    ))?;
    checked(&mut fixture.owner(
        "REVOKE ALL ON DATABASE td_mta FROM PUBLIC;
        CREATE SCHEMA td_mta;
        CREATE TABLE td_mta.mail(id integer,label text,body text);
        CREATE TABLE td_mta.certificates(id integer,der bytea);
        CREATE SCHEMA td_mta_service;
        CREATE VIEW td_mta_service.materials AS SELECT 1 AS value;
        CREATE VIEW td_mta_service.material_chunks AS SELECT 1 AS value;
        CREATE VIEW td_mta_service.active_certificates AS SELECT 1 AS value;",
    ))?;
    fs::write(
        &passfile,
        format!(
            "localhost:{}:*:fixture_owner:{}\n",
            fixture.port, fixture.admin
        ),
    )?;
    fs::set_permissions(&passfile, fs::Permissions::from_mode(0o600))?;
    let conn = format!("dbname=td_mta host=localhost hostaddr=127.0.0.1 sslmode=verify-full sslcertmode=disable port={} sslrootcert={} passfile={}",
        fixture.port, conninfo_path(&fixture.ca)?, conninfo_path(&passfile)?);
    let values = Passwords::new()?;
    let args = [
        "-Xwq",
        "-v",
        "ON_ERROR_STOP=1",
        "-U",
        "fixture_owner",
        "-d",
        &conn,
    ];
    // Verify the exact administrative connection before testing a DDL failure.
    checked(
        Host::default()
            .cmd(&psql)
            .args(args)
            .args(["-c", "SELECT 1"]),
    )?;
    checked(&mut fixture.owner("CREATE ROLE td_mta_acme"))?;
    assert!(create_logins(&Host::default(), &psql, &args, &values).is_err());
    assert_eq!(
        checked(&mut fixture.owner("SELECT rolname FROM pg_roles WHERE rolname LIKE 'td_mta_%'"))?,
        "td_mta_acme"
    );
    checked(&mut fixture.owner("DROP ROLE td_mta_acme"))?;
    // All ten password prompts use the same detached psql child and private stdin.
    create_logins(&Host::default(), &psql, &args, &values)?;
    let log = fs::read_to_string(root.join("server.log"))?;
    assert!(log.contains("CREATE ROLE td_mta_relay"));
    for &role in ROLES {
        assert!(
            !log.contains(values.get(role).ok_or("role password")?),
            "plaintext password in SQL log"
        );
    }
    assert_eq!(checked(&mut fixture.owner("SELECT count(*) FROM pg_authid WHERE rolname LIKE 'td_mta_%'
        AND rolcanlogin AND NOT (rolsuper OR rolcreatedb OR rolcreaterole OR rolinherit OR rolreplication OR rolbypassrls)
        AND rolpassword LIKE 'SCRAM-SHA-256$%'"))?, "5");
    let plan = td_json::parse(PLAN)?;
    checked(&mut fixture.owner(&grant_sql(&plan)?))?;
    for (role, neighbor) in ROLES.iter().zip(ROLES.iter().cycle().skip(1)) {
        let login = format!("td_mta_{role}");
        assert_eq!(
            checked(&mut fixture.service(&values, role, "SELECT current_user")?)?,
            login
        );
        denied(
            fixture
                .query(
                    "td_mta",
                    &login,
                    values.get(neighbor).ok_or("neighbor password")?,
                    "SELECT 1",
                )
                .output()?,
            "password authentication failed",
        );
        let query = fixture
            .service(&values, role, "SELECT value FROM td_mta_service.materials")?
            .output()?;
        if *role == "acme" {
            denied(query, "permission denied");
        } else {
            assert!(query.status.success());
        }
    }
    denied(
        fixture
            .service(&values, "receive", "SELECT 1")?
            .env("PGSSLMODE", "disable")
            .output()?,
        "rejects connection",
    );
    checked(&mut fixture.service(
        &values,
        "receive",
        "INSERT INTO td_mta.mail VALUES(1,'a','immutable')",
    )?)?;
    denied(
        fixture
            .service(&values, "receive", "SELECT body FROM td_mta.mail")?
            .output()?,
        "permission denied",
    );
    checked(&mut fixture.service(&values, "frontend", "UPDATE td_mta.mail SET label='b'")?)?;
    assert_eq!(
        checked(&mut fixture.service(&values, "relay", "SELECT id,label FROM td_mta.mail")?)?,
        "1|b"
    );
    denied(
        fixture
            .service(&values, "migadu", "SELECT label FROM td_mta.mail")?
            .output()?,
        "permission denied",
    );
    checked(&mut fixture.service(
        &values,
        "acme",
        "INSERT INTO td_mta.certificates VALUES(1,NULL); UPDATE td_mta.certificates SET der='test'",
    )?)?;

    let bad = td_json::parse(
        &PLAN
            .replace("SELECT, UPDATE (label)", "DELETE")
            .replace("certificates", "missing_table"),
    )?;
    denied(fixture.owner(&grant_sql(&bad)?).output()?, "does not exist");
    checked(&mut fixture.service(&values, "frontend", "UPDATE td_mta.mail SET label='c'")?)?;
    denied(
        fixture
            .service(&values, "frontend", "DELETE FROM td_mta.mail")?
            .output()?,
        "permission denied",
    );
    checked(&mut fixture.owner("GRANT UPDATE (body) ON td_mta.mail TO td_mta_frontend"))?;
    checked(&mut fixture.owner(&grant_sql(&plan)?))?;
    denied(
        fixture
            .service(
                &values,
                "frontend",
                "UPDATE td_mta.mail SET body='forbidden'",
            )?
            .output()?,
        "permission denied",
    );
    assert_eq!(
        checked(&mut fixture.service(&values, "frontend", "SELECT body FROM td_mta.mail")?)?,
        "immutable"
    );
    Ok(())
}
