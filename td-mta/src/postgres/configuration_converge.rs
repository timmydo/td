//! Converge the selected sources to this release's documented defaults plus
//! a reviewed override policy read from standard input. Validates and
//! publishes one immutable version; never selects it.
use super::{configuration, report_error, selection, with_connection, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    process::ExitCode,
};
use td_json::Json;
use td_mta::{
    config::converge::{self, Change, Converged, Policy},
    ids::StoreEpoch,
    postgres_configuration::{self as preparation, Candidate, Profile},
    postgres_configuration_store::{self as versions, Error, Version},
    postgres_input::Loaded,
    postgres_selection::{self, Selection},
};

const NAME: &str = "postgres-config-converge";

pub(super) struct Options {
    connection: String,
    epoch: StoreEpoch,
    /// None plans: validate and report without publishing.
    version: Option<Version>,
}

pub(super) fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut epoch, mut version, mut plan) = (None, None, None, false);
    while let Some(flag) = args.next() {
        if flag == "--plan-only" && !plan {
            plan = true;
            continue;
        }
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            "--version" if version.is_none() => version = Some(Version::parse(&value).ok()?),
            _ => return None,
        }
    }
    if plan == version.is_some() {
        return None;
    }
    Some(Options {
        connection: connection?,
        epoch: epoch?,
        version,
    })
}

/// At most MAX_POLICY_BYTES through EOF; a larger input refuses unread.
fn read_policy(input: &mut impl Read) -> Result<Vec<u8>, &'static str> {
    let mut bytes = td_alloc::filled(converge::MAX_POLICY_BYTES + 1, 0).map_err(|_| "capacity")?;
    let mut length = 0;
    loop {
        let rest = bytes.get_mut(length..).ok_or("policy-too-large")?;
        if rest.is_empty() {
            return Err("policy-too-large");
        }
        match input.read(rest) {
            Ok(0) => break,
            Ok(n) => length = length.checked_add(n).ok_or("policy-too-large")?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("policy-read"),
        }
    }
    bytes.truncate(length);
    Ok(bytes)
}

pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error(NAME, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let policy = match read_policy(&mut io::stdin().lock()).map(|bytes| Policy::parse(&bytes)) {
        Ok(Ok(policy)) => policy,
        Ok(Err(converge::Error::Policy(line))) => {
            // The policy is public reviewed text: name the refused line.
            writeln!(
                io::stdout().lock(),
                "{{\"schema\":1,\"command\":\"{NAME}\",\"status\":\"error\",\"stage\":\"policy\",\"error\":\"policy-syntax\",\"line\":{line}}}"
            )?;
            return Ok(ExitCode::FAILURE);
        }
        Ok(Err(error)) => {
            report_error(NAME, "policy", error.code())?;
            return Ok(ExitCode::FAILURE);
        }
        Err(code) => {
            report_error(NAME, "policy", code)?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let loaded = match Loaded::open(&options.connection, "configuration") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(NAME, "input", error.code())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_connection(&loaded, |request, deadline| {
        let selected = postgres_selection::observe(request, deadline, options.epoch)
            .map_err(selection::classify)?
            .ok_or(AttemptError::Failed("configuration", "unselected"))?;
        let sources = versions::read_sources(request, deadline, options.epoch, selected.version)
            .map_err(configuration::classify)?;
        let (shared, stored) = sources.parts().map_err(configuration::classify)?;
        let failed = |error: converge::Error| {
            let stage = if error.in_policy() {
                "policy"
            } else {
                "configuration"
            };
            AttemptError::Failed(stage, error.code())
        };
        let mut names = td_alloc::with_capacity(stored.len()).map_err(|_| capacity())?;
        for profile in &stored {
            td_alloc::push(&mut names, profile.name).map_err(|_| capacity())?;
        }
        policy.check_profiles(&names).map_err(failed)?;
        let mut converged = td_alloc::with_capacity(stored.len()).map_err(|_| capacity())?;
        for profile in &stored {
            let result = converge::profile(profile.name, profile.source, &policy);
            td_alloc::push(&mut converged, result.map_err(failed)?).map_err(|_| capacity())?;
        }
        let mut profiles = td_alloc::with_capacity(stored.len()).map_err(|_| capacity())?;
        for (profile, converged) in stored.iter().zip(&converged) {
            let profile = Profile {
                source: &converged.source,
                ..*profile
            };
            td_alloc::push(&mut profiles, profile).map_err(|_| capacity())?;
        }
        let changed = stored
            .iter()
            .zip(&converged)
            .any(|(profile, converged)| profile.source != converged.source.as_slice());
        // Even an unchanged source set is revalidated against current
        // materials, as config-check would.
        let candidate = preparation::prepare(request, deadline, options.epoch, shared, &profiles)
            .map_err(|error| configuration::classify(Error::Validation(error)))?;
        let published = options.version.filter(|_| changed);
        // Built first: after a durable publication only writing the
        // report can fail, and a same-ID retry repeats it.
        let report = report(
            options.epoch,
            selected,
            &stored,
            &converged,
            changed,
            published,
            &candidate,
        )
        .map_err(|_| capacity())?;
        if let Some(version) = published {
            versions::publish(request, deadline, version, &candidate)
                .map_err(configuration::classify)?;
        }
        Ok(report)
    });
    match result {
        Ok(value) => {
            writeln!(io::stdout().lock(), "{value}")?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(NAME, stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn capacity() -> AttemptError {
    AttemptError::Failed("configuration", "capacity")
}

fn set(object: &mut Json, key: &str, value: Json) -> Result<(), td_alloc::Error> {
    if let Json::Obj(pairs) = object {
        td_alloc::push(pairs, (td_alloc::string(key)?, value))?;
    }
    Ok(())
}

fn text(value: &str) -> Result<Json, td_alloc::Error> {
    Ok(Json::Str(td_alloc::string(value)?))
}

fn display(value: impl std::fmt::Display) -> Result<Json, td_alloc::Error> {
    td_alloc::format(format_args!("{value}"))
        .map(Json::Str)
        .map_err(|_| td_alloc::Error)
}

fn count(value: Option<u64>) -> Json {
    value.map_or(Json::Null, Json::from)
}

fn change(profile: &str, change: &Change) -> Result<Json, td_alloc::Error> {
    let mut object = Json::Obj(Vec::new());
    set(&mut object, "profile", text(profile)?)?;
    set(&mut object, "section", text(change.section)?)?;
    if let Some(listener) = &change.listener {
        set(&mut object, "listener", text(listener)?)?;
    }
    set(&mut object, "key", text(change.key)?)?;
    set(&mut object, "before", count(change.before))?;
    set(&mut object, "after", count(change.after))?;
    Ok(object)
}

fn summary(profile: &str, converged: &Converged) -> Result<Json, td_alloc::Error> {
    let plan = &converged.plan;
    let limits = plan.limits();
    let mut object = Json::Obj(Vec::new());
    set(&mut object, "name", text(profile)?)?;
    for (key, value) in [
        ("smtp_sessions", limits.smtp_sessions),
        ("smtp_per_peer", limits.smtp_per_peer),
        ("https_connections", limits.https_connections),
        ("event_streams", limits.event_streams),
        ("planned_bytes", plan.total_bytes()),
        ("budget_bytes", limits.memory_budget_bytes),
        (
            "unassigned_bytes",
            limits
                .memory_budget_bytes
                .saturating_sub(plan.total_bytes()),
        ),
    ] {
        set(&mut object, key, Json::from(value))?;
    }
    Ok(object)
}

fn report(
    epoch: StoreEpoch,
    selected: Selection,
    stored: &[Profile<'_>],
    converged: &[Converged],
    changed: bool,
    published: Option<Version>,
    candidate: &Candidate,
) -> Result<Json, td_alloc::Error> {
    let mut changes = Vec::new();
    let mut profiles = Vec::new();
    for (profile, converged) in stored.iter().zip(converged) {
        for item in &converged.changes {
            td_alloc::push(&mut changes, change(profile.name, item)?)?;
        }
        td_alloc::push(&mut profiles, summary(profile.name, converged)?)?;
    }
    let mut digest = [0; 64];
    for ([high, low], byte) in digest
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(candidate.digest())
    {
        *high = hex(byte >> 4);
        *low = hex(byte & 15);
    }
    let digest = std::str::from_utf8(&digest).map_err(|_| td_alloc::Error)?;
    let mut object = Json::Obj(Vec::new());
    set(&mut object, "schema", Json::from(1_u64))?;
    set(&mut object, "command", text(NAME)?)?;
    set(&mut object, "status", text("ok")?)?;
    set(&mut object, "store_epoch", display(epoch)?)?;
    set(&mut object, "generation", Json::from(selected.generation))?;
    set(&mut object, "selected_version", display(selected.version)?)?;
    set(&mut object, "changed", Json::Bool(changed))?;
    set(&mut object, "published", Json::Bool(published.is_some()))?;
    set(
        &mut object,
        "version",
        match published {
            Some(version) => display(version)?,
            None => Json::Null,
        },
    )?;
    set(&mut object, "digest", text(digest)?)?;
    set(&mut object, "changes", Json::Arr(changes))?;
    set(&mut object, "profiles", Json::Arr(profiles))?;
    set(&mut object, "content_validated", Json::Bool(true))?;
    set(&mut object, "profile_activated", Json::Bool(false))?;
    Ok(object)
}

fn hex(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + nibble.saturating_sub(10),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn the_policy_is_read_through_eof_and_bounded() {
        let exact = vec![b'#'; converge::MAX_POLICY_BYTES];
        assert_eq!(
            read_policy(&mut exact.as_slice()).unwrap().len(),
            exact.len()
        );
        let over = vec![b'#'; converge::MAX_POLICY_BYTES + 1];
        assert_eq!(
            read_policy(&mut over.as_slice()).err(),
            Some("policy-too-large")
        );
        assert_eq!(read_policy(&mut &b""[..]).unwrap(), b"");
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
        }
        assert_eq!(read_policy(&mut Broken).err(), Some("policy-read"));
    }
    #[test]
    fn plan_and_publication_take_exactly_one_shape() {
        let base = [
            "--connection-file",
            "/private/owner",
            "--epoch",
            "01010101010101010101010101010101",
        ];
        let version = ["--version", "02020202020202020202020202020202"];
        let with = |tail: &[&str]| {
            arguments(base.iter().chain(tail.iter()).map(OsString::from)).map(|o| o.version)
        };
        assert!(matches!(with(&version), Some(Some(_))));
        assert!(matches!(with(&["--plan-only"]), Some(None)));
        for tail in [
            &[][..],
            &["--plan-only", version[0], version[1]][..],
            &[version[0], version[1], version[0], version[1]][..],
            &["--plan-only", "--plan-only"][..],
            &["--version"][..],
            &["--version", "nothex"][..],
            &["--request", "x", "--plan-only"][..],
        ] {
            assert!(with(tail).is_none(), "{tail:?}");
        }
        for n in 0..base.len() {
            assert!(arguments(
                base.iter()
                    .take(n)
                    .chain(["--plan-only"].iter())
                    .map(OsString::from)
            )
            .is_none());
        }
    }
}
