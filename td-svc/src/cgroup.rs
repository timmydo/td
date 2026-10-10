//! Boot-time cgroup-v2 controller setup, application delegation, and the
//! per-service leaves system units are accounted and bounded in.

use crate::table::{Limits, Unit};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{chown, MetadataExt};
use std::path::{Path, PathBuf};

const ROOT: &str = "/sys/fs/cgroup";
const DELEGATE: &str = "/sys/fs/cgroup/td-user-1000";
const SESSION: &str = "/sys/fs/cgroup/td-user-1000/session";
/// Parent of the per-service leaves. Unlike the hierarchy root it is NOT exempt
/// from the no-internal-process rule, so it holds controllers for its children
/// and never a process of its own.
const SYSTEM: &str = "/sys/fs/cgroup/system";
const SESSION_UID: u32 = 1000;
const SESSION_GID: u32 = 1000;
const MAX_CONTROL_BYTES: u64 = 4096;
/// How much of a leaf's `cgroup.procs` a stop reads to TERM its members: a
/// few thousand pids. Past it the rest wait for `cgroup.kill`.
const MAX_MEMBERS_BYTES: u64 = 64 * 1024;
/// The controllers enabled at every level td-svc creates.
const CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];
/// What cgroup v2 spells "no limit". Written explicitly for every limit a unit
/// does NOT declare, so a reused leaf cannot keep a bound the table dropped.
const UNBOUNDED: &str = "max";
/// cgroup v2's own default `cpu.weight`. Same reason: an absent key must read
/// back as the default, not as whatever the last table said.
const DEFAULT_CPU_WEIGHT: u32 = 100;

pub(crate) fn delegate_session() -> io::Result<()> {
    require_cgroup2_mount()?;
    let root = Path::new(ROOT);
    require_words(
        &read_control(&root.join("cgroup.controllers"))?,
        CONTROLLERS,
        "root cgroup controllers",
    )?;

    // The hierarchy root is exempt from the no-internal-process rule, which is
    // why PID 1 may stay there. System services do not: `prepare_system` gives
    // each one a leaf. Only the empty application subtree is delegated below
    // the enabled controllers.
    enable_controllers(root, "root cgroup")?;

    let delegate = Path::new(DELEGATE);
    create_leaf(delegate)?;
    require_empty(&delegate.join("cgroup.procs"), "delegated cgroup")?;
    enable_controllers(delegate, "delegated cgroup")?;
    let session = Path::new(SESSION);
    create_leaf(session)?;
    set_owner(session, 0, 0)?;
    for path in [
        delegate.to_path_buf(),
        delegate.join("cgroup.procs"),
        delegate.join("cgroup.subtree_control"),
        delegate.join("cgroup.threads"),
        session.join("cgroup.procs"),
        session.join("cgroup.threads"),
    ] {
        set_owner(&path, SESSION_UID, SESSION_GID)?;
    }
    Ok(())
}

/// Create the parent every per-service leaf hangs from, and enable the
/// controllers its children need.
///
/// Separate from `delegate_session` and called beside it: the delegated
/// application root is uid-1000's, this is td-svc's own, and a failure of one
/// must not decide the other. Like the delegation, a failure here is diagnosed
/// and supervision continues — I5's console outranks accounting.
pub(crate) fn prepare_system() -> io::Result<()> {
    let system = Path::new(SYSTEM);
    create_leaf(system)?;
    // Below the root the no-internal-process rule applies, so this directory
    // must stay empty for its children to hold controllers at all. Nothing
    // places a process here; the check states that rather than trusting it.
    require_empty(&system.join("cgroup.procs"), "system cgroup")?;
    enable_controllers(system, "system cgroup")
}

/// Where a unit's processes are accounted, or why they are not.
pub(crate) enum Leaf {
    /// This unit owns this leaf.
    Own(PathBuf),
    /// The unit declared `cgroup=session`: its processes are moved into another
    /// cgroup by the program it execs, so td-svc makes it no leaf. Its limits
    /// were refused at parse time, so nothing is lost here.
    Elsewhere,
    /// The unit wants a leaf but its name cannot be one. `parse` refuses such a
    /// name, so this is unreachable through the table — it is separate from
    /// `Elsewhere` because nothing was refused on this unit's behalf, so a
    /// limit on it WOULD be lost and has to be reported rather than assumed
    /// away.
    Unnamable,
    /// This machine has no service accounting at all. Distinct from
    /// `Elsewhere` because a unit that DECLARED a limit is now unbounded, and
    /// that has to be said rather than inferred.
    Unavailable,
}

/// The leaf a unit's processes are accounted in.
///
/// Checked per start rather than cached: `prepare_system` runs once, but the
/// directory it makes can be removed underneath a running supervisor, and a
/// start after that must answer `Unavailable` rather than a path that is gone.
pub(crate) fn leaf_for(unit: &Unit) -> Leaf {
    if unit.cgroup != crate::table::Cgroup::Service {
        return Leaf::Elsewhere;
    }
    let Some(name) = unit.cgroup_leaf_name() else {
        return Leaf::Unnamable;
    };
    if !Path::new(SYSTEM).is_dir() {
        return Leaf::Unavailable;
    }
    Leaf::Own(Path::new(SYSTEM).join(name))
}

/// Create one service's leaf and write its declared limits.
///
/// Called before the spawn that will be placed into it: a limit written after a
/// process is placed never applied while that process started, which is the
/// window a memory bound most needs to cover.
///
/// EVERY control is written on every start, including the ones this unit does
/// not declare. A leaf outlives the process in it and is reused across a
/// restart, so writing only the declared limits would leave a bound the table
/// no longer carries still in force — `memory-max=64M`, then the key deleted
/// and the unit restarted, and the kernel keeps the 64M. "An absent limit is
/// unbounded" has to be written down to be true, so the unset ones are reset to
/// the kernel's own defaults rather than skipped.
pub(crate) fn create_service(leaf: &Path, limits: &Limits) -> io::Result<()> {
    create_leaf(leaf)?;
    for (control, value) in controls(limits) {
        write_control(&leaf.join(control), &value)?;
    }
    Ok(())
}

/// Every control this crate sets, and what it is set to — including the ones
/// the unit did not declare. Split out from the writing so the mapping can be
/// asserted without a cgroupfs, which the test tier does not have.
fn controls(limits: &Limits) -> [(&'static str, String); 3] {
    [
        (
            "memory.max",
            limits
                .memory_max
                .map_or_else(|| UNBOUNDED.to_string(), |bytes| bytes.to_string()),
        ),
        (
            "pids.max",
            limits
                .pids_max
                .map_or_else(|| UNBOUNDED.to_string(), |count| count.to_string()),
        ),
        (
            "cpu.weight",
            limits.cpu_weight.unwrap_or(DEFAULT_CPU_WEIGHT).to_string(),
        ),
    ]
}

/// What a placement attempt actually achieved.
#[derive(Debug)]
pub(crate) enum Placed {
    /// The process is in the leaf, proved by reading its own membership back.
    Yes,
    /// The process exited before it could be placed. Not a fault and not worth
    /// a diagnostic: a oneshot that finished this fast was never going to be
    /// accounted, and there is nothing left to bound. Reported separately from
    /// success so no caller can mistake it for one.
    ProcessGone,
}

/// Move one process into a service leaf, and prove it landed.
///
/// The write is the whole placement: cgroup v2 moves the named process, and its
/// future children are created in the same cgroup. Membership is read back
/// because a write that returns success and a membership that did not change is
/// exactly the failure an unverified limit hides. The read is of
/// `/proc/<pid>/cgroup` — the process's own one-line view — rather than the
/// leaf's `cgroup.procs`: it answers the question directly, and it is one line
/// whatever the cgroup holds, where `cgroup.procs` on a busy service would
/// overrun the control-file read bound and turn a successful placement into a
/// reported failure.
///
/// This runs in the parent, after `spawn` returns. `spawn` does not return
/// until the child's `execve` has completed — glibc's `posix_spawn` suspends
/// the parent until then, and std's fork fallback blocks on the CLOEXEC errno
/// pipe until exec closes it — so the service program is ALREADY RUNNING when
/// this is called, and nothing bounds how far it gets first. Two consequences,
/// stated because an earlier version of this comment claimed the opposite: the
/// exec image was charged outside the leaf because the exec happened BEFORE
/// this window opened rather than during it; and a service that forks before it
/// is placed leaves those descendants in td-svc's cgroup PERMANENTLY, because
/// this moves one pid and not a tree.
///
/// Closing the window needs the child placed at creation, or placing itself
/// before it execs. `clone3(CLONE_INTO_CGROUP)` does the first and is not
/// reachable from `Command`, so taking it means hand-rolling fork and exec in a
/// multithreaded supervisor — the hazard I2 exists to abolish. DESIGN.md I7
/// records both the trigger and the cheaper second option.
pub(crate) fn place(leaf: &Path, pid: i32) -> io::Result<Placed> {
    if pid <= 0 {
        // `0` means the caller in this interface — td-svc would place ITSELF,
        // and every later service would be created inside one service's leaf.
        return Err(io::Error::other(format!(
            "refusing to place pid {pid}: only a real child may be placed"
        )));
    }
    let procs = leaf.join("cgroup.procs");
    if let Err(error) = write_control(&procs, &pid.to_string()) {
        return if has_exited(pid) {
            Ok(Placed::ProcessGone)
        } else {
            Err(error)
        };
    }
    let expected = membership_of(leaf);
    match read_membership(pid) {
        Ok(actual) if actual == expected => Ok(Placed::Yes),
        Ok(_) if has_exited(pid) => Ok(Placed::ProcessGone),
        Ok(actual) => Err(io::Error::other(format!(
            "pid {pid} reads back in {actual:?}, expected {expected:?}"
        ))),
        Err(error) => {
            if has_exited(pid) {
                Ok(Placed::ProcessGone)
            } else {
                Err(error)
            }
        }
    }
}

/// Has this child finished already?
///
/// Asked of the PROCESS, never inferred from the errno of a failed placement.
/// td-svc holds the `Child` until `watch` takes it, so a service that exited
/// between `spawn` and here is an unreaped ZOMBIE, not an absent pid: it still
/// passes the kernel's pid lookup, so the `cgroup.procs` write does not return
/// ESRCH for it, and `/proc/<pid>/cgroup` still reports the cgroup it was in.
/// An earlier version read ESRCH off the write instead and was wrong twice
/// over — it could not fire for the case it existed for, and `ENOENT` from
/// that same open means the LEAF is missing, which it would have reported as
/// a success and logged nowhere.
///
/// Answering "exited" requires having ASKED. `stat_of` reports a reaped pid as
/// `Ok(None)`, and an absent `/proc` produces the same ENOENT — so without the
/// mount check a machine with no `/proc` would call every live process
/// finished, and a real placement failure would return `ProcessGone` and log
/// nothing, which is the silent loss this function exists to prevent. Both
/// "cannot ask" cases therefore answer "not exited", and the failure is
/// reported.
fn has_exited(pid: i32) -> bool {
    if !crate::procfs::is_mounted() {
        return false;
    }
    match crate::procfs::stat_of(pid) {
        Ok(Some(stat)) => stat.zombie,
        Ok(None) => true,
        Err(_) => false,
    }
}

/// The unified-hierarchy row a process inside `leaf` reads back: the leaf path
/// with the cgroupfs mount point removed.
fn membership_of(leaf: &Path) -> String {
    let inside = leaf.strip_prefix(ROOT).unwrap_or(leaf);
    format!("0::/{}", inside.display())
}

/// One process's own cgroup membership.
///
/// Compared whole: `/proc/<pid>/cgroup` carries one row per hierarchy, and the
/// td image mounts cgroup2 alone, so on the machine this runs on that is the
/// single `0::/…` row. A host with v1 hierarchies mounted would report more,
/// which is why this is a membership check and not a parser.
fn read_membership(pid: i32) -> io::Result<String> {
    read_control(Path::new(&format!("/proc/{pid}/cgroup")))
}

/// A service leaf's `cgroup.kill` and `cgroup.events`, held open.
///
/// The open descriptors pin the kernel cgroup, so neither PID reuse nor a
/// replaced path can redirect the kill or the population read. A paired unit
/// opens them at launch; a `stop=leaf` unit at its stop (DESIGN.md §4).
pub(crate) struct Cohort {
    kill: fs::File,
    events: fs::File,
    procs: fs::File,
    killed: bool,
}

impl Cohort {
    /// Open both controls of the root-owned leaf `create_service` made,
    /// whatever it holds now. A stop attaches to a leaf that is still running.
    pub(crate) fn attach(leaf: &Path) -> Result<Self, String> {
        Ok(Self {
            kill: OpenOptions::new()
                .write(true)
                .open(leaf.join("cgroup.kill"))
                .map_err(|e| format!("cgroup kill control: {e}"))?,
            events: fs::File::open(leaf.join("cgroup.events"))
                .map_err(|e| format!("cgroup events: {e}"))?,
            procs: fs::File::open(leaf.join("cgroup.procs"))
                .map_err(|e| format!("cgroup procs: {e}"))?,
            killed: false,
        })
    }

    /// The leaf's member pids as `cgroup.procs` lists them now, for a stop's
    /// TERM. Best effort, like the session scan: a member may exit between
    /// this read and its signal, and `cgroup.kill` is what ends the leaf. So
    /// an entry that is not a positive pid is skipped and counted, and the
    /// list is cut at `MAX_MEMBERS_BYTES`, whole lines only.
    pub(crate) fn members(&mut self) -> Result<Members, String> {
        self.procs
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("cgroup procs: {e}"))?;
        let mut bytes = Vec::new();
        (&mut self.procs)
            .take(MAX_MEMBERS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("cgroup procs: {e}"))?;
        let cut = bytes.len() as u64 > MAX_MEMBERS_BYTES;
        if cut {
            // Only whole lines: the last one may be a pid cut in half.
            let whole = bytes
                .iter()
                .take(MAX_MEMBERS_BYTES as usize)
                .rposition(|b| *b == b'\n')
                .map_or(0, |at| at + 1);
            bytes.truncate(whole);
        }
        let mut members = Members {
            pids: Vec::new(),
            skipped: 0,
            cut,
        };
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            match std::str::from_utf8(line)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                Some(pid) if pid > 0 => members.pids.push(pid),
                _ => members.skipped += 1,
            }
        }
        Ok(members)
    }
}

/// What one read of a leaf's `cgroup.procs` found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Members {
    pub(crate) pids: Vec<i32>,
    /// Entries that were not a positive pid.
    pub(crate) skipped: usize,
    /// The list was longer than one bounded read.
    pub(crate) cut: bool,
}

impl Cohort {
    /// `attach`, refusing a leaf that is not proven empty: a paired launch
    /// never shares its leaf with what an earlier generation left.
    pub(crate) fn open(leaf: &Path) -> Result<Self, String> {
        let mut cohort = Self::attach(leaf)?;
        if !cohort.empty()? {
            return Err("cgroup already contains live processes".into());
        }
        Ok(cohort)
    }

    /// Kill everything in the leaf, once. A kill is not emptiness: `empty`
    /// is what says it took.
    pub(crate) fn kill(&mut self) -> Result<(), String> {
        if !self.killed {
            self.kill
                .write_all(b"1")
                .map_err(|e| format!("cgroup kill: {e}"))?;
            self.killed = true;
        }
        Ok(())
    }

    /// `populated 0`, read from the pinned control. Missing, malformed,
    /// ambiguous, oversized or unreadable state is an error, never emptiness.
    pub(crate) fn empty(&mut self) -> Result<bool, String> {
        self.events
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("cgroup events: {e}"))?;
        let mut text = String::new();
        (&mut self.events)
            .take(MAX_CONTROL_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("cgroup events: {e}"))?;
        if text.len() as u64 > MAX_CONTROL_BYTES {
            return Err(format!("cgroup events exceeded {MAX_CONTROL_BYTES} bytes"));
        }
        let mut populated = None;
        for line in text.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("populated") {
                continue;
            }
            let value = match words.next() {
                Some("0") => false,
                Some("1") => true,
                _ => return Err("cgroup has a malformed populated value".into()),
            };
            if words.next().is_some() || populated.replace(value).is_some() {
                return Err("cgroup has an ambiguous populated value".into());
            }
        }
        populated
            .map(|value| !value)
            .ok_or("cgroup has no populated value".into())
    }
}

/// Enable this cgroup's controllers for its children, and read back that they
/// took. Writing `cgroup.subtree_control` is silently partial when a controller
/// is unavailable, so the read is what makes the write a guarantee.
fn enable_controllers(path: &Path, name: &str) -> io::Result<()> {
    let mut request = String::new();
    for controller in CONTROLLERS {
        // `write!` to a String cannot fail; the result is consumed so the crate
        // keeps its no-unwrap rule without a discard that reads as an oversight.
        let _ = write!(request, "+{controller} ");
    }
    write_control(&path.join("cgroup.subtree_control"), request.trim_end())?;
    require_words(
        &read_control(&path.join("cgroup.subtree_control"))?,
        CONTROLLERS,
        &format!("{name} subtree control"),
    )
}

fn create_leaf(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let canonical = fs::canonicalize(path)?;
            let metadata = fs::symlink_metadata(path)?;
            if canonical == path && metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "cgroup {} is not a canonical directory",
                    path.display()
                )))
            }
        }
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("create cgroup {}: {error}", path.display()),
        )),
    }
}

fn require_cgroup2_mount() -> io::Result<()> {
    let mountinfo = read_bounded(Path::new("/proc/self/mountinfo"), 1024 * 1024)?;
    match cgroup_mount_filesystem(&mountinfo)? {
        Some("cgroup2") => Ok(()),
        Some(filesystem) => Err(io::Error::other(format!(
            "{ROOT} is mounted as {filesystem:?}, expected cgroup2"
        ))),
        None => Err(io::Error::other(format!("{ROOT} is not mounted"))),
    }
}

fn cgroup_mount_filesystem(text: &str) -> io::Result<Option<&str>> {
    for line in text.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            return Err(io::Error::other("mountinfo row lacks separator"));
        };
        let mut fields = before.split_ascii_whitespace();
        let mountpoint = fields.nth(4);
        let filesystem = after.split_ascii_whitespace().next();
        if mountpoint == Some(ROOT) {
            return Ok(filesystem);
        }
    }
    Ok(None)
}

fn require_empty(path: &Path, name: &str) -> io::Result<()> {
    let value = read_control(path)?;
    if value.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{name} has internal processes: {value:?}"
        )))
    }
}

fn require_words(text: &str, required: &[&str], name: &str) -> io::Result<()> {
    for required in required {
        if !text.split_ascii_whitespace().any(|word| word == *required) {
            return Err(io::Error::other(format!(
                "{name} lacks required controller {required:?}"
            )));
        }
    }
    Ok(())
}

fn write_control(path: &Path, value: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path).map_err(|error| {
        io::Error::new(error.kind(), format!("open {}: {error}", path.display()))
    })?;
    let command = format!("{value}\n");
    let written = file.write(command.as_bytes()).map_err(|error| {
        io::Error::new(error.kind(), format!("write {}: {error}", path.display()))
    })?;
    if written != command.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "write {} consumed {written} of {} bytes",
                path.display(),
                command.len()
            ),
        ));
    }
    Ok(())
}

fn set_owner(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    chown(path, Some(uid), Some(gid)).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("set owner on {}: {error}", path.display()),
        )
    })?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != uid || metadata.gid() != gid {
        return Err(io::Error::other(format!(
            "{} did not read back as owned by {uid}:{gid}",
            path.display()
        )));
    }
    Ok(())
}

fn read_control(path: &Path) -> io::Result<String> {
    Ok(read_bounded(path, MAX_CONTROL_BYTES)?.trim().to_string())
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!(
            "{} exceeds {limit} bytes",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::other(format!("{} is not UTF-8: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn controller_names_are_tokens() {
        require_words("cpu memory pids", &["cpu", "memory", "pids"], "controllers").unwrap();
        assert!(require_words("memoryish pids", &["memory"], "controllers").is_err());
    }

    /// `cgroup.procs` reads `0` as "the writing process". td-svc writing it
    /// would place ITSELF, and every service started afterwards would be forked
    /// inside one service's leaf and bounded by its limits. Refused before any
    /// I/O, so the check holds on a machine with no cgroupfs at all.
    #[test]
    fn only_a_real_child_is_placed() {
        let leaf = Path::new("/sys/fs/cgroup/system/nonexistent");
        for pid in [0, -1, -12345] {
            let error = place(leaf, pid).expect_err("a non-child pid was placed");
            assert!(
                error.to_string().contains("only a real child"),
                "pid {pid}: {error}"
            );
        }
    }

    /// An exited-but-unreaped child reads as finished.
    ///
    /// This is the discriminator `place` uses, and the one an earlier version
    /// got wrong: td-svc holds the `Child` until `watch` takes it, so a service
    /// that exits between `spawn` and placement is a ZOMBIE. A zombie passes
    /// the kernel's pid lookup — the `cgroup.procs` write does not answer ESRCH
    /// for one — so reading the errno off that write could never detect this
    /// case. Asking `/proc` does.
    #[test]
    fn a_child_that_exited_but_was_not_reaped_reads_as_finished() {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn");
        let pid = child.id() as i32;
        // Deliberately NOT reaped: that is the state place() sees.
        let mut zombie = false;
        for _ in 0..500 {
            if has_exited(pid) {
                zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(zombie, "pid {pid} never read as finished");
        // And it really is still an unreaped zombie, not a gone pid — so this
        // asserted the case place() actually meets.
        let stat = crate::procfs::stat_of(pid).expect("stat").expect("present");
        assert!(stat.zombie, "pid {pid} was not a zombie");
        let _ = child.wait();
    }

    /// A unit that wants a leaf but cannot name one is its OWN answer.
    ///
    /// Not `Elsewhere`: nothing was refused on this unit's behalf, so a limit
    /// on it would be lost, and the caller has to say so. `parse` makes this
    /// unreachable through the table, which is why it is asserted here.
    #[test]
    fn a_service_unit_that_cannot_name_a_leaf_is_not_mistaken_for_a_handoff() {
        let unit = Unit {
            name: "..".to_string(),
            cgroup: crate::table::Cgroup::Service,
            ..Unit::default()
        };
        assert!(matches!(leaf_for(&unit), Leaf::Unnamable));
    }

    /// A unit that does not own its leaf names none, so nothing builds a path
    /// from it. Both halves matter: the placement, and the name.
    #[test]
    fn a_unit_without_its_own_leaf_names_none() {
        let mut unit = Unit {
            name: "audio".to_string(),
            ..Unit::default()
        };
        assert_eq!(unit.cgroup_leaf_name(), Some("audio"));
        unit.cgroup = crate::table::Cgroup::Session;
        assert_eq!(unit.cgroup_leaf_name(), None);
        // A name that could traverse never becomes a path component, even if a
        // future parser change let one through.
        unit.cgroup = crate::table::Cgroup::Service;
        unit.name = "..".to_string();
        assert_eq!(unit.cgroup_leaf_name(), None);
    }

    /// An absent limit is written as the kernel's own default, not skipped.
    ///
    /// A leaf outlives the process in it and is reused across a restart, so
    /// skipping would leave a bound the table no longer carries still in force.
    /// This is the assertion that makes "an absent limit is unbounded" true.
    #[test]
    fn an_undeclared_limit_is_written_back_to_its_default() {
        let none = controls(&Limits::default());
        assert_eq!(none[0], ("memory.max", "max".to_string()));
        assert_eq!(none[1], ("pids.max", "max".to_string()));
        assert_eq!(none[2], ("cpu.weight", "100".to_string()));

        // A declared limit is written as itself, and the OTHERS still reset.
        let some = controls(&Limits {
            memory_max: Some(64 * 1024 * 1024),
            pids_max: None,
            cpu_weight: None,
        });
        assert_eq!(some[0], ("memory.max", "67108864".to_string()));
        assert_eq!(some[1], ("pids.max", "max".to_string()));
        assert_eq!(some[2], ("cpu.weight", "100".to_string()));
    }

    /// The membership a placed process reads back is the leaf path without the
    /// mount point — which is what `/proc/<pid>/cgroup` reports.
    #[test]
    fn membership_is_the_leaf_below_the_mount_point() {
        assert_eq!(
            membership_of(Path::new("/sys/fs/cgroup/system/audio")),
            "0::/system/audio"
        );
    }

    /// A unit that hands its processes away answers `Elsewhere` whether or not
    /// this machine has cgroupfs at all.
    ///
    /// The order of the two checks is the whole point: asking about the machine
    /// first would report a session unit as UNBOUNDED on a machine with no
    /// cgroupfs, and a session unit cannot declare a limit to lose.
    #[test]
    fn a_handoff_unit_is_elsewhere_before_the_machine_is_consulted() {
        let unit = Unit {
            name: "terminal".to_string(),
            cgroup: crate::table::Cgroup::Session,
            ..Unit::default()
        };
        assert!(matches!(leaf_for(&unit), Leaf::Elsewhere));
    }

    struct Controls(std::path::PathBuf);
    impl Controls {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            // Tests run in parallel, and a clock stamp alone can repeat.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "td-cgroup-controls-{}-{stamp}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("cgroup.kill"), "").unwrap();
            std::fs::write(path.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
            std::fs::write(path.join("cgroup.procs"), "").unwrap();
            Self(path)
        }
        fn events(&self, text: &str) {
            std::fs::write(self.0.join("cgroup.events"), text).unwrap();
        }
    }
    impl Drop for Controls {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cohort_does_not_claim_a_kill_is_emptiness() {
        let controls = Controls::new();
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("populated 1\nfrozen 0\n");
        cohort.kill().unwrap();
        cohort.kill().unwrap();
        assert_eq!(
            std::fs::read_to_string(controls.0.join("cgroup.kill")).unwrap(),
            "1"
        );
        assert!(!cohort.empty().unwrap());
        controls.events("populated 0\nfrozen 0\n");
        assert!(cohort.empty().unwrap());
    }

    #[test]
    fn cohort_refuses_unknown_state_and_occupied_launches() {
        let controls = Controls::new();
        for text in [
            "",
            "frozen 0\n",
            "populated 2\n",
            "populated 0\npopulated 0\n",
            "populated 0 extra\n",
            "populated 1\n",
        ] {
            controls.events(text);
            assert!(Cohort::open(&controls.0).is_err(), "{text:?}");
        }
        controls.events("populated 0\n");
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("frozen 0\n");
        assert!(cohort.empty().is_err());
        controls.events(&"x".repeat(4097));
        assert!(cohort.empty().is_err());
    }

    #[test]
    fn cohort_keeps_the_original_kernel_objects_after_path_replacement() {
        let controls = Controls::new();
        let mut cohort = Cohort::open(&controls.0).unwrap();
        controls.events("populated 1\n");
        std::fs::rename(
            controls.0.join("cgroup.events"),
            controls.0.join("old-events"),
        )
        .unwrap();
        controls.events("populated 0\n");
        assert!(!cohort.empty().unwrap());
    }

    /// A stop attaches to a leaf that is still running, which a launch must
    /// refuse; both then read the same population.
    #[test]
    fn a_stop_attaches_to_an_occupied_leaf_that_a_launch_refuses() {
        let controls = Controls::new();
        controls.events("populated 1\n");
        assert!(Cohort::open(&controls.0).is_err());
        let mut cohort = Cohort::attach(&controls.0).unwrap();
        assert!(!cohort.empty().unwrap());
        cohort.kill().unwrap();
        controls.events("populated 0\n");
        assert!(cohort.empty().unwrap());
        // Attaching needs both controls; a leaf missing one is no leaf to stop.
        std::fs::remove_file(controls.0.join("cgroup.kill")).unwrap();
        assert!(Cohort::attach(&controls.0).is_err());
    }

    /// A stop TERMs what the pinned `cgroup.procs` lists: whole, positive
    /// pids, the rest skipped and counted.
    #[test]
    fn a_leafs_members_are_read_whole_and_bounded() {
        let controls = Controls::new();
        let procs = controls.0.join("cgroup.procs");
        let mut cohort = Cohort::attach(&controls.0).unwrap();
        let found = |pids: Vec<i32>, skipped, cut| Members { pids, skipped, cut };
        assert_eq!(cohort.members().unwrap(), found(vec![], 0, false));
        std::fs::write(&procs, "12\n345\n").unwrap();
        assert_eq!(cohort.members().unwrap(), found(vec![12, 345], 0, false));
        // Past the bound: whole lines only, and said so.
        let line = "1234567\n";
        let count = (MAX_MEMBERS_BYTES as usize) / line.len() + 2;
        std::fs::write(&procs, line.repeat(count)).unwrap();
        let members = cohort.members().unwrap();
        assert!(members.cut);
        assert_eq!(
            members.pids.len(),
            (MAX_MEMBERS_BYTES as usize) / line.len()
        );
        assert!(members.pids.iter().all(|pid| *pid == 1_234_567));
        // An entry that is not a positive pid is skipped, not the whole list.
        std::fs::write(&procs, "x\n0\n7\n-1\n12 13\n8\n").unwrap();
        assert_eq!(cohort.members().unwrap(), found(vec![7, 8], 4, false));
        // A stop needs this control too.
        std::fs::remove_file(&procs).unwrap();
        assert!(Cohort::attach(&controls.0).is_err());
    }

    #[test]
    fn mountinfo_requires_the_exact_cgroup2_shape() {
        let good = "31 22 0:27 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw";
        let bad = "31 22 0:27 / /sys/fs/cgroup rw - tmpfs tmpfs rw";
        assert_eq!(cgroup_mount_filesystem(good).unwrap(), Some("cgroup2"));
        assert_eq!(cgroup_mount_filesystem(bad).unwrap(), Some("tmpfs"));
        assert_eq!(cgroup_mount_filesystem("").unwrap(), None);
        assert!(cgroup_mount_filesystem("malformed").is_err());
    }
}
