//! Retiring rollback material once its window has passed: earlier job
//! directories (receipts, the original data directory a migration left,
//! uploaded closures), the physical recovery copies activations took, and
//! the retained Debian PostgreSQL tree once a verified switch left it.
//! Irreversible, so the job must say so. Early retirement (`RETIRE_ALL`)
//! removes it all whatever its age, once a ready scheduled backup is
//! newer than all of it.
use super::*;
use crate::common::DEBIAN_POSTGRES;
use fence::PG_FENCE;

#[path = "activation_retire_backup.rs"]
mod backup;

/// How long rollback material stays after the change that left it.
pub(super) const WINDOW: u64 = 7 * 24 * 3600;
/// Earlier job directories, and the recovery copies activations took.
const MATERIAL: &[&str] = &[OP, crate::physical::VOLUME];
/// A directory being removed; a rerun finishes it whatever its age.
const TOMBSTONE: &str = ".retired-";
/// When a verified switch to td's PostgreSQL left the Debian tree.
pub(super) const SWITCHED: &str = "/var/lib/td-mta-operator/debian-postgresql-retained-since";

pub(super) fn now_secs() -> R<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// The time `SWITCHED` holds; none if absent or unreadable in any way,
/// which only a verified switch, a verified rerun on td's server or a
/// rollback to a verified td closure rewrites. Until then the tree stays.
pub(super) fn switched_at(path: &Path) -> Option<u64> {
    td_fs::read_bounded(path, 64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse().ok())
}

/// Records `now` in `path` atomically: always with `restart` (a verified
/// switch, a rollback's put-back), else only where nothing valid is
/// recorded (a verified rerun). Its staged name is fixed: the activation
/// lock admits one writer.
pub(super) fn record_switch(path: &Path, now: u64, restart: bool) -> R<()> {
    if !restart && switched_at(path).is_some() {
        return Ok(());
    }
    let directory = parent(path)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("record name")?;
    let staged = child(directory, &fmt(format_args!(".{name}.new"))?)?;
    remove_file(&staged)?;
    let result = (|| -> R<()> {
        let mut file = create_file(&staged, 0o600)?;
        file.write_all(fmt(format_args!("{now}\n"))?.as_bytes())?;
        file.sync_all()?;
        fs::rename(&staged, path)?;
        sync_dir(directory)
    })();
    if result.is_err() {
        // The first error is the one to report.
        let _ = remove_file(&staged);
    }
    result
}

/// Before a switch stops the server: the time recorded so far, and none
/// left while the switch is unverified.
pub(super) fn take_switch(path: &Path) -> R<Option<u64>> {
    let window = switched_at(path);
    clear_switch(path)?;
    Ok(window)
}

/// A verified td closure starts the window at `now` (`restart`), or a
/// verified rerun where none is recorded; nothing once the tree is gone.
pub(super) fn start_window(path: &Path, tree: &Path, now: u64, restart: bool) -> R<()> {
    if !tree.try_exists()? {
        return Ok(());
    }
    record_switch(path, now, restart)
}

/// After a rollback to a td source: the time that closure's own verified
/// switch recorded, if any and while the tree exists. Whether it did.
pub(super) fn put_back(path: &Path, tree: &Path, td_source: bool, window: Option<u64>) -> R<bool> {
    match window {
        Some(window) if td_source && tree.try_exists()? => {
            record_switch(path, window, true)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// After the return to the Debian server: nothing left it.
pub(super) fn clear_switch(path: &Path) -> R<()> {
    remove_file(path)?;
    sync_dir(parent(path)?)
}

/// The Debian tree as material: while td's server is selected and a
/// verified switch recorded when it left it.
pub(super) fn debian_material(
    tree: &Path,
    record: &Path,
    td_selected: bool,
) -> R<(Option<Material>, Option<PathBuf>)> {
    let dead = tombstone(tree)?;
    let dead = if fs::symlink_metadata(&dead).is_ok() {
        Some(dead)
    } else {
        None
    };
    if !td_selected || fs::symlink_metadata(tree).is_err() {
        return Ok((None, dead));
    }
    let Some(since) = switched_at(record) else {
        return Ok((None, dead));
    };
    Ok((
        Some(Material {
            path: PathBuf::from(owned(tree.to_str().ok_or("tree path")?)?),
            since,
        }),
        dead,
    ))
}

/// One piece of rollback material and the time its window opened.
pub(super) struct Material {
    pub(super) path: PathBuf,
    pub(super) since: u64,
}

/// What retiring at `now` does with `material`: the paths past the window,
/// or every path with `all`, and the earliest time one still inside it
/// may go.
pub(super) fn due(material: &[Material], now: u64, all: bool) -> R<(Vec<&Path>, Option<u64>)> {
    let mut past = Vec::new();
    let mut next: Option<u64> = None;
    for item in material {
        let until = item.since.saturating_add(WINDOW);
        if all || until <= now {
            td_alloc::push(&mut past, item.path.as_path())?;
        } else {
            next = Some(next.map_or(until, |n| n.min(until)));
        }
    }
    Ok((past, next))
}

/// A job or recovery directory's name: `deploy-` and 8 or 24 letters or
/// digits.
pub(super) fn material_name(name: &str) -> bool {
    name.strip_prefix("deploy-")
        .is_some_and(|n| matches!(n.len(), 8 | 24) && n.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn modified(path: &Path) -> R<u64> {
    Ok(fs::symlink_metadata(path)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs())
}

/// The material in `dir` but `current`, each from its last write: the
/// later of the directory's and its receipt's. Tombstones are returned
/// apart.
pub(super) fn material_in(dir: &Path, current: &Path) -> R<(Vec<Material>, Vec<PathBuf>)> {
    let mut material = Vec::new();
    let mut tombstones = Vec::new();
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((material, tombstones)),
        other => other?,
    };
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.strip_prefix(TOMBSTONE).is_some_and(material_name) {
            td_alloc::push(&mut tombstones, path)?;
            continue;
        }
        if !material_name(name) || path == current {
            continue;
        }
        let receipt = child(&path, "receipt.json")?;
        let mut since = modified(&path)?;
        // Anything but a directory is refused before removal.
        if fs::symlink_metadata(&path)?.is_dir() && receipt.try_exists()? {
            since = since.max(modified(&receipt)?);
        }
        td_alloc::push(&mut material, Material { path, since })?;
    }
    Ok((material, tombstones))
}

/// A root-owned real directory with nothing mounted inside it.
pub(super) fn removable(path: &Path, mountinfo: &str, owner: u32) -> R<()> {
    let meta = fs::symlink_metadata(path)?;
    let prefix = path.to_str().ok_or("material path")?;
    if !(meta.is_dir() && meta.uid() == owner) {
        return Err(fmt(format_args!(
            "rollback material {prefix} is not a root-owned directory: inspect it"
        ))?
        .into());
    }
    for line in mountinfo.lines() {
        let point = line.split(' ').nth(4).unwrap_or("");
        if point
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        {
            return Err(fmt(format_args!(
                "something is mounted at or inside rollback material {prefix}: unmount it"
            ))?
            .into());
        }
    }
    Ok(())
}

/// The tombstone `path` is removed under, beside it.
pub(super) fn tombstone(path: &Path) -> R<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("material name")?;
    if name.starts_with(TOMBSTONE) {
        child(parent(path)?, name)
    } else {
        child(parent(path)?, &fmt(format_args!("{TOMBSTONE}{name}"))?)
    }
}

/// Removes `path` under its tombstone, after `announce`; a tombstone
/// itself is removed in place.
pub(super) fn remove(path: &Path, announce: impl FnOnce(&Path) -> R<()>) -> R<()> {
    let doomed = tombstone(path)?;
    announce(path)?;
    if doomed != path {
        fs::rename(path, &doomed)?;
        sync_dir(parent(path)?)?;
    }
    fs::remove_dir_all(&doomed)?;
    sync_dir(parent(path)?)
}

/// What retirement requires before it reads the host's health, in this
/// order: the job says the removal is irreversible, no deployment or
/// database fence is held, and it keeps the live release.
pub(super) fn preflight(
    job: &Json,
    fenced: impl FnOnce() -> R<bool>,
    keeps_live: impl FnOnce() -> R<bool>,
) -> R<()> {
    require(
        job.get("irreversible").and_then(Json::as_bool) == Some(true),
        "retiring rollback material cannot be undone: run td-mta/deploy --retire-rollback --irreversible",
    )?;
    require(
        !fenced()?,
        "unfinished deployment fence: run td-mta/deploy --recover",
    )?;
    require(
        keeps_live()?,
        "retiring rollback material keeps the live release",
    )
}

/// Why the Debian tree stays, if it does: td's server is selected but
/// not what runs (`in_use`), or no verified switch to it recorded the
/// window's start (`recorded`). On the Debian server it is in use.
pub(super) fn tree_kept(
    td_server: bool,
    in_use: &R<()>,
    recorded: bool,
) -> R<(bool, Option<String>)> {
    if !td_server {
        return Ok((false, None));
    }
    if let Err(e) = in_use {
        return Ok((
            false,
            Some(fmt(format_args!(
                "{e}: run td-mta/deploy --postgres-binaries"
            ))?),
        ));
    }
    if !recorded {
        return Ok((
            true,
            Some(owned(
                "no verified switch to the running td PostgreSQL recorded: run td-mta/deploy --postgres-binaries",
            )?),
        ));
    }
    Ok((true, None))
}

impl Deployment {
    pub(super) fn retire_rollback(&mut self) -> R<()> {
        preflight(
            &self.record,
            || Ok(Path::new(FENCE).try_exists()? || Path::new(PG_FENCE).try_exists()?),
            || Ok(self.previous == self.candidate && fs::canonicalize(ACTIVE)? == self.previous),
        )?;
        let all = self.record.get(RETIRE_ALL).and_then(Json::as_bool) == Some(true);
        let mode = if all { "all" } else { "window" };
        set(&mut self.record, RETIRE_MODE, Json::Str(owned(mode)?))?;
        // Early, a backup that is not ready refuses before anything else.
        let backup = if all {
            let backup = self.scheduled_backup()?;
            set(&mut self.record, "rollback_retire_backup", backup.json()?)?;
            self.backup_ready(&backup)?;
            Some(backup)
        } else {
            None
        };
        // Rollback material is what an unhealthy host needs.
        self.check_source()?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        let mut material = Vec::new();
        let mut tombstones = Vec::new();
        // The tree is material only while nothing that runs uses it: the
        // selected td server, mapping only its closure, and the backup job
        // on td's tools. Otherwise it stays and the rest goes.
        let present = Path::new(DEBIAN_POSTGRES).try_exists()?;
        let td_server = !self.host.debian_tools();
        let in_use = if td_server { self.td_in_use() } else { Ok(()) };
        // A cancelled run stops here, whatever the check said.
        self.host.remaining()?;
        let recorded = switched_at(Path::new(SWITCHED)).is_some();
        let (td_selected, kept) = tree_kept(td_server, &in_use, recorded)?;
        let (tree, dead) =
            debian_material(Path::new(DEBIAN_POSTGRES), Path::new(SWITCHED), td_selected)?;
        if let (true, Some(reason)) = (present, kept) {
            self.event(&fmt(format_args!(
                "keeping the Debian PostgreSQL tree: {reason}"
            ))?)?;
            set(&mut self.record, "debian_tree_kept", Json::Str(reason))?;
        }
        // Gone already: a tree restored later must not inherit the window.
        if !Path::new(DEBIAN_POSTGRES).try_exists()? {
            clear_switch(Path::new(SWITCHED))?;
        }
        if let Some(tree) = tree {
            td_alloc::push(&mut material, tree)?;
        }
        if let Some(dead) = dead {
            td_alloc::push(&mut tombstones, dead)?;
        }
        for dir in MATERIAL {
            let (found, dead) = material_in(Path::new(dir), &self.work)?;
            for item in found {
                td_alloc::push(&mut material, item)?;
            }
            for item in dead {
                td_alloc::push(&mut tombstones, item)?;
            }
        }
        // Early, nothing goes unless the backup is newer than all of it.
        let planned = backup::retiring(&material, now_secs()?, all, backup.as_ref());
        let (past, next) = self.awaiting_backup(planned)?;
        if let Some(next) = next {
            set(
                &mut self.record,
                "rollback_retire_not_before",
                Json::from(next),
            )?;
        }
        require(
            !past.is_empty() || !tombstones.is_empty() || next.is_none(),
            INSIDE_WINDOW,
        )?;
        // Every check before the first removal.
        let bound = 1024 * 1024;
        let mountinfo = String::from_utf8(td_fs::read_bounded(
            Path::new("/proc/self/mountinfo"),
            bound,
        )?)?;
        let mut paths = Vec::new();
        for path in tombstones.iter().map(PathBuf::as_path).chain(past) {
            removable(path, &mountinfo, 0)?;
            td_alloc::push(&mut paths, path)?;
        }
        let mut retired = Vec::new();
        for path in paths {
            let named = path.to_str().ok_or("material path")?;
            // Named in the receipt before it goes.
            remove(path, |_| {
                self.event(&fmt(format_args!("retiring {named}"))?)
                    .map(|_| ())
            })?;
            td_alloc::push(&mut retired, Json::Str(owned(named)?))?;
        }
        set(&mut self.record, "rollback_retired", Json::Arr(retired))?;
        if !Path::new(DEBIAN_POSTGRES).try_exists()? {
            clear_switch(Path::new(SWITCHED))?;
        }
        self.event("rollback material retired")?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/retire.rs"]
mod tests;
