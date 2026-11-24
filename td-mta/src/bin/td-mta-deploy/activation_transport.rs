//! Fenced move of the production database from verified loopback TLS to a
//! group-shared Unix socket with SCRAM. Nothing in production changes before
//! a rehearsal on a fresh physical copy passes; every replaced file is kept.
use super::*;
use crate::{
    isolation::files::{self, Setting},
    physical::{PG_DROPIN, SOCKET_DIR},
};
use checks::{
    check_unit_state, copied_unit, generated_check, hardened_service_text, health_unit,
    notification_config, schedule_check, service_text, timer_text, tls_checks, tls_word, TlsCheck,
};
use database::{CONTROL_SERVICE, OWNER_SERVICE};
use fence::{fence_condition, open_socket, LiveSocket, SocketFence, FENCE_DROPIN, PG_FENCE};
use std::{io::Read, os::unix::fs::FileTypeExt};

#[path = "activation_retired_checks.rs"]
mod checks;

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
const HEALTH: &str = "td-mta-health.service";
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
    // The timers of retired jobs this job left out of writer admission.
    excluded: Vec<String>,
    // The generated expiry checks of the retired material, retired with it.
    checks: Vec<TlsCheck>,
}

/// The refusals of a check-only run, collected so that one run reports
/// every blocker it reaches; a cutover stops at the first.
pub(super) struct Refusals {
    collect: bool,
    found: Vec<String>,
}

impl Refusals {
    pub(super) fn new(collect: bool) -> Self {
        Self {
            collect,
            found: Vec::new(),
        }
    }

    /// `result`'s value; a refusal is the cutover's error, or a check's to
    /// report later, giving None.
    pub(super) fn take<T>(&mut self, result: R<T>) -> R<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) if self.collect => {
                td_alloc::push(&mut self.found, fmt(format_args!("{error}"))?)?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// `take`, a check's refusal naming `subject`, the file it concerns.
    pub(super) fn take_for<T>(&mut self, subject: &Path, result: R<T>) -> R<Option<T>> {
        match result {
            Err(error) if self.collect => {
                let refusal = fmt(format_args!("{}: {error}", subject.display()))?;
                td_alloc::push(&mut self.found, refusal)?;
                Ok(None)
            }
            result => self.take(result),
        }
    }

    pub(super) fn found(&self) -> &[String] {
        &self.found
    }
}

/// A notification configuration to rewrite: its bytes as read, its new
/// text, and the names of the checks it drops.
type Rewrite = (Vec<u8>, String, Vec<String>);

/// What staging found beside the files it converts.
struct Staging {
    /// Each inert `local` HBA rule dropped.
    dropped: Vec<String>,
    /// Each notification check of the retired material dropped.
    notification: Vec<String>,
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
    require(
        socket_directory_owned(path, uid, Some(gid))?,
        "missing PostgreSQL socket directory",
    )?;
    Ok(created)
}

/// Whether the socket directory `path` exists, refusing one that is not
/// `uid`'s and the socket group `gid`'s (None: a group still to be
/// created), mode 0750.
fn socket_directory_owned(path: &Path, uid: u32, gid: Option<u32>) -> R<bool> {
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
        Ok(meta) => meta,
    };
    require(
        meta.is_dir()
            && meta.uid() == uid
            && Some(meta.gid()) == gid
            && meta.mode() & 0o7777 == 0o750,
        "PostgreSQL socket directory ownership or mode",
    )?;
    Ok(true)
}

/// Whether the job asks only for a check (`plan_only`, absent in a job
/// from a launcher that has no such run).
pub(super) fn plan_only(job: &Json) -> R<bool> {
    match job.get("plan_only") {
        None => Ok(false),
        Some(flag) => flag.as_bool().ok_or_else(|| "invalid plan flag".into()),
    }
}

/// The steps of a socket cutover from loopback TLS. A check-only run
/// (`--postgres-socket --plan`) reads what `prepare` would install,
/// stages in its own job directory, and checks what the cutover requires
/// before its fence, stopping before its rehearsal, its fence or any live
/// change. Only `cutover` chooses between the two.
pub(super) trait CutoverSteps {
    /// Install the readiness settings and create the socket group.
    fn prepare(&mut self) -> R<()>;
    /// Read the readiness settings and socket group as they are.
    fn inspect(&mut self) -> R<()>;
    /// The live release's identity, configuration and health.
    fn preflight(&mut self) -> R<()>;
    /// Stage every file the cutover would change, and run its guards.
    fn stage(&mut self) -> R<()>;
    /// Keep a check's refusal with those it collected.
    fn refused(&mut self, error: Failure) -> R<()>;
    /// What the cutover requires before its fence, changing nothing, and
    /// a check's end.
    fn checked(&mut self) -> R<()>;
    /// The cutover itself, which stages under its own rollback.
    fn cut_over(&mut self) -> R<()>;
}

pub(super) type Failure = Box<dyn std::error::Error + Send + Sync>;

/// A cutover stops at its first refusal; a check stops only where nothing
/// after it can be judged (its preflight), and otherwise reports at its
/// end everything it refused.
pub(super) fn cutover(steps: &mut impl CutoverSteps, plan: bool) -> R<()> {
    if plan {
        steps.inspect()?;
        steps.preflight()?;
        if let Err(error) = steps.stage() {
            steps.refused(error)?;
        }
        return steps.checked();
    }
    steps.prepare()?;
    steps.preflight()?;
    steps.cut_over()
}

/// The steps of `--postgres-socket` on a host already on the socket,
/// which finishes retiring what a cutover left; a check reports it.
pub(super) trait SocketSteps {
    /// The private socket state every activation expects.
    fn transport(&mut self) -> R<()>;
    /// Install the socket layout's readiness settings.
    fn prepare(&mut self) -> R<()>;
    /// Read the readiness settings as they are.
    fn inspect(&mut self) -> R<()>;
    /// The live release's identity and configuration.
    fn preflight(&mut self) -> R<()>;
    /// The retired material's check units, as the cutover would find them.
    fn units(&mut self) -> R<()>;
    /// Retire what is left of the loopback transport.
    fn retire(&mut self) -> R<()>;
    /// Record what `retire` would retire.
    fn pending(&mut self) -> R<()>;
    /// The live release's health and public listeners.
    fn verify(&mut self) -> R<()>;
    /// Keep a check's refusal with those it collected.
    fn refused(&mut self, error: Failure) -> R<()>;
    fn checked(&mut self) -> R<()>;
    fn finished(&mut self) -> R<()>;
}

pub(super) fn on_socket(steps: &mut impl SocketSteps, plan: bool) -> R<()> {
    steps.transport()?;
    if !plan {
        steps.prepare()?;
        steps.preflight()?;
        steps.units()?;
        steps.retire()?;
        steps.verify()?;
        return steps.finished();
    }
    steps.inspect()?;
    steps.preflight()?;
    if let Err(error) = steps.units() {
        steps.refused(error)?;
    }
    steps.pending()?;
    if let Err(error) = steps.verify() {
        steps.refused(error)?;
    }
    steps.checked()
}

/// The receipt event that ends a check-only run.
const SOCKET_CHECKED: &str = "socket cutover checked: nothing changed";
const ON_SOCKET: &str = "already on the private socket";

/// The host side of `CutoverSteps`.
struct LiveCutover<'a> {
    deployment: &'a mut Deployment,
    cut: Cutover,
    /// The socket group; None while a check finds it absent.
    gid: Option<u32>,
    refusals: Refusals,
}

impl LiveCutover<'_> {
    fn group(&mut self, gid: Option<u32>) -> R<()> {
        self.gid = gid;
        set(
            &mut self.deployment.record,
            "socket_group",
            gid.map_or(Json::Null, Json::from),
        )
    }
}

impl CutoverSteps for LiveCutover<'_> {
    fn prepare(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        require(
            !Path::new(PG_FENCE).try_exists()?,
            "stale PostgreSQL socket fence: inspect the previous receipt",
        )?;
        crate::readiness::prepare(&d.host, true)?;
        let gid = isolation::db_group(&d.host, true)?;
        self.group(Some(gid))
    }

    fn inspect(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        require(
            !Path::new(PG_FENCE).try_exists()?,
            "stale PostgreSQL socket fence: inspect the previous receipt",
        )?;
        // Staging reads the settings `prepare` installs first.
        require(
            crate::readiness::check(&d.host, true)?,
            "the readiness settings are not all installed: the cutover installs them before it stages, so a check cannot stage",
        )?;
        let gid = isolation::db_group_present(&d.host)?;
        self.group(gid)
    }

    fn preflight(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        d.journal_identity()?;
        d.config(&d.previous, &[], d.isolated)?;
        d.healthy(&d.previous)
    }

    fn stage(&mut self) -> R<()> {
        self.deployment
            .stage_cutover(&mut self.cut, &mut self.refusals)
    }

    fn refused(&mut self, error: Failure) -> R<()> {
        self.refusals.take::<()>(Err(error)).map(|_| ())
    }

    fn checked(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        let r = &mut self.refusals;
        // The cutover creates the fence directory if it is absent.
        r.take(match fs::symlink_metadata(parent(Path::new(PG_FENCE))?) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => fence::fence_directory(false).map(|_| ()),
        })?;
        let path = Path::new(SOCKET_DIR);
        r.take(
            files::safe_parents(path)
                .and_then(|()| fs::metadata(PGDATA).map_err(Into::into))
                .and_then(|pgdata| socket_directory_owned(path, pgdata.uid(), self.gid)),
        )?;
        // The rehearsal's volume and room, and the fence's timers.
        if r.take(crate::physical::recovery_volume())?.is_some() {
            r.take(d.rehearsal_capacity(Path::new(crate::physical::VOLUME)))?;
        }
        r.take(
            d.active_timers()
                .and_then(|timers| fence::required_timers(&timers)),
        )?;
        r.take(d.probes.probe("smtp"))?;
        r.take(d.probes.probe("jmap"))?;
        d.concluded(r)
    }

    fn cut_over(&mut self) -> R<()> {
        let gid = self.gid.ok_or("missing PostgreSQL socket group")?;
        self.deployment.guarded(
            &mut self.cut,
            |d, cut| d.socket_cutover(cut, gid),
            Deployment::restore_transport,
        )
    }
}

/// The host side of `SocketSteps`.
struct LiveSocketHost<'a> {
    deployment: &'a mut Deployment,
    checks: Vec<TlsCheck>,
    refusals: Refusals,
}

impl SocketSteps for LiveSocketHost<'_> {
    fn transport(&mut self) -> R<()> {
        self.deployment.socket_preflight()
    }

    fn prepare(&mut self) -> R<()> {
        crate::readiness::prepare(&self.deployment.host, false)
    }

    fn inspect(&mut self) -> R<()> {
        let complete = crate::readiness::check(&self.deployment.host, false)?;
        set(
            &mut self.deployment.record,
            "socket_readiness_complete",
            complete.into(),
        )
    }

    fn preflight(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        d.journal_identity()?;
        d.config(&d.previous, &[], d.isolated)
    }

    fn units(&mut self) -> R<()> {
        // A cutover that failed after admission may have left the retired
        // transport's units or material; finish retiring them.
        self.checks = self.deployment.check_units(&mut self.refusals)?;
        Ok(())
    }

    fn retire(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        let root = child(&d.work, "postgres-socket")?;
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        d.retire_tls(&root, &self.checks)
    }

    fn pending(&mut self) -> R<()> {
        let mut names = Vec::new();
        for check in &self.checks {
            td_alloc::push(&mut names, Json::Str(owned(&check.name)?))?;
        }
        set(
            &mut self.deployment.record,
            "socket_retired_checks",
            Json::Arr(names),
        )
    }

    fn verify(&mut self) -> R<()> {
        let d = &mut *self.deployment;
        let r = &mut self.refusals;
        let healthy = r.take(d.healthy(&d.previous))?.is_some();
        let smtp = r.take(d.probes.probe("smtp"))?.is_some();
        let jmap = r.take(d.probes.probe("jmap"))?.is_some();
        if healthy && smtp && jmap {
            d.event(ON_SOCKET)?;
        }
        Ok(())
    }

    fn refused(&mut self, error: Failure) -> R<()> {
        self.refusals.take::<()>(Err(error)).map(|_| ())
    }

    fn checked(&mut self) -> R<()> {
        self.deployment.concluded(&self.refusals)
    }

    fn finished(&mut self) -> R<()> {
        Ok(())
    }
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
        let plan = plan_only(&self.record)?;
        self.check_source()?;
        if !self.tls {
            return on_socket(
                &mut LiveSocketHost {
                    deployment: self,
                    checks: Vec::new(),
                    refusals: Refusals::new(plan),
                },
                plan,
            );
        }
        let cut = Cutover {
            root: child(&self.work, "postgres-socket")?,
            files: Vec::new(),
            created_dir: false,
            created: Vec::new(),
            touched: false,
            excluded: Vec::new(),
            checks: Vec::new(),
        };
        cutover(
            &mut LiveCutover {
                deployment: self,
                cut,
                gid: None,
                refusals: Refusals::new(plan),
            },
            plan,
        )
    }

    fn stage_cutover(&mut self, cut: &mut Cutover, refusals: &mut Refusals) -> R<()> {
        self.event("staging private socket transport")?;
        fs::DirBuilder::new().mode(0o700).create(&cut.root)?;
        let staging = self.stage(cut, refusals)?;
        self.record_staging(cut, &staging)
    }

    /// The end of a check: every refusal it collected, the first as its
    /// error, or that it found none and changed nothing.
    fn concluded(&mut self, refusals: &Refusals) -> R<()> {
        if let Some(first) = refusals.found().first() {
            let mut all = Vec::new();
            for refusal in refusals.found() {
                td_alloc::push(&mut all, Json::Str(owned(refusal)?))?;
            }
            set(&mut self.record, "socket_refusals", Json::Arr(all))?;
            return Err(owned(first)?.into());
        }
        set(&mut self.record, "socket_checked", true.into())?;
        self.event(SOCKET_CHECKED)?;
        Ok(())
    }

    fn socket_cutover(&mut self, cut: &mut Cutover, gid: u32) -> R<()> {
        let uid = fs::metadata(PGDATA)?.uid();
        self.stage_cutover(cut, &mut Refusals::new(false))?;
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

        // The retired material's renewal and expiry checks never run again.
        let mut retiring = Vec::new();
        td_alloc::push(&mut retiring, owned(RENEW)?)?;
        for check in &cut.checks {
            td_alloc::push(&mut retiring, check.unit("timer")?)?;
        }
        let mut excluded = Vec::new();
        for timer in retiring {
            if self.timers.contains(&timer) {
                td_alloc::push(&mut excluded, Json::Str(owned(&timer)?))?;
                td_alloc::push(&mut cut.excluded, timer)?;
            }
        }
        self.timers.retain(|t| !cut.excluded.contains(t));
        set(
            &mut self.record,
            "retired_timers_excluded",
            Json::Arr(excluded),
        )?;
        self.admit()?;
        self.wait(240, "ACME lease recovery", || self.acme_online())?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "timer set changed")?;
        self.event("verified private socket transport")?;
        self.retire_tls(&cut.root, &cut.checks)?;
        self.event("retired loopback TLS")?;
        Ok(())
    }

    fn record_staging(&mut self, cut: &Cutover, staging: &Staging) -> R<()> {
        let mut rules = Vec::new();
        let verb = if plan_only(&self.record)? {
            "would drop"
        } else {
            "dropped"
        };
        for rule in &staging.dropped {
            self.event(&fmt(format_args!("{verb} inert local HBA rule: {rule}"))?)?;
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
        let mut units = Vec::new();
        for check in &cut.checks {
            td_alloc::push(&mut units, Json::Str(owned(&check.name)?))?;
        }
        set(&mut self.record, "socket_retired_checks", Json::Arr(units))?;
        let mut checks = Vec::new();
        for name in &staging.notification {
            td_alloc::push(&mut checks, Json::Str(owned(name)?))?;
        }
        set(
            &mut self.record,
            "socket_notification_checks_dropped",
            Json::Arr(checks),
        )
    }

    /// `systemctl show` of every td-mta unit's commands.
    fn unit_commands(&self) -> R<String> {
        self.host.out(self.host.cmd("systemctl").args([
            "show",
            "--all",
            "-p",
            "Id,ExecStartPre,ExecStart,ExecStartPost",
            "td-mta-*",
        ]))
    }

    /// The installer's expiry checks of the retired material, each still
    /// in a form it generated.
    fn check_units(&self, refusals: &mut Refusals) -> R<Vec<TlsCheck>> {
        let found = tls_checks(
            &self.unit_commands()?,
            &checks::material_dirs(self.isolated)?,
        )?;
        for check in &found {
            self.verify_check(check, refusals)?;
        }
        Ok(found)
    }

    /// `check`'s service and timer are the files the operations installer
    /// generated, or production's copies of them (`generated_check`), with
    /// no drop-in but a fence's own condition; the Shepherd definition
    /// beside generated ones, if any, registers and runs the same check.
    fn verify_check(&self, check: &TlsCheck, refusals: &mut Refusals) -> R<()> {
        let condition = fence_condition()?;
        let mut service = Vec::new();
        td_alloc::push(&mut service, service_text(check)?)?;
        td_alloc::push(&mut service, hardened_service_text(check)?)?;
        let mut timer = Vec::new();
        td_alloc::push(&mut timer, timer_text(check)?)?;
        let mut kinds = Vec::new();
        for (suffix, forms) in [("service", service), ("timer", timer)] {
            if let Some(Some(kind)) =
                refusals.take(self.verify_check_unit(check, suffix, &forms, &condition))?
            {
                td_alloc::push(&mut kinds, kind)?;
            }
        }
        refusals.take(same_kind(&check.name, &kinds))?;
        Ok(())
    }

    /// One of `check`'s units: where its file is (None for a copy, else
    /// the directory it is linked from), None when it is a timer systemd
    /// does not find.
    fn verify_check_unit(
        &self,
        check: &TlsCheck,
        suffix: &str,
        forms: &[String],
        condition: &str,
    ) -> R<Option<Option<PathBuf>>> {
        let unit = check.unit(suffix)?;
        let props = self.host.out(self.host.cmd("systemctl").args([
            "show",
            &unit,
            "-p",
            "LoadState",
            "-p",
            "FragmentPath",
            "-p",
            "DropInPaths",
        ]))?;
        let directory = child(Path::new(UNIT_DIR), &fmt(format_args!("{unit}.d"))?)?;
        let fence = child(&directory, FENCE_DROPIN)?;
        let clean = fence_only(&directory, condition)?;
        let Some(fragment) = check_unit_state(&unit, suffix == "timer", &props, &fence, clean)?
        else {
            return Ok(None);
        };
        let (real, copied) = copy_of(&unit, Path::new(fragment))?;
        let (bytes, _) = read_file(&real, FILE_LIMIT)?;
        generated_check(&unit, &real, copied, &bytes, forms)?;
        if copied {
            return Ok(Some(None));
        }
        let directory = PathBuf::from(parent(&real)?);
        if suffix == "service" {
            let scm = child(&directory, &check.unit("scm")?)?;
            if scm.try_exists()? {
                let (bytes, _) = read_file(&scm, FILE_LIMIT)?;
                schedule_check(check, &bytes)?;
            }
        }
        Ok(Some(Some(directory)))
    }

    /// The aggregate health unit's own file may still list the checks of
    /// the retired material that its readiness drop-in replaces
    /// (`ExecStart=` resets them); the cutover drops those lines too.
    fn stage_health(&self, cut: &mut Cutover) -> R<()> {
        let fragment = self.host.out(self.host.cmd("systemctl").args([
            "show",
            HEALTH,
            "-p",
            "FragmentPath",
            "--value",
        ]))?;
        require(
            fragment.starts_with('/'),
            "td-mta-health.service has no unit file",
        )?;
        let text = td_fs::read_bounded(Path::new(&fragment), FILE_LIMIT)?;
        let dirs = checks::material_dirs(self.isolated)?;
        let Some(new) = health_unit(std::str::from_utf8(&text)?, &dirs)? else {
            return Ok(());
        };
        let path = child(Path::new(UNIT_DIR), HEALTH)?;
        require(
            Path::new(&fragment) == path,
            "td-mta-health.service names loopback TLS material outside /etc/systemd/system",
        )?;
        let (old, meta) = read_file(&path, FILE_LIMIT)?;
        require(old == text, "td-mta-health.service changed while staging")?;
        cut.add(&path, Some((old, meta)), new, false)
    }

    /// Stage every converted file and find the jobs retired with the
    /// material; refuses anything else that still needs the old transport.
    /// Each independent part's refusal goes to `refusals`.
    fn stage(&self, cut: &mut Cutover, refusals: &mut Refusals) -> R<Staging> {
        // pg_hba.conf's inert rules depend on postgresql.conf's reading.
        let dropped = refusals
            .take((|| -> R<Vec<String>> {
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
                Ok(dropped)
            })())?
            .unwrap_or_default();

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
        for (path, user, ca) in &logins {
            refusals.take_for(
                path,
                (|| -> R<()> {
                    let (old, meta) = read_file(path, FILE_LIMIT)?;
                    let new = connection(std::str::from_utf8(&old)?, user, ca)?;
                    cut.add(path, Some((old, meta)), new, true)
                })(),
            )?;
        }

        let mut passfiles = Vec::new();
        for path in [OWNER_SERVICE, CONTROL_SERVICE] {
            let path = Path::new(path);
            refusals.take_for(
                path,
                (|| -> R<()> {
                    let (old, meta) = read_file(path, FILE_LIMIT)?;
                    let new = service_file(std::str::from_utf8(&old)?, &mut passfiles)?;
                    cut.add(path, Some((old, meta)), new, true)
                })(),
            )?;
        }
        for path in passfiles {
            refusals.take_for(
                &path,
                (|| -> R<()> {
                    let (old, meta) = read_file(&path, FILE_LIMIT)?;
                    let new = passfile(std::str::from_utf8(&old)?)?;
                    cut.add(&path, Some((old, meta)), new, true)
                })(),
            )?;
        }

        let dropin = Setting {
            path: PathBuf::from(PG_DROPIN),
            text: owned(PG_DROPIN_TEXT)?,
            mode: 0o644,
        };
        refusals.take_for(&dropin.path, cut.setting(None, &dropin))?;
        if self.isolated {
            for (old, new) in files::production_tls()?
                .iter()
                .zip(files::production()?.iter())
            {
                require(old.path == new.path, "identity setting order")?;
                if old.text != new.text {
                    refusals.take_for(&new.path, cut.setting(Some(old), new))?;
                }
            }
        }
        let aggregate = crate::readiness::aggregate(false)?;
        refusals.take_for(
            &aggregate.path,
            cut.setting(Some(&crate::readiness::aggregate(true)?), &aggregate),
        )?;
        refusals.take(self.stage_health(cut))?;
        // Every other td-mta job must already be independent of the retired
        // transport; the cutover would otherwise leave it failing. The
        // installer's expiry checks of the material retire with it.
        let dirs = checks::material_dirs(self.isolated)?;
        if let Some(show) = refusals.take(self.unit_commands())? {
            cut.checks = tls_checks(&show, &dirs)?;
            let mut retiring = Vec::new();
            for check in &cut.checks {
                self.verify_check(check, refusals)?;
                td_alloc::push(&mut retiring, check.unit("service")?)?;
            }
            // Judged against the files the cutover converts, so a refused
            // conversion is reported once, not again for each job using it.
            let mut paths = Vec::new();
            for (path, _, _) in &logins {
                td_alloc::push(&mut paths, path.as_path())?;
            }
            for unit in retired_references(&show, &paths, &retiring)? {
                refusals.take::<()>(Err(fmt(format_args!(
                    "{unit} names loopback TLS material or an unconverted connection file"
                ))?
                .into()))?;
            }
        }
        // The notification jobs name theirs inside their configuration,
        // and run the installer's checks of the material from it too.
        let mut notifications = Vec::new();
        let mut rewritten = Vec::new();
        for (name, canonical) in [("config.json", true), ("isolated.json", false)] {
            let path = child(Path::new(isolation::NOTIFY), name)?;
            let staged = refusals.take_for(
                &path,
                (|| -> R<Option<Rewrite>> {
                    if !path.try_exists()? {
                        return Ok(None);
                    }
                    let bytes = td_fs::read_bounded_real_file(
                        &path,
                        "notification configuration",
                        isolation::NOTIFICATION_LIMIT as u64,
                    )?;
                    let text = std::str::from_utf8(&bytes)?;
                    let config =
                        td_json::parse(text).map_err(|_| "invalid notification configuration")?;
                    let connection = config
                        .get("connection")
                        .and_then(Json::as_str)
                        .ok_or("notification configuration without a connection")?;
                    require(
                        logins
                            .iter()
                            .any(|(path, _, _)| path.as_os_str() == connection),
                        "notification configuration names an unconverted connection file",
                    )?;
                    // The installer writes config.json canonical, and the
                    // operator may have edited it since (its `alerts`); the
                    // isolation helper always derives isolated.json as td-json
                    // prints it.
                    let encode = |json: &Json| -> R<String> {
                        if canonical {
                            Ok(json.to_canonical())
                        } else {
                            fmt(format_args!("{json}\n"))
                        }
                    };
                    let Some((new, names)) = notification_config(text, &dirs, !canonical, encode)?
                    else {
                        return Ok(None);
                    };
                    Ok(Some((bytes, new, names)))
                })(),
            )?;
            let Some(Some((bytes, new, names))) = staged else {
                continue;
            };
            // isolated.json derives from config.json: one name each.
            for name in names {
                if !notifications.contains(&name) {
                    td_alloc::push(&mut notifications, name)?;
                }
            }
            td_alloc::push(&mut rewritten, (path, bytes, new))?;
        }
        for (path, bytes, new) in rewritten {
            refusals.take_for(
                &path,
                (|| -> R<()> {
                    let (old, meta) = read_file(&path, isolation::NOTIFICATION_LIMIT as u64)?;
                    require(
                        old == bytes,
                        "notification configuration changed while staging",
                    )?;
                    cut.add(&path, Some((old, meta)), new, false)
                })(),
            )?;
        }
        Ok(Staging {
            dropped,
            notification: notifications,
        })
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
                    let (bytes, meta) = read_file(&file.target, u64::try_from(old.len())?)?;
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
        if !cut.excluded.is_empty() {
            td_alloc::reserve(&mut self.timers, cut.excluded.len())?;
            for timer in cut.excluded.drain(..) {
                td_alloc::push(&mut self.timers, timer)?;
            }
            self.timers.sort_unstable();
        }
        self.config(&self.previous, &[], self.isolated)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        set(&mut self.record, "rollback", "transport restored".into())?;
        remove_socket_directory(cut)
    }

    // td-mta's operations installer generated the renewal units and the
    // expiry checks in its prefix's `share/td-mta/services`, where the
    // operator linked them; production copied the checks into
    // /etc/systemd/system instead, as staging verified. Once disabled,
    // those files move into the job directory, owner and mode kept. A unit
    // file found anywhere else was never td-mta's and stays, recorded for
    // the operator. The certificate material is moved,
    // never deleted, for rollback. Each step acts only on what is still
    // present, so a later run on the socket finishes what an interrupted one
    // left.
    fn retire_tls(&mut self, root: &Path, checks: &[TlsCheck]) -> R<()> {
        let retired = child(root, "retired")?;
        fs::DirBuilder::new().mode(0o700).create(&retired)?;
        let mut units = Vec::new();
        let mut services = Vec::new();
        td_alloc::push(&mut units, owned(RENEW)?)?;
        td_alloc::push(&mut units, owned(RENEW_SERVICE)?)?;
        td_alloc::push(&mut services, owned(RENEW_SERVICE)?)?;
        for check in checks {
            td_alloc::push(&mut units, check.unit("timer")?)?;
            td_alloc::push(&mut units, check.unit("service")?)?;
            td_alloc::push(&mut services, check.unit("service")?)?;
        }
        let show = |unit: &str, property: &str| {
            self.host.out(
                self.host
                    .cmd("systemctl")
                    .args(["show", unit, "-p", property, "--value"]),
            )
        };
        let mut generated = Vec::new();
        let mut left = Vec::new();
        let mut loaded = Vec::new();
        for unit in &units {
            if show(unit, "LoadState")? != "not-found" {
                td_alloc::push(&mut loaded, unit.as_str())?;
            }
            let fragment = show(unit, "FragmentPath")?;
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
            let check = checks.iter().any(|c| {
                c.unit("service").is_ok_and(|u| &u == unit)
                    || c.unit("timer").is_ok_and(|u| &u == unit)
            });
            let copied = check && copy_of(unit, Path::new(&fragment))?.1;
            if generated_unit(&real, unit) || copied {
                td_alloc::push(&mut generated, real)?;
            } else {
                td_alloc::push(&mut left, Json::Str(owned(&fragment)?))?;
            }
        }
        // A run that failed against the closed TCP port or the moved
        // material would otherwise keep failing the health check, even once
        // its files are gone: reset before stopping, which leaves a failed
        // unit failed, and again after, for a run that failed meanwhile.
        let reset = |services: &[String]| -> R<()> {
            for service in services {
                self.host.exec(
                    self.host.cmd("systemctl").args(["reset-failed", service]),
                    &[],
                    Duration::from_secs(30),
                    false,
                )?;
            }
            Ok(())
        };
        reset(&services)?;
        if !loaded.is_empty() {
            self.host.run(
                self.host
                    .cmd("systemctl")
                    .args(["disable", "--now"])
                    .args(&loaded),
            )?;
            reset(&services)?;
            for unit in &loaded {
                require(
                    show(unit, "ActiveState")? == "inactive",
                    "a retired loopback TLS unit is still active",
                )?;
            }
        }
        // The Shepherd definition generated beside each goes too, and so
        // does a sibling unit file systemd no longer loads, such as a timer
        // disabled by hand; only in the installer's directory, since a copy
        // in /etc/systemd/system goes alone, as verified.
        let mut siblings = Vec::new();
        for path in &generated {
            if parent(path)? == Path::new(UNIT_DIR) {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or("unit path")?;
            for suffix in ["service", "timer", "scm"] {
                let sibling = child(parent(path)?, &fmt(format_args!("{stem}.{suffix}"))?)?;
                if !generated.contains(&sibling)
                    && !siblings.contains(&sibling)
                    && sibling.try_exists()?
                {
                    td_alloc::push(&mut siblings, sibling)?;
                }
            }
        }
        for sibling in siblings {
            td_alloc::push(&mut generated, sibling)?;
        }
        let kept = child(&retired, "units")?;
        fs::DirBuilder::new().mode(0o700).create(&kept)?;
        let mut moved = Vec::new();
        for path in &generated {
            retire_file(path, &kept)?;
            td_alloc::push(
                &mut moved,
                Json::Str(owned(path.to_str().ok_or("unit path")?)?),
            )?;
        }
        // `disable` undoes `link`; a link left to a moved or generated file
        // goes as well, including one an earlier run left dangling.
        for unit in &units {
            let link = child(Path::new(UNIT_DIR), unit)?;
            if let Ok(target) = fs::read_link(&link) {
                if generated.contains(&target) || generated_unit(&target, unit) {
                    remove_file(&link)?;
                    sync_dir(Path::new(UNIT_DIR))?;
                }
            }
        }
        // A fence leaves its condition, or an empty directory, beside each
        // check; anything else there stays, recorded for the operator.
        let condition = fence_condition()?;
        let mut dropins = Vec::new();
        let mut dropins_left = Vec::new();
        let mut kept_dropins = Vec::new();
        for check in checks {
            for suffix in ["service", "timer"] {
                let unit = check.unit(suffix)?;
                let directory = child(Path::new(UNIT_DIR), &fmt(format_args!("{unit}.d"))?)?;
                let path = Json::Str(owned(directory.to_str().ok_or("drop-in path")?)?);
                match retire_dropins(&directory, &condition)? {
                    Some(true) => td_alloc::push(&mut dropins, path)?,
                    Some(false) => {
                        td_alloc::push(&mut dropins_left, path)?;
                        td_alloc::push(&mut kept_dropins, unit)?;
                    }
                    None => (),
                }
            }
        }
        self.host
            .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        // A unit file left in place keeps its unit; so may drop-ins left.
        if left.is_empty() {
            for unit in units.iter().filter(|u| !kept_dropins.contains(u)) {
                require(
                    matches!(show(unit, "LoadState")?.as_str(), "not-found" | "masked"),
                    "a retired loopback TLS unit is still loaded",
                )?;
            }
        }
        set(&mut self.record, "retired_units", Json::Arr(moved))?;
        set(&mut self.record, "retired_units_left", Json::Arr(left))?;
        set(&mut self.record, "retired_unit_dropins", Json::Arr(dropins))?;
        set(
            &mut self.record,
            "retired_unit_dropins_left",
            Json::Arr(dropins_left),
        )?;
        let mut record = Vec::new();
        for (from, to) in checks::material(self.isolated)? {
            match fs::symlink_metadata(&from) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
                Ok(meta) => require(meta.is_dir(), "unexpected TLS material")?,
            }
            require(
                fs::metadata(parent(&from)?)?.dev() == fs::metadata(&retired)?.dev(),
                "retired TLS material is on another filesystem",
            )?;
            fs::rename(&from, child(&retired, &to)?)?;
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

/// `fragment`, a unit file as systemd names it, resolved, and whether it is
/// a copy in /etc/systemd/system (`copied_unit`).
fn copy_of(unit: &str, fragment: &Path) -> R<(PathBuf, bool)> {
    let meta = fs::symlink_metadata(fragment)?;
    let real = fs::canonicalize(fragment)?;
    let copied = copied_unit(
        unit,
        fragment,
        &real,
        meta.is_file(),
        meta.uid(),
        meta.mode(),
    );
    Ok((real, copied))
}

/// Retirement moves a check's service and timer into one directory by
/// name: `kinds`, each found unit's (None for a copy, else the directory
/// it is linked from), must agree.
pub(super) fn same_kind(name: &str, kinds: &[Option<PathBuf>]) -> R<()> {
    match kinds {
        [service, timer] if service != timer => checks::refuse(
            name,
            "has a service and a timer that are not both copies or both linked from one directory",
        ),
        _ => Ok(()),
    }
}

/// Moves the file `path` into the directory `kept`, keeping its bytes,
/// owner and mode, so that copying it back restores it.
fn retire_file(path: &Path, kept: &Path) -> R<()> {
    let (bytes, meta) = read_file(path, FILE_LIMIT)?;
    let mut copy = create_file(&child(kept, name(path)?)?, 0o600)?;
    copy.write_all(&bytes)?;
    std::os::unix::fs::fchown(&copy, Some(meta.uid()), Some(meta.gid()))?;
    copy.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    copy.sync_all()?;
    sync_dir(kept)?;
    remove_file(path)?;
    sync_dir(parent(path)?)?;
    Ok(())
}

/// Whether `path` is the fence's drop-in holding exactly `condition`, which
/// a job that ended before its cleanup leaves beside each scheduled service.
fn fence_dropin(path: &Path, condition: &str) -> R<bool> {
    Ok(path.file_name() == Some(std::ffi::OsStr::new(FENCE_DROPIN))
        && td_fs::read_bounded(path, 4096)? == condition.as_bytes())
}

/// Whether the drop-in `directory` is absent or holds nothing but the
/// fence's `condition`.
fn fence_only(directory: &Path, condition: &str) -> R<bool> {
    let entries = match fs::read_dir(directory) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
        Ok(entries) => entries,
    };
    for entry in entries {
        if !fence_dropin(&entry?.path(), condition)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Clears a retired check's drop-in `directory` of the fence's `condition`
/// and removes it: Some(true) once it is gone, Some(false) when it holds
/// anything else, None when there is none.
fn retire_dropins(directory: &Path, condition: &str) -> R<Option<bool>> {
    let entries = match fs::read_dir(directory) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(entries) => entries,
    };
    let mut other = false;
    for entry in entries {
        let path = entry?.path();
        if fence_dropin(&path, condition)? {
            remove_file(&path)?;
        } else {
            other = true;
        }
    }
    if other {
        return Ok(Some(false));
    }
    fs::remove_dir(directory)?;
    sync_dir(parent(directory)?)?;
    Ok(Some(true))
}

// Units the cutover itself replaces or retires.
const RETIRED_UNITS: &[&str] = &[RENEW, RENEW_SERVICE, HEALTH];

/// Each unit in `systemctl show` output, other than those `retiring`
/// names, whose commands name a PostgreSQL TLS directory, or a connection
/// file the cutover does not convert. Units are blank-line separated;
/// `Id=` may follow the commands.
pub(super) fn retired_references<'a>(
    show: &'a str,
    converted: &[&Path],
    retiring: &[String],
) -> R<Vec<&'a str>> {
    let mut found = Vec::new();
    'units: for block in show.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let unit = block
            .lines()
            .find_map(|line| line.strip_prefix("Id="))
            .ok_or("unit without an Id")?;
        if RETIRED_UNITS.contains(&unit) || retiring.iter().any(|r| r == unit) {
            continue;
        }
        for line in block.lines() {
            let Some((_, rest)) = line.split_once("argv[]=") else {
                continue;
            };
            let argv = rest.split(" ;").next().unwrap_or(rest);
            if names_retired(argv, converted)? {
                td_alloc::push(&mut found, unit)?;
                continue 'units;
            }
        }
    }
    Ok(found)
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
        if tls_word(word) {
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
