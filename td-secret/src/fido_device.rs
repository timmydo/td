//! Root-owned USB hidraw transport. Public discovery hints are never token identity.

use crate::{fido_hid as hid, store};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const NOFOLLOW: i32 = 0o400000;
const DIRECTORY: i32 = 0o200000;
const NONBLOCK: i32 = 0o4000;
const LOCK_REFUSED: u8 = 1;
pub(crate) const MAX_LIFETIME: Duration = Duration::from_secs(120);
const MAX_DESCRIPTOR: usize = 4096;
const WRITE: u8 = 1;
const READ: u8 = 2;

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Device {
    index: u8,
    inode: u64,
    rdev: u64,
}

impl Device {
    pub fn discover() -> Result<Vec<Self>, String> {
        store::require_root()?;
        let mut found = Vec::new();
        // Probe a bounded set of fixed names without relying on a kernel limit.
        // This avoids an
        // unbounded directory walk and accepts no caller-selected path.
        for index in 0..=u8::MAX {
            if let Ok(meta) = fs::symlink_metadata(format!("/dev/hidraw{index}")) {
                if let Ok(device) = Self::inspect(index, &meta) {
                    found.push(device);
                }
            }
        }
        Ok(found)
    }

    fn open(index: u8) -> Result<(Self, File), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(NOFOLLOW)
            .open(format!("/dev/hidraw{index}"))
            .map_err(|_| "open token device")?;
        let meta = file.metadata().map_err(|_| "read token device metadata")?;
        Ok((Self::inspect(index, &meta)?, file))
    }

    fn inspect(index: u8, meta: &fs::Metadata) -> Result<Self, String> {
        if !meta.file_type().is_char_device()
            || meta.uid() != 0
            || meta.gid() != 0
            || meta.mode() & 0o7777 != 0o600
        {
            return Err("token device must be root-owned private character device".into());
        }
        let (event_path, descriptor_path) = metadata_paths(meta.rdev());
        let event = bounded_file(&event_path, 4096)?;
        let event = std::str::from_utf8(&event).map_err(|_| "invalid token bus metadata")?;
        let mut ids = event
            .lines()
            .filter_map(|line| line.strip_prefix("HID_ID="));
        let id = ids.next().ok_or("missing token bus metadata")?;
        if ids.next().is_some() || !id.starts_with("0003:") {
            return Err("token is not a USB HID device".into());
        }
        descriptor(&bounded_file(&descriptor_path, MAX_DESCRIPTOR)?)?;
        Ok(Self {
            index,
            inode: meta.ino(),
            rdev: meta.rdev(),
        })
    }
}

fn bounded_file(path: &str, max: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(max + 1);
    File::open(path)
        .map_err(|error| format!("open kernel token metadata {path}: {error}"))?
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "read kernel token metadata")?;
    if bytes.len() > max {
        return Err("oversized kernel token metadata".into());
    }
    Ok(bytes)
}

fn metadata_paths(rdev: u64) -> (String, String) {
    let (major, minor) = device_numbers(rdev);
    let sysfs = format!("/sys/dev/char/{major}:{minor}/device");
    (
        format!("{sysfs}/uevent"),
        format!("{sysfs}/report_descriptor"),
    )
}

fn device_numbers(value: u64) -> (u64, u64) {
    (
        ((value >> 8) & 0xfff) | ((value >> 32) & 0xfffff000),
        (value & 0xff) | ((value >> 12) & 0xffffff00),
    )
}

/// Admit the unnumbered 64-byte FIDO application collection only.
fn descriptor(mut bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR {
        return Err("invalid HID descriptor size".into());
    }
    let (mut page, mut usage, mut size, mut count, mut min, mut max) =
        (None, None, None, None, None, None);
    let (mut opened, mut closed, mut input, mut output) = (false, false, false, false);
    while let Some(&prefix) = bytes.first() {
        if closed || prefix == 0xfe {
            return Err("unsupported HID descriptor item".into());
        }
        let len = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let raw = bytes.get(1..1 + len).ok_or("truncated HID descriptor")?;
        let mut value = 0u32;
        for (i, byte) in raw.iter().enumerate() {
            value |= u32::from(*byte) << (i * 8);
        }
        bytes = bytes.get(1 + len..).ok_or("truncated HID descriptor")?;
        match (prefix >> 2 & 3, prefix >> 4) {
            (1, 0) => page = Some(value),
            (1, 1) => min = Some(value),
            (1, 2) => max = Some(value),
            (1, 7) => size = Some(value),
            (1, 9) => count = Some(value),
            (2, 0) if usage.is_none() => usage = Some(value),
            (0, 10) if !opened && value == 1 && page == Some(0xf1d0) && usage == Some(1) => {
                opened = true;
                usage = None;
            }
            (0, kind @ (8 | 9))
                if opened
                    && value == 2
                    && page == Some(0xf1d0)
                    && size == Some(8)
                    && count == Some(64)
                    && min == Some(0)
                    && max == Some(255) =>
            {
                if kind == 8 && usage == Some(0x20) && !input {
                    input = true;
                } else if kind == 9 && usage == Some(0x21) && !output {
                    output = true;
                } else {
                    return Err("invalid FIDO report usage".into());
                }
                usage = None;
            }
            (0, 12) if opened && input && output && len == 0 && usage.is_none() => closed = true,
            _ => return Err("unsupported FIDO HID report profile".into()),
        }
    }
    if !closed {
        return Err("incomplete FIDO HID collection".into());
    }
    Ok(())
}

struct Worker {
    child: Child,
    socket: UnixStream,
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Session {
    worker: Option<Worker>,
    deadline: Instant,
    channel: u32,
}

impl Session {
    pub fn open(device: Device, deadline: Instant) -> Result<Self, String> {
        store::require_root()?;
        if deadline.saturating_duration_since(Instant::now()) > MAX_LIFETIME {
            return Err("token operation exceeds lifetime limit".into());
        }
        remaining(deadline)?;
        let (socket, child_socket) =
            UnixStream::pair().map_err(|_| "create token worker channel")?;
        let child_output: OwnedFd = child_socket
            .try_clone()
            .map_err(|_| "clone token worker channel")?
            .into();
        let child_input: OwnedFd = child_socket.into();
        let child = Command::new("/proc/self/exe")
            .args([
                "hid-worker",
                &device.index.to_string(),
                &device.inode.to_string(),
                &device.rdev.to_string(),
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(child_input))
            .stdout(Stdio::from(child_output))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "start token worker")?;
        let mut session = Self {
            worker: Some(Worker { child, socket }),
            deadline,
            channel: hid::BROADCAST,
        };
        let result = session.initialize();
        if result.is_err() {
            session.worker.take();
        }
        result.map(|()| session)
    }

    fn socket(&mut self) -> Result<&mut UnixStream, String> {
        self.worker
            .as_mut()
            .map(|worker| &mut worker.socket)
            .ok_or_else(|| "token session is closed".into())
    }

    fn initialize(&mut self) -> Result<(), String> {
        let deadline = self.deadline;
        let mut ready = [0];
        receive(self.socket()?, &mut ready, deadline)?;
        if ready == [LOCK_REFUSED] {
            return Err("token transport is busy or unavailable".into());
        }
        if ready != [0] {
            return Err("token worker refused initialization".into());
        }
        let mut nonce = [0; 8];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|_| "read token channel nonce")?;
        let mut init = hid::Initialization::new(hid::BROADCAST, nonce)?;
        nonce.fill(0);
        for report in init.request()?.as_ref() {
            self.write(report)?;
        }
        loop {
            let mut report = self.read()?;
            let result = init.push(&report);
            report.fill(0);
            if let Some(channel) = result? {
                self.channel = channel;
                return Ok(());
            }
        }
    }

    /// Errors close the session; an uncertain operation is never replayed.
    pub fn cbor(&mut self, bytes: &[u8]) -> Result<hid::Message, String> {
        let result = self.exchange(bytes);
        if result.is_err() {
            self.worker.take();
        }
        result
    }
    fn exchange(&mut self, bytes: &[u8]) -> Result<hid::Message, String> {
        for report in hid::cbor(self.channel, bytes)?.as_ref() {
            self.write(report)?;
        }
        let mut decoder = hid::Decoder::cbor(self.channel)?;
        loop {
            let mut report = self.read()?;
            let result = decoder.push(&report);
            report.fill(0);
            if let hid::Event::Complete(message) = result? {
                return Ok(message);
            }
        }
    }
    fn write(&mut self, report: &[u8; 64]) -> Result<(), String> {
        let deadline = self.deadline;
        let mut frame = Report([WRITE; 65]);
        frame
            .0
            .get_mut(1..)
            .ok_or("invalid worker frame storage")?
            .copy_from_slice(report);
        send(self.socket()?, &frame.0, deadline)?;
        let mut ack = [0];
        receive(self.socket()?, &mut ack, deadline)?;
        if ack != [0] {
            return Err("token write refused".into());
        }
        Ok(())
    }
    fn read(&mut self) -> Result<[u8; 64], String> {
        let deadline = self.deadline;
        send(self.socket()?, &[READ], deadline)?;
        let mut report = [0; 64];
        if let Err(error) = receive(self.socket()?, &mut report, deadline) {
            report.fill(0);
            return Err(error);
        }
        Ok(report)
    }
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    let time = deadline.saturating_duration_since(Instant::now());
    if time.is_zero() {
        Err("token operation expired".into())
    } else {
        Ok(time)
    }
}
fn send(socket: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> Result<(), String> {
    while !bytes.is_empty() {
        socket
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|_| "set token write deadline")?;
        match socket.write(bytes) {
            Ok(0) => return Err("token worker disconnected".into()),
            Ok(n) => bytes = bytes.get(n..).ok_or("invalid token write size")?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("token worker write failed or expired".into()),
        }
    }
    remaining(deadline).map(|_| ())
}
fn receive(socket: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> Result<(), String> {
    while !bytes.is_empty() {
        socket
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|_| "set token read deadline")?;
        match socket.read(bytes) {
            Ok(0) => return Err("token worker disconnected".into()),
            Ok(n) => bytes = bytes.get_mut(n..).ok_or("invalid token read size")?,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err("token worker read failed or expired".into()),
        }
    }
    remaining(deadline).map(|_| ())
}

struct Report([u8; 65]);
impl Drop for Report {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

fn operation_lock() -> Result<File, String> {
    let run = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open("/run")
        .map_err(|e| format!("open token runtime: {e}"))?;
    operation_lock_in(&run, 0, 0)
}

fn checked_directory(file: &File, uid: u32, gid: u32, mode: u32, name: &str) -> Result<(), String> {
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != uid || meta.gid() != gid || meta.mode() & 0o7777 != mode {
        return Err(format!("{name} has invalid ownership, mode or type"));
    }
    Ok(())
}

// The stable lock is never renamed or removed while /run exists.
fn operation_lock_in(run: &File, uid: u32, gid: u32) -> Result<File, String> {
    checked_directory(run, uid, gid, 0o755, "token /run")?;
    let path = format!("/proc/self/fd/{}/td-fido", run.as_raw_fd());
    let created = match fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(format!("create token runtime: {e}")),
    };
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)
        .map_err(|e| format!("open private token runtime: {e}"))?;
    if created {
        std::os::unix::fs::fchown(&directory, Some(uid), Some(gid)).map_err(|e| e.to_string())?;
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    checked_directory(&directory, uid, gid, 0o700, "token /run/td-fido")?;
    let path = format!("/proc/self/fd/{}/operation.lock", directory.as_raw_fd());
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(NOFOLLOW | NONBLOCK);
    let file = match options.create_new(true).mode(0o600).open(&path) {
        Ok(file) => {
            std::os::unix::fs::fchown(&file, Some(uid), Some(gid)).map_err(|e| e.to_string())?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
            file
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(&path)
            .map_err(|e| format!("open token operation lock: {e}"))?,
        Err(e) => return Err(format!("create token operation lock: {e}")),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.len() != 0
        || meta.uid() != uid
        || meta.gid() != gid
        || meta.mode() & 0o7777 != 0o600
    {
        return Err("token operation lock has invalid ownership, mode, type or contents".into());
    }
    file.try_lock()
        .map_err(|e| format!("token transport is busy or unavailable: {e}"))?;
    Ok(file)
}

// A separate thread can request process exit while the device thread is blocked.
// This function belongs only to the dedicated worker process, never the authority.
fn arm_watchdog(lifetime: Duration) -> Result<(), String> {
    std::thread::Builder::new()
        .name("hid-deadline".into())
        .spawn(move || {
            std::thread::sleep(lifetime);
            std::process::exit(1);
        })
        .map_err(|_| "start token worker watchdog")?;
    Ok(())
}

/// Root-only worker entry; the normal caller supplies private inherited stdio.
pub fn worker(index: &str, inode: &str, rdev: &str) -> Result<(), String> {
    store::require_root()?;
    let index: u8 = canonical(index)?;
    let expected_inode: u64 = canonical(inode)?;
    let expected_rdev: u64 = canonical(rdev)?;
    let deadline = Instant::now() + MAX_LIFETIME;
    arm_watchdog(MAX_LIFETIME)?;
    let _operation = match operation_lock() {
        Ok(lock) => lock,
        Err(error) => {
            io::stdout()
                .write_all(&[LOCK_REFUSED])
                .and_then(|()| io::stdout().flush())
                .map_err(|_| "report token lock refusal")?;
            return Err(error);
        }
    };
    let (device, mut file) = Device::open(index)?;
    if device.inode != expected_inode || device.rdev != expected_rdev {
        return Err("token device changed before opening".into());
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    output
        .write_all(&[0])
        .and_then(|()| output.flush())
        .map_err(|_| "token worker channel closed")?;
    loop {
        let mut op = [0];
        match input.read_exact(&mut op) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(_) => return Err("read token worker operation".into()),
            Ok(()) => {}
        }
        remaining(deadline)?;
        let mut report = Report([0; 65]);
        match op {
            [WRITE] => {
                input
                    .read_exact(report.0.get_mut(1..).ok_or("invalid report storage")?)
                    .map_err(|_| "short token write request")?;
                remaining(deadline)?;
                // USB output is synchronous even with O_NONBLOCK. Both the
                // parent and independent watchdog can retire this worker.
                let size = file
                    .write(&report.0)
                    .map_err(|_| "token device write failed")?;
                if size != report.0.len() {
                    return Err("short token device write".into());
                }
                remaining(deadline)?;
                output
                    .write_all(&[0])
                    .map_err(|_| "token worker channel closed")?;
            }
            [READ] => {
                // The parent's socket deadline and this process's watchdog
                // bound blocking input without polling for a human touch.
                match file.read(&mut report.0) {
                    Ok(64) => {}
                    Ok(_) => return Err("token input report is not 64 bytes".into()),
                    Err(_) => return Err("token device read failed".into()),
                }
                remaining(deadline)?;
                output
                    .write_all(report.0.get(..64).ok_or("invalid report storage")?)
                    .map_err(|_| "token worker channel closed")?;
            }
            _ => return Err("invalid token worker operation".into()),
        }
        output.flush().map_err(|_| "token worker channel closed")?;
    }
}

fn canonical<T: std::str::FromStr + ToString>(text: &str) -> Result<T, String> {
    let value: T = text.parse().map_err(|_| "invalid token device number")?;
    if value.to_string() != text {
        return Err("noncanonical token device number".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SYNC: &[u8] = b"TD-HID-TEST-READY\n";

    fn descriptor_bytes() -> Vec<u8> {
        // CTAP 2.3 section 11.2.8's application collection, with 64-byte reports.
        vec![
            0x06, 0xd0, 0xf1, 0x09, 1, 0xa1, 1, 0x09, 0x20, 0x15, 0, 0x26, 0xff, 0, 0x75, 8, 0x95,
            64, 0x81, 2, 0x09, 0x21, 0x95, 64, 0x91, 2, 0xc0,
        ]
    }

    // The target recipe expands its placeholders in embedded Rust source.
    // This literal oracle also runs after that expansion in the target build.
    #[test]
    fn device_metadata_paths_survive_recipe_source_staging() {
        assert_eq!(
            metadata_paths(0x103),
            (
                "/sys/dev/char/1:3/device/uevent".into(),
                "/sys/dev/char/1:3/device/report_descriptor".into(),
            )
        );
    }

    #[test]
    fn only_the_unnumbered_fido_report_profile_is_admitted() {
        let bytes = descriptor_bytes();
        descriptor(&bytes).unwrap();
        for len in 0..bytes.len() {
            assert!(descriptor(&bytes[..len]).is_err(), "{len}");
        }
        for offset in [1, 2, 4, 6, 8, 10, 12, 13, 15, 17, 19, 21, 23, 25, 26] {
            let mut bad = bytes.clone();
            bad[offset] ^= 1;
            assert!(descriptor(&bad).is_err(), "offset {offset}");
        }
        for extra in [&[0x85, 1][..], &[0xa4], &[0xa1, 1], &[0xfe, 0, 0]] {
            let mut bad = bytes.clone();
            bad.splice(7..7, extra.iter().copied());
            assert!(descriptor(&bad).is_err());
        }
        let mut tail = bytes.clone();
        tail.push(0);
        assert!(descriptor(&tail).is_err());
        assert!(descriptor(&vec![0; MAX_DESCRIPTOR + 1]).is_err());
        for text in ["-1", "256", "01", "+1", " 1", "1/"] {
            assert!(canonical::<u8>(text).is_err());
        }
        assert_eq!(canonical::<u8>("255").unwrap(), 255);
        assert_eq!(device_numbers(0x103), (1, 3));
        assert_eq!(device_numbers(0x123456789abcdef0), (0x12345cde, 0x6789abf0));
    }

    // Re-exec the test binary as a worker with inherited socket stdio. This
    // tests actual child ownership and stream deadlines without opening hardware.
    #[test]
    fn worker_fixture() {
        let Ok(role) = std::env::var("TD_HID_WORKER_FIXTURE") else {
            return;
        };
        if role == "orphan-parent" {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fido_device::tests::worker_fixture",
                    "--nocapture",
                    "--quiet",
                ])
                .env_clear()
                .env("TD_HID_WORKER_FIXTURE", "lock")
                .env(
                    "TD_HID_LOCK_ROOT",
                    std::env::var_os("TD_HID_LOCK_ROOT").unwrap(),
                )
                .stdin(Stdio::inherit())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut output = child.stdout.take().unwrap();
            let mut preamble = Vec::new();
            while !preamble.ends_with(SYNC) {
                let mut byte = [0];
                output.read_exact(&mut byte).unwrap();
                preamble.extend(byte);
                assert!(preamble.len() < 256);
            }
            io::stdout().write_all(SYNC).unwrap();
            io::stdout().flush().unwrap();
            // The grandchild holds the lock and inherited input after this exit.
            std::process::exit(0);
        }
        let _operation = if role == "lock" {
            let run = File::open(std::env::var("TD_HID_LOCK_ROOT").unwrap()).unwrap();
            let meta = run.metadata().unwrap();
            Some(operation_lock_in(&run, meta.uid(), meta.gid()).unwrap())
        } else {
            None
        };
        if role == "watchdog" {
            arm_watchdog(Duration::from_millis(80)).unwrap();
        }
        let mut input = io::stdin().lock();
        let mut output = io::stdout().lock();
        output.write_all(SYNC).unwrap();
        output.flush().unwrap();
        if role == "lock-refused" {
            output.write_all(&[LOCK_REFUSED]).unwrap();
            output.flush().unwrap();
            std::process::exit(0);
        }
        let mut op = [0];
        while input.read_exact(&mut op).is_ok() {
            match op {
                [WRITE] => {
                    let mut request = [0; 64];
                    input.read_exact(&mut request).unwrap();
                    assert_eq!(&request[..8], &[0, 0, 0, 1, 0x90, 0, 1, 4]);
                    output.write_all(&[u8::from(role == "bad-ack")]).unwrap();
                }
                [READ] if role == "stall" => {
                    std::thread::sleep(Duration::from_secs(30));
                }
                [READ] if role == "short" => {
                    output.write_all(&[0]).unwrap();
                    output.flush().unwrap();
                    std::process::exit(0);
                }
                [READ] => {
                    let mut report = [0; 64];
                    if role == "keepalive" {
                        report[..8].copy_from_slice(&[0, 0, 0, 1, 0xbb, 0, 1, 2]);
                    } else {
                        report[..9].copy_from_slice(&[0, 0, 0, 1, 0x90, 0, 2, 0, 0xa0]);
                    }
                    output.write_all(&report).unwrap();
                }
                _ => panic!("unexpected fixture operation"),
            }
            output.flush().unwrap();
        }
    }

    fn fixture(role: &str, time: Duration) -> Session {
        let (socket, peer) = UnixStream::pair().unwrap();
        let input: OwnedFd = peer.try_clone().unwrap().into();
        let output: OwnedFd = peer.into();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fido_device::tests::worker_fixture",
                "--nocapture",
                "--quiet",
            ])
            .env("TD_HID_WORKER_FIXTURE", role)
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Skip a bounded test-runner preamble until our own explicit marker.
        // No dependency on the runner's human-readable banner.
        let mut socket = socket;
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(SYNC) {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            header.extend(byte);
            assert!(header.len() < 256);
        }
        Session {
            worker: Some(Worker { child, socket }),
            channel: 1,
            deadline: Instant::now() + time,
        }
    }

    #[test]
    fn framed_exchange_owns_its_worker_and_closes_on_every_failure() {
        let mut good = fixture("reply", Duration::from_secs(5));
        assert_eq!(good.cbor(&[4]).unwrap().as_ref(), &[0, 0xa0]);
        // A second request is valid before the common operation deadline.
        assert_eq!(good.cbor(&[4]).unwrap().as_ref(), &[0, 0xa0]);
        for role in ["short", "bad-ack", "stall", "keepalive"] {
            let mut session = fixture(role, Duration::from_millis(80));
            let pid = session.worker.as_ref().unwrap().child.id();
            assert!(session.cbor(&[4]).is_err(), "{role}");
            assert!(session.worker.is_none());
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "worker not reaped: {role}"
            );
            assert!(session.cbor(&[4]).is_err());
        }
    }

    struct LockFixture(std::path::PathBuf);
    impl LockFixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("td-hid-lock-{}-{nonce}", std::process::id()));
            fs::DirBuilder::new().mode(0o755).create(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            Self(path)
        }
        fn run(&self) -> File {
            File::open(&self.0).unwrap()
        }
        fn acquire_after_release(&self) -> File {
            // A concurrent test fork may briefly inherit a CLOEXEC descriptor
            // until its exec. Observe final kernel release within a fixed bound.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.acquire() {
                    Ok(file) => return file,
                    Err(error) => {
                        assert!(Instant::now() < deadline, "lock did not release: {error}");
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
        fn acquire(&self) -> Result<File, String> {
            let run = self.run();
            let meta = run.metadata().unwrap();
            operation_lock_in(&run, meta.uid(), meta.gid())
        }
    }
    impl Drop for LockFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn operation_lock_is_exclusive_stable_and_rejects_mutated_metadata() {
        let fixture = LockFixture::new();
        let first = fixture.acquire().unwrap();
        let meta = first.metadata().unwrap();
        assert!(fixture.acquire().is_err());
        drop(first);
        let second = fixture.acquire_after_release();
        assert_eq!(
            (
                second.metadata().unwrap().dev(),
                second.metadata().unwrap().ino()
            ),
            (meta.dev(), meta.ino())
        );
        drop(second);
        let lock = fixture.0.join("td-fido/operation.lock");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(fixture.acquire().is_err());
        assert_eq!(fs::metadata(&lock).unwrap().mode() & 0o777, 0o644);
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&lock, [1]).unwrap();
        assert!(fixture.acquire().is_err());
        fs::write(&lock, []).unwrap();
        let alias = fixture.0.join("alias");
        fs::hard_link(&lock, &alias).unwrap();
        assert!(fixture.acquire().is_err());
        fs::remove_file(alias).unwrap();
        let saved = fixture.0.join("saved");
        fs::rename(&lock, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &lock).unwrap();
        assert!(fixture.acquire().is_err());
        fs::remove_file(&lock).unwrap();
        fs::rename(saved, &lock).unwrap();
        drop(fixture.acquire_after_release());
        fs::set_permissions(fixture.0.join("td-fido"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(fixture.acquire().is_err());
        fs::set_permissions(fixture.0.join("td-fido"), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = fixture.0.join("td-fido");
        let saved = fixture.0.join("saved-directory");
        fs::rename(&directory, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &directory).unwrap();
        assert!(fixture.acquire().is_err());
        fs::remove_file(&directory).unwrap();
        fs::rename(saved, &directory).unwrap();
        let run = fixture.run();
        let meta = run.metadata().unwrap();
        assert!(operation_lock_in(&run, meta.uid().wrapping_add(1), meta.gid()).is_err());
        assert!(operation_lock_in(&run, meta.uid(), meta.gid().wrapping_add(1)).is_err());
    }

    #[test]
    fn production_worker_retains_its_lock_before_device_access() {
        let source = include_str!("fido_device.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let worker = source
            .split("pub fn worker(")
            .nth(1)
            .unwrap()
            .split("fn canonical<")
            .next()
            .unwrap();
        let lock = worker
            .find("let _operation = match operation_lock()")
            .unwrap();
        let open = worker.find("Device::open(index)?").unwrap();
        assert!(lock < open);
        assert_eq!(worker.matches("_operation").count(), 1);
        assert_eq!(source.matches("Device::open(").count(), 1);
        let mut refused = fixture("lock-refused", Duration::from_secs(5));
        assert_eq!(
            refused.initialize().unwrap_err(),
            "token transport is busy or unavailable"
        );
    }

    #[test]
    fn worker_process_owns_the_lock_until_its_kernel_exit() {
        let fixture = LockFixture::new();
        let (mut socket, peer) = UnixStream::pair().unwrap();
        let input: OwnedFd = peer.try_clone().unwrap().into();
        let output: OwnedFd = peer.into();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fido_device::tests::worker_fixture",
                "--nocapture",
                "--quiet",
            ])
            .env_clear()
            .env("TD_HID_WORKER_FIXTURE", "lock")
            .env("TD_HID_LOCK_ROOT", &fixture.0)
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = Worker {
            child,
            socket: socket.try_clone().unwrap(),
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut preamble = Vec::new();
        while !preamble.ends_with(SYNC) {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            preamble.extend(byte);
            assert!(preamble.len() < 256);
        }
        assert!(fixture.acquire().is_err());
        // No parent owns the lock: it is held by the child's independent open.
        drop(socket);
        assert!(fixture.acquire().is_err());
        worker.child.kill().unwrap();
        worker.child.wait().unwrap();
        drop(worker);
        drop(fixture.acquire_after_release());
    }

    #[test]
    fn parent_exit_cannot_release_a_live_workers_lock() {
        let fixture = LockFixture::new();
        let (mut socket, peer) = UnixStream::pair().unwrap();
        let input: OwnedFd = peer.try_clone().unwrap().into();
        let output: OwnedFd = peer.into();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fido_device::tests::worker_fixture",
                "--nocapture",
                "--quiet",
            ])
            .env_clear()
            .env("TD_HID_WORKER_FIXTURE", "orphan-parent")
            .env("TD_HID_LOCK_ROOT", &fixture.0)
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = Worker {
            child,
            socket: socket.try_clone().unwrap(),
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut preamble = Vec::new();
        while !preamble.ends_with(SYNC) {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            preamble.extend(byte);
            assert!(preamble.len() < 256);
        }
        assert!(fixture.acquire().is_err());
        // No parent owns the lock: it is held by the child's independent open.
        drop(socket);
        assert!(fixture.acquire().is_err());
        assert!(worker.child.wait().unwrap().success());
        assert!(
            fixture.acquire().is_err(),
            "parent exit released the worker lock"
        );
        worker.socket.shutdown(std::net::Shutdown::Both).unwrap();
        drop(fixture.acquire_after_release());
        drop(worker);
    }

    #[test]
    fn independent_watchdog_exits_while_the_io_thread_is_blocked() {
        let mut session = fixture("watchdog", Duration::from_secs(5));
        let worker = session.worker.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = worker.child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(Instant::now() < deadline, "worker watchdog did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn partial_stream_traffic_never_renews_the_absolute_deadline() {
        let (mut socket, mut peer) = UnixStream::pair().unwrap();
        let writer = std::thread::spawn(move || {
            for _ in 0..5 {
                if peer.write_all(&[7]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        let mut bytes = [0; 5];
        assert!(receive(
            &mut socket,
            &mut bytes,
            Instant::now() + Duration::from_millis(60)
        )
        .is_err());
        drop(socket);
        writer.join().unwrap();
        let (mut socket, _peer) = UnixStream::pair().unwrap();
        assert!(send(&mut socket, &[7], Instant::now()).is_err());
    }
}

#[cfg(test)]
mod vm_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread::{self, JoinHandle};

    const CREATE2_DESCRIPTOR: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4;
    const UHID_EVENT_SIZE: usize = CREATE2_DESCRIPTOR + 4096;
    const CHANNEL: u32 = 0x10203040;
    const DESCRIPTOR: &[u8] = &[
        0x06, 0xd0, 0xf1, 0x09, 1, 0xa1, 1, 0x09, 0x20, 0x15, 0, 0x26, 0xff, 0, 0x75, 8, 0x95, 64,
        0x81, 2, 0x09, 0x21, 0x95, 64, 0x91, 2, 0xc0,
    ];

    fn guard(case: &str) {
        assert!(fs::read_to_string("/proc/cmdline")
            .unwrap()
            .split_ascii_whitespace()
            .any(|arg| arg == "td.hid-fixture=1"));
        assert_eq!(fs::read_to_string("/case").unwrap(), case);
        store::require_root().unwrap();
    }

    fn write_event(file: &mut File, event: &[u8]) {
        assert_eq!(file.write(event).unwrap(), event.len());
    }

    fn input(file: &mut File, report: &[u8; 64]) {
        let mut event = [0; 70];
        event[..4].copy_from_slice(&12_u32.to_ne_bytes());
        event[4..6].copy_from_slice(&64_u16.to_ne_bytes());
        event[6..].copy_from_slice(report);
        write_event(file, &event);
    }

    struct Token {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<(usize, usize)>>,
    }

    impl Token {
        fn start(expected: Vec<Vec<u8>>, response: Vec<u8>, keepalive: bool) -> Self {
            Self::serve(Duration::from_secs(15), keepalive, move |request, index| {
                assert_eq!(request, expected.get(index).unwrap());
                response.clone()
            })
        }

        fn serve(
            lifetime: Duration,
            keepalive: bool,
            mut reply: impl FnMut(&[u8], usize) -> Vec<u8> + Send + 'static,
        ) -> Self {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(NOFOLLOW | NONBLOCK)
                .open("/dev/uhid")
                .unwrap();
            let meta = file.metadata().unwrap();
            assert!(meta.file_type().is_char_device());
            assert_eq!(
                (meta.uid(), meta.gid(), meta.mode() & 0o7777),
                (0, 0, 0o600)
            );
            let mut event = [0; UHID_EVENT_SIZE];
            event[..4].copy_from_slice(&11_u32.to_ne_bytes());
            event[4..20].copy_from_slice(b"td FIDO fixture\0");
            event[260..262].copy_from_slice(&(DESCRIPTOR.len() as u16).to_ne_bytes());
            event[262..264].copy_from_slice(&3_u16.to_ne_bytes());
            event[264..268].copy_from_slice(&0x1209_u32.to_ne_bytes());
            event[268..272].copy_from_slice(&1_u32.to_ne_bytes());
            event[CREATE2_DESCRIPTOR..CREATE2_DESCRIPTOR + DESCRIPTOR.len()]
                .copy_from_slice(DESCRIPTOR);
            write_event(&mut file, &event);
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                let deadline = Instant::now() + lifetime;
                let mut decoder = hid::Decoder::cbor(CHANNEL).unwrap();
                let mut waiting = false;
                let mut next_keepalive = Instant::now();
                let mut requests = 0;
                let mut keepalives = 0;
                while !stopped.load(Ordering::Relaxed) {
                    assert!(Instant::now() < deadline, "virtual HID fixture expired");
                    event.fill(0);
                    match file.read(&mut event) {
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("read UHID: {error}"),
                        Ok(size) => {
                            assert!(size >= 4);
                            let kind = u32::from_ne_bytes(event[..4].try_into().unwrap());
                            if kind == 6 {
                                let size = u16::from_ne_bytes(event[4100..4102].try_into().unwrap())
                                    as usize;
                                assert_eq!(event[4102], 1);
                                // hidraw's unnumbered output carries its leading report ID.
                                assert_eq!(size, 65);
                                assert_eq!(event[4], 0);
                                let report: [u8; 64] = event[5..69].try_into().unwrap();
                                if report[..7] == [255, 255, 255, 255, 0x86, 0, 8] {
                                    let mut reply = [0; 64];
                                    reply[..7].copy_from_slice(&[255, 255, 255, 255, 0x86, 0, 17]);
                                    reply[7..15].copy_from_slice(&report[7..15]);
                                    reply[15..19].copy_from_slice(&CHANNEL.to_be_bytes());
                                    reply[19..24].copy_from_slice(&[2, 1, 0, 0, 4]);
                                    input(&mut file, &reply);
                                } else if let hid::Event::Complete(request) =
                                    decoder.push(&report).unwrap()
                                {
                                    let response = reply(request.as_ref(), requests);
                                    requests += 1;
                                    if keepalive {
                                        waiting = true;
                                    } else {
                                        for report in
                                            hid::cbor(CHANNEL, &response).unwrap().as_ref()
                                        {
                                            input(&mut file, report);
                                        }
                                    }
                                    decoder = hid::Decoder::cbor(CHANNEL).unwrap();
                                }
                            } else {
                                assert!(
                                    [2, 3, 4, 5].contains(&kind),
                                    "unexpected UHID event {kind}"
                                );
                            }
                        }
                    }
                    if waiting && Instant::now() >= next_keepalive {
                        let mut report = [0; 64];
                        report[..4].copy_from_slice(&CHANNEL.to_be_bytes());
                        report[4..8].copy_from_slice(&[0xbb, 0, 1, 2]);
                        input(&mut file, &report);
                        keepalives += 1;
                        next_keepalive = Instant::now() + Duration::from_millis(20);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                (requests, keepalives)
            });
            Self {
                stop,
                worker: Some(worker),
            }
        }

        fn finish(mut self) -> (usize, usize) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Token {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn discover_one() -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let devices = Device::discover().unwrap();
            if devices.len() == 1 {
                return devices[0];
            }
            assert!(
                Instant::now() < deadline,
                "virtual token was not discovered"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn session(device: Device, lifetime: Duration) -> Session {
        let (socket, peer) = UnixStream::pair().unwrap();
        let input: OwnedFd = peer.try_clone().unwrap().into();
        let output: OwnedFd = peer.into();
        let child = Command::new("/bin/td-secret")
            .args([
                "hid-worker",
                &device.index.to_string(),
                &device.inode.to_string(),
                &device.rdev.to_string(),
            ])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(output))
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut session = Session {
            worker: Some(Worker { child, socket }),
            deadline: Instant::now() + lifetime,
            channel: hid::BROADCAST,
        };
        session.initialize().unwrap();
        session
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn assertion_fixture() -> (Vec<u8>, crate::fido_ctap::Es256PublicKey, [u8; 32]) {
        // The independent OpenSSL fixture also used by the host TPM oracle.
        let mut cose = vec![0xa5, 1, 2, 3, 0x26, 0x20, 1, 0x21, 0x58, 0x20];
        cose.extend(hex(
            "ab8ace3ba858575dd060bf6e790f73982165b36abbfffb86cf0f5e032fafbb5a",
        ));
        cose.extend([0x22, 0x58, 0x20]);
        cose.extend(hex(
            "552ef0c808cfa668e3012f4411fc0a3ad01a39d3a0fb158534721a8016b31553",
        ));
        let key = crate::fido_ctap::Es256PublicKey::from_cose(&cose).unwrap();
        let challenge = hex("f3ad24f2731ea324507944e3ae1b9a172f14eaac6a57e004788390dc14a4c7ca")
            .try_into()
            .unwrap();
        let auth = hex(
            "34e2ef54cd9003d2930734cfb0402ccab6a44dcb5024fc367878c413c78ce2dd8100000007a16b6372656450726f7465637401",
        );
        let signature = hex(
            "3045022100fcd359f2e59ed2e63367ec882724beae6d78fd876d9208b9ec0900b4114aa98c02205c58e5c6d917e85e879fed77b43f0b73cf4394eb1caadb657855e362e541b4f7",
        );
        let mut response = crate::fido_cbor::Encoder::new();
        response.head(5, 2).unwrap();
        response.head(0, 2).unwrap();
        response.bytes(&auth).unwrap();
        response.head(0, 3).unwrap();
        response.bytes(&signature).unwrap();
        let mut bytes = vec![0];
        bytes.extend(response.finish().unwrap());
        (bytes, key, challenge)
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID and TPM devices"]
    fn qemu_hid_assertion_uses_production_worker_and_guest_tpm() {
        guard("fido-hid");
        assert!(Device::discover().unwrap().is_empty());
        let (response, key, challenge) = assertion_fixture();
        let request = crate::fido_ctap::AssertionRequest::new(&[7], challenge, 1024).unwrap();
        let mut changed = challenge;
        changed[0] ^= 1;
        let changed = crate::fido_ctap::AssertionRequest::new(&[7], changed, 1024).unwrap();
        let token = Token::start(
            vec![request.bytes().to_vec(), changed.bytes().to_vec()],
            response.clone(),
            false,
        );
        let mut session = session(discover_one(), Duration::from_secs(10));
        let worker = session.worker.as_ref().unwrap().child.id();
        let mut tpm = crate::tpm::Client::new(crate::tpm::Device::open().unwrap());
        let received = session.cbor(request.bytes()).unwrap();
        assert_eq!(received.as_ref(), response);
        assert_eq!(
            request
                .verify(received.as_ref(), &key, &mut tpm)
                .unwrap()
                .counter,
            7
        );
        let replay = session.cbor(changed.bytes()).unwrap();
        assert_eq!(replay.as_ref(), response);
        let error = changed
            .verify(replay.as_ref(), &key, &mut tpm)
            .err()
            .unwrap();
        assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        drop(session);
        assert!(!std::path::Path::new(&format!("/proc/{worker}")).exists());
        assert_eq!(token.finish(), (2, 0));
        assert!(Device::discover().unwrap().is_empty());
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_keepalives_cannot_extend_the_worker_deadline() {
        guard("fido-deadline");
        assert!(Device::discover().unwrap().is_empty());
        let (response, _, challenge) = assertion_fixture();
        let request = crate::fido_ctap::AssertionRequest::new(&[7], challenge, 1024).unwrap();
        let token = Token::start(vec![request.bytes().to_vec()], response, true);
        let mut session = session(discover_one(), Duration::from_secs(5));
        let worker = session.worker.as_ref().unwrap().child.id();
        let started = Instant::now();
        let error = session.cbor(request.bytes()).err().unwrap();
        assert!(error.contains("expired"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(7));
        assert!(session.worker.is_none());
        assert!(!std::path::Path::new(&format!("/proc/{worker}")).exists());
        let (requests, keepalives) = token.finish();
        assert_eq!(requests, 1);
        assert!(keepalives >= 5);
        assert!(Device::discover().unwrap().is_empty());
    }
    fn fresh() -> [u8; 32] {
        let mut bytes = [0; 32];
        File::open("/dev/urandom")
            .unwrap()
            .read_exact(&mut bytes)
            .unwrap();
        bytes
    }

    fn info_reply() -> Vec<u8> {
        let mut bytes = b"\0\xa2\x01\x81\x68FIDO_2_0\x03\x50".to_vec();
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    struct VirtualCredential {
        id: Vec<u8>,
        signer: Arc<std::sync::Mutex<crate::tpm::tests::SigningKey>>,
    }

    impl VirtualCredential {
        fn new(id: u8) -> Self {
            Self {
                id: vec![id; 32],
                signer: Arc::new(std::sync::Mutex::new(crate::tpm::tests::SigningKey::new())),
            }
        }

        fn start(&self, expected: Vec<Vec<u8>>) -> Token {
            self.checked(move |request, index| {
                assert_eq!(request, expected.get(index).unwrap());
            })
        }

        fn checked(&self, mut check: impl FnMut(&[u8], usize) + Send + 'static) -> Token {
            use crate::fido_cbor::{self as cbor, Encoder, Value};
            let signer = Arc::clone(&self.signer);
            let id = self.id.clone();
            let mut counter = 0u32;
            Token::serve(Duration::from_secs(60), false, move |request, index| {
                check(request, index);
                if request == [4] {
                    return info_reply();
                }
                let value = cbor::decode(&request[1..]).unwrap();
                let mut auth = crate::crypto::digest(crate::fido_ctap::RP_ID.as_bytes()).to_vec();
                let mut out = Encoder::new();
                match request[0] {
                    1 => {
                        assert_eq!(
                            value
                                .required(&Value::Unsigned(2))
                                .unwrap()
                                .required(&Value::Text("id"))
                                .unwrap()
                                .text()
                                .unwrap(),
                            crate::fido_ctap::RP_ID
                        );
                        if let Some(Value::Array(excluded)) =
                            value.get(&Value::Unsigned(5)).unwrap()
                        {
                            if excluded.iter().any(|credential| {
                                credential
                                    .required(&Value::Text("id"))
                                    .unwrap()
                                    .bytes()
                                    .unwrap()
                                    == id
                            }) {
                                return vec![0x19]; // CTAP2_ERR_CREDENTIAL_EXCLUDED
                            }
                        }
                        auth.push(0x41);
                        auth.extend_from_slice(&0u32.to_be_bytes());
                        auth.extend_from_slice(&[0; 16]);
                        auth.extend_from_slice(&(id.len() as u16).to_be_bytes());
                        auth.extend_from_slice(&id);
                        auth.extend_from_slice(&signer.lock().unwrap().cose);
                        out.head(5, 3).unwrap();
                        out.head(0, 1).unwrap();
                        out.text("none").unwrap();
                        out.head(0, 2).unwrap();
                        out.bytes(&auth).unwrap();
                        out.head(0, 3).unwrap();
                        out.head(5, 0).unwrap();
                    }
                    2 => {
                        let challenge: [u8; 32] = value
                            .required(&Value::Unsigned(2))
                            .unwrap()
                            .bytes()
                            .unwrap()
                            .try_into()
                            .unwrap();
                        let canonical =
                            crate::fido_ctap::AssertionRequest::new(&id, challenge, 1024).unwrap();
                        assert_eq!(request, canonical.bytes());
                        counter += 1;
                        auth.push(1);
                        auth.extend_from_slice(&counter.to_be_bytes());
                        let mut signed = auth.clone();
                        signed.extend_from_slice(&challenge);
                        let signature =
                            signer.lock().unwrap().sign(&crate::crypto::digest(&signed));
                        out.head(5, 3).unwrap();
                        out.head(0, 1).unwrap();
                        out.head(5, 2).unwrap();
                        out.text("id").unwrap();
                        out.bytes(&id).unwrap();
                        out.text("type").unwrap();
                        out.text("public-key").unwrap();
                        out.head(0, 2).unwrap();
                        out.bytes(&auth).unwrap();
                        out.head(0, 3).unwrap();
                        out.bytes(&signature).unwrap();
                    }
                    other => panic!("unexpected virtual CTAP command {other}"),
                }
                let mut response = vec![0];
                response.extend_from_slice(&out.finish().unwrap());
                response
            })
        }

        fn enroll(
            &self,
            primary: Option<&crate::fido_enroll::Credential>,
        ) -> crate::fido_enroll::Credential {
            use crate::fido_enroll::{Info, MakeCredential, GET_INFO};
            let creation = fresh();
            let proof_hash = fresh();
            assert_ne!(creation, proof_hash);
            let make = match primary {
                Some(primary) => MakeCredential::recovery(
                    Info::parse(&info_reply()).unwrap(),
                    creation,
                    fresh(),
                    primary,
                ),
                None => {
                    MakeCredential::primary(Info::parse(&info_reply()).unwrap(), creation, fresh())
                }
            }
            .unwrap();
            let assertion =
                crate::fido_ctap::AssertionRequest::new(&self.id, proof_hash, 1024).unwrap();
            let token = self.start(vec![
                GET_INFO.to_vec(),
                make.bytes().to_vec(),
                assertion.bytes().to_vec(),
            ]);
            let mut session = session(discover_one(), Duration::from_secs(15));
            let info = session.cbor(GET_INFO).unwrap();
            assert_eq!(info.as_ref(), info_reply());
            assert_eq!(Info::parse(info.as_ref()).unwrap().max_message(), 1024);
            let response = session.cbor(make.bytes()).unwrap();
            let proof = make.proof(response.as_ref(), proof_hash).unwrap();
            let response = session.cbor(proof.bytes()).unwrap();
            let credential = proof
                .verify(
                    response.as_ref(),
                    &mut crate::tpm::Client::new(crate::tpm::Device::open().unwrap()),
                )
                .unwrap();
            assert_eq!(credential.id(), self.id);
            assert_eq!(credential.cose(), self.signer.lock().unwrap().cose);
            drop(session);
            assert_eq!(token.finish(), (3, 0));
            assert!(Device::discover().unwrap().is_empty());
            credential
        }

        fn exchange(&self, request: &[u8]) -> Vec<u8> {
            let token = self.start(vec![request.to_vec()]);
            let mut session = session(discover_one(), Duration::from_secs(15));
            let response = session.cbor(request).unwrap().as_ref().to_vec();
            drop(session);
            assert_eq!(token.finish(), (1, 0));
            assert!(Device::discover().unwrap().is_empty());
            response
        }
    }

    fn enrollment_and_release(second: bool) {
        use crate::fido_enroll::{Info, MakeCredential};
        use crate::fido_metadata::{Metadata, Recovery, Role};
        use crate::tpm::{Client, Device as Tpm, Pcrs};
        assert!(Device::discover().unwrap().is_empty());
        crate::tpm::tests::qemu_extend(&[9; 32]);
        let primary_token = VirtualCredential::new(42);
        let primary = primary_token.enroll(None);
        // Replugging the same virtual token must not bypass recovery exclusion.
        let excluded = MakeCredential::recovery(
            Info::parse(&info_reply()).unwrap(),
            fresh(),
            fresh(),
            &primary,
        )
        .unwrap();
        let response = primary_token.exchange(excluded.bytes());
        assert_eq!(response, [0x19]);
        assert_eq!(
            excluded.proof(&response, fresh()).err().unwrap(),
            "CTAP enrollment refused: 0x19"
        );
        let recovery_token = second.then(|| VirtualCredential::new(43));
        let recovery = recovery_token
            .as_ref()
            .map(|token| token.enroll(Some(&primary)));
        let metadata = Metadata::new(
            1000,
            &primary,
            match &recovery {
                Some(recovery) => Recovery::SecondToken(recovery),
                None => Recovery::Unrecoverable,
            },
        )
        .unwrap();
        let encoded = metadata.encode().unwrap();
        let metadata = Metadata::decode(&encoded, 1000).unwrap();
        assert_eq!(metadata.has_recovery(), second);
        let master = fresh();
        let sealed = Client::new(Tpm::open().unwrap())
            .seal_bound(
                1000,
                Pcrs::parse("7").unwrap(),
                &master,
                &metadata.binding().unwrap(),
            )
            .unwrap();
        let sealed = crate::tpm::BoundKey::decode(&sealed.encode().unwrap()).unwrap();
        let info = Info::parse(&info_reply()).unwrap();
        for (role, token) in [
            (Role::Primary, Some(&primary_token)),
            (Role::Recovery, recovery_token.as_ref()),
        ] {
            let Some(token) = token else {
                assert_eq!(
                    metadata.request(role, fresh(), &info).err().unwrap(),
                    "store is explicitly unrecoverable"
                );
                continue;
            };
            let challenge = fresh();
            let request = metadata.request(role, challenge, &info).unwrap();
            let response = token.exchange(request.bytes());
            assert_eq!(
                request
                    .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                    .unwrap(),
                master
            );
            let changed = fresh();
            assert_ne!(challenge, changed);
            let replay = metadata.request(role, changed, &info).unwrap();
            let error = replay
                .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                .err()
                .unwrap();
            assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        }
        if let Some(token) = recovery_token {
            let impostor = VirtualCredential {
                id: primary_token.id.clone(),
                signer: token.signer,
            };
            let request = metadata.request(Role::Primary, fresh(), &info).unwrap();
            let response = impostor.exchange(request.bytes());
            let error = request
                .unseal(&response, &sealed, Client::new(Tpm::open().unwrap()))
                .err()
                .unwrap();
            assert!(error.starts_with("TPM command 0x177 refused:"), "{error}");
        }
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_enrolls_unrecoverable_and_unseals_with_a_fresh_assertion() {
        guard("fido-enroll-single");
        enrollment_and_release(false);
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_hid_enrolls_recovery_and_refuses_replays_and_wrong_keys() {
        guard("fido-enroll-recovery");
        enrollment_and_release(true);
    }
    type Script = Arc<std::sync::Mutex<std::collections::VecDeque<ExpectedCtap>>>;
    enum ExpectedCtap {
        Info,
        Create {
            hash: [u8; 32],
            excluded: Option<Vec<u8>>,
            user: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
        },
        Assert {
            hash: [u8; 32],
            id: Vec<u8>,
        },
    }

    fn scripted(token: &VirtualCredential) -> (Token, Script) {
        use crate::fido_cbor::{self as cbor, Value};
        use crate::fido_enroll::{Info, MakeCredential};
        let script: Script = Arc::default();
        let input = Arc::clone(&script);
        let token = token.checked(move |request, _| {
            let expected = input
                .lock()
                .unwrap()
                .pop_front()
                .expect("token I/O before its presentation acknowledgement");
            match expected {
                ExpectedCtap::Info => assert_eq!(request, [4]),
                ExpectedCtap::Assert { hash, id } => {
                    let expected =
                        crate::fido_ctap::AssertionRequest::new(&id, hash, 1024).unwrap();
                    assert_eq!(request, expected.bytes());
                }
                ExpectedCtap::Create {
                    hash,
                    excluded,
                    user,
                } => {
                    assert_eq!(request[0], 1);
                    let actual = cbor::decode(&request[1..]).unwrap();
                    let handle: [u8; 32] = actual
                        .required(&Value::Unsigned(3))
                        .unwrap()
                        .required(&Value::Text("id"))
                        .unwrap()
                        .bytes()
                        .unwrap()
                        .try_into()
                        .unwrap();
                    let mut user = user.lock().unwrap();
                    if let Some(previous) = *user {
                        assert_eq!(previous, handle);
                    }
                    *user = Some(handle);
                    let base =
                        MakeCredential::primary(Info::parse(&info_reply()).unwrap(), hash, handle)
                            .unwrap();
                    let mut expected = cbor::decode(&base.bytes()[1..]).unwrap();
                    if let Some(id) = &excluded {
                        let Value::Map(entries) = &mut expected else {
                            panic!("make map")
                        };
                        entries.insert(
                            4,
                            (
                                Value::Unsigned(5),
                                Value::Array(vec![Value::Map(vec![
                                    (Value::Text("id"), Value::Bytes(id)),
                                    (Value::Text("type"), Value::Text("public-key")),
                                ])]),
                            ),
                        );
                    }
                    assert!(
                        actual == expected,
                        "makeCredential did not bind the presented step and exclusion"
                    );
                }
            }
        });
        (token, script)
    }

    struct OperationChild {
        child: std::process::Child,
        log: std::path::PathBuf,
    }
    impl OperationChild {
        fn start(command: &str) -> (Self, crate::operation::Wire) {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let log = std::path::PathBuf::from(format!(
                "/run/private-operation-{}.log",
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let errors = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&log)
                .unwrap();
            let (parent, child) = UnixStream::pair().unwrap();
            let child = Command::new("/bin/td-secret")
                .args([command, "--uid", "1000"])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::from(OwnedFd::from(child)))
                .stdout(Stdio::null())
                .stderr(errors)
                .spawn()
                .unwrap();
            let wire =
                crate::operation::Wire::new(parent, Instant::now() + Duration::from_secs(120))
                    .unwrap();
            (Self { child, log }, wire)
        }
        fn finish(mut self, error: Option<&str>) {
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "private worker failed to exit");
                thread::sleep(Duration::from_millis(10));
            };
            let mut bytes = Vec::new();
            File::open(&self.log)
                .unwrap()
                .take(65_537)
                .read_to_end(&mut bytes)
                .unwrap();
            assert!(bytes.len() <= 65_536, "private worker stderr exceeded 64 KiB");
            let log = String::from_utf8(bytes).expect("private worker stderr was not UTF-8");
            assert_eq!(status.success(), error.is_none(), "{log}");
            if let Some(error) = error {
                assert!(log.contains(error), "{log}");
            } else {
                assert!(log.is_empty(), "{log}");
            }
            assert!(!std::path::Path::new(&format!("/proc/{}", self.child.id())).exists());
        }
    }
    impl Drop for OperationChild {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn invitation(
        wire: &mut crate::operation::Wire,
        tag: u8,
        request: &crate::consent::Request,
    ) -> Vec<u8> {
        let mut frame = wire.receive().unwrap();
        let encoded = request.encode();
        assert_eq!(frame.len(), 33 + encoded.len(), "unexpected private invitation length");
        assert_eq!(frame[0], tag);
        assert_eq!(&frame[33..], encoded);
        frame[0] += 1;
        frame
    }
    fn presented_hash(domain: &[u8], request: &crate::consent::Request) -> [u8; 32] {
        let mut bytes = domain.to_vec();
        bytes.extend_from_slice(&request.encode());
        crate::crypto::digest(&bytes)
    }
    fn no_release() {
        assert!(!std::path::Path::new("/run/td-secret/1000/key").exists());
    }
    fn sealed_bytes() -> Vec<u8> {
        fs::read("/var/lib/td/secrets/1000/sealed").unwrap()
    }

    fn prepare_operation_accounts() {
        use std::os::unix::fs::PermissionsExt;
        assert!(!std::path::Path::new("/etc/td-principals.tsv").exists());
        fs::create_dir_all("/etc").unwrap();
        for (name, text, mode) in [
            ("td-principals.tsv", "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\tmail\t65537\n", 0o444),
            ("passwd", "tester:x:1000:1000::/home/tester:/bin/false\ntda65537:x:65537:65537::/var/lib/td/applications/65537:/bin/false\n", 0o644),
            ("group", "tester:x:1000:\ntda65537:x:65537:\n", 0o644),
            ("shadow", "tester::0:0:99999:7:::\ntda65537:!td-service:0:0:99999:7:::\n", 0o600),
        ] {
            let path = std::path::Path::new("/etc").join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    fn prepare_operation_store() {
        prepare_operation_accounts();
        assert!(!store::user_path(1000).exists());
        fs::create_dir_all("/var/lib/td/secrets").unwrap();
        let store = store::Store::open_owned(&store::user_path(1000), 1000, 991, true).unwrap();
        store.set("mail", "main", b"firstboot fixture").unwrap();
        store.set("news", "main", b"untouched fixture").unwrap();
        assert!(store.application_secret("mail", "main").is_err());
    }

    fn enroll_worker(primary: &VirtualCredential, recovery: Option<&VirtualCredential>) {
        use crate::consent::{Enrollment, Operation, Platform, Recovery, Request};
        let mut request = Request::new(
            fresh(),
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery: if recovery.is_some() {
                    Recovery::SecondToken
                } else {
                    Recovery::Unrecoverable
                },
                step: Enrollment::CreatePrimary,
            },
        )
        .unwrap();
        let old_master = fs::read(store::user_path(1000).join("master")).unwrap();
        let user = Arc::new(std::sync::Mutex::new(None));
        let (primary_hid, primary_script) = scripted(primary);
        let mut recovery_hid = None;
        let mut recovery_script: Option<Script> = None;
        let (child, mut wire) = OperationChild::start("enroll-operation");
        wire.send(&request.encode()).unwrap();
        loop {
            let reply = invitation(&mut wire, 0x10, &request);
            assert!(primary_script.lock().unwrap().is_empty());
            let Operation::Enroll { step, .. } = request.operation() else {
                panic!("enrollment")
            };
            if *step == Enrollment::CreateRecovery {
                let (hid, script) = scripted(recovery.unwrap());
                recovery_hid = Some(hid);
                recovery_script = Some(script);
            }
            let (script, token, excluded) = match step {
                Enrollment::CreatePrimary | Enrollment::ProvePrimary => {
                    (&primary_script, primary, None)
                }
                _ => (
                    recovery_script.as_ref().unwrap(),
                    recovery.unwrap(),
                    Some(primary.id.clone()),
                ),
            };
            assert!(script.lock().unwrap().is_empty());
            let hash = presented_hash(b"td-secret/presented-enrollment/v1\0", &request);
            let mut pending = script.lock().unwrap();
            match step {
                Enrollment::CreatePrimary | Enrollment::CreateRecovery => {
                    pending.push_back(ExpectedCtap::Info);
                    pending.push_back(ExpectedCtap::Create {
                        hash,
                        excluded,
                        user: Arc::clone(&user),
                    });
                }
                _ => pending.push_back(ExpectedCtap::Assert {
                    hash,
                    id: token.id.clone(),
                }),
            }
            drop(pending);
            no_release();
            assert_eq!(
                fs::read(store::user_path(1000).join("master")).unwrap(),
                old_master
            );
            wire.send(&reply).unwrap();
            let Some(next) = request.following_enrollment_step().unwrap() else {
                break;
            };
            request = next;
        }
        let commit = invitation(&mut wire, 0x12, &request);
        assert!(primary_script.lock().unwrap().is_empty());
        if let Some(script) = &recovery_script {
            assert!(script.lock().unwrap().is_empty());
        }
        assert!(!store::user_path(1000).join("sealed").exists());
        no_release();
        wire.send(&commit).unwrap();
        assert_eq!(wire.receive().unwrap(), [0x14]);
        drop(wire);
        child.finish(None);
        assert_eq!(primary_hid.finish(), (3, 0));
        if let Some(token) = recovery_hid {
            assert_eq!(token.finish(), (3, 0));
        }
        assert!(Device::discover().unwrap().is_empty());
        no_release();
        for retired in ["master", "mail.main", "news.main"] {
            assert!(!store::user_path(1000).join(retired).exists());
        }
        let store = crate::owned_store(1000).unwrap();
        assert!(store.token_protected().unwrap());
        assert!(store.application_secret("mail", "main").is_err());
    }

    fn operate(
        token: &VirtualCredential,
        role: crate::consent::Role,
        value: Option<&[u8]>,
        cancel: bool,
        absent_role: bool,
    ) {
        use crate::consent::{Operation, Request};
        let operation = if value.is_some() {
            Operation::Set {
                role,
                application: "mail".into(),
                name: "main".into(),
                application_uid: 65537,
                requester: 1000,
            }
        } else {
            Operation::Unlock { role }
        };
        let request = Request::new(fresh(), 1000, operation).unwrap();
        let domain = if value.is_some() {
            b"td-secret/presented-write/v1\0".as_slice()
        } else {
            b"td-secret/presented-unlock/v1\0".as_slice()
        };
        let before = sealed_bytes();
        no_release();
        let (hid, script) = scripted(token);
        let (child, mut wire) = OperationChild::start(if value.is_some() {
            "write-operation"
        } else {
            "unlock-operation"
        });
        wire.send(&request.encode()).unwrap();
        if let Some(value) = value {
            wire.send(value).unwrap();
        }
        let reply = invitation(&mut wire, 0x10, &request);
        script.lock().unwrap().push_back(ExpectedCtap::Info);
        if !absent_role {
            script.lock().unwrap().push_back(ExpectedCtap::Assert {
                hash: presented_hash(domain, &request),
                id: token.id.clone(),
            });
        }
        wire.send(&reply).unwrap();
        if absent_role {
            assert!(wire.receive().is_err());
            drop(wire);
            child.finish(Some("store is explicitly unrecoverable"));
        } else {
            let commit = invitation(&mut wire, 0x12, &request);
            assert!(script.lock().unwrap().is_empty());
            assert_eq!(sealed_bytes(), before);
            no_release();
            if !cancel {
                wire.send(&commit).unwrap();
                assert_eq!(wire.receive().unwrap(), [0x14]);
            }
            drop(wire);
            child.finish(cancel.then_some("operation authority disconnected"));
        }
        assert!(script.lock().unwrap().is_empty());
        assert_eq!(hid.finish(), (if absent_role { 1 } else { 2 }, 0));
        assert!(Device::discover().unwrap().is_empty());
        if cancel || absent_role || value.is_none() {
            assert_eq!(sealed_bytes(), before);
        } else {
            assert_ne!(sealed_bytes(), before, "successful write left the bundle unchanged");
        }
        if cancel || absent_role || value.is_some() {
            no_release();
        } else {
            assert!(std::path::Path::new("/run/td-secret/1000/key").exists());
        }
    }

    fn read_records(expected: &[u8]) {
        let store = crate::owned_store(1000).unwrap();
        assert_eq!(
            store.application_secret("mail", "main").unwrap().unwrap(),
            expected
        );
        assert_eq!(
            store.application_secret("news", "main").unwrap().unwrap(),
            b"untouched fixture"
        );
    }

    fn private_operations(second: bool) {
        use crate::consent::Role;
        prepare_operation_store();
        crate::tpm::tests::qemu_extend(&[9; 32]);
        let primary = VirtualCredential::new(42);
        let recovery = second.then(|| VirtualCredential::new(43));
        enroll_worker(&primary, recovery.as_ref());
        operate(&primary, Role::Primary, None, true, false);
        operate(&primary, Role::Primary, None, false, false);
        read_records(b"firstboot fixture");
        store::lock_session(1000).unwrap();
        operate(
            &primary,
            Role::Primary,
            Some(b"cancelled fixture"),
            true,
            false,
        );
        operate(
            &primary,
            Role::Primary,
            Some(b"changed fixture"),
            false,
            false,
        );
        operate(&primary, Role::Primary, None, false, false);
        read_records(b"changed fixture");
        store::lock_session(1000).unwrap();
        if let Some(recovery) = recovery {
            operate(
                &recovery,
                Role::Recovery,
                Some(b"recovered fixture"),
                false,
                false,
            );
            operate(&recovery, Role::Recovery, None, false, false);
            read_records(b"recovered fixture");
            store::lock_session(1000).unwrap();
        } else {
            operate(&primary, Role::Recovery, None, false, true);
        }
        no_release();
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_private_workers_enroll_unlock_write_and_cancel_without_recovery() {
        guard("fido-operations-single");
        private_operations(false);
    }

    #[test]
    #[ignore = "requires qemu-secret --tpm with disposable guest HID devices"]
    fn qemu_private_workers_enroll_unlock_and_write_with_recovery() {
        guard("fido-operations-recovery");
        private_operations(true);
    }

    mod desktop {
        use super::*;
        use std::os::unix::fs::{chown, PermissionsExt};
        use std::path::Path;
        use std::sync::atomic::AtomicUsize;

        struct Diagnostics;
        impl Drop for Diagnostics {
            fn drop(&mut self) {
                if !thread::panicking() { return; }
                for log in [
                    "/run/desktop-authd.log", "/run/desktop-compositor.log",
                    "/run/desktop-set.log", "/run/desktop-busd.log",
                    "/run/desktop-portal.log", "/run/desktop-mail.log",
                    "/run/desktop-news.log",
                ] {
                    if let Ok(file) = File::open(log) {
                        let mut bytes = Vec::new();
                        if file.take(65_536).read_to_end(&mut bytes).is_ok() {
                            eprintln!("{log}: {}", String::from_utf8_lossy(&bytes));
                        }
                    }
                }
            }
        }

        fn wait(label: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !done() {
                assert!(Instant::now() < deadline, "desktop fixture timed out: {label}");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn read_released(expected: &[u8]) {
            // Key publication precedes the worker dropping its store lock.
            wait("released store admission", || {
                let Ok(store) = crate::owned_store(1000) else {
                    return false;
                };
                assert_eq!(store.application_secret("mail", "main").unwrap().unwrap(), expected);
                assert_eq!(store.application_secret("news", "main").unwrap().unwrap(), b"untouched fixture");
                true
            });
        }

        const APP_TEST: &str = "fido_device::vm_tests::desktop::qemu_application_portal_client";
        const RUNTIME: &str = "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1";

        fn directory(path: impl AsRef<Path>, owner: u32, mode: u32) {
            let path = path.as_ref();
            fs::create_dir_all(path).unwrap();
            chown(path, Some(owner), Some(owner))
                .unwrap_or_else(|error| panic!("chown {}: {error}", path.display()));
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn application_home(uid: u32, app: &str) -> std::path::PathBuf {
            Path::new("/var/lib/td/applications")
                .join(uid.to_string())
                .join(".td/app")
                .join(app)
                .join("home")
        }

        fn setup_portal(reopen: bool) {
            for (path, owner, mode) in [
                ("/run/td-bus", 0, 0o755),
                ("/run/td-bus/1000", 992, 0o755),
                ("/run/td-portal", 0, 0o755),
                ("/run/td-portal/1000", 991, 0o700),
                ("/var/lib/td/applications", 0, 0o755),
                ("/var/home", 0, 0o755),
                ("/var/home/tester", 1000, 0o700),
                ("/etc/ssl/certs", 0, 0o755),
            ] {
                directory(path, owner, mode);
            }
            directory(format!("{RUNTIME}/files"), 0, 0o755);
            // Only the PEM envelope is admitted; this offline guest performs no TLS.
            fs::write(
                format!("{RUNTIME}/ca.pem"),
                b"-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
            )
            .unwrap();
            std::os::unix::fs::symlink(
                format!("{RUNTIME}/ca.pem"),
                "/etc/ssl/certs/ca-certificates.crt",
            )
            .unwrap();
            fs::write("/etc/td-app.conf", "format=1\npackage-root=/td/store\nstate-root=.td/app\nregistry=/etc/td-applications.tsv\nlauncher-table=/etc/td-launcher.tsv\ncgroup-root=/sys/fs/cgroup/td-user-1000\n").unwrap();
            fs::write("/etc/td-portal-settings", "format=1\ncolor-scheme=1\naccent-color=0.125,0.375,0.75\ncontrast=0\ngtk-theme=Adwaita\nicon-theme=Adwaita\ncursor-theme=Adwaita\ncursor-size=24\nfont-name=Sans 11\ndocument-font-name=Sans 11\nmonospace-font-name=Monospace 11\ntext-scaling-factor=1.0\n").unwrap();
            for (name, text) in [
                            ("passwd", "tdb1000:x:992:992::/run/td-bus/1000:/bin/false\ntdp1000:x:991:991::/run/td-portal/1000:/bin/false\ntda65538:x:65538:65538::/var/lib/td/applications/65538:/bin/false\n"),
                            ("group", "tdb1000:x:992:\ntdp1000:x:991:\ntda65538:x:65538:\n"),
                            ("shadow", "tdb1000:!td-service:0:0:99999:7:::\ntdp1000:!td-service:0:0:99999:7:::\ntda65538:!td-service:0:0:99999:7:::\n"),
                            ("td-principals.tsv", "application\t1000\tnews\t65538\n"),
                        ] {
                            OpenOptions::new().append(true).open(format!("/etc/{name}"))
                                .unwrap().write_all(text.as_bytes()).unwrap();
                        }
            if reopen {
                assert_eq!(fs::read("/etc/td-principals.tsv").unwrap(),
                    fs::read("/var/lib/td/principals.tsv").unwrap());
            } else {
                fs::copy("/etc/td-principals.tsv", "/var/lib/td/principals.tsv").unwrap();
                fs::set_permissions("/var/lib/td/principals.tsv",
                    fs::Permissions::from_mode(0o600)).unwrap();
            }
            fs::write(
                "/etc/td-bus-applications.tsv",
                "td-bus-applications-v1\t1000\n65537\tmail\t\n65538\tnews\t\n",
            )
            .unwrap();
            // Mount the real hierarchy and delegate each app's sibling leaves.
            fs::create_dir_all("/sys/fs/cgroup").unwrap();
            assert!(Command::new("/bin/td-init")
                .args(["mount", "-t", "cgroup2", "cgroup2", "/sys/fs/cgroup"])
                .status()
                .unwrap()
                .success());
            fs::write(
                "/sys/fs/cgroup/cgroup.subtree_control",
                "+cpu +memory +pids\n",
            )
            .unwrap();
            let mut registry = String::new();
            for (uid, app) in [(65537, "mail"), (65538, "news")] {
                directory(format!("/var/lib/td/applications/{uid}"), uid, 0o700);
                directory(format!("/run/user/{uid}"), uid, 0o700);
                let home = application_home(uid, app);
                for ancestor in home
                    .ancestors()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    directory(ancestor, uid, 0o700);
                }
                let cgroup = format!("/sys/fs/cgroup/td-app-{uid}");
                directory(&cgroup, uid, 0o755);
                fs::write(
                    format!("{cgroup}/cgroup.subtree_control"),
                    "+cpu +memory +pids\n",
                )
                .unwrap();
                directory(format!("{cgroup}/session"), 0, 0o755);
                for leaf in [
                    "cgroup.procs",
                    "cgroup.threads",
                    "cgroup.subtree_control",
                    "session/cgroup.procs",
                    "session/cgroup.threads",
                ] {
                    chown(format!("{cgroup}/{leaf}"), Some(uid), Some(uid)).unwrap();
                }
                let package = format!("/td/store/portal-{app}");
                directory(format!("{package}/files/bin"), 0, 0o755);
                fs::hard_link("/bin/td-secret-tests", format!("{package}/files/bin/probe")).unwrap();
                fs::hard_link("/bin/td-secret", format!("{package}/files/bin/td-secret")).unwrap();
                fs::write(
                    format!("{package}/manifest"),
                    "disposable source-built portal fixture\n",
                )
                .unwrap();
                // Both apps advertise mail: only the broker's UID binding counts.
                fs::write(format!("{package}/spec"), format!("format=1\nname={app}\nruntime={RUNTIME}\nentry=/app/bin/probe\n\n[Environment]\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\nFLATPAK_ID=mail\nHOME=/home/td\nWAYLAND_DISPLAY=wayland-0\nXDG_RUNTIME_DIR=/run/user/1000\n\n[Context]\nsockets=wayland\n")).unwrap();
                std::os::unix::fs::symlink("/bin/td-jail", format!("/bin/{app}")).unwrap();
                registry.push_str(&format!("{app}\t{package}\n"));
            }
            fs::write("/etc/td-applications.tsv", registry).unwrap();
            std::os::unix::fs::symlink("/bin/td-init", "/bin/umount").unwrap();
            application_files("prepare-application-files");
            if !reopen {
                let store = crate::owned_store(1000).unwrap();
                store.set("mail", "private", b"mail-only fixture").unwrap();
            }
        }

        fn application_files(operation: &str) {
            let output = Command::new("/bin/td-authd")
                .args([operation, "mail"])
                .env_clear()
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        struct Portal {
            broker: Process,
            supervisor: Process,
        }
        impl Portal {
            fn start() -> Self {
                let mut command = Command::new("/bin/td-login");
                command.args([
                    "exec-service-as",
                    "tdb1000",
                    "--",
                    "/bin/td-busd",
                    "run-session",
                ]);
                let mut broker = Process::start(command, "/run/desktop-busd.log");
                wait("session broker", || {
                    assert!(
                        broker.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-busd.log").unwrap()
                    );
                    fs::read_to_string("/run/desktop-busd.log")
                        .unwrap()
                        .contains("td-busd: listening on /run/td-bus/1000/bus as ")
                });
                let mut command = Command::new("/bin/td-portal");
                command.args([
                    "supervise",
                    "--bus",
                    "/run/td-bus/1000/bus",
                    "--settings",
                    "/etc/td-portal-settings",
                ]);
                let supervisor = Process::start(command, "/run/desktop-portal.log");
                let mut portal = Self { broker, supervisor };
                wait("activated portal", || {
                    portal.live();
                    let result = Command::new("/bin/td-secret")
                        .args(["get", "main"])
                        .env_clear()
                        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/td-bus/1000/bus")
                        .output()
                        .unwrap();
                    assert!(!result.status.success() && result.stdout.is_empty());
                    let error = String::from_utf8(result.stderr).unwrap();
                    if error == "td-secret: credential portal refused the request: org.freedesktop.DBus.Error.NameHasNoOwner\n" {
                                    return false;
                                }
                    assert_eq!(error, "td-secret: credential portal refused the request: org.freedesktop.portal.Error.NotAllowed\n");
                    true
                });
                portal
            }
            fn live(&mut self) {
                assert!(self.broker.exited().is_none());
                assert!(
                    self.supervisor.exited().is_none(),
                    "{}",
                    fs::read_to_string("/run/desktop-portal.log").unwrap()
                );
            }
            fn finish(mut self) {
                self.live();
                self.broker.0.kill().unwrap();
                assert!(!self.broker.0.wait().unwrap().success());
                let mut status = None;
                wait("portal child and supervisor exit", || {
                    status = self.supervisor.exited();
                    status.is_some()
                });
                assert!(!status.unwrap().success());
            }
        }

        struct Application {
            process: Process,
            home: std::path::PathBuf,
            sequence: usize,
        }
        impl Application {
            fn start(uid: u32, app: &str) -> Self {
                let mut command = Command::new("/bin/td-login");
                command.args([
                    "exec-service-as",
                    &format!("tda{uid}"),
                    "--",
                    &format!("/bin/{app}"),
                    "--exact",
                    APP_TEST,
                    "--ignored",
                    "--test-threads=1",
                ]);
                let mut result = Self {
                    process: Process::start(command, &format!("/run/desktop-{app}.log")),
                    home: application_home(uid, app),
                    sequence: 0,
                };
                wait("jailed application startup", || {
                    assert!(
                        result.process.exited().is_none(),
                        "{}",
                        fs::read_to_string(format!("/run/desktop-{app}.log")).unwrap()
                    );
                    result.home.join("ready").exists()
                });
                result
            }
            fn retrieve(&mut self, name: &str, expected: &str) {
                self.sequence += 1;
                let sequence = self.sequence;
                fs::write(
                    self.home.join("request.next"),
                    format!("{sequence}\t{name}\t{expected}"),
                )
                .unwrap();
                // Root is the disposable fixture controller; the app only reads.
                fs::set_permissions(
                    self.home.join("request.next"),
                    fs::Permissions::from_mode(0o444),
                )
                .unwrap();
                fs::rename(self.home.join("request.next"), self.home.join("request")).unwrap();
                wait("application portal response", || {
                    assert!(self.process.exited().is_none());
                    let response = fs::read_to_string(self.home.join("response")).unwrap_or_default();
                    if !response.starts_with(&format!("{sequence}\t")) {
                        return false;
                    }
                    assert_eq!(response, format!("{sequence}\tok"));
                    true
                });
            }
            fn finish(mut self) {
                fs::write(self.home.join("stop"), b"").unwrap();
                let mut status = None;
                wait("application exit", || {
                    status = self.process.exited();
                    status.is_some()
                });
                assert!(status.unwrap().success());
                for name in ["ready", "request", "response", "stop"] {
                    fs::remove_file(self.home.join(name)).unwrap();
                }
            }
        }

        #[test]
        #[ignore = "test-only application entry for the disposable desktop portal guest"]
        fn qemu_application_portal_client() {
            assert_eq!(fs::metadata("/proc/self").unwrap().uid(), 1000);
            assert!(!Path::new("/var/lib/td/secrets").exists());
            assert!(!Path::new("/run/td-secret").exists());
            let home = Path::new("/home/td");
            fs::write(home.join("ready"), b"ready").unwrap();
            let deadline = Instant::now() + Duration::from_secs(90);
            let mut previous = String::new();
            while !home.join("stop").exists() {
                assert!(Instant::now() < deadline);
                let request = fs::read_to_string(home.join("request")).unwrap_or_default();
                if !request.is_empty() && request != previous {
                    let parts: Vec<_> = request.split('\t').collect();
                    assert_eq!(parts.len(), 3);
                    let result = Command::new("/app/bin/td-secret")
                        .args(["get", parts[1]])
                        .output()
                        .unwrap();
                    let success = if parts[2] == "unavailable" {
                        !result.status.success() && result.stdout.is_empty()
                                        && std::str::from_utf8(&result.stderr).is_ok_and(|error|
                                            error == "td-secret: credential portal refused the request: org.freedesktop.portal.Error.Failed\n")
                    } else {
                        result.status.success() && result.stdout == parts[2].as_bytes()
                    };
                    let response = if success {
                        "ok".into()
                    } else {
                        format!(
                            "failed: {} {}",
                            result.status,
                            String::from_utf8_lossy(&result.stderr)
                        )
                    };
                    fs::write(
                        home.join("response.next"),
                        format!("{}\t{response}", parts[0]),
                    )
                    .unwrap();
                    fs::rename(home.join("response.next"), home.join("response")).unwrap();
                    previous = request;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Standard keyboard: eight modifiers, padding, and six key usages.
        const KEYBOARD: &[u8] = &[
            5, 1, 9, 6, 0xa1, 1, 5, 7, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0, 0x25, 1, 0x75, 1, 0x95, 8,
            0x81, 2, 0x95, 1, 0x75, 8, 0x81, 1, 0x95, 6, 0x75, 8, 0x15, 0, 0x25, 0x65, 5, 7, 0x19, 0,
            0x29, 0x65, 0x81, 0, 0xc0,
        ];

        struct Keyboard(File);
        impl Keyboard {
            fn new() -> Self {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(NOFOLLOW | NONBLOCK)
                    .open("/dev/uhid")
                    .unwrap();
                let mut event = [0; UHID_EVENT_SIZE];
                event[..4].copy_from_slice(&11_u32.to_ne_bytes());
                event[4..24].copy_from_slice(b"td desktop keyboard\0");
                event[260..262].copy_from_slice(&(KEYBOARD.len() as u16).to_ne_bytes());
                event[262..264].copy_from_slice(&3_u16.to_ne_bytes());
                event[264..268].copy_from_slice(&0x1209_u32.to_ne_bytes());
                event[268..272].copy_from_slice(&2_u32.to_ne_bytes());
                event[CREATE2_DESCRIPTOR..CREATE2_DESCRIPTOR + KEYBOARD.len()]
                    .copy_from_slice(KEYBOARD);
                write_event(&mut file, &event);
                wait("keyboard enumeration", || {
                    fs::read_dir("/sys/class/input").unwrap().any(|entry| {
                        let path = entry.unwrap().path();
                        path.file_name()
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .starts_with("event")
                            && fs::read_to_string(path.join("device/name")).ok().as_deref()
                                == Some("td desktop keyboard\n")
                    })
                });
                Self(file)
            }
            fn report(&mut self, modifiers: u8, key: u8) {
                let mut event = [0; 14];
                event[..4].copy_from_slice(&12_u32.to_ne_bytes());
                event[4..6].copy_from_slice(&8_u16.to_ne_bytes());
                event[6] = modifiers;
                event[8] = key;
                write_event(&mut self.0, &event);
                thread::sleep(Duration::from_millis(100));
            }
            fn key(&mut self, key: u8) {
                self.report(0, key);
                self.report(0, 0);
            }
            fn select(&mut self, key: u8) {
                // A fresh report drains the post-close input quarantine.
                self.key(0x39); // Caps Lock, outside the attention vocabulary.
                self.report(5, 0); // Left Ctrl + Left Alt.
                self.report(5, 0x29); // Escape.
                self.report(0, 0);
                self.key(key);
            }
            fn close(&mut self) {
                self.key(0x29);
            }
        }

        struct Process(std::process::Child);
        impl Process {
            fn start(mut command: Command, log: &str) -> Self {
                let errors = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(log)
                    .unwrap();
                command
                    .env_clear()
                    .current_dir("/")
                    .stdout(Stdio::null())
                    .stderr(errors);
                Self(command.spawn().unwrap())
            }
            fn exited(&mut self) -> Option<std::process::ExitStatus> {
                self.0.try_wait().unwrap()
            }
        }
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        struct Pair {
            compositor: Process,
            authority: Process,
        }
        impl Pair {
            fn start() -> Self {
                let (root, peer) = UnixStream::pair().unwrap();
                let mut command = Command::new("/bin/td-authd");
                command
                    .args([
                        "terminal-serve",
                        "--user",
                        "tester",
                        "--uid",
                        "1000",
                        "--peer-uid",
                        "993",
                    ])
                    .stdin(Stdio::from(OwnedFd::from(root)));
                let authority = Process::start(command, "/run/desktop-authd.log");
                let mut command = Command::new("/bin/td-login");
                command
                    .args([
                        "exec-service-as",
                        "tdc1000",
                        "--",
                        "/bin/td-compositor",
                        "run",
                        "--framebuffer",
                        "/dev/fb0",
                        "--input",
                        "/dev/input",
                        "--socket",
                        "/run/td-compositor/1000/wayland-0",
                        "--portal-socket",
                        "/run/td-compositor/1000/portal-wayland",
                        "--control-socket",
                        "/run/td-compositor/1000/td-control",
                        "--launcher-application",
                        "mail",
                        "--application-ready-socket",
                        "/run/td-compositor/1000/application-ready",
                        "--application-app-id",
                        "td.mail",
                        "--application-content-rgb-a",
                        "112233",
                        "--application-content-rgb-b",
                        "445566",
                        "--terminal-authority",
                        "stdin",
                    ])
                    .stdin(Stdio::from(OwnedFd::from(peer)));
                let compositor = Process::start(command, "/run/desktop-compositor.log");
                let mut pair = Self {
                    compositor,
                    authority,
                };
                wait("paired compositor startup", || {
                    assert!(
                        pair.authority.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-authd.log").unwrap()
                    );
                    assert!(
                        pair.compositor.exited().is_none(),
                        "{}",
                        fs::read_to_string("/run/desktop-compositor.log").unwrap()
                    );
                    fs::read_to_string("/run/desktop-compositor.log")
                        .unwrap()
                        .contains("software output")
                });
                no_release();
                pair
            }
            fn disconnect(mut self) {
                assert!(self.authority.exited().is_none());
                assert!(self.compositor.exited().is_none());
                self.compositor.0.kill().unwrap();
                assert!(!self.compositor.0.wait().unwrap().success());
                let mut status = None;
                wait("authority generation cleanup", || {
                    status = self.authority.exited();
                    status.is_some()
                });
                assert!(!status.unwrap().success());
                no_release();
                assert!(!Path::new("/run/td-authd/1000/set").exists());
            }
        }

        fn setup(reopen: bool) {
            if reopen { prepare_operation_accounts(); } else { prepare_operation_store(); }
            fs::write("/etc/td-bus-applications.tsv",
                "td-bus-applications-v1\t1000\n65537\tmail\t\n").unwrap();
            fs::set_permissions("/etc/td-bus-applications.tsv",
                fs::Permissions::from_mode(0o444)).unwrap();
            for (name, text) in [
                (
                    "passwd",
                    "tdc1000:x:993:993::/run/td-compositor/1000:/bin/false\n",
                ),
                ("group", "tdc1000:x:993:\n"),
                ("shadow", "tdc1000:!td-service:0:0:99999:7:::\n"),
            ] {
                OpenOptions::new()
                    .append(true)
                    .open(format!("/etc/{name}"))
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
            }
            for (path, owner, mode) in [
                ("/run/td-compositor", 0, 0o755),
                ("/run/td-compositor/1000", 993, 0o755),
                ("/run/user", 0, 0o755),
                ("/run/user/1000", 1000, 0o700),
                ("/home/tester", 1000, 0o700),
            ] {
                fs::create_dir_all(path).unwrap();
                chown(path, Some(owner), Some(owner)).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            }
            for entry in fs::read_dir("/dev/input").unwrap() {
                let path = entry.unwrap().path();
                if !path.file_name().unwrap().to_str().unwrap().starts_with("event") {
                    continue;
                }
                assert!(fs::symlink_metadata(&path).unwrap().file_type().is_char_device());
                chown(&path, Some(993), Some(993)).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            chown("/dev/fb0", Some(993), Some(993)).unwrap();
            fs::set_permissions("/dev/fb0", fs::Permissions::from_mode(0o600)).unwrap();
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with a disposable desktop and HID devices"]
        fn qemu_compositor_enrolls_unlocks_and_authorizes_public_credential_write() {
            guard("fido-desktop");
            desktop_roundtrip(false, false);
        }

        fn desktop_roundtrip(persistent: bool, recovery: bool) {
            assert!(!recovery || persistent);
            let _diagnostics = Diagnostics;
            assert!(Command::new("/bin/td-init")
                .args(["hostname", "td-secret-fixture"])
                .status().unwrap().success());
            let mut keyboard = Keyboard::new();
            // Mail's declared idmapped view needs a mountable backing filesystem.
            fs::create_dir_all("/var").unwrap();
            if persistent {
                mount_persistent_var(true);
            } else {
                applet(&["mount", "-t", "tmpfs", "-o", "nosuid,nodev,mode=0755", "tmpfs", "/var"]);
            }
            setup(false);
            setup_portal(false);
            crate::tpm::tests::qemu_extend(&[9; 32]);
            let token = if persistent { persistent_token(true, false) } else { VirtualCredential::new(44) };
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&requests);
            let enrollment_user = Arc::new(std::sync::Mutex::new(None::<Vec<u8>>));
            let primary_user = Arc::clone(&enrollment_user);
            let mut challenges = std::collections::BTreeSet::new();
            let hid = token.checked(move |request, index| {
                eprintln!("desktop CTAP {index}: {}", request[0]);
                let expected = [4, 1, 2, 4, 2, 4, 2, 4, 2];
                assert_eq!(request[0], expected[index]);
                if request[0] != 4 {
                    use crate::fido_cbor::{self, Value};
                    let value = fido_cbor::decode(&request[1..]).unwrap();
                    if request[0] == 1 {
                        *primary_user.lock().unwrap() = Some(value.required(&Value::Unsigned(3)).unwrap()
                            .required(&Value::Text("id")).unwrap().bytes().unwrap().to_vec());
                    }
                    let field = if request[0] == 1 { 1 } else { 2 };
                    let hash: [u8; 32] = value.required(&Value::Unsigned(field)).unwrap()
                        .bytes().unwrap().try_into().unwrap();
                    assert_ne!(hash, [0; 32]);
                    assert!(challenges.insert(hash), "desktop reused a token challenge");
                    if persistent {
                        let path = format!("{COLD_STATE}/challenges");
                        if Path::new(&path).exists() {
                            let prior = fs::read(&path).unwrap();
                            assert!(!prior.as_chunks::<32>().0.contains(&hash));
                        }
                        OpenOptions::new().append(true).create(true)
                            .mode(0o600).open(path).unwrap().write_all(&hash).unwrap();
                    }
                }
                observed.store(index + 1, Ordering::SeqCst);
            });
            let pair = Pair::start();
            let mut portal = Portal::start();
            let mut mail = Application::start(65537, "mail");
            let mut news = Application::start(65538, "news");
            portal.live();
            mail.retrieve("main", "unavailable");
            keyboard.key(0x1b); // X outside attention must not enroll.
            thread::sleep(Duration::from_millis(500));
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            assert!(!Path::new("/var/lib/td/secrets/1000/sealed").exists());
            keyboard.select(if recovery { 0x08 } else { 0x1b }); // E: recovery; X: unrecoverable.
            let recovery_hid = if recovery {
                wait("primary proof request", || requests.load(Ordering::SeqCst) == 3);
                let second = persistent_token(true, true);
                assert_ne!(token.signer.lock().unwrap().cose, second.signer.lock().unwrap().cose);
                Some(second.checked(move |request, index| {
                    use crate::fido_cbor::{self, Value};
                    assert_eq!(request[0], [4, 1, 2][index]);
                    if request[0] != 4 {
                        let value = fido_cbor::decode(&request[1..]).unwrap();
                        if request[0] == 1 {
                            let user = value.required(&Value::Unsigned(3)).unwrap()
                                .required(&Value::Text("id")).unwrap().bytes().unwrap();
                            assert_eq!(Some(user), enrollment_user.lock().unwrap().as_deref());
                            let Value::Array(excluded) = value.required(&Value::Unsigned(5)).unwrap() else {
                                panic!("recovery creation omitted the primary exclusion");
                            };
                            assert_eq!(excluded.len(), 1);
                            assert_eq!(excluded[0].required(&Value::Text("id")).unwrap().bytes().unwrap(), [44; 32]);
                        }
                        let field = if request[0] == 1 { 1 } else { 2 };
                        let hash = value.required(&Value::Unsigned(field)).unwrap().bytes().unwrap();
                        assert_eq!(hash.len(), 32);
                        assert_ne!(hash, [0; 32]);
                        let path = format!("{COLD_STATE}/challenges");
                        let prior = fs::read(&path).unwrap();
                        assert!(!prior.as_chunks::<32>().0.iter().any(|old| old == hash));
                        OpenOptions::new().append(true).open(path).unwrap().write_all(hash).unwrap();
                    }
                }))
            } else { None };
            wait("desktop enrollment", || {
                Path::new("/var/lib/td/secrets/1000/sealed").exists()
            });
            no_release();
            if let Some(hid) = recovery_hid {
                assert_eq!(hid.finish(), (3, 0));
                wait("recovery device removal", || Device::discover().unwrap().len() == 1);
            }
            assert_eq!(requests.load(Ordering::SeqCst), 3);
            mail.retrieve("main", "unavailable");
            keyboard.close();
            keyboard.select(0x18); // U: fresh primary assertion.
            wait("desktop unlock", || {
                Path::new("/run/td-secret/1000/key").exists()
            });
            read_released(b"firstboot fixture");
            mail.retrieve("main", "firstboot fixture");
            mail.retrieve("private", "mail-only fixture");
            news.retrieve("main", "untouched fixture");
            news.retrieve("private", "unavailable");
            keyboard.close();
            let before = sealed_bytes();
            let mut command = Command::new("/bin/td-login");
            command
                .args([
                    "exec-as",
                    "tester",
                    "--",
                    "/bin/td-secret",
                    "set",
                    "mail/main",
                ])
                .stdin(Stdio::piped());
            let mut client = Process::start(command, "/run/desktop-set.log");
            client
                .0
                .stdin
                .take()
                .unwrap()
                .write_all(b"desktop fixture")
                .unwrap();
            wait("public credential queue", || {
                assert!(client.exited().is_none(), "{}", fs::read_to_string("/run/desktop-set.log").unwrap());
                fs::read_to_string("/run/desktop-set.log")
                    .unwrap()
                    .contains("then W")
            });
            thread::sleep(Duration::from_millis(500));
            assert!(client.exited().is_none());
            assert_eq!(sealed_bytes(), before);
            assert_eq!(requests.load(Ordering::SeqCst), 5);
            keyboard.select(0x1a); // W: authorize exactly the queued write.
            let mut status = None;
            wait("public credential completion", || {
                status = client.exited();
                status.is_some()
            });
            assert!(
                status.unwrap().success(),
                "{}",
                fs::read_to_string("/run/desktop-set.log").unwrap()
            );
            assert_ne!(sealed_bytes(), before);
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 7);
            mail.retrieve("main", "desktop fixture");
            news.retrieve("main", "untouched fixture");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            // A fresh production generation must prepare while locked and
            // require another physical selection and assertion before release.
            let pair = Pair::start();
            keyboard.select(0x18);
            wait("replacement generation unlock", || {
                Path::new("/run/td-secret/1000/key").exists()
            });
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 9);
            mail.retrieve("main", "desktop fixture");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            mail.finish();
            news.finish();
            portal.finish();
            application_files("release-application-files");
            assert_eq!(hid.finish(), (9, 0));
            if persistent {
                fs::write(format!("{COLD_STATE}/bundle-hash"), crate::crypto::digest(&sealed_bytes())).unwrap();
                fs::copy("/proc/sys/kernel/random/boot_id", format!("{COLD_STATE}/boot-id")).unwrap();
                applet(&["umount", "/var"]);
            }
        }

        const COLD_STATE: &str = "/var/lib/td/secret-fixture";

        fn applet(args: &[&str]) {
            assert!(Command::new("/bin/td-init").args(args).status().unwrap().success(), "{args:?}");
        }

        fn mount_persistent_var(create: bool) {
            assert!(fs::metadata("/dev/vda").unwrap().file_type().is_block_device());
            fs::create_dir_all("/var").unwrap();
            if create {
                let mut prefix = [0; 4096];
                File::open("/dev/vda").unwrap().read_exact(&mut prefix).unwrap();
                assert_eq!(prefix, [0; 4096], "fixture disk is not fresh");
                assert!(Command::new("/bin/mkfs.btrfs").args(["-q", "/dev/vda"])
                    .status().unwrap().success());
                fs::create_dir("/volume").unwrap();
                applet(&["mount", "-t", "btrfs", "-o", "nosuid,nodev", "/dev/vda", "/volume"]);
                assert!(Command::new("/bin/btrfs").args(["subvolume", "create", "/volume/@var"])
                    .status().unwrap().success());
                applet(&["umount", "/volume"]);
            }
            applet(&["mount", "-t", "btrfs", "-o", "nosuid,nodev,subvol=@var", "/dev/vda", "/var"]);
            assert!(fs::read_to_string("/proc/self/mountinfo").unwrap().lines().any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.get(3) == Some(&"/@var") && fields.get(4) == Some(&"/var")
                    && fields.get(5).is_some_and(|options|
                        ["rw", "nosuid", "nodev"].iter().all(|required|
                            options.split(',').any(|option| option == *required)))
                    && line.contains(" - btrfs ")
            }));
        }

        fn persistent_token(create: bool, recovery: bool) -> VirtualCredential {
            let role = if recovery { "recovery" } else { "primary" };
            if create {
                directory(COLD_STATE, 0, 0o700);
                fs::write(format!("{COLD_STATE}/{role}-template"), fresh()).unwrap();
            }
            let seed: [u8; 32] = fs::read(format!("{COLD_STATE}/{role}-template")).unwrap().try_into().unwrap();
            let signer = crate::tpm::tests::SigningKey::persistent(&seed);
            let public = format!("{COLD_STATE}/{role}-public");
            if create { fs::write(public, &signer.cose).unwrap(); }
            else { assert_eq!(fs::read(public).unwrap(), signer.cose, "cold token changed key"); }
            VirtualCredential { id: vec![if recovery { 45 } else { 44 }; 32], signer: Arc::new(std::sync::Mutex::new(signer)) }
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with disposable persistent disk"]
        fn qemu_desktop_creates_persistent_store() {
            guard("fido-cold-create");
            desktop_roundtrip(true, false);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm after the persistent creation guest"]
        fn qemu_desktop_reopens_persistent_store_locked() {
            guard("fido-cold-reopen");
            cold_reopen(false);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm with a disposable recovery-policy disk"]
        fn qemu_desktop_creates_persistent_recovery_store() {
            guard("fido-cold-recovery-create");
            desktop_roundtrip(true, true);
        }

        #[test]
        #[ignore = "requires qemu-secret --tpm after recovery-policy creation"]
        fn qemu_desktop_recovers_persistent_store_without_primary() {
            guard("fido-cold-recovery-reopen");
            cold_reopen(true);
        }

        fn cold_reopen(recovery: bool) {
            let _diagnostics = Diagnostics;
            applet(&["hostname", "td-secret-fixture"]);
            let mut keyboard = Keyboard::new();
            mount_persistent_var(false);
            assert_ne!(fs::read("/proc/sys/kernel/random/boot_id").unwrap(),
                fs::read(format!("{COLD_STATE}/boot-id")).unwrap());
            let before = sealed_bytes();
            assert_eq!(crate::crypto::digest(&before).as_slice(),
                fs::read(format!("{COLD_STATE}/bundle-hash")).unwrap());
            for name in ["master", "mail.main", "news.main", "mail.private"] {
                assert!(!store::user_path(1000).join(name).exists());
            }
            no_release();
            setup(true);
            setup_portal(true);
            assert_eq!(sealed_bytes(), before, "reopen setup rewrote the store");
            crate::tpm::tests::qemu_extend(&[9; 32]);
            let token = persistent_token(false, recovery);
            let prior = fs::read(format!("{COLD_STATE}/challenges")).unwrap();
            assert_eq!(prior.len(), if recovery { 7 * 32 } else { 5 * 32 });
            let requests = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&requests);
            let hid = token.checked(move |request, index| {
                assert_eq!(request[0], [4, 2][index]);
                if request[0] == 2 {
                    use crate::fido_cbor::{self, Value};
                    let value = fido_cbor::decode(&request[1..]).unwrap();
                    let hash = value.required(&Value::Unsigned(2)).unwrap().bytes().unwrap();
                    assert_eq!(hash.len(), 32);
                    assert_ne!(hash, [0; 32]);
                    assert!(!prior.as_chunks::<32>().0.iter().any(|old| old == hash), "cold unlock replayed a challenge");
                }
                observed.store(index + 1, Ordering::SeqCst);
            });
            discover_one();
            let pair = Pair::start();
            let mut portal = Portal::start();
            let mut mail = Application::start(65537, "mail");
            let mut news = Application::start(65538, "news");
            mail.retrieve("main", "unavailable");
            news.retrieve("main", "unavailable");
            assert_eq!(requests.load(Ordering::SeqCst), 0);
            keyboard.select(if recovery { 0x15 } else { 0x18 }); // R or U.
            wait("cold desktop unlock", || Path::new("/run/td-secret/1000/key").exists());
            read_released(b"desktop fixture");
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            mail.retrieve("main", "desktop fixture");
            mail.retrieve("private", "mail-only fixture");
            news.retrieve("main", "untouched fixture");
            news.retrieve("private", "unavailable");
            pair.disconnect();
            portal.live();
            mail.retrieve("main", "unavailable");
            mail.finish();
            news.finish();
            portal.finish();
            application_files("release-application-files");
            assert_eq!(hid.finish(), (2, 0));
            assert_eq!(sealed_bytes(), before, "cold unlock rewrote the store");
            applet(&["umount", "/var"]);
        }
    }
}
