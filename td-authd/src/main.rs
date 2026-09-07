#![deny(unsafe_code)]

mod channel;
mod launch;
mod mount_sys;
mod portal_files;
mod sys;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: td-authd channel-check --peer-uid UID | \
    td-authd terminal-serve --user USER --uid UID --peer-uid UID | \
    td-authd terminal-exec UID GENERATION HANDLE | td-authd prepare-portal-files | \
    td-authd release-portal-files";

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

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(std::io::stderr().lock(), "td-authd: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "../tests/confinement.rs"]
mod confinement;
