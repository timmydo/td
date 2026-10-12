//! The verified account handoff's consumer (td-install/ENCRYPTION.md,
//! "Verified account handoff"; td-authd/DESIGN.md amendment 9). At a
//! paired generation's Prepare root reads and unlinks the admission record
//! `td-boot admit` left on the system's `/run`, creates the consumed marker
//! exclusively, and only then judges it. Only a protected release leaves
//! the directory, so whatever it holds the boot enforces, through the
//! login predicate, and every Prepare that finds it makes or retries the
//! persistent protected marker; a primary release's record for this
//! account and deployment, within its age, admits the generation once
//! that marker exists.

use crate::admission::{self, Record};
use crate::login_status::login_state::{self, Owner};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const NOFOLLOW: i32 = 0o400000;
const NONBLOCK: i32 = 0o4000;
/// `/run/td-deployment`: the running deployment's ID, 64 lowercase hex
/// digits and a newline, which the deployment init writes.
const DEPLOYMENT: &str = "td-deployment";
const DEPLOYMENT_READ: u64 = 80;
/// One line of `/proc/uptime` is far shorter.
const UPTIME_READ: u64 = 128;
/// `/proc/self/mountinfo` is read whole, to this bound.
const MOUNTINFO_READ: u64 = 1 << 20;

/// Where consumption reads and writes: the production paths, or a test's.
pub(crate) struct Paths {
    /// The system's `/run`, which must be the tmpfs mounted there.
    pub run: PathBuf,
    /// What the login predicate reads under, `/` in production, and the
    /// owner its directory, the record and the markers must have.
    pub root: PathBuf,
    pub owner: Owner,
    pub mountinfo: PathBuf,
    /// `CLOCK_BOOTTIME`'s seconds, `/proc/uptime`'s first field.
    pub uptime: PathBuf,
}

impl Paths {
    pub fn production() -> Self {
        Self {
            run: PathBuf::from("/run"),
            root: PathBuf::from("/"),
            owner: Owner::ROOT,
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            uptime: PathBuf::from("/proc/uptime"),
        }
    }
}

/// What a Prepare's consumption found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No record: an unprotected boot, or one already consumed.
    Absent,
    /// No record, on a protected boot whose persistent marker could not
    /// be made.
    Unmarked(String),
    /// The record was refused, or an earlier consumption's marker exists:
    /// nothing is judged and nothing admitted.
    Discarded(String),
    /// A valid record that admits nothing, or one whose persistent marker
    /// could not be made.
    Protected(String),
    /// A valid record admits this generation's first frame.
    Admitted,
}

impl Outcome {
    pub fn admitted(&self) -> bool {
        *self == Self::Admitted
    }

    /// This outcome when the persistent marker failed for `why`: nothing
    /// is admitted, though the boot still enforces.
    fn unmarked(self, why: &str) -> Self {
        let marker = format!("the protected marker: {why}");
        match self {
            Self::Absent | Self::Unmarked(_) => Self::Unmarked(marker),
            Self::Admitted => Self::Protected(marker),
            Self::Discarded(was) => Self::Discarded(format!("{was}; {marker}")),
            Self::Protected(was) => Self::Protected(format!("{was}; {marker}")),
        }
    }
}

/// Reads, unlinks and judges `run/td-admit/v1` for the account `uid`.
/// Only `td-boot admit` makes `run/td-admit`, from a protected release's
/// member, so whatever it holds the persistent marker is then made, or
/// retried.
pub(crate) fn consume(paths: &Paths, uid: u32) -> Outcome {
    let directory = paths.run.join(admission::DIRECTORY);
    let held = match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Outcome::Absent,
        other => other,
    };
    let outcome = take(&directory, held, paths, uid);
    match ensure_protected(&paths.root, paths.owner) {
        Ok(()) => outcome,
        Err(why) => outcome.unmarked(&why),
    }
}

/// The record in `directory`, whose metadata is `held`, consumed and
/// judged.
fn take(directory: &Path, held: io::Result<Metadata>, paths: &Paths, uid: u32) -> Outcome {
    let held = match held {
        Err(error) => return Outcome::Discarded(format!("inspect {directory:?}: {error}")),
        Ok(meta) => meta,
    };
    if !held.is_dir() || !owned(&held, paths.owner) || held.mode() & 0o7777 != 0o700 {
        return Outcome::Discarded(format!(
            "{directory:?} is not a {}:{} mode-0700 directory",
            paths.owner.uid, paths.owner.gid
        ));
    }
    let path = directory.join(admission::RECORD);
    let present = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Outcome::Absent,
        other => other,
    };
    let read = present
        .map_err(|error| format!("inspect {path:?}: {error}"))
        .and_then(|_| read_record(&path, paths, &held));
    // The name goes whatever it held, and the marker is made before the
    // bytes are judged, so a record admits at most once a boot.
    if let Err(error) = fs::remove_file(&path) {
        return Outcome::Discarded(format!("unlink {path:?}: {error}"));
    }
    if let Err(why) = mark_consumed(&directory.join(admission::CONSUMED)) {
        return Outcome::Discarded(why);
    }
    let record = match read.and_then(|bytes| Record::decode(&bytes).map_err(String::from)) {
        Ok(record) => record,
        Err(why) => return Outcome::Discarded(why),
    };
    match judge(&record, paths, uid) {
        Ok(()) => Outcome::Admitted,
        Err(why) => Outcome::Protected(why),
    }
}

fn owned(meta: &Metadata, owner: Owner) -> bool {
    meta.uid() == owner.uid && meta.gid() == owner.gid
}

/// The record's exact bytes: a single-link regular file of the owner's,
/// mode 0600, exactly `RECORD_BYTES` long, in `directory` on the tmpfs
/// mounted at `run`, opened without following a link.
fn read_record(path: &Path, paths: &Paths, directory: &Metadata) -> Result<Vec<u8>, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(path)
        .map_err(|error| format!("open {path:?}: {error}"))?;
    let meta = file
        .metadata()
        .map_err(|error| format!("inspect {path:?}: {error}"))?;
    if !meta.is_file()
        || !owned(&meta, paths.owner)
        || meta.mode() & 0o7777 != 0o600
        || meta.nlink() != 1
        || meta.len() != admission::RECORD_BYTES as u64
    {
        return Err(format!(
            "{path:?} is not a single-link {}:{} mode-0600 file of {} bytes",
            paths.owner.uid,
            paths.owner.gid,
            admission::RECORD_BYTES
        ));
    }
    let run = run_device(paths)?;
    if meta.dev() != directory.dev() || meta.dev() != run {
        return Err(format!(
            "{path:?} is not on the tmpfs mounted at {:?}",
            paths.run
        ));
    }
    let mut bytes = Vec::with_capacity(admission::RECORD_BYTES + 1);
    (&mut file)
        .take(admission::RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {path:?}: {error}"))?;
    if bytes.len() != admission::RECORD_BYTES {
        return Err(format!("{path:?} changed length while read"));
    }
    Ok(bytes)
}

/// The device of the topmost mount at `run`, which must be tmpfs, from
/// this process's mountinfo.
fn run_device(paths: &Paths) -> Result<u64, String> {
    let text = read_bounded(&paths.mountinfo, MOUNTINFO_READ)?;
    let run = paths
        .run
        .to_str()
        .ok_or_else(|| format!("{:?} is not UTF-8", paths.run))?;
    let mount = text
        .lines()
        .rev()
        .filter_map(mount_entry)
        .find(|(_, point, _)| *point == run)
        .ok_or_else(|| format!("nothing is mounted at {run:?}"))?;
    let (device, _, kind) = mount;
    if kind != "tmpfs" {
        return Err(format!("{run:?} is {kind}, not tmpfs"));
    }
    Ok(device)
}

/// One mountinfo line's device, mount point and filesystem type.
fn mount_entry(line: &str) -> Option<(u64, &str, &str)> {
    let mut fields = line.split(' ');
    let device = fields.nth(2)?;
    let point = fields.nth(1)?;
    let kind = fields.skip_while(|field| *field != "-").nth(1)?;
    let (major, minor) = device.split_once(':')?;
    Some((
        device_number(major.parse().ok()?, minor.parse().ok()?),
        point,
        kind,
    ))
}

/// glibc's `makedev`, the encoding `st_dev` carries.
fn device_number(major: u64, minor: u64) -> u64 {
    ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
}

/// Creates `path` exclusively, mode 0600, without following a link. An
/// existing marker, of any type, means an earlier generation consumed a
/// record this boot.
fn mark_consumed(path: &Path) -> Result<(), String> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(format!(
            "{path:?} exists: a record was already consumed this boot"
        )),
        Err(error) => Err(format!("create {path:?}: {error}")),
    }
}

/// Makes the persistent protected marker in the login directory unless a
/// name is already there, syncing it and the directory.
fn ensure_protected(root: &Path, owner: Owner) -> Result<(), String> {
    let directory = login_state::Directory::open(&login_state::directory(root), owner)
        .map_err(|refusal| refusal.reason)?;
    let found = directory
        .lookup(login_state::PROTECTED)
        .map_err(|refusal| refusal.reason)?;
    if found.is_some() {
        return Ok(());
    }
    let marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(directory.at(login_state::PROTECTED));
    match marker {
        Ok(file) => file
            .sync_all()
            .map_err(|error| format!("sync the protected marker: {error}"))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create the protected marker: {error}")),
    }
    directory
        .file()
        .sync_all()
        .map_err(|error| format!("sync the login directory: {error}"))
}

/// A primary release's record for `uid` and the running deployment, read
/// within `MAX_AGE_SECONDS` of the kexec.
fn judge(record: &Record, paths: &Paths, uid: u32) -> Result<(), String> {
    if !record.method.admits() {
        return Err("a recovery or unmeasured release admits nothing".into());
    }
    if record.uid != uid || record.uid != admission::PRIMARY_UID {
        return Err(format!("the record names UID {}", record.uid));
    }
    let path = paths.run.join(DEPLOYMENT);
    let text = read_bounded(&path, DEPLOYMENT_READ)?;
    let running = text
        .strip_suffix('\n')
        .and_then(|hex| admission::manifest_id(hex.as_bytes()))
        .ok_or_else(|| format!("{path:?} names no deployment"))?;
    if record.manifest != running {
        return Err("the record names another deployment".into());
    }
    let age = boottime_centiseconds(&paths.uptime)?;
    if age > admission::MAX_AGE_SECONDS.saturating_mul(100) {
        return Err(format!(
            "the record is older than {} seconds",
            admission::MAX_AGE_SECONDS
        ));
    }
    Ok(())
}

/// `CLOCK_BOOTTIME` in centiseconds, `/proc/uptime`'s first field.
fn boottime_centiseconds(path: &Path) -> Result<u64, String> {
    let text = read_bounded(path, UPTIME_READ)?;
    let malformed = || format!("{path:?} is malformed");
    let (seconds, fraction) = text
        .split_whitespace()
        .next()
        .and_then(|uptime| uptime.split_once('.'))
        .ok_or_else(malformed)?;
    if fraction.len() != 2 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    let seconds: u64 = seconds.parse().map_err(|_| malformed())?;
    let centis: u64 = fraction.parse().map_err(|_| malformed())?;
    seconds
        .checked_mul(100)
        .and_then(|whole| whole.checked_add(centis))
        .ok_or_else(malformed)
}

fn read_bounded(path: &Path, limit: u64) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .and_then(|file| file.take(limit).read_to_string(&mut text))
        .map_err(|error| format!("read {path:?}: {error}"))?;
    Ok(text)
}

/// One console line per consumption that found a record.
pub(crate) fn log(outcome: &Outcome) {
    let line = match outcome {
        Outcome::Absent => return,
        Outcome::Unmarked(why) => format!("protected boot without a record: {why}"),
        Outcome::Discarded(why) => format!("admission record discarded: {why}"),
        Outcome::Protected(why) => format!("admission record admits nothing: {why}"),
        Outcome::Admitted => "admission record admits this generation".into(),
    };
    let _ = writeln!(io::stderr().lock(), "td-authd: {line}");
}

#[cfg(test)]
#[path = "../tests/handoff.rs"]
pub(crate) mod tests;
