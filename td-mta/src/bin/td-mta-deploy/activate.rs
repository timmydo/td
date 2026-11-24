//! Production cutover. A positively closed writer fence is required to rewind
//! the database. Once writers might have been admitted, recover forward.
use crate::{
    common::{child, digest, durable, fmt, free_bytes, owned, require, set, Host},
    isolation,
    physical::{Recovery, GIB, MAIL, PGDATA, SYSTEM},
    probes::Probes,
    service_database::{self, Passwords, ROLES},
    target::HostConfig,
    R,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use td_json::Json;

#[path = "activation_binaries.rs"]
pub(crate) mod binaries;
#[path = "activation_config.rs"]
mod config;
#[path = "activation_database.rs"]
mod database;
#[path = "activation_fence.rs"]
mod fence;
#[path = "activation_recover.rs"]
mod recover;
#[path = "activation_release.rs"]
mod release;
#[path = "activation_retire.rs"]
mod retire;
#[path = "activation_transport.rs"]
mod transport;

pub(crate) use binaries::{discard_nars, selected_store, BACKUP_DROPIN, BINARIES_DROPIN};
pub(crate) use config::RESULTS as CONVERGE_RESULTS;
pub(crate) use transport::{redacted_runtime_tls, REDACT_RUNTIME, RUNTIME_CONNECTION};

const OP: &str = "/var/lib/td-mta-operator";
const RELEASES: &str = "/opt/td-mta/lib/td-mta/releases";
const ACTIVE: &str = "/opt/td-mta/bin/td-mta";
pub(crate) const FENCE: &str = "/var/lib/td-mta-operator/deployment-fence";
const PERMIT: &str = "/run/td-mta-deployment-primary.permit";
const UNIT_DIR: &str = "/etc/systemd/system";
const PG: &str = "td-mta-postgres.service";
pub(crate) const PROFILES: &[&str] = &["frontend", "receive", "relay", "migadu"];
const ACME: &str = "td-mta-acme-production.service";
const UNITS: &[&str] = &[
    "td-mta-frontend.service",
    "td-mta-receive.service",
    "td-mta-relay.service",
    "td-mta-migadu.service",
    ACME,
];
const SHARED_RESTORED: &str = "shared release verified against current primary";
const SELECTED: &str = "SELECT encode(version_id,'hex') FROM td_mta.config_selection";

fn now() -> R<f64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64())
}

fn field<'a>(json: &'a Json, key: &str) -> R<&'a str> {
    json.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| "missing or invalid text field".into())
}
fn uint(json: &Json, key: &str) -> R<u64> {
    json.get(key)
        .and_then(Json::as_u64)
        .ok_or_else(|| "missing or invalid integer field".into())
}
fn lower_hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn numbered_name(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|n| n.len() == 8 && n.bytes().all(|b| b.is_ascii_alphanumeric()))
}
fn read_json(path: &Path, bound: u64) -> R<Json> {
    let bytes = td_fs::read_bounded(path, bound)?;
    Ok(td_json::parse_slice(&bytes)?)
}
fn remove_file(path: &Path) -> R<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn sync_dir(path: &Path) -> R<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
fn create_file(path: &Path, mode: u32) -> R<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?)
}
fn name(path: &Path) -> R<&str> {
    path.file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid filename".into())
}
fn parent(path: &Path) -> R<&Path> {
    path.parent()
        .ok_or_else(|| "missing parent directory".into())
}
fn start_receipt(path: &Path, record: &Json) -> R<()> {
    // The per-host activation lock serializes the existence check and publish.
    require(!path.try_exists()?, "job already has an activation receipt")?;
    durable(path, record)
}

fn require_pending_rollback(work: &Path) -> R<()> {
    let path = child(work, "shared-rollback.json")?;
    if path.try_exists()? {
        let prior = read_json(&path, 1024 * 1024)?;
        let verified = prior
            .get("events")
            .and_then(Json::as_arr)
            .is_some_and(|events| {
                events.iter().any(|event| {
                    event
                        .as_arr()
                        .and_then(|row| row.get(1))
                        .and_then(Json::as_str)
                        == Some(SHARED_RESTORED)
                })
            });
        require(
            field(&prior, "status")? != SHARED_RESTORED && !verified,
            "this job already completed shared rollback",
        )?;
    }
    Ok(())
}

fn parse_output(out: &std::process::Output) -> R<Json> {
    Ok(td_json::parse_slice(&out.stdout)?)
}

pub(super) struct Deployment {
    pub(super) host: Host,
    pub(super) work: PathBuf,
    pub(super) previous: PathBuf,
    pub(super) isolated: bool,
    pub(super) isolation_change: bool,
    pub(super) plan: Option<Json>,
    candidate: PathBuf,
    record: Json,
    receipt: PathBuf,
    budget: Duration,
    old_version: u64,
    epoch: String,
    selected: String,
    passwords: Option<Passwords>,
    builder: release::Builder,
    probes: Probes,
    timers: Vec<String>,
    drops: Vec<PathBuf>,
    record_listeners: bool,
    window: Option<Instant>,
    fence_owned: bool,
    admitting: bool,
    pg_stopped: bool,
    socket_fence: bool,
    tls: bool,
    dropins_started: bool,
    cold: PathBuf,
    original: PathBuf,
    /// The job whose deployment fence `--recover` finishes; else this one.
    fence_owner: Option<PathBuf>,
}

pub(super) fn run(job: Json, mode: &str) -> R<()> {
    let host = Host::default();
    let config = HostConfig::load()?;
    require(
        fs::read_to_string("/proc/sys/kernel/hostname")?.trim() == config.host_name
            && host.out(host.cmd("id").arg("-u"))? == "0",
        "the controller must run as root on the host host.json's host_name names",
    )?;
    let work = std::env::current_dir()?;
    // Settings of the loopback TLS layout differ from those this controller
    // installs, so only the transport cutover may run against it. The
    // launcher names the step a TLS host takes first (launcher_transport.rs).
    let tls = transport::loopback_tls()?;
    require(
        !tls || mode == "postgres-socket",
        "PostgreSQL still uses loopback TLS: run td-mta/deploy, which names the next step",
    )?;
    host.select_tools(binaries::selected()?);
    let job = if mode == "rollback" {
        recover::rollback_job(job, Path::new(OP), &work, Path::new(RELEASES))?
    } else {
        job
    };
    let mut deployment = Deployment::new(host, work, job, tls, &config, mode == "recover")?;
    if mode == "refresh-notifications" {
        require(
            deployment.isolated && !Path::new(FENCE).try_exists()?,
            "active isolated service settings required",
        )?;
        return isolation::prepare_notifications(&deployment.host);
    }
    if mode == "restore-shared-services" {
        // Check job history under the host lock even if a later job isolated
        // the same release again. This precedes the rollback failure writer.
        require_pending_rollback(&deployment.work)?;
        require(
            deployment.isolated,
            "shared service identities are already selected",
        )?;
    }
    if matches!(
        mode,
        "activate"
            | "postgres-socket"
            | "postgres-binaries"
            | "postgres-to-debian"
            | "retire-rollback"
            | "recover"
            | "rollback"
            | "converge-config"
    ) {
        // Reserve the first activation receipt under the host lock. A repeated
        // invocation must leave the original event and recovery evidence intact.
        start_receipt(&deployment.receipt, &deployment.record)?;
    }
    let result = match mode {
        "restore-shared-services" => deployment.restore_shared_services(),
        "activate" | "rollback" => deployment.execute(),
        "postgres-socket" => deployment.postgres_socket(),
        "postgres-binaries" => deployment.postgres_binaries(),
        "postgres-to-debian" => deployment.postgres_to_debian(),
        "retire-rollback" => deployment.retire_rollback(),
        "recover" => deployment.recover(),
        "converge-config" => deployment.converge_config(),
        _ => Err("unknown deployment operation".into()),
    };
    if let Err(ref err) = result {
        set(
            &mut deployment.record,
            "error",
            Json::Str(fmt(format_args!("{err}"))?),
        )?;
        deployment.event("deployment failed")?;
    }
    result
}

impl Deployment {
    fn new(
        host: Host,
        work: PathBuf,
        mut job: Json,
        tls: bool,
        config: &HostConfig,
        recover: bool,
    ) -> R<Self> {
        let basename = name(&work)?;
        require(
            work.parent() == Some(Path::new(OP)) && numbered_name(basename, "deploy-"),
            "operator workspace",
        )?;
        for key in ["revision", "previous", "tree", "main", "controller"] {
            require(lower_hex(field(&job, key)?, 40), "invalid pinned revision")?;
        }
        let release = |key| -> R<PathBuf> {
            child(
                Path::new(RELEASES),
                &fmt(format_args!("git-{}/td-mta", field(&job, key)?))?,
            )
        };
        let previous = release("previous")?;
        let candidate = release("revision")?;
        let seconds = uint(&job, "budget")?;
        require((1..=3600).contains(&seconds), "invalid window")?;
        set(&mut job, "status", "preflight".into())?;
        set(&mut job, "started", now()?.into())?;
        set(&mut job, "events", Json::Arr(Vec::new()))?;
        let isolated = if recover && isolation::partial(tls)? {
            // A recovery removes the partial install its job left.
            false
        } else if tls {
            isolation::installed_tls()?
        } else {
            isolation::installed()?
        };
        Ok(Self {
            host,
            previous,
            candidate,
            budget: Duration::from_secs(seconds),
            record: job,
            cold: child(&work, "candidate-pgdata")?,
            original: child(&work, "original-pgdata")?,
            receipt: child(&work, "receipt.json")?,
            work,
            isolated,
            isolation_change: false,
            plan: None,
            old_version: 0,
            epoch: String::new(),
            selected: String::new(),
            passwords: None,
            builder: release::Builder::new(config)?,
            probes: Probes::new(&config.public_name)?,
            timers: Vec::new(),
            drops: Vec::new(),
            record_listeners: false,
            window: None,
            fence_owned: false,
            admitting: false,
            pg_stopped: false,
            socket_fence: false,
            tls,
            dropins_started: false,
            fence_owner: None,
        })
    }

    fn event(&mut self, message: &str) -> R<f64> {
        let stamp = now()?;
        let mut entry = Vec::new();
        td_alloc::push(&mut entry, stamp.into())?;
        td_alloc::push(&mut entry, Json::Str(owned(message)?))?;
        let Some(Json::Arr(events)) = self.record.get_mut("events") else {
            return Err("receipt events missing".into());
        };
        td_alloc::push(events, Json::Arr(entry))?;
        set(&mut self.record, "status", Json::Str(owned(message)?))?;
        durable(&self.receipt, &self.record)?;
        writeln!(io::stdout().lock(), "{message}")?;
        Ok(stamp)
    }

    fn wait(&self, seconds: u64, reason: &str, mut predicate: impl FnMut() -> R<bool>) -> R<()> {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(seconds) {
            self.host.remaining()?;
            if predicate()? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(fmt(format_args!("{reason} timeout"))?.into())
    }

    fn rolling_back(&self) -> bool {
        field(&self.record, "mode").ok() == Some("rollback")
    }

    fn journal_identity(&mut self) -> R<()> {
        let old = self.native(&self.previous, "schema-inspect", &[], &[], Login::Owner)?;
        self.epoch = owned(field(&old, "store_epoch")?)?;
        self.old_version = uint(&old, "schema_version")?;
        self.selected = self.sql("owner", SELECTED, &[], false)?;
        require(lower_hex(&self.selected, 32), "selected configuration")
    }

    fn execute(&mut self) -> R<()> {
        require(
            !Path::new(FENCE).try_exists()?,
            "unfinished deployment fence: run td-mta/deploy --recover",
        )?;
        require(
            fs::canonicalize(ACTIVE)? == self.previous,
            "production release changed",
        )?;
        self.check_source()?;
        self.socket_preflight()?;
        crate::readiness::prepare(&self.host, false)?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        // Before the build: host.json's public_name must verify now. A
        // rollback leaves a release that may be failing: it records that.
        let live = self
            .healthy(&self.previous)
            .and_then(|()| self.probes.probe("smtp"))
            .and_then(|()| self.probes.probe("jmap"));
        match live {
            Err(e) if self.rolling_back() => set(
                &mut self.record,
                "live_unhealthy",
                Json::Str(fmt(format_args!("{e}"))?),
            )?,
            other => other?,
        }
        if self.previous == self.candidate {
            self.event("already deployed and healthy")?;
            return Ok(());
        }
        self.event("building pinned revision")?;
        self.build()?;
        self.plan = self.classify()?;
        let scoped = self.has_service_grants();
        require(
            scoped || !self.isolated,
            "candidate does not support isolated services",
        )?;
        self.isolation_change = scoped && !self.isolated;
        set(
            &mut self.record,
            "isolation_change",
            self.isolation_change.into(),
        )?;
        set(
            &mut self.record,
            "store_epoch",
            Json::Str(owned(&self.epoch)?),
        )?;
        if scoped {
            service_database::grant_sql(self.service_grants()?)?;
            let passwords = service_database::passwords(self.isolation_change)?;
            isolation::provision(&self.host, &passwords)?;
            self.passwords = Some(passwords);
            self.hba_file()?;
        }
        let migrate = self.needs_migration()?;
        let target = self
            .plan
            .as_ref()
            .map(|plan| uint(plan, "target"))
            .transpose()?
            .unwrap_or(self.old_version);
        for (key, val) in [
            ("schema_before", self.old_version.into()),
            ("schema_after", target.into()),
            ("epoch", Json::Str(owned(&self.epoch)?)),
            ("config", Json::Str(owned(&self.selected)?)),
        ] {
            set(&mut self.record, key, val)?;
        }
        // A migration (schema, service identities or grants) is undone only
        // by restoring the recovery copy it takes first, losing later mail:
        // the job must say so. The receipt names the schema change.
        require(
            !migrate || !self.rolling_back(),
            "the release rolled back to would migrate the database: deploy a forward fix",
        )?;
        require(
            !migrate || self.record.get("irreversible").and_then(Json::as_bool) == Some(true),
            "this release migrates the database, which cannot be rolled back: run td-mta/deploy --irreversible",
        )?;
        let mut recovery = None;
        let result = self.cutover(migrate, &mut recovery);
        let result = if result.is_err() {
            self.host.recover();
            let rollback = if self.admitting {
                Ok(())
            } else {
                self.rollback()
            };
            let admission = if self.admitting {
                self.finish_admission()
            } else {
                Ok(())
            };
            admission.and(rollback).and(result)
        } else {
            result
        };
        self.host.deadline(None);
        let logs = self.finish_probes("listener-samples.json");
        let close = if let Some(recovery) = recovery.as_mut() {
            recovery.close(self)
        } else {
            Ok(())
        };
        let cleanup = self.cleanup(true);
        result.and(logs).and(close).and(cleanup)
    }

    /// Run a fenced operation that keeps the live release. A failure before
    /// writer admission runs `restore`; once admission began it recovers
    /// forward on whatever is selected.
    fn guarded<S>(
        &mut self,
        state: &mut S,
        operation: impl FnOnce(&mut Self, &mut S) -> R<()>,
        restore: impl FnOnce(&mut Self, &mut S) -> R<()>,
    ) -> R<()> {
        let result = operation(self, state);
        let result = if result.is_err() {
            self.host.recover();
            let restore = if self.admitting {
                Ok(())
            } else {
                restore(self, state)
            };
            let admission = if self.admitting {
                self.finish_admission()
            } else {
                Ok(())
            };
            admission.and(restore).and(result)
        } else {
            result
        };
        self.host.deadline(None);
        let logs = self.finish_probes("listener-samples.json");
        let cleanup = self.cleanup(true);
        result.and(logs).and(cleanup)
    }

    /// A fresh verified base backup on the recovery volume for `trial`,
    /// removed afterwards whatever happens.
    fn rehearsal(&mut self, trial: impl FnOnce(&mut Self, &mut Recovery) -> R<()>) -> R<()> {
        let mut copy = Recovery::new(self)?;
        set(
            &mut self.record,
            "recovery",
            Json::Str(owned(copy.work.to_str().ok_or("recovery path")?)?),
        )?;
        let result = (|| -> R<()> {
            let size = self
                .sql(
                    "owner",
                    "SELECT sum(pg_database_size(oid)) FROM pg_database",
                    &[],
                    false,
                )?
                .parse::<u64>()?;
            require(
                free_bytes(&copy.work)?
                    > size
                        .checked_mul(2)
                        .and_then(|n| n.checked_add(4 * GIB))
                        .ok_or("rehearsal size overflow")?,
                "rehearsal capacity",
            )?;
            copy.base_backup(self)?;
            trial(self, &mut copy)
        })();
        let close = copy.close(self);
        // Unlike an activation's, a rehearsal's base backup is evidence of
        // nothing later.
        let base = child(&copy.work, "base").and_then(|base| {
            if base.try_exists()? {
                fs::remove_dir_all(base)?;
            }
            Ok(())
        });
        result.and(close).and(base)
    }

    /// Root, replication, marker and every login of the live release reach
    /// the writable trial behind the holder `ns`.
    fn qualify_trial(&self, ns: &[String]) -> R<()> {
        self.wait(40, "rehearsal startup", || self.ready_db(ns))?;
        require(
            self.sql("owner", "SELECT pg_is_in_recovery()", ns, false)? == "f",
            "writable rehearsal",
        )?;
        let identity = self.sql("replication", "IDENTIFY_SYSTEM", ns, false)?;
        require(
            identity.split('|').next() == Some(SYSTEM),
            "rehearsal replication identity",
        )?;
        require(
            self.sql("marker", "SELECT 1", ns, false)? == "1",
            "rehearsal marker login",
        )?;
        self.config(&self.previous, ns, self.isolated)
    }

    fn cutover(&mut self, migration: bool, recovery: &mut Option<Recovery>) -> R<()> {
        if migration {
            let size = self
                .sql(
                    "owner",
                    "SELECT sum(pg_database_size(oid)) FROM pg_database",
                    &[],
                    false,
                )?
                .parse::<u64>()?;
            require(
                free_bytes(Path::new(PGDATA))? > size.saturating_add(2 * GIB),
                "primary rollback space",
            )?;
            let copy = Recovery::new(self)?;
            set(
                &mut self.record,
                "recovery",
                Json::Str(owned(copy.work.to_str().ok_or("recovery path")?)?),
            )?;
            *recovery = Some(copy);
            self.event("online backup and migration rehearsal")?;
            recovery.as_mut().ok_or("missing recovery")?.prepare(self)?;
            self.copy_cold(true)?;
        } else {
            self.config(&self.candidate, &[], self.isolated)?;
        }
        // A rollback may start from listeners that are down.
        self.prepare_fence(!self.rolling_back(), migration)?;
        self.fence(false, migration)?;
        if let Some(copy) = recovery.as_mut() {
            self.migrate(copy)?;
        } else {
            self.config(&self.candidate, &[], self.isolated)?;
        }
        if self.isolation_change {
            isolation::transfer_clocks(&self.host, true)?;
            self.dropins_started = true;
            isolation::install(&self.host)?;
            self.isolated = true;
            isolation::verify(&self.host, false, self.tls)?;
        }
        self.switch(&self.candidate)?;
        self.admit()?;
        self.wait(240, "ACME lease recovery", || self.acme_online())?;
        self.config(&self.candidate, &[], self.isolated)?;
        self.healthy(&self.candidate)?;
        require(self.active_timers()? == self.timers, "timer set changed")?;
        self.event("verified deployment")?;
        Ok(())
    }

    fn restore_shared_services(&mut self) -> R<()> {
        let rollback = child(&self.work, "shared-rollback.json")?;
        let deployment_receipt = std::mem::replace(&mut self.receipt, rollback);
        let original = read_json(&deployment_receipt, 1024 * 1024)?;
        require(
            original.get("isolation_change").and_then(Json::as_bool) == Some(true)
                && field(&original, "revision")? == field(&self.record, "revision")?
                && field(&original, "previous")? == field(&self.record, "previous")?,
            "incorrect cutover receipt",
        )?;
        require(
            self.isolated
                && !Path::new(FENCE).try_exists()?
                && fs::canonicalize(ACTIVE)? == self.candidate,
            "active release or identity state changed",
        )?;
        self.check_source()?;
        self.journal_identity()?;
        require(
            self.epoch == field(&original, "store_epoch")?,
            "rollback database identity",
        )?;
        let result = reverse_identities(self);
        self.host.deadline(None);
        let logs = self.finish_probes("shared-rollback-listeners.json");
        let cleanup = self.cleanup(false);
        result.and(logs).and(cleanup)
    }

    fn cleanup(&self, primary: bool) -> R<()> {
        if Path::new(FENCE).try_exists()? {
            return Ok(());
        }
        if primary {
            if self.cold.try_exists()? {
                fs::remove_dir_all(&self.cold)?;
            }
            remove_file(Path::new(PERMIT))?;
        }
        for path in &self.drops {
            remove_file(path)?;
        }
        if !self.drops.is_empty() {
            self.host
                .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Login<'a> {
    Owner,
    Runtime,
    Service(&'a str),
}

// Reversing identity isolation selects binaries and service logins on the
// current database. No operation in this interface can replace the primary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Writers {
    Unfenced,
    Fenced,
    Opening,
}

trait SharedRollback {
    fn qualify_previous(&mut self) -> R<()>;
    fn close_writers(&mut self) -> R<()>;
    fn select_shared(&mut self) -> R<()>;
    fn restore_isolated(&mut self) -> R<()>;
    fn finish_opening(&mut self) -> R<()>;
    fn recovery_state(&mut self) -> Writers;
}

fn reverse_identities(host: &mut impl SharedRollback) -> R<()> {
    host.qualify_previous()?;
    let result = host.close_writers().and_then(|()| host.select_shared());
    if result.is_err() {
        match host.recovery_state() {
            Writers::Unfenced => (),
            Writers::Opening => host.finish_opening()?,
            Writers::Fenced => {
                if let Err(error) = host.restore_isolated() {
                    if host.recovery_state() == Writers::Opening {
                        host.finish_opening()?;
                    }
                    return Err(error);
                }
            }
        }
    }
    result
}

impl SharedRollback for Deployment {
    fn qualify_previous(&mut self) -> R<()> {
        self.config(&self.previous, &[], false)
    }
    fn close_writers(&mut self) -> R<()> {
        self.prepare_fence(false, false)?;
        self.fence(true, false)
    }
    fn select_shared(&mut self) -> R<()> {
        isolation::transfer_clocks(&self.host, false)?;
        isolation::rollback(&self.host)?;
        self.isolated = false;
        self.config(&self.previous, &[], false)?;
        self.switch(&self.previous)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "rollback timer set")?;
        self.event(SHARED_RESTORED)?;
        Ok(())
    }
    fn restore_isolated(&mut self) -> R<()> {
        isolation::install(&self.host)?;
        self.isolated = true;
        self.config(&self.candidate, &[], true)?;
        self.switch(&self.candidate)?;
        self.admit()?;
        self.wait(240, "isolated ACME lease", || self.acme_online())?;
        self.healthy(&self.candidate)
    }
    fn finish_opening(&mut self) -> R<()> {
        self.finish_admission()
    }
    fn recovery_state(&mut self) -> Writers {
        self.host.recover();
        if self.admitting {
            Writers::Opening
        } else if self.fence_owned {
            Writers::Fenced
        } else {
            Writers::Unfenced
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    struct Reversal {
        transitions: Vec<&'static str>,
        fault: Option<&'static str>,
        writers: Writers,
    }
    impl Reversal {
        fn new(fault: Option<&'static str>) -> Self {
            Self {
                transitions: Vec::new(),
                fault,
                writers: Writers::Unfenced,
            }
        }
        fn step(&mut self, step: &'static str) -> R<()> {
            self.transitions.push(step);
            if self.fault == Some(step) {
                return Err("injected fault".into());
            }
            Ok(())
        }
    }
    impl SharedRollback for Reversal {
        fn qualify_previous(&mut self) -> R<()> {
            self.step("qualify shared on current data")
        }
        fn close_writers(&mut self) -> R<()> {
            self.step("install fence")?;
            self.writers = Writers::Fenced;
            self.step("stop services")
        }
        fn select_shared(&mut self) -> R<()> {
            self.step("select shared")?;
            self.writers = Writers::Opening;
            self.step("start and verify shared")
        }
        fn restore_isolated(&mut self) -> R<()> {
            self.step("select and verify isolated")
        }
        fn finish_opening(&mut self) -> R<()> {
            self.step("finish admission on selected release")
        }
        fn recovery_state(&mut self) -> Writers {
            self.writers
        }
    }

    #[test]
    fn completed_job_cannot_repeat_shared_rollback() -> R<()> {
        let temp = td_fs::TempDir::new("deployment-shared-retry")?;
        require_pending_rollback(temp.path())?;
        let path = temp.path().join("shared-rollback.json");
        let mut receipt = Json::Obj(Vec::new());
        set(&mut receipt, "status", "deployment failed".into())?;
        durable(&path, &receipt)?;
        require_pending_rollback(temp.path())?;
        set(&mut receipt, "status", SHARED_RESTORED.into())?;
        durable(&path, &receipt)?;
        assert!(require_pending_rollback(temp.path()).is_err());
        assert_eq!(read_json(&path, 4096)?, receipt);
        // Diagnostics or cleanup can fail after services have been verified.
        set(&mut receipt, "status", "deployment failed".into())?;
        set(
            &mut receipt,
            "events",
            Json::Arr(vec![
                Json::Arr(vec![1_u64.into(), SHARED_RESTORED.into()]),
                Json::Arr(vec![2_u64.into(), "deployment failed".into()]),
            ]),
        )?;
        durable(&path, &receipt)?;
        assert!(require_pending_rollback(temp.path()).is_err());
        assert_eq!(read_json(&path, 4096)?, receipt);
        Ok(())
    }

    #[test]
    fn retry_keeps_cutover_evidence() -> R<()> {
        let temp = td_fs::TempDir::new("deployment-retry")?;
        let path = temp.path().join("receipt.json");
        let initial = td_json::parse(r#"{"isolation_change":true,"store_epoch":"old"}"#)?;
        start_receipt(&path, &initial)?;
        assert!(start_receipt(&path, &Json::Null).is_err());
        assert_eq!(read_json(&path, 4096)?, initial);
        Ok(())
    }

    #[test]
    fn shared_rollback_qualifies_and_reopens_current_primary() {
        let mut host = Reversal::new(None);
        reverse_identities(&mut host).unwrap();
        assert_eq!(
            host.transitions,
            [
                "qualify shared on current data",
                "install fence",
                "stop services",
                "select shared",
                "start and verify shared"
            ]
        );
        assert_eq!(host.writers, Writers::Opening);
    }

    #[test]
    fn incompatible_previous_release_leaves_writers_open() {
        let mut host = Reversal::new(Some("qualify shared on current data"));
        assert!(reverse_identities(&mut host).is_err());
        assert_eq!(host.transitions, ["qualify shared on current data"]);
        assert_eq!(host.writers, Writers::Unfenced);
    }

    #[test]
    fn partial_shared_rollback_reinstalls_scoped_release_before_admission() {
        for fault in ["stop services", "select shared"] {
            let mut host = Reversal::new(Some(fault));
            assert!(reverse_identities(&mut host).is_err());
            assert_eq!(host.transitions.last(), Some(&"select and verify isolated"));
        }
        let mut host = Reversal::new(Some("start and verify shared"));
        assert!(reverse_identities(&mut host).is_err());
        assert_eq!(
            host.transitions.last(),
            Some(&"finish admission on selected release")
        );
        assert!(!host.transitions.contains(&"select and verify isolated"));
    }
}
