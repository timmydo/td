//! Permanent production readiness jobs shared by both service identity layouts.
use crate::common::{child, fmt, owned, require, Host};
use crate::isolation::files::{self, Setting};
use crate::R;
use std::path::Path;
use td_json::Json;

const HEALTH: &str = "td-mta-health.service";
const HEALTH_CHECKS: &str = include_str!("units/zz-readiness-jobs.conf");
// The aggregate while PostgreSQL used loopback TLS: only the private socket
// cutover expects and replaces it.
const HEALTH_CHECKS_TLS: &str = include_str!("units/zz-readiness-jobs.tls.conf");
const UNIT_DIR: &str = "/etc/systemd/system";
const BUS_PATH: &str = "/org/freedesktop/systemd1/unit/td_2dmta_2dhealth_2eservice";

fn checks(tls: bool) -> &'static str {
    if tls {
        HEALTH_CHECKS_TLS
    } else {
        HEALTH_CHECKS
    }
}

pub(crate) fn settings(dir: &Path, tls: bool) -> R<Vec<Setting>> {
    let mut list = Vec::new();
    for role in crate::activate::PROFILES {
        let name = fmt(format_args!("td-mta-maint-mail-{role}"))?;
        let text = fmt(format_args!(
            include_str!("units/readiness.service.in"),
            role
        ))?;
        td_alloc::push(
            &mut list,
            Setting {
                path: child(dir, &fmt(format_args!("{name}.service"))?)?,
                text,
                mode: 0o644,
            },
        )?;
    }
    td_alloc::push(
        &mut list,
        Setting {
            path: child(dir, "td-mta-health.service.d/zz-readiness-jobs.conf")?,
            text: owned(checks(tls))?,
            mode: 0o644,
        },
    )?;
    Ok(list)
}

fn socket_role(argv: &[Json]) -> Option<&str> {
    let (runtime, command) = argv.split_last()?;
    let args = [
        "/opt/td-mta/bin/td-mta",
        "status",
        "--json",
        "--require-ready",
        "--runtime",
    ];
    if !command
        .iter()
        .map(Json::as_str)
        .eq(args.into_iter().map(Some))
    {
        return None;
    }
    runtime
        .as_str()?
        .strip_prefix("/var/lib/td-mta/mail/")?
        .strip_suffix("/runtime")
}

fn arguments<'a>(row: &'a Json, expected_flags: &[&str]) -> R<&'a [Json]> {
    let [path, argv, flags, ..] = row.as_arr().ok_or("invalid ExecStartEx row")? else {
        return Err("short ExecStartEx row".into());
    };
    let argv = argv.as_arr().ok_or("invalid ExecStartEx arguments")?;
    require(
        flags.as_arr().is_some_and(|flags| {
            flags
                .iter()
                .map(Json::as_str)
                .eq(expected_flags.iter().copied().map(Some))
        }) && path.as_str().is_some()
            && argv.first() == Some(path),
        "unexpected health command flags or executable",
    )?;
    Ok(argv)
}

// ExecStart= resets the whole list. Compare systemd's parsed argv and flags
// before publishing it so a host-local check cannot silently disappear.
fn check_commands(property: &Json, allow_old_sockets: bool, expected: &str) -> R<()> {
    let rows = property
        .get("data")
        .and_then(Json::as_arr)
        .ok_or("missing ExecStartEx")?;
    let mut retained = expected
        .lines()
        .filter_map(|l| l.strip_prefix("ExecStart="))
        .filter(|l| !l.is_empty());
    for row in rows {
        let argv = arguments(row, &[])?;
        if allow_old_sockets
            && socket_role(argv).is_some_and(|r| crate::activate::PROFILES.contains(&r))
        {
            continue;
        }
        require(
            retained.next().is_some_and(|line| {
                argv.iter()
                    .map(Json::as_str)
                    .eq(line.split_ascii_whitespace().map(Some))
            }),
            "production aggregate health command differs from the managed checks",
        )?;
    }
    require(
        retained.next().is_none(),
        "missing production aggregate health check",
    )
}

fn command_property(host: &Host, unit: &str, path: &str) -> R<Json> {
    let props = host.out(host.cmd("systemctl").args([
        "show",
        "-p",
        "LoadState",
        "-p",
        "Type",
        "-p",
        "RemainAfterExit",
        unit,
    ]))?;
    require(
        ["LoadState=loaded", "Type=oneshot", "RemainAfterExit=no"]
            .iter()
            .all(|p| props.lines().any(|l| l == *p)),
        "health unit must run a fresh oneshot at each invocation",
    )?;
    exec_start(host, path)
}

/// systemd's parsed ExecStart= list of the unit at bus object `path`.
pub(crate) fn exec_start(host: &Host, path: &str) -> R<Json> {
    let reply = host.out(host.cmd("busctl").args([
        "--json=short",
        "get-property",
        "org.freedesktop.systemd1",
        path,
        "org.freedesktop.systemd1.Service",
        "ExecStartEx",
    ]))?;
    Ok(td_json::parse(&reply)?)
}

fn check_job(property: &Json, role: &str) -> R<()> {
    let Some([row]) = property.get("data").and_then(Json::as_arr) else {
        return Err("readiness job must run exactly one command".into());
    };
    require(
        socket_role(arguments(row, &["no-env-expand"])?) == Some(role),
        "readiness job must check its own runtime",
    )
}

fn qualify(host: &Host, tls: bool) -> R<()> {
    check_commands(
        &command_property(host, HEALTH, BUS_PATH)?,
        false,
        checks(tls),
    )?;
    for role in crate::activate::PROFILES {
        let unit = fmt(format_args!("td-mta-maint-mail-{role}.service"))?;
        let path = fmt(format_args!(
            "/org/freedesktop/systemd1/unit/td_2dmta_2dmaint_2dmail_2d{role}_2eservice"
        ))?;
        check_job(&command_property(host, &unit, &path)?, role)?;
    }
    for property in ["Requires", "After"] {
        let names = host.out(host.cmd("systemctl").args(["show", "-P", property, HEALTH]))?;
        for role in crate::activate::PROFILES {
            let unit = fmt(format_args!("td-mta-maint-mail-{role}.service"))?;
            require(
                names.split_ascii_whitespace().any(|v| v == unit),
                "missing aggregate readiness dependency",
            )?;
        }
    }
    Ok(())
}

/// The production aggregate health drop-in of one transport layout.
pub(crate) fn aggregate(tls: bool) -> R<Setting> {
    settings(Path::new(UNIT_DIR), tls)?
        .pop()
        .ok_or_else(|| "missing aggregate health setting".into())
}

/// What `prepare` requires, changing nothing: true when every setting is
/// in place and the aggregate qualifies, false when `prepare` would
/// install the missing ones over the commands it replaces.
pub(crate) fn check(host: &Host, tls: bool) -> R<bool> {
    let mut complete = true;
    for file in settings(Path::new(UNIT_DIR), tls)? {
        complete &= files::present(&file)?;
    }
    if complete {
        qualify(host, tls)?;
    } else {
        check_commands(
            &command_property(host, HEALTH, BUS_PATH)?,
            true,
            checks(tls),
        )?;
    }
    Ok(complete)
}

pub(crate) fn prepare(host: &Host, tls: bool) -> R<()> {
    let mut missing = Vec::new();
    for file in settings(Path::new(UNIT_DIR), tls)? {
        if !files::present(&file)? {
            td_alloc::push(&mut missing, file)?;
        }
    }
    host.run(host.cmd("systemctl").arg("daemon-reload"))?;
    if missing.is_empty() {
        return qualify(host, tls);
    }
    check_commands(
        &command_property(host, HEALTH, BUS_PATH)?,
        true,
        checks(tls),
    )?;
    let mut created = td_alloc::with_capacity(missing.len())?;
    let result = files::install_tracking(host, &missing, |file| {
        Ok(td_alloc::push(&mut created, file)?)
    })
    .and_then(|()| host.run(host.cmd("systemctl").arg("daemon-reload")))
    .and_then(|()| qualify(host, tls));
    if result.is_err() {
        let recovery = Host::default();
        recovery.recover();
        files::rollback(created)?;
        recovery.run(recovery.cmd("systemctl").arg("daemon-reload"))?;
    }
    result
}

#[cfg(test)]
#[path = "tests/readiness.rs"]
mod tests;
