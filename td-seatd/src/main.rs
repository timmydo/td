#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, FileType, Metadata, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{self as unix_fs, DirEntryExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::{Duration, Instant};

const FRAMEBUFFER: &str = "/dev/fb0";
/// Where the DRM cards are. Optional: a machine whose only display is a legacy
/// firmware framebuffer has none.
const DRI_DIR: &str = "/dev/dri";
/// What the kernel says of each card's connectors.
const DRM_CLASS: &str = "/sys/class/drm";
/// The root-owned link naming the card the compositor is to drive.
const CARD_LINK: &str = "/run/td-seat-card";
/// Cards a walk considers. The DRM core numbers primary nodes below 64.
const MAX_CARDS: usize = 64;
const INPUT_DIR: &str = "/dev/input";
const SOUND_DIR: &str = "/dev/snd";
const MAX_PLAYBACK_DEVICES: usize = 64;
const RUNTIME_BASE: &str = "/run/user";
const AUDIO_RUNTIME: &str = "/run/td-audio";
const COMPOSITOR_RUNTIME_NAME: &str = "td-compositor";
const READY_MARKER: &str = "TD-SEAT-READY";
const PRIVATE_DEVICES_MARKER: &str = "TD-COMPOSITOR-DEVICES-PRIVATE";
/// The input core's character-device major (`INPUT_MAJOR`).
const INPUT_MAJOR: u64 = 13;
/// How often `watch` reads `/dev/input` (td-compositor/DESIGN.md, "Input
/// hotplug").
const WATCH_PERIOD: Duration = Duration::from_millis(250);
/// `/dev/input` entries a watch tick handles, in read order: the input
/// core allots at most 1024 character devices.
const INPUT_DIRECTORY_ENTRIES: usize = 2048;

fn usage() -> String {
    "usage: td-seatd assign --uid UID --gid GID --compositor-uid UID --audio-uid UID --audio-gid GID | \
     td-seatd probe --uid UID --gid GID --compositor-uid UID --audio-uid UID --audio-gid GID | \
     td-seatd probe-access --uid UID --gid GID --compositor-uid UID --audio-uid UID --audio-gid GID | \
     td-seatd watch --uid UID --gid GID --compositor-uid UID --audio-uid UID --audio-gid GID | \
     td-seatd exec-audio --uid UID --gid GID --compositor-uid UID --audio-uid UID --audio-gid GID \
     -- PROGRAM [ARG...] | \
     td-seatd selftest"
        .into()
}

#[derive(Clone, Copy)]
struct Account {
    uid: u32,
    gid: u32,
}

#[derive(Clone, Copy)]
struct Assignment {
    seat: Account,
    compositor: Account,
    audio: Account,
}

struct AudioExec<'a> {
    assignment: Assignment,
    program: &'a str,
    arguments: &'a [String],
}

fn parse_assignment(args: &[String]) -> Result<Assignment, String> {
    let mut uid = None;
    let mut gid = None;
    let mut compositor_uid = None;
    let mut audio_uid = None;
    let mut audio_gid = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args
            .get(index)
            .ok_or_else(|| "missing account flag".to_string())?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--uid" if uid.is_none() => {
                uid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid uid '{value}'"))?,
                );
            }
            "--gid" if gid.is_none() => {
                gid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid gid '{value}'"))?,
                );
            }
            "--compositor-uid" if compositor_uid.is_none() => {
                let service_uid = value.parse::<u32>().map_err(|_| "invalid compositor uid")?;
                if !(1..=999).contains(&service_uid) || service_uid.to_string() != *value {
                    return Err("compositor uid must be a canonical service identity".into());
                }
                compositor_uid = Some(service_uid);
            }
            "--audio-uid" if audio_uid.is_none() => {
                audio_uid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid audio uid '{value}'"))?,
                );
            }
            "--audio-gid" if audio_gid.is_none() => {
                audio_gid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid audio gid '{value}'"))?,
                );
            }
            "--uid" | "--gid" | "--compositor-uid" | "--audio-uid" | "--audio-gid" => {
                return Err(format!("duplicate flag '{flag}'"))
            }
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    let uid = uid.ok_or_else(|| "--uid is required".to_string())?;
    let gid = gid.ok_or_else(|| "--gid is required".to_string())?;
    let compositor_uid = compositor_uid.ok_or("--compositor-uid is required")?;
    let audio_uid = audio_uid.ok_or_else(|| "--audio-uid is required".to_string())?;
    let audio_gid = audio_gid.ok_or_else(|| "--audio-gid is required".to_string())?;
    if uid == 0 || gid == 0 || audio_uid == 0 || audio_gid == 0 {
        return Err("the graphical and audio identities must not use uid or gid 0".into());
    }
    if uid == audio_uid
        || gid == audio_gid
        || uid == compositor_uid
        || gid == compositor_uid
        || audio_uid == compositor_uid
        || audio_gid == compositor_uid
    {
        return Err("the human, compositor and audio identities must be distinct".into());
    }
    Ok(Assignment {
        seat: Account { uid, gid },
        compositor: Account {
            uid: compositor_uid,
            gid: compositor_uid,
        },
        audio: Account {
            uid: audio_uid,
            gid: audio_gid,
        },
    })
}

fn parse_audio_exec(args: &[String]) -> Result<AudioExec<'_>, String> {
    let separator = args
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "exec-audio requires `--` before the program".to_string())?;
    let assignment = parse_assignment(args.get(..separator).ok_or_else(usage)?)?;
    let command = args
        .get(separator + 1..)
        .ok_or_else(|| "exec-audio requires a program after `--`".to_string())?;
    let (program, arguments) = command
        .split_first()
        .ok_or_else(|| "exec-audio requires a program after `--`".to_string())?;
    if !program.starts_with('/') || program.ends_with('/') {
        return Err("exec-audio requires an absolute program path with a basename".into());
    }
    Ok(AudioExec {
        assignment,
        program,
        arguments,
    })
}

fn event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn input_paths(input_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(input_dir)
        .map_err(|e| format!("read input directory {}: {e}", input_dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read input directory entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if event_name(name) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(format!(
            "{} contains no evdev event nodes",
            input_dir.display()
        ));
    }
    Ok(paths)
}

/// Only the named td channel is delegated; other virtio ports stay private
/// to their original owner. Sysfs is kernel-owned, /dev is root-owned.
fn vm_port(sys: &Path, dev: &Path) -> Result<Option<PathBuf>, String> {
    let entries = match fs::read_dir(sys) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("inspect VM ports: {e}")),
    };
    let mut found = None;
    for (index, entry) in entries.enumerate() {
        if index >= 64 {
            return Err("too many virtio ports".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name
            .strip_prefix("vport")
            .and_then(|s| s.split_once('p'))
            .is_some_and(|(a, b)| decimal_component(a) && decimal_component(b))
        {
            continue;
        }
        let mut value = String::new();
        let file = match File::open(entry.path().join("name")) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("read virtio port name: {e}")),
        };
        file.take(129)
            .read_to_string(&mut value)
            .map_err(|e| e.to_string())?;
        if value == "org.td.vm.1\n" {
            if found.is_some() {
                return Err("duplicate td VM bridge ports".into());
            }
            found = Some(dev.join(name));
        }
    }
    Ok(found)
}

fn wait_vm_port(sys: &Path, dev: &Path, timeout: Duration) -> Result<Option<PathBuf>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = vm_port(sys, dev)?;
        if found.is_some() || Instant::now() >= deadline {
            return Ok(found);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assign_vm_port(runtime: &Path, account: Account) -> Result<(), String> {
    // The kernel receives a port's name after probing its device.
    let result = wait_vm_port(
        Path::new("/sys/class/virtio-ports"),
        Path::new("/dev"),
        Duration::from_secs(2),
    )
    .and_then(|path| assign_vm_port_path(runtime, account, path.as_deref(), true));
    optional_vm_assignment(runtime, account, result)
}

fn optional_vm_assignment(
    runtime: &Path,
    account: Account,
    result: Result<(), String>,
) -> Result<(), String> {
    if let Err(error) = result {
        // Disable the optional channel before admitting the ordinary desktop.
        assign_vm_port_path(runtime, account, None, true)?;
        eprintln!("td-seatd: optional VM bridge unavailable: {error}");
    }
    Ok(())
}

fn assign_vm_port_path(
    runtime: &Path,
    account: Account,
    path: Option<&Path>,
    require_char: bool,
) -> Result<(), String> {
    let record = runtime.join("vm-port");
    let Some(path) = path else {
        return match fs::remove_file(&record) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("clear VM port assignment: {e}")),
        };
    };
    let existing = match fs::symlink_metadata(&record) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("inspect VM port assignment: {e}")),
    };
    if existing {
        checked_metadata(&record, false)?;
        verify_owner_mode(&record, account, 0o600)?;
        let mut current = Vec::new();
        File::open(&record)
            .map_err(|e| e.to_string())?
            .take(129)
            .read_to_end(&mut current)
            .map_err(|e| e.to_string())?;
        if current == path.as_os_str().as_encoded_bytes() {
            return assign_path(path, account, require_char);
        }
        return Err("VM port assignment changed; restart the deployment".into());
    }
    assign_path(path, account, require_char)?;
    let mut options = fs::OpenOptions::new();
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&record)
        .map_err(|e| format!("publish VM port assignment: {e}"))?;
    file.write_all(path.as_os_str().as_encoded_bytes())
        .map_err(|e| e.to_string())?;
    assign_path(&record, account, false)
}

fn decimal_component(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// The playback PCM nodes `td-audio` is allowed to open.
///
/// Control, sequencer, timer and capture nodes stay root-owned. The daemon
/// implements volume in its mixer and v1 exposes no capture protocol, so
/// granting those devices would be authority with no supported use.
fn playback_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("pcmC") else {
        return false;
    };
    let Some((card, rest)) = rest.split_once('D') else {
        return false;
    };
    let Some(device) = rest.strip_suffix('p') else {
        return false;
    };
    decimal_component(card) && decimal_component(device)
}

fn playback_paths(sound_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(sound_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "read sound directory {}: {error}",
                sound_dir.display()
            ));
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read sound directory entry: {error}"))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(playback_name) {
            if paths.len() >= MAX_PLAYBACK_DEVICES {
                return Err(format!(
                    "{} has more than {MAX_PLAYBACK_DEVICES} playback devices",
                    sound_dir.display()
                ));
            }
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn checked_metadata(path: &Path, require_char: bool) -> Result<Metadata, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let kind: FileType = metadata.file_type();
    if kind.is_symlink() {
        return Err(format!("refusing symlink {}", path.display()));
    }
    if require_char && !kind.is_char_device() {
        return Err(format!("{} is not a character device", path.display()));
    }
    if !require_char && !kind.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(metadata)
}

fn assign_path(path: &Path, account: Account, require_char: bool) -> Result<(), String> {
    checked_metadata(path, require_char)?;
    unix_fs::lchown(path, Some(account.uid), Some(account.gid))
        .map_err(|e| format!("lchown {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    verify_owner_mode(path, account, 0o600)
}

/// A device number's major and minor, as glibc's `gnu_dev_major` and
/// `gnu_dev_minor` split it.
fn device_major(device: u64) -> u64 {
    ((device >> 8) & 0xfff) | ((device >> 32) & !0xfff)
}

fn device_minor(device: u64) -> u64 {
    (device & 0xff) | ((device >> 12) & 0xffff_ff00)
}

/// Refuses a symlink or anything but an input core character device, and
/// changes nothing. `require_char: false` is the scratch-tree tests'
/// regular-file seam, and the only path that skips the major check.
fn input_node(path: &Path, require_char: bool) -> Result<Metadata, String> {
    let metadata = checked_metadata(path, require_char)?;
    let major = device_major(metadata.rdev());
    if require_char && major != INPUT_MAJOR {
        return Err(format!(
            "{} is character major {major}, not an input device",
            path.display()
        ));
    }
    Ok(metadata)
}

/// The boot node assignment, shared by `assign` and `watch`: `input_node`,
/// then `lchown` to the compositor, `chmod` 0600 and verify. Answers the
/// metadata the check passed.
fn assign_input(path: &Path, account: Account, require_char: bool) -> Result<Metadata, String> {
    let node = input_node(path, require_char)?;
    assign_path(path, account, require_char)?;
    Ok(node)
}

/// Whether an input node already has the boot assignment.
fn assigned_input(metadata: &Metadata, account: Account, require_char: bool) -> bool {
    let kind = metadata.file_type();
    let node = if require_char {
        kind.is_char_device() && device_major(metadata.rdev()) == INPUT_MAJOR
    } else {
        kind.is_file()
    };
    node && metadata.uid() == account.uid
        && metadata.gid() == account.gid
        && metadata.permissions().mode() & 0o7777 == 0o600
}

/// What the watch last found of `/dev/input` itself.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum Listing {
    #[default]
    Read,
    Unreadable,
    Overflowing,
}

/// `td-seatd watch`'s state: the inodes whose assignment and whose failure
/// it has said, pruned each tick to those still present, and the listing
/// it last found, said on each change to a failure.
#[derive(Default)]
struct Watch {
    assigned: BTreeSet<u64>,
    reported: BTreeSet<u64>,
    listing: Listing,
}

impl Watch {
    /// One tick: gives each `eventN` node in `input_dir` that is not
    /// already the compositor's the boot assignment. Answers what it
    /// assigned and what failed, each said once per inode.
    fn tick(
        &mut self,
        input_dir: &Path,
        account: Account,
        require_char: bool,
    ) -> (Vec<String>, Vec<String>) {
        let mut assigned = Vec::new();
        let mut said = Vec::new();
        let entries = match fs::read_dir(input_dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.found(Listing::Unreadable, &mut said, || {
                    format!("read {}: {error}", input_dir.display())
                });
                return (assigned, said);
            }
        };
        let mut present = BTreeSet::new();
        let mut listing = Listing::Read;
        let mut failure = String::new();
        for (index, entry) in entries.enumerate() {
            if index >= INPUT_DIRECTORY_ENTRIES {
                listing = Listing::Overflowing;
                failure = format!(
                    "{} has more than {INPUT_DIRECTORY_ENTRIES} entries; handling the first",
                    input_dir.display()
                );
                break;
            }
            // std ends the listing after an error, so the rest waits.
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    listing = Listing::Unreadable;
                    failure = format!("read {}: {error}", input_dir.display());
                    break;
                }
            };
            if !entry.file_name().to_str().is_some_and(event_name) {
                continue;
            }
            let path = entry.path();
            let inode = entry.ino();
            present.insert(inode);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if assigned_input(&metadata, account, require_char) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => {}
            }
            match assign_input(&path, account, require_char) {
                Ok(node) => {
                    if self.assigned.insert(inode) {
                        assigned.push(format!(
                            "assigned {}, device {}:{}, inode {}",
                            path.display(),
                            device_major(node.rdev()),
                            device_minor(node.rdev()),
                            node.ino()
                        ));
                    }
                }
                // Removed meanwhile, or assigned after all: nothing to say.
                Err(_) if settled(&path, account, require_char) => {}
                Err(error) => {
                    if self.reported.insert(inode) {
                        said.push(error);
                    }
                }
            }
        }
        self.found(listing, &mut said, || failure);
        // A cut listing is no evidence of what is gone.
        if listing == Listing::Read {
            self.assigned.retain(|inode| present.contains(inode));
            self.reported.retain(|inode| present.contains(inode));
        }
        (assigned, said)
    }

    /// Records the listing found, saying a failure the first tick it is
    /// found after any other.
    fn found(&mut self, listing: Listing, said: &mut Vec<String>, say: impl FnOnce() -> String) {
        if listing != Listing::Read && listing != self.listing {
            said.push(say());
        }
        self.listing = listing;
    }
}

/// Whether a node whose assignment failed is gone or assigned after all.
fn settled(path: &Path, account: Account, require_char: bool) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => assigned_input(&metadata, account, require_char),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// `td-seatd watch` (td-compositor/DESIGN.md, "Input hotplug"): every
/// 250 ms, the boot node assignment for each event node that appears
/// after the `[seat]` oneshot. Nothing it meets ends it.
fn watch(assignment: Assignment) -> Result<(), String> {
    let mut state = Watch::default();
    loop {
        let (assigned, said) = state.tick(Path::new(INPUT_DIR), assignment.compositor, true);
        for line in assigned.into_iter().chain(said) {
            let _ = writeln!(std::io::stderr().lock(), "td-seatd: watch: {line}");
        }
        std::thread::sleep(WATCH_PERIOD);
    }
}

fn verify_owner_mode(path: &Path, account: Account, mode: u32) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let actual_mode = metadata.permissions().mode() & 0o7777;
    if metadata.uid() != account.uid || metadata.gid() != account.gid || actual_mode != mode {
        return Err(format!(
            "{} is {:04o} {}:{}, expected {:04o} {}:{}",
            path.display(),
            actual_mode,
            metadata.uid(),
            metadata.gid(),
            mode,
            account.uid,
            account.gid
        ));
    }
    Ok(())
}

fn verify_runtime_base(path: &Path, require_root: bool) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode != 0o755 {
        return Err(format!("{} is {mode:04o}, expected 0755", path.display()));
    }
    if require_root && (metadata.uid() != 0 || metadata.gid() != 0) {
        return Err(format!(
            "{} is owned by {}:{}, expected root:root",
            path.display(),
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

fn prepare_runtime_base(path: &Path, require_root: bool) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(format!("{} exists but is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|create| format!("mkdir {}: {create}", path.display()))?;
        }
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    }
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    verify_runtime_base(path, require_root)
}

fn prepare_owned_runtime(path: &Path, account: Account, mode: u32) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(format!("{} exists but is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|create| format!("mkdir {}: {create}", path.display()))?;
        }
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    }
    unix_fs::lchown(path, Some(account.uid), Some(account.gid))
        .map_err(|e| format!("lchown {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    verify_owner_mode(path, account, mode)
}

fn prepare_runtime(path: &Path, account: Account, require_root_base: bool) -> Result<(), String> {
    let base = path
        .parent()
        .ok_or_else(|| format!("runtime path {} has no parent", path.display()))?;
    prepare_runtime_base(base, require_root_base)?;
    prepare_owned_runtime(path, account, 0o700)
}

fn prepare_audio_runtime(
    path: &Path,
    account: Account,
    require_root_base: bool,
) -> Result<(), String> {
    let base = path
        .parent()
        .ok_or_else(|| format!("audio runtime path {} has no parent", path.display()))?;
    verify_runtime_base(base, require_root_base)?;
    prepare_owned_runtime(path, account, 0o755)
}

fn compositor_runtime(human_runtime: &Path) -> Result<PathBuf, String> {
    shared_runtime(human_runtime, COMPOSITOR_RUNTIME_NAME)
}

fn shared_runtime(human_runtime: &Path, name: &str) -> Result<PathBuf, String> {
    let owner = human_runtime
        .file_name()
        .ok_or("human runtime has no owner")?;
    let run = human_runtime
        .parent()
        .and_then(Path::parent)
        .ok_or("human runtime has no runtime root")?;
    Ok(run.join(name).join(owner))
}

fn prepare_compositor_runtime(
    path: &Path,
    account: Account,
    require_root: bool,
) -> Result<(), String> {
    let base = path.parent().ok_or("compositor runtime has no parent")?;
    let run = base
        .parent()
        .ok_or("compositor runtime has no runtime root")?;
    verify_runtime_base(run, require_root)?;
    prepare_runtime_base(base, require_root)?;
    prepare_owned_runtime(path, account, 0o755)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    card: bool,
    inputs: usize,
    playback: usize,
}

/// The display nodes the compositor is given: the framebuffer it draws
/// through today, and the DRM card behind it when there is one.
#[derive(Clone, Copy)]
struct Display<'a> {
    framebuffer: &'a Path,
    card: &'a Path,
}

/// Whether an optional device node exists. Only absence is optional; any
/// other lookup failure refuses, as a required node's would.
fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("stat {}: {e}", path.display())),
    }
}

/// Each `cardN` node under `dri`, in number order.
fn card_nodes(dri: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match fs::read_dir(dri) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", dri.display())),
    };
    let mut cards: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dri.display()))?;
        let number = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("card"))
            .filter(|digits| {
                digits.bytes().all(|byte| byte.is_ascii_digit())
                    && (*digits == "0" || !digits.starts_with('0'))
            })
            .and_then(|digits| digits.parse().ok());
        if let Some(number) = number {
            cards.push((number, entry.path()));
        }
    }
    cards.sort();
    cards.truncate(MAX_CARDS);
    Ok(cards.into_iter().map(|(_, path)| path).collect())
}

/// Whether any of a card's connectors reports a display.
fn has_connected_output(class: &Path, card: &str) -> bool {
    let prefix = format!("{card}-");
    let Ok(entries) = fs::read_dir(class) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().starts_with(&prefix)
            && fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim_end() == "connected")
    })
}

/// The card to drive: the lowest-numbered one with a display on it, else the
/// lowest-numbered one. A machine's first card need not have the panel: a
/// discrete GPU can probe first, and simpledrm can leave a native driver's card
/// above a number it no longer holds.
fn select_card(dri: &Path, class: &Path) -> Result<Option<PathBuf>, String> {
    let cards = card_nodes(dri)?;
    let named = |card: &&PathBuf| {
        card.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| has_connected_output(class, name))
    };
    Ok(cards.iter().find(named).or(cards.first()).cloned())
}

/// Name the selected card at `link`, or leave nothing there for a machine with
/// none, which is what sends the compositor to the framebuffer. The rename means
/// the link is never briefly absent, which would read as having no card.
fn publish_card(link: &Path, card: Option<&Path>) -> Result<(), String> {
    let Some(card) = card else {
        return match fs::remove_file(link) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", link.display())),
        };
    };
    let building = link.with_extension("new");
    match fs::remove_file(&building) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("remove {}: {e}", building.display())),
    }
    unix_fs::symlink(card, &building).map_err(|e| format!("link {}: {e}", building.display()))?;
    fs::rename(&building, link).map_err(|e| format!("rename to {}: {e}", link.display()))
}

/// The card `assign` published, which is the one it granted.
fn published_card(link: &Path) -> Result<Option<PathBuf>, String> {
    match fs::read_link(link) {
        Ok(card) => Ok(Some(card)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", link.display())),
    }
}

fn assign(
    display: Display<'_>,
    input_dir: &Path,
    sound_dir: &Path,
    runtime: &Path,
    audio_runtime: &Path,
    assignment: Assignment,
    require_char: bool,
) -> Result<Counts, String> {
    prepare_runtime(runtime, assignment.seat, require_char)?;
    prepare_compositor_runtime(
        &compositor_runtime(runtime)?,
        assignment.compositor,
        require_char,
    )?;
    prepare_compositor_runtime(
        &shared_runtime(runtime, "td-guest")?,
        assignment.seat,
        require_char,
    )?;
    prepare_audio_runtime(audio_runtime, assignment.audio, require_char)?;
    assign_path(display.framebuffer, assignment.compositor, require_char)?;
    let card = if present(display.card)? {
        assign_path(display.card, assignment.compositor, require_char)?;
        true
    } else {
        false
    };
    let inputs = input_paths(input_dir)?;
    for path in &inputs {
        assign_input(path, assignment.compositor, require_char)?;
    }
    let playback = assign_playback(sound_dir, assignment.audio, require_char)?;
    Ok(Counts {
        card,
        inputs: inputs.len(),
        playback: playback.len(),
    })
}

fn assign_playback(
    sound_dir: &Path,
    account: Account,
    require_char: bool,
) -> Result<Vec<PathBuf>, String> {
    let playback = playback_paths(sound_dir)?;
    for path in &playback {
        assign_path(path, account, require_char)?;
    }
    Ok(playback)
}

fn exec_audio(args: &[String]) -> Result<(), String> {
    let options = parse_audio_exec(args)?;
    let _assigned = assign_playback(Path::new(SOUND_DIR), options.assignment.audio, true)?;
    let error = Command::new(options.program).args(options.arguments).exec();
    Err(format!("exec {}: {error}", options.program))
}

fn probe(
    display: Display<'_>,
    input_dir: &Path,
    sound_dir: &Path,
    runtime: &Path,
    audio_runtime: &Path,
    assignment: Assignment,
    require_char: bool,
) -> Result<Counts, String> {
    verify_owner_mode(runtime, assignment.seat, 0o700)?;
    let compositor_runtime = compositor_runtime(runtime)?;
    verify_runtime_base(
        compositor_runtime
            .parent()
            .ok_or("missing compositor parent")?,
        require_char,
    )?;
    verify_owner_mode(&compositor_runtime, assignment.compositor, 0o755)?;
    let vm_runtime = shared_runtime(runtime, "td-guest")?;
    verify_runtime_base(
        vm_runtime.parent().ok_or("missing VM parent")?,
        require_char,
    )?;
    verify_owner_mode(&vm_runtime, assignment.seat, 0o755)?;
    verify_owner_mode(audio_runtime, assignment.audio, 0o755)?;
    checked_metadata(display.framebuffer, require_char)?;
    verify_owner_mode(display.framebuffer, assignment.compositor, 0o600)?;
    let card = if present(display.card)? {
        checked_metadata(display.card, require_char)?;
        verify_owner_mode(display.card, assignment.compositor, 0o600)?;
        true
    } else {
        false
    };
    let inputs = input_paths(input_dir)?;
    for path in &inputs {
        input_node(path, require_char)?;
        verify_owner_mode(path, assignment.compositor, 0o600)?;
    }
    let playback = playback_paths(sound_dir)?;
    for path in &playback {
        checked_metadata(path, require_char)?;
        verify_owner_mode(path, assignment.audio, 0o600)?;
    }
    Ok(Counts {
        card,
        inputs: inputs.len(),
        playback: playback.len(),
    })
}

fn require_human_process(status: &str, account: Account) -> Result<(), String> {
    if status.len() > 8192 {
        return Err("seat access probe status exceeds 8192 bytes".into());
    }
    if account.uid == 0 || account.gid == 0 {
        return Err("seat access probe requires the unprivileged human identity".into());
    }
    for (key, expected) in [("Uid:", account.uid), ("Gid:", account.gid)] {
        let columns = status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .ok_or("seat access probe has no kernel credentials")?;
        let fields: Vec<_> = columns.split_whitespace().collect();
        let expected = expected.to_string();
        if fields.len() != 4 || fields.iter().any(|field| *field != expected) {
            return Err("seat access probe has the wrong kernel credentials".into());
        }
    }
    Ok(())
}

fn denied_device_read(path: &Path) -> Result<(), String> {
    match File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(error) => Err(format!(
            "seat access probe could not test {}: {error}",
            path.display()
        )),
        Ok(_) => Err(format!(
            "human session can read compositor device {}",
            path.display()
        )),
    }
}

fn backlight_access(command: &str, account: Account) -> Result<(), String> {
    let Some(panel) = td_backlight::discover().map_err(|e| e.to_string())? else {
        return Ok(());
    };
    assign_backlight(&panel, command, account)
}

fn assign_backlight(
    panel: &td_backlight::Panel,
    command: &str,
    account: Account,
) -> Result<(), String> {
    let path = panel.brightness_path();
    checked_metadata(&path, false)?;
    if command == "assign" {
        unix_fs::lchown(&path, Some(account.uid), Some(account.gid)).map_err(|e| e.to_string())?;
        fs::set_permissions(&path, Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
    }
    verify_owner_mode(&path, account, 0o644)?;
    let _ = writeln!(
        std::io::stderr(),
        "td-seatd: backlight {} writable only by compositor uid {}",
        panel.directory().display(),
        account.uid
    );
    Ok(())
}

fn probe_access(assignment: Assignment) -> Result<(), String> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .map_err(|e| e.to_string())?
        .take(8193)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    require_human_process(&status, assignment.seat)?;
    denied_device_read(Path::new(FRAMEBUFFER))?;
    for card in card_nodes(Path::new(DRI_DIR))? {
        denied_device_read(&card)?;
    }
    for path in input_paths(Path::new(INPUT_DIR))? {
        denied_device_read(&path)?;
    }
    match vm_port(Path::new("/sys/class/virtio-ports"), Path::new("/dev")) {
        Ok(Some(path)) => match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => denied_device_read(&path)?,
        },
        Ok(None) => (),
        Err(error) => eprintln!("td-seatd: optional VM bridge privacy probe unavailable: {error}"),
    }
    writeln!(std::io::stdout().lock(), "\n{PRIVATE_DEVICES_MARKER}").map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    if command == "selftest" {
        if args.get(1).is_some() {
            return Err(usage());
        }
        let assignment = |uid: &str, gid: &str, audio_uid: &str, audio_gid: &str| {
            parse_assignment(&[
                "--uid".into(),
                uid.into(),
                "--gid".into(),
                gid.into(),
                "--compositor-uid".into(),
                "993".into(),
                "--audio-uid".into(),
                audio_uid.into(),
                "--audio-gid".into(),
                audio_gid.into(),
            ])
        };
        if !event_name("event0")
            || event_name("event")
            || assignment("1000", "1000", "994", "994").is_err()
            || parse_assignment(&[
                "--uid".into(),
                "1000".into(),
                "--gid".into(),
                "1000".into(),
                "--audio-gid".into(),
                "994".into(),
            ])
            .is_ok()
            || parse_assignment(&[
                "--uid".into(),
                "1000".into(),
                "--gid".into(),
                "1000".into(),
                "--audio-uid".into(),
                "994".into(),
            ])
            .is_ok()
            || [
                ("0", "1000", "994", "994"),
                ("1000", "0", "994", "994"),
                ("1000", "1000", "0", "994"),
                ("1000", "1000", "994", "0"),
                ("1000", "1000", "1000", "994"),
                ("1000", "1000", "994", "1000"),
            ]
            .into_iter()
            .any(|(uid, gid, audio_uid, audio_gid)| {
                assignment(uid, gid, audio_uid, audio_gid).is_ok()
            })
        {
            return Err("seat parser selftest failed".into());
        }
        println!("TD-SEAT-SELFTEST-OK");
        return Ok(());
    }
    if command == "exec-audio" {
        return exec_audio(args.get(1..).ok_or_else(usage)?);
    }
    let assignment = parse_assignment(args.get(1..).ok_or_else(usage)?)?;
    if command == "probe-access" {
        return probe_access(assignment);
    }
    if command == "watch" {
        return watch(assignment);
    }
    let runtime = PathBuf::from(RUNTIME_BASE).join(assignment.seat.uid.to_string());
    // Selection is `assign`'s, once a boot. A later card or display does not move
    // it, and `probe` checks what was granted rather than choosing again.
    let selected = if command == "assign" {
        select_card(Path::new(DRI_DIR), Path::new(DRM_CLASS))?
    } else {
        published_card(Path::new(CARD_LINK))?
    };
    // With none, a path that is absent for that very reason, so `assign` and
    // `probe` read no card even if one has registered since.
    let card = selected.clone().unwrap_or_else(|| PathBuf::from(CARD_LINK));
    let counts = match command.as_str() {
        "assign" => assign(
            Display {
                framebuffer: Path::new(FRAMEBUFFER),
                card: &card,
            },
            Path::new(INPUT_DIR),
            Path::new(SOUND_DIR),
            &runtime,
            Path::new(AUDIO_RUNTIME),
            assignment,
            true,
        )?,
        "probe" => probe(
            Display {
                framebuffer: Path::new(FRAMEBUFFER),
                card: &card,
            },
            Path::new(INPUT_DIR),
            Path::new(SOUND_DIR),
            &runtime,
            Path::new(AUDIO_RUNTIME),
            assignment,
            true,
        )?,
        _ => return Err(usage()),
    };
    if command == "assign" {
        publish_card(Path::new(CARD_LINK), selected.as_deref())?;
        assign_vm_port(&compositor_runtime(&runtime)?, assignment.compositor)?;
    }
    if let Err(error) = backlight_access(command, assignment.compositor) {
        let _ = writeln!(
            std::io::stderr(),
            "td-seatd: optional panel backlight unavailable: {error}"
        );
    }
    println!(
        "{READY_MARKER} uid={} gid={} framebuffer={} inputs={} runtime={} \
         compositor-uid={} compositor-gid={} compositor-runtime={} \
         audio-uid={} audio-gid={} audio-runtime={AUDIO_RUNTIME} audio-pcms={} card={}",
        assignment.seat.uid,
        assignment.seat.gid,
        FRAMEBUFFER,
        counts.inputs,
        runtime.display(),
        assignment.compositor.uid,
        assignment.compositor.gid,
        compositor_runtime(&runtime)?.display(),
        assignment.audio.uid,
        assignment.audio.gid,
        counts.playback,
        if counts.card {
            card.to_string_lossy()
        } else {
            "none".into()
        },
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("td-seatd: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "td-seatd-test-{}-{}",
                process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
            Scratch { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn only_the_selected_brightness_attribute_is_delegated() {
        let root = td_fs::TempDir::new("td-seat-backlight").unwrap();
        let devices = root.path().join("devices");
        let class = root.path().join("class");
        let device = devices.join("intel_backlight");
        fs::create_dir_all(&device).unwrap();
        fs::create_dir(&class).unwrap();
        unix_fs::symlink(&device, class.join("intel_backlight")).unwrap();
        for (name, value) in [
            ("brightness", "1"),
            ("max_brightness", "100"),
            ("type", "raw"),
        ] {
            fs::write(device.join(name), value).unwrap();
            fs::set_permissions(device.join(name), Permissions::from_mode(0o600)).unwrap();
        }
        let identity = fs::metadata(root.path()).unwrap();
        let account = Account {
            uid: identity.uid(),
            gid: identity.gid(),
        };
        let panel = td_backlight::discover_in(&class, &devices)
            .unwrap()
            .unwrap();
        assign_backlight(&panel, "assign", account).unwrap();
        assign_backlight(&panel, "probe", account).unwrap();
        assert_eq!(
            fs::metadata(device.join("brightness")).unwrap().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(device.join("max_brightness")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(device.join("type")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn access_evidence_checks_kernel_denial_without_requiring_live_assignment() {
        let source = include_str!("main.rs");
        let body = source
            .split("fn probe_access(")
            .nth(1)
            .unwrap()
            .split("fn run(")
            .next()
            .unwrap();
        assert!(body.contains("require_human_process(&status, assignment.seat)?"));
        assert!(body.contains("denied_device_read(Path::new(FRAMEBUFFER))?"));
        assert!(body.contains(
            "for card in card_nodes(Path::new(DRI_DIR))? {\n        denied_device_read(&card)?;\n    }"
        ));
        assert!(body.contains("input_paths(Path::new(INPUT_DIR))?"));
        assert!(body.contains("denied_device_read(&path)?"));
        assert!(!body.contains("probe("));
        assert!(!body.contains("verify_owner_mode"));
    }

    fn drm_fixture(cards: &[&str], connected: &[&str]) -> td_fs::TempDir {
        let root = td_fs::TempDir::new("td-seatd-cards").unwrap();
        let dri = root.path().join("dri");
        let class = root.path().join("class");
        fs::create_dir_all(&dri).unwrap();
        fs::create_dir_all(&class).unwrap();
        fs::write(dri.join("renderD128"), b"").unwrap();
        for card in cards {
            fs::write(dri.join(card), b"").unwrap();
            let output = class.join(format!("{card}-eDP-1"));
            fs::create_dir_all(&output).unwrap();
            let status = if connected.contains(card) {
                "connected\n"
            } else {
                "disconnected\n"
            };
            fs::write(output.join("status"), status).unwrap();
        }
        root
    }

    fn selected(root: &td_fs::TempDir) -> Option<String> {
        select_card(&root.path().join("dri"), &root.path().join("class"))
            .unwrap()
            .map(|card| card.file_name().unwrap().to_string_lossy().into_owned())
    }

    #[test]
    fn the_card_with_a_display_is_the_one_driven() {
        let hybrid = drm_fixture(&["card0", "card1"], &["card1"]);
        assert_eq!(selected(&hybrid).as_deref(), Some("card1"));
        let both = drm_fixture(&["card0", "card1"], &["card0", "card1"]);
        assert_eq!(selected(&both).as_deref(), Some("card0"));
        // Number order, not name order: card10 sorts before card2 by name.
        let ordered = drm_fixture(&["card10", "card2"], &["card10", "card2"]);
        assert_eq!(selected(&ordered).as_deref(), Some("card2"));
    }

    #[test]
    fn a_card_with_nothing_connected_is_still_a_card() {
        let dark = drm_fixture(&["card1", "card0"], &[]);
        assert_eq!(selected(&dark).as_deref(), Some("card0"));
        let none = drm_fixture(&[], &[]);
        assert_eq!(selected(&none), None);
        let root = td_fs::TempDir::new("td-seatd-no-dri").unwrap();
        assert_eq!(
            select_card(&root.path().join("absent"), &root.path().join("absent")).unwrap(),
            None
        );
    }

    #[test]
    fn a_connector_of_another_card_says_nothing_of_this_one() {
        // card1's connector is not card10's.
        let root = drm_fixture(&["card1", "card10"], &["card10"]);
        assert_eq!(selected(&root).as_deref(), Some("card10"));
    }

    #[test]
    fn the_link_follows_the_selection_and_is_absent_without_a_card() {
        let root = td_fs::TempDir::new("td-seatd-link").unwrap();
        let link = root.path().join("card");
        let (one, two) = (Path::new("/dev/dri/card1"), Path::new("/dev/dri/card0"));
        publish_card(&link, None).unwrap();
        assert_eq!(published_card(&link).unwrap(), None);
        publish_card(&link, Some(one)).unwrap();
        assert_eq!(published_card(&link).unwrap().as_deref(), Some(one));
        publish_card(&link, Some(two)).unwrap();
        assert_eq!(published_card(&link).unwrap().as_deref(), Some(two));
        assert!(fs::symlink_metadata(link.with_extension("new")).is_err());
        publish_card(&link, None).unwrap();
        assert_eq!(published_card(&link).unwrap(), None);
    }

    #[test]
    fn only_a_cards_own_name_is_a_card() {
        let root = drm_fixture(&["card0", "card1"], &[]);
        for other in ["card+1", "card007", "card", "card1-eDP-1", "cardx"] {
            fs::write(root.path().join("dri").join(other), b"").unwrap();
        }
        let names: Vec<String> = card_nodes(&root.path().join("dri"))
            .unwrap()
            .iter()
            .map(|card| card.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["card0", "card1"]);
    }

    fn own_account(path: &Path) -> Account {
        let metadata = fs::metadata(path).unwrap();
        Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        }
    }

    #[test]
    fn watch_assigns_late_and_recreated_nodes_and_leaves_assigned_ones() {
        let root = td_fs::TempDir::new("td-seat-watch").unwrap();
        let input = root.path().join("input");
        fs::create_dir(&input).unwrap();
        let account = own_account(root.path());
        let mut watch = Watch::default();
        let none = (Vec::new(), Vec::new());
        // A node present at boot, already assigned, is left alone.
        fs::write(input.join("event0"), b"").unwrap();
        assign_input(&input.join("event0"), account, false).unwrap();
        assert_eq!(watch.tick(&input, account, false), none);
        // A late node, root's mode 0600 being the kernel's, is assigned
        // and said once; `mouse0` and other names are never touched.
        fs::write(input.join("event5"), b"").unwrap();
        fs::set_permissions(input.join("event5"), Permissions::from_mode(0o644)).unwrap();
        fs::write(input.join("mouse0"), b"").unwrap();
        fs::set_permissions(input.join("mouse0"), Permissions::from_mode(0o644)).unwrap();
        let (assigned, said) = watch.tick(&input, account, false);
        assert!(said.is_empty(), "{said:?}");
        assert_eq!(assigned.len(), 1);
        assert!(assigned[0].starts_with(&format!(
            "assigned {}, device 0:0, inode ",
            input.join("event5").display()
        )));
        verify_owner_mode(&input.join("event5"), account, 0o600).unwrap();
        assert_eq!(watch.tick(&input, account, false), none);
        assert_eq!(
            fs::metadata(input.join("mouse0")).unwrap().mode() & 0o777,
            0o644
        );
        // Loosened again by its owner, it is put back without a second line.
        fs::set_permissions(input.join("event5"), Permissions::from_mode(0o644)).unwrap();
        assert_eq!(watch.tick(&input, account, false), none);
        verify_owner_mode(&input.join("event5"), account, 0o600).unwrap();
        // Recreated under the same name, it is a new node, assigned again.
        fs::remove_file(input.join("event5")).unwrap();
        fs::write(input.join("event5"), b"").unwrap();
        fs::set_permissions(input.join("event5"), Permissions::from_mode(0o640)).unwrap();
        let (assigned, said) = watch.tick(&input, account, false);
        assert_eq!((assigned.len(), said.len()), (1, 0));
        verify_owner_mode(&input.join("event5"), account, 0o600).unwrap();
    }

    #[test]
    fn a_watch_failure_is_said_once_per_inode_and_does_not_end_it() {
        let root = td_fs::TempDir::new("td-seat-watch").unwrap();
        let input = root.path().join("input");
        fs::create_dir(&input).unwrap();
        let account = own_account(root.path());
        let mut watch = Watch::default();
        let none = (Vec::new(), Vec::new());
        // A symlink is refused, never followed.
        fs::write(root.path().join("target"), b"").unwrap();
        fs::set_permissions(root.path().join("target"), Permissions::from_mode(0o644)).unwrap();
        unix_fs::symlink(root.path().join("target"), input.join("event3")).unwrap();
        fs::write(input.join("event4"), b"").unwrap();
        fs::set_permissions(input.join("event4"), Permissions::from_mode(0o644)).unwrap();
        let (assigned, said) = watch.tick(&input, account, false);
        assert_eq!(assigned.len(), 1);
        assert_eq!(said.len(), 1);
        assert!(said[0].contains("refusing symlink"), "{said:?}");
        assert_eq!(
            fs::metadata(root.path().join("target")).unwrap().mode() & 0o777,
            0o644
        );
        // Retried each tick, said once.
        assert_eq!(watch.tick(&input, account, false), none);
        // Once the node is gone the record is pruned, so a new failure at
        // the same name is said again.
        fs::remove_file(input.join("event3")).unwrap();
        assert_eq!(watch.tick(&input, account, false), none);
        assert!(watch.reported.is_empty());
        fs::create_dir(input.join("event3")).unwrap();
        assert_eq!(watch.tick(&input, account, false).1.len(), 1);
        // A failed directory read is said once per transition.
        let missing = root.path().join("missing");
        assert_eq!(watch.tick(&missing, account, false).1.len(), 1);
        assert!(watch.tick(&missing, account, false).1.is_empty());
        watch.tick(&input, account, false);
        assert_eq!(watch.tick(&missing, account, false).1.len(), 1);
        // An overflow is its own transition, before and after a failed read.
        let crowded = root.path().join("crowded");
        fs::create_dir(&crowded).unwrap();
        for index in 0..=INPUT_DIRECTORY_ENTRIES {
            fs::write(crowded.join(format!("other{index}")), b"").unwrap();
        }
        let said = watch.tick(&crowded, account, false).1;
        assert!(said[0].contains("more than 2048 entries"), "{said:?}");
        assert!(watch.tick(&crowded, account, false).1.is_empty());
        assert_eq!(watch.tick(&missing, account, false).1.len(), 1);
        assert_eq!(watch.tick(&crowded, account, false).1.len(), 1);
        // A node that is gone, or assigned after all, by the time its
        // assignment fails is not a failure.
        assert!(settled(&input.join("event9"), account, false));
        assert!(settled(&input.join("event4"), account, false));
        assert!(!settled(&input.join("event3"), account, false));
    }

    #[test]
    fn only_the_regular_file_seam_skips_the_input_major_check() {
        // A character device of another major, `/dev/null`'s 1, is refused
        // by the check that precedes any change.
        let null = Path::new("/dev/null");
        let refused = input_node(null, true).unwrap_err();
        assert!(
            refused.contains("major 1, not an input device"),
            "{refused}"
        );
        assert!(!assigned_input(
            &fs::symlink_metadata(null).unwrap(),
            own_account(Path::new("/")),
            true
        ));
        assert_eq!(device_major(0x0d40), 13);
        assert_eq!(device_minor(0x0d40), 64);
        assert_eq!(
            device_major((0x0012_3000 << 32) | (0xabc << 8)),
            0x0012_3abc
        );
        assert_eq!(
            device_minor((0x0012_3000 << 32) | (0x0456_7000 << 12) | 0x89),
            0x0456_7089
        );
        let source = include_str!("main.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert_eq!(production.matches("assign_input(").count(), 3);
        assert!(production
            .contains("        assign_input(path, assignment.compositor, require_char)?;\n"));
        assert!(
            production.contains("state.tick(Path::new(INPUT_DIR), assignment.compositor, true)")
        );
        assert!(
            production.contains("            match assign_input(&path, account, require_char) {\n")
        );
        // The check comes first in every assignment and in the probe.
        assert!(production.contains(
            "    let node = input_node(path, require_char)?;\n    assign_path(path, account, require_char)?;\n"
        ));
        assert!(production.contains("Err(_) if settled(&path, account, require_char) => {}"));
        assert!(production
            .contains("    for path in &inputs {\n        input_node(path, require_char)?;\n"));
    }

    #[test]
    fn event_names_are_narrow() {
        assert!(event_name("event0"));
        assert!(event_name("event123"));
        for bad in ["event", "event-1", "mouse0", "event0.bak"] {
            assert!(!event_name(bad), "{bad}");
        }
    }

    #[test]
    fn compositor_identity_is_canonical_dedicated_and_required() {
        for uid in ["0", "0993", "1000", "994", "65536", "-1", "4294967296"] {
            let arguments = [
                "--uid",
                "1000",
                "--gid",
                "1000",
                "--compositor-uid",
                uid,
                "--audio-uid",
                "994",
                "--audio-gid",
                "994",
            ]
            .map(str::to_string);
            assert!(parse_assignment(&arguments).is_err(), "{uid}");
        }
        let missing = [
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--audio-uid",
            "994",
            "--audio-gid",
            "994",
        ]
        .map(str::to_string);
        assert!(parse_assignment(&missing).is_err());
    }

    #[test]
    fn device_denial_probe_requires_all_human_credentials_and_real_denial() {
        let account = Account {
            uid: 1000,
            gid: 1000,
        };
        let good = "Uid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
        assert!(require_human_process(good, account).is_ok());
        for column in 0..4 {
            for key in ["Uid:", "Gid:"] {
                let bad: String = good
                    .lines()
                    .map(|line| {
                        if line.starts_with(key) {
                            let mut fields = ["1000"; 4];
                            fields[column] = "0";
                            format!("{key}\t{}\n", fields.join("\t"))
                        } else {
                            format!("{line}\n")
                        }
                    })
                    .collect();
                assert!(require_human_process(&bad, account).is_err());
            }
        }
        let scratch = Scratch::new();
        let device = scratch.path.join("device");
        assert!(denied_device_read(&device).is_err());
        fs::write(&device, b"readable").unwrap();
        assert!(denied_device_read(&device).is_err());
        if fs::metadata(&device).unwrap().uid() != 0 {
            fs::set_permissions(&device, Permissions::from_mode(0o0)).unwrap();
            assert!(denied_device_read(&device).is_ok());
        }
    }

    #[test]
    fn playback_names_exclude_capture_and_control_devices() {
        for good in ["pcmC0D0p", "pcmC12D34p"] {
            assert!(playback_name(good), "{good}");
        }
        for bad in [
            "pcmC0D0c",
            "pcmC0D0",
            "pcmCD0p",
            "pcmC0Dp",
            "pcmC0D0p.bak",
            "controlC0",
            "seq",
            "timer",
        ] {
            assert!(!playback_name(bad), "{bad}");
        }
    }

    #[test]
    fn playback_scan_accepts_no_sound_hardware_and_caps_named_devices() {
        let scratch = Scratch::new();
        let absent = scratch.path.join("absent");
        assert!(playback_paths(&absent).unwrap().is_empty());

        let sound = scratch.path.join("snd");
        fs::create_dir(&sound).unwrap();
        for device in 0..MAX_PLAYBACK_DEVICES {
            fs::write(sound.join(format!("pcmC0D{device}p")), b"").unwrap();
        }
        assert_eq!(playback_paths(&sound).unwrap().len(), MAX_PLAYBACK_DEVICES);
        fs::write(sound.join(format!("pcmC0D{MAX_PLAYBACK_DEVICES}p")), b"").unwrap();
        assert!(playback_paths(&sound).is_err());
    }

    #[test]
    fn account_parser_rejects_root_duplicates_and_shared_identities() {
        let assignment = |uid: &str, gid: &str, audio_uid: &str, audio_gid: &str| {
            parse_assignment(&[
                "--uid".into(),
                uid.into(),
                "--gid".into(),
                gid.into(),
                "--compositor-uid".into(),
                "993".into(),
                "--audio-uid".into(),
                audio_uid.into(),
                "--audio-gid".into(),
                audio_gid.into(),
            ])
        };
        assert!(assignment("1000", "1000", "994", "994").is_ok());
        assert!(parse_assignment(&[
            "--uid".into(),
            "1000".into(),
            "--gid".into(),
            "1000".into(),
            "--audio-gid".into(),
            "994".into(),
        ])
        .is_err());
        assert!(parse_assignment(&[
            "--uid".into(),
            "1000".into(),
            "--gid".into(),
            "1000".into(),
            "--audio-uid".into(),
            "994".into(),
        ])
        .is_err());
        for (uid, gid, audio_uid, audio_gid) in [
            ("0", "1000", "994", "994"),
            ("1000", "0", "994", "994"),
            ("1000", "1000", "0", "994"),
            ("1000", "1000", "994", "0"),
            ("1000", "1000", "1000", "994"),
            ("1000", "1000", "994", "1000"),
        ] {
            assert!(assignment(uid, gid, audio_uid, audio_gid).is_err());
        }
        assert!(parse_assignment(&[
            "--uid".into(),
            "1000".into(),
            "--uid".into(),
            "1001".into(),
            "--gid".into(),
            "1000".into(),
            "--audio-uid".into(),
            "994".into(),
            "--audio-gid".into(),
            "994".into(),
        ])
        .is_err());
    }

    #[test]
    fn audio_exec_parser_preserves_the_literal_service_command() {
        let args = [
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--compositor-uid",
            "993",
            "--audio-uid",
            "994",
            "--audio-gid",
            "994",
            "--",
            "/bin/td-login",
            "exec-service-as",
            "audio",
            "--",
            "/bin/td-audio",
            "serve",
        ]
        .map(String::from);
        let parsed = parse_audio_exec(&args).unwrap();
        assert_eq!(parsed.assignment.seat.uid, 1000);
        assert_eq!(parsed.assignment.seat.gid, 1000);
        assert_eq!(parsed.assignment.audio.uid, 994);
        assert_eq!(parsed.assignment.audio.gid, 994);
        assert_eq!(parsed.program, "/bin/td-login");
        assert_eq!(
            parsed.arguments,
            ["exec-service-as", "audio", "--", "/bin/td-audio", "serve"]
        );

        let missing_separator = args[..10].to_vec();
        assert!(parse_audio_exec(&missing_separator).is_err());
        let missing_program = args[..11].to_vec();
        assert!(parse_audio_exec(&missing_program).is_err());
        for program in ["td-login", "/"] {
            let mut invalid = args[..11].to_vec();
            invalid.push(program.into());
            assert!(parse_audio_exec(&invalid).is_err(), "{program}");
        }
    }

    #[test]
    fn vm_port_scan_selects_only_the_td_name_and_refuses_duplicates() {
        let scratch = Scratch::new();
        let sys = scratch.path.join("ports");
        assert!(vm_port(&sys, Path::new("/dev")).unwrap().is_none());
        fs::create_dir_all(sys.join("vport0p1")).unwrap();
        fs::create_dir_all(sys.join("vport0p2")).unwrap();
        fs::write(sys.join("vport0p1/name"), b"com.other.port\n").unwrap();
        fs::write(sys.join("vport0p2/name"), b"org.td.vm.1\n").unwrap();
        assert_eq!(
            vm_port(&sys, Path::new("/dev")).unwrap(),
            Some(PathBuf::from("/dev/vport0p2"))
        );
        fs::write(sys.join("vport0p1/name"), b"org.td.vm.1\n").unwrap();
        assert!(vm_port(&sys, Path::new("/dev"))
            .unwrap_err()
            .contains("duplicate"));
        assert!(include_str!("../../td-compositor/src/vm_wire.rs").contains("\"org.td.vm.1\""));
    }

    #[test]
    fn vm_port_wait_observes_a_late_name_and_bounds_absence() {
        let scratch = Scratch::new();
        let sys = scratch.path.join("sys");
        fs::create_dir_all(sys.join("vport0p1")).unwrap();
        let name = sys.join("vport0p1/name");
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            fs::write(name, b"org.td.vm.1\n").unwrap();
        });
        assert_eq!(
            wait_vm_port(&sys, Path::new("/dev"), Duration::from_secs(2)).unwrap(),
            Some(PathBuf::from("/dev/vport0p1"))
        );
        publisher.join().unwrap();
        fs::remove_file(sys.join("vport0p1/name")).unwrap();
        assert_eq!(
            wait_vm_port(&sys, Path::new("/dev"), Duration::ZERO).unwrap(),
            None
        );
    }

    #[test]
    fn vm_port_assignment_clears_absence_and_refuses_renumber_before_mutation() {
        let scratch = Scratch::new();
        let meta = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        let old = scratch.path.join("vport0p1");
        let new = scratch.path.join("vport0p2");
        fs::write(&old, b"").unwrap();
        fs::write(&new, b"").unwrap();
        fs::set_permissions(&new, Permissions::from_mode(0o644)).unwrap();
        assign_vm_port_path(&scratch.path, account, Some(&old), false).unwrap();
        assign_vm_port_path(&scratch.path, account, Some(&old), false).unwrap();
        assert_eq!(fs::metadata(&old).unwrap().mode() & 0o777, 0o600);
        assert!(
            assign_vm_port_path(&scratch.path, account, Some(&new), false)
                .unwrap_err()
                .contains("assignment changed")
        );
        assert_eq!(fs::metadata(&new).unwrap().mode() & 0o777, 0o644);
        assert_eq!(
            fs::read(scratch.path.join("vm-port")).unwrap(),
            old.as_os_str().as_encoded_bytes()
        );
        fs::remove_file(&old).unwrap();
        assign_vm_port_path(&scratch.path, account, None, false).unwrap();
        assert!(!scratch.path.join("vm-port").exists());
        assign_vm_port_path(&scratch.path, account, None, false).unwrap();
        assign_vm_port_path(&scratch.path, account, Some(&new), false).unwrap();
    }

    #[test]
    fn optional_vm_failure_clears_authority_but_cleanup_failure_refuses() {
        let scratch = Scratch::new();
        let meta = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        let record = scratch.path.join("vm-port");
        fs::write(&record, b"stale").unwrap();
        optional_vm_assignment(&scratch.path, account, Err("duplicate ports".into())).unwrap();
        assert!(!record.exists());
        fs::create_dir(&record).unwrap();
        assert!(optional_vm_assignment(&scratch.path, account, Err("scan failed".into())).is_err());
    }

    #[test]
    fn vm_port_lookup_errors_refuse_before_delegation() {
        let scratch = Scratch::new();
        let meta = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        let runtime = scratch.path.join("not-a-directory");
        let device = scratch.path.join("vport0p1");
        fs::write(&runtime, b"").unwrap();
        fs::write(&device, b"").unwrap();
        fs::set_permissions(&device, Permissions::from_mode(0o644)).unwrap();
        assert!(assign_vm_port_path(&runtime, account, Some(&device), false).is_err());
        assert_eq!(fs::metadata(&device).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn assignment_is_bounded_to_framebuffer_events_and_runtime() {
        let scratch = Scratch::new();
        let dev = scratch.path.join("dev");
        let input = dev.join("input");
        let run = scratch.path.join("run");
        let sound = dev.join("snd");
        let runtime = run.join("user/1000");
        let audio_runtime = run.join("td-audio");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir(&sound).unwrap();
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, Permissions::from_mode(0o755)).unwrap();
        fs::write(dev.join("fb0"), b"").unwrap();
        fs::create_dir(dev.join("dri")).unwrap();
        fs::write(dev.join("dri/card0"), b"").unwrap();
        fs::set_permissions(dev.join("dri/card0"), Permissions::from_mode(0o644)).unwrap();
        fs::write(dev.join("dri/renderD128"), b"render").unwrap();
        fs::set_permissions(dev.join("dri/renderD128"), Permissions::from_mode(0o644)).unwrap();
        fs::write(input.join("event0"), b"").unwrap();
        fs::write(input.join("event12"), b"").unwrap();
        fs::write(input.join("mouse0"), b"untouched").unwrap();
        fs::write(sound.join("pcmC0D0p"), b"").unwrap();
        fs::write(sound.join("pcmC0D0c"), b"capture").unwrap();
        fs::write(sound.join("controlC0"), b"control").unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let assignment = Assignment {
            seat: Account {
                uid: metadata.uid(),
                gid: metadata.gid(),
            },
            compositor: Account {
                uid: metadata.uid(),
                gid: metadata.gid(),
            },
            audio: Account {
                uid: metadata.uid(),
                gid: metadata.gid(),
            },
        };
        let count = assign(
            Display {
                framebuffer: &dev.join("fb0"),
                card: &dev.join("dri/card0"),
            },
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false,
        )
        .unwrap();
        assert_eq!(
            count,
            Counts {
                card: true,
                inputs: 2,
                playback: 1,
            }
        );
        assert_eq!(
            fs::read(input.join("mouse0")).unwrap(),
            b"untouched".to_vec()
        );
        assert_eq!(fs::read(sound.join("pcmC0D0c")).unwrap(), b"capture");
        assert_eq!(fs::read(sound.join("controlC0")).unwrap(), b"control");
        verify_owner_mode(&dev.join("dri/card0"), assignment.compositor, 0o600).unwrap();
        // The render node is not the display and stays as it was.
        assert_eq!(
            fs::metadata(dev.join("dri/renderD128")).unwrap().mode() & 0o777,
            0o644
        );
        assert_eq!(
            probe(
                Display {
                    framebuffer: &dev.join("fb0"),
                    card: &dev.join("dri/card0")
                },
                &input,
                &sound,
                &runtime,
                &audio_runtime,
                assignment,
                false,
            )
            .unwrap(),
            Counts {
                card: true,
                inputs: 2,
                playback: 1,
            }
        );
        assert_eq!(
            fs::symlink_metadata(&audio_runtime)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        let vm_runtime = shared_runtime(&runtime, "td-guest").unwrap();
        verify_owner_mode(&vm_runtime, assignment.seat, 0o755).unwrap();
        verify_runtime_base(vm_runtime.parent().unwrap(), false).unwrap();
        fs::set_permissions(&vm_runtime, Permissions::from_mode(0o777)).unwrap();
        assert!(probe(
            Display {
                framebuffer: &dev.join("fb0"),
                card: &dev.join("dri/card0")
            },
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false
        )
        .is_err());
        let base = fs::symlink_metadata(run.join("user")).unwrap();
        assert!(base.file_type().is_dir());
        assert_eq!(base.permissions().mode() & 0o7777, 0o755);
    }

    #[test]
    fn a_machine_without_a_card_is_assigned_and_probed_without_one() {
        let scratch = Scratch::new();
        let dev = scratch.path.join("dev");
        let input = dev.join("input");
        let run = scratch.path.join("run");
        let sound = dev.join("snd");
        let runtime = run.join("user/1000");
        let audio_runtime = run.join("td-audio");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir(&sound).unwrap();
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, Permissions::from_mode(0o755)).unwrap();
        fs::write(dev.join("fb0"), b"").unwrap();
        fs::write(input.join("event0"), b"").unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };
        let assignment = Assignment {
            seat: account,
            compositor: account,
            audio: account,
        };
        let card = dev.join("dri/card0");
        let expected = Counts {
            card: false,
            inputs: 1,
            playback: 0,
        };
        let assigned = assign(
            Display {
                framebuffer: &dev.join("fb0"),
                card: &card,
            },
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false,
        );
        assert_eq!(assigned.unwrap(), expected);
        let probed = probe(
            Display {
                framebuffer: &dev.join("fb0"),
                card: &card,
            },
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false,
        );
        assert_eq!(probed.unwrap(), expected);
        // A card that appears later is not silently taken as assigned.
        fs::create_dir(dev.join("dri")).unwrap();
        fs::write(&card, b"").unwrap();
        fs::set_permissions(&card, Permissions::from_mode(0o644)).unwrap();
        assert!(probe(
            Display {
                framebuffer: &dev.join("fb0"),
                card: &card
            },
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false
        )
        .unwrap_err()
        .contains("expected 0600"));
    }

    #[test]
    fn a_card_that_is_a_symlink_is_refused_not_followed() {
        let scratch = Scratch::new();
        let dev = scratch.path.join("dev");
        let input = dev.join("input");
        let run = scratch.path.join("run");
        let sound = dev.join("snd");
        let runtime = run.join("user/1000");
        let audio_runtime = run.join("td-audio");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir(&sound).unwrap();
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, Permissions::from_mode(0o755)).unwrap();
        fs::write(dev.join("fb0"), b"").unwrap();
        fs::write(input.join("event0"), b"").unwrap();
        let target = scratch.path.join("target");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o644)).unwrap();
        fs::create_dir(dev.join("dri")).unwrap();
        let card = dev.join("dri/card0");
        unix_fs::symlink(&target, &card).unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };
        let assignment = Assignment {
            seat: account,
            compositor: account,
            audio: account,
        };
        let display = Display {
            framebuffer: &dev.join("fb0"),
            card: &card,
        };
        let assigned = assign(
            display,
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false,
        );
        assert!(assigned.unwrap_err().contains("refusing symlink"));
        let probed = probe(
            display,
            &input,
            &sound,
            &runtime,
            &audio_runtime,
            assignment,
            false,
        );
        assert!(probed.unwrap_err().contains("refusing symlink"));
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn audio_runtime_refuses_a_symlink_without_touching_its_target() {
        let scratch = Scratch::new();
        let runtime = scratch.path.join("td-audio");
        let target = scratch.path.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o700)).unwrap();
        unix_fs::symlink(&target, &runtime).unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };

        let error = prepare_audio_runtime(&runtime, account, false).unwrap_err();
        assert!(error.contains("exists but is not a directory"));
        assert_eq!(
            fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn compositor_runtime_is_separate_and_refuses_redirected_parents() {
        let scratch = Scratch::new();
        let run = scratch.path.join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };
        let path = compositor_runtime(&run.join("user/1000")).unwrap();
        assert_eq!(path, run.join("td-compositor/1000"));
        prepare_compositor_runtime(&path, account, false).unwrap();
        verify_owner_mode(&path, account, 0o755).unwrap();
        fs::remove_dir(&path).unwrap();
        fs::remove_dir(run.join("td-compositor")).unwrap();
        let target = scratch.path.join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o700)).unwrap();
        unix_fs::symlink(&target, run.join("td-compositor")).unwrap();
        assert!(prepare_compositor_runtime(&path, account, false).is_err());
        assert!(!target.join("1000").exists());
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o7777, 0o700);
    }

    #[test]
    fn audio_runtime_does_not_repair_its_parent() {
        let scratch = Scratch::new();
        fs::set_permissions(&scratch.path, Permissions::from_mode(0o700)).unwrap();
        let runtime = scratch.path.join("td-audio");
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };

        let error = prepare_audio_runtime(&runtime, account, false).unwrap_err();
        assert!(error.contains("expected 0755"));
        assert_eq!(
            fs::symlink_metadata(&scratch.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert!(!runtime.exists());
    }
}
