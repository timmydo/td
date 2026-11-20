//! Isolated, disk-backed PostgreSQL verification of a scheduled backup.
use super::restore_layout;
use crate::sandbox::{self, Bind};
use std::error::Error;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use td_engine::allocation;

#[path = "install_mta_backup/restore_worker.rs"]
mod database;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const GIB: u64 = 1 << 30;
const JOB_PREFIX: &str = ".td-mta-restore-v1-";
const CLAIM: &str = ".td-mta-restore-active-v1";
const DEADLINE: Duration = Duration::from_secs(3600);

pub(super) struct Verified {
    pub(super) accounts: u64,
    pub(super) mailboxes: u64,
    pub(super) emails: u64,
    pub(super) bodies: u64,
    pub(super) body_bytes: u64,
}

impl Verified {
    fn parse(text: &str) -> Result<Self> {
        let mut fields = text.trim_end().split('|');
        let mut count =
            || -> Result<u64> { Ok(fields.next().ok_or("missing restore count")?.parse()?) };
        let result = Self {
            accounts: count()?,
            mailboxes: count()?,
            emails: count()?,
            bodies: count()?,
            body_bytes: count()?,
        };
        if fields.next().is_some() {
            return Err("invalid restore counts".into());
        }
        Ok(result)
    }
}

fn subpath(base: &Path, name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(allocation::format(format_args!(
        "{}/{name}",
        base.to_str().ok_or("invalid scratch path")?
    ))?))
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

fn new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

// The claim is published in one symlink(2) before the new directory exists.
// It remains outside the payload until recursive removal succeeds, including
// after a crash partway through that removal. Never follow the link.
fn claimed_job(claim: &Path) -> Result<Option<PathBuf>> {
    let meta = match fs::symlink_metadata(claim) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_symlink()
        || meta.nlink() != 1
        || meta.uid() != crate::sys::getuid()
        || meta.len() != (JOB_PREFIX.len() + 32) as u64
    {
        return Err("invalid restore scratch claim".into());
    }
    let name = fs::read_link(claim)?;
    let name = name.to_str().ok_or("invalid restore scratch claim")?;
    if !name.strip_prefix(JOB_PREFIX).is_some_and(|n| {
        n.len() == 32
            && n.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err("invalid restore scratch claim".into());
    }
    Ok(Some(subpath(
        claim.parent().ok_or("invalid restore claim location")?,
        name,
    )?))
}

struct Scratch {
    path: PathBuf,
    claim: PathBuf,
}
impl Scratch {
    fn clean(&self) -> Result<()> {
        match fs::symlink_metadata(&self.path) {
            Ok(meta)
                if meta.is_dir()
                    && meta.uid() == crate::sys::getuid()
                    && meta.mode() & 0o777 == 0o700 =>
            {
                fs::remove_dir_all(&self.path)?;
            }
            Ok(_) => return Err("invalid restore scratch entry".into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        match fs::remove_file(&self.claim) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.clean();
    }
}

fn prepare_job(parent: &Path) -> Result<Scratch> {
    let claim = subpath(parent, CLAIM)?;
    if let Some(path) = claimed_job(&claim)? {
        let previous = Scratch {
            path,
            claim: subpath(parent, CLAIM)?,
        };
        previous.clean()?;
    }
    let nonce = super::install_mta_setup::random_hex(16)?;
    let name = allocation::format(format_args!("{JOB_PREFIX}{nonce}"))?;
    let path = subpath(parent, &name)?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        _ => return Err("new restore scratch name is unavailable".into()),
    }
    std::os::unix::fs::symlink(&name, &claim)?;
    if let Err(e) = private_dir(&path) {
        fs::remove_file(&claim)?;
        return Err(e.into());
    }
    Ok(Scratch { path, claim })
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = crate::sys::kill_child_recorded(&mut self.0, "terminate disposable restore");
            let _ = self.0.wait();
        }
    }
}

fn space(path: &Path) -> Result<u64> {
    let df = ["/usr/bin/df", "/run/current-system/profile/bin/df"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .ok_or("missing host df")?;
    let mut process = Process(
        Command::new(df)
            .env_clear()
            .args(["-B1", "--output=avail", "--"])
            .arg(path)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let start = Instant::now();
    loop {
        if let Some(status) = process.0.try_wait()? {
            if !status.success() {
                return Err("cannot check restore filesystem space".into());
            }
            break;
        }
        if start.elapsed() >= Duration::from_secs(5) {
            return Err("restore filesystem space check timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut out = [0u8; 128];
    let mut stdout = process.0.stdout.take().ok_or("missing df output")?;
    let mut used = 0;
    loop {
        let buf = out
            .get_mut(used..)
            .filter(|v| !v.is_empty())
            .ok_or("oversized df output")?;
        let n = stdout.read(buf)?;
        if n == 0 {
            break;
        }
        used += n;
    }
    Ok(
        std::str::from_utf8(out.get(..used).ok_or("invalid df output")?)?
            .split_whitespace()
            .nth(1)
            .ok_or("invalid free space")?
            .parse()?,
    )
}

fn usage(path: &Path, depth: usize, entries: &mut usize) -> Result<u64> {
    if depth > 12 || *entries > 131072 {
        return Err("restore directory inventory exceeds bounds".into());
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    *entries += 1;
    let mut bytes = meta.blocks().saturating_mul(512);
    if meta.is_dir() {
        let contents = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        for entry in contents {
            bytes = bytes.saturating_add(usage(&entry?.path(), depth + 1, entries)?);
        }
    }
    Ok(bytes)
}

pub(super) fn verify(
    archive: &mut File,
    scratch_parent: &Path,
) -> std::result::Result<Verified, String> {
    run(archive, scratch_parent)
        .map_err(|e| allocation::format(format_args!("{e}")).unwrap_or_default())
}

fn run(archive: &File, parent: &Path) -> Result<Verified> {
    if crate::sys::getuid() == 0 {
        return Err("restore drill requires an unprivileged user".into());
    }
    let runtime = runtime_from_path()?;
    let lock = File::open(parent)?;
    if !crate::sys::flock_try_exclusive(lock.as_raw_fd())? {
        return Err("restore scratch is busy".into());
    }
    let job = prepare_job(parent)?;
    let ceiling = archive
        .metadata()?
        .len()
        .saturating_mul(12)
        .clamp(4 * GIB, 64 * GIB);
    if space(parent)? < 2 * GIB + ceiling {
        return Err("insufficient free space for restore drill".into());
    }
    let work = subpath(&job.path, "work")?;
    private_dir(&work)?;
    let result = subpath(&job.path, "counts")?;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&result)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["install-td-mta", "--backup-restore-drill-jail"])
        .current_dir(&job.path)
        .env("PATH", &runtime.bin)
        .stdin(Stdio::from(archive.try_clone()?))
        .stdout(Stdio::null());
    sandbox::cap_child_data_rlimit(&mut cmd, GIB);
    sandbox::die_with_parent(&mut cmd);
    let mut child = Process(cmd.spawn()?);
    let start = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                return Err("disposable PostgreSQL restore or verification failed".into());
            }
            break;
        }
        if start.elapsed() >= DEADLINE {
            return Err("restore deadline exceeded".into());
        }
        if space(parent)? < 2 * GIB || usage(&work, 0, &mut 0)? > ceiling {
            return Err("restore scratch space limit reached".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let mut buf = [0u8; 128];
    let mut input = File::open(&result)?;
    let size = usize::try_from(input.metadata()?.len())?;
    let bytes = buf.get_mut(..size).ok_or("invalid restore result length")?;
    input.read_exact(bytes)?;
    let verified = Verified::parse(std::str::from_utf8(bytes)?)?;
    job.clean()
        .map_err(|_| "restore verified but scratch cleanup failed")?;
    Ok(verified)
}

fn runtime_from_path() -> Result<restore_layout::Layout> {
    Ok(restore_layout::from_path(
        &std::env::var("PATH")?,
        std::env::var_os("LD_LIBRARY_PATH").as_deref(),
    )?)
}

fn add_bind(binds: &mut Vec<Bind>, from: &Path, to: Option<&str>, writable: bool) -> Result<()> {
    allocation::push(
        binds,
        Bind {
            src: allocation::string(from.to_str().ok_or("invalid restore bind")?)?,
            dest: to.map(allocation::string).transpose()?,
            readonly: !writable,
            ro_optional: false,
        },
    )?;
    Ok(())
}

fn jailed() -> Result<()> {
    let uid = crate::sys::getuid();
    let gid = crate::sys::getgid();
    if uid == 0 {
        return Err("restore jail requires an unprivileged user".into());
    }
    let job = std::env::current_dir()?;
    let parent = job.parent().ok_or("invalid restore workspace")?;
    if claimed_job(&subpath(parent, CLAIM)?)?.as_deref() != Some(&job) {
        return Err("restore jail requires a prepared private workspace".into());
    }
    let lock = File::open(job.parent().ok_or("invalid restore workspace")?)?;
    if crate::sys::flock_try_exclusive(lock.as_raw_fd())? {
        return Err("restore jail requires the active supervisor".into());
    }
    let runtime = runtime_from_path()?;
    let mut binds = Vec::new();
    for path in restore_layout::system_runtime() {
        if path.exists() {
            add_bind(&mut binds, path, None, false)?;
        }
    }
    for path in [&runtime.bin, &runtime.lib, &runtime.share]
        .into_iter()
        .chain(&runtime.extra_libs)
    {
        if !binds
            .iter()
            .any(|bind| path.starts_with(Path::new(&bind.src)))
        {
            add_bind(&mut binds, path, None, false)?;
        }
    }
    let passwd = subpath(&job, "passwd")?;
    let group = subpath(&job, "group")?;
    new_file(
        &passwd,
        allocation::format(format_args!("drill:x:{uid}:{gid}::/tmp:/bin/sh\n"))?.as_bytes(),
    )?;
    new_file(
        &group,
        allocation::format(format_args!("drill:x:{gid}:\n"))?.as_bytes(),
    )?;
    add_bind(&mut binds, &passwd, Some("/etc/passwd"), false)?;
    add_bind(&mut binds, &group, Some("/etc/group"), false)?;
    add_bind(
        &mut binds,
        &std::env::current_exe()?,
        Some("/drill-controller"),
        false,
    )?;
    add_bind(&mut binds, &subpath(&job, "work")?, Some("/work"), true)?;
    add_bind(&mut binds, &subpath(&job, "counts")?, Some("/counts"), true)?;
    let args = [
        allocation::string("install-td-mta")?,
        allocation::string("--backup-restore-drill-worker")?,
    ];
    let mut env = Vec::new();
    let libraries = restore_layout::controller_library_path(runtime.library_path.as_deref())?;
    if !libraries.is_empty() {
        allocation::push(
            &mut env,
            (allocation::string("LD_LIBRARY_PATH")?, libraries),
        )?;
    }
    if let Some(ld) = runtime.library_path {
        allocation::push(
            &mut env,
            (allocation::string("TD_MTA_DRILL_PG_LIBRARIES")?, ld),
        )?;
    }
    if !sandbox::host_shell(
        "/drill-controller",
        &args,
        &binds,
        &[allocation::string("/tmp")?],
        runtime
            .bin
            .to_str()
            .ok_or("invalid PostgreSQL binary path")?,
        "/tmp",
        "/work",
        &env,
        &[],
        &job,
    )?
    .success()
    {
        return Err("isolated restore worker failed".into());
    }
    Ok(())
}

fn finish(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("restore drill: {e}");
            ExitCode::FAILURE
        }
    }
}
pub(super) fn jail() -> ExitCode {
    finish(jailed())
}
pub(super) fn worker() -> ExitCode {
    finish(database::run())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_reclaims_partial_cleanup_and_dangling_registration() {
        let tmp = crate::test_temp::TempDir::new("restore-abandoned").unwrap();
        let first = prepare_job(tmp.path()).unwrap();
        let stale = first.path.clone();
        fs::create_dir(stale.join("pgdata")).unwrap();
        fs::write(stale.join("pgdata/leftover"), b"data").unwrap();
        std::mem::forget(first);
        let second = prepare_job(tmp.path()).unwrap();
        assert!(!stale.exists());
        // An abrupt exit after the tree disappears but before unlinking the claim.
        fs::remove_dir(&second.path).unwrap();
        std::mem::forget(second);
        let third = prepare_job(tmp.path()).unwrap();
        drop(third);
        assert!(fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn scratch_preserves_unregistered_paths_and_refuses_ambiguous_claims() {
        let tmp = crate::test_temp::TempDir::new("restore-unregistered").unwrap();
        let unrelated = tmp.join(format!("{JOB_PREFIX}{}", "a".repeat(32)));
        private_dir(&unrelated).unwrap();
        let keep = unrelated.join("unknown");
        fs::write(&keep, b"keep").unwrap();
        drop(prepare_job(tmp.path()).unwrap());
        let claim = tmp.join(CLAIM);
        fs::write(&claim, b"operator file").unwrap();
        assert!(prepare_job(tmp.path()).is_err());
        assert_eq!(fs::read(&claim).unwrap(), b"operator file");
        fs::remove_file(&claim).unwrap();
        std::os::unix::fs::symlink("../unsafe", &claim).unwrap();
        assert!(prepare_job(tmp.path()).is_err());
        assert_eq!(fs::read(&keep).unwrap(), b"keep");
    }
}
