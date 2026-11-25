//! The systemd journal's size bound on the production host: one journald
//! drop-in td-mta owns, beside the distribution's own journald.conf.
use crate::common::{fmt, owned, require, Host};
use crate::isolation::files::{self, Held, Setting};
use crate::R;
use std::{
    fs::{self, DirBuilder, File},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(crate) const DROPIN: &str = "/etc/systemd/journald.conf.d/td-mta.conf";
const DIRECTORY: &str = "/etc/systemd/journald.conf.d";
const TEXT: &str = "# Written by td-mta-deploy (td-mta/PRODUCTION.md, \"Journal size\").\n\
[Journal]\nSystemMaxUse=500M\nSystemKeepFree=4G\n";
/// What the drop-in sets, which no file journald reads later may undo.
const KEYS: &[(&str, &str)] = &[("SystemMaxUse", "500M"), ("SystemKeepFree", "4G")];
const JOURNALD: &str = "systemd-journald.service";
/// The refusal of a file at the drop-in's path that is not the drop-in.
pub(crate) const FOREIGN: &str = "/etc/systemd/journald.conf.d/td-mta.conf is not the journal bound td-mta-deploy writes (root:root 0644, with the text td-mta/PRODUCTION.md, \"Journal size\", gives): remove it or make it that file";

/// What an activation did about the journal's bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cap {
    /// The drop-in was placed and journald restarted under it.
    Installed,
    /// The drop-in was in place but journald may have started before it.
    Restarted,
    /// The drop-in was in place and journald started after it.
    Unchanged,
}

impl Cap {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Cap::Installed => "installed",
            Cap::Restarted => "restarted",
            Cap::Unchanged => "unchanged",
        }
    }
}

pub(crate) fn setting() -> R<Setting> {
    Ok(Setting {
        path: PathBuf::from(DROPIN),
        text: owned(TEXT)?,
        mode: 0o644,
    })
}

/// The drop-in exactly as td-mta writes it, and journald running under it.
/// An existing file is adopted only when it is exactly that file; any other
/// refuses, naming it.
pub(crate) fn ensure(host: &Host) -> R<Cap> {
    directory().map_err(|e| fmt(format_args!("{DIRECTORY}: {e}")).map_or(e, Into::into))?;
    let setting = setting()?;
    match files::held(&setting)? {
        Held::This => {
            check(host)?;
            let written = fs::symlink_metadata(DROPIN)?.mtime();
            // A start time systemd cannot print as Unix seconds is not
            // known to follow the file: restart rather than refuse.
            if started(host).is_ok_and(|started| started >= written) {
                return Ok(Cap::Unchanged);
            }
            restart(host)?;
            Ok(Cap::Restarted)
        }
        Held::Other => Err(FOREIGN.into()),
        Held::Absent => {
            files::install(host, std::slice::from_ref(&setting))?;
            // Neither a later journald file overriding it nor journald
            // failing under it leaves the file behind.
            let mut restarted = false;
            let result = check(host).and_then(|()| {
                restarted = true;
                restart(host)
            });
            if let Err(error) = result {
                let recovery = Host::default();
                recovery.recover();
                let undone = match files::rollback(std::slice::from_ref(&setting)) {
                    Err(e) => Some(fmt(format_args!("removing {DROPIN} again failed: {e}"))?),
                    // journald can have read the file only if it restarted.
                    Ok(()) if restarted => match restart(&recovery) {
                        Err(e) => Some(fmt(format_args!(
                            "restarting journald without {DROPIN} failed: {e}"
                        ))?),
                        Ok(()) => None,
                    },
                    Ok(()) => None,
                };
                return Err(match undone {
                    None => error,
                    Some(cleanup) => fmt(format_args!("{error}; and {cleanup}"))?.into(),
                });
            }
            Ok(Cap::Installed)
        }
    }
}

/// The drop-in directory, root's and readable by all whatever the
/// controller's umask, so an unprivileged `systemd-analyze cat-config`
/// sees it too.
fn directory() -> R<()> {
    let path = Path::new(DIRECTORY);
    files::safe_parents(path)?;
    match DirBuilder::new().mode(0o755).create(path) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            File::open(path.parent().ok_or("missing parent")?)?.sync_all()?;
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    require(
        meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o022 == 0,
        "not a root-owned directory only root can write",
    )
}

fn restart(host: &Host) -> R<()> {
    host.run(host.cmd("systemctl").args(["restart", JOURNALD]))?;
    require(
        host.out(
            host.cmd("systemctl")
                .args(["show", "-P", "ActiveState", JOURNALD]),
        )? == "active",
        "systemd-journald did not return after its restart",
    )
}

/// When journald last started, in Unix seconds.
fn started(host: &Host) -> R<i64> {
    unix_time(&host.out(host.cmd("systemctl").args([
        "show",
        "--timestamp=unix",
        "-P",
        "ActiveEnterTimestamp",
        JOURNALD,
    ]))?)
}

/// `@SECONDS`, as `systemctl show --timestamp=unix` prints a time.
pub(crate) fn unix_time(text: &str) -> R<i64> {
    let digits = text.trim().strip_prefix('@').unwrap_or_default();
    require(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "systemd-journald reports no start time",
    )?;
    Ok(digits.parse()?)
}

fn check(host: &Host) -> R<()> {
    effective(
        &host.out(
            host.cmd("systemd-analyze")
                .args(["cat-config", "systemd/journald.conf"]),
        )?,
    )
}

/// The file a `systemd-analyze cat-config` header line names: `# PATH`,
/// an absolute path to a `.conf` file with no white space, which a
/// comment inside a file is not.
fn header(line: &str) -> Option<&str> {
    let path = line.strip_prefix("# ")?;
    (path.starts_with('/')
        && path.ends_with(".conf")
        && !path.bytes().any(|b| b.is_ascii_whitespace()))
    .then_some(path)
}

/// The last `[Journal]` assignment of `key` in `text`, and the file that
/// makes it: `systemd-analyze cat-config` prints each file journald reads,
/// in the order it reads them, after its header line.
fn last<'a>(text: &'a str, key: &str) -> Option<(&'a str, &'a str)> {
    let mut file = "";
    let mut journal = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(path) = header(line) {
            // Each file starts outside any section.
            file = path;
            journal = false;
        } else if line.starts_with('[') {
            journal = line == "[Journal]";
        } else if journal && !line.starts_with('#') && !line.starts_with(';') {
            if let Some((name, value)) = line.split_once('=') {
                if name.trim() == key {
                    found = Some((value.trim(), file));
                }
            }
        }
    }
    found
}

/// Whether the drop-in's settings are journald's last word in `text`, the
/// output of `systemd-analyze cat-config systemd/journald.conf`.
pub(crate) fn effective(text: &str) -> R<()> {
    for (key, wanted) in KEYS {
        match last(text, key) {
            Some((value, _)) if value == *wanted => (),
            Some((_, file)) => {
                return Err(fmt(format_args!(
                    "{file} sets journald's {key}= after {DROPIN}: inspect sudo systemd-analyze cat-config systemd/journald.conf"
                ))?
                .into())
            }
            None => {
                return Err(fmt(format_args!(
                    "journald does not read {DROPIN}: inspect sudo systemd-analyze cat-config systemd/journald.conf"
                ))?
                .into())
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/journald.rs"]
mod tests;
