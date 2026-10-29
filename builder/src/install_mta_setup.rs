//! Explicit host-only preparation of a fresh private local mail deployment.
use super::{absolute, directory, scheme, service_files, shell, systemd, text, write_new, Result};
use crate::json::{self, Json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const HELP: &str = "\
Usage: ./install-td-mta --setup-local --root PATH --account EMAIL
  --relay-host HOST --relay-user USER --relay-password-file PATH
  --resolver IP:PORT [--relay-port PORT] [--relay-ca-file PATH]
  [--postgres-port PORT] [--https-port PORT] [--smtp-port PORT] [--prefix PATH]

Requires an installed td-mta, PostgreSQL 17 tools and openssl on PATH.
Creates a NEW private directory; never adopts or overwrites existing data.
Defaults: prefix $HOME/.local; PostgreSQL 35432, HTTPS 54443, SMTP 55225;
relay 465 (implicit TLS). All local listeners bind IPv4 loopback only.
Provisioning briefly starts its own database, then stops it. No mail is sent.
Writes credentials, receipts, td-mail config and user service definitions.
It does not enable services, alter DNS/MX, or install PostgreSQL.
On failure, retain the directory and logs; there is no automatic retry/resume.
";

struct Settings {
    root: PathBuf,
    prefix: PathBuf,
    account: String,
    relay_host: String,
    relay_user: String,
    relay_password: PathBuf,
    relay_ca: Option<PathBuf>,
    resolver: SocketAddr,
    pg_port: u16,
    https_port: u16,
    smtp_port: u16,
    relay_port: u16,
    replica: Option<Replica>,
}
struct Replica {
    origin: String,
    username: String,
    password: PathBuf,
    ca: Option<PathBuf>,
    quotas: [u64; 4],
}
const REPLICA_HELP: &str = "Usage: ./install-td-mta --setup-replica --root PATH --account EMAIL
  --source-origin HTTPS_ORIGIN --password-file PATH --resolver IP:PORT
  --body-bytes N --blobs N --emails N --receipts N
  [--source-username USER] [--ca-file PATH] [--prefix PATH]
  [--postgres-port PORT] [--https-port PORT]

Creates a NEW private read-only replica with independent local credentials.
Requires installed td-mta, PostgreSQL 17 tools and openssl on PATH.
Defaults: PostgreSQL 35433, HTTPS 54444; loopback only.
Source username defaults to --account, which is also the local login/address.
Discovers and seeds the source account; the sync service continues copying.
Writes PostgreSQL, read-only and sync service definitions, but enables none.
No SMTP, relay or ACME service is generated. Retain receipts on failure.
";

fn atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 254
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@+-".contains(&b))
}

fn settings(args: &[String], home: &str) -> Result<Settings> {
    settings_mode(args, home, false)
}
fn settings_mode(args: &[String], home: &str, replica: bool) -> Result<Settings> {
    let mut values = BTreeMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let common = [
            "--root",
            "--prefix",
            "--account",
            "--resolver",
            "--postgres-port",
            "--https-port",
        ]
        .contains(&flag.as_str());
        let mode_option = if replica {
            [
                "--source-origin",
                "--source-username",
                "--password-file",
                "--ca-file",
                "--body-bytes",
                "--blobs",
                "--emails",
                "--receipts",
            ]
            .contains(&flag.as_str())
        } else {
            [
                "--relay-host",
                "--relay-user",
                "--relay-password-file",
                "--relay-ca-file",
                "--smtp-port",
                "--relay-port",
            ]
            .contains(&flag.as_str())
        };
        if !common && !mode_option {
            return Err(format!("unknown setup option: {flag}"));
        }
        let value = it
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        if values.insert(flag.as_str(), value.as_str()).is_some() {
            return Err(format!("duplicate setup option: {flag}"));
        }
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing {key}"))
    };
    let port = |key, default| -> Result<u16> {
        let n = match values.get(key) {
            Some(v) => v.parse::<u16>().map_err(|_| format!("invalid {key}"))?,
            None => default,
        };
        if n == 0 || (key != "--relay-port" && n < 1024) {
            return Err(format!(
                "{key} must be nonzero; local ports must be unprivileged"
            ));
        }
        Ok(n)
    };
    let account = required("--account")?;
    let host = if replica {
        "localhost"
    } else {
        required("--relay-host")?
    };
    let user = if replica {
        "disabled"
    } else {
        required("--relay-user")?
    };
    if !atom(account)
        || account.split('@').count() != 2
        || account.starts_with('@')
        || account.ends_with('@')
        || !atom(host)
        || host.contains('@')
        || !atom(user)
    {
        return Err("account, relay hostname and user must be simple ASCII names".into());
    }
    let s = Settings {
        root: absolute(required("--root")?)?,
        prefix: absolute(values.get("--prefix").copied().unwrap_or(home))?,
        account: account.into(),
        relay_host: host.into(),
        relay_user: user.into(),
        relay_password: if replica {
            absolute(required("--root")?)?.join("relay-password")
        } else {
            absolute(required("--relay-password-file")?)?
        },
        relay_ca: values
            .get("--relay-ca-file")
            .map(|v| absolute(v))
            .transpose()?,
        resolver: required("--resolver")?
            .parse()
            .map_err(|_| "resolver must be IP:PORT")?,
        pg_port: port("--postgres-port", if replica { 35433 } else { 35432 })?,
        https_port: port("--https-port", if replica { 54444 } else { 54443 })?,
        smtp_port: port("--smtp-port", 55225)?,
        relay_port: port("--relay-port", 465)?,
        replica: if replica {
            let mut quotas = [0; 4];
            for (slot, name) in
                quotas
                    .iter_mut()
                    .zip(["--body-bytes", "--blobs", "--emails", "--receipts"])
            {
                let value = required(name)?;
                *slot = value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid {name}"))?;
                if !value.bytes().all(|b| b.is_ascii_digit())
                    || *slot == 0
                    || *slot > i64::MAX as u64
                {
                    return Err(format!("{name} must be positive and at most {}", i64::MAX));
                }
            }
            Some(Replica {
                origin: required("--source-origin")?.into(),
                username: values
                    .get("--source-username")
                    .copied()
                    .unwrap_or(account)
                    .into(),
                password: absolute(required("--password-file")?)?,
                ca: values.get("--ca-file").map(|v| absolute(v)).transpose()?,
                quotas,
            })
        } else {
            None
        },
    };
    if s.pg_port == s.https_port
        || (s.replica.is_none() && (s.pg_port == s.smtp_port || s.https_port == s.smtp_port))
    {
        return Err("local ports must be distinct".into());
    }
    Ok(Settings {
        prefix: if values.contains_key("--prefix") {
            s.prefix
        } else {
            s.prefix.join(".local")
        },
        ..s
    })
}

pub(super) fn random_hex(bytes: usize) -> Result<String> {
    let mut value = vec![0; bytes];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut value))
        .map_err(|e| format!("random input: {e}"))?;
    Ok(value.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn protected_input(path: &Path, limit: u64) -> Result<Vec<u8>> {
    trusted_ancestors(path.parent().ok_or("missing input parent")?)?;
    read_private_input(path, limit)
}

pub(super) fn trusted_ancestors(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err("private paths must be absolute".into());
    }
    for ancestor in path.ancestors() {
        let m = fs::symlink_metadata(ancestor).map_err(|e| e.to_string())?;
        if !m.is_dir() || (m.uid() != 0 && m.uid() != crate::sys::getuid()) || m.mode() & 0o022 != 0
        {
            return Err(format!(
                "untrusted physical ancestor: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn read_private_input(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("input {}: {e}", path.display()))?;
    if !meta.is_file()
        || meta.uid() != crate::sys::getuid()
        || meta.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "{} must be a private regular file owned by this user",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| format!("open private input: {e}"))?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    if !opened.is_file()
        || opened.dev() != meta.dev()
        || opened.ino() != meta.ino()
        || opened.uid() != meta.uid()
        || opened.mode() != meta.mode()
    {
        return Err("private input changed while opening".into());
    }
    let mut data = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|e| format!("read protected input: {e}"))?;
    if data.is_empty() || data.len() as u64 > limit {
        return Err("invalid protected input size".into());
    }
    Ok(data)
}

pub(super) fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    crate::sandbox::die_with_parent(&mut cmd);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PG") {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove("OPENSSL_CONF")
        .env_remove("OPENSSL_MODULES")
        .stdin(Stdio::null());
    cmd
}

struct Setup<'a> {
    s: &'a Settings,
    serial: usize,
}
impl Setup<'_> {
    fn path(&self, name: &str) -> PathBuf {
        self.s.root.join(name)
    }
    fn write(&self, name: &str, data: impl AsRef<[u8]>) -> Result<()> {
        let path = self.path(name);
        write_new(&path, data.as_ref())?;
        File::open(&path)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        File::open(path.parent().ok_or("missing output parent")?)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())
    }
    fn execute(&mut self, label: &str, cmd: &mut Command, input: Option<&[u8]>) -> Result<String> {
        self.serial += 1;
        let base = format!("receipts/{:02}-{label}", self.serial);
        self.write(&format!("{base}.out"), b"")?;
        self.write(&format!("{base}.err"), b"")?;
        let out_path = self.path(&format!("{base}.out"));
        let err_path = self.path(&format!("{base}.err"));
        let out = fs::OpenOptions::new()
            .write(true)
            .open(&out_path)
            .map_err(|e| e.to_string())?;
        let err = fs::OpenOptions::new()
            .write(true)
            .open(&err_path)
            .map_err(|e| e.to_string())?;
        cmd.stdout(out).stderr(err);
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().map_err(|e| format!("{label}: {e}"))?;
        if let Some(bytes) = input {
            let written = child
                .stdin
                .take()
                .ok_or("missing child input")
                .and_then(|mut f| f.write_all(bytes).map_err(|_| "write child input"));
            if written.is_err() {
                let _ = crate::sys::kill_child_recorded(
                    &mut child,
                    "local mail setup: failed writing private tool input",
                );
                let _ = child.wait();
                return Err(format!(
                    "{label}: failed writing private input; see {}",
                    err_path.display()
                ));
            }
        }
        let status = child.wait().map_err(|e| format!("{label}: {e}"))?;
        if !status.success() {
            return Err(format!(
                "{label} failed; inspect private receipts {}",
                self.path(&base).display()
            ));
        }
        fs::read_to_string(&out_path).map_err(|e| format!("{label} output: {e}"))
    }
    fn mta(&mut self, label: &str, args: &[&str]) -> Result<Json> {
        let mut cmd = command(&self.s.prefix.join("bin/td-mta"));
        cmd.arg("postgres").args(args);
        let output = self.execute(label, &mut cmd, None)?;
        json::parse(&output).map_err(|_| format!("{label}: invalid JSON; retain receipts"))
    }
    fn owner(&mut self, label: &str, args: &[&str]) -> Result<Json> {
        let connection = self.path("owner-connection");
        let mut full = args.to_vec();
        full.extend(["--connection-file", text(&connection)?]);
        self.mta(label, &full)
    }
    fn sql(&mut self, label: &str, sql: &str) -> Result<()> {
        let socket = self.path("socket");
        let mut cmd = command(Path::new("psql"));
        cmd.args(["-X", "--no-password", "-v", "ON_ERROR_STOP=1"])
            .args([
                "-h",
                text(&socket)?,
                "-p",
                &self.s.pg_port.to_string(),
                "-U",
                "setup_admin",
                "-d",
                "postgres",
            ])
            .env("PGCONNECT_TIMEOUT", "2");
        self.execute(label, &mut cmd, Some(sql.as_bytes()))?;
        Ok(())
    }
}

fn field(json: &Json, key: &str) -> Result<String> {
    json.get(key)
        .and_then(Json::as_str)
        .map(String::from)
        .ok_or_else(|| format!("missing {key} in retained command receipt"))
}

struct Database(Child);
impl Drop for Database {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = crate::sys::kill_child_recorded(
                &mut self.0,
                "local mail setup: stop owned provisioning database",
            );
        }
        let _ = self.0.wait();
    }
}

fn preflight(s: &Settings) -> Result<()> {
    if crate::sys::getuid() == 0 {
        return Err("local setup must run as an unprivileged user".into());
    }
    if s.root.exists() || fs::symlink_metadata(&s.root).is_ok() {
        return Err("setup root already exists; retain it and choose a new directory".into());
    }
    let parent = s.root.parent().ok_or("missing setup parent")?;
    if fs::canonicalize(parent).map_err(|e| e.to_string())? != parent {
        return Err("setup parent must use a physical absolute path".into());
    }
    trusted_ancestors(parent)?;
    if text(&s.root.join("frontend/runtime/smtp-control.sock"))?.len() > 107 {
        return Err("setup root is too long for the MTA control socket (maximum 72 bytes)".into());
    }
    for n in [
        Some(s.pg_port),
        Some(s.https_port),
        s.replica.is_none().then_some(s.smtp_port),
    ]
    .into_iter()
    .flatten()
    {
        TcpListener::bind(("127.0.0.1", n)).map_err(|e| format!("local port {n}: {e}"))?;
    }
    for program in ["initdb", "postgres", "pg_ctl", "psql"] {
        let output = command(Path::new(program))
            .arg("--version")
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        if !output.status.success()
            || !String::from_utf8_lossy(&output.stdout).contains("(PostgreSQL) 17.")
        {
            return Err(format!("{program} must be PostgreSQL 17"));
        }
    }
    if !command(Path::new("openssl"))
        .arg("version")
        .output()
        .map_err(|e| format!("openssl: {e}"))?
        .status
        .success()
    {
        return Err("openssl version failed".into());
    }
    if !s.prefix.join("bin/td-mta").is_file() {
        return Err("install td-mta before local setup".into());
    }
    Ok(())
}

fn certificates(p: &mut Setup<'_>) -> Result<()> {
    let tls = p.path("tls");
    let mut cmd = command(Path::new("openssl"));
    cmd.current_dir(&tls).args([
        "req",
        "-x509",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:P-256",
        "-noenc",
        "-days",
        "3650",
        "-subj",
        "/CN=td-mta local CA",
        "-keyout",
        "ca.key",
        "-out",
        "ca.pem",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    p.execute("ca", &mut cmd, None)?;
    let mut cmd = command(Path::new("openssl"));
    cmd.current_dir(&tls).args([
        "req",
        "-new",
        "-newkey",
        "ec",
        "-pkeyopt",
        "ec_paramgen_curve:P-256",
        "-noenc",
        "-subj",
        "/CN=localhost",
        "-keyout",
        "server.key",
        "-out",
        "server.csr",
    ]);
    p.execute("csr", &mut cmd, None)?;
    p.write("tls/extensions", "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n")?;
    let serial = format!("0x01{}", random_hex(15)?);
    p.write("tls/serial", &serial)?;
    let mut cmd = command(Path::new("openssl"));
    cmd.current_dir(&tls).args([
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca.key",
        "-set_serial",
        &serial,
        "-days",
        "90",
        "-extfile",
        "extensions",
        "-out",
        "server.pem",
    ]);
    p.execute("leaf", &mut cmd, None)?;
    for name in ["ca.key", "ca.pem", "server.key", "server.pem", "server.csr"] {
        fs::set_permissions(tls.join(name), fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn uri_path(path: &Path) -> Result<String> {
    Ok(text(path)?
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect())
}

fn postgres_program() -> Result<PathBuf> {
    host_program("postgres")
}
fn host_program(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or("PATH is missing")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_absolute()
            && fs::metadata(&candidate).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
        {
            return Ok(candidate);
        }
    }
    Err(format!("{name} must be in an absolute PATH directory"))
}

fn database_services(p: &Setup<'_>, postgres: &Path, name: &str) -> Result<()> {
    let data = p.path("postgres");
    let config = format!("config_file={}", text(&p.path("postgresql.conf"))?);
    let args = [text(postgres)?, "-D", text(&data)?, "-c", &config];
    let exec = args
        .iter()
        .map(|s| systemd(s))
        .collect::<Vec<_>>()
        .join(" ");
    p.write(&format!("services/{name}.service"), format!("[Unit]\nDescription=td-mta private PostgreSQL\n\n[Service]\nType=exec\nExecStart=:{exec}\nRestart=on-failure\nRestartSec=5\nKillSignal=SIGINT\nTimeoutStopSec=120\nUMask=0077\n\n[Install]\nWantedBy=default.target\n"))?;
    let exec = args.iter().map(|s| scheme(s)).collect::<Vec<_>>().join(" ");
    p.write(&format!("services/{name}.scm"), format!("(use-modules (shepherd service))\n(register-services\n (service '({name})\n  #:documentation \"Private local mail PostgreSQL\"\n  #:respawn? #t\n  #:respawn-delay 5\n  #:start (make-forkexec-constructor '({exec})\n    #:log-file {} #:file-creation-mask #o077)\n  #:stop (make-kill-destructor SIGINT #:grace-period 120)))\n", scheme(text(&p.path("postgres.log"))?)))?;
    Ok(())
}
fn services(p: &Setup<'_>, postgres: &Path) -> Result<()> {
    database_services(p, postgres, "td-mta-postgres")?;
    for profile in ["frontend", "receive", "relay"] {
        for name in ["runtime", "scratch"] {
            directory(&p.path(profile).join(name), 0o700)?;
        }
        let (unit, scm) = service_files(
            &p.s.prefix.join("bin/td-mta"),
            &p.path("runtime-connection"),
            &p.s.root,
            profile,
        )?;
        let unit = unit.replacen("[Unit]\n", "[Unit]\nRequires=td-mta-postgres.service\nAfter=td-mta-postgres.service\nPartOf=td-mta-postgres.service\n", 1);
        let scm = scm.replacen(
            "#:respawn? #t",
            "#:requirement '(td-mta-postgres)\n  #:respawn? #t",
            1,
        );
        p.write(&format!("services/td-mta-{profile}.service"), unit)?;
        p.write(&format!("services/td-mta-{profile}.scm"), scm)?;
    }
    Ok(())
}

const REPLICA_GRANTS: &str = r"\connect td_mta
BEGIN;
SET ROLE td_mta_owner;
GRANT INSERT,UPDATE,DELETE ON td_mta.replica_progress,td_mta.replica_pending,
 td_mta.replica_mailbox_receipts,td_mta.replica_email_receipts,
 td_mta.replica_blob_identities TO td_mta_runtime;
GRANT USAGE ON SCHEMA td_mta TO td_mta_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA td_mta TO td_mta_reader;
GRANT UPDATE(singleton) ON td_mta.config_selection TO td_mta_reader;
GRANT UPDATE(id) ON td_mta.accounts TO td_mta_reader;
GRANT INSERT ON td_mta.leases TO td_mta_reader;
GRANT INSERT,UPDATE,DELETE ON td_mta.application_logs,td_mta.application_log_state TO td_mta_reader;
COMMIT;
";
fn source_args(p: &Setup<'_>, replica: &Replica) -> Result<Vec<String>> {
    let mut args = vec![
        "--source-origin".into(),
        replica.origin.clone(),
        "--username".into(),
        replica.username.clone(),
        "--password-file".into(),
        text(&p.path("source-password"))?.into(),
    ];
    if replica.ca.is_some() {
        args.extend(["--ca-file".into(), text(&p.path("source-ca.pem"))?.into()]);
    }
    Ok(args)
}
fn replica_services(
    p: &Setup<'_>,
    postgres: &Path,
    openssl: &Path,
    replica: &Replica,
    epoch: &str,
    account: &str,
) -> Result<()> {
    database_services(p, postgres, "td-mta-replica-postgres")?;
    for name in ["runtime", "scratch"] {
        directory(&p.path("frontend").join(name), 0o700)?;
    }
    let bin = p.s.prefix.join("bin/td-mta");
    let connection = p.path("reader-connection");
    let runtime = p.path("frontend/runtime");
    let scratch = p.path("frontend/scratch");
    let argv = [
        text(&bin)?,
        "serve",
        "--read-only",
        "--connection-file",
        text(&connection)?,
        "--profile",
        "frontend",
        "--runtime",
        text(&runtime)?,
        "--scratch",
        text(&scratch)?,
    ];
    let (unit, scm) = super::supervisor_files(
        &argv,
        "td-mta-replica",
        &p.path("frontend/service.log"),
        120,
        "td-mta status",
    )?;
    p.write("services/td-mta-replica.service", unit.replacen("[Unit]\n", "[Unit]\nRequires=td-mta-replica-postgres.service\nAfter=td-mta-replica-postgres.service\nPartOf=td-mta-replica-postgres.service\n", 1))?;
    p.write(
        "services/td-mta-replica.scm",
        scm.replacen(
            "#:respawn? #t",
            "#:requirement '(td-mta-replica-postgres)\n  #:respawn? #t",
            1,
        ),
    )?;
    let mut copy = vec![
        text(&bin)?.to_owned(),
        "postgres".into(),
        "replica-copy".into(),
        "--connection-file".into(),
        text(&p.path("runtime-connection"))?.into(),
        "--epoch".into(),
        epoch.into(),
        "--account".into(),
        account.into(),
        "--source-epoch".into(),
        fs::read_to_string(p.path("source-epoch")).map_err(|e| e.to_string())?,
    ];
    copy.extend(source_args(p, replica)?);
    let state = p.path("copy-state");
    directory(&state, 0o700)?;
    let mut follow = copy.clone();
    *follow.get_mut(2).ok_or("missing copy operation")? = "replica-follow".into();
    follow.extend(["--state-dir".into(), text(&state)?.into()]);
    let (unit, scm) = super::supervisor_files(
        &follow.iter().map(String::as_str).collect::<Vec<_>>(),
        "td-mta-replica-sync",
        &p.path("copy-service.log"),
        120,
        "td-mta postgres replica-follow-status",
    )?;
    p.write("services/td-mta-replica-sync.service", unit.replacen("[Unit]\n", "[Unit]\nRequires=td-mta-replica-postgres.service\nAfter=td-mta-replica-postgres.service\nPartOf=td-mta-replica-postgres.service\n", 1))?;
    p.write(
        "services/td-mta-replica-sync.scm",
        scm.replacen(
            "#:respawn? #t",
            "#:requirement '(td-mta-replica-postgres)\n  #:respawn? #t",
            1,
        ),
    )?;
    copy.extend([
        "--steps".into(),
        "1024".into(),
        "--timeout-seconds".into(),
        "3600".into(),
    ]);

    let copy_command = copy
        .iter()
        .map(|arg| shell(Path::new(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    p.write(
        "copy-arguments.json",
        Json::Arr(copy.into_iter().map(Json::Str).collect()).to_canonical(),
    )?;
    super::install_mta_operations::configure(
        &[
            "--prefix".into(),
            text(&p.s.prefix)?.into(),
            "--state-dir".into(),
            text(&p.s.root)?.into(),
            "--postgres-tls-dir".into(),
            text(&p.path("tls"))?.into(),
            "--postgres-data-dir".into(),
            text(&p.path("postgres"))?.into(),
            "--postgres-port".into(),
            p.s.pg_port.to_string(),
            "--postgres-bin".into(),
            text(postgres.parent().ok_or("PostgreSQL directory missing")?)?.into(),
            "--openssl-bin".into(),
            text(&openssl)?.into(),
            "--replica-root".into(),
            text(&p.s.root)?.into(),
            "--replica-https-port".into(),
            p.s.https_port.to_string(),
        ],
        text(&p.s.root)?,
    )?;
    let renewal_services = p.s.prefix.join("share/td-mta/services");
    let mut start = String::from("Replica preparation complete. Services are stopped; no full copy is claimed.\nUse ONE supervisor. Replica service names differ from ordinary local mail.\n\nSystemd:\n");
    for name in [
        "td-mta-replica-postgres",
        "td-mta-replica",
        "td-mta-replica-sync",
    ] {
        start.push_str(&format!(
            "  systemctl --user link {}\n",
            shell(&p.path(&format!("services/{name}.service")))
        ));
    }
    start.push_str("  systemctl --user daemon-reload\n  systemctl --user enable --now td-mta-replica-postgres td-mta-replica td-mta-replica-sync\n\nShepherd (Guix Home persistence follows DEPLOYMENT.md):\n");
    for name in [
        "td-mta-replica-postgres",
        "td-mta-replica",
        "td-mta-replica-sync",
    ] {
        start.push_str(&format!(
            "  herd load root {}\n",
            shell(&p.path(&format!("services/{name}.scm")))
        ));
    }
    start.push_str("  herd start td-mta-replica\n  herd start td-mta-replica-sync\n\nFor a manual bounded batch, stop the sync service first:\n  ");
    start.push_str(&copy_command);
    start.push_str(&format!("\n\nHistorical synchronization status (not copy completeness or process liveness):\n  {} postgres replica-follow-status --state-dir {} --epoch {} --account {}\n", shell(&bin), shell(&state), shell(Path::new(epoch)), shell(Path::new(account))));
    start.push_str(&format!("\n\nLocal client:\n  td-net launch --allow-loopback td-mail --config={}\n\nThe local credential is independent of the upstream password.\nThe sync service continues copying when started. Enable the generated daily local TLS renewal job below; recovery promotion remains manual.\nThe unused relay configuration is required by the shared config grammar; no relay service runs.\nProtect this directory: it holds full mail, passwords and private keys.\n", shell(&p.path("mail.toml"))));
    start.push_str(&format!("\nDaily PostgreSQL and JMAP certificate renewal (enable with the same supervisor):\n  systemctl --user link {} {}\n  systemctl --user daemon-reload\n  systemctl --user enable --now td-mta-maint-replica-tls-renew.timer\n\nShepherd (also retain this definition in Guix Home):\n  herd load root {}\n  herd start td-mta-maint-replica-tls-renew\n", shell(&renewal_services.join("td-mta-maint-replica-tls-renew.service")),shell(&renewal_services.join("td-mta-maint-replica-tls-renew.timer")),shell(&renewal_services.join("td-mta-maint-replica-tls-renew.scm"))));
    p.write("START.txt", start)
}

fn prepare(s: &Settings) -> Result<()> {
    preflight(s)?;
    let postgres = postgres_program()?;
    let mut password = if s.replica.is_some() {
        random_hex(32)?.into_bytes()
    } else {
        protected_input(&s.relay_password, 4096)?
    };
    if password.last() == Some(&b'\n') {
        password.pop();
        if password.last() == Some(&b'\r') {
            password.pop();
        }
    }
    if password.is_empty() || password.contains(&b'\n') || password.contains(&b'\r') {
        return Err("relay password must be one nonempty line".into());
    }
    let relay_ca = s
        .relay_ca
        .as_ref()
        .map(|p| protected_input(p, 1024 * 1024))
        .transpose()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&s.root)
        .map_err(|e| e.to_string())?;
    let mut p = Setup { s, serial: 0 };
    for name in ["receipts", "tls", "socket", "services"] {
        directory(&p.path(name), 0o700)?;
    }
    p.write("relay-password", password)?;
    if let Some(ca) = relay_ca {
        p.write("relay-ca.pem", ca)?;
    }
    let discovered = if let Some(replica) = &s.replica {
        p.write("source-password", protected_input(&replica.password, 1026)?)?;
        if let Some(ca) = &replica.ca {
            p.write("source-ca.pem", protected_input(ca, 128 * 1024)?)?;
        }
        let mut args = vec!["replica-discover".to_owned()];
        args.extend(source_args(&p, replica)?);
        Some(p.mta(
            "source-discovery",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?)
    } else {
        None
    };
    certificates(&mut p)?;
    let mut init = command(Path::new("initdb"));
    init.args([
        "-D",
        text(&p.path("postgres"))?,
        "-U",
        "setup_admin",
        "--auth-local=trust",
        "--auth-host=scram-sha-256",
        "--no-locale",
        "-E",
        "UTF8",
    ]);
    p.execute("initdb", &mut init, None)?;
    let quote = |path: PathBuf| -> Result<String> {
        Ok(text(&path)?.replace('\'', "''").replace('\\', "\\\\"))
    };
    let base = format!("listen_addresses = '127.0.0.1'\nport = {}\nssl = on\nssl_cert_file = '{}'\nssl_key_file = '{}'\npassword_encryption = 'scram-sha-256'\nfsync = on\nsynchronous_commit = on\nfull_page_writes = on\nlog_statement = 'none'\nlog_min_error_statement = 'panic'\n", s.pg_port, quote(p.path("tls/server.pem"))?, quote(p.path("tls/server.key"))?);
    p.write(
        "postgresql.conf",
        format!("{base}unix_socket_directories = ''\n"),
    )?;
    p.write(
        "bootstrap.conf",
        format!(
            "{base}unix_socket_directories = '{}'\n",
            quote(p.path("socket"))?
        ),
    )?;
    fs::write(p.path("postgres/pg_hba.conf"), "local all setup_admin trust\nhostssl all td_mta_owner 127.0.0.1/32 scram-sha-256\nhostssl all td_mta_runtime 127.0.0.1/32 scram-sha-256\nhostssl all td_mta_reader 127.0.0.1/32 scram-sha-256\nhost all all 0.0.0.0/0 reject\n").map_err(|e| e.to_string())?;
    p.write("postgres.log", b"")?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(p.path("postgres.log"))
        .map_err(|e| e.to_string())?;
    let mut cmd = command(Path::new("postgres"));
    cmd.args([
        "-D",
        text(&p.path("postgres"))?,
        "-c",
        &format!("config_file={}", text(&p.path("bootstrap.conf"))?),
    ])
    .stdout(log.try_clone().map_err(|e| e.to_string())?)
    .stderr(log);
    let mut database = Database(cmd.spawn().map_err(|e| format!("postgres: {e}"))?);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if database.0.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Err("private PostgreSQL exited; inspect postgres.log".into());
        }
        if p.sql("wait", "SELECT 1;").is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            return Err("private PostgreSQL startup timed out".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let owner = random_hex(32)?;
    let runtime = random_hex(32)?;
    p.write("owner-password", &owner)?;
    p.write("runtime-password", &runtime)?;
    p.sql("roles", &format!("CREATE ROLE td_mta_owner LOGIN PASSWORD '{owner}';\nCREATE ROLE td_mta_runtime LOGIN PASSWORD '{runtime}';\nCREATE DATABASE td_mta OWNER td_mta_owner;\nREVOKE ALL ON DATABASE td_mta FROM PUBLIC;\nGRANT CONNECT ON DATABASE td_mta TO td_mta_runtime;\n"))?;
    for (name, password) in [("owner", owner), ("runtime", runtime)] {
        p.write(&format!("{name}-connection"), format!("postgresql://td_mta_{name}:{password}@localhost:{}/td_mta?sslmode=verify-full&sslrootcert={}", s.pg_port, uri_path(&p.path("tls/ca.pem"))?))?;
    }
    if s.replica.is_some() {
        let password = random_hex(32)?;
        p.write("reader-password", &password)?;
        p.sql("reader-role", &format!("CREATE ROLE td_mta_reader LOGIN PASSWORD '{password}'; GRANT CONNECT ON DATABASE td_mta TO td_mta_reader;"))?;
        p.write("reader-connection", format!("postgresql://td_mta_reader:{password}@localhost:{}/td_mta?sslmode=verify-full&sslrootcert={}", s.pg_port, uri_path(&p.path("tls/ca.pem"))?))?;
    }
    let receipt = p.owner("schema", &["schema-init"])?;
    let epoch = field(&receipt, "store_epoch")?;
    p.write("store-epoch", &epoch)?;
    let account = if let (Some(replica), Some(discovered)) = (&s.replica, discovered) {
        let account = field(&discovered, "account")?;
        let source_epoch = field(&discovered, "source_epoch")?;
        let origin = field(&discovered, "source_origin")?;
        p.write("source-origin", &origin)?;
        p.write("source-epoch", &source_epoch)?;
        p.owner(
            "source-bind",
            &[
                "replica-source-init",
                "--epoch",
                &epoch,
                "--source-origin",
                &origin,
                "--source-epoch",
                &source_epoch,
            ],
        )?;
        let mut args = vec![
            "replica-account-init".into(),
            "--epoch".into(),
            epoch.clone(),
            "--account".into(),
            account.clone(),
            "--source-epoch".into(),
            source_epoch,
        ];
        args.extend(source_args(&p, replica)?);
        for (flag, value) in ["--body-bytes", "--blobs", "--emails", "--receipts"]
            .into_iter()
            .zip(replica.quotas)
        {
            args.extend([flag.into(), value.to_string()]);
        }
        p.owner(
            "replica-account",
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )?;
        account
    } else {
        let account = random_hex(16)?;
        p.owner(
            "account",
            &["account-init", "--epoch", &epoch, "--account", &account],
        )?;
        account
    };
    p.write("account-id", &account)?;
    let device = p.owner(
        "device",
        &[
            "device-create",
            "--epoch",
            &epoch,
            "--account",
            &account,
            "--label",
            "local td-mail",
        ],
    )?;
    p.write("jmap-password", field(&device, "password")?)?;
    let mut materials = BTreeMap::new();
    for (name, kind, file) in [
        ("chain", "certificate-chain", "tls/server.pem"),
        ("key", "private-key", "tls/server.key"),
        ("password", "relay-password", "relay-password"),
        ("ca", "trust-bundle", "relay-ca.pem"),
    ] {
        if name == "ca" && s.relay_ca.is_none() {
            continue;
        }
        let id = field(
            &p.mta("material-version", &["material-version"])?,
            "version",
        )?;
        p.write(&format!("material-{name}-id"), &id)?;
        p.owner(
            "material",
            &[
                "material-import",
                "--epoch",
                &epoch,
                "--version",
                &id,
                "--kind",
                kind,
                "--input-file",
                text(&p.path(file))?,
            ],
        )?;
        materials.insert(name, id);
    }
    let material = |key| {
        materials
            .get(key)
            .map(String::as_str)
            .ok_or("missing material".to_string())
    };
    let domain = s
        .account
        .split_once('@')
        .map(|(_, d)| d)
        .ok_or("invalid account")?;
    let identity = random_hex(16)?;
    p.write("shared.toml", format!("version = 1\n[account \"{account}\"]\nusername = \"{}\"\n[domain \"{domain}\"]\nmx_host = \"localhost\"\n[alias \"{}\"]\naccount = \"{account}\"\n[identity \"{identity}\"]\naccount = \"{account}\"\nemail = \"{}\"\n", s.account, s.account, s.account))?;
    let mut base = format!("[server]\nhostname = \"localhost\"\njmap_origin = \"https://localhost:{}\"\n[resolver \"primary\"]\naddress = \"{}\"\n[relay]\nroute = \"local-relay\"\nhost = \"{}\"\nport = {}\nusername = \"{}\"\npassword_file = \"/postgres-material/{}\"\n", s.https_port, s.resolver, s.relay_host, s.relay_port, s.relay_user, material("password")?);
    if let Some(ca) = materials.get("ca") {
        base.push_str(&format!("ca_file = \"/postgres-material/{ca}\"\n"));
    }
    if s.replica.is_none() {
        p.write("relay.toml", &base)?;
    }
    base.push_str(&format!("[certificate \"local\"]\nmode = \"files\"\nchain_file = \"/postgres-material/{}\"\nkey_file = \"/postgres-material/{}\"\n", material("chain")?, material("key")?));
    p.write("frontend.toml", format!("{base}[listener \"https\"]\nkind = \"https\"\nbind = \"127.0.0.1:{}\"\ncertificate = \"local\"\n", s.https_port))?;
    if s.replica.is_none() {
        p.write("receive.toml", format!("{base}[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"127.0.0.1:{}\"\nserver_name = \"localhost\"\ncertificate = \"local\"\nsession_limit = 2\nper_peer_limit = 2\n", s.smtp_port))?;
    }
    let version = field(&p.mta("config-version", &["config-version"])?, "version")?;
    p.write("config-version", &version)?;
    let shared_path = p.path("shared.toml");
    let frontend_path = p.path("frontend.toml");
    let receive_path = p.path("receive.toml");
    let relay_path = p.path("relay.toml");
    let mut import = vec![
        "config-import",
        "--epoch",
        &epoch,
        "--version",
        &version,
        "--shared-file",
        text(&shared_path)?,
        "--instance",
        "frontend",
        "jmap",
        text(&frontend_path)?,
    ];
    if s.replica.is_none() {
        import.extend([
            "--instance",
            "receive",
            "receive",
            text(&receive_path)?,
            "--instance",
            "relay",
            "relay",
            text(&relay_path)?,
        ]);
    }
    p.owner("config-import", &import)?;
    p.owner(
        "config-check",
        &["config-check", "--epoch", &epoch, "--version", &version],
    )?;
    let selection = p.owner(
        "selection-prepare",
        &[
            "config-selection-prepare",
            "--epoch",
            &epoch,
            "--version",
            &version,
            "--generation",
            "0",
        ],
    )?;
    let request = field(&selection, "request")?;
    p.write("selection-request", &request)?;
    p.owner(
        "selection-publish",
        &["config-select", "--request", &request],
    )?;
    let grants = r#"\connect td_mta
BEGIN;
SET ROLE td_mta_owner;
GRANT USAGE ON SCHEMA td_mta TO td_mta_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA td_mta
    TO td_mta_runtime;
REVOKE INSERT, UPDATE, DELETE ON
    td_mta.schema_migrations, td_mta.runtime_barrier,
    td_mta.config_materials, td_mta.config_material_chunks,
    td_mta.config_versions, td_mta.config_version_chunks,
    td_mta.config_selection, td_mta.config_selection_receipts,
    td_mta.account_bootstraps, td_mta.leases, td_mta.imports, td_mta.replica_source,
    td_mta.replica_accounts, td_mta.replica_progress, td_mta.replica_pending,
    td_mta.replica_mailbox_receipts, td_mta.replica_email_receipts,
    td_mta.replica_blob_identities
    FROM td_mta_runtime;
GRANT UPDATE (singleton) ON td_mta.config_selection TO td_mta_runtime;
GRANT INSERT ON td_mta.leases TO td_mta_runtime;
COMMIT;
"#;
    p.sql("runtime-grants", grants)?;
    if s.replica.is_some() {
        p.sql("replica-grants", REPLICA_GRANTS)?;
    } else {
        p.mta(
            "runtime-check",
            &[
                "runtime-check",
                "--connection-file",
                text(&p.path("runtime-connection"))?,
            ],
        )?;
    }
    let mut stop = command(Path::new("pg_ctl"));
    stop.args(["-D", text(&p.path("postgres"))?, "-m", "fast", "-w", "stop"]);
    p.execute("stop", &mut stop, None)?;
    database.0.wait().map_err(|e| e.to_string())?;
    let password_command = format!("cat {}", shell(&p.path("jmap-password")));
    let toml = |s: &str| Json::Str(s.into()).to_canonical();
    let client_account = if s.replica.is_some() {
        "replica"
    } else {
        "local"
    };
    p.write("mail.toml", format!("[account.{client_account}]\nwell_known_url = \"https://localhost:{}/.well-known/jmap\"\nusername = {}\npassword_command = {}\ntls_ca_file = {}\n[mail]\nreply_from = {}\n", s.https_port, toml(&s.account), toml(&password_command), toml(text(&p.path("tls/ca.pem"))?), toml(&s.account)))?;
    if let Some(replica) = &s.replica {
        replica_services(
            &p,
            &postgres,
            &host_program("openssl")?,
            replica,
            &epoch,
            &account,
        )?;
        println!(
            "Replica setup complete: {} (services stopped; initial copy pending)",
            s.root.display()
        );
        println!("Next steps: {}", p.path("START.txt").display());
        return Ok(());
    }
    services(&p, &postgres)?;
    let mut start = String::from("Provisioning complete; PostgreSQL and MTA are stopped.\nUse ONE supervisor; these names represent one local setup per user manager.\n\nSystemd user manager: link all units before enabling any:\n");
    for profile in ["postgres", "frontend", "receive", "relay"] {
        start.push_str(&format!(
            "  systemctl --user link {}\n",
            shell(&p.path(&format!("services/td-mta-{profile}.service")))
        ));
    }
    start.push_str("  systemctl --user daemon-reload\n  systemctl --user enable --now td-mta-postgres td-mta-frontend td-mta-receive td-mta-relay\n\nShepherd: load all four definitions, then start all profiles:\n");
    for profile in ["postgres", "frontend", "receive", "relay"] {
        start.push_str(&format!(
            "  herd load root {}\n",
            shell(&p.path(&format!("services/td-mta-{profile}.scm")))
        ));
    }
    for profile in ["frontend", "receive", "relay"] {
        start.push_str(&format!("  herd start td-mta-{profile}\n"));
    }
    start.push_str("Runtime registration is not login persistence; use the Guix Home procedure in DEPLOYMENT.md.\n\nCheck readiness for EACH MTA profile:\n");
    for profile in ["frontend", "receive", "relay"] {
        start.push_str(&format!(
            "  {} status --json --require-ready --runtime {}\n",
            shell(&s.prefix.join("bin/td-mta")),
            shell(&p.path(&format!("{profile}/runtime")))
        ));
    }
    start.push_str(&format!("\nClient:\n  td-net launch --allow-loopback td-mail --config={}\nRelay uses implicit TLS. Setup did not send any mail.\nLeaf certificates expire in 90 days; renew before expiry (DEPLOYMENT.md).\nProtect this directory and backups: it contains passwords and private keys.\n", shell(&p.path("mail.toml"))));
    p.write("START.txt", start)?;
    println!(
        "Local setup complete: {} (services stopped)",
        s.root.display()
    );
    println!("Next steps: {}", p.path("START.txt").display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: &Path) -> Vec<String> {
        [
            "--root",
            root.to_str().unwrap(),
            "--account",
            "test@example.test",
            "--relay-host",
            "smtp.example.test",
            "--relay-user",
            "test@example.test",
            "--relay-password-file",
            "/private/password",
            "--resolver",
            "127.0.0.1:53",
        ]
        .map(String::from)
        .to_vec()
    }

    fn options(root: &Path) -> Settings {
        settings(&args(root), "/home/test").unwrap()
    }

    fn replica_args(root: &Path) -> Vec<String> {
        [
            "--root",
            root.to_str().unwrap(),
            "--account",
            "user@example.test",
            "--source-origin",
            "https://mail.example.test",
            "--password-file",
            "/private/password",
            "--resolver",
            "127.0.0.1:53",
            "--body-bytes",
            "1048576",
            "--blobs",
            "100",
            "--emails",
            "100",
            "--receipts",
            "1000",
        ]
        .map(String::from)
        .to_vec()
    }
    #[test]
    fn replica_setup_has_distinct_ports_and_refuses_sending_options() {
        let args = replica_args(Path::new("/private/replica"));
        let s = settings_mode(&args, "/home/test", true).unwrap();
        assert_eq!(s.pg_port, 35433);
        assert_eq!(s.https_port, 54444);
        assert_eq!(s.replica.as_ref().unwrap().username, "user@example.test");
        for extra in [
            vec!["--smtp-port", "55226"],
            vec!["--relay-host", "smtp.example.test"],
            vec!["--account", "duplicate@example.test"],
            vec!["--postgres-port", "54444"],
        ] {
            let mut invalid = args.clone();
            invalid.extend(extra.into_iter().map(String::from));
            assert!(settings_mode(&invalid, "/home/test", true).is_err());
        }
        for value in ["0", "-1", "9223372036854775808"] {
            let mut invalid = args.clone();
            let index = invalid
                .iter()
                .position(|arg| arg == "--body-bytes")
                .unwrap()
                + 1;
            invalid[index] = value.into();
            assert!(settings_mode(&invalid, "/home/test", true).is_err());
        }
        assert!(settings_mode(&args, "/home/test", false).is_err());
    }
    #[test]
    fn replica_services_separate_reader_from_copy_and_generate_no_smtp_units() {
        let temp = crate::test_temp::TempDir::new("replica-setup-services").unwrap();
        let mut s = settings_mode(&replica_args(temp.path()), "/home/test", true).unwrap();
        s.prefix = temp.path().join("prefix");
        let p = Setup { s: &s, serial: 0 };
        directory(&p.path("services"), 0o700).unwrap();
        p.write("source-epoch", "11111111111111111111111111111111")
            .unwrap();
        replica_services(
            &p,
            Path::new("/host/bin/postgres"),
            Path::new("/host/bin/openssl"),
            s.replica.as_ref().unwrap(),
            "22222222222222222222222222222222",
            "33333333333333333333333333333333",
        )
        .unwrap();
        let unit = fs::read_to_string(p.path("services/td-mta-replica.service")).unwrap();
        let scm = fs::read_to_string(p.path("services/td-mta-replica.scm")).unwrap();
        for service in [&unit, &scm] {
            assert!(service.contains("--read-only"));
            assert!(service.contains("reader-connection"));
            assert!(!service.contains("runtime-connection"));
            assert!(!service.contains("owner-connection"));
        }
        assert!(unit.contains("Requires=td-mta-replica-postgres.service"));
        assert!(scm.contains("#:requirement '(td-mta-replica-postgres)"));
        let files = fs::read_dir(p.path("services")).unwrap().count();
        assert_eq!(files, 6);
        let renewal = fs::read_to_string(
            s.prefix
                .join("share/td-mta/services/td-mta-maint-replica-tls-renew.service"),
        )
        .unwrap();
        assert!(renewal.contains("--replica-root"));
        assert!(renewal.contains("--https-port"));
        assert!(!renewal.contains("--force"));
        assert!(fs::read_to_string(p.path("START.txt"))
            .unwrap()
            .contains("td-mta-maint-replica-tls-renew.timer"));
        let sync = fs::read_to_string(p.path("services/td-mta-replica-sync.service")).unwrap();
        assert!(sync.contains("replica-follow"));
        assert!(sync.contains("runtime-connection"));
        assert!(!sync.contains("reader-connection"));
        assert!(!sync.contains("owner-connection"));
        assert!(sync.contains("--state-dir"));
        let copy = fs::read_to_string(p.path("copy-arguments.json")).unwrap();
        let copy = json::parse(&copy).unwrap();
        let args = copy.as_arr().unwrap();
        assert!(args.iter().any(|arg| arg
            .as_str()
            .is_some_and(|v| v.ends_with("runtime-connection"))));
        assert!(!args.iter().any(|arg| arg
            .as_str()
            .is_some_and(|v| v.contains("reader-connection") || v.contains("owner-connection"))));
    }
    #[test]
    fn defaults_accept_tls_relay_but_refuse_local_privileged_ports() {
        let s = options(Path::new("/private/mail"));
        assert_eq!(s.relay_port, 465);
        assert_eq!(s.prefix, Path::new("/home/test/.local"));
        for extra in [
            vec!["--postgres-port", "543"],
            vec!["--relay-port", "0"],
            vec!["--smtp-port", "35432"],
            vec!["--account", "duplicate@example.test"],
        ] {
            let mut input = args(Path::new("/private/mail"));
            input.extend(extra.into_iter().map(String::from));
            assert!(settings(&input, "/home/test").is_err());
        }
        assert!(!atom("x\"\n[relay]"));
        assert_eq!(uri_path(Path::new("/a b/%x")).unwrap(), "/a%20b/%25x");
    }

    #[test]
    fn existing_root_refuses_before_tools_or_modifications() {
        let temp = crate::test_temp::TempDir::new("mta-setup-existing").unwrap();
        fs::write(temp.path().join("keep"), "existing").unwrap();
        assert!(preflight(&options(temp.path()))
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(
            fs::read_to_string(temp.path().join("keep")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn secret_inputs_refuse_public_permissions_symlinks_and_oversize() {
        let temp = crate::test_temp::TempDir::new("mta-setup-secret").unwrap();
        let file = temp.path().join("password");
        write_new(&file, b"secret").unwrap();
        assert_eq!(read_private_input(&file, 6).unwrap(), b"secret");
        assert!(read_private_input(&file, 5).is_err());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_private_input(&link, 6).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_input(&file, 6).is_err());
    }

    #[test]
    fn credential_ancestors_refuse_shared_directories_and_symlinks() {
        let temp = crate::test_temp::TempDir::new("mta-setup-ancestors").unwrap();
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        write_new(&shared.join("password"), b"secret").unwrap();
        let error = protected_input(&shared.join("password"), 6).unwrap_err();
        assert!(error.contains(shared.to_str().unwrap()));
        let link = temp.path().join("linked");
        std::os::unix::fs::symlink(&shared, &link).unwrap();
        let error = protected_input(&link.join("password"), 6).unwrap_err();
        assert!(error.contains(link.to_str().unwrap()));
    }

    #[test]
    fn command_receipts_are_private_and_errors_do_not_print_child_output() {
        let temp = crate::test_temp::TempDir::new("mta-setup-command").unwrap();
        let s = options(temp.path());
        let mut p = Setup { s: &s, serial: 0 };
        directory(&p.path("receipts"), 0o700).unwrap();
        let mut cmd = command(&std::env::current_exe().unwrap());
        cmd.arg("--not-a-test-option");
        let error = p.execute("fixture", &mut cmd, None).unwrap_err();
        assert!(!error.contains("Unrecognized"));
        for suffix in ["out", "err"] {
            assert_eq!(
                fs::metadata(p.path(&format!("receipts/01-fixture.{suffix}")))
                    .unwrap()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn local_services_order_database_and_keep_runtime_separate_from_owner() {
        let temp = crate::test_temp::TempDir::new("mta-setup-services").unwrap();
        let s = options(temp.path());
        let p = Setup { s: &s, serial: 0 };
        directory(&p.path("services"), 0o700).unwrap();
        services(&p, Path::new("/host/bin/postgres")).unwrap();
        let pg = fs::read_to_string(p.path("services/td-mta-postgres.service")).unwrap();
        assert!(pg.contains("KillSignal=SIGINT"));
        assert!(pg.contains("postgresql.conf"));
        assert!(!pg.contains("bootstrap.conf"));
        for name in ["frontend", "receive", "relay"] {
            let unit =
                fs::read_to_string(p.path(&format!("services/td-mta-{name}.service"))).unwrap();
            assert!(unit.contains("After=td-mta-postgres.service"));
            assert!(unit.contains("PartOf=td-mta-postgres.service"));
            assert!(unit.contains("runtime-connection"));
            assert!(!unit.contains("owner-connection"));
            let scm = fs::read_to_string(p.path(&format!("services/td-mta-{name}.scm"))).unwrap();
            assert!(scm.contains("#:requirement '(td-mta-postgres)"));
        }
    }
}

pub(super) fn run(args: &[String]) -> ExitCode {
    if args == ["--help"] || args == ["-h"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let result = std::env::var("HOME")
        .map_err(|_| "HOME is required".into())
        .and_then(|home| settings(args, &home))
        .and_then(|s| prepare(&s));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("local td-mta setup: {e}\nRetain any created directory and private receipts; setup never resumes partial data.");
            ExitCode::FAILURE
        }
    }
}

pub(super) fn run_replica(args: &[String]) -> ExitCode {
    if args == ["--help"] || args == ["-h"] {
        print!("{REPLICA_HELP}");
        return ExitCode::SUCCESS;
    }
    let result = std::env::var("HOME")
        .map_err(|_| "HOME is required".into())
        .and_then(|home| settings_mode(args, &home, true))
        .and_then(|s| prepare(&s));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("replica td-mta setup: {error}\nRetain any created directory and private receipts; setup never resumes partial data.");
            ExitCode::FAILURE
        }
    }
}
