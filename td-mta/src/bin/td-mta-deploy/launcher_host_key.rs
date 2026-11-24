//! Enrolling the host's SSH key from a fingerprint the operator obtained
//! over another channel. The key the host offers must have exactly that
//! SHA-256 fingerprint; it goes, alone, into a known_hosts file beside
//! target.json that this tool owns and every SSH connection of the
//! launcher then uses, with ssh's own key learning off.
use super::*;
use std::{io::ErrorKind, os::unix::fs::MetadataExt};

/// The refusal whose remedy is the operator's decision.
pub(super) const KEY_DIFFERS: &str =
    "the enrolled host key is another: pass --replace to enroll the one with this fingerprint";

/// `SHA256:` and 43 unpadded base64 characters, as ssh-keygen prints it.
pub(super) fn fingerprint(value: &str) -> R<&str> {
    let body = value
        .strip_prefix("SHA256:")
        .ok_or("--host-key-sha256 takes SHA256:BASE64 as ssh-keygen -l prints it")?;
    require(
        body.len() == 43
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'),
        "--host-key-sha256 takes SHA256:BASE64 as ssh-keygen -l prints it",
    )?;
    Ok(value)
}

/// The known_hosts file of the target at `target`: its own name and
/// `.known_hosts`, as an absolute path ssh takes literally.
pub(super) fn known_hosts(target: &Path) -> R<PathBuf> {
    let file = std::path::absolute(known_hosts_name(target)?)?;
    // ssh expands `%`, `$` and `~` in the option, unescapes `\\` and
    // splits on white space.
    require(
        file.to_str().is_some_and(|p| {
            !p.contains(|c: char| c.is_whitespace() || "%$\"'~\\".contains(c))
        }),
        "the target's path must not contain white space, quotes, backslashes, %, $ or ~ to enroll its host key",
    )?;
    Ok(file)
}

/// Whether `file` is a private regular file itself, not through a link;
/// None if absent.
fn private_file(file: &Path) -> R<Option<bool>> {
    match fs::symlink_metadata(file) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(meta) => Ok(Some(meta.is_file() && meta.mode() & 0o077 == 0)),
    }
}

/// The ssh options that make it trust exactly the enrolled key, when one
/// is enrolled; otherwise ssh's own known_hosts as before.
pub(super) fn ssh_options(target: &Path) -> R<Vec<String>> {
    let mut options = Vec::new();
    let name = known_hosts_name(target)?;
    let Some(private) = private_file(&name)? else {
        return Ok(options);
    };
    require(
        fs::symlink_metadata(&name)?.is_file(),
        "the target's known_hosts path is not a regular file: remove it",
    )?;
    require(
        private,
        "the target's known_hosts file must be a regular file only its owner can read: run td-mta/deploy --write-target with --host-key-sha256 again",
    )?;
    let file = known_hosts(target)?;
    let path = file.to_str().ok_or("known_hosts path")?;
    for option in [
        fmt(format_args!("-oUserKnownHostsFile={path}"))?,
        owned("-oGlobalKnownHostsFile=/dev/null")?,
        owned("-oUpdateHostKeys=no")?,
        owned("-oCheckHostIP=no")?,
        owned("-oVerifyHostKeyDNS=no")?,
    ] {
        td_alloc::push(&mut options, option)?;
    }
    Ok(options)
}

/// The file's name, whether or not its path can be enrolled.
fn known_hosts_name(target: &Path) -> R<PathBuf> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("deploy target path")?;
    child(
        files::parent(target)?,
        &fmt(format_args!("{name}.known_hosts"))?,
    )
}

/// One key: its type and base64 blob.
#[derive(PartialEq, Debug)]
pub(super) struct Key<'a> {
    pub(super) kind: &'a str,
    pub(super) blob: &'a str,
}

/// The `HOST TYPE BLOB` lines ssh-keyscan prints, comments skipped.
pub(super) fn scanned<'a>(output: &'a str, host: &str) -> R<Vec<Key<'a>>> {
    let mut keys = Vec::new();
    for line in output.lines().filter(|l| !l.starts_with('#')) {
        let mut fields = line.split_ascii_whitespace();
        let (Some(name), Some(kind), Some(blob), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        require(name == host, "ssh-keyscan answered for another host")?;
        td_alloc::push(&mut keys, Key { kind, blob })?;
    }
    Ok(keys)
}

/// The line the file holds for `host` and `key`.
pub(super) fn line(host: &str, key: &Key<'_>) -> R<String> {
    fmt(format_args!("{host} {} {}\n", key.kind, key.blob))
}

/// What enrolling `wanted` does to a file that holds `existing`.
pub(super) fn decision(existing: Option<&str>, wanted: &str, replace: bool) -> R<&'static str> {
    match existing {
        None => Ok("host-key-enrolled"),
        Some(text) if text == wanted => Ok("host-key-unchanged"),
        Some(_) if replace => Ok("host-key-replaced"),
        Some(_) => Err(KEY_DIFFERS.into()),
    }
}

/// The enrolled line and what writing it does, decided before anything is
/// written.
pub(super) struct Enrollment {
    pub(super) line: String,
    pub(super) file: PathBuf,
    pub(super) result: &'static str,
}

impl Enrollment {
    pub(super) fn plan(host: &str, expected: &str, target: &Path, replace: bool) -> R<Self> {
        let local = Host::default();
        let scan = |host: &str| -> R<Vec<u8>> {
            Ok(local
                .exec(
                    Command::new("ssh-keyscan").args(["-T", "10", "--", host]),
                    &[],
                    Duration::from_secs(30),
                    false,
                )
                .map_err(|_| {
                    "ssh-keyscan did not finish: OpenSSH's client tools must be installed and the host name must resolve"
                })?
                .stdout)
        };
        let fingerprint = |line: &str| -> R<Vec<u8>> {
            Ok(local
                .exec(
                    Command::new("ssh-keygen").args(["-l", "-E", "sha256", "-f", "-"]),
                    line.as_bytes(),
                    Duration::from_secs(10),
                    true,
                )
                .map_err(|_| "ssh-keygen could not fingerprint a key the host offered")?
                .stdout)
        };
        Self::plan_with(host, expected, target, replace, scan, fingerprint)
    }

    /// `plan` with what ssh-keyscan prints for a host (`scan`) and what
    /// ssh-keygen prints for one known_hosts line (`fingerprint`).
    pub(super) fn plan_with(
        host: &str,
        expected: &str,
        target: &Path,
        replace: bool,
        scan: impl FnOnce(&str) -> R<Vec<u8>>,
        fingerprint: impl Fn(&str) -> R<Vec<u8>>,
    ) -> R<Self> {
        // ssh names hosts in lowercase in what it prints and looks up.
        require(
            !host.bytes().any(|b| b.is_ascii_uppercase()),
            "--host must be lowercase to enroll its key",
        )?;
        // Before anything is written: the path later connections can use.
        let file = known_hosts(target)?;
        let scan = scan(host)?;
        let keys = scanned(std::str::from_utf8(&scan)?, host)?;
        require(
            !keys.is_empty(),
            "the host offered no SSH key: it must be reachable on SSH's port",
        )?;
        let mut chosen = None;
        for key in keys {
            let candidate = line(host, &key)?;
            let printed = fingerprint(&candidate)?;
            let printed = std::str::from_utf8(&printed)?;
            if printed.split_ascii_whitespace().nth(1) == Some(expected) {
                chosen = Some(candidate);
                break;
            }
        }
        let wanted = chosen.ok_or(
            "none of the keys the host offers has that fingerprint: check the fingerprint and that this is the host",
        )?;
        let private = private_file(&file)?;
        require(
            private != Some(false) || fs::symlink_metadata(&file)?.is_file(),
            "the target's known_hosts path is not a regular file: remove it",
        )?;
        // An unreadable or damaged file is one that says something else.
        let existing = private.map(|_| {
            td_fs::read_bounded(&file, 64 * 1024)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default()
        });
        let mut result = decision(existing.as_deref(), &wanted, replace)?;
        // The right key in a file others can read is rewritten privately.
        if result == "host-key-unchanged" && private == Some(false) {
            result = "host-key-repaired";
        }
        Ok(Self {
            line: wanted,
            file,
            result,
        })
    }

    pub(super) fn apply(&self) -> R<()> {
        if self.result == "host-key-unchanged" {
            return Ok(());
        }
        let directory = files::parent(&self.file)?;
        files::publish(
            directory,
            &self.file,
            &self.line,
            self.result != "host-key-enrolled",
        )
        .map_err(|_| {
            "target.json is written but the host key could not be: run the same command again"
        })?;
        Ok(())
    }
}
