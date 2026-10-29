//! Passive owner export for configuration repair; never validates or activates.
use super::{configuration, report_error, selection, with_addresses, AttemptError};
use std::{
    ffi::OsString,
    io::{self, Write},
    process::ExitCode,
};
use td_json::{json, Json};
use td_mta::{
    ids::StoreEpoch, postgres_configuration_store as versions, postgres_input::Loaded,
    postgres_selection,
};

fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<(String, StoreEpoch)> {
    let (mut connection, mut epoch) = (None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--epoch" if epoch.is_none() => epoch = Some(StoreEpoch::parse(&value).ok()?),
            _ => return None,
        }
    }
    Some((connection?, epoch?))
}

pub(super) fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    const NAME: &str = "postgres-config-snapshot";
    let Some((connection, epoch)) = arguments(args) else {
        report_error(NAME, "arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&connection, "configuration") {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error(NAME, "input", error.code())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = with_addresses(&loaded, |request, deadline| {
        let selected = postgres_selection::observe(request, &loaded.trust, deadline, epoch)
            .map_err(selection::classify)?;
        let Some(selected) = selected else {
            return Ok(json!({"schema":1,"command":NAME,"status":"ok",
                "store_epoch":epoch.to_string(),"selected":false,
                "content_validated":false,"profile_activated":false}));
        };
        let sources =
            versions::read_sources(request, &loaded.trust, deadline, epoch, selected.version)
                .map_err(configuration::classify)?;
        report(epoch, selected, &sources)
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

fn report(
    epoch: StoreEpoch,
    selected: postgres_selection::Selection,
    sources: &versions::Sources,
) -> Result<Json, AttemptError> {
    let (shared, profiles) = sources.parts().map_err(configuration::classify)?;
    let text = |bytes| {
        std::str::from_utf8(bytes).map_err(|_| AttemptError::Failed("configuration", "source-text"))
    };
    let mut items = Vec::new();
    items
        .try_reserve_exact(profiles.len())
        .map_err(|_| AttemptError::Failed("configuration", "capacity"))?;
    for profile in profiles {
        items.push(json!({"name":profile.name,"roles":profile.roles.bits(),"source":text(profile.source)?}));
    }
    Ok(
        json!({"schema":1,"command":"postgres-config-snapshot","status":"ok",
        "store_epoch":epoch.to_string(),"selected":true,"generation":selected.generation,
        "version":selected.version.to_string(),"shared":text(shared)?,"profiles":items,
        "content_validated":false,"profile_activated":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_accepts_only_explicit_connection_and_epoch() {
        let base = [
            "--connection-file",
            "/private/owner",
            "--epoch",
            "01010101010101010101010101010101",
        ];
        assert!(arguments(base.iter().map(OsString::from)).is_some());
        for tail in [
            vec!["--epoch", base[3]],
            vec!["--version", base[3]],
            vec!["--connection-file"],
            vec!["--request", "x"],
        ] {
            assert!(arguments(base.iter().chain(tail.iter()).map(OsString::from)).is_none());
        }
        for n in 0..base.len() {
            assert!(arguments(base.iter().take(n).map(OsString::from)).is_none());
        }
    }
}
