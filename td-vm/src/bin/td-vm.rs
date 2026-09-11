#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../../../engine/src/sha256.rs"]
mod sha256;

#[allow(dead_code)]
#[path = "../term.rs"]
mod term;

#[allow(dead_code)]
#[path = "../../../engine/src/json.rs"]
mod json;

#[path = "../vm_bridge.rs"]
mod vm_bridge;
#[path = "../vm_clipboard.rs"]
mod vm_clipboard;
#[path = "../../../td-compositor/src/vm_wire.rs"]
mod vm_wire;

#[path = "../vm_git_profile.rs"]
mod vm_git_profile;
#[path = "../vm_git_origin.rs"]
#[allow(dead_code)]
mod vm_git_origin;
#[path = "../vm_git_names.rs"]
mod vm_git_names;
#[path = "../vm_workspace.rs"]
mod vm_workspace;
#[path = "../vm_provision.rs"]
mod vm_provision;

type Result<T> = std::result::Result<T, String>;
const TABLE_HEADER: &str = "NAME                             STATE    ACCEL    TEMPLATE         CPU RAM MiB HOST MiB  CAP MiB";
const DISK_LEGEND: &str = "HOST: allocated overlay; CAP: virtual capacity. Neither is guest free space.";
const HELP: &str = "td-vm: manage persistent graphical td instances

  td-vm                              open the TUI
  td-vm import TEMPLATE BUNDLE        verify and copy an existing clean bundle
  td-vm templates
  td-vm remove-template TEMPLATE
  td-vm create NAME TEMPLATE [CPUS MEMORY_MIB]
  td-vm create NAME TEMPLATE --branch BRANCH [CPUS MEMORY_MIB]
  td-vm list
  td-vm status NAME                  inspect QEMU execution and disk I/O state
  td-vm resume NAME                  resume an explicitly paused guest
  td-vm open NAME [--accel auto|kvm|tcg] [--display gtk|sdl]
  td-vm stop NAME                     request orderly guest poweroff
  td-vm stop NAME --force             cut power (guest work may be lost)
  td-vm delete NAME --yes             delete a stopped instance and its work
  td-vm logs NAME
  td-vm prune                        remove interrupted import/create staging
  td-vm clipboard put NAME            stdin text to guest selection
  td-vm clipboard get NAME            guest selection to stdout
  td-vm sharing NAME on|off           enable/disable explicit transfers
  td-vm feed NAME PORT|off            provision host feed endpoint
  td-vm bridge NAME                   query guest clipboard capability
  td-vm git-profile set FILE          save a host Git profile
  td-vm git-profile show              display configured profile
  td-vm git-profile check             authenticate registrar and verify origin
  td-vm workspace prepare NAME BRANCH save a private workspace plan
  td-vm workspace show NAME           inspect saved identity and Git profile
  td-vm workspace key NAME            request the guest-generated SSH public key
  td-vm workspace enroll NAME         enroll its Git key, branch and starting commit
  td-vm workspace clone NAME          provision its private guest clone and worktree
  td-vm workspace terminal NAME       open a terminal in its prepared task worktree

TD_VM_HOME defaults to ~/.local/share/td-vm. Requires host QEMU, qemu-img and qemu-io.
Reuse dist/td-vm-x86-64 from ./build-qcow; no image rebuild on create/open.
Clipboard and feed actions require a bridge-capable system image. Workspace
automatic provisioning on Open uses the updated image. Agent launch and login
integration remain pending. Shut down from inside td;
Stop requests guest poweroff; --force explicitly cuts power.";

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-vm: {}", term::scrub_lines(&e));
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<()> {
    if matches!(
        args.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        println!("{HELP}");
        return Ok(());
    }
    let home = match env::var_os("TD_VM_HOME") {
        Some(path) => PathBuf::from(path),
        None => {
            PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?).join(".local/share/td-vm")
        }
    };
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let read_only = matches!(words.first(), Some(&"list" | &"templates" | &"logs" | &"status"))
        || matches!(words.as_slice(), ["git-profile", "show" | "check"] | ["workspace", "show", _]);
    let manager = if read_only {
        if !home.exists() && matches!(words.as_slice(), ["list"] | ["templates"]) {
            println!("No managed instances or templates yet.");
            return Ok(());
        }
        Manager::existing(&home)?
    } else {
        Manager::new(&home)?
    };
    match words.as_slice() {
        [] => tui(&manager),
        ["git-profile", "set", file] => {
            let _lock = manager.lock("git-profile")?;
            vm_git_profile::configure(&manager.root, Path::new(file))?;
            println!("Git profile saved; run td-vm git-profile check to verify host setup");
            Ok(())
        }
        ["git-profile", "show"] => {
            let profile = vm_git_profile::load(&manager.root)?;
            println!("{}", term::scrub_lines(&profile.encode()));
            Ok(())
        }
        ["git-profile", "check"] => {
            let profile = vm_git_profile::load(&manager.root)?;
            let origin = profile.check()?;
            println!("Git profile {}: registrar authenticated; origin {} at main {}. Guest SSH and workspace readiness remain unverified.", profile.fingerprint(), origin.repository, origin.head);
            Ok(())
        }
        ["import", name, bundle] => manager.import(name, Path::new(bundle)),
        ["workspace", "prepare", name, branch] => {
            println!("{}", term::scrub_lines(&manager.prepare_workspace(name, branch)?));
            Ok(())
        }
        ["workspace", "enroll", name] => {
            println!("{}", term::scrub_lines(&manager.enroll_workspace(name)?));
            Ok(())
        }
        ["workspace", "clone", name] => {
            println!("{}", term::scrub_lines(&manager.clone_workspace(name)?));
            Ok(())
        }
        ["workspace", "terminal", name] => {
            println!("{}", term::scrub_lines(&manager.workspace_terminal(name)?));
            Ok(())
        }
        ["workspace", "key", name] => {
            println!("{}", manager.workspace_key(name)?);
            Ok(())
        }
        ["workspace", "show", name] => {
            println!("{}", term::scrub_lines(&manager.workspace(name)?));
            Ok(())
        }
        ["templates"] => manager.templates(),
        ["prune"] => manager.prune(),
        ["remove-template", name] => manager.remove_template(name),
        ["create", name, template] => manager.create(name, template, "4", "8192"),
        ["create", name, template, "--branch", branch, resources @ ..] => {
            let (cpus, memory) = match resources {
                [] => ("4", "8192"),
                [cpus, memory] => (*cpus, *memory),
                _ => return Err("create --branch needs either no resources or CPUS MEMORY_MIB".into()),
            };
            manager.create_workspace(name, template, cpus, memory, Some(branch))
        }
        ["create", name, template, cpus, memory] => manager.create(name, template, cpus, memory),
        ["list"] => manager.list(),
        ["status", name] => {
            println!("{}", term::scrub_lines(&manager.status(name)?));
            Ok(())
        }
        ["resume", name] => {
            println!("{}", term::scrub_lines(&manager.resume(name)?));
            Ok(())
        }
        ["open" | "start", name, options @ ..] => manager.start(name, Launch::parse(options)?),
        ["_supervise", name, accel, display] => manager.supervise(
            name,
            Launch::parse(&["--accel", accel, "--display", display])?,
        ),
        ["stop", name] => {
            println!("{}", manager.poweroff(name)?);
            Ok(())
        },
        ["stop", name, "--force"] => manager.stop(name),
        ["clipboard", "put", name] => {
            let mut bytes = Vec::new();
            io(
                std::io::stdin()
                    .take((vm_wire::MAX_TEXT + 1) as u64)
                    .read_to_end(&mut bytes),
                "read clipboard input",
            )?;
            vm_wire::text(&bytes)?;
            manager.bridge(name, vm_wire::PUT, bytes).map(|_| ())
        }
        ["clipboard", "get", name] => {
            let bytes = manager.bridge(name, vm_wire::GET, Vec::new())?;
            vm_wire::text(&bytes)?;
            io(
                std::io::stdout().write_all(&bytes),
                "write clipboard output",
            )
        }
        ["sharing", name, mode @ ("on" | "off")] => manager.sharing(name, mode),
        ["feed", name, port] => manager.feed(name, port),
        ["bridge", name] => {
            let result = manager.bridge(name, vm_wire::SNAPSHOT, Vec::new())?;
            println!("{}", term::scrub_lines(&String::from_utf8_lossy(&result)));
            Ok(())
        }
        ["delete", name, "--yes"] => manager.delete(name),
        ["logs", name] => {
            println!("{}", term::scrub_lines(&manager.logs(name)?));
            Ok(())
        }
        _ => Err(format!("invalid arguments\n{HELP}")),
    }
}

fn name(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || value.starts_with('-')
    {
        return Err("names must be 1–32 lowercase letters, digits or hyphens, starting with a letter or digit".into());
    }
    Ok(value)
}

fn number(value: &str, low: u32, high: u32, label: &str) -> Result<u32> {
    let value = value
        .parse::<u32>()
        .map_err(|_| format!("invalid {label}"))?;
    if !(low..=high).contains(&value) {
        return Err(format!("{label} must be {low}–{high}"));
    }
    Ok(value)
}

fn io<T>(result: std::io::Result<T>, action: &str) -> Result<T> {
    result.map_err(|e| format!("{action}: {e}"))
}

fn text(path: &Path) -> Result<String> {
    if !io(fs::symlink_metadata(path), "inspect metadata file")?.is_file() {
        return Err(format!("{} must be a regular file", path.display()));
    }
    let file = io(File::open(path), &format!("open {}", path.display()))?;
    let mut value = String::new();
    io(file.take(65537).read_to_string(&mut value), "read metadata")?;
    if value.len() > 65536 {
        return Err("metadata exceeds 64 KiB".into());
    }
    Ok(value)
}

fn write_new(path: &Path, value: &[u8]) -> Result<()> {
    let mut file = io(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path),
        "create metadata",
    )?;
    io(file.write_all(value), "write metadata")?;
    io(file.sync_all(), "sync metadata")
}

fn path_text(path: &Path) -> Result<&str> {
    let value = path.to_str().ok_or("paths must be UTF-8")?;
    if value.contains(['\n', '\r', ',', '"', '\\', '%']) {
        return Err(
            "VM paths must not contain commas, newlines, quotes, backslashes or percent signs"
                .into(),
        );
    }
    Ok(value)
}

fn private_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().recursive(true).mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    check_private_dir(path)
}

fn check_private_dir(path: &Path) -> Result<()> {
    let meta = io(fs::symlink_metadata(path), "inspect VM directory")?;
    let uid = io(fs::metadata("/proc/self"), "inspect current uid")?.uid();
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a private, caller-owned directory (0700), not a symlink",
            path.display()
        ));
    }
    Ok(())
}

fn command(command: &mut Command) -> Result<()> {
    let status = io(
        command.status(),
        &format!("start {:?}", command.get_program()),
    )?;
    if !status.success() {
        return Err(format!("{:?} exited {status}", command.get_program()));
    }
    Ok(())
}

fn entries(path: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in io(fs::read_dir(path), "list VM state")? {
        let entry = io(entry, "read directory entry")?;
        let value = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 state entry")?;
        if name(&value).is_err() || !io(entry.file_type(), "inspect entry")?.is_dir() {
            continue;
        }
        names.push(value);
    }
    names.sort();
    Ok(names)
}

struct Manager {
    root: PathBuf,
}

fn stage_bundle(bundle: &Path, scratch: &Path) -> Result<(&'static str, &'static str)> {
    let (disk, format) = match (
        bundle.join("td-system.qcow2").is_file(),
        bundle.join("td-system.img").is_file(),
    ) {
        (true, false) => ("td-system.qcow2", "qcow2"),
        (false, true) => ("td-system.img", "raw"),
        _ => {
            return Err(
                "bundle must contain exactly one of td-system.qcow2 or td-system.img".into(),
            )
        }
    };
    let manifest = text(&bundle.join("SHA256SUMS"))?;
    for file in ["bzImage", "selector-initramfs.cpio", disk] {
        if !io(
            fs::symlink_metadata(bundle.join(file)),
            "inspect bundle artifact",
        )?
        .is_file()
        {
            return Err(format!("bundle artifact {file} must be a regular file"));
        }
        let mut rows = manifest
            .lines()
            .filter_map(|line| line.split_once("  "))
            .filter(|(_, name)| *name == file);
        let (expected, _) = rows
            .next()
            .ok_or_else(|| format!("SHA256SUMS omits {file}"))?;
        if rows.next().is_some()
            || expected.len() != 64
            || !expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(format!(
                "SHA256SUMS has a duplicate or malformed digest for {file}"
            ));
        }
        println!("copying and verifying {file}...");
        let staged = scratch.join(file);
        io(fs::copy(bundle.join(file), &staged), "copy bundle artifact")?;
        if io(
            sha256::sha256_file(&staged),
            "verify staged bundle artifact",
        )? != expected
        {
            return Err(format!("bundle checksum mismatch for {file}"));
        }
    }
    if format == "qcow2" {
        let info = io(
            Command::new("qemu-img")
                .args(["info", "--output=json", "-f", "qcow2"])
                .arg(scratch.join(disk))
                .output(),
            "inspect staged qcow2",
        )?;
        if !info.status.success() {
            return Err(format!(
                "qemu-img rejected staged disk: {}",
                String::from_utf8_lossy(&info.stderr)
            ));
        }
        let info = String::from_utf8_lossy(&info.stdout);
        if info.contains("\"backing-filename\"") || info.contains("\"data-file\"") {
            return Err("bundle disk must be self-contained; external backing/data files are not covered by SHA256SUMS".into());
        }
    }
    Ok((disk, format))
}

fn verify_template(base: &Path) -> Result<()> {
    let manifest = text(&base.join("SHA256SUMS"))?;
    for file in ["bzImage", "selector-initramfs.cpio", "disk.qcow2"] {
        let path = base.join(file);
        if !io(fs::symlink_metadata(&path), "inspect template")?.is_file() {
            return Err(format!("template {file} must be a regular file"));
        }
        let digest = io(sha256::sha256_file(&path), "verify template")?;
        if manifest
            .lines()
            .filter(|line| *line == format!("{digest}  {file}"))
            .count()
            != 1
        {
            return Err(format!(
                "template checksum mismatch: {file}; import a clean bundle under a new name"
            ));
        }
    }
    Ok(())
}

// Read only the stable header fields, including while QEMU owns the disk.
// This is capacity reporting, not validation of the image's allocation tables.
fn disk_usage(path: &Path) -> Result<(u64, u64)> {
    if !io(fs::symlink_metadata(path), "inspect disk")?.is_file() {
        return Err("disk must be a regular qcow2 file".into());
    }
    // Linux O_NOFOLLOW | O_NONBLOCK: replacement cannot follow a link or
    // block on a FIFO before the opened-file type check.
    let mut file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(0x20000 | 0x800)
            .open(path),
        "open disk header",
    )?;
    let metadata = io(file.metadata(), "inspect opened disk")?;
    if !metadata.is_file() {
        return Err("disk must be a regular qcow2 file".into());
    }
    let mut header = [0u8; 104];
    let prefix = header.get_mut(..72).ok_or("invalid disk header buffer")?;
    io(file.read_exact(prefix), "read qcow2 header")?;
    if header.get(..4) != Some(b"QFI\xfb") {
        return Err("disk has no qcow2 header".into());
    }
    let word = |bytes: Option<&[u8]>| -> Result<u32> {
        Ok(u32::from_be_bytes(
            bytes
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or("short qcow2 field")?,
        ))
    };
    match word(header.get(4..8))? {
        2 => {}
        3 => {
            io(
                file.read_exact(header.get_mut(72..).ok_or("invalid disk header buffer")?),
                "read qcow2 v3 header",
            )?;
            let length = u64::from(word(header.get(100..104))?);
            if length < 104 || length % 8 != 0 || length > metadata.len() {
                return Err("invalid qcow2 header length".into());
            }
        }
        _ => return Err("unsupported qcow2 version".into()),
    }
    let capacity = u64::from_be_bytes(
        header
            .get(24..32)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("short qcow2 capacity")?,
    );
    Ok((metadata.blocks() / 2048, capacity / (1024 * 1024)))
}

fn lock_file(path: &Path, key: &str, retry: bool) -> Result<File> {
    optional_lock(path, key, retry)?.ok_or_else(|| format!("another operation holds {key}"))
}

fn optional_lock(path: &Path, key: &str, retry: bool) -> Result<Option<File>> {
    if path.is_symlink() {
        return Err("lock path is a symlink".into());
    }
    let file = io(
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path),
        "open lock",
    )?;
    // Another thread's fork can briefly retain a CLOEXEC lock descriptor
    // until exec. Bound that transient retry without waiting on a live VM.
    let deadline = Instant::now()
        + if retry {
            Duration::from_millis(100)
        } else {
            Duration::ZERO
        };
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(e) => return Err(format!("lock {key}: {e}")),
        }
    }
    Ok(Some(file))
}

impl Manager {
    fn new(root: &Path) -> Result<Self> {
        private_dir(root)?;
        for part in ["templates", "instances", "locks"] {
            private_dir(&root.join(part))?;
        }
        Self::existing(root)
    }

    fn existing(root: &Path) -> Result<Self> {
        check_private_dir(root)?;
        let root = io(root.canonicalize(), "resolve VM state directory")?;
        path_text(&root)?;
        // Linux sockaddr_un has room for 107 pathname bytes plus its NUL.
        if root.as_os_str().len() + "/instances/".len() + 32 + "/bridge".len() > 107 {
            return Err("TD_VM_HOME is too long for QEMU Unix sockets; use a shorter path".into());
        }
        for part in ["templates", "instances", "locks"] {
            check_private_dir(&root.join(part))?;
        }
        Ok(Self { root })
    }

    fn lock(&self, key: &str) -> Result<File> {
        // These reusable names retain their inodes across operations.
        lock_file(&self.root.join("locks").join(key), key, true)
    }

    fn active(&self, value: &str) -> Result<bool> {
        let path = self
            .root
            .join("locks")
            .join(format!("run-{}", name(value)?));
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(format!("inspect lifetime lock: {e}")),
        };
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(std::fs::TryLockError::WouldBlock) => Ok(true),
            Err(e) => Err(format!("inspect lifetime lock: {e}")),
        }
    }

    fn instance(&self, value: &str) -> Result<PathBuf> {
        let dir = self.root.join("instances").join(name(value)?);
        check_private_dir(&dir)?;
        Ok(dir)
    }

    fn enroll_workspace(&self, value: &str) -> Result<String> {
        name(value)?;
        let lock = self.lock(&format!("instance-{value}"))?;
        self.enroll_locked(&self.instance(value)?, &lock)?.summary()
    }

    fn enroll_locked(&self, dir: &Path, lock: &File) -> Result<vm_workspace::Workspace> {
        let mut workspace = vm_workspace::load(dir)?.ok_or("instance has no workspace plan")?;
        if workspace.enrollment.as_ref().is_some_and(|state| state.phase == vm_workspace::Phase::Revoking) {
            return Err("cannot enroll a revoking workspace; retry deletion, then create a fresh instance".into());
        }
        workspace.profile.check()?;
        let reply = vm_bridge::ask(dir, vm_wire::KEY, workspace.id.as_bytes().to_vec())?;
        let key = vm_wire::git_key::parse(&reply, &workspace.id)?;
        if let Some(state) = &workspace.enrollment {
            if state.key != key { return Err("guest key differs from the saved enrollment; refusing replacement".into()); }
        }
        let phase = workspace.enrollment.as_ref().map_or(vm_workspace::Phase::Pending, |state| state.phase);
        workspace.transition(dir, phase, &key)?;
        workspace.profile.enroll(&workspace.id, &workspace.branch, &key, lock)
            .map_err(|error| format!("Git enrollment is unconfirmed; retry with this same instance: {error}"))?;
        workspace.transition(dir, vm_workspace::Phase::Enrolled, &key)?;
        let start = workspace.profile.start(&workspace.id, &workspace.branch, workspace.start.as_deref(), lock)
            .map_err(|error| format!("Git key enrolled; starting commit unconfirmed. Retry enrollment: {error}"))?;
        workspace.record_start(dir, &start)?;
        Ok(workspace)
    }

    fn clone_workspace(&self, value: &str) -> Result<String> {
        name(value)?;
        let lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        let workspace = vm_workspace::load(&dir)?.ok_or("instance has no workspace plan")?;
        let enrollment = workspace.enrollment.as_ref().filter(|state| state.phase == vm_workspace::Phase::Enrolled)
            .ok_or("enroll this workspace before cloning")?;
        let start = workspace.start.as_deref().ok_or("enroll again to retain a starting commit")?;
        workspace.profile.check()?;
        workspace.profile.start(&workspace.id, &workspace.branch, Some(start), &lock)?;
        let plan = workspace.profile.clone_plan(&workspace.id, &workspace.branch, start, &enrollment.key)?;
        let reply = vm_bridge::ask(&dir, vm_wire::WORKSPACE, plan.encode())?;
        vm_wire::workspace::parse_ready(&reply, &plan)?;
        Ok(format!("Guest workspace and private build state prepared on {} at /home/tester/src/td-vm/work. Terminal launch and agent setup remain pending.", workspace.branch))
    }

    fn workspace_terminal(&self, value: &str) -> Result<String> {
        let name = name(value)?;
        let dir = self.instance(name)?;
        let lock = self.lock(&format!("instance-{name}"))?;
        let workspace = vm_workspace::load(&dir)?.ok_or("instance has no workspace plan")?;
        let enrollment = workspace
            .enrollment
            .as_ref()
            .filter(|state| state.phase == vm_workspace::Phase::Enrolled)
            .ok_or("enroll this workspace before opening its task terminal")?;
        let start = workspace
            .start
            .as_deref()
            .ok_or("enroll again to retain a starting commit")?;
        workspace.profile.check()?;
        workspace
            .profile
            .start(&workspace.id, &workspace.branch, Some(start), &lock)?;
        let plan = workspace.profile.clone_plan(
            &workspace.id,
            &workspace.branch,
            start,
            &enrollment.key,
        )?;
        let reply = vm_bridge::ask(&dir, vm_wire::WORKSPACE_TERMINAL, plan.encode())?;
        if reply != vm_wire::TASK_TERMINAL_QUEUED {
            return Err("guest did not confirm the task terminal launch".into());
        }
        Ok(format!(
            "Task terminal queued on {} in /home/tester/src/td-vm/work.",
            workspace.branch
        ))
    }

    fn workspace_key(&self, value: &str) -> Result<String> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        let workspace = vm_workspace::load(&dir)?.ok_or("instance has no workspace plan")?;
        let reply = vm_bridge::ask(&dir, vm_wire::KEY, workspace.id.as_bytes().to_vec())?;
        vm_wire::git_key::parse(&reply, &workspace.id)
    }

    fn bridge(&self, value: &str, verb: &str, data: Vec<u8>) -> Result<Vec<u8>> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        vm_bridge::ask(&self.instance(value)?, verb, data)
    }

    fn sharing(&self, value: &str, mode: &str) -> Result<()> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        vm_bridge::sharing(&self.instance(value)?, mode)
    }

    fn feed(&self, value: &str, port: &str) -> Result<()> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        vm_bridge::configure_feed(&self.instance(value)?, port)
    }

    fn template(&self, value: &str) -> Result<PathBuf> {
        let dir = self.root.join("templates").join(name(value)?);
        check_private_dir(&dir)?;
        Ok(dir)
    }

    fn import(&self, value: &str, bundle: &Path) -> Result<()> {
        name(value)?;
        let _template = self.lock(&format!("template-{value}"))?;
        let dir = self.root.join("templates").join(value);
        if dir.exists() {
            return Err("template already exists; import a new name for a new version".into());
        }
        let (scratch, _staging) = self.staging("import")?;
        let result = (|| {
            let (disk, format) = stage_bundle(bundle, &scratch)?;
            println!("preparing managed qcow2 base...");
            command(
                Command::new("qemu-img")
                    .args(["convert", "-p", "-f", format, "-O", "qcow2"])
                    .arg(scratch.join(disk))
                    .arg(scratch.join("disk.qcow2")),
            )?;
            io(
                fs::remove_file(scratch.join(disk)),
                "remove staged source disk",
            )?;
            let mut manifest = String::new();
            for file in ["bzImage", "selector-initramfs.cpio", "disk.qcow2"] {
                let path = scratch.join(file);
                println!("recording immutable checksum for {file}...");
                let hash = io(sha256::sha256_file(&path), "hash managed template")?;
                manifest.push_str(&format!("{hash}  {file}\n"));
                let mut permissions =
                    io(fs::metadata(&path), "inspect template file")?.permissions();
                permissions.set_readonly(true);
                io(
                    fs::set_permissions(&path, permissions),
                    "make template read-only",
                )?;
            }
            write_new(&scratch.join("SHA256SUMS"), manifest.as_bytes())?;
            let _catalog = self.lock("catalog")?;
            io(
                fs::remove_file(scratch.join("lease")),
                "remove published staging lease",
            )?;
            io(fs::rename(&scratch, &dir), "publish template")?;
            println!("imported {value}");
            Ok(())
        })();
        if scratch.exists() {
            let _ = fs::remove_dir_all(&scratch);
        }
        result
    }

    fn templates(&self) -> Result<()> {
        for value in entries(&self.root.join("templates"))? {
            println!("{value}");
        }
        Ok(())
    }

    fn remove_template(&self, value: &str) -> Result<()> {
        let _template = self.lock(&format!("template-{}", name(value)?))?;
        let _lock = self.lock("catalog")?;
        let dir = self.template(value)?;
        for instance in entries(&self.root.join("instances"))? {
            let config = Config::read(&self.instance(&instance)?)?;
            if config.template == value {
                return Err(format!("template is still used by {instance}"));
            }
        }
        io(fs::remove_dir_all(dir), "remove unused template")
    }

    fn create(&self, value: &str, template: &str, cpus: &str, memory: &str) -> Result<()> {
        self.create_workspace(value, template, cpus, memory, None)
    }

    fn create_workspace(&self, value: &str, template: &str, cpus: &str, memory: &str, branch: Option<&str>) -> Result<()> {
        name(value)?;
        let workspace = match vm_git_profile::optional(&self.root)? {
            Some(profile) => {
                if branch.is_none() && !vm_git_names::branch_valid(value) {
                    return Err("instance name is reserved as a task branch; select another with create NAME TEMPLATE --branch BRANCH".into());
                }
                Some(vm_workspace::Workspace::new(branch.unwrap_or(value), profile)?)
            }
            None if branch.is_some() => return Err("configure a Git profile before selecting a task branch".into()),
            None => None,
        };
        let config = Config {
            template: name(template)?.into(),
            cpus: number(cpus, 1, 256, "vCPUs")?,
            memory: number(memory, 512, 1048576, "memory MiB")?,
        };
        let (scratch, _staging) = self.staging("create")?;
        let _catalog = self.lock("catalog")?;
        self.clear_staging()?;
        let _instance = self.lock(&format!("instance-{value}"))?;
        let base = self.template(template)?;
        let dest = self.root.join("instances").join(value);
        if dest.exists() {
            return Err("instance already exists".into());
        }
        let result = (|| {
            if let Some(workspace) = &workspace {
                self.check_workspace(value, workspace)?;
            }
            command(
                Command::new("qemu-img")
                    .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
                    .arg(base.join("disk.qcow2"))
                    .arg(scratch.join("disk.qcow2")),
            )?;
            config.write(&scratch)?;
            if let Some(workspace) = &workspace {
                workspace.publish(&scratch)?;
            }
            io(
                fs::remove_file(scratch.join("lease")),
                "remove published staging lease",
            )?;
            io(fs::rename(&scratch, dest), "publish instance")?;
            println!("created {value}: {cpus} vCPUs, {memory} MiB");
            if let Some(workspace) = &workspace {
                println!("{}", term::scrub_lines(&workspace.summary()?));
            }
            Ok(())
        })();
        if scratch.exists() {
            let _ = fs::remove_dir_all(&scratch);
        }
        result
    }

    // Catalog lock serializes workspace publication across instance names.
    fn check_workspace(&self, value: &str, workspace: &vm_workspace::Workspace) -> Result<()> {
        for other in entries(&self.root.join("instances"))? {
            if other == value { continue; }
            if let Some(existing) = vm_workspace::load(&self.instance(&other)?)
                .map_err(|error| format!("cannot inspect workspace for instance {other}: {error}"))? {
                if workspace.conflicts(&existing)? {
                    return Err(format!("workspace identity or task branch overlaps instance {other}"));
                }
            }
        }
        Ok(())
    }

    fn prepare_workspace(&self, value: &str, branch: &str) -> Result<String> {
        name(value)?;
        let _catalog = self.lock("catalog")?;
        let _instance = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        Config::read(&dir)?;
        if let Some(workspace) = vm_workspace::load(&dir)? {
            if workspace.branch != branch {
                return Err("workspace is already bound to another task branch; create a new instance".into());
            }
            return workspace.summary();
        }
        let _lifetime = lock_file(&self.root.join("locks").join(format!("run-{value}")), "workspace", false)
            .map_err(|error| format!("stop the instance before preparing its workspace: {error}"))?;
        if running(&dir)? {
            return Err("stop the instance before preparing its workspace".into());
        }
        disk_available(&dir)?;
        let workspace = vm_workspace::Workspace::new(branch, vm_git_profile::load(&self.root)?)?;
        self.check_workspace(value, &workspace)?;
        workspace.publish(&dir)?;
        workspace.summary()
    }

    fn workspace(&self, value: &str) -> Result<String> {
        match vm_workspace::load(&self.instance(value)?)? {
            Some(workspace) => Ok(format!("{}\n\n{}\n\n{}", workspace.summary()?, workspace.profile.encode(), provisioning_observation(&self.instance(value)?)?)),
            None => Ok("Workspace is unconfigured; prepare it while the instance is stopped.".into()),
        }
    }

    fn rows(&self) -> Result<Vec<(String, String)>> {
        entries(&self.root.join("instances"))?
            .into_iter()
            .map(|value| {
                let row = (|| {
                    let dir = self.instance(&value)?;
                    let config = Config::read(&dir)?;
                    let (host_mib, capacity_mib) = disk_usage(&dir.join("disk.qcow2"))?;
                    let state = if running(&dir)? {
                        "live"
                    } else if self.active(&value)? {
                        "starting"
                    } else {
                        "stopped"
                    };
                    let accel =
                        fs::read_to_string(dir.join("accel")).unwrap_or_else(|_| "unknown".into());
                    Ok::<_, String>(format!(
                        "{value:<32} {state:<8} {:<8} {:<16} {:>3} {:>7} {host_mib:>8} {capacity_mib:>8}",
                        accel.trim(),
                        config.template,
                        config.cpus,
                        config.memory
                    ))
                })();
                Ok((value, row.unwrap_or_else(|e| format!("unavailable: {e}"))))
            })
            .collect()
    }

    fn list(&self) -> Result<()> {
        println!("{TABLE_HEADER}");
        for (_, row) in self.rows()? {
            println!("{}", term::scrub(&row));
        }
        println!("{DISK_LEGEND}");
        Ok(())
    }

    fn logs(&self, value: &str) -> Result<String> {
        use std::io::{Seek, SeekFrom};
        let dir = self.instance(value)?;
        let mut result = String::new();
        for name in ["supervisor.log", "serial.log"] {
            result.push_str(&format!("{name}:\n"));
            match File::open(dir.join(name)) {
                Ok(mut file) => {
                    let length = io(file.metadata(), "inspect log")?.len();
                    io(
                        file.seek(SeekFrom::Start(length.saturating_sub(16384))),
                        "seek log",
                    )?;
                    let mut bytes = Vec::new();
                    io(file.take(16384).read_to_end(&mut bytes), "read log")?;
                    result.push_str(&String::from_utf8_lossy(&bytes));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    result.push_str("no log yet\n")
                }
                Err(e) => return Err(format!("read log: {e}")),
            }
            result.push('\n');
        }
        Ok(result)
    }

    fn staging(&self, operation: &str) -> Result<(PathBuf, File)> {
        // An orphaned qemu-img can outlive its manager. Never reuse its path,
        // even if a later manager receives the same PID after a crash.
        let mut random = [0u8; 16];
        io(
            io(File::open("/dev/urandom"), "open host randomness")?.read_exact(&mut random),
            "read staging nonce",
        )?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let _catalog = self.lock("catalog")?;
        let dir = self
            .root
            .join(format!("{operation}-{}-{nonce}", std::process::id()));
        io(
            DirBuilder::new().mode(0o700).create(&dir),
            "create staging directory",
        )?;
        let guard = lock_file(&dir.join("lease"), "staging", false)?;
        Ok((dir, guard))
    }

    fn clear_staging(&self) -> Result<()> {
        // Imports prepare outside the catalog lock. Each live staging owner
        // holds its own lock; orphaned qemu-img writes can only hit that
        // never-reused, unpublished staging directory.
        for entry in io(fs::read_dir(&self.root), "inspect staging")? {
            let entry = io(entry, "read staging entry")?;
            let value = entry.file_name();
            let Some(value) = value.to_str() else {
                continue;
            };
            let suffix = value
                .strip_prefix("import-")
                .or_else(|| value.strip_prefix("create-"));
            if !suffix
                .and_then(|v| v.split_once('-'))
                .is_some_and(|(pid, nonce)| {
                    !pid.is_empty()
                        && pid.bytes().all(|b| b.is_ascii_digit())
                        && nonce.len() == 32
                        && nonce
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            {
                continue;
            }
            if !io(entry.file_type(), "inspect staging type")?.is_dir() {
                return Err(format!("reserved staging path {value} is not a directory"));
            }
            let Ok(_stage) = lock_file(&entry.path().join("lease"), "staging", false) else {
                continue;
            };
            io(
                fs::remove_dir_all(entry.path()),
                "remove interrupted staging",
            )?;
        }
        Ok(())
    }

    fn prune(&self) -> Result<()> {
        let _catalog = self.lock("catalog")?;
        self.clear_staging()
    }

    fn start(&self, value: &str, launch: Launch) -> Result<()> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        if running(&dir)? {
            println!("{}", term::scrub_lines(&self.status(value).unwrap_or_else(|e| e)));
            println!("{value} already has a QEMU process; select its existing td-vm window");
            return Ok(());
        }
        // A supervisor may still be starting or reaping a guest without QMP.
        if self.active(value)? {
            println!("{value} is already starting or finishing shutdown; inspect its logs");
            return Ok(());
        }
        let log = io(
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(dir.join("supervisor.log")),
            "open supervisor log",
        )?;
        let mut command = Command::new(io(env::current_exe(), "resolve td-vm executable")?);
        command
            .args(["_supervise", value, launch.accel, launch.display])
            .env("TD_VM_HOME", &self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(io(log.try_clone(), "duplicate log")?))
            .stderr(Stdio::from(log))
            .process_group(0);
        let completion = reap_command(command)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if running(&dir)? && dir.join("accel").is_file() {
                let workspace = match vm_workspace::load(&dir) {
                    Ok(Some(_)) => "saved workspace provisioning continues in the background (w / workspace show)",
                    Ok(None) => "workspace unconfigured; prepare it while stopped",
                    Err(_) => "workspace plan unavailable; inspect logs",
                };
                println!("opened {value} ({}); {workspace}", text(&dir.join("accel"))?.trim());
                return Ok(());
            }
            match completion.try_recv() {
                Ok(result) => {
                    return Err(format!(
                        "supervisor ended: {:?}\n{}",
                        result,
                        self.logs(value)?
                    ))
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err("supervisor waiter disconnected; inspect instance state".into())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if Instant::now() >= deadline {
                println!("{value} continues starting in the background; refresh the table or inspect its logs");
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn supervise(&self, value: &str, launch: Launch) -> Result<()> {
        let config = Config::read(&self.instance(value)?)?;
        let base = self.template(&config.template)?;
        self.run_qemu(
            value,
            qemu_command(&self.instance(value)?, &base, &config, launch)?,
        )
    }

    fn run_qemu(&self, value: &str, mut command: Command) -> Result<()> {
        let lifetime = self.lock(&format!("run-{value}"))?;
        let dir = self.instance(value)?;
        if running(&dir)? {
            return Err("instance already has a live QEMU".into());
        }
        let config = Config::read(&dir)?;
        println!("verifying template {} before launch...", config.template);
        verify_template(&self.template(&config.template)?)?;
        disk_available(&dir)?;
        cleanup_runtime(&dir)?;
        let bridge = vm_bridge::Supervisor::start(&dir)?;
        // QEMU never reads stdin (serial goes to a file). Keeping the same
        // locked open-file description on fd 0 preserves exclusion even if
        // the supervisor dies before QEMU publishes its monitor or pidfile.
        let mut child = io(
            command
                .stdin(Stdio::from(io(
                    lifetime.try_clone(),
                    "inherit lifetime lock",
                )?))
                .spawn(),
            "launch QEMU",
        )?;
        // QEMU owns its pidfile and disk lock. Never unlink them on a timed-out
        // startup: that child may still be coming up after its supervisor dies.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = io(child.try_wait(), "inspect QEMU startup")? {
                return Err(format!("QEMU exited during startup: {status}"));
            }
            if let Ok(mut qmp) = Qmp::connect(&dir.join("qmp")) {
                let result = qmp.execute("query-kvm").and_then(|result| {
                    let accel = if result.get("enabled").is_some_and(json::Json::is_true) {
                        "KVM"
                    } else {
                        "TCG"
                    };
                    write_new(&dir.join("accel"), accel.as_bytes())
                });
                if let Err(error) = result {
                    eprintln!("accelerator status unavailable: {error}");
                }
                break;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "QEMU has not opened its monitor; retaining its lifetime lock while it runs"
                );
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let provision = vm_provision::Worker::start(self.root.clone(), value.into(),
            io(lifetime.try_clone(), "retain provisioning lifetime lock")?);
        let status = child.wait();
        provision.cancel();
        // Closing the listener also releases a worker queued in connect.
        drop(bridge);
        drop(provision);
        let status = io(status, "wait for QEMU")?;
        println!("QEMU ended: {status}; disks retained");
        if status.success() {
            Ok(())
        } else {
            Err(format!("QEMU exited {status}; inspect boot diagnostics"))
        }
    }

    fn status(&self, value: &str) -> Result<String> {
        let dir = self.instance(value)?;
        if !running(&dir)? {
            return Ok(format!("{value}: {}", if self.active(value)? {
                "starting or finishing shutdown"
            } else {
                "stopped"
            }));
        }
        let mut qmp = Qmp::connect(&dir.join("qmp"))
            .map_err(|e| format!("{value}: process live; QEMU status unavailable: {e}"))?;
        Health::query(&mut qmp).map(|health| health.describe(value))
            .map_err(|e| format!("{value}: process live; QEMU status unavailable: {e}"))
    }

    fn resume(&self, value: &str) -> Result<String> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        if !running(&dir)? {
            return Err("instance has no live QEMU to resume; use open to boot it".into());
        }
        let mut qmp = Qmp::connect(&dir.join("qmp"))
            .map_err(|e| format!("{value}: process live; resume monitor unavailable: {e}"))?;
        resume_qmp(&mut qmp).map(|health| health.describe(value))
            .map_err(|e| format!("{value}: resume failed: {e}"))
    }

    fn poweroff(&self, value: &str) -> Result<String> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        if !running(&dir)? {
            return Err("instance has no reachable running QEMU; inspect its logs".into());
        }
        vm_bridge::ask(&dir, vm_wire::POWEROFF, Vec::new())
            .map_err(|e| format!("Guest poweroff confirmation unavailable: {e}. Inspect status; no forced stop was sent."))?;
        Ok(format!("Orderly guest poweroff queued for {value}; refresh status to observe exit."))
    }

    fn stop(&self, value: &str) -> Result<()> {
        name(value)?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        if !running(&dir)? {
            return Err("instance has no reachable running QEMU; inspect its logs".into());
        }
        let request = Qmp::connect(&dir.join("qmp"))?.execute("quit");
        let deadline = Instant::now() + Duration::from_secs(10);
        // During QEMU shutdown its pidfile can disappear before its monitor.
        // Such an indeterminate state is still busy, never permission to delete.
        while !matches!(running(&dir), Ok(false)) {
            if Instant::now() >= deadline {
                return Err(format!("QEMU has not finished shutdown; retain the disk and inspect logs (quit: {request:?})"));
            }
            thread::sleep(Duration::from_millis(100));
        }
        println!("force-stopped {value}; the next boot may need filesystem recovery");
        Ok(())
    }

    fn delete(&self, value: &str) -> Result<()> {
        name(value)?;
        let _catalog = self.lock("catalog")?;
        let _lock = self.lock(&format!("instance-{value}"))?;
        let dir = self.instance(value)?;
        let _lifetime = self.lock(&format!("run-{value}")).map_err(|e| {
            format!(
                "{value} is active or finishing shutdown; stop it and wait before deleting: {e}"
            )
        })?;
        if running(&dir)? {
            return Err("refusing to delete a running instance; stop it first".into());
        }
        // Open raw read/write without writing: require QEMU's write lock while
        // permitting a damaged header. Read-only raw opens do NOT test this lock.
        disk_available(&dir)?;
        // Future enrollment formats need their own revocation path. An older
        // manager must refuse unknown/corrupt state rather than erase its disk.
        if let Some(mut workspace) = vm_workspace::load(&dir)? {
            if let Some(state) = &workspace.enrollment {
                let key = state.key.clone();
                workspace.transition(&dir, vm_workspace::Phase::Revoking, &key)?;
                workspace.profile.revoke(&workspace.id, &_lock).map_err(|error| format!(
                    "Git key revocation unconfirmed; disk retained. Retry deletion: {error}"
                ))?;
            }
        }
        io(fs::remove_dir_all(dir), "delete stopped instance")?;
        println!("deleted {value}");
        Ok(())
    }
}

fn reap_command(
    mut command: Command,
) -> Result<std::sync::mpsc::Receiver<std::io::Result<std::process::ExitStatus>>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    // Create the waiter before spawning anything: thread creation failure
    // cannot strand a child. Closing the TUI reparents its supervisors normally.
    io(
        thread::Builder::new()
            .name("td-vm-reaper".into())
            .spawn(move || {
                let result = command.spawn().and_then(|mut child| child.wait());
                let _ = sender.send(result);
            }),
        "start supervisor waiter",
    )?;
    Ok(receiver)
}

fn disk_available(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir.join("disk.qcow2")) {
        Ok(meta) if meta.is_file() => command(
            Command::new("qemu-io")
                .args(["-f", "raw", "-c", "quit"])
                .arg(dir.join("disk.qcow2")),
        ),
        Ok(_) => Err("instance disk is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect disk: {e}")),
    }
}

struct Config {
    template: String,
    cpus: u32,
    memory: u32,
}

impl Config {
    fn read(dir: &Path) -> Result<Self> {
        let data = text(&dir.join("config"))?;
        let fields: Vec<&str> = data.lines().collect();
        match fields.as_slice() {
            ["td-vm-graphical-v1", template, cpus, memory] => Ok(Self {
                template: name(template)?.into(),
                cpus: number(cpus, 1, 256, "CPUs")?,
                memory: number(memory, 512, 1048576, "memory")?,
            }),
            _ => Err("invalid instance config".into()),
        }
    }
    fn write(&self, dir: &Path) -> Result<()> {
        write_new(
            &dir.join("config"),
            format!(
                "td-vm-graphical-v1\n{}\n{}\n{}\n",
                self.template, self.cpus, self.memory
            )
            .as_bytes(),
        )
    }
}

struct ProvisionHost<'a> {
    manager: &'a Manager,
    dir: &'a Path,
    lock: &'a File,
}
impl vm_provision::Host for ProvisionHost<'_> {
    fn key(&mut self) -> Result<()> {
        let workspace = vm_workspace::load(self.dir)?.ok_or("instance has no workspace plan")?;
        let reply = vm_bridge::ask(self.dir, vm_wire::KEY, workspace.id.as_bytes().to_vec())?;
        vm_wire::git_key::parse(&reply, &workspace.id).map(|_| ())
    }
    fn enroll(&mut self) -> Result<vm_wire::workspace::Plan> {
        let workspace = self.manager.enroll_locked(self.dir, self.lock)?;
        let enrollment = workspace.enrollment.as_ref()
            .filter(|state| state.phase == vm_workspace::Phase::Enrolled)
            .ok_or("workspace is not enrolled")?;
        workspace.profile.clone_plan(&workspace.id, &workspace.branch,
            workspace.start.as_deref().ok_or("starting commit is not retained")?, &enrollment.key)
    }
    fn ensure(&mut self, plan: &vm_wire::workspace::Plan) -> Result<vm_wire::workspace::Progress> {
        let reply = vm_bridge::ask(self.dir, vm_wire::WORKSPACE_ENSURE, plan.encode())?;
        vm_wire::workspace::progress(&reply, plan)
    }
    fn launch(&mut self, plan: &vm_wire::workspace::Plan) -> Result<()> {
        let reply = vm_bridge::ask(self.dir, vm_wire::WORKSPACE_TERMINAL, plan.encode())?;
        if reply != vm_wire::TASK_TERMINAL_QUEUED {
            return Err("guest did not confirm the task terminal launch".into());
        }
        Ok(())
    }
    fn report(&mut self, message: &str) {
        let message: String = term::scrub_lines(message).chars().take(768).collect();
        println!("workspace: {message}");
        if let Err(error) = vm_bridge::publish(self.dir, "provisioning", message.as_bytes()) {
            eprintln!("save workspace observation: {error}");
        }
    }
}

fn provisioning_observation(dir: &Path) -> Result<String> {
    match File::open(dir.join("provisioning")) {
        Ok(file) => {
            let mut message = String::new();
            io(file.take(4097).read_to_string(&mut message), "read provisioning observation")?;
            if message.len() > 4096 { return Err("provisioning observation exceeds limit".into()); }
            Ok(format!("Last Open provisioning observation (not a live check):\n{message}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("Open provisioning has no recorded observation.".into()),
        Err(error) => Err(format!("read provisioning observation: {error}")),
    }
}

fn cleanup_runtime(dir: &Path) -> Result<()> {
    for file in ["qmp", "pid", "accel", "guest", "bridge", "provisioning"] {
        match fs::remove_file(dir.join(file)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(format!("remove stopped {file}: {e}")),
        }
    }
    Ok(())
}

fn running(dir: &Path) -> Result<bool> {
    let running = recorded_process(dir)?;
    if !running && UnixStream::connect(dir.join("qmp")).is_ok() {
        return Err("a live QEMU monitor has no matching process record; refusing to treat the instance as stopped".into());
    }
    Ok(running)
}

fn recorded_process(dir: &Path) -> Result<bool> {
    let pid_path = dir.join("pid");
    let pid = match fs::symlink_metadata(&pid_path) {
        Ok(meta) if meta.is_file() => {
            match number(text(&pid_path)?.trim(), 1, u32::MAX, "QEMU pid") {
                Ok(pid) => pid,
                // A partial record proves nothing. The live monitor and writable
                // disk-lock probes still refuse destructive recovery of a live VM.
                Err(_) => return Ok(false),
            }
        }
        Ok(_) => return Err("QEMU pid record is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("inspect QEMU pid record: {e}")),
    };
    let cmdline = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("inspect recorded QEMU process: {e}")),
    };
    let expected = dir.join("pid");
    let words: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    Ok(words.windows(2).any(|pair| {
        pair.first() == Some(&b"-pidfile".as_slice())
            && pair.get(1) == Some(&expected.as_os_str().as_encoded_bytes())
    }))
}

fn qemu_command(dir: &Path, base: &Path, config: &Config, launch: Launch) -> Result<Command> {
    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args(["-M", "pc"]);
    for accel in launch.accelerators() {
        cmd.args(["-accel", accel]);
    }
    cmd.args(["-m", &config.memory.to_string()])
        .args([
            "-no-user-config",
            "-vga",
            "none",
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-pci,netdev=net0",
            "-device",
            "virtio-vga",
            "-audiodev",
            "none,id=audio0",
            "-device",
            "intel-hda",
            "-device",
            "hda-output,audiodev=audio0",
            "-device",
            "virtio-tablet-pci",
        ])
        .args([
            "-display",
            launch.backend(),
            "-smp",
            &config.cpus.to_string(),
        ])
        .args([
            "-name",
            &format!(
                "td-vm: {}",
                dir.file_name()
                    .and_then(|n| n.to_str())
                    .ok_or("invalid instance directory")?
            ),
        ])
        .arg("-pidfile")
        .arg(dir.join("pid"))
        .arg("-qmp")
        .arg(format!("unix:{}/qmp,server=on,wait=off", path_text(dir)?))
        .args(["-device", "virtio-serial-pci,id=tdbridge"])
        .arg("-chardev")
        .arg(format!(
            "socket,id=tdbridge,path={}/guest,server=on,wait=off",
            path_text(dir)?
        ))
        .args([
            "-device",
            &format!(
                "virtserialport,bus=tdbridge.0,chardev=tdbridge,name={}",
                vm_wire::PORT
            ),
        ])
        .arg("-serial")
        .arg(format!("file:{}/serial.log", path_text(dir)?))
        .arg("-kernel")
        .arg(base.join("bzImage"))
        .arg("-initrd")
        .arg(base.join("selector-initramfs.cpio"))
        .args(["-append", "console=ttyS0 rdinit=/init"])
        .arg("-drive")
        .arg(format!(
            "file={}/disk.qcow2,format=qcow2,if=none,id=disk0",
            path_text(dir)?
        ))
        .args(["-device", "virtio-blk-pci,drive=disk0"]);
    Ok(cmd)
}

#[derive(Clone, Copy)]
struct Launch {
    accel: &'static str,
    display: &'static str,
}
impl Launch {
    fn parse(options: &[&str]) -> Result<Self> {
        let mut launch = Self {
            accel: "auto",
            display: "gtk",
        };
        let mut accel_seen = false;
        let mut display_seen = false;
        for pair in options.chunks(2) {
            match pair {
                ["--accel", value] if !accel_seen => {
                    launch.accel = match *value {
                        "auto" => "auto",
                        "kvm" => "kvm",
                        "tcg" => "tcg",
                        _ => return Err("accel must be auto, kvm or tcg".into()),
                    };
                    accel_seen = true;
                }
                ["--display", value] if !display_seen => {
                    launch.display = match *value {
                        "gtk" => "gtk",
                        "sdl" => "sdl",
                        _ => return Err("display must be gtk or sdl".into()),
                    };
                    display_seen = true;
                }
                _ => {
                    return Err(
                        "expected --accel auto|kvm|tcg and/or --display gtk|sdl, once each".into(),
                    )
                }
            }
        }
        Ok(launch)
    }
    fn accelerators(self) -> &'static [&'static str] {
        match self.accel {
            "kvm" => &["kvm"],
            "tcg" => &["tcg"],
            _ => &["kvm", "tcg"],
        }
    }
    fn backend(self) -> &'static str {
        match self.display {
            "sdl" => "sdl,window-close=off",
            _ => "gtk,window-close=off",
        }
    }
}

struct Qmp {
    stream: BufReader<UnixStream>,
}

fn qmp_json(line: &str) -> Result<json::Json> {
    // The shared recipe parser is recursive. Bound nesting before handing it
    // even a size-bounded local monitor response.
    let mut depth = 0u32;
    let mut quoted = false;
    let mut escaped = false;
    for byte in line.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 64 {
                        return Err("QMP nesting limit exceeded".into());
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    json::parse(line)
}

impl Qmp {
    fn connect(path: &Path) -> Result<Self> {
        let stream = io(UnixStream::connect(path), "connect QEMU monitor")?;
        io(
            stream.set_read_timeout(Some(Duration::from_secs(3))),
            "set QMP read timeout",
        )?;
        io(
            stream.set_write_timeout(Some(Duration::from_secs(3))),
            "set QMP write timeout",
        )?;
        let mut qmp = Self {
            stream: BufReader::new(stream),
        };
        if qmp_json(&qmp.line()?)?.get("QMP").is_none() {
            return Err("invalid QMP greeting".into());
        }
        qmp.execute("qmp_capabilities")?;
        Ok(qmp)
    }

    fn line(&mut self) -> Result<String> {
        let mut bytes = Vec::new();
        io(
            self.stream
                .by_ref()
                .take(65537)
                .read_until(b'\n', &mut bytes),
            "read QMP response",
        )?;
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err("QMP closed or exceeded response limit".into());
        }
        String::from_utf8(bytes).map_err(|_| "QMP response is not UTF-8".into())
    }

    fn execute(&mut self, verb: &str) -> Result<json::Json> {
        io(
            self.stream
                .get_mut()
                .write_all(format!("{{\"execute\":\"{verb}\",\"id\":\"td-vm\"}}\n").as_bytes()),
            "write QMP command",
        )?;
        for _ in 0..64 {
            let line = self.line()?;
            let value = qmp_json(&line)?;
            if value.get("event").is_some() {
                continue;
            }
            if value.get("error").is_some() {
                return Err(format!("QEMU refused {verb}: {}", line.trim()));
            }
            if value.get("id").and_then(json::Json::as_str) == Some("td-vm") {
                return value
                    .get("return")
                    .cloned()
                    .ok_or("QMP response lacks return value".into());
            }
            return Err("unexpected QMP response".into());
        }
        Err("QMP event limit exceeded".into())
    }
}

#[derive(Debug)]
struct Health {
    state: String,
    disk_errors: Vec<(String, String)>,
}

impl Health {
    fn query(qmp: &mut Qmp) -> Result<Self> {
        let status = qmp.execute("query-status")?;
        let blocks = qmp.execute("query-block")?;
        Self::parse(&status, &blocks)
    }

    fn parse(status: &json::Json, blocks: &json::Json) -> Result<Self> {
        let state = status.get("status").and_then(json::Json::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
            .ok_or("QEMU status lacks a valid execution state")?;
        let runnable = match status.get("running") {
            Some(json::Json::Bool(value)) => *value,
            _ => return Err("QEMU status lacks a boolean running field".into()),
        };
        if runnable != (state == "running") {
            return Err("QEMU status has inconsistent execution fields".into());
        }
        let blocks = blocks.as_arr().filter(|v| v.len() <= 256)
            .ok_or("QEMU disk status is not a bounded device list")?;
        let mut disk_errors = Vec::new();
        for block in blocks {
            let Some(io_status) = block.get("io-status") else { continue; };
            let io_status = io_status.as_str().filter(|s| !s.is_empty() && s.len() <= 64)
                .ok_or("QEMU disk I/O status is malformed")?;
            if io_status == "ok" { continue; }
            let device = [
                block.get("device"),
                block.get("qdev"),
                block.get("inserted").and_then(|v| v.get("node-name")),
            ].into_iter().flatten().filter_map(json::Json::as_str)
                .find(|s| !s.is_empty() && s.len() <= 256)
                .ok_or("QEMU disk identity is malformed")?;
            disk_errors.push((device.to_string(), io_status.to_string()));
        }
        Ok(Self { state: state.to_string(), disk_errors })
    }

    fn runnable(&self) -> bool {
        self.state == "running"
    }

    fn resumable(&self) -> bool {
        matches!(self.state.as_str(), "paused" | "io-error" | "prelaunch")
    }

    fn describe(&self, name: &str) -> String {
        let mut report = format!("{name}: QEMU {}; guest CPUs {}", self.state,
            if self.runnable() { "running" } else { "not running" });
        for (device, status) in &self.disk_errors {
            report.push_str(&format!("\ndisk {device}: {status}"));
        }
        if self.disk_errors.iter().any(|(_, state)| state == "nospace") {
            report.push_str("\nThe host backing disk could not allocate space. Restore host capacity first.");
        }
        if self.resumable() {
            report.push_str(&format!("\nAfter correcting any fault, use td-vm resume {name} (R in the TUI)."));
        }
        report
    }
}

fn resume_qmp(qmp: &mut Qmp) -> Result<Health> {
    let health = Health::query(qmp)?;
    if health.runnable() { return Ok(health); }
    if !health.resumable() {
        return Err(format!("refusing resume from QEMU {}; inspect status and logs", health.state));
    }
    qmp.execute("cont")?;
    let health = Health::query(qmp)?;
    if !health.runnable() {
        return Err(format!("QEMU remains {}; inspect status and correct the fault before retrying", health.state));
    }
    Ok(health)
}

fn prompt(terminal: &mut term::Terminal, title: &str) -> Result<Option<String>> {
    terminal.drain_input().map_err(|e| e.to_string())?;
    let mut value = String::new();
    loop {
        let (rows, cols) = terminal.size();
        let mut frame = term::Frame::new(rows, cols);
        frame.push_text(title, term::Style::bold());
        frame.push_text(&format!("> {value}_"), term::Style::PLAIN);
        frame.push_text("Enter accepts; Escape cancels", term::Style::dim());
        terminal.draw(frame.finish()).map_err(|e| e.to_string())?;
        let keys = terminal.read_keys().map_err(|e| e.to_string())?;
        if keys.is_empty() {
            return Ok(None);
        }
        for key in keys {
            match key {
                term::Key::Enter => return Ok(Some(value)),
                term::Key::Esc | term::Key::Ctrl('c') => return Ok(None),
                term::Key::Backspace => {
                    value.pop();
                }
                term::Key::Char(ch) if !ch.is_control() && value.len() < 4096 => value.push(ch),
                _ => {}
            }
        }
    }
}

fn tui(manager: &Manager) -> Result<()> {
    let mut terminal = term::Terminal::open().map_err(|e| e.to_string())?;
    let mut selected = 0usize;
    let mut status =
        String::from("Import a clean ./build-qcow bundle with i, then create an instance with n.");
    loop {
        let rows = manager.rows()?;
        selected = selected.min(rows.len().saturating_sub(1));
        let (height, width) = terminal.size();
        let mut frame = term::Frame::new(height, width);
        frame.push_text("td-vm  Enter open · n new · i import · t templates · S stop · D delete · X cut power", term::Style::bar(term::CYAN));
        frame.push_text("h status · R resume · w workspace · W prepare · E enroll · C clone · T task terminal · l logs · v paste · c copy · f feed · s sharing · r refresh · q quit", term::Style::bar(term::CYAN));
        frame.push_text(TABLE_HEADER, term::Style::bold());
        let page = height.saturating_sub(8).max(1);
        let offset = selected.saturating_sub(page - 1);
        for (index, (_, row)) in rows.iter().enumerate().skip(offset).take(page) {
            let style = if index == selected {
                term::Style::PLAIN.with_invert()
            } else {
                term::Style::PLAIN
            };
            frame.push_text(row, style);
        }
        if rows.is_empty() {
            frame.push_text("No instances yet.", term::Style::dim());
        }
        frame.push_text("Live means a QEMU process exists. h inspects guest execution and disk faults.", term::Style::dim());
        frame.push_text(DISK_LEGEND, term::Style::dim());
        frame.push_text(&status, term::Style::fg(term::YELLOW));
        terminal.draw(frame.finish()).map_err(|e| e.to_string())?;
        let keys = terminal.read_keys().map_err(|e| e.to_string())?;
        if keys.is_empty() {
            return Ok(());
        }
        for key in keys {
            let current = rows.get(selected).map(|(name, _)| name.as_str());
            let operation = match key {
                term::Key::Char('q') | term::Key::Ctrl('c') => return Ok(()),
                term::Key::Up | term::Key::Char('k') => {
                    selected = selected.saturating_sub(1);
                    continue;
                }
                term::Key::Down | term::Key::Char('j') => {
                    selected = (selected + 1).min(rows.len().saturating_sub(1));
                    continue;
                }
                term::Key::Char('r') => {
                    status = "Refreshed".into();
                    continue;
                }
                term::Key::Char('i') => {
                    let Some(name) = prompt(&mut terminal, "Template name")? else {
                        break;
                    };
                    let Some(bundle) = prompt(&mut terminal, "Existing bundle directory (bzImage, selector-initramfs.cpio, disk, SHA256SUMS)")? else { break; };
                    terminal
                        .suspend(|| manager.import(&name, Path::new(&bundle)))
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('n') => {
                    let templates = entries(&manager.root.join("templates"))?;
                    if templates.is_empty() {
                        status = "Import a template with i first".into();
                        break;
                    }
                    let Some(name) = prompt(&mut terminal, "New instance name")? else {
                        break;
                    };
                    let default = templates.first().ok_or("no template")?;
                    let Some(template) = prompt(
                        &mut terminal,
                        &format!("Template ({}) — blank uses {default}", templates.join(", ")),
                    )?
                    else {
                        break;
                    };
                    let template = if template.is_empty() {
                        default.as_str()
                    } else {
                        template.as_str()
                    };
                    let branch = if vm_git_profile::optional(&manager.root)?.is_some() {
                        let Some(branch) = prompt(&mut terminal, &format!("Task branch — blank uses {name}"))? else { break; };
                        Some(if branch.is_empty() { name.clone() } else { branch })
                    } else { None };
                    terminal
                        .suspend(|| manager.create_workspace(&name, template, "4", "8192", branch.as_deref()))
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('W') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    let Some(branch) = prompt(&mut terminal, &format!("Task branch — blank uses {name}"))? else { break; };
                    let branch = if branch.is_empty() { name } else { branch.as_str() };
                    status = match manager.prepare_workspace(name, branch) {
                        Ok(_) => "Workspace plan saved. Press w for details; guest enrollment and cloning remain pending.".into(),
                        Err(error) => error,
                    };
                    break;
                }
                term::Key::Char('E') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    status = match manager.enroll_workspace(name) {
                        Ok(_) => "Git key enrolled, task branch reserved and starting commit retained. Press w for details; cloning remains pending.".into(),
                        Err(error) => error,
                    };
                    break;
                }
                term::Key::Char('C') if current.is_some() => {
                    status = match manager.clone_workspace(current.ok_or("no instance selected")?) {
                        Ok(message) => message,
                        Err(error) => error,
                    };
                    break;
                }
                term::Key::Char('T') if current.is_some() => {
                    status = match manager.workspace_terminal(current.ok_or("no instance selected")?) {
                        Ok(message) => message,
                        Err(error) => error,
                    };
                    break;
                }
                term::Key::Char('t') => {
                    status = format!(
                        "Templates: {}. CLI: remove-template NAME removes an unused template.",
                        entries(&manager.root.join("templates"))?.join(", ")
                    );
                    break;
                }
                term::Key::Enter if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    terminal
                        .suspend(|| manager.start(name, Launch::parse(&[])?))
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('S') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    if prompt(&mut terminal, &format!("Shut down {name}? Running tasks will stop. Type yes"))?.as_deref() != Some("yes") {
                        status = "Cancelled".into();
                    } else {
                        status = match manager.poweroff(name) {
                            Ok(message) => message,
                            Err(error) => error,
                        };
                    }
                    break;
                }
                term::Key::Char('D' | 'X') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    let deleting = key == term::Key::Char('D');
                    let title = if deleting {
                        format!("Delete {name} and ALL its disk data? Unsubmitted work is unknown. Type {name} to confirm")
                    } else {
                        format!(
                            "Cut power to {name}? Unsaved work may be lost. Type {name} to confirm"
                        )
                    };
                    if prompt(&mut terminal, &title)?.as_deref() != Some(name) {
                        status = "Cancelled".into();
                        break;
                    }
                    terminal
                        .suspend(|| {
                            if deleting {
                                manager.delete(name)
                            } else {
                                manager.stop(name)
                            }
                        })
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('v') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    match vm_clipboard::capture(&mut terminal, name) {
                        Ok(bytes) => {
                            let Some(answer) = prompt(
                                &mut terminal,
                                &format!(
                                    "Send {} bytes to {name}'s clipboard? Type yes",
                                    bytes.len()
                                ),
                            )?
                            else {
                                break;
                            };
                            if answer != "yes" {
                                status = "Cancelled".into();
                                break;
                            }
                            manager.bridge(name, vm_wire::PUT, bytes).map(|_| ())
                        }
                        Err(error) => Err(error),
                    }
                }
                term::Key::Char('c') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    manager.bridge(name, vm_wire::GET, Vec::new())
                        .and_then(|bytes| vm_clipboard::copy(&mut terminal, &bytes))
                }
                term::Key::Char('f') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    let Some(port) = prompt(
                        &mut terminal,
                        "Host td-feed port (run td-feed ensure-serve on host); off disables",
                    )?
                    else {
                        break;
                    };
                    terminal
                        .suspend(|| manager.feed(name, &port))
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('s') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    let Some(mode) =
                        prompt(&mut terminal, "Explicit clipboard transfers: on or off")?
                    else {
                        break;
                    };
                    terminal
                        .suspend(|| manager.sharing(name, &mode))
                        .map_err(|e| e.to_string())?
                }
                term::Key::Char('R') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    status = manager.resume(name).unwrap_or_else(|e| e);
                    break;
                }
                term::Key::Char('l' | 'h' | 'w') if current.is_some() => {
                    let name = current.ok_or("no instance selected")?;
                    let logs = if key == term::Key::Char('h') {
                        manager.status(name).unwrap_or_else(|e| e)
                    } else if key == term::Key::Char('w') {
                        manager.workspace(name).unwrap_or_else(|e| e)
                    } else {
                        manager.logs(name)?
                    };
                    let (height, width) = terminal.size();
                    let mut frame = term::Frame::new(height, width);
                    let title = if key == term::Key::Char('h') {
                        "QEMU status snapshot — any key returns"
                    } else if key == term::Key::Char('w') {
                        "Workspace plan — any key returns"
                    } else {
                        "Log tail — any key returns"
                    };
                    frame.push_text(title, term::Style::bar(term::CYAN));
                    let lines: Vec<_> = logs.lines().collect();
                    for line in lines
                        .iter()
                        .skip(lines.len().saturating_sub(height.saturating_sub(2)))
                    {
                        frame.push_text(line, term::Style::PLAIN);
                    }
                    terminal.drain_input().map_err(|e| e.to_string())?;
                    terminal.draw(frame.finish()).map_err(|e| e.to_string())?;
                    terminal.read_keys().map_err(|e| e.to_string())?;
                    break;
                }
                _ => continue,
            };
            status = match operation {
                Ok(()) if key == term::Key::Char('c') =>
                    "Copy requested through the host terminal (requires OSC 52 support).".into(),
                Ok(()) => "Done".into(),
                Err(e) => e,
            };
            terminal.drain_input().map_err(|e| e.to_string())?;
            break;
        }
    }
}
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn automatic_worker_defers_contention_and_retains_only_its_own_lifetime() {
        let scratch = Scratch::new(); let manager = Manager::new(&scratch.0).unwrap();
        let dir = scratch.0.join("instances/worker"); private_dir(&dir).unwrap();
        let key = vm_wire::workspace::example().host_key;
        let profile = vm_git_profile::Profile::parse(&format!("TDVM-GIT-PROFILE-1\nrepository=/srv/git/td.git\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid=1001\nsocket=/home/test/.td-vm-registrar\nregistrar=/usr/local/libexec/td-vm-registrar\ngit=/bin/git\nhost-key={key}\nauthor-name=Fixture\nauthor-email=fixture@example.invalid\n")).unwrap();
        vm_workspace::Workspace::new("task", profile).unwrap().publish(&dir).unwrap();
        let operation = manager.lock("instance-worker").unwrap();
        let worker = vm_provision::Worker::start(scratch.0.clone(), "worker".into(), manager.lock("run-worker").unwrap());
        thread::sleep(Duration::from_millis(250));
        assert!(manager.active("worker").unwrap());
        assert!(!dir.join("provisioning").exists(), "no attempt while another operation owns the instance");
        drop(operation);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !dir.join("provisioning").exists() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
        assert!(fs::read_to_string(dir.join("provisioning")).unwrap().starts_with("Waiting for"));
        drop(worker);
        while manager.active("worker").unwrap() && Instant::now() < deadline { thread::sleep(Duration::from_millis(10)); }
        assert!(!manager.active("worker").unwrap());
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = env::temp_dir().join(format!(
                "tdvm-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn config() -> Config {
        Config {
            template: "base".into(),
            cpus: 4,
            memory: 8192,
        }
    }
    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect()
    }

    fn checksums(dir: &Path, disk: &str) {
        let mut manifest = String::new();
        for file in ["bzImage", "selector-initramfs.cpio", disk] {
            manifest.push_str(&format!(
                "{}  {file}\n",
                sha256::sha256_file(&dir.join(file)).unwrap()
            ));
        }
        fs::write(dir.join("SHA256SUMS"), manifest).unwrap();
    }

    #[test]
    fn copied_bundle_bytes_must_match_each_unique_checksum() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let dest = scratch.0.join("dest");
        private_dir(&source).unwrap();
        private_dir(&dest).unwrap();
        for file in ["bzImage", "selector-initramfs.cpio", "td-system.img"] {
            fs::write(source.join(file), b"fixture").unwrap();
        }
        checksums(&source, "td-system.img");
        assert_eq!(
            stage_bundle(&source, &dest).unwrap(),
            ("td-system.img", "raw")
        );
        fs::write(source.join("bzImage"), b"truncated").unwrap();
        assert!(stage_bundle(&source, &dest)
            .unwrap_err()
            .contains("checksum mismatch for bzImage"));
        checksums(&source, "td-system.img");
        let manifest = fs::read_to_string(source.join("SHA256SUMS")).unwrap();
        fs::write(source.join("SHA256SUMS"), format!("{manifest}{manifest}")).unwrap();
        assert!(stage_bundle(&source, &dest)
            .unwrap_err()
            .contains("duplicate"));
        fs::write(source.join("td-system.qcow2"), b"ambiguous").unwrap();
        assert!(stage_bundle(&source, &dest)
            .unwrap_err()
            .contains("exactly one"));
    }

    #[test]
    fn names_and_metadata_refuse_path_and_option_injection() {
        for bad in [
            "", "../other", "-qmp", "a/b", "a.b", "a b", "a\n", "A", "a;true",
        ] {
            assert!(name(bad).is_err(), "{bad:?}");
        }
        assert!(name("worker-01").is_ok());
        assert!(name(&"a".repeat(33)).is_err());
        for bad in ["/tmp/x,y", "/tmp/x\ny", "/tmp/%h", "/tmp/\"x", "/tmp/\\x"] {
            assert!(path_text(Path::new(bad)).is_err());
        }
        let scratch = Scratch::new();
        config().write(&scratch.0).unwrap();
        assert_eq!(Config::read(&scratch.0).unwrap().template, "base");
        fs::write(
            scratch.0.join("config"),
            "td-vm-graphical-v1\n../base\n4\n8192\n",
        )
        .unwrap();
        assert!(Config::read(&scratch.0).is_err());
        fs::write(scratch.0.join("config"), "x".repeat(65537)).unwrap();
        assert!(Config::read(&scratch.0).is_err());
    }

    #[test]
    fn private_state_and_locks_refuse_aliases_and_serialize_only_one_instance() {
        let scratch = Scratch::new();
        let manager = Manager::new(&scratch.0).unwrap();
        let lock = manager.lock("instance-one").unwrap();
        assert!(manager.lock("instance-one").is_err());
        assert!(manager.lock("instance-two").is_ok());
        assert!(!manager.active("pending").unwrap());
        let lifetime = manager.lock("run-pending").unwrap();
        assert!(manager.active("pending").unwrap());
        drop(lifetime);
        assert!(!manager.active("pending").unwrap());
        drop(lock);
        assert!(manager.lock("instance-one").is_ok());
        symlink(&scratch.0, scratch.0.join("alias")).unwrap();
        assert!(Manager::new(&scratch.0.join("alias")).is_err());
        symlink(&scratch.0, scratch.0.join("instances/evil")).unwrap();
        assert!(manager.instance("evil").is_err());
        assert!(manager.delete("../templates").is_err());
    }

    #[test]
    fn stale_pid_never_claims_an_unrelated_process_and_live_monitor_fails_closed() {
        let scratch = Scratch::new();
        assert!(!running(&scratch.0).unwrap());
        fs::write(scratch.0.join("pid"), std::process::id().to_string()).unwrap();
        assert!(!running(&scratch.0).unwrap());
        let _listener = UnixListener::bind(scratch.0.join("qmp")).unwrap();
        assert!(running(&scratch.0).is_err());
        fs::remove_file(scratch.0.join("pid")).unwrap();
        assert!(running(&scratch.0).is_err());
    }

    #[test]
    fn qmp_negotiates_and_skips_events_but_refuses_errors() {
        let scratch = Scratch::new();
        let path = scratch.0.join("qmp");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut stream = BufReader::new(stream);
            stream.get_mut().write_all(b"{\"QMP\":{}}\r\n").unwrap();
            let mut line = String::new();
            stream.read_line(&mut line).unwrap();
            assert!(line.contains("qmp_capabilities"));
            stream
                .get_mut()
                .write_all(b"{\"event\":\"RESET\"}\n{\"return\":{},\"id\":\"td-vm\"}\n")
                .unwrap();
            line.clear();
            stream.read_line(&mut line).unwrap();
            assert!(line.contains("quit"));
            stream
                .get_mut()
                .write_all(b"{\"error\":{\"desc\":\"denied\"},\"id\":\"td-vm\"}\n")
                .unwrap();
        });
        let mut qmp = Qmp::connect(&path).unwrap();
        assert!(qmp.execute("quit").unwrap_err().contains("denied"));
        worker.join().unwrap();
    }

    #[test]
    fn health_refuses_malformed_status_and_reports_disk_exhaustion() {
        let blocks = json::parse(r#"[{"device":"disk0","io-status":"nospace"}]"#).unwrap();
        let status = json::parse(r#"{"status":"io-error","running":false}"#).unwrap();
        let health = Health::parse(&status, &blocks).unwrap();
        assert!(!health.runnable());
        assert!(health.resumable());
        let report = health.describe("worker");
        assert!(report.contains("disk disk0: nospace"));
        assert!(report.contains("Restore host capacity"));
        assert!(report.contains("td-vm resume worker"));
        for (blocks, label) in [
            (r#"[{"device":"","qdev":"/machine/disk","io-status":"nospace"}]"#, "/machine/disk"),
            (r#"[{"device":"","inserted":{"node-name":"data"},"io-status":"failed"}]"#, "data"),
        ] {
            let health = Health::parse(&status, &json::parse(blocks).unwrap()).unwrap();
            assert!(health.describe("worker").contains(&format!("disk {label}:")));
        }
        for bad in [
            r#"{"status":"paused","running":true}"#,
            r#"{"status":"running","running":false}"#,
            r#"{"status":"paused"}"#,
            r#"{"status":"paused","running":"false"}"#,
            r#"{"status":"paused\u001b","running":false}"#,
        ] {
            assert!(Health::parse(&json::parse(bad).unwrap(), &blocks).is_err());
        }
        assert!(Health::parse(&status, &json::Json::Null).is_err());
        assert!(Health::parse(&status, &json::parse(r#"[{"io-status":false}]"#).unwrap()).is_err());
        let future = json::parse(r#"{"status":"future-state","running":false}"#).unwrap();
        assert!(!Health::parse(&future, &blocks).unwrap().resumable());
    }

    fn scripted_qmp(script: Vec<(&'static str, &'static str)>) -> (Qmp, thread::JoinHandle<()>) {
        let (client, server) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let worker = thread::spawn(move || {
            let mut server = BufReader::new(server);
            for (verb, response) in script {
                let mut request = String::new();
                server.read_line(&mut request).unwrap();
                let request = json::parse(&request).unwrap();
                assert_eq!(request.get("execute").and_then(json::Json::as_str), Some(verb));
                writeln!(server.get_mut(), "{{\"return\":{response},\"id\":\"td-vm\"}}").unwrap();
            }
            let mut request = String::new();
            assert_eq!(server.read_line(&mut request).unwrap(), 0, "unexpected mutation: {request}");
        });
        (Qmp { stream: BufReader::new(client) }, worker)
    }

    #[test]
    fn resume_checks_the_state_and_verifies_execution_after_cont() {
        let paused = r#"{"status":"io-error","running":false}"#;
        let active = r#"{"status":"running","running":true}"#;
        let blocks = r#"[{"device":"disk0","io-status":"nospace"}]"#;
        let (mut qmp, worker) = scripted_qmp(vec![
            ("query-status", paused), ("query-block", blocks), ("cont", "{}"),
            ("query-status", active), ("query-block", "[]"),
        ]);
        assert!(resume_qmp(&mut qmp).unwrap().runnable());
        drop(qmp); worker.join().unwrap();
        let (mut qmp, worker) = scripted_qmp(vec![
            ("query-status", paused), ("query-block", blocks), ("cont", "{}"),
            ("query-status", paused), ("query-block", blocks),
        ]);
        assert!(resume_qmp(&mut qmp).unwrap_err().contains("remains io-error"));
        drop(qmp); worker.join().unwrap();
        let (mut qmp, worker) = scripted_qmp(vec![
            ("query-status", r#"{"status":"internal-error","running":false}"#),
            ("query-block", "[]"),
        ]);
        assert!(resume_qmp(&mut qmp).unwrap_err().contains("refusing resume"));
        drop(qmp); worker.join().unwrap();
        let (mut qmp, worker) = scripted_qmp(vec![
            ("query-status", active), ("query-block", "[]"),
        ]);
        assert!(resume_qmp(&mut qmp).unwrap().runnable());
        drop(qmp); worker.join().unwrap();
    }

    #[test]
    fn qmp_refuses_oversize_responses() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let _ = server.write_all(&vec![b'x'; 65537]);
        });
        let mut qmp = Qmp {
            stream: BufReader::new(client),
        };
        assert!(qmp.line().is_err());
        worker.join().unwrap();
        assert!(qmp_json(&format!("{}0{}", "[".repeat(65), "]".repeat(65))).is_err());
        assert!(qmp_json(r#"{"return":"[[[quoted text]]]","id":"td-vm"}"#).is_ok());
    }

    fn constant<'a>(source: &'a str, name: &str) -> &'a str {
        source
            .split_once(&format!("const {name}: "))
            .unwrap()
            .1
            .split_once('=')
            .unwrap()
            .1
            .split_once(';')
            .unwrap()
            .0
            .trim()
    }

    fn platform_tokens(body: &str, profile: &str, boot: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for ch in body.chars().chain(std::iter::once(',')) {
            if ch == '"' {
                quoted = !quoted;
            }
            if ch == ',' && !quoted {
                let token = current.trim();
                if !token.is_empty() {
                    let resolved = match token {
                        "MEMORY_FLAG" => constant(profile, token),
                        "SYSTEM_GUEST_MEMORY_MIB" | "QEMU_USER_NETDEV" | "QEMU_USER_NET_DEVICE" => {
                            constant(boot, token)
                        }
                        literal if literal.starts_with('"') && literal.ends_with('"') => literal,
                        unknown => panic!("new canonical platform token needs mapping: {unknown}"),
                    };
                    tokens.push(resolved.trim_matches('"').to_string());
                }
                current.clear();
            } else {
                current.push(ch);
            }
        }
        tokens
    }

    fn canonical_platform(config: &Config) -> Vec<String> {
        let profile = include_str!("../../../recipes/src/bin/td_recipe_eval/checks/vm_profile.rs");
        let boot = include_str!("../../../recipes/src/bin/td_recipe_eval/checks/qemu_boot.rs");
        let machine = constant(profile, "MACHINE")
            .strip_prefix("&[")
            .unwrap()
            .strip_suffix(']')
            .unwrap();
        let mut result = platform_tokens(machine, profile, boot);
        let body = profile
            .split_once("pub(crate) fn platform()")
            .unwrap()
            .1
            .split_once("vec![")
            .unwrap()
            .1
            .split_once(']')
            .unwrap()
            .0;
        let mut platform = platform_tokens(body, profile, boot).into_iter();
        while let Some(token) = platform.next() {
            if token == "-no-reboot" {
                continue;
            }
            result.push(token.clone());
            if token == "-m" {
                platform.next().unwrap();
                result.push(config.memory.to_string());
            }
        }
        result
    }

    fn managed_platform(values: &[String]) -> Vec<String> {
        let mut result = Vec::new();
        let mut iter = values.iter();
        while let Some(flag) = iter.next() {
            if flag == "-no-user-config" {
                result.push(flag.clone());
                continue;
            }
            let value = iter.next().unwrap();
            match flag.as_str() {
                "-accel" | "-display" | "-smp" | "-name" | "-pidfile" | "-qmp" | "-serial"
                | "-kernel" | "-initrd" | "-append" | "-drive" | "-chardev" => {}
                "-device"
                    if value == "virtio-blk-pci,drive=disk0"
                        || value == "virtio-serial-pci,id=tdbridge"
                        || value.starts_with("virtserialport,bus=tdbridge.0,") => {}
                _ => {
                    result.push(flag.clone());
                    result.push(value.clone());
                }
            }
        }
        result
    }

    #[test]
    fn supervisor_waiter_reports_spawn_failure_and_reaps_success() {
        let missing = reap_command(Command::new("/no-such-td-vm-supervisor")).unwrap();
        assert!(missing
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err());
        let mut child = Command::new(env::current_exe().unwrap());
        child
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let completion = reap_command(child).unwrap();
        assert!(completion
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .success());
    }

    #[test]
    fn graphical_launch_uses_canonical_devices_and_acceleration_fallback() {
        for (option, expected) in [
            ("auto", vec!["kvm", "tcg"]),
            ("kvm", vec!["kvm"]),
            ("tcg", vec!["tcg"]),
        ] {
            let values = args(
                &qemu_command(
                    Path::new("/tmp/instance"),
                    Path::new("/tmp/base"),
                    &config(),
                    Launch::parse(&["--accel", option]).unwrap(),
                )
                .unwrap(),
            );
            let pair = |a: &str, b: &str| values.windows(2).any(|v| v == [a, b]);
            assert!(pair("-M", "pc"));
            assert_eq!(
                values
                    .windows(2)
                    .filter(|p| p.first().map(String::as_str) == Some("-accel"))
                    .filter_map(|p| p.get(1).map(String::as_str))
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(!values.contains(&"-cpu".into()));
            assert!(pair("-display", "gtk,window-close=off"));
            assert!(pair("-netdev", "user,id=net0"));
            assert!(pair(
                "-drive",
                "file=/tmp/instance/disk.qcow2,format=qcow2,if=none,id=disk0"
            ));
            assert!(!values.iter().any(|v| v.contains("hostfwd")
                || v.contains("snapshot")
                || v.contains("daemonize")));
            assert_eq!(managed_platform(&values), canonical_platform(&config()));
            assert!(pair(
                "-chardev",
                "socket,id=tdbridge,path=/tmp/instance/guest,server=on,wait=off"
            ));
            assert!(pair(
                "-device",
                "virtserialport,bus=tdbridge.0,chardev=tdbridge,name=org.td.vm.1"
            ));
            assert!(!values
                .iter()
                .any(|value| value.contains("vdagent") || value.contains("spice")));
            let profile =
                include_str!("../../../recipes/src/bin/td_recipe_eval/checks/vm_profile.rs");
            for (key, expected) in [
                ("APPEND", "console=ttyS0 rdinit=/init"),
                ("DISK_DEVICE", "virtio-blk-pci,drive=disk0"),
                ("DRIVE_ID", "disk0"),
                ("KERNEL_NAME", "bzImage"),
                ("INITRD_NAME", "selector-initramfs.cpio"),
            ] {
                assert_eq!(constant(profile, key).trim_matches('"'), expected);
            }
            assert!(!values.contains(&"-no-reboot".into()));
        }
        for options in [
            &["--accel"][..],
            &["--accel", "host"],
            &["--display", "none"],
            &["--accel", "kvm", "--accel", "tcg"],
        ] {
            assert!(Launch::parse(options).is_err());
        }
        assert_eq!(
            Launch::parse(&["--display", "sdl"]).unwrap().backend(),
            "sdl,window-close=off"
        );
    }

    #[test]
    fn pruning_skips_live_staging_and_status_does_not_require_catalog_lock() {
        let scratch = Scratch::new();
        let manager = Manager::new(&scratch.0).unwrap();
        let (staging, guard) = manager.staging("import").unwrap();
        manager.prune().unwrap();
        assert!(staging.exists());
        let catalog = manager.lock("catalog").unwrap();
        assert!(manager.prune().is_err());
        manager.list().unwrap();
        manager.templates().unwrap();
        drop(catalog);
        drop(guard);
        manager.prune().unwrap();
        assert!(!staging.exists());
        assert!(!fs::read_dir(manager.root.join("locks"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("stage-")));
        let absent = scratch.0.join("absent");
        assert!(Manager::existing(&absent).is_err());
        assert!(!absent.exists());
    }

    #[test]
    fn capacity_header_refuses_truncation_unknown_format_and_non_files() {
        let scratch = Scratch::new();
        let path = scratch.0.join("disk.qcow2");
        let mut header = [0u8; 104];
        header[..4].copy_from_slice(b"QFI\xfb");
        header[4..8].copy_from_slice(&3u32.to_be_bytes());
        header[24..32].copy_from_slice(&(10u64 * 1024 * 1024 * 1024).to_be_bytes());
        header[100..104].copy_from_slice(&104u32.to_be_bytes());
        for length in [0, 31, 71, 72, 103] {
            fs::write(&path, &header[..length]).unwrap();
            assert!(disk_usage(&path).is_err(), "accepted length {length}");
        }
        fs::write(&path, header).unwrap();
        assert_eq!(disk_usage(&path).unwrap().1, 10240);
        for length in [72u32, 105, 112] {
            header[100..104].copy_from_slice(&length.to_be_bytes());
            fs::write(&path, header).unwrap();
            assert!(disk_usage(&path).is_err());
        }
        let mut extended = header.to_vec();
        extended.resize(112, 0);
        fs::write(&path, extended).unwrap();
        assert_eq!(disk_usage(&path).unwrap().1, 10240);
        header[4..8].copy_from_slice(&2u32.to_be_bytes());
        fs::write(&path, &header[..72]).unwrap();
        assert_eq!(disk_usage(&path).unwrap().1, 10240);
        for version in [0u32, 1, 4, u32::MAX] {
            header[4..8].copy_from_slice(&version.to_be_bytes());
            fs::write(&path, header).unwrap();
            assert!(disk_usage(&path).is_err());
        }
        header[4..8].copy_from_slice(&2u32.to_be_bytes());
        header[0] = 0;
        fs::write(&path, header).unwrap();
        assert!(disk_usage(&path).is_err());
        header[0] = b'Q';
        fs::write(&path, header).unwrap();
        assert_eq!(disk_usage(&path).unwrap().1, 10240);
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(disk_usage(&link).is_err());
        assert!(disk_usage(&scratch.0).is_err());
    }

    fn firmware_command(dir: &Path, accel: &str) -> Command {
        // Exercise the production devices/acceleration/disk command with only
        // boot payloads removed and display disabled: no td image in host gates.
        let command = qemu_command(
            dir,
            Path::new("/unused"),
            &Config {
                template: "base".into(),
                cpus: 1,
                memory: 64,
            },
            Launch::parse(&["--accel", accel]).unwrap(),
        )
        .unwrap();
        let mut child = Command::new(command.get_program());
        let values = args(&command);
        let mut iter = values.iter();
        while let Some(flag) = iter.next() {
            if flag == "-no-user-config" {
                child.arg(flag);
                continue;
            }
            let value = iter.next().unwrap();
            match flag.as_str() {
                "-kernel" | "-initrd" | "-append" => {}
                "-display" => {
                    child.args(["-display", "none"]);
                }
                "-accel" if value == "tcg" => {
                    child.args(["-accel", "tcg,tb-size=16"]);
                }
                _ => {
                    child.args([flag, value]);
                }
            }
        }
        child.arg("-S");
        child
    }

    fn wait_running(dir: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !dir.join("accel").exists() {
            assert!(
                Instant::now() < deadline,
                "QEMU did not become available: {}",
                dir.display()
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(running(dir).unwrap());
    }

    struct StopGuests(Vec<PathBuf>);
    impl Drop for StopGuests {
        fn drop(&mut self) {
            for dir in &self.0 {
                if let Ok(mut qmp) = Qmp::connect(&dir.join("qmp")) {
                    let _ = qmp.execute("quit");
                }
            }
        }
    }

    // Host preflight opts into ignored tests; the target gate has no QEMU.
    #[test]
    #[ignore]
    fn real_qemu_two_overlays_lifecycle_and_supervisor_loss_exclusion() {
        let scratch = Scratch::new();
        let manager = Manager::new(&scratch.0.join("state")).unwrap();
        let bundle = scratch.0.join("bundle");
        private_dir(&bundle).unwrap();
        fs::write(bundle.join("bzImage"), b"fixture").unwrap();
        fs::write(bundle.join("selector-initramfs.cpio"), b"fixture").unwrap();
        command(
            Command::new("qemu-img")
                .args(["create", "-f", "qcow2"])
                .arg(bundle.join("td-system.qcow2"))
                .arg("16M"),
        )
        .unwrap();
        checksums(&bundle, "td-system.qcow2");
        manager.import("base", &bundle).unwrap();
        verify_template(&manager.template("base").unwrap()).unwrap();
        for name in ["one", "two"] {
            manager.create(name, "base", "1", "512").unwrap();
        }
        let one = manager.instance("one").unwrap();
        let two = manager.instance("two").unwrap();
        assert_eq!(disk_usage(&one.join("disk.qcow2")).unwrap().1, 16);
        assert_eq!(disk_usage(&one.join("disk.qcow2")).unwrap().0,
            fs::metadata(one.join("disk.qcow2")).unwrap().blocks() / 2048);
        command(
            Command::new("qemu-io")
                .args(["-f", "qcow2", "-c", "write -P 0x5a 0 4096"])
                .arg(one.join("disk.qcow2")),
        )
        .unwrap();
        assert_eq!(disk_usage(&one.join("disk.qcow2")).unwrap().0,
            fs::metadata(one.join("disk.qcow2")).unwrap().blocks() / 2048);
        for path in [manager.template("base").unwrap(), two.clone()] {
            command(
                Command::new("qemu-io")
                    .args(["-r", "-f", "qcow2", "-c", "read -P 0 0 4096"])
                    .arg(path.join("disk.qcow2")),
            )
            .unwrap();
        }
        assert!(manager.remove_template("base").is_err());
        let _cleanup = StopGuests(vec![one.clone(), two.clone()]);
        let root = manager.root.clone();
        let cmd = firmware_command(&one, "auto");
        let supervisor =
            thread::spawn(move || Manager::existing(&root).unwrap().run_qemu("one", cmd));
        wait_running(&one);
        let (allocated, capacity) = disk_usage(&one.join("disk.qcow2")).unwrap();
        assert_eq!(capacity, 16);
        assert_eq!(
            allocated,
            fs::metadata(one.join("disk.qcow2")).unwrap().blocks() / 2048
        );
        assert!(manager.status("one").unwrap().contains("prelaunch"));
        manager.resume("one").unwrap();
        Qmp::connect(&one.join("qmp")).unwrap().execute("stop").unwrap();
        assert!(manager.status("one").unwrap().contains("paused"));
        assert!(manager.rows().unwrap().iter().any(|(name, row)| name == "one" && row.contains("live")));
        assert!(manager.delete("one").is_err(), "a paused guest still owns its disk");
        manager.resume("one").unwrap();
        assert!(manager.status("one").unwrap().contains("guest CPUs running"));
        let held = Qmp::connect(&one.join("qmp")).unwrap();
        assert!(manager.start("one", Launch::parse(&[]).unwrap()).is_ok(),
            "an occupied monitor does not turn an existing open into a failed launch");
        drop(held);
        assert!(
            manager.start("one", Launch::parse(&[]).unwrap()).is_ok(),
            "duplicate open discovers existing QEMU"
        );
        assert!(manager.delete("one").is_err());
        assert!(disk_available(&one).is_err());
        // Simulate the supervisor disappearing: QEMU alone inherits the locked
        // file description. It must keep its guest protected without a parent.
        let lifetime = manager.lock("run-two").unwrap();
        let mut orphan = firmware_command(&two, "tcg")
            .stdin(Stdio::from(lifetime.try_clone().unwrap()))
            .spawn()
            .unwrap();
        drop(lifetime);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !running(&two).unwrap_or(false) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            manager.lock("run-two").is_err(),
            "QEMU must retain its inherited lifetime lock"
        );
        assert!(manager.delete("two").is_err());
        assert!(!Qmp::connect(&two.join("qmp"))
            .unwrap()
            .execute("query-kvm")
            .unwrap()
            .get("enabled")
            .unwrap()
            .is_true());
        manager.stop("one").unwrap();
        supervisor.join().unwrap().unwrap();
        assert!(running(&two).unwrap(), "stopping one preserves its sibling");
        manager.delete("one").unwrap();
        manager.stop("two").unwrap();
        assert!(orphan.wait().unwrap().success());
        manager.delete("two").unwrap();
        manager.remove_template("base").unwrap();
        assert!(entries(&manager.root.join("instances")).unwrap().is_empty());
    }
}
