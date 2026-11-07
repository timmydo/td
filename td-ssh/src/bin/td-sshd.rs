//! `td-sshd`: dispatches to one of its stages. Only the listener, `-t` and `-T`
//! are for people; the rest are how the stages re-execute this file.
#![deny(unsafe_code)]

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use td_ssh::server::account::{self, SingleUser};
use td_ssh::server::config::ServerConfig;
use td_ssh::server::{listener, log, monitor, preauth, session, signer};

const USAGE: &str =
    "usage: td-sshd -f config [--single-user name]\n       td-sshd -t -f config\n       td-sshd -T -f config [-C user=NAME[,host=H,addr=A]]";

#[derive(Default)]
struct Arguments {
    config: Option<String>,
    single_user: Option<String>,
    test: bool,
    print: bool,
    connection_spec: Option<String>,
    stage: Option<String>,
    read_user_file: Option<String>,
    read_as: Option<String>,
}

fn parse() -> Result<Arguments, String> {
    let mut parsed = Arguments::default();
    let mut args = std::env::args_os().skip(1).map(|arg| {
        arg.into_string()
            .map_err(|arg| format!("argument {} is not UTF-8", arg.display()))
    });
    while let Some(arg) = args.next() {
        let arg = arg?;
        let mut value = |name: &str| {
            args.next()
                .unwrap_or_else(|| Err(format!("{name} needs a value")))
        };
        match arg.as_str() {
            "-f" => parsed.config = Some(value("-f")?),
            "-t" => parsed.test = true,
            "-T" => parsed.print = true,
            "-C" => parsed.connection_spec = Some(value("-C")?),
            "--single-user" => parsed.single_user = Some(value("--single-user")?),
            "--connection" | "--preauth" | "--session" | "--signer" => parsed.stage = Some(arg),
            account::READ_USER_FILE_FLAG => {
                parsed.read_user_file = Some(value(account::READ_USER_FILE_FLAG)?);
            }
            account::READ_AS_FLAG => parsed.read_as = Some(value(account::READ_AS_FLAG)?),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(parsed)
}

fn single_user(name: Option<String>) -> Result<Option<SingleUser>, String> {
    let Some(name) = name else {
        return Ok(None);
    };
    let metadata =
        std::fs::metadata("/proc/self").map_err(|error| format!("/proc/self: {error}"))?;
    account::check_single_user(metadata.uid())?;
    Ok(Some(SingleUser {
        name,
        uid: metadata.uid(),
        gid: metadata.gid(),
        home: std::env::var("HOME").unwrap_or_else(|_| "/".to_string()),
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
    }))
}

fn user_of(spec: Option<&str>) -> Result<Option<String>, String> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    let mut user = None;
    for part in spec.split(',') {
        match part.split_once('=') {
            Some(("user", value)) => user = Some(value.to_string()),
            Some(("host" | "addr", _)) => {}
            _ => return Err(format!("-C {part}: expected user=, host= or addr=")),
        }
    }
    Ok(user)
}

fn run() -> Result<i32, String> {
    let arguments = parse()?;
    if let Some(path) = arguments.read_user_file {
        return Ok(account::read_user_file_main(
            &path,
            arguments.read_as.as_deref(),
        ));
    }
    let single = single_user(arguments.single_user)?;
    let drop_identity = single.is_none();
    match arguments.stage.as_deref() {
        Some("--preauth") => return Ok(preauth::main(drop_identity)),
        Some("--signer") => return Ok(signer::main(drop_identity)),
        Some("--session") => return Ok(session::main(!drop_identity)),
        _ => {}
    }
    let config_path = arguments.config.ok_or(USAGE)?;
    if arguments.print {
        // Parses the file alone, reading no key.
        let text = std::fs::read_to_string(&config_path)
            .map_err(|error| format!("{config_path}: {error}"))?;
        let config = ServerConfig::parse(&text, single.is_some())
            .map_err(|error| format!("{config_path}: {error}"))?;
        let user = user_of(arguments.connection_spec.as_deref())?;
        std::io::stdout()
            .write_all(config.describe(user.as_deref()).as_bytes())
            .map_err(|error| format!("stdout: {error}"))?;
        return Ok(0);
    }
    if arguments.test {
        listener::validate(&config_path, single.as_ref())?;
        return Ok(0);
    }
    if arguments.stage.as_deref() == Some("--connection") {
        return Ok(monitor::main(&monitor::MonitorArgs {
            config_path: &config_path,
            single_user: single,
        }));
    }
    Ok(listener::main(&config_path, single.as_ref()))
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            log::line(format_args!("td-sshd: {error}"));
            std::process::exit(1);
        }
    }
}
