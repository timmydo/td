//! Verified entry of the closure into /td/store. An existing path must
//! already hash as built. A new one comes from its uploaded NAR, checked
//! before unpacking and again once sealed, and is published whole under its
//! final name. Nothing here overwrites or deletes a store path.
use super::{lower_hex, sync_dir, Closure, Entry, STORE};
use crate::{
    common::{child, digest_reader, fmt, owned, random_hex, require, set, Host},
    isolation::files,
    R,
};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use td_json::Json;

/// The retained host td-builder the operations installer selected.
const OPERATOR: &str = "/opt/td-mta/bin/td-mta-operator";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Placed {
    Present,
    Installed,
}

pub(super) trait Store {
    /// Whether `target` exists; an existing one must be a trusted tree.
    fn exists(&mut self, target: &Path) -> R<bool>;
    /// `nar-hash` of a tree, as lowercase hex.
    fn nar_hash(&mut self, tree: &Path) -> R<String>;
    /// SHA-256 of the NAR uploaded for `basename`.
    fn nar_digest(&mut self, basename: &str) -> R<String>;
    fn restore(&mut self, basename: &str, partial: &Path) -> R<()>;
    /// Root ownership, readable by all, writable by none but root, synced.
    fn seal(&mut self, partial: &Path) -> R<()>;
    /// Rename without replacing; sync the store directory.
    fn publish(&mut self, partial: &Path, target: &Path) -> R<()>;
    fn discard(&mut self, partial: &Path) -> R<()>;
}

pub(super) fn place(store: &mut impl Store, entry: &Entry, partial: &Path) -> R<Placed> {
    let target = child(Path::new(STORE), &entry.basename)?;
    if store.exists(&target)? {
        require(
            store.nar_hash(&target)? == entry.nar_sha256,
            "an existing store path differs from the one built",
        )?;
        return Ok(Placed::Present);
    }
    require(
        store.nar_digest(&entry.basename)? == entry.nar_sha256,
        "uploaded NAR digest",
    )?;
    let result = (|| -> R<()> {
        store.restore(&entry.basename, partial)?;
        store.seal(partial)?;
        require(
            store.nar_hash(partial)? == entry.nar_sha256,
            "restored store path digest",
        )?;
        store.publish(partial, &target)
    })();
    if result.is_err() {
        let discard = store.discard(partial);
        return result.and(discard).map(|()| Placed::Installed);
    }
    Ok(Placed::Installed)
}

/// `sha256:HEX` as `nar-hash` prints it.
pub(super) fn nar_hash_line(line: &str) -> R<String> {
    let hex = line
        .strip_prefix("sha256:")
        .filter(|hex| lower_hex(hex, 64))
        .ok_or("unexpected nar-hash output")?;
    owned(hex)
}

fn nar_file(work: &Path, basename: &str) -> R<PathBuf> {
    child(work, &fmt(format_args!("{basename}.nar"))?)
}

/// The bytes of the uploaded NARs of closure paths not yet in the store,
/// the measure of what unpacking them takes there.
pub(super) fn unpacked(work: &Path, closure: &Closure) -> R<u64> {
    let mut total = 0u64;
    for entry in [&closure.glibc, &closure.postgres] {
        if child(Path::new(STORE), &entry.basename)?.try_exists()? {
            continue;
        }
        let size = match fs::symlink_metadata(nar_file(work, &entry.basename)?) {
            Ok(meta) => meta.len(),
            // Placing it refuses without its NAR.
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        total = total.checked_add(size).ok_or("closure size overflow")?;
    }
    Ok(total)
}

// A host program the extracted Debian tools' library path must not reach.
fn operator(host: &Host) -> Command {
    let mut cmd = host.cmd(OPERATOR);
    cmd.env_remove("LD_LIBRARY_PATH");
    cmd
}

/// A store path is a directory; every entry in it is owned by `owner`
/// (root's uid and gid on the host), has no group or other write and no
/// set-id bits, and is a directory, file or symbolic link.
pub(super) fn check_tree(path: &Path, owner: (u32, u32)) -> R<()> {
    require(
        fs::symlink_metadata(path)?.is_dir(),
        "a store path must be a directory",
    )?;
    check_entry(path, owner)
}

fn check_entry(path: &Path, (uid, gid): (u32, u32)) -> R<()> {
    let meta = fs::symlink_metadata(path)?;
    require(
        meta.uid() == uid && meta.gid() == gid,
        "store path ownership",
    )?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Ok(());
    }
    require(meta.mode() & 0o7022 == 0, "store path mode")?;
    if kind.is_dir() {
        for item in fs::read_dir(path)? {
            check_entry(&item?.path(), (uid, gid))?;
        }
        Ok(())
    } else {
        require(kind.is_file(), "store path file type")
    }
}

/// Whether `name` is a partial tree an earlier `install` of `basename`
/// left: `.BASENAME.partial-` and the 16 hex digits it was given.
pub(super) fn stale_partial(name: &str, basename: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|rest| rest.strip_prefix(basename))
        .and_then(|rest| rest.strip_prefix(".partial-"))
        .is_some_and(|suffix| lower_hex(suffix, 16))
}

// Remove `basename`'s own partial trees, which only an interrupted
// controller leaves; nothing else in the store is touched.
fn remove_partials(basename: &str) -> R<u64> {
    let mut removed = 0;
    for item in fs::read_dir(STORE)? {
        let item = item?;
        if !item
            .file_name()
            .to_str()
            .is_some_and(|name| stale_partial(name, basename))
        {
            continue;
        }
        if item.file_type()?.is_dir() {
            fs::remove_dir_all(item.path())?;
        } else {
            fs::remove_file(item.path())?;
        }
        removed += 1;
    }
    if removed > 0 {
        sync_dir(Path::new(STORE))?;
    }
    Ok(removed)
}

/// Remove the NARs the launcher uploaded into the job directory `work`,
/// whatever the job's outcome.
pub(crate) fn discard_nars(work: &Path) -> R<()> {
    let mut removed = false;
    for item in fs::read_dir(work)? {
        let item = item?;
        if item.file_type()?.is_file() && item.path().extension().is_some_and(|e| e == "nar") {
            fs::remove_file(item.path())?;
            removed = true;
        }
    }
    if removed {
        sync_dir(work)?;
    }
    Ok(())
}

// nar-restore ran under the supervisor's umask, so modes are set outright:
// a NAR records only whether a file is executable.
fn seal_tree(path: &Path) -> R<()> {
    let meta = fs::symlink_metadata(path)?;
    std::os::unix::fs::lchown(path, Some(0), Some(0))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Ok(());
    }
    let mode = if kind.is_dir() {
        for item in fs::read_dir(path)? {
            seal_tree(&item?.path())?;
        }
        0o755
    } else if meta.mode() & 0o100 != 0 {
        0o755
    } else {
        0o644
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    File::open(path)?.sync_all()?;
    Ok(())
}

struct HostStore<'a> {
    host: &'a Host,
    work: &'a Path,
}

impl Store for HostStore<'_> {
    fn exists(&mut self, target: &Path) -> R<bool> {
        match fs::symlink_metadata(target) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(_) => check_tree(target, (0, 0)).map(|()| true),
        }
    }
    fn nar_hash(&mut self, tree: &Path) -> R<String> {
        let out = self.host.exec(
            operator(self.host).arg("nar-hash").arg(tree),
            &[],
            Duration::from_secs(600),
            true,
        )?;
        nar_hash_line(std::str::from_utf8(&out.stdout)?.trim())
    }
    fn nar_digest(&mut self, basename: &str) -> R<String> {
        let path = nar_file(self.work, basename)?;
        let (mut file, meta) = td_fs::open_real_file(&path, "uploaded NAR")?;
        require(
            meta.uid() == 0 && meta.mode() & 0o7777 == 0o600 && meta.nlink() == 1,
            "uploaded NAR ownership or mode",
        )?;
        digest_reader(&mut file)
    }
    fn restore(&mut self, basename: &str, partial: &Path) -> R<()> {
        self.host.exec(
            operator(self.host)
                .arg("nar-restore")
                .arg(nar_file(self.work, basename)?)
                .arg(partial),
            &[],
            Duration::from_secs(600),
            true,
        )?;
        Ok(())
    }
    fn seal(&mut self, partial: &Path) -> R<()> {
        seal_tree(partial)?;
        check_tree(partial, (0, 0))
    }
    fn publish(&mut self, partial: &Path, target: &Path) -> R<()> {
        files::publish(self.host, partial, target)?;
        sync_dir(Path::new(STORE))
    }
    fn discard(&mut self, partial: &Path) -> R<()> {
        match fs::symlink_metadata(partial) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
            Ok(_) => {
                fs::remove_dir_all(partial)?;
                sync_dir(Path::new(STORE))
            }
        }
    }
}

// The retained operator must have both verbs. td-builder exits 2 for a verb
// it lacks and 1 when nar-restore refuses an empty archive.
fn check_operator(host: &Host, work: &Path) -> R<()> {
    files::trusted_binary(Path::new(OPERATOR))?;
    let out = host.exec(
        operator(host).arg("nar-hash").arg(child(work, "job.json")?),
        &[],
        Duration::from_secs(40),
        false,
    )?;
    require(
        out.status.success() && nar_hash_line(std::str::from_utf8(&out.stdout)?.trim()).is_ok(),
        "the retained td-mta-operator has no nar-hash: install a current one",
    )?;
    let probe = child(work, "nar-restore-probe")?;
    let out = host.exec(
        operator(host)
            .arg("nar-restore")
            .arg("/dev/null")
            .arg(&probe),
        &[],
        Duration::from_secs(40),
        false,
    )?;
    require(
        out.status.code() == Some(1) && !probe.try_exists()?,
        "the retained td-mta-operator has no nar-restore: install a current one",
    )
}

// /td and /td/store, root:root 0755, created if absent.
fn store_directories() -> R<()> {
    for dir in ["/td", STORE] {
        let path = Path::new(dir);
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
                sync_dir(path.parent().ok_or("store parent")?)?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        let meta = fs::symlink_metadata(path)?;
        require(
            meta.is_dir() && meta.uid() == 0 && meta.gid() == 0 && meta.mode() & 0o7777 == 0o755,
            "/td/store ownership or mode",
        )?;
    }
    files::safe_parents(&child(Path::new(STORE), "entry")?)
}

/// Place td glibc and then the PostgreSQL output, each after removing its
/// own partial trees an interrupted run left; the receipt's record of each.
/// An uploaded NAR is removed once its path is in place.
pub(super) fn install(host: &Host, work: &Path, closure: &Closure) -> R<Json> {
    check_operator(host, work)?;
    store_directories()?;
    let mut store = HostStore { host, work };
    let mut record = Vec::new();
    for entry in [&closure.glibc, &closure.postgres] {
        let stale = remove_partials(&entry.basename)?;
        let partial = child(
            Path::new(STORE),
            &fmt(format_args!(
                ".{}.partial-{}",
                entry.basename,
                random_hex(8)?
            ))?,
        )?;
        let placed = place(&mut store, entry, &partial)?;
        match fs::remove_file(nar_file(work, &entry.basename)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => (),
        }
        let mut row = Json::Obj(Vec::new());
        set(&mut row, "basename", Json::Str(owned(&entry.basename)?))?;
        set(
            &mut row,
            "placed",
            Json::Str(owned(match placed {
                Placed::Present => "present",
                Placed::Installed => "installed",
            })?),
        )?;
        set(&mut row, "stale_partials_removed", Json::from(stale))?;
        td_alloc::push(&mut record, row)?;
    }
    Ok(Json::Arr(record))
}
