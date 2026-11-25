//! File operations performed with the service's own credentials.
use super::{role, status_field, Account, Role, BASE, BIN, NOTIFY, ROLES};
use crate::common::{child, fmt, owned, random_hex, require};
use crate::R;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use td_json::Json;

const NOTIFICATION_LIMIT: usize = 3 * 1024 * 1024;
const JOURNAL_SIZE: usize = 96;
#[path = "isolation_path_sys.rs"]
mod path_sys;

fn proc_path(file: &File) -> R<PathBuf> {
    Ok(PathBuf::from(fmt(format_args!(
        "/proc/self/fd/{}",
        file.as_raw_fd()
    ))?))
}

fn own_account() -> R<Account> {
    let status = fs::read_to_string("/proc/self/status")?;
    let id = |key| -> R<u32> {
        Ok(status_field(&status, key)?
            .split_whitespace()
            .nth(1)
            .ok_or("missing effective identity")?
            .parse()?)
    };
    Ok(Account {
        uid: id("Uid")?,
        gid: id("Gid")?,
    })
}

fn normal_name(component: Component<'_>) -> R<&str> {
    if let Component::Normal(name) = component {
        return name.to_str().ok_or("invalid clock path component".into());
    }
    Err("invalid clock path component".into())
}

fn private_runtime(root: &File, relative: &Path, uid: u32) -> R<File> {
    let mut current = root.try_clone()?;
    for component in relative.components() {
        let next = path_sys::directory(&current, normal_name(component)?)?;
        let metadata = next.metadata()?;
        if (metadata.uid() != 0 && metadata.uid() != uid) || metadata.mode() & 0o022 != 0 {
            return Err(std::io::Error::from(ErrorKind::PermissionDenied).into());
        }
        current = next;
    }
    let metadata = current.metadata()?;
    if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(std::io::Error::from(ErrorKind::PermissionDenied).into());
    }
    Ok(current)
}

fn private_mode(m: &Metadata, uid: u32) -> bool {
    m.is_file() && m.uid() == uid && m.nlink() == 1 && m.mode() & 0o7777 == 0o600
}

fn publish(directory: &Path, name: &str, bytes: &[u8]) -> R<()> {
    let temporary = child(
        directory,
        &fmt(format_args!(".td-mta-pending-{}", random_hex(16)?))?,
    )?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> R<()> {
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temporary, child(directory, name)?)?;
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        match fs::remove_file(temporary) {
            Ok(()) => (),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    result
}

pub(super) fn read_clock(root: &File, path: &Path, uid: u32) -> R<Option<[u8; JOURNAL_SIZE]>> {
    let read = || -> R<Option<[u8; JOURNAL_SIZE]>> {
        let directory = private_runtime(root, path, uid)?;
        let inode = path_sys::journal(&directory)?;
        let m = inode.metadata()?;
        if !private_mode(&m, uid) || m.len() != JOURNAL_SIZE as u64 {
            return Ok(None);
        }
        let mut data = [0; JOURNAL_SIZE];
        let mut f = File::open(proc_path(&inode)?)?;
        match f.read_exact(&mut data) {
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            other => other?,
        }
        Ok((f.read(&mut [0])? == 0).then_some(data))
    };
    match read() {
        Err(e)
            if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::PermissionDenied
                ) || e.raw_os_error() == Some(40)
            }) =>
        {
            Ok(None)
        }
        result => result,
    }
}
pub(super) fn write_clock(root: &File, path: &Path, uid: u32, bytes: &[u8]) -> R<()> {
    let directory = private_runtime(root, path, uid)?;
    let path = proc_path(&directory)?;
    if bytes.is_empty() {
        match fs::remove_file(child(&path, "queue-clock")?) {
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            other => other?,
        }
        File::open(path)?.sync_all()?;
    } else {
        require(bytes.len() == JOURNAL_SIZE, "invalid clock transfer size")?;
        publish(&path, "queue-clock", bytes)?;
    }
    Ok(())
}

fn status_arguments(role: &Role, performance: bool, isolated: bool) -> R<Json> {
    let mut args = Vec::new();
    if isolated {
        for word in ["/usr/bin/sudo", "-n", "-u", role.user, "--"] {
            td_alloc::push(&mut args, Json::Str(owned(word)?))?;
        }
    }
    td_alloc::push(&mut args, Json::Str(owned(BIN)?))?;
    for word in super::files::status_args(performance) {
        td_alloc::push(&mut args, Json::Str(owned(word)?))?;
    }
    td_alloc::push(&mut args, Json::Str(owned("--runtime")?))?;
    td_alloc::push(
        &mut args,
        Json::Str(owned(if isolated {
            role.runtime
        } else {
            role.legacy_runtime
        })?),
    )?;
    Ok(Json::Arr(args))
}

pub(super) fn notification_config(config: &mut Json) -> R<()> {
    let Some(Json::Arr(checks)) = config.get_mut("checks") else {
        return Err("missing notification checks".into());
    };
    for role in ROLES.iter().filter(|role| role.name != "acme") {
        for (prefix, performance) in [("mail-", false), ("performance-", true)] {
            let name = fmt(format_args!("{prefix}{}", role.name))?;
            let mut matches = checks
                .iter_mut()
                .filter(|check| check.get("name").and_then(Json::as_str) == Some(name.as_str()));
            let check = matches.next().ok_or("missing legacy notification check")?;
            require(matches.next().is_none(), "duplicate notification check")?;
            let argv = check
                .get_mut("argv")
                .ok_or("missing notification command")?;
            require(
                *argv == status_arguments(role, performance, false)?,
                "unexpected legacy notification command",
            )?;
            *argv = status_arguments(role, performance, true)?;
        }
    }
    Ok(())
}

pub(super) fn notifications(dir: &Path, uid: u32) -> R<()> {
    let file = child(dir, "config.json")?;
    let source = td_fs::read_bounded_real_file(
        &file,
        "notification configuration",
        NOTIFICATION_LIMIT as u64,
    )?;
    let mut config = td_json::parse(std::str::from_utf8(&source)?)
        .map_err(|_| "invalid notification configuration")?;
    notification_config(&mut config)?;
    let encoded = fmt(format_args!("{config}\n"))?;
    require(
        encoded.len() <= NOTIFICATION_LIMIT,
        "oversized isolated notification configuration",
    )?;
    let dest = child(dir, "isolated.json")?;
    match td_fs::open_real_file(&dest, "isolated notification configuration") {
        Ok((f, m)) => {
            require(
                private_mode(&m, uid),
                "invalid isolated notification configuration",
            )?;
            if m.len() == encoded.len() as u64 {
                let mut bytes = td_alloc::with_capacity(encoded.len() + 1)?;
                f.take(encoded.len() as u64 + 1).read_to_end(&mut bytes)?;
                if bytes == encoded.as_bytes() {
                    return Ok(());
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    publish(dir, "isolated.json", encoded.as_bytes())
}

fn private_entry(path: &Path, account: Account, directory: bool) -> R<()> {
    let m = fs::symlink_metadata(path)?;
    require(
        m.uid() == account.uid
            && m.gid() == account.gid
            && if directory {
                m.is_dir() && m.mode() & 0o7777 == 0o700
            } else {
                m.is_file() && m.nlink() == 1 && m.mode() & 0o7777 == 0o600
            },
        "service path ownership or mode",
    )
}

fn inaccessible(path: &Path) -> R<()> {
    // POSIX test applies the kernel's access decision, including POSIX ACLs.
    let result = Command::new("/usr/bin/test")
        .arg("-r")
        .arg(path)
        .args(["-o", "-w"])
        .arg(path)
        .args(["-o", "-x"])
        .arg(path)
        .status()?;
    require(
        result.code() == Some(1),
        "private path accessible to service",
    )
}

pub(super) fn check_dac(base: &Path, role: &Role, account: Account, forbidden: &[&str]) -> R<()> {
    let home = child(base, role.name)?;
    private_entry(&home, account, true)?;
    private_entry(&child(&home, "runtime-connection")?, account, false)?;
    if role.name != "acme" {
        let profile = child(&home, role.name)?;
        for path in [
            &profile,
            &child(&profile, "runtime")?,
            &child(&profile, "scratch")?,
        ] {
            private_entry(path, account, true)?;
        }
    }
    for file in forbidden {
        inaccessible(Path::new(file))?;
    }
    for peer in ROLES.iter().filter(|other| other.name != role.name) {
        inaccessible(&child(&child(base, peer.name)?, "runtime-connection")?)?;
    }
    Ok(())
}

const FORBIDDEN: &[&str] = &[
    "/var/lib/td-mta/mail/owner-connection",
    "/var/lib/td-mta/mail/owner-password",
    "/var/lib/td-mta/mail/runtime-connection",
    "/var/lib/td-mta/mail/runtime-password",
    "/var/lib/td-mta/mail/postgres",
    "/var/lib/td-mta/mail/.operations/notifications",
    "/var/lib/td-mta/backup-control/service.conf",
    "/var/lib/td-mta/backups/scheduled",
    "/var/lib/td-mta-operator",
    "/data/td-mta-physical-recovery",
    "/data/td-mta/backups/scheduled",
];

pub(super) fn dispatch(args: &[String]) -> R<()> {
    let account = own_account()?;
    require(
        account.uid != 0,
        "isolation helper requires service credentials",
    )?;
    match args {
        [op] if op == "notifications" => notifications(Path::new(NOTIFY), account.uid),
        [op, name] if op == "check-dac" => {
            check_dac(Path::new(BASE), role(name)?, account, FORBIDDEN)
        }
        [op, name, location] if matches!(name.as_str(), "relay" | "migadu") => {
            let role = role(name)?;
            let path = match location.as_str() {
                "isolated" => role.runtime,
                "shared" => role.legacy_runtime,
                _ => return Err("invalid clock location".into()),
            };
            let root = File::open("/")?;
            let path = Path::new(path).strip_prefix("/")?;
            match op.as_str() {
                "clock-read" => {
                    if let Some(record) = read_clock(&root, path, account.uid)? {
                        std::io::stdout().lock().write_all(&record)?;
                    }
                    Ok(())
                }
                "clock-write" => {
                    let mut data = td_alloc::with_capacity(JOURNAL_SIZE + 1)?;
                    std::io::stdin()
                        .lock()
                        .take(JOURNAL_SIZE as u64 + 1)
                        .read_to_end(&mut data)?;
                    write_clock(&root, path, account.uid, &data)
                }
                _ => Err("unknown clock operation".into()),
            }
        }
        _ => Err("unknown isolation helper operation".into()),
    }
}
