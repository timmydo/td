//! The operator's two configuration files, written by command from explicit
//! values: target.json on this machine, host.json on the host over SSH.
//! Each file is checked by the parser that later reads it before it is
//! written, and read back after. Every check that can refuse runs before
//! anything is written.
use super::*;
use crate::{
    common::random_hex,
    target::{target_path, HOST_FLAGS, TARGET_FLAGS},
};
use std::{
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
};

const CANDIDATES: &str = ".local/state/td-mta-candidates";
const OPERATOR_DIR: &str = "/var/lib/td-mta-operator";
/// The one refusal whose remedy is the operator's decision, not a fix.
pub(super) const DIFFERS: &str =
    "the file exists and says something else: pass --replace to overwrite it";
const NO_SUDO: &str = "sudo -n failed on the host: target.json's account needs passwordless sudo";

/// The values of one command's flags, keyed by the file's field names.
pub(super) struct Values<'a> {
    pub(super) fields: Vec<(&'static str, &'a str)>,
    pub(super) config: Option<&'a str>,
    pub(super) replace: bool,
    pub(super) host_key: Option<&'a str>,
}

/// Exactly one `FLAG VALUE` for each of `flags`, an optional `--config
/// FILE` and the `--replace` switch. No value starts with `--`.
pub(super) fn values<'a>(
    args: &'a [String],
    flags: &[(&str, &'static str)],
    required: &str,
) -> R<Values<'a>> {
    let mut out = Values {
        fields: Vec::new(),
        config: None,
        replace: false,
        host_key: None,
    };
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'a, String>| -> R<&'a str> {
        let value = it.next().ok_or("an option needs a value")?;
        require(!value.starts_with("--"), "an option needs a value")?;
        Ok(value.as_str())
    };
    while let Some(arg) = it.next() {
        if arg == "--replace" {
            require(!out.replace, "--replace is repeated")?;
            out.replace = true;
        } else if arg == "--config" {
            require(out.config.is_none(), "--config is repeated")?;
            out.config = Some(value(&mut it)?);
        } else if arg == "--host-key-sha256" {
            require(out.host_key.is_none(), "--host-key-sha256 is repeated")?;
            out.host_key = Some(host_key::fingerprint(value(&mut it)?)?);
        } else {
            let (_, key) = flags
                .iter()
                .find(|(flag, _)| flag == arg)
                .ok_or("unknown option: see td-mta/deploy --help")?;
            require(
                !out.fields.iter().any(|(k, _)| k == key),
                "a configuration flag is repeated",
            )?;
            td_alloc::push(&mut out.fields, (*key, value(&mut it)?))?;
        }
    }
    require(out.fields.len() == flags.len(), required)?;
    Ok(out)
}

/// Run a file command, ending with its outcome line whether it wrote,
/// found nothing to do or refused.
pub(super) fn reported(command: impl FnOnce() -> R<&'static str>) -> R<()> {
    match command() {
        Ok(result) => outcome(&Outcome {
            result,
            host_key: None,
            error: None,
            details: None,
            next: None,
        }),
        Err(error) => {
            let message = fmt(format_args!("{error}"))?;
            outcome(&Outcome {
                result: if message == DIFFERS || message == host_key::KEY_DIFFERS {
                    "refused-differs"
                } else {
                    "refused"
                },
                host_key: None,
                error: Some(&message),
                details: None,
                next: None,
            })?;
            Err(error)
        }
    }
}

/// `--write-target`: target.json from its six values.
pub(super) fn write_target(args: &[String]) -> R<()> {
    let mut config = None;
    let mut enrolled = None;
    let result = (|| -> R<&'static str> {
        let v = values(
            args,
            TARGET_FLAGS,
            "--write-target needs --host, --host-name, --account, --identity, --jmap-account and --jmap-url",
        )?;
        config = v.config;
        let home = operator_home()?;
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let path = target_path(v.config.map(Path::new), &home, xdg.as_deref())?;
        let (text, target) = Target::render(&v.fields, &home)?;
        private_key(&target.identity)?;
        // Decided before anything is written, applied after target.json.
        let plan = match v.host_key {
            Some(expected) => Some(host_key::Enrollment::plan(
                target.host().ok_or("deploy target: no host")?,
                expected,
                &path,
                v.replace,
            )?),
            None => None,
        };
        let stored = store_target(&path, &text, &target, &home, v.replace, v.config.is_none())?;
        if let Some(plan) = plan {
            plan.apply()?;
            enrolled = Some(plan.result);
        }
        Ok(stored)
    })();
    let mut argv = Vec::new();
    td_alloc::extend_from_slice(&mut argv, &["td-mta/deploy", "--provision-host"])?;
    if let Some(config) = config {
        td_alloc::extend_from_slice(&mut argv, &["--config", config])?;
    }
    let next = Next {
        argv: &argv,
        needs: &["--build-account", "--build-home", "--public-name"],
    };
    match result {
        Ok(result) => outcome(&Outcome {
            result,
            host_key: enrolled,
            error: None,
            details: None,
            next: Some(next),
        }),
        Err(_) => reported(|| result),
    }
}

/// ssh reads only a private key file, and `-i` names a file.
pub(super) fn private_key(path: &Path) -> R<()> {
    let meta = fs::metadata(path).map_err(|_| "deploy target: no identity key file there")?;
    require(
        meta.is_file() && meta.mode() & 0o077 == 0,
        "deploy target: the identity key must be a regular file only its owner can read",
    )
}

/// Write `text` to `path` unless it already says `target` in a private
/// file; replace a file that says something else only if `replace`.
/// `create` makes the default configuration directory, private, if it is
/// missing.
pub(super) fn store_target(
    path: &Path,
    text: &str,
    target: &Target,
    home: &Path,
    replace: bool,
    create: bool,
) -> R<&'static str> {
    let directory = parent(path)?;
    let result = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if create {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(directory)?;
            }
            publish(directory, path, text, false)?;
            "target-written"
        }
        Err(e) => return Err(e.into()),
        Ok(_) => {
            let same = Target::load(Some(path), home, None).is_ok_and(|old| old == *target);
            // A link to a private file with these values stays as it is.
            let private = fs::metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o077 == 0);
            if same && private {
                "target-unchanged"
            } else {
                require(same || replace, DIFFERS)?;
                publish(directory, path, text, true)?;
                if same {
                    "target-repaired"
                } else {
                    "target-replaced"
                }
            }
        }
    };
    require(
        Target::load(Some(path), home, None)? == *target,
        "target.json does not read back as written",
    )?;
    Ok(result)
}

pub(super) fn parent(path: &Path) -> R<&Path> {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(p) => Ok(p),
        None => Err("the target configuration file needs a directory".into()),
    }
}

/// A private file under a fresh name, synced, then moved to `path`: over
/// what is there if `replace`, else only where nothing is.
pub(super) fn publish(directory: &Path, path: &Path, text: &str, replace: bool) -> R<()> {
    let staged = child(directory, &fmt(format_args!(".target-{}", random_hex(8)?))?)?;
    let result = (|| -> R<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if replace {
            fs::rename(&staged, path)?;
        } else {
            fs::hard_link(&staged, path)?;
        }
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    match fs::remove_file(&staged) {
        Err(e) if e.kind() != ErrorKind::NotFound && result.is_ok() => Err(e.into()),
        _ => result,
    }
}

/// `--provision-host`: the build account's directories and the host's
/// host.json, from three values and target.json's host_name.
pub(super) fn provision_host(args: &[String]) -> R<()> {
    reported(|| {
        let v = values(
            args,
            HOST_FLAGS,
            "--provision-host needs --build-account, --build-home and --public-name",
        )?;
        require(
            v.host_key.is_none(),
            "--host-key-sha256 belongs to --write-target",
        )?;
        let (local, target, _) = session(v.config)?;
        let mut fields = v.fields;
        td_alloc::push(&mut fields, ("host_name", target.host_name.as_str()))?;
        let (text, config) = HostConfig::render(&fields)?;
        require(
            target.account() == Some(config.build_account.as_str()),
            "--build-account must be target.json's account: the launcher pushes into the build account's mirror",
        )?;
        local.login()?;
        // Everything that can refuse, before anything changes.
        require(
            local.remote_status(&["sudo", "-n", "true"])? == Some(0),
            NO_SUDO,
        )?;
        require(
            local.remote(&["cat", "/proc/sys/kernel/hostname"])? == config.host_name,
            "the host's kernel host name is not target.json's host_name",
        )?;
        let entry = local.remote(&["getent", "passwd", "--", &config.build_account])?;
        passwd_home(&entry, &config.build_home)?;
        let home = Path::new(&config.build_home);
        let cargo = child(home, ".cargo/bin/cargo")?;
        require(
            local.remote_status(&["test", "-x", cargo.to_str().ok_or("build home")?])? == Some(0),
            "the build account has no Cargo in .cargo/bin under its home",
        )?;
        let candidates = child(home, CANDIDATES)?;
        let candidates = candidates.to_str().ok_or("candidate directory")?;
        require(
            local.remote(&["readlink", "-m", "--", candidates])? == candidates,
            "the build account's candidate directory path has a symbolic link",
        )?;
        let mirror = config.mirror()?;
        let mirror = mirror.to_str().ok_or("source mirror")?;
        let mirror_exists =
            exit_flag(local.remote_status(&["test", "-e", mirror, "-o", "-L", mirror])?)?;
        if mirror_exists {
            local.mirror_repository(mirror)?;
        }
        let operator_exists = exit_flag(local.remote_status(&[
            "sudo",
            "-n",
            "test",
            "-e",
            OPERATOR_DIR,
            "-o",
            "-L",
            OPERATOR_DIR,
        ])?)?;
        if operator_exists {
            require(
                local.sudo(&["stat", "-c", "%u:%F", "--", OPERATOR_DIR])? == "0:directory",
                "/var/lib/td-mta-operator must be a root-owned directory",
            )?;
        }
        let existing = if exit_flag(local.remote_status(&[
            "sudo",
            "-n",
            "test",
            "-e",
            HOST_CONFIG,
            "-o",
            "-L",
            HOST_CONFIG,
        ])?)? {
            let old = local.sudo(&["cat", "--", HOST_CONFIG]);
            Some(Existing {
                same: old.is_ok_and(|old| {
                    HostConfig::parse(old.as_bytes()).is_ok_and(|old| old == config)
                }),
                private: local.sudo(&["stat", "-c", "%u:%a:%F", "--", HOST_CONFIG])?
                    == "0:600:regular file",
            })
        } else {
            None
        };
        let result = host_decision(existing.as_ref(), v.replace)?;

        local.remote(&["install", "-d", "-m", "0700", "--", candidates])?;
        require(
            local.remote(&["readlink", "-e", "--", candidates])? == candidates,
            "the build account's candidate directory path has a symbolic link",
        )?;
        if !mirror_exists {
            local.remote(&["git", "init", "--quiet", "--", mirror])?;
            local.mirror_repository(mirror)?;
        }
        if !operator_exists {
            local.sudo(&[
                "install",
                "-d",
                "-o",
                "root",
                "-g",
                "root",
                "-m",
                "0700",
                OPERATOR_DIR,
            ])?;
        }
        if result != "host-unchanged" {
            local.publish_host_config(&text, existing.is_some())?;
        }
        require(
            HostConfig::parse(local.sudo(&["cat", "--", HOST_CONFIG])?.as_bytes())? == config
                && local.sudo(&["stat", "-c", "%u:%a:%F", "--", HOST_CONFIG])?
                    == "0:600:regular file",
            "host.json does not read back as written",
        )?;
        Ok(result)
    })
}

/// What a `test -e` exit status says, once sudo is known to work.
fn exit_flag(status: Option<i32>) -> R<bool> {
    match status {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err("cannot inspect a path on the host".into()),
    }
}

/// What host.json on the host says now.
pub(super) struct Existing {
    /// It says the values given.
    pub(super) same: bool,
    /// Root's 0600 regular file.
    pub(super) private: bool,
}

/// The result of provisioning over `existing`, or the refusal.
pub(super) fn host_decision(existing: Option<&Existing>, replace: bool) -> R<&'static str> {
    Ok(match existing {
        None => "host-provisioned",
        Some(Existing {
            same: true,
            private: true,
        }) => "host-unchanged",
        Some(Existing { same: true, .. }) => "host-repaired",
        Some(_) => {
            require(replace, DIFFERS)?;
            "host-replaced"
        }
    })
}

/// A `getent passwd` entry of an unprivileged account whose home is `home`.
pub(super) fn passwd_home(entry: &str, home: &str) -> R<()> {
    // name:password:uid:gid:gecos:home:shell
    let mut fields = entry.split(':');
    let uid = fields.nth(2);
    let directory = fields.nth(2);
    let (Some(uid), Some(directory), Some(_), None) =
        (uid, directory, fields.next(), fields.next())
    else {
        return Err("unexpected account entry on the host".into());
    };
    require(
        uid.parse::<u32>().is_ok_and(|uid| uid != 0),
        "--build-account must be an unprivileged account on the host",
    )?;
    require(
        directory == home,
        "--build-home must be the build account's home directory",
    )
}

impl Local {
    /// The SSH login works: the host key is known, the key is authorized
    /// and the host answers.
    pub(super) fn login(&self) -> R<()> {
        require(
            self.remote_status(&["true"])? == Some(0),
            "cannot log in over SSH as target.json's account with its key: the host must be reachable, its key enrolled (--write-target --host-key-sha256) and the identity key authorized",
        )
    }

    /// `mirror` is the top of its own Git work tree.
    fn mirror_repository(&self, mirror: &str) -> R<()> {
        let top = self.host.exec(
            &mut self.ssh(&["git", "-C", mirror, "rev-parse", "--show-toplevel"])?,
            &[],
            Duration::from_secs(40),
            false,
        )?;
        require(
            top.status.success() && std::str::from_utf8(&top.stdout)?.trim() == mirror,
            "src/td under the build home exists and is not its own Git repository",
        )
    }

    /// Stage under a fresh name, then move into place; only where nothing
    /// is unless `replace`.
    fn publish_host_config(&self, text: &str, replace: bool) -> R<()> {
        let staged = fmt(format_args!("{HOST_CONFIG}.{}", random_hex(8)?))?;
        self.host.exec(
            &mut self.ssh(&["sudo", "-n", "install", "-m", "0600", "/dev/stdin", &staged])?,
            text.as_bytes(),
            Duration::from_secs(30),
            true,
        )?;
        if replace {
            let moved = self.sudo(&["mv", "-T", "--", &staged, HOST_CONFIG]);
            if moved.is_err() {
                self.sudo(&["rm", "-f", "--", &staged])?;
            }
            moved.map_err(|_| "cannot move host.json into place on the host")?;
        } else {
            let link = self.sudo(&["ln", "-T", "--", &staged, HOST_CONFIG]);
            self.sudo(&["rm", "--", &staged])?;
            link.map_err(|_| "host.json appeared on the host while it was being provisioned")?;
        }
        self.sudo(&["sync", "-f", HOST_CONFIG])?;
        Ok(())
    }
}
