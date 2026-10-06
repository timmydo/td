//! Test-only Linux UHID devices for the guest oracles (td-secret/DESIGN.md,
//! "HID through the QEMU guest kernel"): the event codec the scripted
//! fixtures and the desktop keyboard share, and `Plugged`, which presents a
//! `fido_virtual` key as a USB FIDO hidraw device speaking CTAPHID. It is
//! plain file I/O on `/dev/uhid`; nothing here is compiled into the program.

use crate::fido_hid::{self as hid, REPORT_SIZE};
use crate::fido_virtual::Virtual;
use crate::store;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const NOFOLLOW: i32 = 0o400000;
const NONBLOCK: i32 = 0o4000;

// struct uhid_event types (linux/uhid.h).
const OUTPUT: u32 = 6;
const CREATE2: u32 = 11;
const INPUT2: u32 = 12;
/// START, STOP, OPEN and CLOSE: lifecycle events the fixtures pass over.
const LIFECYCLE: &[u32] = &[2, 3, 4, 5];
/// uhid_create2_req's report descriptor, after name, phys, uniq, its size,
/// bus, vendor, product, version and country.
const CREATE2_DESCRIPTOR: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4;
const EVENT_SIZE: usize = CREATE2_DESCRIPTOR + 4096;
/// uhid_output_req's size and report type, after its 4096 data bytes.
const OUTPUT_SIZE: usize = 4 + 4096;
const OUTPUT_REPORT: u8 = 1;
const BUS_USB: u16 = 3;
const VENDOR: u32 = 0x1209;
/// The unnumbered 64-byte FIDO application collection discovery admits.
pub(crate) const FIDO_DESCRIPTOR: &[u8] = &[
    0x06, 0xd0, 0xf1, 0x09, 1, 0xa1, 1, 0x09, 0x20, 0x15, 0, 0x26, 0xff, 0, 0x75, 8, 0x95, 64,
    0x81, 2, 0x09, 0x21, 0x95, 64, 0x91, 2, 0xc0,
];
pub(crate) const FIDO_PRODUCT: u32 = 1;

// CTAPHID (CTAP 2.1 section 11.2).
const INIT: u8 = 0x86;
const KEEPALIVE: u8 = 0xbb;
const STATUS_PROCESSING: u8 = 1;
const STATUS_UPNEEDED: u8 = 2;
/// CTAPHID protocol 2, device version 1.0.0, and the capabilities CBOR
/// (4) and NMSG (8): a CTAP2-only key that implements no CTAPHID_MSG.
const INIT_TAIL: &[u8] = &[2, 1, 0, 0, 0x0c];
/// The longest a pending request may go without a keepalive (section
/// 11.2.9.2.1).
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(2);
/// How often keepalives go: one poll short of the interval, so the poll
/// cannot carry a gap past it; scheduling still can.
pub(crate) const KEEPALIVE_PERIOD: Duration = KEEPALIVE_INTERVAL.saturating_sub(POLL);
/// A plugged key's thread ends itself past this, within the guest's bound.
const LIFETIME: Duration = Duration::from_secs(170);

/// Requires the guest's kernel opt-in, the exact case selector and root
/// before any fixture opens `/dev/uhid`.
pub(crate) fn guard(case: &str) {
    assert!(fs::read_to_string("/proc/cmdline")
        .unwrap()
        .split_ascii_whitespace()
        .any(|arg| arg == "td.hid-fixture=1"));
    assert_eq!(fs::read_to_string("/case").unwrap(), case);
    store::require_root().unwrap();
}

/// One UHID device; dropping it destroys the device.
pub(crate) struct Uhid {
    file: File,
    event: Vec<u8>,
}

impl Uhid {
    /// A USB-bus device under the pid.codes test vendor, after checking that
    /// `/dev/uhid` is a root-owned, root-group mode-0600 character device.
    pub(crate) fn create(name: &str, product: u32, descriptor: &[u8]) -> Self {
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
        assert!(name.len() < 128);
        let mut event = vec![0; EVENT_SIZE];
        event[..4].copy_from_slice(&CREATE2.to_ne_bytes());
        event[4..4 + name.len()].copy_from_slice(name.as_bytes());
        event[260..262].copy_from_slice(&(descriptor.len() as u16).to_ne_bytes());
        event[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
        event[264..268].copy_from_slice(&VENDOR.to_ne_bytes());
        event[268..272].copy_from_slice(&product.to_ne_bytes());
        event[CREATE2_DESCRIPTOR..CREATE2_DESCRIPTOR + descriptor.len()]
            .copy_from_slice(descriptor);
        write(&mut file, &event);
        Self { file, event }
    }

    /// One input report.
    pub(crate) fn input(&mut self, report: &[u8]) {
        let mut event = vec![0; 6 + report.len()];
        event[..4].copy_from_slice(&INPUT2.to_ne_bytes());
        event[4..6].copy_from_slice(&(report.len() as u16).to_ne_bytes());
        event[6..].copy_from_slice(report);
        write(&mut self.file, &event);
    }

    /// The next queued FIDO output report, passing over lifecycle events,
    /// or none when nothing is queued.
    pub(crate) fn output(&mut self) -> Option<[u8; REPORT_SIZE]> {
        loop {
            self.event.fill(0);
            match self.file.read(&mut self.event) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("read UHID: {error}"),
                Ok(size) => {
                    assert!(size >= 4);
                    let kind = u32::from_ne_bytes(self.event[..4].try_into().unwrap());
                    if kind == OUTPUT {
                        let size = u16::from_ne_bytes(
                            self.event[OUTPUT_SIZE..OUTPUT_SIZE + 2].try_into().unwrap(),
                        );
                        assert_eq!(self.event[OUTPUT_SIZE + 2], OUTPUT_REPORT);
                        // hidraw's unnumbered output carries its leading report ID.
                        assert_eq!(size, 65);
                        assert_eq!(self.event[4], 0);
                        return Some(self.event[5..69].try_into().unwrap());
                    }
                    assert!(LIFECYCLE.contains(&kind), "unexpected UHID event {kind}");
                }
            }
        }
    }
}

fn write(file: &mut File, event: &[u8]) {
    assert_eq!(file.write(event).unwrap(), event.len());
}

/// What a plugged key served before it was removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Served {
    /// Broadcast INIT allocations: one per Session.
    pub(crate) channels: usize,
    /// Complete CTAP requests, each answered by the virtual key.
    pub(crate) requests: usize,
    /// Keepalives sent while the key waited for presence, and otherwise.
    pub(crate) upneeded: usize,
    pub(crate) processing: usize,
    /// The longest a pending request went without a keepalive or its reply.
    pub(crate) silence: Duration,
}

/// A virtual key presented as a FIDO hidraw device through UHID. Its thread
/// allocates a channel for each broadcast INIT, reassembles each CBOR
/// request, hands it to the key, sends keepalives while the key works and
/// then the reply. Removing it destroys the device.
pub(crate) struct Plugged {
    name: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Served>>,
}

impl Plugged {
    /// Creates the device and waits until its hidraw node exists. `name`
    /// tells concurrently plugged keys apart.
    pub(crate) fn insert(key: &Virtual, name: &str) -> Self {
        let mut device = Uhid::create(name, FIDO_PRODUCT, FIDO_DESCRIPTOR);
        let key = key.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::spawn(move || serve(&mut device, &key, &stopped));
        wait(|| node(name), "virtual key node to appear");
        Self {
            name: name.into(),
            stop,
            thread: Some(thread),
        }
    }

    /// Unplugs the key, waits until its node is gone, and says what it served.
    pub(crate) fn remove(mut self) -> Served {
        self.stop.store(true, Ordering::Relaxed);
        let served = self.thread.take().unwrap().join().unwrap();
        wait(|| !node(&self.name), "virtual key node to disappear");
        served
    }
}

impl Drop for Plugged {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wait(mut done: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a hidraw node of the HID device `name` exists.
fn node(name: &str) -> bool {
    let wanted = format!("HID_NAME={name}");
    fs::read_dir("/sys/class/hidraw").unwrap().any(|entry| {
        let entry = entry.unwrap();
        fs::read_to_string(entry.path().join("device/uevent"))
            .is_ok_and(|uevent| uevent.lines().any(|line| line == wanted))
            && fs::symlink_metadata(format!("/dev/{}", entry.file_name().to_str().unwrap())).is_ok()
    })
}

fn serve(device: &mut Uhid, key: &Virtual, stop: &AtomicBool) -> Served {
    let deadline = Instant::now() + LIFETIME;
    let mut served = Served::default();
    let mut allocated = 0x7d00_0000u32;
    let mut session: Option<(u32, hid::Decoder)> = None;
    while !stop.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline, "virtual HID key expired");
        let Some(report) = device.output() else {
            thread::sleep(POLL);
            continue;
        };
        let channel = u32::from_be_bytes(report[..4].try_into().unwrap());
        if report[4] == INIT {
            // td's sessions only ever allocate on the broadcast channel.
            assert_eq!(channel, hid::BROADCAST);
            assert_eq!(report[5..7], [0, 8]);
            allocated += 1;
            served.channels += 1;
            let mut reply = [0; REPORT_SIZE];
            reply[..4].copy_from_slice(&hid::BROADCAST.to_be_bytes());
            reply[4..7].copy_from_slice(&[INIT, 0, 17]);
            reply[7..15].copy_from_slice(&report[7..15]);
            reply[15..19].copy_from_slice(&allocated.to_be_bytes());
            reply[19..24].copy_from_slice(INIT_TAIL);
            device.input(&reply);
            session = Some((allocated, hid::Decoder::cbor(allocated).unwrap()));
            continue;
        }
        let (active, decoder) = session.as_mut().expect("a CTAPHID report before INIT");
        assert_eq!(channel, *active, "a report on another channel");
        if let hid::Event::Complete(request) = decoder.push(&report).unwrap() {
            served.requests += 1;
            let reply = respond(device, key, *active, request.as_ref().to_vec(), &mut served);
            for report in hid::cbor(*active, &reply).unwrap().as_ref() {
                device.input(report);
            }
            *decoder = hid::Decoder::cbor(*active).unwrap();
        }
    }
    served
}

/// The key's reply, with a keepalive each period while it works:
/// UPNEEDED while its scripted presence delay runs, PROCESSING otherwise.
fn respond(
    device: &mut Uhid,
    key: &Virtual,
    channel: u32,
    request: Vec<u8>,
    served: &mut Served,
) -> Vec<u8> {
    let worker = {
        let key = key.clone();
        thread::spawn(move || key.exchange(&request))
    };
    let mut last = Instant::now();
    while !worker.is_finished() {
        if last.elapsed() >= KEEPALIVE_PERIOD {
            let status = if key.touching() {
                served.upneeded += 1;
                STATUS_UPNEEDED
            } else {
                served.processing += 1;
                STATUS_PROCESSING
            };
            let mut report = [0; REPORT_SIZE];
            report[..4].copy_from_slice(&channel.to_be_bytes());
            report[4..8].copy_from_slice(&[KEEPALIVE, 0, 1, status]);
            device.input(&report);
            served.silence = served.silence.max(last.elapsed());
            last = Instant::now();
        }
        thread::sleep(POLL);
    }
    served.silence = served.silence.max(last.elapsed());
    worker.join().unwrap()
}
