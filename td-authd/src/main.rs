#![deny(unsafe_code)]

// The admission record's bytes, one reviewed copy with td-boot's selector
// and its `admit` verb (td-install/ENCRYPTION.md, "Verified account
// handoff").
#[path = "../../td-boot/src/admission.rs"]
#[allow(dead_code, reason = "the record's writer is td-boot's")]
mod admission;
mod application;
mod application_cache;
mod application_files;
mod backoff;
mod channel;
#[allow(
    dead_code,
    reason = "immutable trusted-prompt contract; authority consumer follows"
)]
mod consent;
// The cutover record's bytes and the root-only publication firstboot's
// renders share: one reviewed copy (amendment 7).
#[path = "../../td-firstboot/src/cutover.rs"]
mod cutover;
mod deployment;
mod disk_install;
mod elevation;
// The application cache's status record and declarations, td-app's.
#[path = "../../engine/src/flathub_app.rs"]
#[allow(
    dead_code,
    reason = "td-app's and td-jail's halves of the cache contract"
)]
mod flathub_app;
mod handoff;
// Request 1a's kernel name and consent's hostname descriptions.
#[path = "../../td-firstboot/src/hostname.rs"]
mod hostname;
mod inspection;
#[allow(dead_code, reason = "shared codec; the service sends the other half")]
#[path = "../../td-install/src/installation_consent.rs"]
mod installation_consent;
mod launch;
mod login;
mod login_status;
// Request 19's tier marker and request 1d's selectors, one reviewed copy.
#[path = "../../td-secret/src/login_tier.rs"]
#[allow(
    dead_code,
    reason = "the retained-deployment reader serves td-secret's login worker"
)]
mod login_tier;
mod mount_sys;
mod notebook;
mod notebook_token;
#[path = "../../td-secret/src/notebook_wire.rs"]
#[allow(dead_code, reason = "shared notebook transport halves")]
mod notebook_wire;
mod portal_files;
mod primary_account;
mod proof;
mod revocation;
mod rollback;
// The saved hostname's bounded read and synced write, and the synced write
// the backoff file shares: td-firstboot's provisioning source.
#[path = "../../td-firstboot/src/saved.rs"]
mod saved;
mod secret_intake;
#[allow(
    dead_code,
    reason = "shared public credential transport and client codec"
)]
mod secret_request;
#[allow(
    dead_code,
    reason = "shared public credential transport and client codec"
)]
mod secret_sys;
mod session;
mod set_hostname;
#[allow(dead_code, reason = "shared dependency-free SHA-256 implementation")]
#[path = "../../engine/src/sha256.rs"]
mod sha256;
mod software;
mod storage;
mod suspend;
mod sys;
mod unlock;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: td-authd channel-check --peer-uid UID | \
    td-authd terminal-serve --primary --peer-uid UID | \
    td-authd terminal-serve --user USER --uid UID --peer-uid UID | \
    td-authd terminal-exec UID GENERATION HANDLE [PROGRAM [APP]] | td-authd prepare-portal-files | \
    td-authd release-portal-files | td-authd prepare-portal-handoff | \
    td-authd release-portal-handoff | td-authd prepare-application-files APP | \
    td-authd release-application-files APP | \
    td-authd application-start OWNER APP direct -- ARG... | \
     td-authd application-exec UID OWNER APP direct -- ARG... | \
     td-authd application-cache OWNER APP install|update|remove|delete-data|status | \
     td-authd request-update SOURCE DEPLOYMENT-ID | td-authd request-hostname NAME";

fn run(arguments: &[String]) -> Result<(), String> {
    if let [verb, action, value] = arguments {
        if verb == "time" {
            let change = consent::time_setting::Change::parse(action, value)?;
            return set_hostname::request(&format!("time {}", change.encode()?));
        }
    }
    if arguments == ["suspend-system"] {
        return suspend::run();
    }
    if let [verb, name] = arguments {
        if verb == "request-hostname" {
            return set_hostname::request(name);
        }
    }
    if let [verb, source, deployment] = arguments {
        if verb == "request-update" {
            return deployment::request(source, deployment);
        }
    }
    if arguments == ["prepare-portal-files"] {
        return portal_files::prepare();
    }
    if arguments == ["release-portal-files"] {
        return portal_files::release();
    }
    if arguments == ["prepare-portal-handoff"] {
        return portal_files::prepare_handoff();
    }
    if arguments == ["release-portal-handoff"] {
        return portal_files::release_handoff();
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
        if verb == "application-cache" {
            let output = application_cache::run(rest)?;
            let mut stdout = std::io::stdout().lock();
            return stdout
                .write_all(&output)
                .and_then(|()| stdout.flush())
                .map_err(|e| format!("write application cache answer: {e}"));
        }
        if verb == "notebook-ui" {
            return launch::notebook_ui(rest);
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

fn entry() -> Result<(), String> {
    let arguments = std::env::args_os()
        .skip(1)
        .map(|s| s.into_string().map_err(|_| "invalid argument encoding"))
        .collect::<Result<Vec<_>, _>>()?;
    run(&arguments)
}

fn main() -> ExitCode {
    match entry() {
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

#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;
