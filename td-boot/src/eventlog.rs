//! The firmware's TCG event log as Linux presents it, its replay, and the
//! staged selector update's PCR 4 and PCR 9 prediction
//! (td-install/ENCRYPTION.md "Prediction").
//!
//! Increment 8i revisits two rules here: `predict` takes no input for the
//! Secure Boot configuration extension td makes into PCR 9 after
//! ExitBootServices, and the one-PCR-9-event rule refuses a signed image
//! that carries its initramfs inside, which logs no initrd event.
use crate::sha256::Sha256;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Where Linux presents the log once securityfs is mounted, relative to
/// the root the selector runs in.
pub const LOG_PATH: &str = "sys/kernel/security/tpm0/binary_bios_measurements";
/// A log larger than this is refused. OVMF's is under 64 KiB; a machine
/// with many option ROMs and certificates stays well inside it.
pub const MAX_LOG_BYTES: usize = 1 << 20;
const MAX_EVENTS: usize = 1 << 14;
const MAX_ALGORITHMS: usize = 8;
const MAX_DIGEST_BYTES: usize = 64;
const PCRS: u32 = 24;

const EV_NO_ACTION: u32 = 0x3;
const EV_SEPARATOR: u32 = 0x4;
const EV_EVENT_TAG: u32 = 0x6;
const EV_EFI_BOOT_SERVICES_APPLICATION: u32 = 0x8000_0003;
const EV_EFI_ACTION: u32 = 0x8000_0007;
const TPM_ALG_SHA256: u16 = 0x000b;
const SPEC_ID_SIGNATURE: &[u8] = b"Spec ID Event03\0";
const CALLING: &[u8] = b"Calling EFI Application from Boot Option";
const RETURNING: &[u8] = b"Returning from EFI Application from Boot Option";
/// The EFI stub's initrd measurement: a TCG_PCClientTaggedEvent with the
/// stub's tag (0x8f3b22ec), its data length and `Linux initrd` with its
/// terminator.
const INITRD_EVENT: &[u8] = b"\xec\x22\x3b\x8f\x0d\x00\x00\x00Linux initrd\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Empty,
    Oversized,
    Truncated,
    SpecId(&'static str),
    NoSha256,
    Malformed(&'static str),
    Unreadable,
    Replay(u8),
    Returned,
    Pcr4Event,
    CallingAction,
    Separator,
    NoApplication,
    SecondApplication,
    RunningApplication,
    InitrdEvents,
    InitrdEvent,
    RunningInitrd,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Empty => write!(f, "the TPM event log is empty"),
            Refusal::Oversized => write!(f, "the TPM event log exceeds {MAX_LOG_BYTES} bytes"),
            Refusal::Truncated => write!(f, "the TPM event log ends inside an event"),
            Refusal::SpecId(why) => write!(f, "the TPM event log's Spec ID event: {why}"),
            Refusal::NoSha256 => write!(f, "the TPM event log carries no SHA-256 digests"),
            Refusal::Malformed(why) => write!(f, "the TPM event log is malformed: {why}"),
            Refusal::Unreadable => write!(f, "the TPM event log cannot be read"),
            Refusal::Replay(pcr) => write!(f, "the TPM event log does not replay to PCR {pcr}"),
            Refusal::Returned => write!(
                f,
                "a boot application returned to the firmware before this boot"
            ),
            Refusal::Pcr4Event => write!(f, "PCR 4 records an event no boot repeats"),
            Refusal::CallingAction => write!(f, "PCR 4 does not record exactly one boot attempt"),
            Refusal::Separator => write!(f, "PCR 4 does not record exactly one separator"),
            Refusal::NoApplication => write!(f, "PCR 4 records no boot application"),
            Refusal::SecondApplication => write!(
                f,
                "PCR 4 records a second boot application, a menu, shell or one-time entry"
            ),
            Refusal::RunningApplication => write!(
                f,
                "the measured boot application is not the running BOOTX64.EFI"
            ),
            Refusal::InitrdEvents => write!(
                f,
                "PCR 9 does not record exactly one event, the selector initramfs"
            ),
            Refusal::InitrdEvent => {
                write!(f, "PCR 9's event is not the EFI stub's initrd measurement")
            }
            Refusal::RunningInitrd => {
                write!(f, "the measured initramfs is not the running INITRD")
            }
        }
    }
}

/// One event, with its SHA-256 digest; the data borrows the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event<'a> {
    pub pcr: u32,
    pub kind: u32,
    pub sha256: [u8; 32],
    pub data: &'a [u8],
}

/// A parsed log: the algorithms its Spec ID event lists, with their digest
/// sizes, and every event after it in order, the final-events table's
/// included where Linux appended it.
#[derive(Debug)]
pub struct Log<'a> {
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the tests rebuild logs with its digest sizes")
    )]
    pub algorithms: Vec<(u16, usize)>,
    pub events: Vec<Event<'a>>,
}

/// SHA-256 PCRs 4, 7 and 9: as read, as replayed, or as predicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pcrs {
    pub pcr4: [u8; 32],
    pub pcr7: [u8; 32],
    pub pcr9: [u8; 32],
}

/// The two digests a chain contributes: the boot application's
/// Authenticode SHA-256 and the initramfs's SHA-256.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chain {
    pub application: [u8; 32],
    pub initrd: [u8; 32],
}

/// Read the log at `path`, at most `MAX_LOG_BYTES`. securityfs reports no
/// size, so the bound is on what is read.
pub fn read_log(path: &Path) -> Result<Vec<u8>, Refusal> {
    let file = File::open(path).map_err(|_| Refusal::Unreadable)?;
    let mut log = Vec::new();
    file.take(MAX_LOG_BYTES as u64 + 1)
        .read_to_end(&mut log)
        .map_err(|_| Refusal::Unreadable)?;
    if log.len() > MAX_LOG_BYTES {
        return Err(Refusal::Oversized);
    }
    Ok(log)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Refusal> {
        let end = self.at.checked_add(n).ok_or(Refusal::Truncated)?;
        let out = self.bytes.get(self.at..end).ok_or(Refusal::Truncated)?;
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Refusal> {
        match self.take(1)? {
            &[b] => Ok(b),
            _ => Err(Refusal::Truncated),
        }
    }

    fn u16(&mut self) -> Result<u16, Refusal> {
        match self.take(2)? {
            &[a, b] => Ok(u16::from_le_bytes([a, b])),
            _ => Err(Refusal::Truncated),
        }
    }

    fn u32(&mut self) -> Result<u32, Refusal> {
        match self.take(4)? {
            &[a, b, c, d] => Ok(u32::from_le_bytes([a, b, c, d])),
            _ => Err(Refusal::Truncated),
        }
    }

    fn sized(&mut self) -> Result<&'a [u8], Refusal> {
        let size = usize::try_from(self.u32()?).map_err(|_| Refusal::Truncated)?;
        self.take(size)
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Parse the crypto-agile log: the SHA-1-format Spec ID event, then
/// TCG_PCR_EVENT2 records to the last byte.
pub fn parse(log: &[u8]) -> Result<Log<'_>, Refusal> {
    if log.is_empty() {
        return Err(Refusal::Empty);
    }
    if log.len() > MAX_LOG_BYTES {
        return Err(Refusal::Oversized);
    }
    let mut c = Cursor { bytes: log, at: 0 };
    let pcr = c.u32()?;
    let kind = c.u32()?;
    let digest = c.take(20)?;
    let data = c.sized()?;
    if pcr != 0 || kind != EV_NO_ACTION || digest.iter().any(|b| *b != 0) {
        return Err(Refusal::SpecId("the first event is not it"));
    }
    let algorithms = spec_id(data)?;
    let mut events = Vec::new();
    while !c.done() {
        if events.len() >= MAX_EVENTS {
            return Err(Refusal::Malformed("too many events"));
        }
        let pcr = c.u32()?;
        let kind = c.u32()?;
        let count = usize::try_from(c.u32()?).map_err(|_| Refusal::Truncated)?;
        if count == 0 || count > algorithms.len() {
            return Err(Refusal::Malformed("an event's digest count"));
        }
        let mut seen: Vec<u16> = Vec::with_capacity(count);
        let mut sha256 = None;
        for _ in 0..count {
            let algorithm = c.u16()?;
            let size = algorithms
                .iter()
                .find(|(id, _)| *id == algorithm)
                .map(|(_, size)| *size)
                .ok_or(Refusal::Malformed(
                    "a digest of an algorithm the Spec ID event does not list",
                ))?;
            if seen.contains(&algorithm) {
                return Err(Refusal::Malformed("an event repeats a digest algorithm"));
            }
            seen.push(algorithm);
            let digest = c.take(size)?;
            if algorithm == TPM_ALG_SHA256 {
                sha256 = Some(<[u8; 32]>::try_from(digest).map_err(|_| Refusal::NoSha256)?);
            }
        }
        let sha256 = sha256.ok_or(Refusal::NoSha256)?;
        let data = c.sized()?;
        if pcr >= PCRS {
            return Err(Refusal::Malformed("an event names no PCR"));
        }
        events.push(Event {
            pcr,
            kind,
            sha256,
            data,
        });
    }
    Ok(Log { algorithms, events })
}

/// TCG_EfiSpecIdEvent: the signature, platform class, version, word size,
/// the algorithms with their digest sizes, and vendor data, exactly.
fn spec_id(data: &[u8]) -> Result<Vec<(u16, usize)>, Refusal> {
    let mut c = Cursor { bytes: data, at: 0 };
    fn short<E>(_: E) -> Refusal {
        Refusal::SpecId("it is short")
    }
    if c.take(16).map_err(short)? != SPEC_ID_SIGNATURE {
        return Err(Refusal::SpecId("its signature is not Spec ID Event03"));
    }
    c.u32().map_err(short)?;
    c.u8().map_err(short)?;
    let major = c.u8().map_err(short)?;
    c.u8().map_err(short)?;
    c.u8().map_err(short)?;
    if major != 2 {
        return Err(Refusal::SpecId("its version is not 2"));
    }
    let count = usize::try_from(c.u32().map_err(short)?).map_err(short)?;
    if count == 0 || count > MAX_ALGORITHMS {
        return Err(Refusal::SpecId("its algorithm count"));
    }
    let mut algorithms: Vec<(u16, usize)> = Vec::with_capacity(count);
    for _ in 0..count {
        let id = c.u16().map_err(short)?;
        let size = usize::from(c.u16().map_err(short)?);
        if size == 0 || size > MAX_DIGEST_BYTES || algorithms.iter().any(|(seen, _)| *seen == id) {
            return Err(Refusal::SpecId("an algorithm entry"));
        }
        algorithms.push((id, size));
    }
    let vendor = usize::from(c.u8().map_err(short)?);
    c.take(vendor).map_err(short)?;
    if !c.done() {
        return Err(Refusal::SpecId("it has trailing bytes"));
    }
    match algorithms.iter().find(|(id, _)| *id == TPM_ALG_SHA256) {
        Some((_, 32)) => Ok(algorithms),
        Some(_) => Err(Refusal::SpecId("SHA-256's digest size is not 32")),
        None => Err(Refusal::NoSha256),
    }
}

fn extend(pcr: &mut [u8; 32], digest: &[u8; 32]) {
    let mut hash = Sha256::new();
    hash.update(pcr);
    hash.update(digest);
    *pcr = hash.finalize();
}

/// Replay `log` against the PCRs `read` and predict PCR 4 and PCR 9 with
/// the `staged` chain's digests in place of the `running` chain's; PCR 7
/// keeps its value. Refuses a log whose events may not recur at the next
/// boot, and one that does not replay to what was read.
pub fn predict(log: &[u8], read: &Pcrs, running: &Chain, staged: &Chain) -> Result<Pcrs, Refusal> {
    let log = parse(log)?;
    let zero = Pcrs {
        pcr4: [0; 32],
        pcr7: [0; 32],
        pcr9: [0; 32],
    };
    let mut replay = zero;
    let mut predicted = zero;
    let (mut calling, mut separators) = (0u32, 0u32);
    let mut application = None;
    let mut initrd: Option<&Event<'_>> = None;
    // Counts first, then what the one event of each kind measured, so a
    // second event is refused as one whichever comes first.
    for event in &log.events {
        if event.kind == EV_NO_ACTION {
            continue;
        }
        if event.kind == EV_EFI_ACTION && event.data == RETURNING {
            return Err(Refusal::Returned);
        }
        match event.pcr {
            4 => {
                let next = match event.kind {
                    EV_EFI_ACTION if event.data == CALLING => {
                        calling += 1;
                        event.sha256
                    }
                    EV_SEPARATOR => {
                        separators += 1;
                        event.sha256
                    }
                    EV_EFI_BOOT_SERVICES_APPLICATION => {
                        if application.replace(event.sha256).is_some() {
                            return Err(Refusal::SecondApplication);
                        }
                        staged.application
                    }
                    _ => return Err(Refusal::Pcr4Event),
                };
                extend(&mut replay.pcr4, &event.sha256);
                extend(&mut predicted.pcr4, &next);
            }
            7 => {
                extend(&mut replay.pcr7, &event.sha256);
            }
            9 => {
                if initrd.replace(event).is_some() {
                    return Err(Refusal::InitrdEvents);
                }
                extend(&mut replay.pcr9, &event.sha256);
                extend(&mut predicted.pcr9, &staged.initrd);
            }
            _ => {}
        }
    }
    let application = application.ok_or(Refusal::NoApplication)?;
    if calling != 1 {
        return Err(Refusal::CallingAction);
    }
    if separators != 1 {
        return Err(Refusal::Separator);
    }
    let initrd = initrd.ok_or(Refusal::InitrdEvents)?;
    if initrd.kind != EV_EVENT_TAG || initrd.data != INITRD_EVENT {
        return Err(Refusal::InitrdEvent);
    }
    if application != running.application {
        return Err(Refusal::RunningApplication);
    }
    if initrd.sha256 != running.initrd {
        return Err(Refusal::RunningInitrd);
    }
    for (pcr, replayed, as_read) in [
        (4, replay.pcr4, read.pcr4),
        (7, replay.pcr7, read.pcr7),
        (9, replay.pcr9, read.pcr9),
    ] {
        if replayed != as_read {
            return Err(Refusal::Replay(pcr));
        }
    }
    predicted.pcr7 = read.pcr7;
    Ok(predicted)
}

#[cfg(test)]
#[path = "eventlog_tests.rs"]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests;
