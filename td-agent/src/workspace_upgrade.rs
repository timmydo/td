//! One-time copy of a pre-account conversation's private data. Root receives
//! an ordinary read grant; the copy into the account runs inside its jail.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::workspace_slot::{error, joined, Account};

const TREES: [&str; 5] = [
    "home",
    "runtime/tmp",
    "runtime/var-tmp",
    "runtime/shm",
    "runtime/run",
];
const O_NOFOLLOW: i32 = 0x20000;
const O_DIRECTORY: i32 = 0x10000;
const O_NONBLOCK: i32 = 0x800;
const RECEIPT: &[u8] = b"workspace import complete\n";
const IMPORTED: &str = "account-imported";

fn imported(legacy: &Path) -> io::Result<bool> {
    let path = joined(legacy, IMPORTED)?;
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("invalid workspace import receipt"));
    }
    let mut bytes = [0; RECEIPT.len() + 1];
    let mut len = 0;
    while len < bytes.len() {
        let n = file.read(bytes.get_mut(len..).unwrap_or_default())?;
        if n == 0 {
            break;
        }
        len += n;
    }
    if bytes.get(..len) != Some(RECEIPT) {
        return Err(io::Error::other("invalid workspace import receipt"));
    }
    Ok(true)
}

/// Called only after the durable account-ready receipt, while the pool
/// lock is held. A crash between receipts resumes here without resetting
/// the copied account. Later slot reuse never imports old backups again.
pub(crate) fn finish(legacy: &Path) -> io::Result<()> {
    if !imported(legacy)? {
        td_fs::private_dir(legacy)?;
        td_fs::replace(&joined(legacy, IMPORTED)?, RECEIPT, 0o600)?;
    }
    Ok(())
}

struct Budget {
    entries: usize,
    bytes: u64,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            entries: 250_000,
            bytes: 64 * 1024 * 1024 * 1024,
        }
    }
}

fn child(base: &Path, name: &std::ffi::OsStr) -> io::Result<PathBuf> {
    let mut bytes = Vec::new();
    td_alloc::extend_from_slice(&mut bytes, base.as_os_str().as_bytes())
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    td_alloc::push(&mut bytes, b'/').map_err(|_| io::ErrorKind::OutOfMemory)?;
    td_alloc::extend_from_slice(&mut bytes, name.as_bytes())
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Pin each source directory before walking it. Descendants are opened
/// relative to that fd through procfs, without following symlinks. Targets
/// are fresh, private directories with no concurrent writers.
fn copy(source: &Path, target: &Path, budget: &mut Budget, depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Err(io::Error::other(
            "workspace import exceeds 64 directory levels",
        ));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY | O_NONBLOCK)
        .open(source)?;
    let pinned = td_alloc::format(format_args!("/proc/self/fd/{}", directory.as_raw_fd()))
        .map(PathBuf::from)
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    for entry in fs::read_dir(&pinned)? {
        budget.entries = budget
            .entries
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("workspace import exceeds 250000 entries"))?;
        let entry = entry?;
        let from = child(&pinned, &entry.file_name())?;
        let to = child(target, &entry.file_name())?;
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.is_dir() {
            fs::DirBuilder::new().mode(0o700).create(&to)?;
            copy(&from, &to, budget, depth + 1)?;
            let copied = File::open(&to)?;
            copied.set_permissions(fs::Permissions::from_mode(
                metadata.permissions().mode() & 0o777,
            ))?;
            copied.sync_all()?;
        } else if metadata.file_type().is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
        } else if metadata.is_file() {
            let input = OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(&from)?;
            let metadata = input.metadata()?;
            if !metadata.is_file() {
                return Err(io::Error::other("workspace import source changed type"));
            }
            let len = metadata.len();
            budget.bytes = budget
                .bytes
                .checked_sub(len)
                .ok_or_else(|| io::Error::other("workspace import exceeds 64 GiB"))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&to)?;
            if io::copy(&mut input.take(len + 1), &mut output)? != len {
                return Err(io::Error::other("workspace import source changed length"));
            }
            output.set_permissions(fs::Permissions::from_mode(
                metadata.permissions().mode() & 0o777,
            ))?;
            output.sync_all()?;
        } else if std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type()) {
            // A dead socket has no persistent contents and cannot be moved.
        } else {
            return Err(io::Error::other(
                "workspace import found a special file; move it out of the old private tree",
            ));
        }
    }
    File::open(target)?.sync_all()
}

/// Keep the originals as recovery copies. A failed import leaves no ready
/// receipt; the next attempt empties the account before trying again.
pub(crate) fn prepare(account: &Account, human_home: &Path, legacy: &Path) -> io::Result<()> {
    if imported(legacy)? {
        return Ok(());
    }
    let mut present = false;
    for tree in TREES {
        match fs::symlink_metadata(joined(legacy, tree)?) {
            Ok(_) => present = true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if !present {
        return Ok(());
    }
    let stage = td_fs::TempDir::new_in(human_home, "workspace-upgrade")?;
    let mut budget = Budget::default();
    for (n, tree) in TREES.iter().enumerate() {
        let name = td_alloc::format(format_args!("{n}")).map_err(|_| io::ErrorKind::OutOfMemory)?;
        let target = joined(stage.path(), &name)?;
        fs::create_dir(&target)?;
        let source = joined(legacy, tree)?;
        match fs::symlink_metadata(&source) {
            Ok(_) => copy(&source, &target, &mut budget, 0)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let mut policy = crate::jail::Policy {
        development: true,
        ..Default::default()
    };
    td_alloc::push(&mut policy.read, fs::canonicalize(stage.path())?)
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    let mut args = Vec::new();
    for text in [
        "workspace-import",
        stage
            .path()
            .to_str()
            .ok_or_else(|| io::Error::other("import path is not UTF-8"))?,
    ] {
        td_alloc::push(
            &mut args,
            td_alloc::string(text).map_err(|_| io::ErrorKind::OutOfMemory)?,
        )
        .map_err(|_| io::ErrorKind::OutOfMemory)?;
    }
    let mut stream = account.launch(&policy, &args)?;
    stream.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut answer = [0; RECEIPT.len() + 1];
    // Read exactly the receipt and EOF; a short stream or an extra byte
    // cannot commit a partially imported account.
    let mut got = 0;
    while got < answer.len() {
        let read = stream.read(answer.get_mut(got..).unwrap_or_default())?;
        if read == 0 {
            break;
        }
        got += read;
    }
    if answer.get(..got) != Some(RECEIPT) {
        return Err(io::Error::other(
            "workspace import failed; see /var/log/svc/workspace-intake.log",
        ));
    }
    Ok(())
}

pub fn inside(args: &[String]) -> Result<(), String> {
    let [source] = args else {
        return Err(error("workspace-import needs one source directory"));
    };
    let home = std::env::var_os("HOME").ok_or_else(|| error("workspace import has no HOME"))?;
    let targets = [
        Path::new(&home),
        Path::new("/tmp"),
        Path::new("/var/tmp"),
        Path::new("/dev/shm"),
        Path::new("/run"),
    ];
    let mut budget = Budget::default();
    for (n, target) in targets.into_iter().enumerate() {
        let name = td_alloc::format(format_args!("{n}")).map_err(error)?;
        copy(
            &joined(Path::new(source), &name).map_err(error)?,
            target,
            &mut budget,
            0,
        )
        .map_err(error)?;
    }
    std::io::stdout().write_all(RECEIPT).map_err(error)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn copy_bounds_and_receipts_fail_closed() {
        let fixture = td_fs::TempDir::new("workspace-upgrade-bounds").unwrap();
        let source = fixture.path().join("source");
        let target = fixture.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("file"), b"unchanged").unwrap();
        assert!(copy(
            &source,
            &target,
            &mut Budget {
                entries: 0,
                bytes: 100
            },
            0
        )
        .is_err());
        assert!(copy(
            &source,
            &target,
            &mut Budget {
                entries: 100,
                bytes: 1
            },
            0
        )
        .is_err());
        assert!(copy(&source, &target, &mut Budget::default(), 65).is_err());
        assert_eq!(fs::read(source.join("file")).unwrap(), b"unchanged");
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        let legacy = fixture.path().join("legacy");
        assert!(!imported(&legacy).unwrap());
        finish(&legacy).unwrap();
        assert!(imported(&legacy).unwrap());
        finish(&legacy).unwrap();
        fs::write(legacy.join(IMPORTED), b"partial").unwrap();
        assert!(imported(&legacy).is_err());
        fs::remove_file(legacy.join(IMPORTED)).unwrap();
        std::os::unix::fs::symlink(source.join("file"), legacy.join(IMPORTED)).unwrap();
        assert!(imported(&legacy).is_err());
    }
    #[test]
    fn copies_bytes_modes_and_links_without_following_them() {
        let fixture = td_fs::TempDir::new("workspace-upgrade").unwrap();
        let source = fixture.path().join("source");
        let target = fixture.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("program"), b"data").unwrap();
        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/absent/outside", source.join("link")).unwrap();
        copy(&source, &target, &mut Budget::default(), 0).unwrap();
        assert_eq!(fs::read(target.join("program")).unwrap(), b"data");
        assert_eq!(
            fs::metadata(target.join("program"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(target.join("link")).unwrap(),
            Path::new("/absent/outside")
        );
        assert!(copy(&source, &target, &mut Budget::default(), 0).is_err());
        assert!(copy(&source.join("link"), &target, &mut Budget::default(), 0).is_err());
    }
}
