//! The root side: the boot oneshot, the consented `apply`, and the status
//! they publish for unprivileged readers.

use crate::probe::{self, Found};
use crate::setting::{Change, Kind};
use crate::table::{Row, Table, MAX_BYTES};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;
const STATE: &str = "/var/lib/td/volumes";
const RUN: &str = "/run/td-disks";
const MNT: &str = "/mnt";
const MOUNTS: &str = "/proc/self/mounts";
const MAX_MOUNTS: u64 = 1 << 20;
const MAX_STATUS: u64 = 64 << 10;

/// Where the root side reads and writes; tests point it at a tree.
pub struct Places {
    pub state: PathBuf,
    pub run: PathBuf,
    pub mnt: PathBuf,
    pub sys: PathBuf,
    pub dev: PathBuf,
    pub mounts: PathBuf,
    pub mount: PathBuf,
    pub umount: PathBuf,
    /// The owner every state directory must have.
    pub owner: u32,
}

impl Places {
    pub fn system() -> Self {
        Self {
            state: STATE.into(),
            run: RUN.into(),
            mnt: MNT.into(),
            sys: "/sys/class/block".into(),
            dev: "/dev".into(),
            mounts: MOUNTS.into(),
            mount: "/bin/mount".into(),
            umount: "/bin/umount".into(),
            owner: 0,
        }
    }
}

fn trusted(path: &Path, owner: u32) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "untrusted volume-state directory {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_dir(path: &Path, owner: u32) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o755).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    trusted(path, owner)
}

/// The exclusive state lock; held for every table read that writes.
pub struct Locked {
    _lock: File,
}

pub fn lock(places: &Places) -> Result<Locked, String> {
    if let Some(parent) = places.state.parent() {
        trusted(parent, places.owner)?;
    }
    ensure_dir(&places.state, places.owner)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(places.state.join("lock"))
        .map_err(|e| format!("volume lock: {e}"))?;
    let meta = lock.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.uid() != places.owner || meta.mode() & 0o077 != 0 {
        return Err("invalid volume lock".into());
    }
    lock.try_lock()
        .map_err(|e| format!("volume management is busy: {e}"))?;
    Ok(Locked { _lock: lock })
}

/// The saved table; absent is empty, anything untrusted refuses.
pub fn read_table(places: &Places, _locked: &Locked) -> Result<Table, String> {
    let path = places.state.join("table");
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Table::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.uid() != places.owner
        || meta.mode() & 0o022 != 0
        || meta.len() > MAX_BYTES as u64
    {
        return Err("volume table is not a bounded root-owned file".into());
    }
    let mut text = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("volume table: {e}"))?;
    Table::parse(&text)
}

fn write_table(places: &Places, _locked: &Locked, table: &Table) -> Result<(), String> {
    td_fs::replace(
        &places.state.join("table"),
        table.format().as_bytes(),
        0o644,
    )
    .map_err(|e| format!("save the volume table: {e}"))
}

/// `(source, target)` of every mount, unescaped as the kernel writes them.
fn mounts(places: &Places) -> Result<Vec<(String, String)>, String> {
    let mut text = String::new();
    File::open(&places.mounts)
        .and_then(|file| file.take(MAX_MOUNTS + 1).read_to_string(&mut text))
        .map_err(|e| format!("{}: {e}", places.mounts.display()))?;
    if text.len() as u64 > MAX_MOUNTS {
        return Err(format!(
            "{} is too large to read whole",
            places.mounts.display()
        ));
    }
    let unescape = |field: &str| {
        field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
    };
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            Some((unescape(fields.next()?), unescape(fields.next()?)))
        })
        .collect())
}

fn mounted_at(mounts: &[(String, String)], target: &Path) -> bool {
    mounts.iter().any(|(_, at)| Path::new(at) == target)
}

/// Filesystems a mount already uses through any of their members, so a
/// second mount of the td volume or of an attached disk is never made.
fn in_use<'a>(found: &'a [Found], mounts: &[(String, String)]) -> Vec<&'a Found> {
    found
        .iter()
        .filter(|member| {
            mounts
                .iter()
                .any(|(source, _)| Path::new(source) == member.device)
        })
        .collect()
}

fn run(program: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} {}: {}",
        program.display(),
        args.join(" "),
        why.trim_end()
    ))
}

/// A private tmpfs over the image's empty, read-only `/mnt`, once.
fn mount_root(places: &Places, mounts: &[(String, String)]) -> Result<(), String> {
    if mounted_at(mounts, &places.mnt) {
        return Ok(());
    }
    run(
        &places.mount,
        &[
            "-t",
            "tmpfs",
            "-o",
            "nodev,nosuid,noexec,mode=0755,size=64k",
            "tdvolumes",
            &places.mnt.to_string_lossy(),
        ],
    )
}

fn target(places: &Places, row: &Row) -> PathBuf {
    places.mnt.join(&row.name)
}

/// The `mount` operands for `row` over its complete member set.
pub fn mount_args(row: &Row, members: &[&Found], target: &Path) -> Option<Vec<String>> {
    let first = members.first()?;
    let mut options = String::from(row.mode.flags());
    if row.kind == Kind::Btrfs {
        for member in members {
            options.push_str(",device=");
            options.push_str(&member.device.to_string_lossy());
        }
    }
    Some(vec![
        "-t".into(),
        row.kind.word().into(),
        "-o".into(),
        options,
        first.device.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ])
}

/// Mount one row, or say why not.
fn mount_row(
    places: &Places,
    row: &Row,
    found: &[Found],
    mounts: &[(String, String)],
) -> Result<&'static str, String> {
    let at = target(places, row);
    if mounted_at(mounts, &at) {
        return Ok("mounted");
    }
    let (members, complete) = probe::members(found, row.uuid, row.kind);
    if members.is_empty() {
        if found.iter().any(|member| member.uuid == row.uuid) {
            return Err(format!("attached, but not as {}", row.kind.word()));
        }
        return Err("absent".into());
    }
    if !complete {
        return Err("incomplete: not every member device is attached".into());
    }
    if in_use(found, mounts).iter().any(|m| m.uuid == row.uuid) {
        return Err("already mounted elsewhere".into());
    }
    mount_root(places, mounts)?;
    match fs::DirBuilder::new().mode(0o755).create(&at) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
                return Err(format!("{} is not a directory", at.display()));
            }
        }
        Err(e) => return Err(format!("{}: {e}", at.display())),
    }
    let args = mount_args(row, &members, &at).ok_or("no member")?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(error) = run(&places.mount, &args) {
        let _ = fs::remove_dir(&at);
        return Err(error);
    }
    Ok("mounted")
}

/// Publish what was found and what each row became, for `status`.
fn publish(places: &Places, table: &Table, found: &[Found], lines: &[String], notes: &[String]) {
    let mut text = String::from("volumes:\n");
    for (row, line) in table.rows.iter().zip(lines) {
        text.push_str(&format!(
            "  {} {} {} {} -> /mnt/{}: {line}\n",
            row.name,
            row.uuid,
            row.kind.word(),
            row.mode.word(),
            row.name
        ));
    }
    if table.rows.is_empty() {
        text.push_str("  none\n");
    }
    text.push_str("filesystems found:\n");
    for member in found {
        text.push_str(&format!(
            "  {} {} {}{}\n",
            member.device.display(),
            member.kind.word(),
            member.uuid,
            member
                .member
                .map(|(id, count)| format!(" device {id} of {count}"))
                .unwrap_or_default()
        ));
    }
    for note in notes {
        text.push_str(&format!("note: {note}\n"));
    }
    let written = ensure_dir(&places.run, places.owner).and_then(|()| {
        td_fs::replace(&places.run.join("status"), text.as_bytes(), 0o644)
            .map_err(|e| e.to_string())
    });
    if let Err(error) = written {
        let _ = writeln!(std::io::stderr(), "td-disks: status not published: {error}");
    }
}

fn settle(places: &Places, table: &Table) -> Result<Vec<String>, String> {
    let (found, notes) = probe::scan(&places.sys, &places.dev);
    let mut lines = Vec::new();
    let mut failed = false;
    for row in &table.rows {
        let mounts = mounts(places)?;
        let line = match mount_row(places, row, &found, &mounts) {
            Ok(state) => state.to_string(),
            Err(why) => {
                failed = true;
                format!("not mounted: {why}")
            }
        };
        lines.push(line);
    }
    publish(places, table, &found, &lines, &notes);
    if failed {
        return Err("some volumes were not mounted; see td-disks status".into());
    }
    Ok(lines)
}

/// The boot oneshot: mount every complete, unused volume in the table.
pub fn mount_all(places: &Places) -> Result<(), String> {
    let locked = lock(places)?;
    let table = match read_table(places, &locked) {
        Ok(table) => table,
        Err(error) => {
            publish(
                places,
                &Table::default(),
                &[],
                &[],
                std::slice::from_ref(&error),
            );
            return Err(error);
        }
    };
    let lines = settle(places, &table)?;
    for (row, line) in table.rows.iter().zip(lines) {
        let _ = writeln!(std::io::stdout(), "td-disks: {}: {line}", row.name);
    }
    Ok(())
}

/// The consented change: recheck it against what is attached, mount or
/// unmount that one volume, and only then save the table. A refusal
/// changes nothing.
pub fn apply(places: &Places, change: &Change) -> Result<(), String> {
    let (found, notes) = probe::scan(&places.sys, &places.dev);
    apply_found(places, change, &found, &notes)
}

/// `apply` over the members a scan found. Only the changed row is mounted
/// or unmounted; the status is republished without touching other rows.
fn apply_found(
    places: &Places,
    change: &Change,
    found: &[Found],
    notes: &[String],
) -> Result<(), String> {
    let locked = lock(places)?;
    let table = read_table(places, &locked)?;
    let next = table.changed(change)?;
    match change {
        Change::Add { name, .. } => {
            let row = next.get(name).ok_or("added volume vanished")?.clone();
            let mounts = mounts(places)?;
            mount_row(places, &row, found, &mounts)?;
            if let Err(error) = write_table(places, &locked, &next) {
                let at = target(places, &row);
                let _ = run(&places.umount, &[&at.to_string_lossy()]);
                let _ = fs::remove_dir(&at);
                return Err(error);
            }
        }
        Change::Remove { name } => {
            let row = table.get(name).ok_or("no such volume")?.clone();
            let at = target(places, &row);
            if mounted_at(&mounts(places)?, &at) {
                run(&places.umount, &[&at.to_string_lossy()])
                    .map_err(|e| format!("{name} is in use and stays: {e}"))?;
            }
            write_table(places, &locked, &next)?;
            let _ = fs::remove_dir(&at);
        }
    }
    // The change is applied: a mount table that cannot be read now only
    // leaves the status less precise.
    let mounts = mounts(places);
    let lines: Vec<String> = next
        .rows
        .iter()
        .map(|row| match &mounts {
            Ok(mounts) if mounted_at(mounts, &target(places, row)) => "mounted".to_string(),
            Ok(_) => "not mounted".to_string(),
            Err(_) => "unknown: the mount table could not be read".to_string(),
        })
        .collect();
    publish(places, &next, found, &lines, notes);
    Ok(())
}

/// The last published status, readable by anyone.
pub fn status(places: &Places) -> Result<String, String> {
    let path = places.run.join("status");
    let mut text = String::new();
    File::open(&path)
        .and_then(|file| file.take(MAX_STATUS).read_to_string(&mut text))
        .map_err(|e| format!("{}: {e} (the volumes service has not run)", path.display()))?;
    Ok(text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::setting::{Mode, Uuid};
    use std::os::unix::fs::PermissionsExt;

    fn places(root: &Path) -> Places {
        let state_parent = root.join("var-lib-td");
        fs::create_dir(&state_parent).unwrap();
        fs::set_permissions(&state_parent, fs::Permissions::from_mode(0o755)).unwrap();
        Places {
            state: state_parent.join("volumes"),
            run: root.join("run"),
            mnt: root.join("mnt"),
            sys: root.join("sys"),
            dev: root.join("dev"),
            mounts: root.join("mounts"),
            mount: "/nonexistent/mount".into(),
            umount: "/nonexistent/umount".into(),
            owner: fs::metadata(root).unwrap().uid(),
        }
    }

    #[test]
    fn state_is_locked_exclusively_and_untrusted_tables_refuse() {
        let dir = td_fs::TempDir::new("td-disks-state").unwrap();
        let places = places(dir.path());
        let locked = lock(&places).unwrap();
        assert!(lock(&places).is_err(), "a second holder is refused");
        assert_eq!(read_table(&places, &locked).unwrap(), Table::default());
        let table =
            Table::parse("td-volumes-v1\nbackup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data\n")
                .unwrap();
        write_table(&places, &locked, &table).unwrap();
        assert_eq!(read_table(&places, &locked).unwrap(), table);
        let path = places.state.join("table");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_table(&places, &locked).is_err(), "world-writable");
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
        assert!(
            read_table(&places, &locked).is_err(),
            "a symlink is refused"
        );
        drop(locked);
        fs::set_permissions(&places.state, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            lock(&places).is_err(),
            "a writable state directory is refused"
        );
    }

    #[test]
    fn btrfs_mounts_name_every_member_and_modes_set_their_flags() {
        let row = Row {
            name: "backup".into(),
            uuid: Uuid([1; 16]),
            kind: Kind::Btrfs,
            mode: Mode::Data,
        };
        let member = |device: &str, id| Found {
            device: device.into(),
            uuid: Uuid([1; 16]),
            kind: Kind::Btrfs,
            member: Some((id, 2)),
        };
        let (a, b) = (member("/dev/sda", 1), member("/dev/sdb", 2));
        let args = mount_args(&row, &[&a, &b], Path::new("/mnt/backup")).unwrap();
        assert_eq!(
            args,
            [
                "-t",
                "btrfs",
                "-o",
                "nodev,nosuid,noexec,device=/dev/sda,device=/dev/sdb",
                "/dev/sda",
                "/mnt/backup"
            ]
        );
        let ext = Row {
            kind: Kind::Ext4,
            mode: Mode::Exec,
            ..row
        };
        let c = Found {
            kind: Kind::Ext4,
            member: None,
            ..member("/dev/nvme1n1p2", 1)
        };
        let args = mount_args(&ext, &[&c], Path::new("/mnt/old")).unwrap();
        assert_eq!(args[3], "nodev,nosuid");
        assert!(mount_args(&ext, &[], Path::new("/mnt/old")).is_none());
    }

    const UUID: &str = "3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10";

    fn backup(kind: Kind, device: &str) -> Found {
        Found {
            device: device.into(),
            uuid: Uuid::parse(UUID).unwrap(),
            kind,
            member: (kind == Kind::Btrfs).then_some((1, 1)),
        }
    }

    fn saved(places: &Places) -> Option<String> {
        fs::read_to_string(places.state.join("table")).ok()
    }

    #[test]
    fn a_refused_add_saves_nothing_and_leaves_no_mount_point() {
        let dir = td_fs::TempDir::new("td-disks-add").unwrap();
        let places = places(dir.path());
        fs::create_dir(&places.mnt).unwrap();
        fs::write(
            &places.mounts,
            format!(
                "/dev/nvme0n1p3 / btrfs rw 0 0\ntdvolumes {} tmpfs rw 0 0\n",
                places.mnt.display()
            ),
        )
        .unwrap();
        let add = Change::decode(&format!("add backup {UUID} btrfs data")).unwrap();
        let absent = apply_found(&places, &add, &[], &[]).unwrap_err();
        assert_eq!(absent, "absent");
        let ext4 = [backup(Kind::Ext4, "/dev/sdb")];
        let kind = apply_found(&places, &add, &ext4, &[]).unwrap_err();
        assert!(kind.contains("not as btrfs"), "{kind}");
        let root = [backup(Kind::Btrfs, "/dev/nvme0n1p3")];
        let used = apply_found(&places, &add, &root, &[]).unwrap_err();
        assert!(used.contains("already mounted"), "{used}");
        // The mount itself fails: the table is not saved, /mnt/backup goes.
        let array = [backup(Kind::Btrfs, "/dev/sda")];
        assert!(apply_found(&places, &add, &array, &[]).is_err());
        assert_eq!(saved(&places), None);
        assert!(!places.mnt.join("backup").exists());
    }

    #[test]
    fn a_busy_remove_keeps_its_row_and_an_idle_one_publishes_status() {
        let dir = td_fs::TempDir::new("td-disks-remove").unwrap();
        let places = places(dir.path());
        let table = Table::parse(&format!("td-volumes-v1\nbackup {UUID} btrfs data\n")).unwrap();
        {
            let locked = lock(&places).unwrap();
            write_table(&places, &locked, &table).unwrap();
        }
        let before = saved(&places);
        let remove = Change::decode("remove backup").unwrap();
        fs::write(
            &places.mounts,
            format!(
                "/dev/sda {} btrfs rw 0 0\n",
                places.mnt.join("backup").display()
            ),
        )
        .unwrap();
        let busy = apply_found(&places, &remove, &[], &[]).unwrap_err();
        assert!(busy.contains("in use and stays"), "{busy}");
        assert_eq!(saved(&places), before);
        fs::write(&places.mounts, "/dev/nvme0n1p3 / btrfs rw 0 0\n").unwrap();
        apply_found(&places, &remove, &[], &[]).unwrap();
        assert_eq!(saved(&places).unwrap(), "td-volumes-v1\n");
        let status = status(&places).unwrap();
        assert!(status.contains("volumes:\n  none\n"), "{status}");
        assert!(
            apply_found(&places, &remove, &[], &[]).is_err(),
            "gone already"
        );
    }

    #[test]
    fn mount_table_is_read_unescaped_and_in_use_filesystems_found() {
        let dir = td_fs::TempDir::new("td-disks-mounts").unwrap();
        let places = places(dir.path());
        fs::write(
            &places.mounts,
            "/dev/nvme0n1p3 / btrfs rw 0 0\ntdvolumes /mnt\\040x tmpfs rw 0 0\n",
        )
        .unwrap();
        let table = mounts(&places).unwrap();
        assert!(mounted_at(&table, Path::new("/mnt x")));
        let root = Found {
            device: "/dev/nvme0n1p3".into(),
            uuid: Uuid([9; 16]),
            kind: Kind::Btrfs,
            member: Some((1, 1)),
        };
        let other = Found {
            device: "/dev/sda".into(),
            uuid: Uuid([1; 16]),
            ..root.clone()
        };
        let found = [root.clone(), other];
        assert_eq!(in_use(&found, &table), [&root]);
    }
}
