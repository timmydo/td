//! Read-only operator qualification; never opens mail service admission.
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    process::ExitCode,
    time::{Duration, Instant},
};
use td_mta::{postgres_input::Loaded, postgres_probe, postgres_resolver};

struct Options {
    connection: String,
    profile: String,
}

fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    if args.next().as_deref() != Some(OsStr::new("probe")) {
        return None;
    }
    let mut connection = None;
    let mut profile = None;
    while let Some(flag) = args.next() {
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => {
                connection = Some(args.next()?.into_string().ok()?);
            }
            "--profile" if profile.is_none() => profile = Some(args.next()?.into_string().ok()?),
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        profile: profile?,
    })
}

pub fn run(args: impl Iterator<Item = OsString>) -> io::Result<ExitCode> {
    let Some(options) = arguments(args) else {
        report_error("arguments", "invalid-arguments")?;
        return Ok(ExitCode::from(2));
    };
    let loaded = match Loaded::open(&options.connection, &options.profile) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_error("bootstrap", &error.to_string())?;
            return Ok(ExitCode::FAILURE);
        }
    };
    let result = check(&loaded);
    match result {
        Ok(status) => {
            writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"postgres-probe\",\"status\":\"ok\",\"server_version\":{},\"schema_checked\":false,\"profile_checked\":false}}", status.server_version)?;
            Ok(ExitCode::SUCCESS)
        }
        Err((stage, code)) => {
            report_error(stage, code)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn report_error(stage: &str, code: &str) -> io::Result<()> {
    writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"postgres-probe\",\"status\":\"error\",\"stage\":\"{stage}\",\"error\":\"{code}\"}}")
}

fn check(loaded: &Loaded) -> Result<postgres_probe::Status, (&'static str, &'static str)> {
    let executable = std::env::current_exe().map_err(|_| ("resolver", "executable"))?;
    // Bootstrap files are trusted cold I/O. DNS and every attempted address
    // spend one subsequent network deadline; retries never renew it.
    let now = Instant::now();
    let deadline = now + Duration::from_secs(10);
    let addresses =
        postgres_resolver::resolve(&executable, &loaded.bootstrap, now + Duration::from_secs(5))
            .map_err(|error| {
                (
                    "resolver",
                    match error {
                        postgres_resolver::Error::Deadline => "deadline",
                        _ => "resolution-failed",
                    },
                )
            })?;
    for address in addresses {
        match postgres_probe::probe(&loaded.bootstrap.request(address), &loaded.trust, deadline) {
            Ok(status) => return Ok(status),
            Err(postgres_probe::Error::Transport) => continue,
            Err(error) => {
                return Err((
                    "database",
                    match error {
                        postgres_probe::Error::Invalid => "invalid-request",
                        postgres_probe::Error::Deadline => "deadline",
                        postgres_probe::Error::Tls => "tls",
                        postgres_probe::Error::Database => "connection-or-authentication",
                        postgres_probe::Error::Settings => "primary-or-durability-settings",
                        postgres_probe::Error::Transport => "transport",
                    },
                ))
            }
        }
    }
    Err(exhausted(deadline))
}

fn exhausted(deadline: Instant) -> (&'static str, &'static str) {
    (
        "database",
        if Instant::now() >= deadline {
            "deadline"
        } else {
            "transport"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Option<Options> {
        arguments(args.iter().map(OsString::from))
    }
    #[test]
    fn last_address_exhaustion_distinguishes_deadline_from_transport() {
        assert_eq!(exhausted(Instant::now()), ("database", "deadline"));
        assert_eq!(
            exhausted(Instant::now() + Duration::from_secs(60)),
            ("database", "transport")
        );
    }
    #[test]
    fn command_requires_one_protected_file_and_profile() {
        assert!(parse(&[
            "probe",
            "--connection-file",
            "/private/db",
            "--profile",
            "desktop"
        ])
        .is_some());
        assert!(parse(&[
            "probe",
            "--profile",
            "desktop",
            "--connection-file",
            "/private/db"
        ])
        .is_some());
        for args in [
            vec![],
            vec!["check"],
            vec!["probe"],
            vec!["probe", "--connection-file", "/private/db"],
            vec!["probe", "--profile", "desktop"],
            vec!["probe", "--connection", "secret", "--profile", "desktop"],
            vec![
                "probe",
                "--connection-file",
                "/a",
                "--profile",
                "desktop",
                "--profile",
                "other",
            ],
            vec![
                "probe",
                "--connection-file",
                "/a",
                "--connection-file",
                "/b",
                "--profile",
                "desktop",
            ],
        ] {
            assert!(parse(&args).is_none());
        }
    }
}
