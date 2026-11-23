//! `--converge-config`: the live revision's reviewed override policy for the
//! job, and the outcome line that ends the command, whatever happened.
use super::*;
use std::io::Write;
use td_mta::config::converge::MAX_POLICY_BYTES;

/// Reviewed production differences from the defaults (PRODUCTION.md,
/// "Converge the configuration"); read at the live release's revision.
pub(super) const POLICY_PATH: &str = "td-mta/production-overrides.conf";

/// The policy as the live release's revision has it, so the defaults it
/// differs from and the policy were reviewed together.
pub(super) fn policy(local: &Local, live: &str) -> R<Vec<u8>> {
    let spec = fmt(format_args!("{live}:{POLICY_PATH}"))?;
    let out = local.host.exec(
        local.git().args(["cat-file", "blob", &spec]),
        &[],
        Duration::from_secs(30),
        false,
    )?;
    require(
        out.status.success(),
        "the live release has no td-mta/production-overrides.conf: deploy a release that has it first",
    )?;
    require(
        out.stdout.len() <= MAX_POLICY_BYTES,
        "td-mta/production-overrides.conf exceeds 64 KiB",
    )?;
    Ok(out.stdout)
}

/// The host job, once started.
pub(super) struct Launch {
    pub(super) receipt: String,
    pub(super) unit: String,
}

/// What one invocation has said and started, so that it ends with exactly
/// one outcome line.
#[derive(Default)]
pub(super) struct Session {
    pub(super) reported: bool,
    pub(super) launched: Option<Launch>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Job {
    Succeeded,
    Failed,
    /// ssh lost the connection; the detached job may still run.
    Lost,
}

/// `--config` and `--jmap-account` as given, for the next command.
pub(super) fn carried(args: &[String]) -> R<Vec<&str>> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--config" || arg == "--jmap-account" {
            if let Some(value) = it.next() {
                td_alloc::extend_from_slice(&mut out, &[arg.as_str(), value.as_str()])?;
            }
        }
    }
    Ok(out)
}

/// A launcher error: a refusal before the job started changed nothing; after
/// it started, the job's outcome is unknown here.
pub(super) fn unreported(
    out: &mut impl Write,
    session: &mut Session,
    error: &str,
    carried: &[&str],
) -> R<()> {
    if session.reported {
        return Ok(());
    }
    match &session.launched {
        None => write(out, "refused", Some(error), None, None, carried)?,
        Some(launch) => write(out, "unknown", Some(error), None, Some(launch), carried)?,
    }
    session.reported = true;
    Ok(())
}

/// The job's receipt as the launcher could read it after the job.
pub(super) enum Receipt<'a> {
    Text(&'a str),
    /// The host has none: the supervisor refused before reserving it.
    Missing,
    /// It could not be read, so nothing is known.
    Unreadable,
}

/// The job's last word: the receipt's settled result, the error if the job
/// or the public check after it failed, and the command that follows. A
/// supervisor that exits records `converge_result`, so a receipt without
/// one means the job died or is still running.
pub(super) fn finish(
    out: &mut impl Write,
    session: &mut Session,
    receipt: Receipt<'_>,
    job: Job,
    public: R<f64>,
    carried: &[&str],
) -> R<()> {
    let record = match receipt {
        Receipt::Text(text) => td_json::parse(text).ok(),
        Receipt::Missing | Receipt::Unreadable => None,
    };
    let settled = record
        .as_ref()
        .and_then(|record| record.get("converge_result"))
        .and_then(Json::as_str)
        .filter(|result| crate::activate::CONVERGE_RESULTS.contains(result));
    let recorded = record
        .as_ref()
        .and_then(|record| record.get("error"))
        .and_then(Json::as_str);
    let (result, error): (&str, Option<&str>) = match (settled, &receipt, job) {
        (Some(result), _, Job::Failed) => (result, Some(recorded.unwrap_or("the host job failed"))),
        // These results always fail, even read before their error is.
        (Some(result @ ("refused" | "restored" | "failed" | "uncertain")), _, _) => {
            (result, Some(recorded.unwrap_or("the host job failed")))
        }
        (Some(result), _, _) => (result, recorded),
        (None, Receipt::Missing, Job::Failed) => (
            "refused",
            Some("the host job refused before writing its receipt; see its unit's journal"),
        ),
        (None, _, Job::Lost) => (
            "unknown",
            Some("the connection to the host job was lost before it settled"),
        ),
        (None, Receipt::Unreadable, _) => (
            "unknown",
            Some("the job's receipt could not be read; read it on the host"),
        ),
        (None, _, _) => (
            "unknown",
            Some(recorded.unwrap_or("the host job ended without settling its result")),
        ),
    };
    let error = match (error, public) {
        (Some(error), _) => Some(owned(error)?),
        (None, Err(public)) => Some(fmt(format_args!(
            "public JMAP check after the job: {public}"
        ))?),
        (None, Ok(_)) => None,
    };
    write(
        out,
        result,
        error.as_deref(),
        record.as_ref(),
        session.launched.as_ref(),
        carried,
    )?;
    session.reported = true;
    match error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// The shared outcome line (`super::Outcome`) for a convergence: the
/// selected version, what changed, the job's receipt and unit, any units
/// the job restarted, and after a successful plan the apply command bound
/// to its digest.
pub(super) fn write(
    out: &mut impl Write,
    result: &str,
    error: Option<&str>,
    record: Option<&Json>,
    launch: Option<&Launch>,
    carried: &[&str],
) -> R<()> {
    let mut details = Json::Obj(Vec::new());
    if let Some(launch) = launch {
        set(&mut details, "receipt", Json::Str(owned(&launch.receipt)?))?;
        set(&mut details, "unit", Json::Str(owned(&launch.unit)?))?;
    }
    let request = if result == "uncertain" {
        record.and_then(|record| {
            record
                .get("rollback_request")
                .or_else(|| record.get("selection_request"))
        })
    } else {
        None
    };
    for (from, to, value) in [
        ("config_selected", "selected_version", None),
        ("config_changes", "changes", None),
        ("config_version", "config_version", None),
        ("config_previous_version", "previous_config_version", None),
        ("config_reload_seconds", "reload_seconds", None),
        ("config_restarted_units", "restarted_units", None),
        ("", "request", request),
    ] {
        let value = value.or_else(|| record.and_then(|record| record.get(from)));
        if let Some(value) = value {
            set(&mut details, to, crate::common::copy_json(value)?)?;
        }
    }
    let digest = record
        .and_then(|record| record.get("config_digest"))
        .and_then(Json::as_str);
    let mut argv = Vec::new();
    let next = match (result, error, digest) {
        ("planned", None, Some(digest)) => {
            td_alloc::extend_from_slice(
                &mut argv,
                &[
                    "td-mta/deploy",
                    "--converge-config",
                    "--expect-digest",
                    digest,
                ],
            )?;
            td_alloc::extend_from_slice(&mut argv, carried)?;
            Some(Next {
                argv: &argv,
                needs: &[],
            })
        }
        _ => None,
    };
    write_outcome(
        out,
        &Outcome {
            result,
            host_key: None,
            error,
            details: Some(&details),
            next,
        },
    )
}

#[cfg(test)]
#[path = "tests/converge.rs"]
mod tests;
