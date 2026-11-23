//! Fixed systemd and sudoers settings, including their pre-Rust file format.
use super::{Role, BASE, BIN, DB_GROUP, LEGACY, MAIL, NOTIFY, ROLES};
use crate::common::{child, fmt, require, Host};
use crate::R;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use td_alloc::{push, push_str};

const SYSTEMD: &str = "/etc/systemd/system";
const SUDOERS: &str = "/etc/sudoers.d/td-mta-readiness";
pub(crate) struct Setting {
    pub path: PathBuf,
    pub(crate) text: String,
    pub(crate) mode: u32,
}

pub(super) fn status_args(performance: bool) -> &'static [&'static str] {
    if performance {
        &["performance-status"]
    } else {
        &["status", "--json", "--require-ready"]
    }
}

fn dropin(units: &Path, unit: &str) -> R<PathBuf> {
    child(
        units,
        &fmt(format_args!("{unit}.d/95-td-mta-identity.conf"))?,
    )
}

// A service daemon reaches PostgreSQL's shared socket through its group; a
// readiness job never touches the database. `tls` is the old layout.
fn service_settings(role: &Role, daemon: bool, tls: bool) -> R<String> {
    let user = role.user;
    let group = if daemon && !tls { DB_GROUP } else { "" };
    let mut settings = fmt(format_args!("[Service]\nUser={user}\nGroup={user}\nSupplementaryGroups={group}\nUMask=0077\nNoNewPrivileges=yes\nAmbientCapabilities=\nCapabilityBoundingSet=\n"))?;
    if daemon && role.low_port {
        push_str(&mut settings,"CapabilityBoundingSet=CAP_NET_BIND_SERVICE\nAmbientCapabilities=CAP_NET_BIND_SERVICE\n")?;
    }
    push_str(&mut settings,"ProtectSystem=strict\nPrivateTmp=yes\nPrivateDevices=yes\nProtectHome=yes\nWorkingDirectory=/\nTemporaryFileSystem=/var/lib/td-mta:ro,mode=0755\nInaccessiblePaths=-/var/lib/td-mta-operator -/data/td-mta -/data/td-mta-physical-recovery\n")?;
    if daemon {
        push_str(
            &mut settings,
            &fmt(format_args!(
                "BindPaths={BASE}/{}:{MAIL}\nReadWritePaths={MAIL}\n",
                role.name
            ))?,
        )?;
    } else {
        push_str(
            &mut settings,
            &fmt(format_args!(
                "BindReadOnlyPaths={BASE}/{}:{MAIL}\n",
                role.name
            ))?,
        )?;
    }
    Ok(settings)
}

pub(super) fn settings(units: &Path, sudoers: &Path, tls: bool) -> R<Vec<Setting>> {
    let mut settings = Vec::new();
    let mut sudo = String::new();
    for role in ROLES {
        push(
            &mut settings,
            Setting {
                path: dropin(units, role.unit)?,
                text: service_settings(role, true, tls)?,
                mode: 0o644,
            },
        )?;
        if role.name == "acme" {
            continue;
        }
        let unit = fmt(format_args!("td-mta-maint-mail-{}.service", role.name))?;
        push(
            &mut settings,
            Setting {
                path: dropin(units, &unit)?,
                text: service_settings(role, false, tls)?,
                mode: 0o644,
            },
        )?;
        for performance in [false, true] {
            push_str(
                &mut sudo,
                &fmt(format_args!(
                    "{LEGACY} ALL=({}) NOPASSWD: NOSETENV: {BIN}",
                    role.user
                ))?,
            )?;
            for arg in status_args(performance) {
                push_str(&mut sudo, " ")?;
                push_str(&mut sudo, arg)?;
            }
            push_str(
                &mut sudo,
                &fmt(format_args!(" --runtime {}\n", role.runtime))?,
            )?;
        }
    }
    push(
        &mut settings,
        Setting {
            path: PathBuf::from(sudoers),
            text: sudo,
            mode: 0o440,
        },
    )?;
    for job in ["monitor", "digest"] {
        let unit = fmt(format_args!("td-mta-maint-notifications-{job}.service"))?;
        let text = fmt(format_args!("[Service]\nUser={LEGACY}\nGroup={LEGACY}\nNoNewPrivileges=no\nExecStart=\nExecStart=:/opt/td-mta/bin/td-mta-operator install-td-mta --notifications-{job} --config-file {NOTIFY}/isolated.json\n"))?;
        push(
            &mut settings,
            Setting {
                path: dropin(units, &unit)?,
                text,
                mode: 0o644,
            },
        )?;
    }
    Ok(settings)
}

pub(crate) fn production() -> R<Vec<Setting>> {
    settings(Path::new(SYSTEMD), Path::new(SUDOERS), false)
}

pub(crate) fn production_tls() -> R<Vec<Setting>> {
    settings(Path::new(SYSTEMD), Path::new(SUDOERS), true)
}

// Check from the root down so no lookup traverses an unchecked parent.
pub(crate) fn safe_parents(path: &Path) -> R<()> {
    if let Some(parent) = path.parent() {
        safe_parents(parent)?;
        let m = fs::symlink_metadata(parent)?;
        require(
            m.is_dir() && m.uid() == 0 && m.mode() & 0o022 == 0,
            "untrusted service isolation directory",
        )?;
    } else {
        require(path == Path::new("/"), "relative service isolation path")?;
    }
    Ok(())
}

pub(crate) fn trusted_binary(path: &Path) -> R<()> {
    let mut current = PathBuf::from(path);
    for _ in 0..40 {
        safe_parents(&current)?;
        let m = fs::symlink_metadata(&current)?;
        require(m.uid() == 0, "untrusted executable owner")?;
        if m.is_symlink() {
            let link = fs::read_link(&current)?;
            current = if link.is_absolute() {
                link
            } else {
                child(
                    current.parent().ok_or("missing binary parent")?,
                    link.to_str().ok_or("executable link encoding")?,
                )?
            };
        } else {
            return require(
                m.is_file() && m.mode() & 0o022 == 0 && m.mode() & 0o111 != 0,
                "untrusted production executable",
            );
        }
    }
    Err("production executable symlink depth".into())
}

pub(crate) fn present(file: &Setting) -> R<bool> {
    match safe_parents(&file.path) {
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        result => result?,
    }
    let (source, m) = match td_fs::open_real_file(&file.path, "system setting") {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        result => result?,
    };
    require(
        m.uid() == 0 && m.gid() == 0 && m.mode() & 0o7777 == file.mode && m.nlink() == 1,
        "isolation setting metadata drift",
    )?;
    let mut bytes = td_alloc::with_capacity(file.text.len() + 1)?;
    source
        .take((file.text.len() + 1) as u64)
        .read_to_end(&mut bytes)?;
    require(
        bytes == file.text.as_bytes(),
        "isolation setting content drift",
    )?;
    Ok(true)
}

pub(crate) fn installed(files: &[Setting]) -> R<bool> {
    let mut count = 0;
    for item in files {
        if present(item)? {
            count += 1;
        }
    }
    require(
        count == 0 || count == files.len(),
        "incomplete isolated service installation",
    )?;
    Ok(count == files.len())
}

// Both names are on the same root-owned filesystem. GNU mv uses a single
// no-replace rename here. A killed installer leaves either the staged file
// or a published file with exactly one link, which rollback can recognize.
pub(crate) fn publish(host: &Host, staged: &Path, target: &Path) -> R<()> {
    host.run(
        host.cmd("/usr/bin/mv")
            .args(["-nT", "--"])
            .arg(staged)
            .arg(target),
    )?;
    match fs::symlink_metadata(staged) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
        Ok(_) => Err("conflicting service isolation setting".into()),
    }
}

pub(crate) fn install(host: &Host, files: &[Setting]) -> R<()> {
    install_tracking(host, files, |_| Ok(()))
}

pub(crate) fn install_tracking<'a>(
    host: &Host,
    files: &'a [Setting],
    mut created: impl FnMut(&'a Setting) -> R<()>,
) -> R<()> {
    for file in files {
        if present(file)? {
            continue;
        }
        let parent = file.path.parent().ok_or("missing setting parent")?;
        safe_parents(parent)?;
        match DirBuilder::new().mode(0o755).create(parent) {
            Ok(()) => File::open(parent.parent().ok_or("missing parent")?)?.sync_all()?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        safe_parents(&file.path)?;
        let temp = td_fs::TempDir::new_in(parent, ".td-mta-identity")?;
        let staged = child(temp.path(), "setting")?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        out.write_all(file.text.as_bytes())?;
        out.set_permissions(fs::Permissions::from_mode(file.mode))?;
        out.sync_all()?;
        if file.mode == 0o440 {
            host.run(host.cmd("visudo").arg("-cf").arg(&staged))?;
        }
        let result = publish(host, &staged, &file.path);
        // mv can complete before its supervisor observes cancellation.
        if result.is_ok() || !staged.try_exists()? {
            created(file)?;
        }
        result?;
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// What a setting's path holds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Held {
    Absent,
    This,
    Other,
}

/// Whether `file`'s path holds nothing, exactly `file`, or anything else.
pub(crate) fn held(file: &Setting) -> R<Held> {
    match fs::symlink_metadata(&file.path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Held::Absent),
        Err(e) => return Err(e.into()),
        Ok(_) => (),
    }
    // Drift is an answer here, not a failure.
    Ok(if present(file).unwrap_or(false) {
        Held::This
    } else {
        Held::Other
    })
}

/// Replaces `old`, which must be in place, with `new` at the same path in
/// one rename, so the path always holds one of them.
pub(crate) fn replace(old: &Setting, new: &Setting) -> R<()> {
    require(
        old.path == new.path && present(old)?,
        "the setting to replace is not in place",
    )?;
    let parent = new.path.parent().ok_or("missing setting parent")?;
    safe_parents(&new.path)?;
    let temp = td_fs::TempDir::new_in(parent, ".td-mta-identity")?;
    let staged = child(temp.path(), "setting")?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    out.write_all(new.text.as_bytes())?;
    out.set_permissions(fs::Permissions::from_mode(new.mode))?;
    out.sync_all()?;
    fs::rename(&staged, &new.path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn rollback<'a>(files: impl IntoIterator<Item = &'a Setting>) -> R<()> {
    let mut managed = Vec::new();
    for file in files {
        if present(file)? {
            push(&mut managed, &file.path)?;
        }
    }
    for file in managed.into_iter().rev() {
        fs::remove_file(file)?;
        File::open(file.parent().ok_or("missing setting parent")?)?.sync_all()?;
    }
    Ok(())
}
