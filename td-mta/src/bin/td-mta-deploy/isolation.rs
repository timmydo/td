//! Production service accounts, private roots and systemd identity cutover.
use crate::common::{child, fmt, require, Host};
use crate::service_database::Passwords;
use crate::R;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{chown, DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

#[path = "isolation_files.rs"]
mod files;
#[path = "isolation_helper.rs"]
mod scoped;
#[cfg(test)]
#[path = "tests/isolation.rs"]
mod tests;

pub(crate) const NOTIFY: &str = "/var/lib/td-mta/mail/.operations/notifications";
const BASE: &str = "/var/lib/td-mta-services";
const MAIL: &str = "/var/lib/td-mta/mail";
const BIN: &str = "/opt/td-mta/bin/td-mta";
const LEGACY: &str = "td-mta";

struct Role {
    name: &'static str,
    user: &'static str,
    unit: &'static str,
    connection: &'static str,
    runtime: &'static str,
    legacy_runtime: &'static str,
    low_port: bool,
}

macro_rules! role {
    ($name:literal, $unit:literal, $low_port:literal) => {
        Role {
            name: $name,
            user: concat!("td-mta-", $name),
            unit: concat!("td-mta-", $unit, ".service"),
            connection: concat!("/var/lib/td-mta-services/", $name, "/runtime-connection"),
            runtime: concat!("/var/lib/td-mta-services/", $name, "/", $name, "/runtime"),
            legacy_runtime: concat!("/var/lib/td-mta/mail/", $name, "/runtime"),
            low_port: $low_port,
        }
    };
}
const ROLES: &[Role] = &[
    role!("frontend", "frontend", true),
    role!("receive", "receive", true),
    role!("relay", "relay", false),
    role!("migadu", "migadu", false),
    role!("acme", "acme-production", true),
];

fn role(name: &str) -> R<&'static Role> {
    ROLES
        .iter()
        .find(|r| r.name == name)
        .ok_or("unknown service role".into())
}

pub(crate) fn role_user(name: &str) -> R<&'static str> {
    Ok(role(name)?.user)
}

pub(crate) fn connection(name: &str) -> R<&'static str> {
    Ok(role(name)?.connection)
}

pub(crate) fn runtime(name: &str) -> R<&'static str> {
    Ok(role(name)?.runtime)
}

pub(crate) fn backup_files() -> R<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for file in files::production()? {
        td_alloc::push(&mut paths, file.path)?;
    }
    Ok(paths)
}

pub(crate) fn installed() -> R<bool> {
    files::installed(&files::production()?)
}

pub(crate) fn install(host: &Host) -> R<()> {
    files::install(host, &files::production()?)?;
    host.run(host.cmd("systemctl").arg("daemon-reload"))
}

pub(crate) fn rollback(host: &Host) -> R<()> {
    files::rollback(&files::production()?)?;
    host.run(host.cmd("systemctl").arg("daemon-reload"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Account {
    uid: u32,
    gid: u32,
}

fn passwd_ids(row: &str) -> R<(&str, Account, &str, &str)> {
    let mut fields = row.split(':');
    let name = fields.next().ok_or("missing login")?;
    let uid = fields.nth(1).ok_or("missing uid")?.parse()?;
    let gid = fields.next().ok_or("missing gid")?.parse()?;
    let home = fields.nth(1).ok_or("missing home")?;
    let shell = fields.next().ok_or("missing shell")?;
    require(fields.next().is_none(), "unexpected passwd record")?;
    Ok((name, Account { uid, gid }, home, shell))
}

fn accounts(host: &Host, create: bool) -> R<Vec<(&'static Role, Account)>> {
    let legacy = host.out(host.cmd("getent").args(["passwd", LEGACY]))?;
    let (name, trusted, _, _) = passwd_ids(&legacy)?;
    require(name == LEGACY, "missing maintenance identity")?;
    let mut accounts = Vec::new();
    for role in ROLES {
        let result = host.exec(
            host.cmd("getent").args(["passwd", role.user]),
            &[],
            Duration::from_secs(10),
            false,
        )?;
        let record = match result.status.code() {
            Some(0) => String::from_utf8(result.stdout)?,
            Some(2) if create => {
                host.run(host.cmd("useradd").args([
                    "--system",
                    "--user-group",
                    "--no-create-home",
                    "--home-dir",
                    "/nonexistent",
                    "--shell",
                    "/usr/sbin/nologin",
                    role.user,
                ]))?;
                host.out(host.cmd("getent").args(["passwd", role.user]))?
            }
            _ => return Err("service account lookup failed".into()),
        };
        let (name, account, home, shell) = passwd_ids(record.trim_end())?;
        require(
            name == role.user
                && home == "/nonexistent"
                && shell == "/usr/sbin/nologin"
                && account.uid != 0
                && account.gid != 0
                && account.uid != trusted.uid
                && account.gid != trusted.gid
                && accounts.iter().all(|(_, old): &(&Role, Account)| {
                    old.uid != account.uid && old.gid != account.gid
                }),
            "invalid service identity",
        )?;
        let group = host.out(
            host.cmd("getent")
                .args(["group", &fmt(format_args!("{}", account.gid))?]),
        )?;
        let mut fields = group.split(':');
        require(
            fields.next() == Some(name)
                && fields.nth(1).and_then(|n| n.parse::<u32>().ok()) == Some(account.gid)
                && fields
                    .next()
                    .is_some_and(|members| members.split(',').all(|m| m.is_empty() || m == name))
                && fields.next().is_none(),
            "invalid service group",
        )?;
        let all = host.out(host.cmd("getent").arg("passwd"))?;
        let mut matches = 0;
        for entry in all.lines() {
            let (other, ids, _, _) = passwd_ids(entry)?;
            if ids.uid == account.uid || ids.gid == account.gid {
                require(other == name, "shared service uid or primary group")?;
                matches += 1;
            }
        }
        require(matches == 1, "missing or duplicate passwd entry")?;
        let groups = host.out(host.cmd("id").args(["-G", name]))?;
        require(
            !groups.is_empty()
                && groups
                    .split_whitespace()
                    .all(|g| g.parse::<u32>() == Ok(account.gid)),
            "service supplementary groups",
        )?;
        let status = host.out(host.cmd("passwd").args(["-S", name]))?;
        let mut fields = status.split_whitespace();
        require(
            fields.next() == Some(name) && fields.next() == Some("L"),
            "service login is unlocked",
        )?;
        td_alloc::push(&mut accounts, (role, account))?;
    }
    Ok(accounts)
}

fn as_user(host: &Host, user: &str, program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = host.cmd("runuser");
    command.args(["-u", user, "--"]).arg(program);
    command
}

fn run_helper(host: &Host, user: &str, args: &[&str], input: &[u8]) -> R<Output> {
    let executable = std::env::current_exe()?;
    files::trusted_binary(&executable)?;
    host.exec(
        as_user(host, user, executable)
            .arg("__isolation")
            .args(args),
        input,
        Duration::from_secs(40),
        true,
    )
}

pub(crate) fn helper(args: &[String]) -> R<()> {
    scoped::dispatch(args)
}

pub(crate) fn prepare_notifications(host: &Host) -> R<()> {
    run_helper(host, LEGACY, &["notifications"], &[])?;
    Ok(())
}

pub(crate) fn transfer_clocks(host: &Host, to_isolated: bool) -> R<()> {
    for name in ["relay", "migadu"] {
        let role = role(name)?;
        let shared = (LEGACY, "shared");
        let isolated = (role.user, "isolated");
        let (source, destination) = if to_isolated {
            (shared, isolated)
        } else {
            (isolated, shared)
        };
        let record = run_helper(host, source.0, &["clock-read", name, source.1], &[])?.stdout;
        require(
            matches!(record.len(), 0 | 96),
            "invalid relay clock transfer",
        )?;
        run_helper(
            host,
            destination.0,
            &["clock-write", name, destination.1],
            &record,
        )?;
    }
    Ok(())
}

fn read_legacy(host: &Host, name: &str, limit: usize) -> R<Vec<u8>> {
    let file = child(Path::new(MAIL), name)?;
    let maximum = fmt(format_args!("{}", limit + 1))?;
    let out = host.exec(
        as_user(host, LEGACY, "head")
            .args(["-c", &maximum])
            .arg(file),
        &[],
        Duration::from_secs(10),
        true,
    )?;
    require(out.stdout.len() <= limit, "oversized deployment input")?;
    Ok(out.stdout)
}

fn authority(uri: &str) -> R<&str> {
    let (secret, remainder) = uri
        .trim()
        .strip_prefix("postgresql://td_mta_runtime:")
        .and_then(|s| s.split_once('@'))
        .ok_or("unexpected production runtime connection")?;
    require(
        !secret.is_empty() && !secret.bytes().any(|b| b.is_ascii_whitespace() || b == b'/'),
        "unexpected runtime password syntax",
    )?;
    let host = remainder
        .strip_suffix("/td_mta?sslmode=verify-full&sslrootcert=/var/lib/td-mta/mail/tls/ca.pem")
        .ok_or("unexpected production TLS settings")?;
    require(
        matches!(host, "localhost:35432" | "127.0.0.1:35432"),
        "unexpected database address",
    )?;
    Ok(host)
}

fn make_directory(path: &Path, mode: u32) -> R<()> {
    DirBuilder::new().mode(mode).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn private_file(path: &Path, contents: &[u8], account: Account) -> R<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    chown(path, Some(account.uid), Some(account.gid))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

// Construct and chown the new tree behind an unopened root-only directory.
// Once published, only the service user descends into it.
fn prepare(base: &Path, role: &Role, account: Account, uri: &[u8], ca: &[u8]) -> R<()> {
    let home = child(base, role.name)?;
    if matches!(fs::symlink_metadata(&home), Err(e) if e.kind() == ErrorKind::NotFound) {
        let private = td_fs::TempDir::new_in(base, ".identity")?;
        let stage = child(private.path(), role.name)?;
        let tls = child(&stage, "tls")?;
        let profile = child(&stage, role.name)?;
        let runtime = child(&profile, "runtime")?;
        let scratch = child(&profile, "scratch")?;
        for dir in [&stage, &tls] {
            make_directory(dir, 0o700)?;
        }
        if role.name != "acme" {
            for dir in [&profile, &runtime, &scratch] {
                make_directory(dir, 0o700)?;
            }
        }
        private_file(&child(&stage, "runtime-connection")?, uri, account)?;
        private_file(&child(&tls, "ca.pem")?, ca, account)?;
        for dir in [&scratch, &runtime, &profile, &tls, &stage] {
            if role.name == "acme" && dir != &tls && dir != &stage {
                continue;
            }
            chown(dir, Some(account.uid), Some(account.gid))?;
            File::open(dir)?.sync_all()?;
        }
        fs::rename(stage, &home)?;
        File::open(base)?.sync_all()?;
    }
    let meta = fs::symlink_metadata(home)?;
    require(
        meta.is_dir()
            && meta.mode() & 0o7777 == 0o700
            && meta.uid() == account.uid
            && meta.gid() == account.gid,
        "service root ownership or mode changed",
    )
}

pub(crate) fn provision(host: &Host, passwords: &Passwords) -> R<()> {
    for binary in [BIN, "/opt/td-mta/bin/td-mta-operator"] {
        files::trusted_binary(Path::new(binary))?;
    }
    let base = Path::new(BASE);
    files::safe_parents(base)?;
    match DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => {
            fs::set_permissions(base, fs::Permissions::from_mode(0o711))?;
            File::open("/var/lib")?.sync_all()?;
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(base)?;
    require(
        meta.is_dir() && meta.uid() == 0 && meta.gid() == 0 && meta.mode() & 0o7777 == 0o711,
        "invalid shared service root",
    )?;
    let accounts = accounts(host, true)?;
    let legacy = read_legacy(host, "runtime-connection", 16384)?;
    let server = authority(std::str::from_utf8(&legacy)?)?;
    let ca = read_legacy(host, "tls/ca.pem", 128 * 1024)?;
    let pem = std::str::from_utf8(&ca)?;
    require(
        pem.contains("-----BEGIN CERTIFICATE-----") && !pem.contains("PRIVATE KEY"),
        "unexpected database CA",
    )?;
    prepare_notifications(host)?;
    for (role, account) in accounts {
        let pass = passwords
            .get(role.name)
            .ok_or("missing isolated password")?;
        require(
            pass.len() == 64
                && pass
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid isolated password",
        )?;
        let uri = fmt(format_args!("postgresql://td_mta_{}:{pass}@{server}/td_mta?sslmode=verify-full&sslrootcert={BASE}/{}/tls/ca.pem",role.name,role.name))?;
        prepare(base, role, account, uri.as_bytes(), &ca)?;
        let cert = fmt(format_args!("{BASE}/{}/tls/ca.pem", role.name))?;
        for (path, bytes) in [
            (role.connection, uri.as_bytes()),
            (cert.as_str(), ca.as_slice()),
        ] {
            host.exec(
                as_user(host, role.user, "cmp").args(["-s", path, "-"]),
                bytes,
                Duration::from_secs(10),
                true,
            )?;
        }
    }
    Ok(())
}

fn verify_unit(host: &Host, unit: &str, user: &str) -> R<()> {
    let props = host.out(host.cmd("systemctl").args([
        "show",
        unit,
        "-p",
        "LoadState",
        "-p",
        "User",
        "-p",
        "Group",
    ]))?;
    require(
        props.lines().count() == 3
            && props.lines().any(|l| l == "LoadState=loaded")
            && props.lines().any(|l| l.strip_prefix("User=") == Some(user))
            && props
                .lines()
                .any(|l| l.strip_prefix("Group=") == Some(user)),
        "unexpected effective systemd identity",
    )
}

fn status_field<'a>(status: &'a str, field: &str) -> R<&'a str> {
    status
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| (name == field).then(|| value.trim()))
        .ok_or("missing process credential field".into())
}

fn verify_live(host: &Host, role: &Role, account: Account) -> R<()> {
    let pid: u32 = host
        .out(
            host.cmd("systemctl")
                .args(["show", role.unit, "-p", "MainPID", "--value"]),
        )?
        .parse()?;
    require(pid > 1, "missing running service")?;
    let proc = PathBuf::from(fmt(format_args!("/proc/{pid}"))?);
    let status = fs::read_to_string(child(&proc, "status")?)?;
    for (field, id) in [
        ("Uid", account.uid),
        ("Gid", account.gid),
        ("Groups", account.gid),
    ] {
        let fields = status_field(&status, field)?;
        require(
            (field == "Groups" || fields.split_whitespace().count() == 4)
                && fields
                    .split_whitespace()
                    .all(|v| v.parse::<u32>() == Ok(id)),
            "unexpected live uid or gid",
        )?;
    }
    require(
        status_field(&status, "NoNewPrivs")? == "1",
        "live service allows new privileges",
    )?;
    let allowed = if role.low_port { 1 << 10 } else { 0 };
    for key in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        require(
            u64::from_str_radix(status_field(&status, key)?, 16)? & !allowed == 0,
            "unexpected live capabilities",
        )?;
    }
    let mounted = fs::metadata(child(&proc, "root/var/lib/td-mta/mail")?)?;
    let source = fs::metadata(child(Path::new(BASE), role.name)?)?;
    require(
        mounted.ino() == source.ino() && mounted.dev() == source.dev(),
        "private service mount mismatch",
    )
}

pub(crate) fn verify(host: &Host, live: bool) -> R<()> {
    require(installed()?, "isolated unit settings missing")?;
    for (role, account) in accounts(host, false)? {
        verify_unit(host, role.unit, role.user)?;
        if role.name != "acme" {
            verify_unit(
                host,
                &fmt(format_args!("td-mta-maint-mail-{}.service", role.name))?,
                role.user,
            )?;
            for performance in [false, true] {
                let mut check = as_user(host, LEGACY, "/usr/bin/sudo");
                check.args(["-n", "-l", "-u", role.user, "--", BIN]);
                check
                    .args(files::status_args(performance))
                    .args(["--runtime", role.runtime]);
                host.run(&mut check)?;
            }
        }
        run_helper(host, role.user, &["check-dac", role.name], &[])?;
        if live {
            verify_live(host, role, account)?;
        }
    }
    Ok(())
}
