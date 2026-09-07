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
