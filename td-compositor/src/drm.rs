//! The DRM/KMS output backend: discovery, and the swap chain `run --card`
//! drives.
//!
//! Discovery decides which connector to believe, which of its modes to want,
//! and which CRTC can drive it. It is kept apart from the chain so the
//! selection is testable against recorded connector shapes instead of against
//! a card. `open_kms` then takes DRM mastership for the compositor's life,
//! modesets, and hands back a `SwapChain` of two dumb buffers and the reader
//! for its page-flip completions.
//!
//! The kernel ABI lives in `sys.rs` with the rest of it. What is here is
//! policy.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use crate::output::{
    Damage, Fourcc, FrameId, FrameTarget, FrameView, Output, OutputBackend, OutputDimensions,
    OutputId, OutputScale, OutputTransform, Submission, DRM_FORMAT_XRGB8888,
};
use crate::sys;

/// `O_CLOEXEC`. The x86-64 value, as `sys.rs` says of its own flag words: a
/// card descriptor must not survive into a client or a jailed application,
/// which is the one thing an inherited DRM fd would hand them.
const O_CLOEXEC: i32 = 0o2_000_000;

/// `DRM_MODE_CONNECTOR_*`, for reporting only. A connector's type does not
/// change what td does with it — a virtual sink and an HDMI one are scanned out
/// the same way — but an unreadable proof line is a proof nobody checks, and
/// "Virtual-1" is what makes the virtio-gpu case recognisable at a glance.
const CONNECTOR_TYPES: [(u32, &str); 21] = [
    (0, "Unknown"),
    (1, "VGA"),
    (2, "DVI-I"),
    (3, "DVI-D"),
    (4, "DVI-A"),
    (5, "Composite"),
    (6, "SVIDEO"),
    (7, "LVDS"),
    (8, "Component"),
    (9, "DIN"),
    (10, "DisplayPort"),
    (11, "HDMI-A"),
    (12, "HDMI-B"),
    (13, "TV"),
    (14, "eDP"),
    (15, "Virtual"),
    (16, "DSI"),
    (17, "DPI"),
    (18, "Writeback"),
    (19, "SPI"),
    (20, "USB"),
];

/// How much the kernel is willing to say about a connector's sink.
///
/// Ordered deliberately, and the declaration order IS the preference: the
/// kernel's own advice in `enum drm_connector_status` is to light an `unknown`
/// connector only when nothing reports `connected`. `Unknown` does not mean
/// absent — probing would have flickered, or a resource was busy — so it is a
/// fallback rather than a rejection.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Confidence {
    Unknown,
    Connected,
}

/// A connector that can be scanned out, and everything needed to do it.
#[derive(Clone, Copy)]
pub struct Scanout {
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub encoder_id: u32,
    pub crtc_id: u32,
    pub mode: sys::DrmModeInfo,
    pub mm_width: u32,
    pub mm_height: u32,
}

impl Scanout {
    /// The connector's name as the kernel's own tools spell it: type and the
    /// per-type number, e.g. `Virtual-1`.
    pub fn connector_name(&self) -> String {
        let kind = CONNECTOR_TYPES
            .iter()
            .find(|(number, _)| *number == self.connector_type)
            .map(|(_, name)| *name)
            .unwrap_or("Unknown");
        format!("{kind}-{}", self.connector_type_id)
    }

    /// This scanout as an `Output`.
    ///
    /// Scale one and no transform, and both are ASSERTED rather than read:
    /// a DRM connector carries a rotation property and a physical size this
    /// could compute a scale from, and consuming either needs the renderer
    /// §M row 6 says does not exist yet. Reporting a transform td does not
    /// apply would tell clients the picture is turned when it is not.
    pub fn output(&self) -> Result<Output, String> {
        let width = usize::from(self.mode.hdisplay);
        let height = usize::from(self.mode.vdisplay);
        if width == 0 || height == 0 {
            return Err(format!(
                "connector {} offers mode '{}' with a zero {} — nothing can be scanned out at that size",
                self.connector_name(),
                self.mode.name(),
                if width == 0 { "width" } else { "height" }
            ));
        }
        Ok(Output {
            id: OutputId::FIRST,
            dimensions: OutputDimensions { width, height },
            scale: OutputScale::ONE,
            transform: OutputTransform::Normal,
        })
    }
}

/// What one card answered.
pub struct Discovery {
    pub driver: String,
    pub scanout: Scanout,
}

impl Discovery {
    /// One line, for a proof to match and a person to read.
    pub fn describe(&self) -> String {
        let mode = self.scanout.mode;
        format!(
            "driver={} connector={}#{} status={} crtc={} encoder={} mode={}x{}@{} name={} \
             preferred={} mm={}x{}",
            self.driver,
            self.scanout.connector_name(),
            self.scanout.connector_id,
            match self.scanout.connection {
                sys::DRM_MODE_CONNECTED => "connected",
                sys::DRM_MODE_UNKNOWNCONNECTION => "unknown",
                _ => "other",
            },
            self.scanout.crtc_id,
            self.scanout.encoder_id,
            mode.hdisplay,
            mode.vdisplay,
            mode.vrefresh,
            mode.name(),
            mode.is_preferred(),
            self.scanout.mm_width,
            self.scanout.mm_height,
        )
    }
}

/// Is this CRTC actually showing what was asked for?
///
/// Split out of `CardCrtc::set` so the three refusals are testable against
/// recorded CRTC states rather than against a card. Each is a different failure with a
/// different cause, which is why they are three checks and not one equality:
/// a dark CRTC means the modeset did not take, a different framebuffer means
/// the driver kept the old picture and answered success anyway, and a
/// different size means the kernel validated the request into some other mode.
fn crtc_shows(
    live: &sys::DrmModeCrtc,
    crtc_id: u32,
    fb_id: u32,
    wanted: &sys::DrmModeInfo,
) -> Result<(), String> {
    if live.mode_valid == 0 {
        return Err(format!(
            "CRTC {crtc_id} reports no valid mode after the modeset was accepted"
        ));
    }
    if live.fb_id != fb_id {
        return Err(format!(
            "CRTC {crtc_id} is scanning out framebuffer {} rather than the {fb_id} just set",
            live.fb_id
        ));
    }
    if live.mode.hdisplay != wanted.hdisplay || live.mode.vdisplay != wanted.vdisplay {
        return Err(format!(
            "CRTC {crtc_id} settled on {}x{} rather than the {}x{} asked for",
            live.mode.hdisplay, live.mode.vdisplay, wanted.hdisplay, wanted.vdisplay
        ));
    }
    Ok(())
}

/// Does a driver's reported size actually cover the scanout it is for?
///
/// The kernel picks the PITCH, so the size that matters is `pitch * height`,
/// not `width * 4 * height` — which is what a caller would have assumed and
/// what would be too small on any driver that pads a scanline. Two separate
/// failures are named apart because they are different bugs: a pitch too narrow
/// for the width means the driver and this code disagree about the format,
/// while a size below `pitch * height` means the buffer is short of its own
/// stride. Either one, trusted, is a write past the end of the mapping.
fn buffer_covers_scanout(pitch: u32, width: u32, height: u32, size: u64) -> Result<(), String> {
    let row = u64::from(width).saturating_mul(4);
    if u64::from(pitch) < row {
        return Err(format!(
            "dumb buffer: the driver reported pitch {pitch} for a {width}-pixel row, which \
             needs {row} bytes at four bytes per pixel"
        ));
    }
    // `u32 * u32` always fits a `u64`, so this is exact at every input and the
    // saturation is unreachable. Said rather than guarded: an earlier revision
    // used `checked_mul` and returned an "overflows" error, which no test could
    // reach and which therefore claimed a check that was not one.
    let needed = u64::from(pitch).saturating_mul(u64::from(height));
    if size < needed {
        return Err(format!(
            "dumb buffer: the driver reported {size} bytes for {width}x{height} at pitch \
             {pitch}, which needs {needed}"
        ));
    }
    Ok(())
}

/// Can this saved state be sent back as-is?
///
/// Split out of `restorable` so the shapes the kernel refuses are testable
/// against recorded CRTC states rather than against a card, for the reason
/// `crtc_shows` is split out. All three have to hold at once: a mode to set, a
/// framebuffer to set it onto, and at least one connector to send it to. Any
/// one missing and `SETCRTC` answers `-EINVAL` or `-ENOENT` rather than
/// restoring anything.
fn is_restorable(saved: &sys::DrmModeCrtc, connectors: &[u32]) -> bool {
    saved.mode_valid != 0 && saved.fb_id != 0 && !connectors.is_empty()
}

/// A saved CRTC state as a request `SETCRTC` will accept: itself when it is
/// restorable, and otherwise "switch the CRTC off", the one request that is
/// always well-formed.
///
/// The saved state and a well-formed request are not the same thing, and the
/// gap is reachable. `drm_mode_getcrtc` fills `mode_valid` from
/// `crtc->state->enable` and `fb_id` from the primary plane INDEPENDENTLY
/// (`drm_crtc.c:577`, `:562`), so an enabled CRTC with no primary framebuffer
/// reads back as `mode_valid = 1, fb_id = 0`. Replaying that asks the kernel
/// to look up framebuffer 0, which answers `-ENOENT` (`:773`). Two more shapes
/// are refused outright: a mode with no connectors (`:824`) and connectors
/// with no mode or no framebuffer (`:830`). Switching off is a worse restore
/// than the real one and a better one than a request sent and silently failed.
fn restorable(saved: sys::DrmModeCrtc, connectors: Vec<u32>) -> (sys::DrmModeCrtc, Vec<u32>) {
    if is_restorable(&saved, &connectors) {
        return (saved, connectors);
    }
    let mut off = saved;
    off.mode_valid = 0;
    off.fb_id = 0;
    (off, Vec::new())
}

/// Which connectors the kernel is currently routing to `crtc_id`.
///
/// The long way round, because there is no short one: no ioctl reports a
/// CRTC's connector set. What exists is the reverse mapping — each connector
/// names the encoder it is bound to, and each encoder names its CRTC — so this
/// walks every connector and keeps the ones that lead back here.
///
/// Read UNDER mastership and immediately before the modeset, which is the only
/// time the answer is the one that will need restoring. Individual failures
/// are skipped rather than propagated, for `select_scanout`'s reason: a
/// connector can vanish between being listed and being read, and one that did
/// is one this CRTC is certainly not scanning out.
fn connectors_on_crtc(card: &File, crtc_id: u32) -> Result<Vec<u32>, String> {
    let listed = sys::drm_resources(card)?;
    let mut routed = Vec::with_capacity(listed.connectors.len());
    for id in &listed.connectors {
        let Ok(connector) = sys::drm_connector(card, *id) else {
            continue;
        };
        if connector.encoder_id == 0 {
            continue;
        }
        let Ok(encoder) = sys::drm_encoder(card, connector.encoder_id) else {
            continue;
        };
        if encoder.crtc_id == crtc_id {
            routed.push(connector.id);
        }
    }
    Ok(routed)
}

/// Open a card node and immediately give back the authority opening it took.
///
/// Read-write because a DRM node is: the mode-setting requests are writes to
/// the device, and opening read-only would defer the failure to the modeset
/// rather than report it at the door.
///
/// `drm_master_open` makes the first opener of a primary node the DRM master
/// whenever `dev->master` is NULL, and fbcon — an in-kernel client — never
/// sets it. So the plain `open` IS an acquisition, one nobody asked for.
/// Giving it back here keeps the one place this module TAKES mastership the
/// explicit `SET_MASTER` in `open_kms`, two syscalls later; re-taking it is
/// permitted to a non-root opener because its descriptor was master at open.
fn open_card(path: &Path) -> Result<File, String> {
    let card = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("open DRM device {}: {error}", path.display()))?;
    sys::drm_drop_master(&card)
        .map_err(|error| format!("release DRM mastership on {}: {error}", path.display()))?;
    Ok(card)
}

/// Ask a card what it is and what it can scan out.
/// Every failure after the first question carries the driver's answer to it.
/// "no connector can be scanned out" and "no connector can be scanned out ON
/// `virtio_gpu`" are different reports, and the second is the one that says
/// whether the right node was even opened — a render node answers its name
/// happily and then refuses every modeset request with `EACCES`, which is the
/// most likely way this is pointed at the wrong file.
fn discover(card: &File) -> Result<Discovery, String> {
    let driver = sys::drm_driver_name(card)?;
    let resources =
        sys::drm_resources(card).map_err(|error| format!("{error} (driver {driver})"))?;
    let scanout =
        select_scanout(card, &resources).map_err(|error| format!("{error} (driver {driver})"))?;
    Ok(Discovery { driver, scanout })
}

/// Pick the connector to drive.
///
/// One pass, keeping the best candidate rather than the first: a card with a
/// disconnected HDMI listed before a connected Virtual would otherwise be
/// driven by whichever the kernel happened to enumerate first. Ties go to the
/// earlier connector, which is the only stable answer available — connector
/// order is the kernel's, and nothing here should invent a preference between
/// two equally connected sinks.
fn select_scanout(card: &File, resources: &sys::DrmResources) -> Result<Scanout, String> {
    if resources.crtcs.is_empty() {
        return Err(
            "the DRM device reports no CRTCs, so it has nothing that could scan a frame out"
                .to_string(),
        );
    }
    let mut best: Option<(Confidence, Scanout)> = None;
    let mut disconnected = 0usize;
    let mut without_modes = 0usize;
    let mut without_crtc = 0usize;
    let mut unrecognised: Vec<u32> = Vec::new();
    let mut vanished = 0usize;

    for connector_id in &resources.connectors {
        // An id from GETRESOURCES is not durable: the two ioctls are not one
        // atomic view, so a connector can be unplugged, or not yet fully
        // registered, between them, and `drm_connector_lookup` then answers
        // ENOENT. Tallied and skipped rather than propagated -- an earlier
        // revision used `?` here, so one connector vanishing mid-scan failed
        // the whole boot even when a second was sitting there driveable.
        let Ok(connector) = sys::drm_connector(card, *connector_id) else {
            vanished += 1;
            continue;
        };
        let confidence = match connector.connection {
            sys::DRM_MODE_CONNECTED => Confidence::Connected,
            sys::DRM_MODE_UNKNOWNCONNECTION => Confidence::Unknown,
            sys::DRM_MODE_DISCONNECTED => {
                disconnected += 1;
                continue;
            }
            // Not one of the three the kernel defines. Counted apart from the
            // disconnected ones rather than folded in with them: it means this
            // build's idea of `enum drm_connector_status` and the running
            // kernel's have diverged, which is a different problem from a dark
            // screen and would be invisible inside that tally.
            other => {
                unrecognised.push(other);
                continue;
            }
        };
        let Some(mode) = preferred_mode(&connector.modes) else {
            without_modes += 1;
            continue;
        };
        let Some((encoder_id, crtc_id)) = crtc_for(card, &connector, resources) else {
            without_crtc += 1;
            continue;
        };
        let candidate = Scanout {
            connector_id: connector.id,
            connector_type: connector.connector_type,
            connector_type_id: connector.connector_type_id,
            connection: connector.connection,
            encoder_id,
            crtc_id,
            mode,
            mm_width: connector.mm_width,
            mm_height: connector.mm_height,
        };
        if best.as_ref().is_none_or(|(seen, _)| confidence > *seen) {
            best = Some((confidence, candidate));
        }
    }

    best.map(|(_, scanout)| scanout).ok_or_else(|| {
        format!(
            "no connector on this DRM device can be scanned out: {} connector(s) examined, \
             {disconnected} disconnected, {without_modes} with no mode, {without_crtc} with no \
             reachable CRTC, {vanished} that disappeared between being listed and being \
             read, and status values this build does not know: {unrecognised:?}",
            resources.connectors.len()
        )
    })
}

/// The mode to ask for: the one the driver marked preferred, else the first.
///
/// The fallback is not arbitrary. The kernel returns a connector's modes
/// already sorted best-first, so `first` is its own recommendation for a
/// connector that marked nothing — which is what a virtual sink does when the
/// host window has never been resized.
fn preferred_mode(modes: &[sys::DrmModeInfo]) -> Option<sys::DrmModeInfo> {
    modes
        .iter()
        .find(|mode| mode.is_preferred())
        .or_else(|| modes.first())
        .copied()
}

/// The encoder and CRTC that can drive this connector, if any can.
///
/// The connector's CURRENT encoder and its current CRTC are tried first, and
/// that ordering is the whole of how this stays polite to what is already on
/// screen: on a td image `fbcon` has already lit this connector through some
/// CRTC, and choosing the same one means the backend that follows reuses that
/// configuration instead of moving the picture to a different pipe.
fn crtc_for(
    card: &File,
    connector: &sys::DrmConnector,
    resources: &sys::DrmResources,
) -> Option<(u32, u32)> {
    // An encoder id races the same way a connector id does, so an unreadable
    // one means "not this encoder" rather than "give up on this connector".
    // The answer is an Option and not a Result for that reason: there is no
    // error here that is not simply the absence of a reachable CRTC.
    if connector.encoder_id != 0 {
        if let Ok(encoder) = sys::drm_encoder(card, connector.encoder_id) {
            if encoder.crtc_id != 0 && resources.crtcs.contains(&encoder.crtc_id) {
                return Some((encoder.id, encoder.crtc_id));
            }
            if let Some(crtc) = first_possible_crtc(&encoder, resources) {
                return Some((encoder.id, crtc));
            }
        }
    }
    for encoder_id in &connector.encoders {
        // The connector's current encoder is tried above and may appear in this
        // list too; re-reading it is one ioctl and keeps the fallback a plain
        // walk of what the connector says can drive it.
        let Ok(encoder) = sys::drm_encoder(card, *encoder_id) else {
            continue;
        };
        if let Some(crtc) = first_possible_crtc(&encoder, resources) {
            return Some((encoder.id, crtc));
        }
    }
    None
}

/// The first CRTC this encoder can reach.
///
/// `possible_crtcs` is a bitmask over INDEXES INTO the resources' CRTC list,
/// not over CRTC ids. Reading it as ids is the classic way to modeset onto a
/// pipe the encoder cannot drive, and it is silent: ids are small integers
/// too, so the wrong answer is a plausible one.
fn first_possible_crtc(encoder: &sys::DrmEncoder, resources: &sys::DrmResources) -> Option<u32> {
    resources
        .crtcs
        .iter()
        .enumerate()
        .find_map(|(index, crtc)| {
            let bit = u32::try_from(index).ok()?;
            // A card may list more CRTCs than the mask has bits. Those are not
            // addressable through this encoder by construction, and shifting
            // by 32 is undefined rather than merely false.
            if bit >= u32::BITS {
                return None;
            }
            (encoder.possible_crtcs & (1u32 << bit) != 0).then_some(*crtc)
        })
}

/// The card, shared by the CRTC the backend drives and every buffer it
/// registers.
///
/// Owned rather than borrowed: `Runtime` holds its output for the
/// compositor's life, so nothing it holds can borrow a descriptor from a
/// caller's stack frame.
/// `Arc` rather than `Rc` because the runtime crosses threads behind its lock.
type Card = Arc<File>;

/// The one format the backend registers its buffers in: `drm_add_fb` names
/// XRGB8888 and nothing else.
const KMS_FORMATS: [Fourcc; 1] = [DRM_FORMAT_XRGB8888];

/// One dumb buffer's GEM handle and, once registered, its framebuffer id,
/// released framebuffer first and handle second.
///
/// One guard rather than two because the two ids are only ever released as a
/// pair, and a framebuffer registered on a handle that was already destroyed
/// is the order worth never writing.
struct Registration {
    card: Card,
    handle: u32,
    /// Zero until `ADDFB2` answers. Zero is never a registered id --
    /// `drm_add_fb` refuses one -- so it doubles as "not registered yet".
    fb_id: u32,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.fb_id != 0 {
            let _ = sys::drm_rm_fb(&*self.card, self.fb_id);
        }
        let _ = sys::drm_destroy_dumb(&*self.card, self.handle);
    }
}

/// One scanout buffer the backend owns outright: a dumb buffer, its mapping,
/// and its framebuffer registration.
///
/// Released in the reverse of acquisition, by field order: `region` is
/// declared first, so it unmaps before the registration unregisters and
/// frees. There is deliberately no `Drop` of this type's own, because a
/// type's destructor runs BEFORE its fields drop and would invert that. The
/// kernel tolerates either order -- a GEM mapping holds its own reference --
/// so this is hygiene rather than a requirement.
pub struct ScanoutBuffer {
    region: sys::MappedRegion,
    registration: Registration,
}

impl ScanoutBuffer {
    /// Allocate, map and register one buffer at the mode's size, answering
    /// the kernel's pitch beside it.
    ///
    /// The registration guard exists before anything after the allocation
    /// can fail, so every `?` below releases what was taken: the locals drop
    /// in reverse, the mapping first.
    fn allocate(card: &Card, width: u32, height: u32) -> Result<(ScanoutBuffer, u32), String> {
        let buffer = sys::drm_create_dumb(&**card, width, height)?;
        let mut registration = Registration {
            card: Arc::clone(card),
            handle: buffer.handle,
            fb_id: 0,
        };
        buffer_covers_scanout(buffer.pitch, width, height, buffer.size)?;
        let region = sys::drm_map_dumb(&**card, &buffer)?;
        registration.fb_id = sys::drm_add_fb(&**card, width, height, buffer.pitch, buffer.handle)?;
        Ok((
            ScanoutBuffer {
                region,
                registration,
            },
            buffer.pitch,
        ))
    }
}

/// What a swap chain writes a frame into and names to its CRTC.
///
/// A trait so the chain's bookkeeping -- which buffer is on glass, which rows
/// a copy owes, what a completion means -- is tested against memory a test
/// owns rather than only against a card.
pub trait ScanoutMemory {
    fn bytes_mut(&mut self) -> &mut [u8];
    fn fb_id(&self) -> u32;
}

impl ScanoutMemory for ScanoutBuffer {
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.region.bytes_mut()
    }

    fn fb_id(&self) -> u32 {
        self.registration.fb_id
    }
}

/// What a swap chain asks of the CRTC it drives, for `ScanoutMemory`'s
/// reason.
pub trait ScanoutCrtc {
    /// Queue `fb_id` for the next vblank, tagged `cookie`.
    fn flip(&mut self, fb_id: u32, cookie: u64) -> Result<(), String>;
    /// Put `fb_id` on the CRTC now, by modeset, and confirm it took.
    fn set(&mut self, fb_id: u32) -> Result<(), String>;
}

/// The CRTC the backend drives, mastership over it, and the state to put back.
///
/// Its `Drop` restores the saved state and then gives mastership back, in that
/// order because the restore is a `SETCRTC`, the one step the kernel flags
/// `DRM_MASTER` (`drm_ioctl.c:675`). It is declared FIRST in
/// `SwapChain`, so it runs before any buffer is unregistered -- unregistering
/// a framebuffer the CRTC still scans out blanks the CRTC, which the restore
/// would then have to light again.
///
/// It runs when a start fails once the CRTC state is saved -- in `open_kms`,
/// or in `run_compositor` before the completion threads start -- and never
/// with a flip queued. A running compositor never drops its backend: the
/// completion threads hold the runtime until the process ends. At exit the
/// kernel releases mastership with the descriptors, and `drm_lastclose`
/// restores the fbdev client once the last one closes.
pub struct CardCrtc {
    card: Card,
    crtc_id: u32,
    connector_id: u32,
    mode: sys::DrmModeInfo,
    saved: sys::DrmModeCrtc,
    routed: Vec<u32>,
}

impl ScanoutCrtc for CardCrtc {
    fn flip(&mut self, fb_id: u32, cookie: u64) -> Result<(), String> {
        sys::drm_page_flip(&*self.card, self.crtc_id, fb_id, cookie)
    }

    /// Read back: `SETCRTC` answering success says the request was accepted,
    /// not that this framebuffer is the one the CRTC is committed to. The
    /// kernel may validate the request into another mode, and a driver that
    /// kept the previous framebuffer would still answer success.
    fn set(&mut self, fb_id: u32) -> Result<(), String> {
        let mut wanted = sys::DrmModeCrtc::empty();
        wanted.crtc_id = self.crtc_id;
        wanted.fb_id = fb_id;
        wanted.mode_valid = 1;
        wanted.mode = self.mode;
        let mut connectors = [self.connector_id];
        sys::drm_set_crtc(&*self.card, &wanted, &mut connectors)?;
        let live = sys::drm_get_crtc(&*self.card, self.crtc_id)?;
        crtc_shows(&live, self.crtc_id, fb_id, &self.mode)
    }
}

impl Drop for CardCrtc {
    fn drop(&mut self) {
        // Reported rather than swallowed: a failed restore leaves a screen in a
        // state nothing intended. On stderr, because stdout carries the boot
        // markers the image's check reads whole-line.
        if let Err(error) = sys::drm_set_crtc(&*self.card, &self.saved, &mut self.routed) {
            let _ = writeln!(
                std::io::stderr(),
                "td-compositor: restoring CRTC {} failed: {error}",
                self.crtc_id
            );
        }
        let _ = sys::drm_drop_master(&*self.card);
    }
}

/// One of the chain's two buffers and what it is known to hold.
struct Slot<M> {
    memory: M,
    /// A copy of `memory`'s first `stride * height` bytes, kept so the rows a
    /// frame changes are found by comparing ordinary memory rather than by
    /// reading back through a scanout mapping, which is write-combined on
    /// real hardware and slow to read.
    shadow: Vec<u8>,
    /// Copy every row into this buffer next time, not only those that differ
    /// from `shadow`. Set when the shadow is not known to describe the
    /// memory, and by a caller's `Damage::Whole`.
    stale: bool,
}

/// Two scanout buffers alternating on one CRTC: the KMS output backend.
///
/// One buffer is FRONT -- on glass, or about to be replaced by a queued flip
/// -- and the other is written and flipped to. At most one flip is in flight.
/// The runtime keeps it that way by owing a paint rather than taking one
/// while a flip is queued; `present` refuses a second flip rather than
/// trusting that, since a CRTC with a flip pending answers `-EBUSY` to
/// another.
///
/// Rendering goes into `frame`, ordinary memory, and only the rows that
/// differ from what the back buffer already holds are copied into it. The
/// back buffer is two frames old rather than one, which is why each buffer
/// keeps its own shadow instead of the chain keeping one.
pub struct SwapChain<C, M> {
    /// Declared first so its teardown runs first: see `CardCrtc`.
    crtc: C,
    buffers: [Slot<M>; 2],
    /// Which of `buffers` is front: 0 or 1.
    front: usize,
    /// The frame a flip is in flight for, and the buffer it flips to.
    queued: Option<(FrameId, usize)>,
    /// Whether the front buffer is known to be on glass. False before the
    /// first modeset answers and across a failed recovery.
    shown: bool,
    next: FrameId,
    frame: Vec<u8>,
    /// The caller said `Damage::Whole`: flip even if nothing changed.
    force: bool,
    output: Output,
    stride: usize,
}

/// The KMS backend as `run --card` builds it.
pub type Kms = SwapChain<CardCrtc, ScanoutBuffer>;

/// The first frame a swap chain queues. Not `FrameId::FIRST`: 1 is what a
/// zeroed or defaulted `user_data` is likeliest to hold, and a kernel echoing
/// a constant would then have its first completion accepted.
pub const FIRST_FLIP: FrameId = FrameId::FIRST.next();

impl<C: ScanoutCrtc, M: ScanoutMemory> SwapChain<C, M> {
    /// Build the chain and modeset onto a blank buffer 0.
    ///
    /// The modeset is issued on the ASSEMBLED chain rather than before it, so
    /// a modeset that fails unwinds in field order: the CRTC's restore before
    /// any buffer is unregistered.
    fn new(crtc: C, memory: [M; 2], output: Output, stride: usize) -> Result<Self, String> {
        let size = crate::framebuffer::validate_geometry(
            output.dimensions.width,
            output.dimensions.height,
            stride,
        )?;
        let [mut first, mut second] = memory;
        for (index, buffer) in [&mut first, &mut second].into_iter().enumerate() {
            let length = buffer.bytes_mut().len();
            if length < size {
                return Err(format!(
                    "scanout buffer {index} maps {length} bytes, short of the {size} the frame needs"
                ));
            }
        }
        // Buffer 0 is what the modeset shows, so it is blanked here and its
        // shadow says so. Buffer 1 holds whatever the allocator left, so its
        // first copy is a whole one.
        first
            .bytes_mut()
            .get_mut(..size)
            .ok_or("scanout buffer 0 is shorter than the frame")?
            .fill(0);
        let mut chain = SwapChain {
            crtc,
            buffers: [
                Slot {
                    memory: first,
                    shadow: zeroed(size)?,
                    stale: false,
                },
                Slot {
                    memory: second,
                    shadow: zeroed(size)?,
                    stale: true,
                },
            ],
            front: 0,
            queued: None,
            shown: false,
            next: FIRST_FLIP,
            frame: zeroed(size)?,
            force: false,
            output,
            stride,
        };
        let fb_id = chain.fb_id(0)?;
        chain.crtc.set(fb_id)?;
        chain.shown = true;
        Ok(chain)
    }

    fn fb_id(&self, index: usize) -> Result<u32, String> {
        self.buffers
            .get(index)
            .map(|slot| slot.memory.fb_id())
            .ok_or_else(|| format!("the swap chain has no buffer {index}"))
    }

    /// The buffer `frame` was queued to, taken out of flight. Anything but
    /// the queued frame is refused: a completion this chain cannot place is
    /// not one it may act on.
    fn queued_buffer(&self, frame: FrameId) -> Result<usize, String> {
        match self.queued {
            Some((queued, back)) if queued == frame => Ok(back),
            Some((queued, _)) => Err(format!(
                "frame {:#x} is not the frame in flight, {:#x}",
                frame.cookie(),
                queued.cookie()
            )),
            None => Err(format!(
                "frame {:#x} is not in flight: nothing is",
                frame.cookie()
            )),
        }
    }
}

impl<C: ScanoutCrtc + 'static, M: ScanoutMemory + 'static> OutputBackend for SwapChain<C, M> {
    fn output(&self) -> Output {
        self.output
    }

    fn supported_formats(&self) -> &[Fourcc] {
        &KMS_FORMATS
    }

    /// The kernel's pitch for the dumb buffers, which the frame is rendered
    /// at so a band copies row for row.
    fn target_stride(&self) -> usize {
        self.stride
    }

    /// The front buffer's shadow, and only while nothing is queued: once a
    /// flip is, the glass holds the front buffer until a vblank and the back
    /// one after it, and which of the two is not known until the completion
    /// says so.
    fn completed(&self) -> Option<FrameView<'_>> {
        if !self.shown || self.queued.is_some() {
            return None;
        }
        let front = self.buffers.get(self.front)?;
        Some(FrameView {
            pixels: &front.shadow,
            width: self.output.dimensions.width,
            height: self.output.dimensions.height,
            stride: self.stride,
        })
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// `Whole` means what fbdev's shadow distrust means: repaint everything.
    /// Nothing but this process writes these buffers, so it is a repair
    /// rather than a correction. It is honoured by marking both buffers for a
    /// whole copy, each made when that buffer is next written, and by
    /// flipping even when nothing changed.
    fn begin_frame(&mut self, damage: Damage) -> Result<FrameTarget<'_>, String> {
        if matches!(damage, Damage::Whole) {
            self.force = true;
            for slot in &mut self.buffers {
                slot.stale = true;
            }
        }
        Ok(FrameTarget {
            pixels: &mut self.frame,
            width: self.output.dimensions.width,
            height: self.output.dimensions.height,
            stride: self.stride,
        })
    }

    /// Copy the rows the back buffer lacks and queue it.
    ///
    /// A frame identical to the one already on glass is `Presented` with no
    /// flip: queuing it would only wait a vblank to show nothing new.
    fn present(&mut self) -> Result<Submission, String> {
        if let Some((queued, _)) = self.queued {
            return Err(format!(
                "frame {:#x} is still queued, and a CRTC takes one flip at a time",
                queued.cookie()
            ));
        }
        // Read, not taken: a flip the kernel refuses keeps the repair owed.
        let force = self.force;
        let unchanged = self
            .buffers
            .get(self.front)
            .is_some_and(|front| front.shadow == self.frame);
        if !force && self.shown && unchanged {
            return Ok(Submission::Presented);
        }
        let back = self.front ^ 1;
        let stride = self.stride;
        let slot = self
            .buffers
            .get_mut(back)
            .ok_or("the swap chain has no back buffer")?;
        let rows = self.frame.len().checked_div(stride).unwrap_or(0);
        let band = if slot.stale {
            rows.checked_sub(1).map(|last| (0, last))
        } else {
            crate::framebuffer::damaged_rows(&slot.shadow, &self.frame, stride)
        };
        if let Some((first, last)) = band {
            let start = first
                .checked_mul(stride)
                .ok_or("scanout damage offset overflow")?;
            let end = last
                .checked_add(1)
                .and_then(|rows| rows.checked_mul(stride))
                .ok_or("scanout damage extent overflow")?;
            let rows = self
                .frame
                .get(start..end)
                .ok_or_else(|| format!("scanout damage {start}..{end} is outside the frame"))?;
            // Pessimistic across the copy, as fbdev is across its write: an
            // error between the two halves leaves the shadow describing
            // nothing, and the next copy is then a whole one.
            slot.stale = true;
            slot.memory
                .bytes_mut()
                .get_mut(start..end)
                .ok_or_else(|| format!("scanout damage {start}..{end} is outside the buffer"))?
                .copy_from_slice(rows);
            slot.shadow
                .get_mut(start..end)
                .ok_or_else(|| format!("scanout damage {start}..{end} is outside the shadow"))?
                .copy_from_slice(rows);
        }
        slot.stale = false;
        let frame = self.next;
        let fb_id = slot.memory.fb_id();
        self.crtc.flip(fb_id, frame.cookie())?;
        self.force = false;
        self.next = frame.next();
        self.queued = Some((frame, back));
        Ok(Submission::Queued(frame))
    }

    fn frame_presented(&mut self, frame: FrameId) -> Result<(), String> {
        let back = self.queued_buffer(frame)?;
        self.queued = None;
        self.front = back;
        self.shown = true;
        Ok(())
    }

    /// Modeset onto the buffer the stalled flip was for. Not known to be on
    /// glass until the modeset answers, so `completed` answers nothing across
    /// it; a failed one leaves the frame queued for the caller to try again
    /// or give up on.
    fn recover_stalled_frame(&mut self, frame: FrameId) -> Result<(), String> {
        let back = self.queued_buffer(frame)?;
        let fb_id = self.fb_id(back)?;
        self.shown = false;
        self.crtc.set(fb_id)?;
        self.queued = None;
        self.front = back;
        self.shown = true;
        Ok(())
    }
}

/// A zeroed buffer of `size` bytes, or the reason there is none. Not `vec!`,
/// which aborts the process on an allocation the kernel refused.
fn zeroed(size: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|error| format!("allocate a {size}-byte scanout frame: {error}"))?;
    bytes.resize(size, 0);
    Ok(bytes)
}

/// The card's page-flip completions, read on a thread of their own.
///
/// A second descriptor for the same open card -- `try_clone` is `dup`, so it
/// shares the open file description and with it the event queue -- which is
/// what lets a thread block in `read` on it while the runtime, behind its
/// lock, holds the backend. Blocking and unbounded, deliberately: an idle
/// screen queues no flip and should wait forever. The bound on a flip that
/// was queued and never answered is the runtime's watchdog, which does not
/// need this thread to wake.
///
/// Sharing the description means sharing its status flags. Nothing may make
/// the backend's card non-blocking: this read would answer `WouldBlock`,
/// which it reports as a failure.
pub struct FlipEvents {
    card: File,
    crtc_id: u32,
    pending: Vec<u8>,
    /// A page, because the kernel's documentation recommends one: `drm_read`
    /// never splits an event, and puts back one that does not fit and answers
    /// zero (`drm_file.c:581`), so a smaller buffer takes on a
    /// forward-progress contract this reader has no reason to accept.
    chunk: Vec<u8>,
}

impl FlipEvents {
    /// Block until the next page-flip completion and name its frame.
    pub fn next(&mut self) -> Result<FrameId, String> {
        loop {
            if let Some(completion) = take_completion(&mut self.pending) {
                // The one CRTC this backend drives is the only one it flips,
                // so a completion naming another is not a stale event but a
                // card this code does not understand.
                if completion.crtc_id != self.crtc_id {
                    return Err(format!(
                        "a page-flip completion named CRTC {} rather than the {} this backend \
                         drives",
                        completion.crtc_id, self.crtc_id
                    ));
                }
                return Ok(FrameId::from_cookie(completion.user_data));
            }
            // Everything a whole read delivered has been parsed, and the
            // kernel never splits an event across reads, so bytes left over
            // do not frame one. Waiting for more would wait on a buffer that
            // never parses; refused instead.
            if !self.pending.is_empty() {
                return Err(format!(
                    "the card delivered {} bytes that do not frame a DRM event",
                    self.pending.len()
                ));
            }
            match (&self.card).read(&mut self.chunk) {
                Ok(0) => {
                    return Err(
                        "the DRM card returned a short read for a page-flip completion: the next \
                         event does not fit in a 4 KiB buffer"
                            .to_string(),
                    )
                }
                Ok(read) => self
                    .pending
                    .extend_from_slice(self.chunk.get(..read).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(format!(
                        "read a page-flip completion from the card: {error}"
                    ))
                }
            }
        }
    }
}

/// Parse `pending` up to and including its first flip completion and drop
/// what was parsed. Events that are not completions -- a vblank, a type this
/// build does not know -- are stepped over by the length they declare.
fn take_completion(pending: &mut Vec<u8>) -> Option<sys::DrmFlipCompletion> {
    let mut consumed = 0usize;
    let mut found = None;
    while let Some((length, completion)) =
        sys::parse_drm_event(pending.get(consumed..).unwrap_or_default())
    {
        consumed = consumed.saturating_add(length);
        if completion.is_some() {
            found = completion;
            break;
        }
    }
    pending.drain(..consumed.min(pending.len()));
    found
}

/// Open `path`, take mastership for good, and modeset a blank frame onto the
/// connector discovery prefers. Answers the backend, the reader for its
/// completions, and one line describing what was lit.
///
/// Mastership is dropped by `open_card` and re-taken at once. The window
/// between is two syscalls, and re-taking is permitted to a process that is
/// not root only because its descriptor was master when opened
/// (`drm_master_check_perm`'s `was_master`), which is what the seat grant of
/// the card to the compositor account relies on. Held from here to the
/// backend's drop; an error before `CardCrtc` exists needs no guard, because
/// nothing has been changed yet and closing the only descriptor gives it back.
pub fn open_kms(path: &Path) -> Result<(Kms, FlipEvents, String), String> {
    let card = open_card(path)?;
    sys::drm_set_master(&card).map_err(|error| {
        format!(
            "take DRM mastership of {}: {error} -- another process may be driving the display",
            path.display()
        )
    })?;
    // Discovery reads the mode list the kernel already has (fbcon probed it
    // at boot); it never forces a probe, which "can be slow, might cause
    // flickering and the ioctl will block" (drm_mode.h). A sink that changed
    // since is followed only once hotplug is.
    let discovery = discover(&card)?;
    let scanout = discovery.scanout;
    let output = scanout.output()?;
    let width = u32::from(scanout.mode.hdisplay);
    let height = u32::from(scanout.mode.vdisplay);
    // The frame limits at the tightest pitch the mode allows, before a byte
    // is allocated for it. The kernel's pitch is checked again once known.
    crate::framebuffer::validate_geometry(
        output.dimensions.width,
        output.dimensions.height,
        output.dimensions.width.saturating_mul(4),
    )?;
    // Read under mastership and before anything changes: this is the state
    // the restore puts back. `GETCRTC` reports no connector routing
    // (`drm_crtc.c:543`), and the connector just selected need not be the one
    // lit: on a card with two connected sinks `crtc_for` can pick a CRTC
    // another connector is lit by, and restoring to the selected connector
    // would leave that sink dark. So the routing is read, not assumed.
    let saved = sys::drm_get_crtc(&card, scanout.crtc_id)?;
    let routed = connectors_on_crtc(&card, scanout.crtc_id)?;
    let (saved, routed) = restorable(saved, routed);
    let card: Card = Arc::new(card);
    let crtc = CardCrtc {
        card: Arc::clone(&card),
        crtc_id: scanout.crtc_id,
        connector_id: scanout.connector_id,
        mode: scanout.mode,
        saved,
        routed,
    };
    let (first, pitch) = ScanoutBuffer::allocate(&card, width, height)?;
    let (second, second_pitch) = ScanoutBuffer::allocate(&card, width, height)?;
    // One stride for both, because one frame is rendered at it and copied
    // into either. A driver that pitched two identical allocations
    // differently is not one this backend can drive.
    if pitch != second_pitch {
        return Err(format!(
            "two {width}x{height} dumb buffers came back at pitches {pitch} and {second_pitch}"
        ));
    }
    let stride = usize::try_from(pitch).map_err(|_| format!("pitch {pitch} is not a usize"))?;
    let fbs = (first.fb_id(), second.fb_id());
    let events = FlipEvents {
        card: card
            .try_clone()
            .map_err(|error| format!("duplicate the card descriptor for completions: {error}"))?,
        crtc_id: scanout.crtc_id,
        pending: Vec::with_capacity(sys::DRM_EVENT_VBLANK_LEN * 4),
        chunk: zeroed(4096)?,
    };
    let chain = SwapChain::new(crtc, [first, second], output, stride)?;
    let described = format!("{} fb={},{} modeset=ok", discovery.describe(), fbs.0, fbs.1);
    Ok((chain, events, described))
}

/// A swap chain over ordinary memory and a CRTC that records what it was
/// asked, for tests of the chain and of the runtime driving it.
#[cfg(test)]
pub(crate) mod testing {
    use super::{ScanoutCrtc, ScanoutMemory, SwapChain};
    use crate::output::{Output, OutputDimensions, OutputId, OutputScale, OutputTransform};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum CrtcCall {
        Flip { fb_id: u32, cookie: u64 },
        Set { fb_id: u32 },
    }

    #[derive(Default)]
    pub struct CrtcLog {
        pub calls: Vec<CrtcCall>,
        pub fail_next_flip: bool,
        pub fail_next_set: bool,
    }

    pub struct RecordingCrtc(pub Arc<Mutex<CrtcLog>>);

    impl ScanoutCrtc for RecordingCrtc {
        fn flip(&mut self, fb_id: u32, cookie: u64) -> Result<(), String> {
            let mut log = self.0.lock().map_err(|_| "crtc log poisoned")?;
            if std::mem::take(&mut log.fail_next_flip) {
                return Err("injected flip failure".into());
            }
            log.calls.push(CrtcCall::Flip { fb_id, cookie });
            Ok(())
        }

        fn set(&mut self, fb_id: u32) -> Result<(), String> {
            let mut log = self.0.lock().map_err(|_| "crtc log poisoned")?;
            if std::mem::take(&mut log.fail_next_set) {
                return Err("injected modeset failure".into());
            }
            log.calls.push(CrtcCall::Set { fb_id });
            Ok(())
        }
    }

    /// A buffer's bytes, in ordinary memory.
    pub struct TestMemory {
        fb_id: u32,
        view: Vec<u8>,
    }

    impl ScanoutMemory for TestMemory {
        fn bytes_mut(&mut self) -> &mut [u8] {
            &mut self.view
        }

        fn fb_id(&self) -> u32 {
            self.fb_id
        }
    }

    pub type TestChain = SwapChain<RecordingCrtc, TestMemory>;

    /// A chain at `width`x`height`, its buffers pre-filled with `0xee` so a
    /// copy that skipped a row is visible, and the log its CRTC writes.
    pub fn chain(width: usize, height: usize) -> (TestChain, Arc<Mutex<CrtcLog>>) {
        let stride = width * 4;
        let log = Arc::new(Mutex::new(CrtcLog::default()));
        let memory = |fb_id| TestMemory {
            fb_id,
            view: vec![0xee; stride * height],
        };
        let output = Output {
            id: OutputId::FIRST,
            dimensions: OutputDimensions { width, height },
            scale: OutputScale::ONE,
            transform: OutputTransform::Normal,
        };
        let chain = SwapChain::new(
            RecordingCrtc(Arc::clone(&log)),
            [memory(41), memory(42)],
            output,
            stride,
        )
        .unwrap();
        (chain, log)
    }

    impl TestChain {
        /// The bytes buffer `index` holds.
        pub fn buffer(&self, index: usize) -> &[u8] {
            &self.buffers[index].memory.view
        }

        pub fn front_index(&self) -> usize {
            self.front
        }

        /// The frame last rendered, before any copy.
        pub fn frame(&self) -> &[u8] {
            &self.frame
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::output::{Damage, FrameId, OutputBackend, Submission};
    use testing::CrtcCall;

    fn fill(
        chain: &mut testing::TestChain,
        damage: Damage,
        byte: u8,
        rows: std::ops::Range<usize>,
    ) {
        let target = chain.begin_frame(damage).unwrap();
        let stride = target.stride;
        for row in rows {
            target.pixels[row * stride..(row + 1) * stride].fill(byte);
        }
    }

    /// The chain lights buffer 0, blank, at construction, and a first frame
    /// is queued to buffer 1 as a whole copy: buffer 1 held whatever the
    /// allocator left, so its shadow describes nothing.
    #[test]
    fn a_new_chain_modesets_a_blank_front_and_queues_a_whole_first_frame() {
        let (mut chain, log) = testing::chain(4, 3);
        assert_eq!(log.lock().unwrap().calls, [CrtcCall::Set { fb_id: 41 }]);
        assert!(chain.buffer(0).iter().all(|byte| *byte == 0));
        let blank = chain.completed().unwrap();
        assert!(blank.pixels.iter().all(|byte| *byte == 0));

        fill(&mut chain, Damage::Unknown, 7, 1..2);
        let submission = chain.present().unwrap();
        assert_eq!(submission, Submission::Queued(FIRST_FLIP));
        assert_eq!(
            log.lock().unwrap().calls.last(),
            Some(&CrtcCall::Flip {
                fb_id: 42,
                cookie: FIRST_FLIP.cookie()
            })
        );
        // Every row, including the two the frame left at zero: the 0xee the
        // test allocator left there is gone.
        assert_eq!(chain.buffer(1), chain.frame());
        // Nothing is known to be on glass while the flip is in flight.
        assert!(chain.completed().is_none());
        assert!(
            chain.present().is_err(),
            "a second flip was queued over the first"
        );

        chain.frame_presented(FIRST_FLIP).unwrap();
        assert_eq!(chain.front_index(), 1);
        assert_eq!(chain.completed().unwrap().pixels, chain.frame());
    }

    /// The back buffer is two frames old, so its copy is the rows IT lacks,
    /// which is more than the rows the last frame changed.
    #[test]
    fn a_back_buffer_receives_the_rows_it_lacks_not_only_the_last_frames() {
        let (mut chain, _log) = testing::chain(2, 4);
        // Frame 1 → buffer 1: row 0 set.
        fill(&mut chain, Damage::Unknown, 1, 0..1);
        chain.present().unwrap();
        chain.frame_presented(FIRST_FLIP).unwrap();
        // Frame 2 → buffer 0: row 3 set too. Buffer 0 is blank, so it needs
        // row 0 as well as row 3.
        fill(&mut chain, Damage::Unknown, 3, 3..4);
        let second = FIRST_FLIP.next();
        assert_eq!(chain.present().unwrap(), Submission::Queued(second));
        assert_eq!(chain.buffer(0), chain.frame());
        chain.frame_presented(second).unwrap();
        assert_eq!(chain.front_index(), 0);
    }

    /// A frame the glass already shows is answered without a flip: queuing
    /// it would wait a vblank to show nothing, and would owe a completion.
    /// `Whole` flips anyway, because it is a request to repaint.
    #[test]
    fn an_unchanged_frame_is_presented_without_a_flip_unless_whole() {
        let (mut chain, log) = testing::chain(2, 2);
        fill(&mut chain, Damage::Unknown, 0, 0..0);
        assert_eq!(chain.present().unwrap(), Submission::Presented);
        assert_eq!(log.lock().unwrap().calls.len(), 1, "only the modeset");

        fill(&mut chain, Damage::Whole, 0, 0..0);
        assert_eq!(chain.present().unwrap(), Submission::Queued(FIRST_FLIP));
    }

    /// Completions are matched on the frame, and one for anything but the
    /// queued frame is refused without moving the front.
    #[test]
    fn a_completion_for_another_frame_is_refused() {
        let (mut chain, _log) = testing::chain(2, 2);
        assert!(
            chain.frame_presented(FIRST_FLIP).is_err(),
            "nothing was queued"
        );
        fill(&mut chain, Damage::Unknown, 5, 0..1);
        chain.present().unwrap();
        assert!(chain.frame_presented(FIRST_FLIP.next()).is_err());
        assert_eq!(chain.front_index(), 0);
        assert!(chain.completed().is_none());
    }

    /// A stalled flip is recovered by modesetting onto its buffer, and a
    /// failed recovery leaves it queued and the glass unknown.
    #[test]
    fn a_stalled_flip_is_recovered_by_modeset_onto_its_buffer() {
        let (mut chain, log) = testing::chain(2, 2);
        fill(&mut chain, Damage::Unknown, 5, 0..1);
        chain.present().unwrap();

        log.lock().unwrap().fail_next_set = true;
        assert!(chain.recover_stalled_frame(FIRST_FLIP).is_err());
        assert!(chain.completed().is_none());
        assert!(
            chain.present().is_err(),
            "the failed recovery released the flip"
        );

        chain.recover_stalled_frame(FIRST_FLIP).unwrap();
        assert_eq!(
            log.lock().unwrap().calls.last(),
            Some(&CrtcCall::Set { fb_id: 42 })
        );
        assert_eq!(chain.front_index(), 1);
        assert_eq!(chain.completed().unwrap().pixels, chain.frame());
        // And the late completion the kernel may still send is not the
        // chain's to act on.
        assert!(chain.frame_presented(FIRST_FLIP).is_err());
    }

    /// A `Whole` repair of an unchanged frame survives a refused flip: a
    /// retry flips rather than answering that the glass already shows it.
    #[test]
    fn a_refused_forced_flip_keeps_the_repair_owed() {
        let (mut chain, log) = testing::chain(2, 2);
        fill(&mut chain, Damage::Whole, 0, 0..0);
        log.lock().unwrap().fail_next_flip = true;
        assert!(chain.present().is_err());
        assert_eq!(chain.present().unwrap(), Submission::Queued(FIRST_FLIP));
    }

    /// A flip the kernel refused leaves nothing queued, and the frame's rows
    /// are still in the back buffer, so a retry need not copy them again.
    #[test]
    fn a_refused_flip_queues_nothing() {
        let (mut chain, log) = testing::chain(2, 2);
        fill(&mut chain, Damage::Unknown, 5, 0..1);
        log.lock().unwrap().fail_next_flip = true;
        assert!(chain.present().is_err());
        assert_eq!(chain.present().unwrap(), Submission::Queued(FIRST_FLIP));
    }

    /// One DRM event: `kind`, a 32-byte length, the cookie, and CRTC `crtc`.
    fn event_bytes(kind: u32, cookie: u64, crtc: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&32u32.to_le_bytes());
        bytes.extend_from_slice(&cookie.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&crtc.to_le_bytes());
        bytes
    }

    /// A reader over a regular file holding `bytes`, which reads them and
    /// then answers end-of-file the way `drm_read` answers an event too
    /// large for the buffer.
    fn events_from(bytes: &[u8], name: &str) -> FlipEvents {
        let path =
            std::env::temp_dir().join(format!("td-flip-events-{name}-{}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let card = File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        FlipEvents {
            card,
            crtc_id: 29,
            pending: Vec::new(),
            chunk: vec![0; 4096],
        }
    }

    /// The reader names each completion's frame, in order, past a vblank.
    #[test]
    fn flip_events_name_each_completions_frame_in_order() {
        let mut bytes = event_bytes(0x01, 9, 29);
        bytes.extend(event_bytes(sys::DRM_EVENT_FLIP_COMPLETE, 2, 29));
        bytes.extend(event_bytes(sys::DRM_EVENT_FLIP_COMPLETE, 3, 29));
        let mut events = events_from(&bytes, "order");
        assert_eq!(events.next().unwrap(), FrameId::from_cookie(2));
        assert_eq!(events.next().unwrap(), FrameId::from_cookie(3));
        let short = events.next().unwrap_err();
        assert!(short.contains("short read"), "{short}");
    }

    /// Each of the reader's refusals, which end the compositor: another
    /// CRTC's completion, and bytes that cannot frame an event.
    #[test]
    fn flip_events_refuse_another_crtc_and_unframed_bytes() {
        let foreign = events_from(&event_bytes(sys::DRM_EVENT_FLIP_COMPLETE, 2, 30), "crtc")
            .next()
            .unwrap_err();
        assert!(foreign.contains("CRTC 30"), "{foreign}");
        // A header declaring 64 bytes over a 32-byte read: the kernel never
        // splits an event, so what is left over is not the start of one.
        let mut torn = event_bytes(sys::DRM_EVENT_FLIP_COMPLETE, 2, 29);
        torn[4] = 64;
        let unframed = events_from(&torn, "torn").next().unwrap_err();
        assert!(unframed.contains("do not frame"), "{unframed}");
    }

    /// Completion parsing takes the first completion and leaves the rest,
    /// stepping over any other event on the way.
    #[test]
    fn completions_are_taken_one_at_a_time_past_other_events() {
        let event = |kind: u32, cookie: u64| event_bytes(kind, cookie, 29);
        let mut pending = event(0x01, 9);
        pending.extend(event(sys::DRM_EVENT_FLIP_COMPLETE, 2));
        pending.extend(event(sys::DRM_EVENT_FLIP_COMPLETE, 3));
        assert_eq!(take_completion(&mut pending).map(|c| c.user_data), Some(2));
        assert_eq!(pending.len(), 32);
        assert_eq!(take_completion(&mut pending).map(|c| c.user_data), Some(3));
        assert!(pending.is_empty());
        assert_eq!(take_completion(&mut pending), None);
    }

    /// A driver whose reported size covers its own stride is accepted, and an
    /// over-aligned pitch is fine: the kernel picks it.
    #[test]
    fn a_buffer_that_covers_its_scanout_is_accepted() {
        assert_eq!(buffer_covers_scanout(5120, 1280, 800, 4_096_000), Ok(()));
        // A driver padding the scanline to 8192 needs a bigger buffer, and
        // reports one. Demanding pitch == width * 4 would reject this.
        assert_eq!(buffer_covers_scanout(8192, 1280, 800, 6_553_600), Ok(()));
    }

    /// A pitch narrower than four bytes per pixel means the driver and this
    /// code disagree about the format, and every row would land on the one
    /// before it.
    #[test]
    fn a_pitch_narrower_than_the_row_is_refused() {
        let error = buffer_covers_scanout(4096, 1280, 800, 4_096_000).unwrap_err();
        assert!(error.contains("pitch 4096"), "{error}");
        assert!(error.contains("5120"), "{error}");
    }

    /// A size below `pitch * height` is a buffer short of its own stride, and
    /// a renderer trusting it writes past the mapping on the last rows.
    #[test]
    fn a_size_below_its_own_stride_is_refused() {
        let error = buffer_covers_scanout(5120, 1280, 800, 4_095_999).unwrap_err();
        assert!(error.contains("4096000"), "{error}");
    }

    /// The widest inputs do not wrap into a small `needed` that would pass.
    ///
    /// `u32 * u32` fits a `u64`, so the product is exact even at the extreme
    /// and a short buffer is still refused there. This is the test that made
    /// the point: an earlier revision guarded the multiply with `checked_mul`
    /// and reported an overflow that cannot happen, and this test could not be
    /// written to reach it.
    #[test]
    fn the_widest_pitch_and_height_do_not_wrap() {
        let error = buffer_covers_scanout(u32::MAX, 1, u32::MAX, 0).unwrap_err();
        assert!(error.contains("needs"), "{error}");
        assert!(error.contains("18446744065119617025"), "{error}");
    }

    fn live_crtc(fb_id: u32, width: u16, height: u16, valid: u32) -> sys::DrmModeCrtc {
        let mut state = sys::DrmModeCrtc::empty();
        state.crtc_id = 29;
        state.fb_id = fb_id;
        state.mode_valid = valid;
        state.mode = mode(width, height, true, "1280x800");
        state
    }

    /// A CRTC showing exactly what was asked for passes.
    #[test]
    fn a_crtc_showing_the_requested_framebuffer_and_mode_passes() {
        let wanted = mode(1280, 800, true, "1280x800");
        assert_eq!(
            crtc_shows(&live_crtc(7, 1280, 800, 1), 29, 7, &wanted),
            Ok(())
        );
    }

    /// `SETCRTC` answering success is not the same claim as the CRTC being on.
    #[test]
    fn a_crtc_that_stayed_dark_is_refused() {
        let wanted = mode(1280, 800, true, "1280x800");
        let error = crtc_shows(&live_crtc(7, 1280, 800, 0), 29, 7, &wanted).unwrap_err();
        assert!(error.contains("no valid mode"), "{error}");
    }

    /// The failure this check exists for: a driver that accepted the request,
    /// answered success, and kept scanning out the picture that was already
    /// there. Nothing else in the backend would notice.
    #[test]
    fn a_crtc_still_showing_the_previous_framebuffer_is_refused() {
        let wanted = mode(1280, 800, true, "1280x800");
        let error = crtc_shows(&live_crtc(3, 1280, 800, 1), 29, 7, &wanted).unwrap_err();
        assert!(error.contains("framebuffer 3"), "{error}");
        assert!(error.contains("the 7 just set"), "{error}");
    }

    /// The kernel validates a requested mode and may land on another one, so
    /// the size that matters is the size the CRTC ended up at.
    #[test]
    fn a_crtc_that_settled_on_another_mode_is_refused() {
        let wanted = mode(1280, 800, true, "1280x800");
        let error = crtc_shows(&live_crtc(7, 1024, 768, 1), 29, 7, &wanted).unwrap_err();
        assert!(error.contains("1024x768"), "{error}");
        assert!(error.contains("1280x800"), "{error}");
    }

    /// A CRTC that was lit, onto a real framebuffer, with somewhere to send
    /// it: the only shape that can be replayed as it stands.
    #[test]
    fn a_lit_crtc_is_restored_as_it_was() {
        assert!(is_restorable(&live_crtc(7, 1280, 800, 1), &[31]));
    }

    /// A CRTC that was already off. Replaying it means switching it off, which
    /// `restorable` expresses as the empty request rather than as this one.
    #[test]
    fn a_dark_crtc_is_not_replayed() {
        assert!(!is_restorable(&live_crtc(7, 1280, 800, 0), &[31]));
    }

    /// The edge `GETCRTC` makes reachable: `mode_valid` comes from
    /// `crtc->state->enable` and `fb_id` from the primary plane, filled
    /// INDEPENDENTLY, so an enabled CRTC with no primary framebuffer reads
    /// back like this. Sent as-is the kernel looks up framebuffer 0 and
    /// answers `-ENOENT`, which the old restore discarded.
    #[test]
    fn an_enabled_crtc_with_no_framebuffer_is_not_replayed() {
        assert!(!is_restorable(&live_crtc(0, 1280, 800, 1), &[31]));
    }

    /// A mode with no connectors is refused outright by `drm_mode_setcrtc`
    /// (`count_connectors == 0 && mode`), so it is never sent.
    #[test]
    fn a_mode_with_nowhere_to_send_it_is_not_replayed() {
        assert!(!is_restorable(&live_crtc(7, 1280, 800, 1), &[]));
    }

    fn mode(width: u16, height: u16, preferred: bool, name: &str) -> sys::DrmModeInfo {
        let mut mode = sys::DrmModeInfo {
            clock: 0,
            hdisplay: width,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 0,
            hskew: 0,
            vdisplay: height,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 0,
            vscan: 0,
            vrefresh: 60,
            flags: 0,
            mode_type: if preferred {
                sys::DRM_MODE_TYPE_PREFERRED
            } else {
                0
            },
            name: [0; 32],
        };
        for (slot, byte) in mode.name.iter_mut().zip(name.bytes()) {
            *slot = byte;
        }
        mode
    }

    fn resources(crtcs: &[u32]) -> sys::DrmResources {
        sys::DrmResources {
            crtcs: crtcs.to_vec(),
            connectors: Vec::new(),
        }
    }

    fn encoder(possible_crtcs: u32) -> sys::DrmEncoder {
        sys::DrmEncoder {
            id: 7,
            crtc_id: 0,
            possible_crtcs,
        }
    }

    /// The preferred bit wins even when it is not first, because a driver that
    /// set it is naming the mode the sink actually wants.
    #[test]
    fn the_preferred_mode_is_taken_over_the_first_one() {
        let modes = [
            mode(1920, 1080, false, "1920x1080"),
            mode(1024, 768, true, "1024x768"),
        ];
        let chosen = preferred_mode(&modes).expect("a mode is offered");
        assert_eq!(chosen.hdisplay, 1024);
        assert_eq!(chosen.name(), "1024x768");
        assert!(chosen.is_preferred());
    }

    /// With nothing marked, the kernel's own ordering is the recommendation.
    #[test]
    fn with_no_preferred_mode_the_kernels_first_is_taken() {
        let modes = [
            mode(1280, 800, false, "1280x800"),
            mode(1024, 768, false, "1024x768"),
        ];
        let chosen = preferred_mode(&modes).expect("a mode is offered");
        assert_eq!(chosen.hdisplay, 1280);
    }

    #[test]
    fn a_connector_offering_nothing_selects_no_mode() {
        assert!(preferred_mode(&[]).is_none());
    }

    /// The mask indexes the CRTC LIST. A mask of 0b10 must select the second
    /// crtc in the list, not the crtc whose id happens to be 2.
    #[test]
    fn possible_crtcs_indexes_the_list_and_is_not_a_set_of_ids() {
        let resources = resources(&[70, 80, 90]);
        assert_eq!(first_possible_crtc(&encoder(0b010), &resources), Some(80));
        assert_eq!(first_possible_crtc(&encoder(0b100), &resources), Some(90));
        // Were the mask read as ids, a mask naming bit 1 would find nothing
        // here and a mask of 0b1010000... would find id 80. Both are wrong in
        // a way that still returns a valid-looking CRTC.
        assert_eq!(first_possible_crtc(&encoder(0b110), &resources), Some(80));
    }

    #[test]
    fn an_encoder_that_reaches_nothing_selects_no_crtc() {
        assert_eq!(first_possible_crtc(&encoder(0), &resources(&[70])), None);
    }

    /// A card may list more CRTCs than the 32-bit mask can name. The extra
    /// ones are unreachable through this encoder, and asking must not shift
    /// by 32.
    #[test]
    fn a_crtc_past_the_masks_width_is_unreachable_rather_than_undefined() {
        let many: Vec<u32> = (0..40).collect();
        assert_eq!(first_possible_crtc(&encoder(0), &resources(&many)), None);
        assert_eq!(
            first_possible_crtc(&encoder(1 << 31), &resources(&many)),
            Some(31)
        );
    }

    /// `Connected` outranks `Unknown`, which is the kernel's own advice and
    /// the reason the enum is ordered rather than matched.
    #[test]
    fn a_connected_sink_outranks_one_the_kernel_could_not_probe() {
        assert!(Confidence::Connected > Confidence::Unknown);
    }

    #[test]
    fn a_scanout_names_its_connector_the_way_the_kernels_tools_do() {
        let scanout = Scanout {
            connector_id: 31,
            connector_type: 15,
            connector_type_id: 1,
            connection: sys::DRM_MODE_CONNECTED,
            encoder_id: 30,
            crtc_id: 29,
            mode: mode(1280, 800, true, "1280x800"),
            mm_width: 0,
            mm_height: 0,
        };
        assert_eq!(scanout.connector_name(), "Virtual-1");
        let output = scanout.output().expect("a sized mode is an output");
        assert_eq!(output.dimensions.width, 1280);
        assert_eq!(output.dimensions.height, 800);
    }

    /// A zero-sized mode is refused where it is read rather than dividing by
    /// zero somewhere further away.
    #[test]
    fn a_mode_with_no_pixels_is_not_an_output() {
        let scanout = Scanout {
            connector_id: 31,
            connector_type: 15,
            connector_type_id: 1,
            connection: sys::DRM_MODE_CONNECTED,
            encoder_id: 30,
            crtc_id: 29,
            mode: mode(0, 800, false, "bad"),
            mm_width: 0,
            mm_height: 0,
        };
        let error = scanout.output().expect_err("a zero width is not an output");
        assert!(error.contains("zero width"), "{error}");
    }

    /// The mode name is NUL-padded and the kernel does not promise a
    /// terminator in the last byte, so a name filling the field is read whole.
    #[test]
    fn a_mode_name_filling_the_field_is_not_truncated_by_a_missing_nul() {
        let mut full = mode(640, 480, false, "");
        full.name = [b'x'; 32];
        assert_eq!(full.name().len(), 32);
    }
}
