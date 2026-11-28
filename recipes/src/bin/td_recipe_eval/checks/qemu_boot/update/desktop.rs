//! The installed viewer and helper share an enrolled guest's ordinary session.
//! QMP observes the physical output independently of either desktop stream.
use super::super as boot;
use super::{Guest, Options, Result};
use crate::check_runner::RecipeCheckRunner;
use std::fs::{self, File, Permissions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use td_engine::allocation;

const RECORD: &str = "td-secret/tests/login_record_vectors.txt";
const WAIT: Duration = Duration::from_secs(40);
const LAYOUT: &str = "/bin/td-ctl --socket /run/td-compositor/1000/td-control layout";

fn error(e: impl std::fmt::Display) -> String {
    allocation::format(format_args!("{e}")).unwrap_or_default()
}

fn no_storage<T>(_: T) -> String {
    String::new()
}

fn child(parent: &Path, name: &str) -> Result<PathBuf> {
    let mut bytes = allocation::copy(parent.as_os_str().as_bytes()).map_err(no_storage)?;
    if !bytes.ends_with(b"/") && !name.is_empty() {
        allocation::push(&mut bytes, b'/').map_err(no_storage)?;
    }
    allocation::extend_from_slice(&mut bytes, name.as_bytes()).map_err(no_storage)?;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

fn read_bytes(mut input: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = input.read(&mut chunk).map_err(error)?;
        if n == 0 {
            return Ok(bytes);
        }
        if n > limit.saturating_sub(bytes.len()) {
            return Err(error("desktop oracle input exceeds bound"));
        }
        allocation::extend_from_slice(&mut bytes, chunk.get(..n).ok_or("invalid read count")?)
            .map_err(no_storage)?;
    }
}

fn text_file(path: &Path, limit: usize) -> Result<String> {
    String::from_utf8(read_bytes(File::open(path).map_err(error)?, limit)?).map_err(error)
}

pub(in super::super) fn stage(keys: &Path, seed: &Path) -> Result<()> {
    let state = child(seed, "@var/lib/td")?;
    for path in ["@var/lib", "@var/lib/td", "@var/home"] {
        let path = child(seed, path)?;
        fs::create_dir_all(&path)
            .and_then(|()| fs::set_permissions(path, Permissions::from_mode(0o755)))
            .map_err(error)?;
    }
    for name in ["login", "ssh"] {
        let dir = child(&state, name)?;
        fs::create_dir_all(&dir)
            .and_then(|()| fs::set_permissions(&dir, Permissions::from_mode(0o700)))
            .map_err(error)?;
    }
    let vectors = text_file(&child(keys, "public-vectors")?, 64 * 1024)?;
    let encoded = vectors
        .lines()
        .find_map(|l| l.strip_prefix("record "))
        .ok_or("missing enrollment fixture")?;
    let scripts = child(seed, "@var/home/tester/.ssh")?;
    fs::create_dir_all(&scripts).map_err(error)?;
    fs::copy(child(keys, "key.pub")?, child(&scripts, "authorized_keys")?).map_err(error)?;
    let mut record = super::private_file(&child(&state, "login/1000")?)?;
    for byte in encoded.as_bytes().chunks(2) {
        if byte.len() != 2 {
            return Err("incomplete enrollment fixture".into());
        }
        let b = u8::from_str_radix(std::str::from_utf8(byte).map_err(error)?, 16).map_err(error)?;
        record.write_all(&[b]).map_err(error)?;
    }
    for (name, dest) in [
        ("host", "ssh_host_ed25519_key"),
        ("host.pub", "ssh_host_ed25519_key.pub"),
    ] {
        fs::copy(child(keys, name)?, child(&child(&state, "ssh")?, dest)?).map_err(error)?;
    }
    Ok(())
}

struct Process(Child);
impl Process {
    fn start(command: &mut Command) -> Result<Self> {
        command.spawn().map(Self).map_err(error)
    }
    fn alive(&mut self) -> Result<()> {
        if let Some(status) = self.0.try_wait().map_err(error)? {
            return Err(error(format_args!(
                "desktop fixture process exited: {status}"
            )));
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tool(name: &str) -> Result<String> {
    boot::find_qemu_tool(name).ok_or_else(|| error(format_args!("desktop oracle requires {name}")))
}

fn wait<T>(what: &str, mut poll: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(value) = poll()? {
            return Ok(value);
        }
        if Instant::now() >= end {
            return Err(error(format_args!("timed out waiting for {what}")));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Login<'a> {
    work: &'a Path,
    ssh: String,
}
impl<'a> Login<'a> {
    fn prepare(work: &'a Path, port: u16) -> Result<Self> {
        let keygen = tool("ssh-keygen")?;
        for name in ["key", "host"] {
            let status = Command::new(&keygen)
                .args(["-q", "-t", "ed25519", "-N", "", "-f", name])
                .current_dir(work)
                .stdin(Stdio::null())
                .status()
                .map_err(error)?;
            if !status.success() {
                return Err(error(format_args!("generate {name} key: {status}")));
            }
        }
        let host = text_file(&child(work, "host.pub")?, 4096)?;
        fs::write(
            child(work, "known")?,
            allocation::format(format_args!("desktop-oracle {host}")).map_err(no_storage)?,
        )
        .map_err(error)?;
        // Relative key paths are resolved in the private working directory
        // of both SSH clients. An absolute config path is passed with -F.
        let config = allocation::format(format_args!(
            "Host desktop-oracle\n HostName 127.0.0.1\n Port {port}\n User tester\n \
             IdentityFile key\n IdentityAgent none\n IdentitiesOnly yes\n \
             HostKeyAlias desktop-oracle\n UserKnownHostsFile known\n GlobalKnownHostsFile /dev/null\n \
             StrictHostKeyChecking yes\n BatchMode yes\n ConnectTimeout 3\n \
             ServerAliveInterval 3\n ServerAliveCountMax 2\n"
        )).map_err(no_storage)?;
        fs::write(child(work, "config")?, config).map_err(error)?;
        let bin = child(work, "bin")?;
        fs::create_dir(&bin).map_err(error)?;
        let wrapper = child(&bin, "ssh")?;
        fs::write(
            &wrapper,
            b"#!/bin/sh\nexec \"$TD_ORACLE_SSH\" -F \"$TD_ORACLE_CONFIG\" \"$@\"\n",
        )
        .and_then(|()| fs::set_permissions(&wrapper, Permissions::from_mode(0o700)))
        .map_err(error)?;
        Ok(Self {
            work,
            ssh: tool("ssh")?,
        })
    }

    fn read(&self, command: &str) -> Result<Option<String>> {
        let file = child(self.work, "observation")?;
        let mut child = Process::start(
            Command::new(&self.ssh)
                .current_dir(self.work)
                .args(["-F", "config", "desktop-oracle", command])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .stdout(File::create(&file).map_err(error)?),
        )?;
        let status = wait("SSH observation", || child.0.try_wait().map_err(error))?;
        if !status.success() {
            return Ok(None);
        }
        let contents = text_file(&file, 256 * 1024)?;
        Ok(Some(contents))
    }
}

struct Viewer {
    process: Process,
    _compositor: Process,
    control: PathBuf,
    session: String,
    serial: u64,
}
impl Viewer {
    fn start(login: &Login, compositor: &Path, viewer: &Path) -> Result<Self> {
        let work = login.work;
        let ready = child(work, "headless.log")?;
        let mut comp = Process::start(
            Command::new(compositor)
                .current_dir(work)
                .args([
                    "headless",
                    "--session-dir",
                    "/proc/self/cwd/v",
                    "--width",
                    "800",
                    "--height",
                    "600",
                    "--input-control",
                    "enabled",
                    "--capture-control",
                    "enabled",
                ])
                .stdin(Stdio::piped())
                .stdout(File::create(&ready).map_err(error)?),
        )?;
        let session = wait("host compositor", || {
            comp.alive()?;
            let log = text_file(&ready, 64 * 1024)?;
            Ok(log
                .lines()
                .filter(|line| line.starts_with("TD-COMPOSITOR-HEADLESS-READY "))
                .flat_map(str::split_ascii_whitespace)
                .find_map(|f| f.strip_prefix("session="))
                .map(allocation::string)
                .transpose()
                .map_err(no_storage)?)
        })?;
        let path = child(work, "bin")?;
        let process = Process::start(
            Command::new(viewer)
                .current_dir(work)
                .arg("desktop-oracle")
                .env("PATH", path)
                .env("TD_ORACLE_SSH", &login.ssh)
                .env("TD_ORACLE_CONFIG", child(work, "config")?)
                .env_remove("WAYLAND_SOCKET")
                .env("WAYLAND_DISPLAY", "/proc/self/cwd/v/wayland-0")
                .env("XDG_RUNTIME_DIR", work)
                .stdin(Stdio::null()),
        )?;
        Ok(Self {
            process,
            _compositor: comp,
            control: child(work, "v/td-control")?,
            session,
            serial: 0,
        })
    }

    fn request(&mut self, command: &str) -> Result<Vec<u8>> {
        self.process.alive()?;
        let mut socket = UnixStream::connect(&self.control).map_err(error)?;
        socket.set_read_timeout(Some(WAIT)).map_err(error)?;
        socket.set_write_timeout(Some(WAIT)).map_err(error)?;
        writeln!(socket, "{command}").map_err(error)?;
        let reply = read_bytes(socket, 2_000_000)?;
        if !reply.starts_with(b"ok\n") {
            return Err(error(format_args!("local compositor refused {command}")));
        }
        Ok(reply)
    }
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.serial += 1;
        let direction = if down { "down" } else { "up" };
        self.request(
            &allocation::format(format_args!(
                "key {} {} {code} {direction}",
                self.session, self.serial
            ))
            .map_err(no_storage)?,
        )
        .map(|_| ())
    }
    fn tap(&mut self, code: u16) -> Result<()> {
        self.key(code, true)?;
        self.key(code, false)
    }
    fn text(&mut self, text: &str) -> Result<()> {
        for byte in text.bytes() {
            let code = [
                (16, "qwertyuiop"),
                (30, "asdfghjkl"),
                (44, "zxcvbnm"),
                (53, "/"),
            ]
            .into_iter()
            .find_map(|(first, row)| {
                row.bytes()
                    .position(|key| key == byte)
                    .and_then(|offset| u16::try_from(offset).ok())
                    .map(|offset| first + offset)
            })
            .ok_or("unsupported fixture key")?;
            self.tap(code)?;
        }
        Ok(())
    }
    fn capture(&mut self) -> Result<Vec<u8>> {
        let mut reply = self.request("capture")?;
        let start = reply
            .len()
            .checked_sub(800 * 600 * 3)
            .ok_or("short viewer capture")?;
        reply.drain(..start);
        Ok(reply)
    }
}

// Ignore the host compositor's bars and the guest's window title. A
// blinking caret changes only a few columns, a word changes many.
fn changed_columns(before: &[u8], after: &[u8]) -> usize {
    (8..792)
        .filter(|x| {
            (75..510).any(|y| {
                let i = 3 * (800 * y + x);
                before.get(i..i + 3) != after.get(i..i + 3)
            })
        })
        .count()
}

fn binary(runner: &RecipeCheckRunner, name: &str) -> Result<PathBuf> {
    runner.prepare_recipe_target(name)?;
    let output = runner.build_plan(name)?;
    let bin = child(&runner.ladder_out_from(&output, name)?, "bin")?;
    child(&bin, name)
}

fn lock(guest: &mut Guest, work: &Path) -> Result<()> {
    guest
        .screen_until(
            &child(work, "physical.ppm")?,
            WAIT,
            "enrolled physical lock",
            boot::secret::desktop_locked,
        )
        .map(|_| ())
}

// mkfs maps the host seed owner to guest root. Set the primary home and
// key owners on the disposable overlay before booting the stock selector.
fn provision_home(options: &mut Options, overlay: &Path, deadline: Instant) -> Result<()> {
    use td_engine::cpio::{self, Entry, Kind};
    let initial = child(&options.work, "fixture-initramfs.cpio")?;
    let length = fs::copy(&options.selector, &initial).map_err(error)?;
    let script = b"#!/bin/sh
set -e
/bin/mount -t devtmpfs devtmpfs /dev
/bin/mount -t btrfs -o subvol=@var /dev/vda /volume
/bin/td-util chmod 0700 /volume/home/tester /volume/home/tester/.ssh
/bin/td-util chmod 0600 /volume/home/tester/.ssh/authorized_keys
/bin/td-util chown 1000:1000 /volume/home/tester /volume/home/tester/.ssh /volume/home/tester/.ssh/authorized_keys
/bin/umount /volume
echo TD-DESKTOP-HOME-READY
/bin/reboot -f
";
    let appendix = cpio::build(&[Entry {
        name: "desktop-provision",
        mode: 0o700,
        kind: Kind::File(script),
    }])?;
    boot::append_initramfs(&initial, usize::try_from(length).map_err(error)?, &appendix)?;
    let original = std::mem::replace(&mut options.selector, initial);
    let mut guest = Guest::spawn_network(
        options,
        0,
        deadline,
        overlay,
        "rdinit=/desktop-provision panic=1",
        "user,id=net0,restrict=on",
    )?;
    options.selector = original;
    guest.until(WAIT, |line| {
        Ok((line == "TD-DESKTOP-HOME-READY").then_some(()))
    })?;
    wait("offline provisioning VM shutdown", || {
        guest.child.try_wait().map_err(error)
    })?;
    Ok(())
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<()> {
    let accel = match crate::checks::accel::headless_from_env()?.names {
        ["kvm"] => "kvm",
        ["tcg"] => "tcg",
        _ => return Err("one VM accelerator required".into()),
    };
    let (kernel, template, deployment) = boot::build_system(runner)?;
    let (mkfs, btrfs) = boot::build_btrfs_tools(runner)?;
    let compositor = binary(runner, "td-compositor")?;
    let viewer = binary(runner, "td-desktop")?;
    let owned =
        boot::release_delivery::fixture_temp::TempDir::new("td-desktop-oracle").map_err(error)?;
    let work = owned.path();
    fs::copy(
        child(runner.repo_root(), RECORD)?,
        child(work, "public-vectors")?,
    )
    .map_err(error)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(error)?;
    let port = listener.local_addr().map_err(error)?.port();
    let login = Login::prepare(work, port)?;
    let identity = boot::RunVolume::generate_root_owned(PathBuf::from(
        runner.builder_command().get_program(),
    ))?;
    let selector = boot::provision_selector(&template, work, &identity)?;
    let disk = child(work, "volume.btrfs")?;
    boot::create_persistent_volume_layout(
        &deployment,
        &mkfs,
        &btrfs,
        &disk,
        boot::VolumeLayout::Desktop(child(work, "")?),
        false,
        &identity,
        None,
        boot::VolumePurpose::Fixture,
        None,
        None,
        None,
    )?;
    let overlay = child(work, "disk.qcow2")?;
    super::overlay(&disk, "raw", &overlay)?;
    let mut options = Options {
        kernel,
        selector,
        disk,
        format: allocation::string("raw").map_err(no_storage)?,
        work: child(work, "")?,
        timeout: Duration::from_secs(600),
        rollback: false,
        accel,
    };
    let deadline = Instant::now() + options.timeout;
    provision_home(&mut options, &overlay, deadline)?;
    drop(listener);
    let mut guest = Guest::spawn_network(
        &options,
        1,
        deadline,
        &overlay,
        "",
        &allocation::format(format_args!(
            "user,id=net0,restrict=on,hostfwd=tcp:127.0.0.1:{port}-:22"
        ))
        .map_err(no_storage)?,
    )?;
    guest.until(options.timeout, |line| {
        Ok(line.contains("console login refused").then_some(()))
    })?;
    guest.qmp = Some(boot::Qmp::connect_until(
        &child(work, "qmp-1.sock")?,
        deadline,
    )?);
    lock(&mut guest, work)?;
    let mut view = Viewer::start(&login, &compositor, &viewer)?;
    // The empty ordinary scene has a BGRX ground of 30 25 20; sample
    // the viewer's RGB capture before sending the first shortcut.
    wait(
        "focused viewer with its first ordinary desktop frame",
        || {
            let layout = String::from_utf8(view.request("layout")?).map_err(error)?;
            if !layout
                .lines()
                .any(|line| line.contains("app_id=td-desktop") && line.contains("focused=true"))
            {
                return Ok(None);
            }
            let pixels = view.capture()?;
            let center = 3 * (300 * 800 + 400);
            Ok((pixels.get(center..center + 3) == Some(&[0x20, 0x25, 0x30])).then_some(()))
        },
    )?;
    view.tap(66)?;
    view.tap(10)?;
    wait("empty workspace nine", || {
        Ok(login.read(LAYOUT)?.filter(|s| {
            s.contains("workspace active=9 ")
                && !s
                    .lines()
                    .any(|l| l.starts_with("window ") && l.contains("workspace=9 "))
        }))
    })?;
    let empty = view.capture()?;
    view.tap(66)?;
    view.text("editor")?;
    view.tap(28)?;
    wait("focused editor", || {
        Ok(login.read(LAYOUT)?.filter(|s| {
            s.lines()
                .any(|l| l.contains("app_id=td-editor") && l.contains("focused=true"))
        }))
    })?;
    lock(&mut guest, work)?;
    let editor = wait("editor shown in viewer", || {
        let pixels = view.capture()?;
        Ok((changed_columns(&empty, &pixels) > 200).then_some(pixels))
    })?;
    guest.key(&["x"])?;
    view.text("tdedit")?;
    wait("typed word rendered in viewer", || {
        Ok((changed_columns(&editor, &view.capture()?) >= 12).then_some(()))
    })?;
    view.key(29, true)?;
    view.tap(31)?;
    view.key(29, false)?; // Save As
    view.key(29, true)?;
    view.tap(22)?;
    view.key(29, false)?; // Clear the path field.
    view.text("/var/home/tester/viewer")?;
    view.tap(28)?;
    wait("remote-only text saved by the editor", || {
        Ok(login
            .read("cat \"$HOME/viewer\"")?
            .filter(|s| s == "tdedit"))
    })?;
    lock(&mut guest, work)?;
    guest.key(&["ctrl", "alt", "esc"])?;
    wait("viewer paused for physical secure attention", || {
        Ok(String::from_utf8(view.request("layout")?)
            .map_err(error)?
            .contains("desktop unavailable")
            .then_some(()))
    })?;
    let blank = wait("withheld viewer pixels", || {
        let pixels = view.capture()?;
        let center = 3 * (300 * 800 + 400);
        Ok((pixels.get(center..center + 3) == Some(&[0, 0, 0])).then_some(pixels))
    })?;
    guest.key(&["esc"])?;
    wait("ordinary viewer pixels resumed", || {
        Ok((changed_columns(&blank, &view.capture()?) > 200).then_some(()))
    })?;
    lock(&mut guest, work)?;
    drop(view);
    lock(&mut guest, work)?;
    guest.stop()?;
    println!("TD-REMOTE-DESKTOP-LOCKED-NAVIGATION-PASS");
    Ok(())
}
