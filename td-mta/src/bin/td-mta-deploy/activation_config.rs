//! Converge the selected configuration to the live release's documented
//! defaults plus the reviewed policy its revision carries. It goes through
//! the ordinary immutable version, selection and selection rollback.
use super::*;
use crate::common::{copy_json, random_hex};
use std::error::Error;
use td_mta::config::converge::MAX_POLICY_BYTES;

/// The launcher's copy of the live revision's td-mta/production-overrides.conf.
const POLICY: &str = "overrides.conf";
/// Serving polls the selection every 30 seconds, then drains and reloads.
const RELOAD_SECONDS: u64 = 150;
/// POSTGRES.md: an uncertain publication is settled under the same ID.
const INDETERMINATE: &str = "indeterminate-retain-version";

/// The `converge_result` a job records once its outcome is settled.
pub(crate) const RESULTS: &[&str] = &[
    "refused",
    "converged",
    "unchanged",
    "planned",
    "restored",
    "failed",
    "uncertain",
];

type Failure = Box<dyn Error + Send + Sync>;

/// What `config-converge` reported.
pub(super) struct Report {
    pub(super) changed: bool,
    pub(super) published: bool,
    pub(super) version: Option<String>,
    pub(super) digest: String,
    pub(super) generation: u64,
    pub(super) selected: String,
    pub(super) changes: Json,
    pub(super) profiles: Json,
}

/// A refusal `config-converge` printed, or `INDETERMINATE` when its
/// outcome is unknown.
pub(super) struct Refusal {
    pub(super) code: String,
    pub(super) text: String,
}

pub(super) enum Selection {
    /// Recorded at this generation.
    Recorded(u64),
    /// Definitely not applied.
    Refused(Failure),
    /// May or may not have been applied.
    Uncertain(Failure),
}

/// The host operations convergence composes; `Live` runs them, tests fake
/// them to fix the ordering and recovery.
pub(super) trait Steps {
    /// `config-converge` over the selected version; publishes with a version.
    fn converge(&mut self, version: Option<&str>) -> R<Result<Report, Refusal>>;
    /// The digest `config-check` reports for a published version.
    fn check(&mut self, version: &str) -> R<String>;
    /// A selection request for `version` at the observed `generation`.
    fn prepare(&mut self, version: &str, generation: u64) -> R<String>;
    fn select(&mut self, request: &str, version: &str) -> Selection;
    /// Only `Recorded` or `Uncertain`: an unrecorded request is not proof.
    fn inspect(&mut self, request: &str, version: &str) -> Selection;
    /// Every profile serves `generation`.
    fn serving(&mut self, generation: u64) -> R<()>;
    /// `version` is selected at `generation`: every profile serves it, is
    /// ready, and the public listeners answer.
    fn verify(&mut self, version: &str, generation: u64) -> R<()>;
    fn new_version(&mut self) -> R<String>;
    /// Restart serving units that failed, and add them to the receipt's
    /// `config_restarted_units`.
    fn restart_failed(&mut self) -> R<()>;
    /// Record in the receipt durably.
    fn retain(&mut self, key: &str, value: Json) -> R<()>;
    fn event(&mut self, message: &str) -> R<()>;
}

/// Run convergence and record its `converge_result`; an error is returned
/// after the result is durable.
pub(super) fn converge<S: Steps>(steps: &mut S, plan_only: bool, expect: Option<&str>) -> R<()> {
    let (result, error) = match attempt(steps, plan_only, expect) {
        Ok(result) => (result, None),
        Err((result, error)) => (result, Some(error)),
    };
    steps.retain("converge_result", Json::Str(owned(result)?))?;
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

type Stop = (&'static str, Failure);
fn failed(error: Failure) -> Stop {
    ("failed", error)
}
fn text(value: &str) -> R<Json> {
    Ok(Json::Str(owned(value)?))
}
fn both(first: &Failure, context: &str, second: &Failure) -> Failure {
    match fmt(format_args!("{first}; {context}: {second}")) {
        Ok(joined) => joined.into(),
        Err(error) => error,
    }
}

fn attempt<S: Steps>(
    steps: &mut S,
    plan_only: bool,
    expect: Option<&str>,
) -> Result<&'static str, Stop> {
    let plan = steps
        .converge(None)
        .map_err(failed)?
        .map_err(|refusal| failed(refusal.text.into()))?;
    for (key, value) in [
        ("config_changes", &plan.changes),
        ("config_profiles", &plan.profiles),
    ] {
        steps
            .retain(key, copy_json(value).map_err(failed)?)
            .map_err(failed)?;
    }
    steps
        .retain("config_digest", text(&plan.digest).map_err(failed)?)
        .map_err(failed)?;
    steps
        .retain("config_generation", Json::from(plan.generation))
        .map_err(failed)?;
    steps
        .retain("config_selected", text(&plan.selected).map_err(failed)?)
        .map_err(failed)?;
    steps
        .retain("config_reload_seconds", Json::from(RELOAD_SECONDS))
        .map_err(failed)?;
    if expect.is_some_and(|expect| expect != plan.digest) {
        return Err(failed(
            "the derived configuration is not the reviewed plan: plan again".into(),
        ));
    }
    if !plan.changed {
        steps.serving(plan.generation).map_err(failed)?;
        return Ok("unchanged");
    }
    if plan_only {
        return Ok("planned");
    }
    // Retained before publication, as the version contract requires.
    let version = steps.new_version().map_err(failed)?;
    steps
        .retain("config_version", text(&version).map_err(failed)?)
        .map_err(failed)?;
    steps
        .event("publishing converged configuration")
        .map_err(failed)?;
    let published = match steps.converge(Some(&version)).map_err(failed)? {
        Ok(report) => report,
        Err(refusal) if refusal.code == INDETERMINATE => steps
            .converge(Some(&version))
            .map_err(failed)?
            .map_err(|refusal| failed(refusal.text.into()))?,
        Err(refusal) => return Err(failed(refusal.text.into())),
    };
    require(
        published.changed
            && published.published
            && published.version.as_deref() == Some(version.as_str())
            && published.digest == plan.digest
            && published.selected == plan.selected
            && published.generation == plan.generation,
        "the published configuration is not the plan",
    )
    .map_err(failed)?;
    require(
        steps.check(&version).map_err(failed)? == plan.digest,
        "published configuration differs",
    )
    .map_err(failed)?;
    steps
        .retain(
            "config_previous_version",
            text(&plan.selected).map_err(failed)?,
        )
        .map_err(failed)?;
    let generation = match select(
        steps,
        &version,
        plan.generation,
        "selection_request",
        &plan.selected,
    ) {
        Selection::Recorded(generation) => generation,
        Selection::Refused(error) => return Err(failed(error)),
        Selection::Uncertain(error) => return Err(uncertain(steps, error)),
    };
    // From here every failure goes through the restore.
    let verified = text(&version)
        .and_then(|version| steps.retain("config_selected", version))
        .and_then(|()| steps.retain("config_generation", Json::from(generation)))
        .and_then(|()| steps.event("selected converged configuration"))
        .and_then(|()| steps.verify(&version, generation));
    let Err(error) = verified else {
        return Ok("converged");
    };
    if let Ok(message) = fmt(format_args!("{error}")) {
        // Kept even if the restore below loses the receipt's later state.
        if let Ok(message) = text(&message) {
            let _ = steps.retain("config_verify_error", message);
        }
    }
    Err(restore(steps, &plan.selected, &version, generation, error))
}

/// Select the version the converged one replaced, at the generation the
/// converged selection produced, and verify it serves again.
fn restore<S: Steps>(
    steps: &mut S,
    previous: &str,
    converged: &str,
    generation: u64,
    error: Failure,
) -> Stop {
    if let Err(event) = steps.event("restoring previous configuration") {
        return ("failed", both(&error, "restore", &event));
    }
    // A profile that could not load a version exits: each exit below
    // first brings failed serving units back on whatever is selected.
    let generation = match select(steps, previous, generation, "rollback_request", converged) {
        Selection::Recorded(generation) => generation,
        Selection::Refused(refused) => {
            let joined = restarted(steps, both(&error, "restore", &refused));
            return ("failed", joined);
        }
        Selection::Uncertain(uncertain_error) => {
            let joined = restarted(steps, both(&error, "restore", &uncertain_error));
            return uncertain(steps, joined);
        }
    };
    let prepared = text(previous)
        .and_then(|previous| steps.retain("config_selected", previous))
        .and_then(|()| steps.retain("config_generation", Json::from(generation)))
        .and_then(|()| steps.restart_failed());
    if let Err(second) = prepared {
        return (
            "failed",
            both(&error, "restored selection unverified", &second),
        );
    }
    match steps.verify(previous, generation) {
        Ok(()) => ("restored", error),
        Err(second) => {
            let joined = both(&error, "restored selection unverified", &second);
            ("failed", restarted(steps, joined))
        }
    }
}

/// `error`, with any failure to restart failed serving units added.
fn restarted<S: Steps>(steps: &mut S, error: Failure) -> Failure {
    match steps.restart_failed() {
        Ok(()) => error,
        Err(restart) => both(&error, "restarting failed units", &restart),
    }
}

/// The serving units `systemctl list-units --state=failed --no-legend
/// --plain` lists.
pub(super) fn failed_serving_units(listed: &str) -> impl Iterator<Item = &str> {
    listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| UNITS.contains(unit))
}

/// The selection is unknown: the receipt keeps its request.
fn uncertain<S: Steps>(steps: &mut S, error: Failure) -> Stop {
    match steps.retain("config_selected", Json::Null) {
        Ok(()) => ("uncertain", error),
        Err(lost) => ("uncertain", both(&error, "receipt", &lost)),
    }
}

/// Prepare and retain one selection request, then publish it. A failed or
/// uncertain publication is inspected once. While it is in flight the
/// receipt names no selected version; a definite refusal restores
/// `current`.
fn select<S: Steps>(
    steps: &mut S,
    version: &str,
    generation: u64,
    key: &str,
    current: &str,
) -> Selection {
    let request = match steps.prepare(version, generation) {
        Ok(request) => request,
        Err(error) => return Selection::Refused(error),
    };
    let retained = text(&request)
        .and_then(|value| steps.retain(key, value))
        .and_then(|()| steps.retain("config_selected", Json::Null));
    if let Err(error) = retained {
        return Selection::Refused(error);
    }
    let selection = match steps.select(&request, version) {
        Selection::Uncertain(_) => steps.inspect(&request, version),
        settled => settled,
    };
    if let Selection::Refused(error) = selection {
        let restored = text(current).and_then(|current| steps.retain("config_selected", current));
        return match restored {
            Ok(()) => Selection::Refused(error),
            Err(lost) => Selection::Uncertain(both(&error, "receipt", &lost)),
        };
    }
    selection
}

/// How a `config-select` or `config-selection-inspect` result settles a
/// request for `version`. Only a definite refusal by `config-select`
/// means nothing was applied.
#[derive(Debug, PartialEq)]
pub(super) enum Classified<'a> {
    Recorded(u64),
    Refused(&'a str),
    Uncertain(&'a str),
}
pub(super) fn classify<'a>(
    operation: &str,
    success: bool,
    result: Option<&'a Json>,
    version: &str,
) -> Classified<'a> {
    let Some(result) = result else {
        return Classified::Uncertain("unreadable-output");
    };
    if success
        && result.get("recorded").and_then(Json::as_bool) == Some(true)
        && result.get("version").and_then(Json::as_str) == Some(version)
    {
        if let Some(generation) = result.get("generation").and_then(Json::as_u64) {
            return Classified::Recorded(generation);
        }
    }
    let code = result
        .get("error")
        .and_then(Json::as_str)
        .unwrap_or("not-recorded");
    let definite = operation == "config-select"
        && !success
        && result.get("status").and_then(Json::as_str) == Some("error")
        && !code.contains("indeterminate")
        && !code.contains("unknown");
    if definite {
        Classified::Refused(code)
    } else {
        Classified::Uncertain(code)
    }
}

/// The host side of `Steps`.
struct Live<'a> {
    deployment: &'a mut Deployment,
    policy: Vec<u8>,
    restarted: Vec<String>,
}

impl Deployment {
    pub(super) fn converge_config(&mut self) -> R<()> {
        let result = self
            .converge_preflight()
            .and_then(|(policy, plan_only, expect)| {
                let mut live = Live {
                    deployment: self,
                    policy,
                    restarted: Vec::new(),
                };
                converge(&mut live, plan_only, expect.as_deref())
            });
        // Preflight changes nothing.
        if result.is_err() && self.record.get("converge_result").is_none() {
            set(&mut self.record, "converge_result", text("refused")?)?;
        }
        result
    }

    fn converge_preflight(&mut self) -> R<(Vec<u8>, bool, Option<String>)> {
        require(
            !Path::new(FENCE).try_exists()?,
            "unfinished deployment fence: run td-mta/deploy --recover",
        )?;
        require(
            !Path::new(super::fence::PG_FENCE).try_exists()?,
            "unfinished PostgreSQL socket fence: run td-mta/deploy --recover",
        )?;
        require(
            self.previous == self.candidate && fs::canonicalize(ACTIVE)? == self.previous,
            "configuration convergence keeps the live release",
        )?;
        self.check_source()?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        // A probe that already fails would otherwise force a rollback.
        self.probes.probe("smtp")?;
        self.probes.probe("jmap")?;
        let policy = self.policy()?;
        let plan_only = self
            .record
            .get("plan_only")
            .and_then(Json::as_bool)
            .ok_or("invalid plan flag")?;
        let expect = match self.record.get("expect_digest") {
            None | Some(Json::Null) => None,
            Some(value) => {
                let digest = value.as_str().ok_or("invalid expected digest")?;
                require(
                    lower_hex(digest, 64) && !plan_only,
                    "invalid expected digest",
                )?;
                Some(owned(digest)?)
            }
        };
        Ok((policy, plan_only, expect))
    }

    /// The job's policy, exactly the bytes the launcher hashed.
    fn policy(&self) -> R<Vec<u8>> {
        let path = child(&self.work, POLICY)?;
        let (_, meta) = td_fs::open_real_file(&path, "configuration policy")?;
        require(
            meta.uid() == 0 && meta.mode() & 0o7777 == 0o600,
            "invalid configuration policy file",
        )?;
        // The job directory is root's alone; the digest binds the bytes.
        let bytes = td_fs::read_bounded_real_file(
            &path,
            "configuration policy",
            u64::try_from(MAX_POLICY_BYTES)?,
        )?;
        let mut input: &[u8] = &bytes;
        require(
            crate::common::digest_reader(&mut input)? == field(&self.record, "policy_sha256")?,
            "configuration policy digest",
        )?;
        Ok(bytes)
    }

    fn await_generation(&self, generation: u64) -> R<()> {
        for &role in PROFILES {
            let (uid, runtime) = self.profile_runtime(role)?;
            self.wait(RELOAD_SECONDS, "configuration reload", || {
                let out = self.host.exec(
                    self.host
                        .cmd("runuser")
                        .args(["-u", uid, "--"])
                        .arg(&self.previous)
                        .args(["performance-status", "--runtime", &runtime]),
                    &[],
                    Duration::from_secs(20),
                    false,
                )?;
                Ok(out.status.success()
                    && parse_output(&out)
                        .ok()
                        .and_then(|status| status.get("config_generation").and_then(Json::as_u64))
                        == Some(generation))
            })?;
        }
        Ok(())
    }

    fn selection_command(&self, operation: &str, request: &str, version: &str) -> Selection {
        let out = self
            .native_command(
                &self.previous,
                operation,
                &["--request", request],
                &[],
                Login::Owner,
            )
            .and_then(|mut command| {
                self.host
                    .exec(&mut command, &[], Duration::from_secs(40), false)
            });
        let out = match out {
            Ok(out) => out,
            Err(error) => return Selection::Uncertain(error),
        };
        let parsed = parse_output(&out).ok();
        let message = |code: &str| -> Failure {
            match fmt(format_args!(
                "{operation} {code}: the receipt retains its request for config-selection-inspect"
            )) {
                Ok(message) => message.into(),
                Err(error) => error,
            }
        };
        match classify(operation, out.status.success(), parsed.as_ref(), version) {
            Classified::Recorded(generation) => Selection::Recorded(generation),
            Classified::Refused(code) => Selection::Refused(message(code)),
            Classified::Uncertain(code) => Selection::Uncertain(message(code)),
        }
    }
}

impl Steps for Live<'_> {
    fn converge(&mut self, version: Option<&str>) -> R<Result<Report, Refusal>> {
        let d = &*self.deployment;
        let mut extra = Vec::new();
        td_alloc::extend_from_slice(&mut extra, &["--epoch", d.epoch.as_str()])?;
        match version {
            Some(version) => td_alloc::extend_from_slice(&mut extra, &["--version", version])?,
            None => td_alloc::push(&mut extra, "--plan-only")?,
        }
        let mut command =
            d.native_command(&d.previous, "config-converge", &extra, &[], Login::Owner)?;
        let out = d
            .host
            .exec(&mut command, &self.policy, Duration::from_secs(40), false);
        // A publishing run that ends unread is settled under its ID.
        let indeterminate = |error: &Failure| {
            Ok(Err(Refusal {
                code: owned(INDETERMINATE)?,
                text: fmt(format_args!("config-converge outcome unknown: {error}"))?,
            }))
        };
        let (out, report) = match out.map(|out| (parse_output(&out), out)) {
            Ok((Ok(report), out)) => (out, report),
            Err(error) | Ok((Err(error), _)) if version.is_some() => return indeterminate(&error),
            Err(error) | Ok((Err(error), _)) => return Err(error),
        };
        if out.status.success() && report.get("status").and_then(Json::as_str) == Some("ok") {
            let flag = |key| {
                report
                    .get(key)
                    .and_then(Json::as_bool)
                    .ok_or("invalid convergence report")
            };
            let copy = |key| copy_json(report.get(key).ok_or("invalid convergence report")?);
            let digest = owned(field(&report, "digest")?)?;
            require(lower_hex(&digest, 64), "invalid convergence digest")?;
            return Ok(Ok(Report {
                changed: flag("changed")?,
                published: flag("published")?,
                version: report
                    .get("version")
                    .and_then(Json::as_str)
                    .map(owned)
                    .transpose()?,
                digest,
                generation: uint(&report, "generation")?,
                selected: owned(field(&report, "selected_version")?)?,
                changes: copy("changes")?,
                profiles: copy("profiles")?,
            }));
        }
        let detail = |key| report.get(key).and_then(Json::as_str).unwrap_or("unknown");
        let code = owned(detail("error"))?;
        let text = match report.get("line").and_then(Json::as_u64) {
            Some(line) => fmt(format_args!(
                "config-converge refused: {} {code} at policy line {line}",
                detail("stage")
            ))?,
            None => fmt(format_args!(
                "config-converge refused: {} {code}",
                detail("stage")
            ))?,
        };
        Ok(Err(Refusal { code, text }))
    }

    fn check(&mut self, version: &str) -> R<String> {
        let d = &*self.deployment;
        let check = d.native(
            &d.previous,
            "config-check",
            &["--epoch", &d.epoch, "--version", version],
            &[],
            Login::Owner,
        )?;
        owned(field(&check, "digest")?)
    }

    fn prepare(&mut self, version: &str, generation: u64) -> R<String> {
        let d = &*self.deployment;
        let generation = fmt(format_args!("{generation}"))?;
        let prepared = d.native(
            &d.previous,
            "config-selection-prepare",
            &[
                "--epoch",
                &d.epoch,
                "--version",
                version,
                "--generation",
                &generation,
            ],
            &[],
            Login::Owner,
        )?;
        let request = owned(field(&prepared, "request")?)?;
        require(lower_hex(&request, 178), "invalid selection request")?;
        Ok(request)
    }

    fn select(&mut self, request: &str, version: &str) -> Selection {
        // A retained request is published to its end: an interrupt must not
        // abandon it mid-commit, and later steps verify or restore it.
        self.deployment.host.recover();
        self.deployment
            .selection_command("config-select", request, version)
    }

    fn inspect(&mut self, request: &str, version: &str) -> Selection {
        match self
            .deployment
            .selection_command("config-selection-inspect", request, version)
        {
            Selection::Refused(error) => Selection::Uncertain(error),
            settled => settled,
        }
    }

    fn serving(&mut self, generation: u64) -> R<()> {
        self.deployment.await_generation(generation)
    }

    fn verify(&mut self, version: &str, generation: u64) -> R<()> {
        // The configuration check compares the database with this.
        self.deployment.selected = owned(version)?;
        let d = &*self.deployment;
        d.await_generation(generation)?;
        d.config(&d.previous, &[], d.isolated)?;
        d.healthy(&d.previous)?;
        d.probes.probe("smtp")?;
        d.probes.probe("jmap")
    }

    fn new_version(&mut self) -> R<String> {
        random_hex(16)
    }

    fn restart_failed(&mut self) -> R<()> {
        let d = &*self.deployment;
        let listed = d.host.out(d.host.cmd("systemctl").args([
            "list-units",
            "--state=failed",
            "--no-legend",
            "--plain",
            "td-mta-*",
        ]))?;
        // Every unit is attempted and recorded; the first failure returns.
        let mut first: Option<Failure> = None;
        for unit in failed_serving_units(&listed) {
            let started = d
                .host
                .run(d.host.cmd("systemctl").args(["reset-failed", unit]))
                .and_then(|()| d.host.run(d.host.cmd("systemctl").args(["start", unit])));
            match started {
                Ok(()) if !self.restarted.iter().any(|known| known == unit) => {
                    td_alloc::push(&mut self.restarted, owned(unit)?)?;
                }
                Ok(()) => {}
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        let mut units = td_alloc::with_capacity(self.restarted.len())?;
        for unit in &self.restarted {
            td_alloc::push(&mut units, Json::Str(owned(unit)?))?;
        }
        self.retain("config_restarted_units", Json::Arr(units))?;
        match first {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn retain(&mut self, key: &str, value: Json) -> R<()> {
        set(&mut self.deployment.record, key, value)?;
        durable(&self.deployment.receipt, &self.deployment.record)
    }

    fn event(&mut self, message: &str) -> R<()> {
        self.deployment.event(message).map(|_| ())
    }
}

#[cfg(test)]
#[path = "tests/activation_config.rs"]
mod tests;
