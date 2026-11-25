//! What early retirement relies on in place of the window: a ready
//! scheduled backup newer than every piece it removes. The destination
//! and maximum age are those systemd runs the backup's status job with,
//! and the backup job must write that directory. The newest archive is
//! judged by the receipt beside it with the rules of `install-td-mta
//! --backup-status` (builder/src/install_mta_backup.rs, which notes this
//! copy); the health checks also run that job itself before any removal.
use super::*;
use crate::activate::NO_BACKUP;

/// The scheduled backup's status job, and the backup's own timer and
/// job, as systemd's bus names them.
const STATUS_JOB: &str =
    "/org/freedesktop/systemd1/unit/td_2dmta_2dmaint_2dbackup_2dstatus_2eservice";
const BACKUP_TIMER: &str = "/org/freedesktop/systemd1/unit/td_2dmta_2dmaint_2dbackup_2etimer";
const BACKUP_JOB: &str = "/org/freedesktop/systemd1/unit/td_2dmta_2dmaint_2dbackup_2eservice";
/// The status job's maximum age when its command names none, and its
/// bound on one.
const MAXIMUM_AGE: u64 = 172_800;
const AGE_BOUND: u64 = 31_536_000;
/// The status job's bound on the directory's entries.
const ENTRIES: usize = 4096;
/// An off-host copy's marker, whose receipts the status job reads
/// otherwise; never the scheduled backup's directory.
const RECEIVER: &str = "offhost-receiver-v1";
const UNEXPECTED: &str = "td-mta-maint-backup-status.service does not run install-td-mta --backup-status --destination DIR [--maximum-age-seconds N]: early retirement cannot find the scheduled backup";
/// The clause every refusal ends with: the rerun's own job directory is
/// material too.
const RETRY: &str =
    "retry once a backup started after this run has ended (rollback_retire_not_before)";

/// The scheduled backup as its status job would judge it now.
pub(super) struct Backup {
    pub(super) destination: PathBuf,
    /// The newest archive and its receipt's creation time.
    pub(super) newest: Option<(PathBuf, u64)>,
    /// The status job's word: `ready`, `missing`, `stale`, `future` or
    /// `last-attempt-failed`.
    pub(super) status: &'static str,
}

impl Backup {
    pub(super) fn json(&self) -> R<Json> {
        let mut object = Json::Obj(Vec::new());
        let text =
            |path: &Path| -> R<Json> { Ok(Json::Str(owned(path.to_str().ok_or("backup path")?)?)) };
        set(&mut object, "destination", text(&self.destination)?)?;
        set(&mut object, "status", Json::Str(owned(self.status)?))?;
        if let Some((archive, created)) = &self.newest {
            set(&mut object, "archive", text(archive)?)?;
            set(&mut object, "created_unix_seconds", Json::from(*created))?;
        }
        Ok(object)
    }
}

/// The arguments after argv[0] of the one command systemd runs for a
/// job (its `ExecStartEx`), whose flags at most turn off expansion.
fn one_command(property: &Json) -> Option<&[Json]> {
    let [row] = property.get("data")?.as_arr()? else {
        return None;
    };
    let [path, argv, flags, ..] = row.as_arr()? else {
        return None;
    };
    let (first, rest) = argv.as_arr()?.split_first()?;
    let flags = flags.as_arr()?;
    (flags.len() <= 1
        && flags
            .iter()
            .all(|flag| flag.as_str() == Some("no-env-expand"))
        && path.as_str().is_some()
        && first == path)
        .then_some(rest)
}

/// An absolute directory systemd passes through unchanged, with no `.`
/// or `..` component.
fn plain_directory(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_.-".contains(&b))
        && path.split('/').all(|part| part != "." && part != "..")
}

/// The destination and maximum age of the status job's command.
fn status_command(property: &Json) -> R<(PathBuf, u64)> {
    let rest = one_command(property).ok_or(UNEXPECTED)?;
    let mut words = rest.iter().map(Json::as_str);
    require(
        words.next() == Some(Some("install-td-mta"))
            && words.next() == Some(Some("--backup-status")),
        UNEXPECTED,
    )?;
    let mut destination = None;
    let mut age = None;
    while let Some(flag) = words.next() {
        let value = words.next().flatten().ok_or(UNEXPECTED)?;
        match flag {
            Some("--destination") if destination.is_none() => destination = Some(value),
            Some("--maximum-age-seconds") if age.is_none() => {
                age = Some(value.parse::<u64>().map_err(|_| UNEXPECTED)?)
            }
            _ => return Err(UNEXPECTED.into()),
        }
    }
    let destination = destination.ok_or(UNEXPECTED)?;
    let age = age.unwrap_or(MAXIMUM_AGE);
    require(
        plain_directory(destination) && (1..=AGE_BOUND).contains(&age),
        UNEXPECTED,
    )?;
    Ok((PathBuf::from(owned(destination)?), age))
}

/// The one directory the backup job's command names with `--destination`.
fn backup_destination(property: &Json) -> R<&str> {
    const OTHER: &str = "td-mta-maint-backup.service does not run install-td-mta --backup with one --destination: early retirement cannot tell where the scheduled backup is written";
    let rest = one_command(property).ok_or(OTHER)?;
    let mut words = rest.iter().map(Json::as_str);
    require(
        words.next() == Some(Some("install-td-mta")) && words.next() == Some(Some("--backup")),
        OTHER,
    )?;
    let mut destination = None;
    while let Some(word) = words.next() {
        if word == Some("--destination") {
            require(destination.is_none(), OTHER)?;
            destination = Some(words.next().flatten().ok_or(OTHER)?);
        }
    }
    Ok(destination.ok_or(OTHER)?)
}

/// `backup-STAMP-PID.dump`'s stamp, as the status job reads it.
fn archive_stamp(name: &str) -> Option<u64> {
    let stem = name.strip_prefix("backup-")?.strip_suffix(".dump")?;
    let (stamp, pid) = stem.split_once('-')?;
    pid.parse::<u32>().ok()?;
    stamp.parse().ok()
}

/// The status job's judgement of an archive created at `created`.
fn freshness(created: u64, now: u64, maximum_age: u64, last_error: bool) -> &'static str {
    if created > now {
        "future"
    } else if now - created > maximum_age {
        "stale"
    } else if last_error {
        "last-attempt-failed"
    } else {
        "ready"
    }
}

/// The scheduled backup in `destination` at `now`: the newest archive,
/// whose receipt must match it, judged as its status job judges it.
/// What the status job refuses to read is an error here too.
fn scheduled(destination: &Path, maximum_age: u64, now: u64) -> R<Backup> {
    let named = destination.to_str().ok_or("backup path")?;
    let meta = fs::symlink_metadata(destination)?;
    require(
        meta.is_dir() && meta.mode() & 0o777 == 0o700,
        "it is not a private directory",
    )?;
    let owner = meta.uid();
    // The newest by stamp, then name, as the status job orders them.
    let mut newest: Option<(u64, String, u64)> = None;
    for (i, entry) in fs::read_dir(destination)?.enumerate() {
        require(i < ENTRIES, "it has too many entries")?;
        let entry = entry?;
        let meta = entry.metadata()?;
        require(
            meta.is_file() && meta.uid() == owner && meta.mode() & 0o077 == 0,
            "it holds something other than its owner's private files",
        )?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("it holds a name that is not UTF-8")?;
        require(name != RECEIVER, "it is an off-host copy's directory")?;
        let Some(stamp) = archive_stamp(name) else {
            continue;
        };
        if newest
            .as_ref()
            .is_none_or(|(s, n, _)| (stamp, name) > (*s, n.as_str()))
        {
            newest = Some((stamp, owned(name)?, meta.len()));
        }
    }
    let Some((_, name, bytes)) = newest else {
        return Ok(Backup {
            destination: PathBuf::from(owned(named)?),
            newest: None,
            status: "missing",
        });
    };
    let archive = child(destination, &name)?;
    let stem = name.strip_suffix(".dump").ok_or("archive name")?;
    let receipt = child(destination, &fmt(format_args!("{stem}.json"))?)?;
    let receipt = td_json::parse_slice(&td_fs::read_bounded_real_file(
        &receipt,
        "backup receipt",
        65536,
    )?)?;
    require(
        receipt.get("schema").and_then(Json::as_u64) == Some(1)
            && bytes != 0
            && receipt.get("bytes").and_then(Json::as_u64) == Some(bytes)
            && receipt.get("archive").and_then(Json::as_str) == archive.to_str(),
        "its newest archive's receipt does not match the archive",
    )?;
    let created = receipt
        .get("created_unix_seconds")
        .and_then(Json::as_u64)
        .ok_or("its newest archive's receipt has no creation time")?;
    let last_error = match fs::symlink_metadata(child(destination, "last-error")?) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    Ok(Backup {
        destination: PathBuf::from(owned(named)?),
        newest: Some((archive, created)),
        status: freshness(created, now, maximum_age, last_error),
    })
}

/// Early retirement may go on: the backup is ready and, once the
/// material is known, newer than its newest last write.
fn guard(backup: &Backup, newest: Option<u64>) -> R<()> {
    let destination = backup.destination.to_str().ok_or("backup path")?;
    if backup.status != "ready" {
        return Err(fmt(format_args!(
            "{NO_BACKUP} the rollback material: the scheduled backup in {destination} is {}; {RETRY}",
            backup.status
        ))?
        .into());
    }
    match (newest, &backup.newest) {
        (None, _) => Ok(()),
        (Some(newest), Some((_, created))) if *created > newest => Ok(()),
        (Some(newest), Some((archive, created))) => Err(fmt(format_args!(
            "{NO_BACKUP} {newest}, the newest rollback material's last write: the newest scheduled backup, {}, was created at {created}; {RETRY}",
            archive.to_str().ok_or("backup path")?
        ))?
        .into()),
        (Some(_), None) => Err(fmt(format_args!(
            "{NO_BACKUP} the rollback material: {destination} holds no archive; {RETRY}"
        ))?
        .into()),
    }
}

/// What retiring at `now` removes from `material`: with `all`, every
/// piece once `backup` (which `all` requires) passes the guard; else
/// what `due` finds past the window, reading no backup.
pub(super) fn retiring<'a>(
    material: &'a [Material],
    now: u64,
    all: bool,
    backup: Option<&Backup>,
) -> R<(Vec<&'a Path>, Option<u64>)> {
    if all {
        let backup = backup.ok_or("early retirement without a judged scheduled backup")?;
        guard(backup, material.iter().map(|item| item.since).max())?;
    }
    due(material, now, all)
}

/// When a backup the timer starts at `start` (microseconds) has ended,
/// given the job's start timeout: the Unix second, if both are known.
fn finished_by(start: u64, timeout: u64) -> Option<u64> {
    if start == 0 {
        return None;
    }
    Some(start.checked_add(timeout)?.div_ceil(1_000_000))
}

/// The value of busctl's `--json=short` reply for an unsigned property.
fn unsigned(reply: &str) -> Option<u64> {
    td_json::parse(reply).ok()?.get("data")?.as_u64()
}

fn property(host: &Host, path: &str, interface: &str, name: &str) -> Option<u64> {
    let reply = host
        .out(host.cmd("busctl").args([
            "--json=short",
            "get-property",
            "org.freedesktop.systemd1",
            path,
            interface,
            name,
        ]))
        .ok()?;
    unsigned(&reply)
}

impl Deployment {
    /// The scheduled backup now: the directory its status job reads,
    /// which its backup job writes, judged as that status job judges it.
    pub(super) fn scheduled_backup(&self) -> R<Backup> {
        let (destination, age) =
            status_command(&crate::readiness::exec_start(&self.host, STATUS_JOB)?)?;
        let job = crate::readiness::exec_start(&self.host, BACKUP_JOB)?;
        require(
            Path::new(backup_destination(&job)?) == destination,
            "td-mta-maint-backup.service writes another directory than td-mta-maint-backup-status.service reads",
        )?;
        let named = destination.to_str().ok_or("backup path")?;
        scheduled(&destination, age, now_secs()?).map_err(|e| {
            match fmt(format_args!(
                "cannot judge the scheduled backup in {named}: {e}"
            )) {
                Ok(text) => text.into(),
                Err(error) => error,
            }
        })
    }

    /// Early retirement before the material is known: the backup must
    /// already be ready.
    pub(super) fn backup_ready(&mut self, backup: &Backup) -> R<()> {
        let result = guard(backup, None);
        self.awaiting_backup(result)
    }

    /// `result`; on a refusal, first records when the next scheduled
    /// backup will have ended, if systemd can say.
    pub(super) fn awaiting_backup<T>(&mut self, result: R<T>) -> R<T> {
        if result.is_ok() {
            return result;
        }
        let start = property(
            &self.host,
            BACKUP_TIMER,
            "org.freedesktop.systemd1.Timer",
            "NextElapseUSecRealtime",
        );
        let timeout = property(
            &self.host,
            BACKUP_JOB,
            "org.freedesktop.systemd1.Service",
            "TimeoutStartUSec",
        );
        if let Some(time) = start.zip(timeout).and_then(|(s, t)| finished_by(s, t)) {
            set(
                &mut self.record,
                "rollback_retire_not_before",
                Json::from(time),
            )?;
        }
        result
    }
}

#[cfg(test)]
#[path = "tests/retire_backup.rs"]
mod tests;
