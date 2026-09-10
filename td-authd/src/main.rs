#![deny(unsafe_code)]

mod application;
mod application_shell;
mod shell_channel;
mod terminal;
mod terminal_sys;
mod application_files;
mod channel;
#[allow(
    dead_code,
    reason = "immutable trusted-prompt contract; authority consumer follows"
)]
mod consent;
mod launch;
mod unlock;
mod secret_intake;
#[allow(dead_code, reason = "shared public credential transport and client codec")]
mod secret_request;
#[allow(dead_code, reason = "shared public credential transport and client codec")]
mod secret_sys;
mod session;
mod inspection;
mod mount_sys;
mod portal_files;
mod sys;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: td-authd channel-check --peer-uid UID | \
    td-authd terminal-serve --user USER --uid UID --peer-uid UID | \
    td-authd terminal-exec UID GENERATION HANDLE | td-authd prepare-portal-files | \
    td-authd release-portal-files | td-authd prepare-application-files APP | \
    td-authd release-application-files APP | \
    td-authd application-start OWNER APP direct|terminal|shell -- ARG... | \
     td-authd application-exec UID OWNER APP direct|terminal|shell -- ARG... | \
     td-authd application-client ARG... | td-authd application-probe";

fn run(arguments: &[String]) -> Result<(), String> {
    if arguments == ["prepare-portal-files"] {
        return portal_files::prepare();
    }
    if arguments == ["release-portal-files"] {
        return portal_files::release();
    }
    if arguments == ["portal-file-namespace"] {
        return portal_files::namespace_helper();
    }
    if let Some((verb, rest)) = arguments.split_first() {
        if verb == "prepare-application-files" {
            return application_files::prepare(rest);
        }
        if verb == "release-application-files" {
            return application_files::release(rest);
        }
        if verb == "application-start" {
            return application::start(rest);
        }
        if verb == "application-exec" {
            return application::exec(rest);
        }
        if verb == "terminal-exec" {
            return launch::terminal_exec(rest);
        }
        if verb == "terminal-serve" {
            let config = launch::Config::parse(rest)?;
            launch::require_launch_startup()?;
            let channel =
                channel::Channel::from_stdin(config.peer_uid()).map_err(|e| e.to_string())?;
            return launch::serve(channel, config);
        }
    }
    let [verb, option, value] = arguments else {
        return Err(USAGE.into());
    };
    if verb != "channel-check" || option != "--peer-uid" {
        return Err(USAGE.into());
    }
    let uid = value.parse::<u32>().map_err(|_| "invalid peer uid")?;
    if matches!(uid, 65534 | u32::MAX) || uid.to_string() != *value {
        return Err("invalid peer uid".into());
    }
    // Authenticate before any future worker, subprocess, or fd delegation.
    let mut channel = channel::Channel::from_stdin(uid).map_err(|e| e.to_string())?;
    channel.send(b"channel-check").map_err(|e| e.to_string())?;
    if channel.receive().map_err(|e| e.to_string())? != b"channel-check" {
        return Err("authority channel check payload mismatch".into());
    }
    writeln!(
        std::io::stdout().lock(),
        "TD-AUTH-CHANNEL: peer pinned; no operations enabled"
    )
    .map_err(|e| e.to_string())?;
    // Each ping must arrive within five seconds. Peer loss fails the pair;
    // the stdout marker, not a successful exit, proves this diagnostic.
    loop {
        if channel.receive().map_err(|e| e.to_string())? != b"ping" {
            return Err("channel check accepts only ping".into());
        }
        channel.send(b"pong").map_err(|e| e.to_string())?;
    }
}

fn entry() -> Result<u8, String> {
    let mut arguments = std::env::args_os().skip(1).peekable();
    if arguments
        .peek()
        .is_some_and(|s| s == "application-client" || s == "application-probe")
    {
        let probe = arguments.next().is_some_and(|s| s == "application-probe");
        return application_shell::client(arguments.collect(), probe).map_err(|e| e.to_string());
    }
    let arguments = arguments
        .map(|s| s.into_string().map_err(|_| "invalid argument encoding"))
        .collect::<Result<Vec<_>, _>>()?;
    run(&arguments).map(|()| 0)
}

fn main() -> ExitCode {
    let result = entry();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(why) => {
            let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "../tests/confinement.rs"]
mod confinement;
