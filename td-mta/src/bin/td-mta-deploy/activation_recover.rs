//! `--recover`: a job whose controller died, or failed, inside its writer
//! fence, finished or rolled back from what the host and the job's
//! directory hold, as its controller would have. Once writer admission
//! began it recovers forward on the selected release. Before that it
//! restores what the job replaced: a switch's source PostgreSQL, an
//! activation's original primary (the data directory the migration kept
//! whole beside the candidate) and previous release; a shared-service
//! restore, which changes no data, it completes.
use super::*;
use fence::{open_socket, LiveSocket, PG_FENCE};

pub(super) const OPENING: &str = "opening writers";
const SEALED: &str = "exact recovery point verified";
const FAILED: &str = "deployment failed";
const FENCING: &str = "fencing";
const VERIFIED: &str = "verified deployment";
/// The record of a shared-service restore run by hand in its cutover's
/// directory, before restores were jobs of their own.
const SHARED: &str = "shared-rollback.json";
const RECEIPT_BOUND: u64 = 1024 * 1024;
/// In the fenced job's directory once a recovery chose to roll it back:
/// a later recovery never goes forward on what that one half restored.
const DECIDED: &str = "recover-rollback";

/// What the fenced job left.
#[derive(Debug, PartialEq)]
pub(super) enum Interrupted {
    /// Writer admission began: recover forward on what is selected.
    Admitting,
    /// A PostgreSQL switch or return before admission.
    Switch,
    /// An activation before admission; `stopped` once its migration may
    /// have stopped the primary or swapped its data directory.
    Activation { stopped: bool },
    /// A shared-service restore before admission.
    Shared,
}

/// The messages of a receipt's `events`, in order.
pub(super) fn messages(receipt: &Json) -> R<Vec<&str>> {
    let mut out = Vec::new();
    for event in receipt
        .get("events")
        .and_then(Json::as_arr)
        .ok_or("receipt events missing")?
    {
        let message = event
            .as_arr()
            .and_then(|row| row.get(1))
            .and_then(Json::as_str)
            .ok_or("receipt event")?;
        td_alloc::push(&mut out, message)?;
    }
    Ok(out)
}

/// The last step a job took: its failure is not one.
pub(super) fn last_step<'a>(events: &[&'a str]) -> Option<&'a str> {
    events.iter().rev().find(|e| **e != FAILED).copied()
}

/// The fenced job's state, from its operation (`mode`; a shared-service
/// restore is `restore-shared-services`), its events, whether it closed
/// the database socket fence and whether that fence's file is still there.
/// As in `open_writer_fences`, admission began once `opening writers` was
/// the last step and the first fence it removes, the socket's, is gone;
/// the deployment fence is there, or recovery would not be classifying.
/// Once a recovery decided to roll the job back (`decided`) it stays so.
pub(super) fn classify(
    mode: &str,
    events: &[&str],
    socket_fence: bool,
    pg_fence: bool,
    decided: bool,
) -> R<Interrupted> {
    if !decided && last_step(events) == Some(OPENING) && socket_fence && !pg_fence {
        return Ok(Interrupted::Admitting);
    }
    Ok(match mode {
        "restore-shared-services" => Interrupted::Shared,
        "activate" | "rollback" => Interrupted::Activation {
            stopped: events.contains(&SEALED),
        },
        "postgres-binaries" | "postgres-to-debian" => Interrupted::Switch,
        _ => return Err(crate::activate::NO_FENCE.into()),
    })
}

/// A job directory's record: a shared-service restore's when there is
/// one, since it can run only after its deployment ended; else the job's.
fn record_of(dir: &Path) -> R<Option<(Json, bool)>> {
    let shared = child(dir, SHARED)?;
    if shared.try_exists()? {
        return Ok(Some((read_json(&shared, RECEIPT_BOUND)?, true)));
    }
    let receipt = child(dir, "receipt.json")?;
    if !receipt.try_exists()? {
        return Ok(None);
    }
    Ok(Some((read_json(&receipt, RECEIPT_BOUND)?, false)))
}

/// The most recently started job, other than `exclude`, whose record
/// `want` accepts; unless `strict`, unreadable records are passed over.
fn latest(
    op: &Path,
    exclude: &Path,
    strict: bool,
    want: impl Fn(&Json) -> R<bool>,
) -> R<Option<(PathBuf, Json, bool)>> {
    let mut found: Option<(PathBuf, Json, bool, f64)> = None;
    for entry in fs::read_dir(op)? {
        let dir = entry?.path();
        if dir == exclude || !name(&dir).is_ok_and(|n| numbered_name(n, "deploy-")) {
            continue;
        }
        let record = match record_of(&dir) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(e) if strict => return Err(e),
            Err(_) => continue,
        };
        let (record, shared) = record;
        // A record without a start time predates these controllers.
        let Some(started) = record.get("started").and_then(Json::as_f64) else {
            continue;
        };
        let wanted = match want(&record) {
            Ok(wanted) => wanted,
            Err(e) if strict => return Err(e),
            Err(_) => false,
        };
        if wanted && found.as_ref().is_none_or(|f| started > f.3) {
            found = Some((dir, record, shared, started));
        }
    }
    Ok(found.map(|(dir, record, shared, _)| (dir, record, shared)))
}

/// `--rollback`: `job` (live release in `previous`) becomes the activation
/// of the release replaced by the latest activation or rollback in `op`
/// that installed the live one, verified or kept once writers returned,
/// which must have migrated nothing.
pub(super) fn rollback_job(mut job: Json, op: &Path, work: &Path, releases: &Path) -> R<Json> {
    let live = owned(field(&job, "previous")?)?;
    let (dir, last) = installed(op, work, &live)?
        .ok_or("no retained activation installed the live release: nothing to roll back")?;
    require(
        last.get("isolation_change").and_then(Json::as_bool) != Some(true),
        "the latest activation isolated the service identities: run td-mta/deploy --restore-shared-services",
    )?;
    require(
        last.get("recovery").is_none() && uint(&last, "schema_before")? == uint(&last, "schema_after")?,
        "the latest activation migrated the database, which a binary rollback cannot undo: deploy a forward fix",
    )?;
    let target = owned(field(&last, "previous")?)?;
    require(lower_hex(&target, 40), "invalid recorded previous release")?;
    let stamp = child(
        releases,
        &fmt(format_args!("git-{target}/deployment-inputs.json"))?,
    )?;
    require(
        stamp.try_exists()?,
        "the release rolled back to is no longer retained",
    )?;
    let stamp = read_json(&stamp, RECEIPT_BOUND)?;
    require(
        field(&stamp, "revision")? == target,
        "retained release provenance",
    )?;
    let tree = owned(field(&stamp, "tree")?)?;
    set(&mut job, "revision", Json::Str(target))?;
    set(&mut job, "tree", Json::Str(tree))?;
    set(
        &mut job,
        "rollback_of",
        Json::Str(owned(dir.to_str().ok_or("job path")?)?),
    )?;
    Ok(job)
}

/// `--restore-shared-services`: `job` (live release in `revision`) returns
/// from the identity cutover, the latest activation or rollback in `op`
/// that installed the live release, which must be the one that isolated
/// the identities and one no restore has returned from yet. Its
/// receipt's `previous` becomes the job's, as `restore_of` names it.
pub(super) fn restore_job(mut job: Json, op: &Path, work: &Path) -> R<Json> {
    let live = owned(field(&job, "revision")?)?;
    let (dir, last) = installed(op, work, &live)?
        .ok_or("no retained activation installed the live release: nothing to restore")?;
    require(
        last.get("isolation_change").and_then(Json::as_bool) == Some(true),
        "the activation that installed the live release did not isolate the service identities: the shared identities return only from the identity cutover while its release is live",
    )?;
    require(
        !restored(op, work, &dir)?,
        "a restore already returned from this identity cutover",
    )?;
    let previous = owned(field(&last, "previous")?)?;
    require(
        lower_hex(&previous, 40),
        "invalid recorded previous release",
    )?;
    set(&mut job, "previous", Json::Str(previous))?;
    set(
        &mut job,
        "restore_of",
        Json::Str(owned(dir.to_str().ok_or("job path")?)?),
    )?;
    Ok(job)
}

/// Whether a restore verified the shared identities from `cutover`: a
/// restore run by hand in its directory, or a restore or recovery job
/// naming it.
fn restored(op: &Path, exclude: &Path, cutover: &Path) -> R<bool> {
    let verified = |record: &Json| -> R<bool> { Ok(messages(record)?.contains(&SHARED_RESTORED)) };
    let beside = child(cutover, SHARED)?;
    if beside.try_exists()? && verified(&read_json(&beside, RECEIPT_BOUND)?)? {
        return Ok(true);
    }
    let named = cutover.to_str().ok_or("job path")?;
    Ok(latest(op, exclude, false, |record| {
        Ok(record.get("restore_of").and_then(Json::as_str) == Some(named) && verified(record)?)
    })?
    .is_some())
}

/// The latest activation or rollback whose `revision` is `live` and that
/// left it selected: verified, or kept once its writers returned. Its
/// deployment's own receipt, not a later shared-service restore's record.
fn installed(op: &Path, exclude: &Path, live: &str) -> R<Option<(PathBuf, Json)>> {
    let mut found: Option<(PathBuf, Json, f64)> = None;
    for entry in fs::read_dir(op)? {
        let dir = entry?.path();
        if dir == exclude || !name(&dir).is_ok_and(|n| numbered_name(n, "deploy-")) {
            continue;
        }
        let path = child(&dir, "receipt.json")?;
        // Unreadable or legacy records install nothing this can verify.
        let Ok(record) = read_json(&path, RECEIPT_BOUND) else {
            continue;
        };
        let Some(started) = record.get("started").and_then(Json::as_f64) else {
            continue;
        };
        let Ok(events) = messages(&record) else {
            continue;
        };
        let kept = events.contains(&VERIFIED)
            || (events.contains(&OPENING) && record.get("rollback").is_none());
        if matches!(
            record.get("mode").and_then(Json::as_str),
            None | Some("rollback")
        ) && record.get("revision").and_then(Json::as_str) == Some(live)
            && kept
            && found.as_ref().is_none_or(|f| started > f.2)
        {
            found = Some((dir, record, started));
        }
    }
    Ok(found.map(|(dir, record, _)| (dir, record)))
}

/// Whether a record could still hold the deployment fence: it recorded
/// `fencing` and, since, neither began admission nor recorded an end.
pub(super) fn holds_fence(record: &Json) -> R<bool> {
    let events = messages(record)?;
    let Some(fenced) = events.iter().rposition(|e| *e == FENCING) else {
        return Ok(false);
    };
    let admitted = events.iter().skip(fenced).any(|e| *e == OPENING);
    let ended = ["rollback", "recovery_status", "listeners_up_utc"]
        .iter()
        .any(|key| record.get(key).is_some());
    Ok(!admitted && !ended)
}

/// The job directory the deployment fence names, and whether it was
/// inferred. A fence that names no job directory (its controller died
/// while writing it) belongs to the most recent job that still holds a
/// fence; a job that began admission wrote its fence long before. None
/// such: no controller left this fence, and recovery stops.
pub(super) fn owner(fence: &str, op: &Path, exclude: &Path) -> R<(PathBuf, bool)> {
    let named = fence.trim();
    if !named.is_empty() {
        let path = PathBuf::from(owned(named)?);
        if path.parent() == Some(op)
            && name(&path).is_ok_and(|n| numbered_name(n, "deploy-"))
            && path.is_dir()
        {
            return Ok((path, false));
        }
    }
    latest(op, exclude, true, holds_fence)?
        .map(|(dir, _, _)| (dir, true))
        .ok_or_else(|| "the deployment fence names no job, and no job still holds one".into())
}

/// Whether a record's last step left an admission unfinished: a finished
/// rollback, admission recovery or admission records its end.
pub(super) fn unfinished_admission(record: &Json) -> R<bool> {
    let ended = ["rollback", "recovery_status", "listeners_up_utc"]
        .iter()
        .any(|key| record.get(key).is_some());
    Ok(!ended && last_step(&messages(record)?) == Some(OPENING))
}

/// The original primary back in place of a migrated or half-swapped one,
/// as `rollback` puts it; whether it moved anything.
pub(super) fn swap_back(pgdata: &Path, original: &Path, unreleased: &Path) -> R<bool> {
    if !original.try_exists()? {
        // Never swapped, or already swapped back.
        return Ok(false);
    }
    if pgdata.try_exists()? {
        require(
            !unreleased.try_exists()?,
            "a migrated and an unreleased primary both exist beside the original",
        )?;
        fs::rename(pgdata, unreleased)?;
    }
    fs::rename(original, pgdata)?;
    sync_dir(parent(pgdata)?)?;
    sync_dir(parent(original)?)?;
    Ok(true)
}

/// A job's recorded timer set.
fn timers(record: &Json) -> R<Vec<String>> {
    let mut out = Vec::new();
    for timer in record
        .get("timers")
        .and_then(Json::as_arr)
        .ok_or("the fenced job recorded no timers")?
    {
        td_alloc::push(&mut out, owned(timer.as_str().ok_or("timer")?)?)?;
    }
    Ok(out)
}

impl Deployment {
    pub(super) fn recover(&mut self) -> R<()> {
        let fence = match td_fs::read_bounded(Path::new(FENCE), 4096) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            other => Some(String::from_utf8(other?)?),
        };
        let Some(fence) = fence else {
            return self.recover_unfenced();
        };
        let (job, inferred) = owner(&fence, Path::new(OP), &self.work)?;
        if inferred {
            // Named, so that admission checks the fence as its own.
            let staged = child(Path::new(OP), ".deployment-fence.recover")?;
            remove_file(&staged)?;
            let mut file = create_file(&staged, 0o600)?;
            writeln!(file, "{}", job.to_str().ok_or("job path")?)?;
            file.sync_all()?;
            fs::rename(&staged, FENCE)?;
            sync_dir(Path::new(OP))?;
        }
        let (receipt, shared) = record_of(&job)?.ok_or("the fenced job has no receipt")?;
        let mode = if shared {
            "restore-shared-services"
        } else {
            receipt
                .get("mode")
                .and_then(Json::as_str)
                .unwrap_or("activate")
        };
        let events = messages(&receipt)?;
        // Every migration and switch closes the socket; nothing else does.
        let socket_fence = match mode {
            "restore-shared-services" => false,
            "activate" | "rollback" => receipt.get("recovery").is_some(),
            _ => true,
        };
        let pg_fence = Path::new(PG_FENCE).try_exists()?;
        let decided = child(&job, DECIDED)?;
        let state = classify(mode, &events, socket_fence, pg_fence, decided.try_exists()?)?;
        self.adopt(&job, &receipt)?;
        if state != Interrupted::Admitting && !decided.try_exists()? {
            create_file(&decided, 0o600)?.sync_all()?;
            sync_dir(&job)?;
        }
        self.event(&fmt(format_args!(
            "recovering {} {mode}: {state:?}",
            job.display()
        ))?)?;
        self.socket_fence = pg_fence;
        self.fence_owned = true;
        // A primary may start inside the fence only with the permit.
        let boot = child(
            Path::new(UNIT_DIR),
            &fmt(format_args!("{PG}.d/98-td-mta-deploy-boot.conf"))?,
        )?;
        if boot.try_exists()? {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .open(PERMIT)?
                .sync_all()?;
        }
        // After a reboot the fence kept the primary down; a switch's
        // restore and a migration's swap-back start it themselves.
        if matches!(
            state,
            Interrupted::Admitting
                | Interrupted::Shared
                | Interrupted::Activation { stopped: false }
        ) {
            self.host
                .run(self.host.cmd("systemctl").args(["start", PG]))?;
            self.wait(25, "fenced primary start", || self.ready_db(&[]))?;
            self.fenced_start()?;
        }
        let forward = state == Interrupted::Admitting;
        let result = match state {
            Interrupted::Admitting => self.recover_admission(),
            Interrupted::Switch => self.listen().and_then(|()| self.recover_switch(&receipt)),
            Interrupted::Activation { stopped } => self
                .listen()
                .and_then(|()| self.recover_activation(&job, &receipt, stopped)),
            Interrupted::Shared => self
                .listen()
                .and_then(|()| self.recover_shared(&job, &receipt)),
        };
        // A failure before admission leaves the fence for the next run.
        let result = if result.is_err() && self.admitting && !forward {
            self.host.recover();
            self.finish_admission().and(result)
        } else {
            result
        };
        self.host.deadline(None);
        let logs = self.finish_probes("listener-samples.json");
        result.and(logs)?;
        self.clear_leftovers(&job, &receipt)?;
        remove_file(&decided)?;
        self.event("recovered")?;
        Ok(())
    }

    /// No deployment fence: the latest job that fenced its writers, if
    /// its controller stopped inside admission, which removes that fence
    /// before it starts the services.
    fn recover_unfenced(&mut self) -> R<()> {
        let fenced = latest(Path::new(OP), &self.work, false, |r| {
            let events = messages(r)?;
            Ok(events.contains(&FENCING) || events.contains(&OPENING))
        })?;
        if let Some((job, receipt, _)) = fenced {
            if unfinished_admission(&receipt)? {
                self.adopt(&job, &receipt)?;
                self.event(&fmt(format_args!(
                    "recovering {}: its admission",
                    job.display()
                ))?)?;
                self.recover_admission()?;
                self.clear_leftovers(&job, &receipt)?;
                self.event("recovered")?;
                return Ok(());
            }
        }
        // A socket fence alone outlived the admission that removed the
        // deployment fence: reopened only over the production primary.
        if Path::new(PG_FENCE).try_exists()? {
            self.check_source()?;
            open_socket(&mut LiveSocket(&self.host))?;
            self.event("database socket reopened")?;
        }
        self.healthy(&fs::canonicalize(ACTIVE)?)?;
        // What a controller that died before fencing left: every
        // candidate copy is dead under the lock without a fence.
        for entry in fs::read_dir(OP)? {
            let dir = entry?.path();
            if name(&dir).is_ok_and(|n| numbered_name(n, "deploy-")) {
                let cold = child(&dir, "candidate-pgdata")?;
                if cold.try_exists()? {
                    fs::remove_dir_all(&cold)?;
                }
            }
        }
        if let Some((job, receipt, _)) = latest(Path::new(OP), &self.work, false, |_| Ok(true))? {
            self.clear_leftovers(&job, &receipt)?;
        }
        self.event("nothing to recover")?;
        Ok(())
    }

    /// The fenced job's timers and fence become this job's.
    fn adopt(&mut self, job: &Path, receipt: &Json) -> R<()> {
        self.timers = timers(receipt)?;
        let mut recorded = Vec::new();
        for timer in &self.timers {
            td_alloc::push(&mut recorded, Json::Str(owned(timer)?))?;
        }
        set(&mut self.record, "timers", Json::Arr(recorded))?;
        set(
            &mut self.record,
            "recovered_job",
            Json::Str(owned(job.to_str().ok_or("job path")?)?),
        )?;
        self.fence_owner = Some(PathBuf::from(owned(job.to_str().ok_or("job path")?)?));
        Ok(())
    }

    fn listen(&mut self) -> R<()> {
        self.probes.start(false)?;
        self.record_listeners = true;
        Ok(())
    }

    /// Forward, as the controller's own admission recovery does. Recorded
    /// first, so that a recovery dying here is itself recovered forward.
    fn recover_admission(&mut self) -> R<()> {
        self.event(OPENING)?;
        self.admitting = true;
        self.finish_admission()
    }

    /// Before admission: the original primary, swapped back if the
    /// migration had stopped it, the identity settings this job installed
    /// removed, and the previous release selected and admitted.
    fn recover_activation(&mut self, job: &Path, receipt: &Json, stopped: bool) -> R<()> {
        self.previous = child(
            Path::new(RELEASES),
            &fmt(format_args!("git-{}/td-mta", field(receipt, "previous")?))?,
        )?;
        if stopped {
            self.host.exec(
                self.host.cmd("systemctl").args(["stop", PG]),
                &[],
                Duration::from_secs(120),
                true,
            )?;
            let original = child(job, "original-pgdata")?;
            if original.try_exists()? {
                // Never a damaged directory over the migrated one.
                require(
                    self.control_state(&original)? == "shut down"
                        && self.control_field(&original, "Database system identifier")? == SYSTEM,
                    "the job's original data directory is not the cleanly shut-down primary",
                )?;
            }
            if swap_back(
                Path::new(PGDATA),
                &original,
                &child(job, "unreleased-pgdata")?,
            )? {
                self.event("original primary data directory restored")?;
            }
            self.host
                .run(self.host.cmd("systemctl").args(["start", PG]))?;
            self.wait(25, "original primary restart", || self.ready_db(&[]))?;
            self.fenced_start()?;
        }
        self.check_source()?;
        if receipt.get("isolation_change").and_then(Json::as_bool) == Some(true) {
            // The job began on shared identities: what is installed is its.
            isolation::rollback(&self.host)?;
            self.isolated = false;
        }
        self.switch(&self.previous)?;
        // Only root reaches a fenced socket; the logins below need it open.
        if self.socket_fence {
            open_socket(&mut LiveSocket(&self.host))?;
            self.socket_fence = false;
        }
        self.journal_identity()?;
        require(
            self.epoch == field(receipt, "epoch")?
                && self.old_version == uint(receipt, "schema_before")?,
            "the primary is not the one the job fenced",
        )?;
        self.config(&self.previous, &[], self.isolated)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "timer set changed")?;
        set(
            &mut self.record,
            "rollback",
            "original release and database verified".into(),
        )
    }

    /// A shared-service restore fenced its writers only to change the
    /// identity settings over the same primary: it is completed.
    fn recover_shared(&mut self, job: &Path, receipt: &Json) -> R<()> {
        self.previous = child(
            Path::new(RELEASES),
            &fmt(format_args!("git-{}/td-mta", field(receipt, "previous")?))?,
        )?;
        // The cutover's receipt holds the epoch; the restore's does not. A
        // restore job names it; a restore run by hand wrote its record in it.
        let cutover = receipt
            .get("restore_of")
            .and_then(Json::as_str)
            .map_or(job, Path::new);
        let deployment = read_json(&child(cutover, "receipt.json")?, RECEIPT_BOUND)?;
        // A restore this finishes is one restore_job sees.
        set(
            &mut self.record,
            "restore_of",
            Json::Str(owned(cutover.to_str().ok_or("job path")?)?),
        )?;
        self.journal_identity()?;
        require(
            self.epoch == field(&deployment, "store_epoch")?,
            "the primary is not the one the job fenced",
        )?;
        self.config(&self.previous, &[], false)?;
        isolation::transfer_clocks(&self.host, false)?;
        isolation::rollback(&self.host)?;
        self.isolated = false;
        self.switch(&self.previous)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "rollback timer set")?;
        self.event(SHARED_RESTORED)?;
        Ok(())
    }

    /// What the fenced job would have removed once its fence was gone:
    /// its candidate copy, the start permit, the fence conditions, and the
    /// working parts of a recovery copy.
    fn clear_leftovers(&mut self, job: &Path, receipt: &Json) -> R<()> {
        if Path::new(FENCE).try_exists()? {
            return Ok(());
        }
        let cold = child(job, "candidate-pgdata")?;
        if cold.try_exists()? {
            fs::remove_dir_all(&cold)?;
        }
        remove_file(Path::new(PERMIT))?;
        let mut removed = false;
        for entry in fs::read_dir(UNIT_DIR)? {
            let dir = entry?.path();
            if !name(&dir).is_ok_and(|n| n.starts_with("td-mta-") && n.ends_with(".d")) {
                continue;
            }
            for file in ["98-td-mta-deploy-fence.conf", "98-td-mta-deploy-boot.conf"] {
                let path = child(&dir, file)?;
                if fs::symlink_metadata(&path).is_ok() {
                    remove_file(&path)?;
                    removed = true;
                }
            }
        }
        if removed {
            self.host
                .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        }
        let copy = receipt
            .get("recovery")
            .and_then(Json::as_str)
            .map(Path::new);
        crate::physical::close_dead(copy)
    }
}

#[cfg(test)]
#[path = "tests/recover.rs"]
mod tests;
