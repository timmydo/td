//! DRM/KMS discovery: which card, which connector, which mode, which CRTC.
//!
//! §M's first row is a DRM/KMS output backend. This module is its DISCOVERY
//! half — everything a card will answer when it is only read: no modeset, no
//! buffer, no mapping, and no DRM mastership. Keeping that half apart is what
//! makes the selection testable against recorded connector shapes instead of
//! against a card, and it leaves taking mastership away from `fbcon` as a
//! decision the backend landing has to make out loud rather than inherit.
//!
//! The kernel ABI lives in `sys.rs` with the rest of it. What is here is
//! policy: which connector to believe, which of its modes to want, and which
//! CRTC can drive it.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::output::{Output, OutputDimensions, OutputId, OutputScale, OutputTransform};
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

/// The GEM handle alone, so releasing it is a FIELD DROP rather than a
/// statement someone can reorder or an early return can skip.
struct DumbHandle<'card> {
    card: &'card File,
    handle: u32,
}

impl Drop for DumbHandle<'_> {
    fn drop(&mut self) {
        let _ = sys::drm_destroy_dumb(self.card, self.handle);
    }
}

/// One dumb buffer and its mapping, released together and in that order.
///
/// Field order is load-bearing, and there is deliberately NO `impl Drop` on
/// this type. Rust drops fields in declaration order, so `region` unmaps before
/// `handle` releases the GEM handle. An explicit `Drop` here would run before
/// either field and invert that.
///
/// The kernel tolerates either order — a GEM mapping takes its own reference,
/// so closing the handle first leaves the mapping valid — so this is hygiene
/// rather than a requirement, and an earlier comment here called it "the one
/// ordering worth avoiding", which overstated it. It is still expressed in the
/// type rather than in a destructor, because releasing in the reverse of
/// acquisition is the default worth having and this is the version of it the
/// compiler enforces for free.
pub struct DumbFrame<'card> {
    region: sys::MappedRegion,
    /// Never read, and that is the point: this field exists for its `Drop`,
    /// which is what releases the GEM handle after `region` has unmapped it.
    /// Held by value rather than released in a destructor here so the ordering
    /// is the compiler's business rather than a comment's.
    #[allow(dead_code)]
    handle: DumbHandle<'card>,
    width: u32,
    height: u32,
    pitch: u32,
}

impl<'card> DumbFrame<'card> {
    /// Allocate a buffer for one scanout and map it.
    ///
    /// The handle guard is built BEFORE the mapping is attempted, so a failure
    /// to map releases the buffer on the way out rather than leaking it: the
    /// `?` below drops the guard. That is the reason for the two-step rather
    /// than a tidier single constructor.
    pub fn allocate(
        card: &'card File,
        width: u32,
        height: u32,
    ) -> Result<DumbFrame<'card>, String> {
        let buffer = sys::drm_create_dumb(card, width, height)?;
        let handle = DumbHandle {
            card,
            handle: buffer.handle,
        };
        buffer_covers_scanout(buffer.pitch, width, height, buffer.size)?;
        let region = sys::drm_map_dumb(card, &buffer)?;
        Ok(DumbFrame {
            region,
            handle,
            width,
            height,
            pitch: buffer.pitch,
        })
    }

    /// The mapping's length.
    pub fn len(&self) -> usize {
        self.region.len()
    }

    /// The pixels, borrowed for no longer than this frame.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.region.bytes_mut()
    }

    /// Write a known pattern through the mapping and read it back.
    ///
    /// The point is not the pattern. `mmap` answering an address is not
    /// evidence that the address IS the buffer, and a length that disagreed
    /// with its mapping would still map and still write — it would simply write
    /// somewhere else, or past the end. Reading back what was written is the
    /// cheapest thing that separates a live mapping from a plausible pointer.
    ///
    /// Split into three so the halves are testable without a card: the offsets
    /// are chosen by arithmetic, the write and the verify are separate passes
    /// over a plain slice, and a test can corrupt a byte between them. A single
    /// function writing and reading the same slice could not have failed.
    pub fn prove_mapping(&mut self) -> Result<(), String> {
        let offsets = pattern_offsets(self.len())?;
        let pixels = self.pixels_mut();
        write_pattern(pixels, &offsets);
        verify_pattern(pixels, &offsets)
    }

    /// One line, for a proof to match and a person to read.
    pub fn describe(&self) -> String {
        format!(
            "buffer={}x{} pitch={} bytes={} mapping=ok",
            self.width,
            self.height,
            self.pitch,
            self.len()
        )
    }
}

/// The first byte of the read-back pattern. Not zero and not `0xff`: a mapping
/// that reads back as freshly-zeroed or as unwritten memory would satisfy
/// either of those without anything having been written.
const PATTERN_BASE: u8 = 0xa5;

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

/// The three offsets the read-back pattern uses: first, middle and LAST.
///
/// The last is the one that earns its place. An off-by-one in a mapping's
/// length shows up at the final byte and nowhere else, which is the failure
/// `UNSAFE.md` §6 asks the length to be held in the region type to prevent. A
/// zero-length mapping is refused rather than handed an empty set, because
/// "every one of no probes passed" is not evidence of anything.
fn pattern_offsets(length: usize) -> Result<[usize; 3], String> {
    let last = length
        .checked_sub(1)
        .ok_or_else(|| "dumb mapping: a zero-length mapping proves nothing".to_string())?;
    Ok([0, length / 2, last])
}

/// Stamp the pattern. Offsets past the end are skipped rather than refused:
/// `pattern_offsets` derives them from the same length, so an out-of-range one
/// is unreachable, and `verify_pattern` is what reports a byte that did not
/// take rather than this silently deciding a short buffer is fine.
fn write_pattern(pixels: &mut [u8], offsets: &[usize]) {
    for (step, offset) in offsets.iter().enumerate() {
        if let Some(slot) = pixels.get_mut(*offset) {
            *slot = PATTERN_BASE.wrapping_add(step as u8);
        }
    }
}

/// Read the pattern back, naming the first byte that disagrees.
fn verify_pattern(pixels: &[u8], offsets: &[usize]) -> Result<(), String> {
    let length = pixels.len();
    for (step, offset) in offsets.iter().enumerate() {
        let expected = PATTERN_BASE.wrapping_add(step as u8);
        let seen = pixels
            .get(*offset)
            .ok_or_else(|| format!("dumb mapping: offset {offset} is past {length} bytes"))?;
        if *seen != expected {
            return Err(format!(
                "dumb mapping: byte {offset} of {length} read back {seen:#04x}, not \
                 {expected:#04x} — the mapping is not the buffer"
            ));
        }
    }
    Ok(())
}

/// Open a card node and immediately give back the authority opening it took.
///
/// Read-write because a DRM node is: the mode-setting requests the next
/// landing issues are writes to the device even though this one only reads,
/// and opening read-only would defer the failure to the modeset rather than
/// report it at the door.
///
/// The `drm_drop_master` is not politeness, it is the correctness of every
/// claim this module makes about not disturbing the screen. `drm_master_open`
/// makes the first opener of a primary node the DRM master whenever
/// `dev->master` is NULL, and fbcon — an in-kernel client — never sets it. So
/// the plain `open` above IS the acquisition, and while it is held the running
/// compositor's fbdev damage is dropped with `-EBUSY`. Dropping it here closes
/// a window measured in the whole length of the probe down to the two syscalls
/// between them.
pub fn open_card(path: &Path) -> Result<File, String> {
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
pub fn discover(card: &File) -> Result<Discovery, String> {
    let driver = sys::drm_driver_name(card)?;
    let resources = sys::drm_resources(card)
        .map_err(|error| format!("{error} (driver {driver})"))?;
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
             read, and status values this build does not know: {unrecognised:?}. A \
             connector reporting no modes is as much a statement about \
             mastership as about the sink: DRM_IOCTL_MODE_GETCONNECTOR re-probes only for the \
             current DRM master, and this process is deliberately not one",
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A zero-length mapping proves nothing, and is refused rather than given
    /// an empty probe set that would pass vacuously.
    #[test]
    fn a_zero_length_mapping_is_not_a_proof() {
        let error = pattern_offsets(0).unwrap_err();
        assert!(error.contains("proves nothing"), "{error}");
    }

    /// The LAST byte is probed, not just the first and the middle. This is the
    /// whole reason the set has three members: a mapping one byte short of its
    /// buffer reads back correctly everywhere except here.
    #[test]
    fn the_last_byte_of_the_mapping_is_probed() {
        assert_eq!(pattern_offsets(1).unwrap(), [0, 0, 0]);
        assert_eq!(pattern_offsets(2).unwrap(), [0, 1, 1]);
        let offsets = pattern_offsets(4096).unwrap();
        assert_eq!(offsets, [0, 2048, 4095]);
    }

    /// A mapping that keeps what was written to it passes.
    #[test]
    fn a_mapping_that_retains_its_writes_is_proven() {
        let mut pixels = vec![0u8; 4096];
        let offsets = pattern_offsets(pixels.len()).unwrap();
        write_pattern(&mut pixels, &offsets);
        assert_eq!(verify_pattern(&pixels, &offsets), Ok(()));
    }

    /// The three offsets carry DIFFERENT bytes, so a mapping that aliased all
    /// three onto one address would fail rather than read back its own last
    /// write three times.
    #[test]
    fn the_three_probes_are_not_the_same_byte() {
        let mut pixels = vec![0u8; 4096];
        let offsets = pattern_offsets(pixels.len()).unwrap();
        write_pattern(&mut pixels, &offsets);
        let seen: Vec<u8> = offsets.iter().filter_map(|at| pixels.get(*at).copied()).collect();
        assert_eq!(seen.len(), 3);
        assert_ne!(seen.first(), seen.get(1));
        assert_ne!(seen.get(1), seen.get(2));
    }

    /// A byte that does not survive the write is named, with its offset. This
    /// is the case a single write-then-read pass over one slice could not have
    /// produced, and it is why the two halves are separate functions.
    #[test]
    fn a_byte_that_does_not_stick_is_reported_by_offset() {
        let mut pixels = vec![0u8; 4096];
        let offsets = pattern_offsets(pixels.len()).unwrap();
        write_pattern(&mut pixels, &offsets);
        // The LAST byte, which is the off-by-one a short mapping produces.
        if let Some(slot) = pixels.get_mut(4095) {
            *slot = 0;
        }
        let error = verify_pattern(&pixels, &offsets).unwrap_err();
        assert!(error.contains("byte 4095 of 4096"), "{error}");
        assert!(error.contains("not the buffer"), "{error}");
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
