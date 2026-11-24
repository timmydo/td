//! Fenced move of the production database from verified loopback TLS to a
//! group-shared Unix socket with SCRAM. Nothing in production changes before
//! a rehearsal on a fresh physical copy passes; every replaced file is kept.
use super::*;
use crate::{
    isolation::files::{self, Setting},
    physical::{PG_DROPIN, SOCKET_DIR},
};
use database::{CONTROL_SERVICE, OWNER_SERVICE};
use fence::{open_socket, LiveSocket, SocketFence, PG_FENCE};
use std::{io::Read, os::unix::fs::FileTypeExt};

pub(crate) const RUNTIME_CONNECTION: &str = "/var/lib/td-mta/mail/runtime-connection";
/// `sed -n` program printing the runtime connection URI after its
/// `USER:PASSWORD@`, so the launcher reads the transport without the
/// password; a line not of that form, or with another `@`, prints nothing.
pub(crate) const REDACT_RUNTIME: &str =
    "s|^postgresql://td_mta_runtime:[A-Za-z0-9._~-]\\{1,128\\}@\\([^@]*\\)$|\\1|p";
const OWNER_CONNECTION: &str = "/var/lib/td-mta/mail/owner-connection";
const MAIL_CA: &str = "/var/lib/td-mta/mail/tls/ca.pem";
const PG_DROPIN_TEXT: &str = "[Service]\nSupplementaryGroups=td-mta-db\n";
const RENEW: &str = "td-mta-maint-postgres-tls-renew.timer";
const RENEW_SERVICE: &str = "td-mta-maint-postgres-tls-renew.service";
const RENEW_SCHEDULE: &str = "td-mta-maint-postgres-tls-renew.scm";
pub(super) const CONF_LIMIT: u64 = 64 * 1024;
pub(super) const FILE_LIMIT: u64 = 16 * 1024;
const DROPPED: &[&str] = &[
    "listen_addresses",
    "ssl",
    "ssl_cert_file",
    "ssl_key_file",
    "ssl_ca_file",
    "unix_socket_directories",
    "unix_socket_group",
    "unix_socket_permissions",
];
const INCLUDES: &[&str] = &["include", "include_dir", "include_if_exists"];
const HBA_DATABASES: &[&str] = &["td_mta", "replication", "postgres", "all"];
// `local` methods the cutover does not carry to the group-shared socket:
// trust and peer would admit every account in its group, and the rest
// are not the SCRAM every login there uses.
const LOCAL_UNCARRIED: &[&str] = &[
    "trust", "peer", "ident", "md5", "password", "pam", "ldap", "radius", "gss", "bsd",
];
const SERVICE_KEYS: &[&str] = &[
    "host",
    "hostaddr",
    "port",
    "dbname",
    "user",
    "passfile",
    "sslmode",
    "sslrootcert",
    "sslcertmode",
    "connect_timeout",
    "application_name",
    "target_session_attrs",
];
// What the staged configuration must give the server, as `postgres -C`
// reports it with no fence file present: it prints the mode 0660 in decimal.
const STAGED_SETTINGS: &[(&str, &str)] = &[
    ("listen_addresses", ""),
    ("ssl", "off"),
    ("port", "35432"),
    ("unix_socket_directories", SOCKET_DIR),
    ("unix_socket_group", isolation::DB_GROUP),
    ("unix_socket_permissions", "432"),
];

fn legacy_password<'a>(uri: &'a str, user: &str, ca: &str) -> R<&'a str> {
    let (password, location) = uri
        .strip_prefix("postgresql://")
        .and_then(|s| s.strip_prefix(user))
        .and_then(|s| s.strip_prefix(':'))
        .and_then(|s| s.split_once('@'))
        .ok_or("unexpected loopback TLS connection")?;
    let tail = location
        .strip_prefix("localhost:35432")
        .or_else(|| location.strip_prefix("127.0.0.1:35432"))
        .and_then(|s| s.strip_prefix("/td_mta?sslmode=verify-full&sslrootcert="))
        .ok_or("unexpected loopback TLS connection")?;
    require(tail == ca, "unexpected loopback TLS root certificate")?;
    // The socket form carries only unreserved characters verbatim.
    isolation::socket_uri(user, password)?;
    Ok(password)
}

fn split_newline(text: &str) -> (&str, &str) {
    match text.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (text, ""),
    }
}

/// Whether the production runtime login still uses loopback TLS. Anything
/// other than exactly one of the two provisioned forms is refused.
pub(super) fn loopback_tls() -> R<bool> {
    let bytes = td_fs::read_bounded_real_file(
        Path::new(RUNTIME_CONNECTION),
        "runtime connection",
        FILE_LIMIT,
    )?;
    let (uri, _) = split_newline(std::str::from_utf8(&bytes)?);
    runtime_tls(uri)
}

fn runtime_tls(uri: &str) -> R<bool> {
    if isolation::socket_password(uri, "td_mta_runtime").is_ok() {
        return Ok(false);
    }
    legacy_password(uri, "td_mta_runtime", MAIL_CA)?;
    Ok(true)
}

/// `loopback_tls` for what `REDACT_RUNTIME` printed: the same two forms,
/// judged by the same parser under a stand-in password.
pub(crate) fn redacted_runtime_tls(location: &str) -> R<bool> {
    runtime_tls(&fmt(format_args!(
        "postgresql://td_mta_runtime:redacted@{location}"
    ))?)
}

/// The same credentials over the private socket.
pub(super) fn connection(text: &str, user: &str, ca: &str) -> R<String> {
    let (uri, newline) = split_newline(text);
    let mut out = isolation::socket_uri(user, legacy_password(uri, user, ca)?)?;
    td_alloc::push_str(&mut out, newline)?;
    Ok(out)
}

fn setting_key(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c == '=' || c.is_ascii_whitespace())
        .unwrap_or(line.len());
    line.get(..end)
}

/// The block the cutover appends: socket-only listening and the fence.
pub(super) fn socket_settings(directory: &str, group: &str, fence: &str) -> R<String> {
    fmt(format_args!(
        "# Private socket, installed by td-mta-deploy --postgres-socket.\n\
         listen_addresses = ''\nssl = off\nunix_socket_directories = '{directory}'\n\
         unix_socket_group = '{group}'\nunix_socket_permissions = 0660\n\
         include_if_exists = '{fence}'\n"
    ))
}

/// Drop every listening, TLS and socket setting and append `settings`.
/// An include or another TLS setting is refused: the result must be all of
/// the server's configuration and the fence include must come last.
pub(super) fn postgresql_conf(text: &str, settings: &str) -> R<String> {
    require(
        text.len() as u64 <= CONF_LIMIT,
        "oversized PostgreSQL configuration",
    )?;
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len() + settings.len() + 1)?;
    for line in text.split_inclusive('\n') {
        if let Some(key) = setting_key(line) {
            let named = |list: &[&str]| list.iter().any(|k| key.eq_ignore_ascii_case(k));
            require(!named(INCLUDES), "PostgreSQL configuration includes a file")?;
            if named(DROPPED) {
                continue;
            }
            require(
                !key.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("ssl_")),
                "unexpected PostgreSQL TLS setting",
            )?;
        }
        td_alloc::push_str(&mut out, line)?;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        td_alloc::push_str(&mut out, "\n")?;
    }
    td_alloc::push_str(&mut out, settings)?;
    Ok(out)
}

pub(super) fn role_name(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether `text`, the whole server configuration, leaves the server
/// without a Unix socket: its last `unix_socket_directories` is `''`.
/// Absent, the compiled-in directory applies; any other value, or an
/// include that could set it, counts as a socket.
pub(super) fn no_unix_socket(text: &str) -> bool {
    let mut none = false;
    for line in text.lines() {
        let Some(key) = setting_key(line) else {
            continue;
        };
        if INCLUDES.iter().any(|k| key.eq_ignore_ascii_case(k)) {
            return false;
        }
        if !key.eq_ignore_ascii_case("unix_socket_directories") {
            continue;
        }
        let rest = line
            .trim_start()
            .get(key.len()..)
            .unwrap_or_default()
            .trim_start();
        let value = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        none = value.strip_prefix("''").is_some_and(|tail| {
            let tail = tail.trim();
            tail.is_empty() || tail.starts_with('#')
        });
    }
    none
}

/// Whether the server has no Unix socket today, as three readings agree:
/// the configuration file `conf`, `postgres -C unix_socket_directories`
/// over it and postgresql.auto.conf (`setting`), and the running
/// postmaster's `postmaster.pid`, whose fifth line is its first socket
/// directory, empty when it has none.
pub(super) fn no_socket_today(conf: &str, setting: &str, postmaster: &str) -> bool {
    no_unix_socket(conf) && setting.is_empty() && postmaster.lines().nth(4) == Some("")
}

/// What the cutover makes of the production HBA file.
pub(super) struct Hba {
    pub(super) text: String,
    /// Each inert `local` rule dropped, its words single-spaced.
    pub(super) dropped: Vec<String>,
}

/// Each loopback SCRAM rule becomes the same rule on the socket, in place.
/// Socket rules must already use SCRAM or reject: the group-shared socket
/// would otherwise expose a peer or trust rule to every service account.
/// Such a rule is dropped instead when `no_socket` says the server has no
/// Unix socket today, so that it admits no one now; with a socket it is
/// refused. TCP rejections stay; any other rule is refused.
pub(super) fn pg_hba(text: &str, no_socket: bool) -> R<Hba> {
    require(text.len() as u64 <= CONF_LIMIT, "oversized HBA file")?;
    // A continuation would join what follows a dropped line to another.
    require(!text.contains('\\'), "unexpected HBA line continuation")?;
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len())?;
    let mut dropped = Vec::new();
    for line in text.split_inclusive('\n') {
        let (body, newline) = split_newline(line);
        let (rule, comment) = match body.split_once('#') {
            Some((rule, comment)) => (rule, Some(comment)),
            None => (body, None),
        };
        let mut words = [""; 7];
        let mut count = 0;
        for word in rule.split_ascii_whitespace() {
            *words.get_mut(count).ok_or("unexpected HBA rule")? = word;
            count += 1;
        }
        if count == 0 {
            td_alloc::push_str(&mut out, line)?;
            continue;
        }
        require(
            !rule.contains(['"', ',', '@', '+']),
            "unexpected HBA database or role list",
        )?;
        let fields = words.get(..count).ok_or("unexpected HBA rule")?;
        match fields {
            ["local", _, _, method] if matches!(*method, "scram-sha-256" | "reject") => {
                td_alloc::push_str(&mut out, line)?;
            }
            ["local", _, _, method, options @ ..]
                if LOCAL_UNCARRIED.contains(method) && options.iter().all(|o| o.contains('=')) =>
            {
                require(
                    no_socket,
                    "local HBA rule without SCRAM while PostgreSQL has a Unix socket",
                )?;
                // The rule reaches the receipt and the operator's terminal.
                require(
                    fields
                        .iter()
                        .all(|w| w.bytes().all(|b| b.is_ascii_graphic())),
                    "unexpected HBA rule",
                )?;
                let mut rule = String::new();
                for word in fields {
                    if !rule.is_empty() {
                        td_alloc::push_str(&mut rule, " ")?;
                    }
                    td_alloc::push_str(&mut rule, word)?;
                }
                td_alloc::push(&mut dropped, rule)?;
            }
            ["host" | "hostssl", _, _, _, "reject"]
            | ["host" | "hostssl", _, _, _, _, "reject"] => {
                td_alloc::push_str(&mut out, line)?;
            }
            ["hostssl", database, user, "127.0.0.1/32", "scram-sha-256"]
                if HBA_DATABASES.contains(database) && role_name(user) =>
            {
                let local = fmt(format_args!("local {database} {user} scram-sha-256"))?;
                td_alloc::push_str(&mut out, &local)?;
                if let Some(comment) = comment {
                    td_alloc::push_str(&mut out, " #")?;
                    td_alloc::push_str(&mut out, comment)?;
                }
                td_alloc::push_str(&mut out, newline)?;
            }
            _ => return Err("unexpected HBA rule".into()),
        }
    }
    Ok(Hba { text: out, dropped })
}

/// Point each libpq service at the socket directory, dropping TLS settings,
/// and collect the passfiles it names into `passfiles`.
pub(super) fn service_file(text: &str, passfiles: &mut Vec<PathBuf>) -> R<String> {
    require(text.len() as u64 <= FILE_LIMIT, "oversized service file")?;
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len())?;
    // Host and port seen in the open section, if any.
    let mut section: Option<(bool, bool)> = None;
    let complete = |section: Option<(bool, bool)>| {
        require(
            section.is_none_or(|(host, port)| host && port),
            "service without host and port",
        )
    };
    for line in text.split_inclusive('\n') {
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            td_alloc::push_str(&mut out, line)?;
            continue;
        }
        if body.starts_with('[') {
            require(
                body.len() > 2 && body.ends_with(']'),
                "invalid service section",
            )?;
            complete(section)?;
            section = Some((false, false));
            td_alloc::push_str(&mut out, line)?;
            continue;
        }
        let (key, value) = body.split_once('=').ok_or("invalid service setting")?;
        let seen = section
            .as_mut()
            .ok_or("service setting outside a section")?;
        require(SERVICE_KEYS.contains(&key), "unexpected service setting")?;
        match key {
            "hostaddr" | "sslmode" | "sslrootcert" | "sslcertmode" => continue,
            "host" => {
                seen.0 = true;
                td_alloc::push_str(&mut out, &fmt(format_args!("host={SOCKET_DIR}\n"))?)?;
                continue;
            }
            "port" => {
                require(value == "35432", "unexpected service port")?;
                seen.1 = true;
            }
            "passfile" => {
                require(
                    value.starts_with('/')
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
                    "unexpected service passfile",
                )?;
                if !passfiles.iter().any(|p| p.as_os_str() == value) {
                    td_alloc::push(passfiles, PathBuf::from(owned(value)?))?;
                }
            }
            _ => (),
        }
        td_alloc::push_str(&mut out, line)?;
        if !line.ends_with('\n') {
            td_alloc::push_str(&mut out, "\n")?;
        }
    }
    complete(section)?;
    Ok(out)
}

/// libpq matches a passfile entry to a non-default socket directory by
/// that directory's path.
pub(super) fn passfile(text: &str) -> R<String> {
    require(text.len() as u64 <= FILE_LIMIT, "oversized passfile")?;
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len() + 256)?;
    for line in text.split_inclusive('\n') {
        let (body, newline) = split_newline(line);
        if body.trim().is_empty() || body.trim_start().starts_with('#') {
            td_alloc::push_str(&mut out, line)?;
            continue;
        }
        let mut fields = body.splitn(5, ':');
        let (host, port, database, user, password) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        );
        let (Some(database), Some(user), Some(password)) = (database, user, password) else {
            return Err("unexpected passfile entry".into());
        };
        require(
            matches!(host, Some("localhost" | "127.0.0.1"))
                && port == Some("35432")
                && !database.is_empty()
                && !user.is_empty()
                && !database.contains('\\')
                && !user.contains('\\'),
            "unexpected passfile entry",
        )?;
        let entry = fmt(format_args!(
            "{SOCKET_DIR}:35432:{database}:{user}:{password}{newline}"
        ))?;
        td_alloc::push_str(&mut out, &entry)?;
    }
    Ok(out)
}

/// Whether a /proc/net/tcp or tcp6 table lists a listening socket.
pub(super) fn tcp_listening(table: &str) -> R<bool> {
    let mut lines = table.lines();
    require(
        lines
            .next()
            .is_some_and(|h| h.trim_start().starts_with("sl")),
        "unexpected TCP table",
    )?;
    for line in lines {
        let state = line
            .split_ascii_whitespace()
            .nth(3)
            .ok_or("unexpected TCP table row")?;
        if state == "0A" {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn check_socket(directory: &Path, uid: u32, gid: u32, mode: u32) -> R<()> {
    let dir = fs::symlink_metadata(directory)?;
    require(
        dir.is_dir() && dir.uid() == uid && dir.gid() == gid && dir.mode() & 0o7777 == 0o750,
        "PostgreSQL socket directory ownership or mode",
    )?;
    let socket = fs::symlink_metadata(child(directory, ".s.PGSQL.35432")?)?;
    require(
        socket.file_type().is_socket()
            && socket.uid() == uid
            && socket.gid() == gid
            && socket.mode() & 0o7777 == mode,
        "PostgreSQL socket ownership or mode",
    )
}

pub(super) fn read_file(path: &Path, limit: u64) -> R<(Vec<u8>, fs::Metadata)> {
    let (mut file, meta) = td_fs::open_real_file(path, "production file")?;
    require(
        meta.len() <= limit && meta.nlink() == 1,
        "unexpected production file size or links",
    )?;
    let mut bytes = td_alloc::filled(usize::try_from(meta.len())?, 0)?;
    file.read_exact(&mut bytes)?;
    require(file.read(&mut [0])? == 0, "production file changed")?;
    Ok((bytes, meta))
}

fn mirror(root: &Path, target: &Path) -> R<PathBuf> {
    child(
        root,
        target
            .strip_prefix("/")?
            .to_str()
            .ok_or("non-UTF8 production path")?,
    )
}

// A private copy with the owner, group and mode its production file has.
fn write_copy(path: &Path, file: &Staged, bytes: &[u8]) -> R<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent(path)?)?;
    let mut out = create_file(path, 0o600)?;
    std::os::unix::fs::fchown(&out, Some(file.uid), Some(file.gid))?;
    out.set_permissions(fs::Permissions::from_mode(file.mode))?;
    out.write_all(bytes)?;
    out.sync_all()?;
    Ok(())
}

fn publish(file: &Staged, bytes: &[u8]) -> R<()> {
    let (uid, gid, mode) = (file.uid, file.gid, file.mode);
    td_fs::replace_with(&file.target, bytes, mode, |out| {
        std::os::unix::fs::fchown(out, Some(uid), Some(gid))?;
        out.set_permissions(fs::Permissions::from_mode(mode))
    })?;
    Ok(())
}

pub(super) struct Staged {
    target: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
    old: Option<Vec<u8>>,
    new: Vec<u8>,
    staged: PathBuf,
    // Bound over its production path in the rehearsal holder.
    bind: bool,
    installed: bool,
}

pub(super) struct Cutover {
    root: PathBuf,
    files: Vec<Staged>,
    created_dir: bool,
    // Drop-in directories the install created, removed again on restore.
    created: Vec<PathBuf>,
    touched: bool,
    renew_excluded: bool,
}

impl Cutover {
    fn file(&self, target: &Path) -> R<&Staged> {
        self.files
            .iter()
            .find(|f| f.target == target)
            .ok_or_else(|| "missing staged file".into())
    }

    fn add(
        &mut self,
        target: &Path,
        old: Option<(Vec<u8>, fs::Metadata)>,
        new: String,
        bind: bool,
    ) -> R<()> {
        let staged = mirror(&child(&self.root, "staged")?, target)?;
        let (uid, gid, mode, old) = match old {
            Some((bytes, meta)) => (meta.uid(), meta.gid(), meta.mode() & 0o7777, Some(bytes)),
            None => (0, 0, 0o644, None),
        };
        let file = Staged {
            target: PathBuf::from(target),
            uid,
            gid,
            mode,
            old,
            new: new.into_bytes(),
            staged,
            bind,
            installed: false,
        };
        write_copy(&file.staged, &file, &file.new)?;
        td_alloc::push(&mut self.files, file)?;
        Ok(())
    }

    fn setting(&mut self, old: Option<&Setting>, new: &Setting) -> R<()> {
        let current = match old {
            Some(old) => {
                let (bytes, meta) = read_file(&old.path, FILE_LIMIT)?;
                require(
                    bytes == old.text.as_bytes() && meta.uid() == 0 && meta.gid() == 0,
                    "unexpected loopback TLS system setting",
                )?;
                Some((bytes, meta))
            }
            None => {
                require(
                    !files::present(new)? && !new.path.try_exists()?,
                    "unexpected socket system setting",
                )?;
                None
            }
        };
        self.add(&new.path, current, owned(&new.text)?, false)
    }
}

// The socket directory exists before the rehearsal, empty and unnamed by any
// configuration, so the holder can mount the trial's directory there.
fn socket_directory(uid: u32, gid: u32) -> R<bool> {
    let path = Path::new(SOCKET_DIR);
    files::safe_parents(path)?;
    let created = match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o750))?;
            sync_dir(parent(path)?)?;
            true
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.into()),
    };
    let meta = fs::symlink_metadata(path)?;
    require(
        meta.is_dir() && meta.uid() == uid && meta.gid() == gid && meta.mode() & 0o7777 == 0o750,
        "PostgreSQL socket directory ownership or mode",
    )?;
    Ok(created)
}

impl Deployment {
    // The settings as the server reads them, postgresql.auto.conf included,
    // with no fence file present.
    fn socket_settings(&self, ns: &[String]) -> R<()> {
        for &(setting, expected) in STAGED_SETTINGS {
            let mut cmd = self.in_namespace(ns, "runuser");
            cmd.args(["-u", "td-mta", "--"])
                .arg(self.host.tool("postgres")?)
                .args(["-D", PGDATA, "-c"])
                .arg(fmt(format_args!("config_file={MAIL}/postgresql.conf"))?)
                .args(["-C", setting]);
            require(
                self.host.out(&mut cmd)? == expected,
                "PostgreSQL socket setting",
            )?;
        }
        Ok(())
    }

    /// The private socket state every activation expects.
    pub(super) fn socket_preflight(&self) -> R<()> {
        let gid = isolation::db_group(&self.host, false)?;
        let uid = fs::metadata(PGDATA)?.uid();
        let dropin = Setting {
            path: PathBuf::from(PG_DROPIN),
            text: owned(PG_DROPIN_TEXT)?,
            mode: 0o644,
        };
        require(
            files::present(&dropin)?,
            "missing PostgreSQL socket group setting",
        )?;
        require(
            !Path::new(PG_FENCE).try_exists()?,
            "stale PostgreSQL socket fence: run td-mta/deploy --recover",
        )?;
        fence::fence_directory(false)?;
        self.socket_settings(&[])?;
        files::safe_parents(Path::new(SOCKET_DIR))?;
        check_socket(Path::new(SOCKET_DIR), uid, gid, 0o660)
    }

    pub(super) fn postgres_socket(&mut self) -> R<()> {
        require(
            !Path::new(FENCE).try_exists()?,
            "unfinished deployment fence: run td-mta/deploy --recover",
        )?;
        require(
            self.previous == self.candidate && fs::canonicalize(ACTIVE)? == self.previous,
            "the socket cutover keeps the live release",
        )?;
        self.check_source()?;
        if !self.tls {
            self.socket_preflight()?;
            crate::readiness::prepare(&self.host, false)?;
            self.journal_identity()?;
            self.config(&self.previous, &[], self.isolated)?;
            // A cutover that failed after admission may have left the retired
            // transport's units or material; finish retiring them.
            let root = child(&self.work, "postgres-socket")?;
            fs::DirBuilder::new().mode(0o700).create(&root)?;
            self.retire_tls(&root)?;
            self.healthy(&self.previous)?;
            self.probes.probe("smtp")?;
            self.probes.probe("jmap")?;
            self.event("already on the private socket")?;
            return Ok(());
        }
        require(
            !Path::new(PG_FENCE).try_exists()?,
            "stale PostgreSQL socket fence: inspect the previous receipt",
        )?;
        crate::readiness::prepare(&self.host, true)?;
        let gid = isolation::db_group(&self.host, true)?;
        set(&mut self.record, "socket_group", gid.into())?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        let mut cut = Cutover {
            root: child(&self.work, "postgres-socket")?,
            files: Vec::new(),
            created_dir: false,
            created: Vec::new(),
            touched: false,
            renew_excluded: false,
        };
        self.guarded(
            &mut cut,
            |d, cut| d.socket_cutover(cut, gid),
            Self::restore_transport,
        )
    }

    fn socket_cutover(&mut self, cut: &mut Cutover, gid: u32) -> R<()> {
        let uid = fs::metadata(PGDATA)?.uid();
        self.event("staging private socket transport")?;
        fs::DirBuilder::new().mode(0o700).create(&cut.root)?;
        let dropped = self.stage(cut)?;
        let mut rules = Vec::new();
        for rule in &dropped {
            self.event(&fmt(format_args!("dropped inert local HBA rule: {rule}"))?)?;
            td_alloc::push(&mut rules, Json::Str(owned(rule)?))?;
        }
        set(&mut self.record, "socket_hba_dropped", Json::Arr(rules))?;
        let mut staged = Vec::new();
        for file in &cut.files {
            td_alloc::push(
                &mut staged,
                Json::Str(owned(file.target.to_str().ok_or("staged path")?)?),
            )?;
        }
        set(&mut self.record, "socket_files", Json::Arr(staged))?;
        // Checked before any fence: the server's include must reach the file.
        fence::fence_directory(true)?;
        cut.created_dir = socket_directory(uid, gid)?;
        set(
            &mut self.record,
            "socket_directory_created",
            cut.created_dir.into(),
        )?;
        self.event("rehearsing private socket transport")?;
        self.rehearse(cut, uid, gid)?;
        self.event("private socket rehearsal verified")?;
        self.prepare_fence(true, false)?;
        self.fence(false, false)?;

        // The included file fences a restart from here on.
        self.socket_fence = true;
        LiveSocket(&self.host).write_file()?;
        cut.touched = true;
        self.host
            .run(self.host.cmd("systemctl").args(["stop", PG]))?;
        require(
            self.control_state(Path::new(PGDATA))? == "shut down",
            "clean PostgreSQL shutdown",
        )?;
        self.save_originals(cut)?;
        set(
            &mut self.record,
            "socket_originals",
            Json::Str(owned(
                child(&cut.root, "original")?
                    .to_str()
                    .ok_or("originals path")?,
            )?),
        )?;
        for file in cut.files.iter_mut() {
            file.installed = true;
            if file.old.is_none() {
                let directory = parent(&file.target)?;
                match fs::DirBuilder::new().mode(0o755).create(directory) {
                    Ok(()) => td_alloc::push(&mut cut.created, PathBuf::from(directory))?,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                    Err(e) => return Err(e.into()),
                }
            }
            publish(file, &file.new)?;
        }
        sync_dir(Path::new(UNIT_DIR))?;
        socket_directory(uid, gid)?;
        self.host
            .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        crate::readiness::prepare(&self.host, false)?;
        self.host
            .run(self.host.cmd("systemctl").args(["start", PG]))?;
        self.tls = false;
        self.wait(25, "private socket startup", || self.ready_db(&[]))?;
        check_socket(Path::new(SOCKET_DIR), uid, gid, 0)?;
        self.check_source()?;
        require(
            self.sql("owner", "SHOW listen_addresses", &[], false)?
                .is_empty()
                && self.sql("owner", "SHOW ssl", &[], false)? == "off",
            "PostgreSQL still listens on TCP or TLS",
        )?;
        require(
            self.host
                .out(self.host.cmd("ss").args(["-H", "-ltn", "sport = :35432"]))?
                .is_empty(),
            "PostgreSQL TCP listener",
        )?;
        self.event("private socket installed")?;

        // The retired certificate's renewal never runs again.
        cut.renew_excluded = self.timers.iter().any(|t| t == RENEW);
        self.timers.retain(|t| t != RENEW);
        set(
            &mut self.record,
            "renewal_timer_was_active",
            cut.renew_excluded.into(),
        )?;
        self.admit()?;
        self.wait(240, "ACME lease recovery", || self.acme_online())?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "timer set changed")?;
        self.event("verified private socket transport")?;
        self.retire_tls(&cut.root)?;
        self.event("retired loopback TLS")?;
        Ok(())
    }

    /// Stage every converted file; returns the inert HBA rules dropped.
    fn stage(&self, cut: &mut Cutover) -> R<Vec<String>> {
        let conf = child(Path::new(MAIL), "postgresql.conf")?;
        let (old, meta) = read_file(&conf, CONF_LIMIT)?;
        let settings = socket_settings(SOCKET_DIR, isolation::DB_GROUP, PG_FENCE)?;
        let text = std::str::from_utf8(&old)?;
        let new = postgresql_conf(text, &settings)?;
        let postmaster = td_fs::read_bounded_real_file(
            &child(Path::new(PGDATA), "postmaster.pid")?,
            "postmaster.pid",
            FILE_LIMIT,
        )?;
        let no_socket = no_socket_today(
            text,
            &self.production_setting("unix_socket_directories")?,
            std::str::from_utf8(&postmaster)?,
        );
        cut.add(&conf, Some((old, meta)), new, true)?;
        let hba = self.hba_file()?;
        let (old, meta) = read_file(&hba, CONF_LIMIT)?;
        let Hba { text: new, dropped } = pg_hba(std::str::from_utf8(&old)?, no_socket)?;
        cut.add(&hba, Some((old, meta)), new, false)?;

        let mut logins = Vec::new();
        for (path, user) in [
            (OWNER_CONNECTION, "td_mta_owner"),
            (RUNTIME_CONNECTION, "td_mta_runtime"),
        ] {
            td_alloc::push(
                &mut logins,
                (PathBuf::from(path), owned(user)?, owned(MAIL_CA)?),
            )?;
        }
        if self.isolated {
            for &role in ROLES {
                let path = PathBuf::from(isolation::connection(role)?);
                let ca = child(parent(&path)?, "tls/ca.pem")?;
                td_alloc::push(
                    &mut logins,
                    (
                        path,
                        fmt(format_args!("td_mta_{role}"))?,
                        owned(ca.to_str().ok_or("role certificate path")?)?,
                    ),
                )?;
            }
        }
        for (path, user, ca) in logins {
            let (old, meta) = read_file(&path, FILE_LIMIT)?;
            let new = connection(std::str::from_utf8(&old)?, &user, &ca)?;
            cut.add(&path, Some((old, meta)), new, true)?;
        }

        let mut passfiles = Vec::new();
        for path in [OWNER_SERVICE, CONTROL_SERVICE] {
            let path = Path::new(path);
            let (old, meta) = read_file(path, FILE_LIMIT)?;
            let new = service_file(std::str::from_utf8(&old)?, &mut passfiles)?;
            cut.add(path, Some((old, meta)), new, true)?;
        }
        for path in passfiles {
            let (old, meta) = read_file(&path, FILE_LIMIT)?;
            let new = passfile(std::str::from_utf8(&old)?)?;
            cut.add(&path, Some((old, meta)), new, true)?;
        }

        let dropin = Setting {
            path: PathBuf::from(PG_DROPIN),
            text: owned(PG_DROPIN_TEXT)?,
            mode: 0o644,
        };
        cut.setting(None, &dropin)?;
        if self.isolated {
            for (old, new) in files::production_tls()?
                .iter()
                .zip(files::production()?.iter())
            {
                require(old.path == new.path, "identity setting order")?;
                if old.text != new.text {
                    cut.setting(Some(old), new)?;
                }
            }
        }
        cut.setting(
            Some(&crate::readiness::aggregate(true)?),
            &crate::readiness::aggregate(false)?,
        )?;
        // Every other td-mta job must already be independent of the retired
        // transport; the cutover would otherwise leave it failing.
        let show = self.host.out(self.host.cmd("systemctl").args([
            "show",
            "--all",
            "-p",
            "Id,ExecStartPre,ExecStart,ExecStartPost",
            "td-mta-*",
        ]))?;
        let mut converted = Vec::new();
        for file in &cut.files {
            td_alloc::push(&mut converted, file.target.as_path())?;
        }
        if let Some(unit) = retired_reference(&show, &converted)? {
            return Err(fmt(format_args!(
                "{unit} names loopback TLS material or an unconverted connection file"
            ))?
            .into());
        }
        // The notification jobs name theirs inside their configuration.
        for name in ["config.json", "isolated.json"] {
            let path = child(Path::new(isolation::NOTIFY), name)?;
            if !path.try_exists()? {
                continue;
            }
            let bytes = td_fs::read_bounded_real_file(
                &path,
                "notification configuration",
                isolation::NOTIFICATION_LIMIT as u64,
            )?;
            let config = td_json::parse(std::str::from_utf8(&bytes)?)
                .map_err(|_| "invalid notification configuration")?;
            let connection = config
                .get("connection")
                .and_then(Json::as_str)
                .ok_or("notification configuration without a connection")?;
            require(
                converted.iter().any(|p| p.as_os_str() == connection),
                "notification configuration names an unconverted connection file",
            )?;
        }
        Ok(dropped)
    }

    fn rehearse(&mut self, cut: &Cutover, uid: u32, gid: u32) -> R<()> {
        self.rehearsal(|d, copy| {
            let conf = cut.file(&child(Path::new(MAIL), "postgresql.conf")?)?;
            let hba = cut.file(&child(Path::new(PGDATA), "pg_hba.conf")?)?;
            let postgres = d.host.tool("postgres")?;
            copy.start_staged(d, "transport", &postgres, &conf.new, &hba.new, b"")?;
            let mut binds = Vec::new();
            for file in cut.files.iter().filter(|f| f.bind) {
                td_alloc::push(&mut binds, (file.staged.as_path(), file.target.as_path()))?;
            }
            let ns = copy.hold(d, "transport", &binds)?;
            // The live release must admit the group-shared socket.
            d.qualify_trial(&ns)?;
            d.socket_settings(&ns)?;
            check_socket(&child(&copy.runtime, "socket")?, uid, gid, 0o660)?;
            let pid = copy.trial_pid(&d.host)?;
            for table in ["tcp", "tcp6"] {
                let rows = fs::read_to_string(fmt(format_args!("/proc/{pid}/net/{table}"))?)?;
                require(!tcp_listening(&rows)?, "rehearsal TCP listener")?;
            }
            Ok(())
        })
    }

    // Every original is unchanged since staging and is kept for rollback.
    fn save_originals(&self, cut: &Cutover) -> R<()> {
        let root = child(&cut.root, "original")?;
        for file in &cut.files {
            match &file.old {
                Some(old) => {
                    let (bytes, meta) = read_file(&file.target, CONF_LIMIT)?;
                    require(
                        bytes == *old
                            && meta.uid() == file.uid
                            && meta.gid() == file.gid
                            && meta.mode() & 0o7777 == file.mode,
                        "production file changed since staging",
                    )?;
                    write_copy(&mirror(&root, &file.target)?, file, old)?;
                }
                None => require(
                    !file.target.try_exists()?,
                    "socket system setting appeared since staging",
                )?,
            }
        }
        Ok(())
    }

    fn restore_transport(&mut self, cut: &mut Cutover) -> R<()> {
        if !self.fence_owned || self.admitting {
            // Production is untouched: only the empty socket directory exists.
            return remove_socket_directory(cut);
        }
        self.event("restoring loopback TLS before writer admission")?;
        if cut.touched {
            self.host.exec(
                self.host.cmd("systemctl").args(["stop", PG]),
                &[],
                Duration::from_secs(120),
                true,
            )?;
            // Restore every file even if one fails; report the first error.
            let mut restored: R<()> = Ok(());
            for file in cut.files.iter().filter(|f| f.installed) {
                let one = match &file.old {
                    Some(old) => publish(file, old),
                    None => {
                        remove_file(&file.target).and_then(|()| sync_dir(parent(&file.target)?))
                    }
                };
                restored = restored.and(one);
            }
            for directory in &cut.created {
                restored = restored.and(
                    fs::remove_dir(directory)
                        .map_err(Into::into)
                        .and_then(|()| sync_dir(parent(directory)?)),
                );
            }
            restored?;
            self.tls = true;
            self.host
                .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
            crate::readiness::prepare(&self.host, true)?;
            self.host
                .run(self.host.cmd("systemctl").args(["start", PG]))?;
            self.wait(25, "loopback TLS restart", || self.ready_db(&[]))?;
            self.check_source()?;
        }
        self.tls = true;
        if self.socket_fence {
            open_socket(&mut LiveSocket(&self.host))?;
            self.socket_fence = false;
        }
        if cut.renew_excluded {
            td_alloc::push(&mut self.timers, owned(RENEW)?)?;
            self.timers.sort_unstable();
            cut.renew_excluded = false;
        }
        self.config(&self.previous, &[], self.isolated)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        set(&mut self.record, "rollback", "transport restored".into())?;
        remove_socket_directory(cut)
    }

    // td-mta's operations installer generated the renewal units in its
    // prefix's `share/td-mta/services`, where the operator linked them; once
    // disabled, those files move into the job directory. A unit file found
    // anywhere else was never td-mta's and stays, recorded for the operator.
    // The certificate material is moved, never deleted, for rollback.
    // Each step acts only on what is still present, so a later run on the
    // socket finishes what an interrupted one left.
    fn retire_tls(&mut self, root: &Path) -> R<()> {
        let retired = child(root, "retired")?;
        fs::DirBuilder::new().mode(0o700).create(&retired)?;
        let mut generated = Vec::new();
        let mut left = Vec::new();
        for unit in [RENEW, RENEW_SERVICE] {
            let fragment = self.host.out(self.host.cmd("systemctl").args([
                "show",
                unit,
                "-p",
                "FragmentPath",
                "--value",
            ]))?;
            // A masked unit cannot run and has no file of its own.
            if fragment.is_empty() || fragment == "/dev/null" {
                continue;
            }
            let real = match fs::canonicalize(&fragment) {
                Ok(real) => real,
                // A link left to a file already moved; `disable` removes it.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if generated_unit(&real, unit) {
                td_alloc::push(&mut generated, real)?;
            } else {
                td_alloc::push(&mut left, Json::Str(owned(&fragment)?))?;
            }
        }
        let mut loaded = false;
        for unit in [RENEW, RENEW_SERVICE] {
            loaded |= self.host.out(self.host.cmd("systemctl").args([
                "show",
                unit,
                "-p",
                "LoadState",
                "--value",
            ]))? != "not-found";
        }
        // A run that failed against the closed TCP port would otherwise keep
        // failing the health check, even once its files are gone.
        self.host.exec(
            self.host
                .cmd("systemctl")
                .args(["reset-failed", RENEW_SERVICE]),
            &[],
            Duration::from_secs(30),
            false,
        )?;
        if loaded {
            self.host.run(self.host.cmd("systemctl").args([
                "disable",
                "--now",
                RENEW,
                RENEW_SERVICE,
            ]))?;
            for unit in [RENEW, RENEW_SERVICE] {
                let state = self.host.out(self.host.cmd("systemctl").args([
                    "show",
                    unit,
                    "-p",
                    "ActiveState",
                    "--value",
                ]))?;
                require(state == "inactive", "PostgreSQL TLS renewal still active")?;
            }
        }
        // The Shepherd definition generated beside them goes too.
        if let Some(directory) = generated.first().and_then(|p| p.parent()) {
            let scm = child(directory, RENEW_SCHEDULE)?;
            if scm.try_exists()? {
                td_alloc::push(&mut generated, scm)?;
            }
        }
        let units = child(&retired, "units")?;
        fs::DirBuilder::new().mode(0o700).create(&units)?;
        let mut moved = Vec::new();
        for path in &generated {
            let (bytes, _) = read_file(path, FILE_LIMIT)?;
            let mut copy = create_file(&child(&units, name(path)?)?, 0o600)?;
            copy.write_all(&bytes)?;
            copy.sync_all()?;
            sync_dir(&units)?;
            remove_file(path)?;
            sync_dir(parent(path)?)?;
            td_alloc::push(
                &mut moved,
                Json::Str(owned(path.to_str().ok_or("unit path")?)?),
            )?;
        }
        // `disable` undoes `link`; a link left to a moved or generated file
        // goes as well, including one an earlier run left dangling.
        for unit in [RENEW, RENEW_SERVICE] {
            let link = child(Path::new(UNIT_DIR), unit)?;
            if let Ok(target) = fs::read_link(&link) {
                if generated.contains(&target) || generated_unit(&target, unit) {
                    remove_file(&link)?;
                    sync_dir(Path::new(UNIT_DIR))?;
                }
            }
        }
        self.host
            .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        if left.is_empty() {
            for unit in [RENEW, RENEW_SERVICE] {
                let state = self.host.out(self.host.cmd("systemctl").args([
                    "show",
                    unit,
                    "-p",
                    "LoadState",
                    "--value",
                ]))?;
                require(
                    matches!(state.as_str(), "not-found" | "masked"),
                    "PostgreSQL TLS renewal unit still loaded",
                )?;
            }
        }
        set(&mut self.record, "renewal_units_retired", Json::Arr(moved))?;
        set(&mut self.record, "renewal_units_left", Json::Arr(left))?;
        let mut moves = Vec::new();
        td_alloc::push(
            &mut moves,
            (child(Path::new(MAIL), "tls")?, child(&retired, "mail-tls")?),
        )?;
        if self.isolated {
            for &role in ROLES {
                let home = PathBuf::from(parent(Path::new(isolation::connection(role)?))?);
                td_alloc::push(
                    &mut moves,
                    (
                        child(&home, "tls")?,
                        child(&retired, &fmt(format_args!("{role}-tls"))?)?,
                    ),
                )?;
            }
        }
        let mut record = Vec::new();
        for (from, to) in moves {
            match fs::symlink_metadata(&from) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
                Ok(meta) => require(meta.is_dir(), "unexpected TLS material")?,
            }
            require(
                fs::metadata(parent(&from)?)?.dev() == fs::metadata(&retired)?.dev(),
                "retired TLS material is on another filesystem",
            )?;
            fs::rename(&from, &to)?;
            sync_dir(parent(&from)?)?;
            sync_dir(&retired)?;
            td_alloc::push(
                &mut record,
                Json::Str(owned(from.to_str().ok_or("TLS path")?)?),
            )?;
        }
        set(&mut self.record, "retired_tls", Json::Arr(record))
    }
}

// Units the cutover itself replaces or retires.
const RETIRED_UNITS: &[&str] = &[RENEW, RENEW_SERVICE, "td-mta-health.service"];

/// The first unit in `systemctl show` output whose commands name a
/// PostgreSQL TLS directory, or a connection file the cutover does not
/// convert. Units are blank-line separated; `Id=` may follow the commands.
pub(super) fn retired_reference<'a>(show: &'a str, converted: &[&Path]) -> R<Option<&'a str>> {
    for block in show.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let unit = block
            .lines()
            .find_map(|line| line.strip_prefix("Id="))
            .ok_or("unit without an Id")?;
        if RETIRED_UNITS.contains(&unit) {
            continue;
        }
        for line in block.lines() {
            let Some((_, rest)) = line.split_once("argv[]=") else {
                continue;
            };
            let argv = rest.split(" ;").next().unwrap_or(rest);
            if names_retired(argv, converted)? {
                return Ok(Some(unit));
            }
        }
    }
    Ok(None)
}

fn names_retired(argv: &str, converted: &[&Path]) -> R<bool> {
    let option = |flag: &str| flag.starts_with("--") && flag.ends_with("connection-file");
    let mut connection = false;
    for word in argv.split_ascii_whitespace() {
        let path = if connection {
            Some(word)
        } else {
            word.split_once('=')
                .filter(|(flag, _)| option(flag))
                .map(|(_, value)| value)
        };
        connection = option(word);
        if word.starts_with("/var/lib/td-mta") && (word.contains("/tls/") || word.ends_with("/tls"))
        {
            return Ok(true);
        }
        if path.is_some_and(|path| !converted.iter().any(|p| p.as_os_str() == path)) {
            return Ok(true);
        }
    }
    require(!connection, "connection-file option without a value")?;
    Ok(false)
}

/// Whether `path`, a canonical unit file, is one td-mta's operations
/// installer generated for `unit`.
pub(super) fn generated_unit(path: &Path, unit: &str) -> bool {
    path.is_absolute()
        && path.file_name() == Some(std::ffi::OsStr::new(unit))
        && path
            .parent()
            .is_some_and(|d| d.ends_with("share/td-mta/services"))
}

fn remove_socket_directory(cut: &mut Cutover) -> R<()> {
    if cut.created_dir {
        match fs::remove_dir(SOCKET_DIR) {
            Ok(()) => sync_dir(parent(Path::new(SOCKET_DIR))?)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        cut.created_dir = false;
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/transport.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/postgres_socket_pg.rs"]
mod postgres_test;
