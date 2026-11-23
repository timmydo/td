//! Retiring rollback material once its window has passed: earlier job
//! directories (receipts, the original data directory a migration left,
//! uploaded closures, the loopback TLS material and CA key the socket
//! cutover retired) and the physical recovery copies activations took.
//! Irreversible, so the job must say so. The retained Debian PostgreSQL
//! tree stays: it is the only way to change td's PostgreSQL closure.
use super::*;
use fence::PG_FENCE;

/// How long rollback material stays after the change that left it.
pub(super) const WINDOW: u64 = 7 * 24 * 3600;
/// Earlier job directories, and the recovery copies activations took.
const MATERIAL: &[&str] = &[OP, crate::physical::VOLUME];
/// A directory being removed; a rerun finishes it whatever its age.
const TOMBSTONE: &str = ".retired-";

/// One piece of rollback material and the time its window opened.
pub(super) struct Material {
    pub(super) path: PathBuf,
    pub(super) since: u64,
}

/// What retiring at `now` does with `material`: the paths past the window,
/// and the earliest time one still inside it may go.
pub(super) fn due(material: &[Material], now: u64) -> R<(Vec<&Path>, Option<u64>)> {
    let mut past = Vec::new();
    let mut next: Option<u64> = None;
    for item in material {
        let until = item.since.saturating_add(WINDOW);
        if until <= now {
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

/// The tombstone `path` is removed under.
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

impl Deployment {
    pub(super) fn retire_rollback(&mut self) -> R<()> {
        require(
            self.record.get("irreversible").and_then(Json::as_bool) == Some(true),
            "retiring rollback material cannot be undone: run td-mta/deploy --retire-rollback --irreversible",
        )?;
        require(
            !Path::new(FENCE).try_exists()? && !Path::new(PG_FENCE).try_exists()?,
            "unfinished deployment fence: recover that job first",
        )?;
        require(
            self.previous == self.candidate && fs::canonicalize(ACTIVE)? == self.previous,
            "retiring rollback material keeps the live release",
        )?;
        // Rollback material is what an unhealthy host needs.
        self.check_source()?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        let mut material = Vec::new();
        let mut tombstones = Vec::new();
        for dir in MATERIAL {
            let (found, dead) = material_in(Path::new(dir), &self.work)?;
            for item in found {
                td_alloc::push(&mut material, item)?;
            }
            for item in dead {
                td_alloc::push(&mut tombstones, item)?;
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let (past, next) = due(&material, now)?;
        if let Some(next) = next {
            set(
                &mut self.record,
                "rollback_retire_not_before",
                Json::from(next),
            )?;
        }
        require(
            !past.is_empty() || !tombstones.is_empty() || next.is_none(),
            "all rollback material is inside its 7-day window: run td-mta/deploy --retire-rollback --irreversible after rollback_retire_not_before",
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
            let doomed = tombstone(path)?;
            // Named in the receipt before it goes.
            let named = path.to_str().ok_or("material path")?;
            self.event(&fmt(format_args!("retiring {named}"))?)?;
            if doomed != path {
                fs::rename(path, &doomed)?;
                sync_dir(parent(path)?)?;
            }
            fs::remove_dir_all(&doomed)?;
            sync_dir(parent(path)?)?;
            td_alloc::push(&mut retired, Json::Str(owned(named)?))?;
        }
        set(&mut self.record, "rollback_retired", Json::Arr(retired))?;
        self.event("rollback material retired")?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/retire.rs"]
mod tests;
