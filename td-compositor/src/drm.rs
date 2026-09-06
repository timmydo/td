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
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

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
    /// Held by value rather than released in a destructor here, so the release
    /// ORDER is the compiler's business rather than a comment's. It is read
    /// now, by `handle()`, which the modeset needs to name the buffer to
    /// `ADDFB2`; before that it existed only for its `Drop`.
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

    /// The kernel's stride for this buffer, in bytes.
    ///
    /// The kernel's, never `width * 4`: a driver may align a scanline well past
    /// the pixel width, and `ADDFB2` has to be told the real one or the
    /// framebuffer it registers describes rows that are not where the pixels
    /// are.
    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    /// The GEM handle, for naming this buffer to `ADDFB2`.
    pub fn handle(&self) -> u32 {
        self.handle.handle
    }

    /// The size this buffer was ALLOCATED at.
    ///
    /// Read by the modeset so it can refuse to register a framebuffer whose
    /// declared size is not the size of the memory behind it. `ADDFB2` is told
    /// a width, a height, a pitch and a handle as four bare numbers, and the
    /// kernel's own object-size check is the only thing that would catch a
    /// mismatch -- which makes it an error reported from the wrong place, in
    /// terms of a GEM object rather than of the two sizes that disagreed.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The height this buffer was allocated at. See `width`.
    pub fn height(&self) -> u32 {
        self.height
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

/// Is this CRTC actually showing what was asked for?
///
/// Split out of `verify` so the three refusals are testable against recorded
/// CRTC states rather than against a card. Each is a different failure with a
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

/// DRM mastership, taken for a bounded window and given back on drop.
struct MasterGuard<'card> {
    card: &'card File,
}

impl Drop for MasterGuard<'_> {
    fn drop(&mut self) {
        let _ = sys::drm_drop_master(self.card);
    }
}

/// One registered scanout framebuffer, unregistered on drop.
struct FbGuard<'card> {
    card: &'card File,
    fb_id: u32,
}

impl Drop for FbGuard<'_> {
    fn drop(&mut self) {
        let _ = sys::drm_rm_fb(self.card, self.fb_id);
    }
}

/// The CRTC state as it was before this process touched it.
///
/// `GETCRTC` reports the mode, framebuffer and position but NOT the connector
/// routing — the kernel fills neither `set_connectors_ptr` nor
/// `count_connectors` on the way out (`drm_mode_getcrtc`, `drm_crtc.c:543`) —
/// so the routing is read the long way instead, by `connectors_on_crtc`.
///
/// An earlier revision substituted the connector this code was about to drive
/// and called it "the same connector fbcon was using". It is not the same
/// thing. The connector is chosen by `select_scanout` on PREFERENCE and its
/// CRTC by `crtc_for`, which falls back to the first CRTC the connector's
/// encoder can reach — so on a card with two connected sinks, this can pick a
/// CRTC that some OTHER connector is currently lit by, and restoring to the
/// selected connector would then leave that sink dark. Reading the routing
/// costs three ioctls this module already issues and removes the guess.
///
/// This is still best-effort, and the backstop is weaker than it looks:
/// `drm_lastclose`'s `drm_client_dev_restore` puts the fbdev client back
/// exactly, but `drm_release` calls it only when the device's open count
/// reaches zero (`drm_file.c:440`). Nothing else opens the card today, so it
/// does run; the moment §M row 1's backend holds the node open it stops
/// running, and this restore becomes the only one there is.
struct CrtcRestore<'card> {
    card: &'card File,
    saved: sys::DrmModeCrtc,
    connectors: Vec<u32>,
}

impl<'card> CrtcRestore<'card> {
    /// Capture a CRTC state as a request `SETCRTC` will actually accept.
    ///
    /// The saved state and a well-formed request are not the same thing, and
    /// the gap is reachable. `drm_mode_getcrtc` fills `mode_valid` from
    /// `crtc->state->enable` and `fb_id` from the primary plane INDEPENDENTLY
    /// (`drm_crtc.c:577`, `:562`), so an enabled CRTC with no primary
    /// framebuffer reads back as `mode_valid = 1, fb_id = 0`. Replaying that
    /// asks the kernel to look up framebuffer 0, which answers `-ENOENT`
    /// (`:773`). Two more shapes are refused outright: a mode with no
    /// connectors (`:824`) and connectors with no mode or no framebuffer
    /// (`:830`).
    ///
    /// So anything that would be refused is turned into the one request that
    /// is always well-formed — switch the CRTC off — rather than sent and
    /// silently failed. That is a worse restore than the real one and a better
    /// one than none, and `drm_lastclose` is what makes it recoverable.
    fn capture(card: &'card File, saved: sys::DrmModeCrtc, connectors: Vec<u32>) -> Self {
        if is_restorable(&saved, &connectors) {
            return CrtcRestore {
                card,
                saved,
                connectors,
            };
        }
        let mut off = saved;
        off.mode_valid = 0;
        off.fb_id = 0;
        CrtcRestore {
            card,
            saved: off,
            connectors: Vec::new(),
        }
    }
}

impl Drop for CrtcRestore<'_> {
    fn drop(&mut self) {
        // Reported rather than swallowed. A restore that failed leaves a
        // screen this process is responsible for in a state it did not
        // intend, and the old `let _ =` made that indistinguishable from
        // success. stderr and never stdout: the probe marker is on stdout and
        // the boot check reads that stream whole-line.
        if let Err(error) = sys::drm_set_crtc(self.card, &self.saved, &mut self.connectors) {
            let _ = writeln!(
                std::io::stderr(),
                "td-compositor: restoring CRTC {} failed: {error}",
                self.saved.crtc_id
            );
        }
    }
}

/// How long to wait for a flip the kernel accepted.
///
/// A queued flip completes at the next vblank, so this is orders of magnitude
/// longer than it needs to be — a 60Hz output is 17ms and even a slow virtual
/// one is far inside this. It is a bound on a HANG, not a schedule: without
/// it a driver that accepted the flip and never delivered would park the probe
/// on a blocking read, and a boot that never finishes reports as a timeout
/// blaming whatever ran last rather than as the flip that did not arrive.
///
/// Five seconds rather than the two an earlier revision used, for one reason:
/// this runs inside every agent's `qemu-boot-system`, under TCG, on a host
/// that may be running several other agents' VMs at once. virtio-gpu delivers
/// the completion from an hrtimer that needs the guest scheduled, so the
/// number that matters is not the frame interval but how long the guest might
/// go unscheduled. The extra three seconds cost nothing — they are only ever
/// spent on a flip that is already failing — and an intermittent red on a row
/// every agent runs is expensive to diagnose.
const FLIP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to sleep between reads while waiting.
///
/// The descriptor is made non-blocking for the wait, so this is a poll
/// interval rather than a latency floor. Small enough that the wait costs one
/// interval on average, large enough not to spin a core for the whole vblank.
const FLIP_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// One page flip: a second framebuffer, queued onto a live modeset and waited
/// for.
///
/// A flip is not a modeset and does not unwind like one. It changes which
/// framebuffer a CRTC scans out and changes nothing else, so what it owns is
/// the framebuffer registration — the CRTC's own restoration stays the
/// `Modeset`'s, which is why this borrows one rather than replacing it.
pub struct Flip<'card, 'frame> {
    /// Unregistered on drop, like the modeset's. This runs BEFORE the
    /// modeset's restore, since a `Flip` is created after one and dropped
    /// first, so the kernel blanks the CRTC on the way out and the restore
    /// then turns it back on — the flicker `Modeset` records, not an error.
    #[allow(dead_code)]
    fb: FbGuard<'card>,
    /// For `Modeset`'s reason: the compiler refuses a flip that outlives the
    /// buffer the kernel is scanning out.
    #[allow(dead_code)]
    frame: &'frame DumbFrame<'card>,
    /// And the modeset it was queued onto, for the same reason one step out.
    /// A flip that outlived its modeset would unregister its framebuffer after
    /// mastership had been given back, and `verify` would read a CRTC that had
    /// already been restored. Nothing reads this field; holding the borrow is
    /// the whole point, exactly as with `frame` above.
    #[allow(dead_code)]
    modeset: &'frame Modeset<'card, 'frame>,
    fb_id: u32,
    crtc_id: u32,
    completion: sys::DrmFlipCompletion,
}

impl<'card, 'frame> Flip<'card, 'frame> {
    /// Register `frame`, queue it on the modeset's CRTC, and wait for the
    /// kernel to say it is on glass.
    ///
    /// `cookie` is carried through the kernel and compared on the way back.
    /// That comparison is the point of the whole increment: a completion that
    /// cannot be matched to the frame that caused it is not a completion path,
    /// and a caller with two frames in flight would otherwise be guessing.
    pub fn queue_and_wait(
        card: &'card File,
        modeset: &'frame Modeset<'card, 'frame>,
        scanout: &Scanout,
        frame: &'frame DumbFrame<'card>,
        cookie: u64,
    ) -> Result<Flip<'card, 'frame>, String> {
        let width = u32::from(scanout.mode.hdisplay);
        let height = u32::from(scanout.mode.vdisplay);
        // The flip's framebuffer has to cover the plane's source rectangle,
        // which the kernel checks and refuses. Checked here for `apply`'s
        // reason: the two sizes that disagree are visible right here.
        frame_fits_mode(frame.width(), frame.height(), width, height)?;
        let fb_id = sys::drm_add_fb(card, width, height, frame.pitch(), frame.handle())?;
        let fb = FbGuard { card, fb_id };
        sys::drm_page_flip(card, modeset.crtc_id(), fb_id, cookie)?;
        // Queued. From here the framebuffer must stay registered until the
        // completion arrives, which is what `fb` living to the end of this
        // function and then into the returned value achieves.
        let completion = await_flip(card, cookie)?;
        // The completion names its CRTC (`drm_plane.c:1526` fills it from
        // `crtc->base.id`), and matching on the cookie alone would accept a
        // completion for the right frame on the wrong CRTC. Cheap, and the
        // field is already parsed.
        if completion.crtc_id != modeset.crtc_id() {
            return Err(format!(
                "the flip completion for {cookie:#x} named CRTC {} rather than the {} it was \
                 queued on",
                completion.crtc_id,
                modeset.crtc_id()
            ));
        }
        Ok(Flip {
            fb,
            frame,
            modeset,
            fb_id,
            crtc_id: modeset.crtc_id(),
            completion,
        })
    }

    /// Ask the CRTC which framebuffer it has been COMMITTED to.
    ///
    /// Weaker than an earlier revision of this comment claimed, and the
    /// difference matters. That revision said a driver which reported
    /// completion but kept the previous framebuffer would fail here. It would
    /// not: `drm_mode_getcrtc` reports `plane->state->fb` (`drm_crtc.c:561`),
    /// and for an atomic driver `drm_atomic_helper_commit` swaps the software
    /// state at `drm_atomic_helper.c:2284` — BEFORE it queues the work that
    /// performs the flip at `:2309`, with the kernel's own comment saying "we
    /// can commit the new state on the software side now". So this field
    /// reads as the new framebuffer the moment `PAGE_FLIP` returns, whatever
    /// the hardware is scanning out.
    ///
    /// What it does prove is worth keeping anyway: the CRTC is the one that
    /// was asked, it is still enabled, it is at the size that was asked for,
    /// and the framebuffer it is committed to is this flip's rather than the
    /// modeset's. The claim that the frame REACHED the screen rests on the
    /// completion event, which the kernel sends from the vblank handler, and
    /// not on this.
    pub fn verify(&self, card: &File, wanted: &sys::DrmModeInfo) -> Result<(), String> {
        let live = sys::drm_get_crtc(card, self.crtc_id)?;
        crtc_shows(&live, self.crtc_id, self.fb_id, wanted)
    }

    /// The frame this completion belongs to.
    ///
    /// The one place a `u64` from the kernel becomes a `FrameId`, which is
    /// what `FrameId`'s own doc claims and what an earlier revision left
    /// unenforced: `from_cookie` had no production caller at all, so the
    /// stated single-conversion-point discipline was a comment rather than a
    /// property.
    pub fn frame(&self) -> crate::output::FrameId {
        crate::output::FrameId::from_cookie(self.completion.user_data)
    }

    /// One line, for a proof to match and a person to read.
    ///
    /// The framebuffer field is `flipfb=` rather than `fb=`, because
    /// `Modeset::describe` already emits `fb=` on the same line and two fields
    /// of one name in one whitespace-split report are read by whichever comes
    /// first. The same collision `Modeset::describe` avoids by not emitting
    /// `crtc=`, one increment later and one field along.
    pub fn describe(&self) -> String {
        format!(
            "flipfb={} cookie={:#x} seq={} flip=ok",
            self.fb_id,
            self.frame().cookie(),
            self.completion.sequence
        )
    }
}

/// Wait for the flip tagged `cookie`, or say why it did not arrive.
///
/// The descriptor is made non-blocking for the duration and its prior status
/// word restored afterwards, including on the error paths — the card outlives
/// this call and a retained `O_NONBLOCK` would change how every later read
/// behaves.
///
/// Events that are not this flip's completion are SKIPPED rather than
/// refused. A vblank event, or a completion carrying some other cookie,
/// means the kernel had something else to say first; it is not evidence that
/// this flip failed, and treating it as such would make the probe fail on a
/// card that merely reported more than one thing.
fn await_flip(card: &File, cookie: u64) -> Result<sys::DrmFlipCompletion, String> {
    let saved_flags = sys::make_nonblocking(card)?;
    let outcome = read_until_flip(card, cookie);
    // Restored before the result is examined, so an error path cannot leave
    // the descriptor non-blocking.
    let restored = sys::restore_status_flags(card, saved_flags);
    // Both are reported when both fail. An earlier revision wrote
    // `outcome?; restored?;`, which DISCARDED the restore failure whenever the
    // flip had also failed -- and a card left non-blocking is the more
    // consequential of the two, because it changes how every later read on
    // this descriptor behaves.
    match (outcome, restored) {
        (Ok(completion), Ok(())) => Ok(completion),
        (Err(flip), Ok(())) => Err(flip),
        (Ok(_), Err(restore)) => Err(restore),
        (Err(flip), Err(restore)) => Err(format!(
            "{flip}; and the card's status flags could not be restored afterwards: {restore}"
        )),
    }
}

fn read_until_flip(card: &File, cookie: u64) -> Result<sys::DrmFlipCompletion, String> {
    // `checked_add` rather than `+`, which panics on overflow. Unreachable on
    // `CLOCK_MONOTONIC`, but every other deadline in this crate is written this
    // way and a new production `panic!` is what the rule forbids.
    let deadline = Instant::now().checked_add(FLIP_TIMEOUT);
    let mut pending: Vec<u8> = Vec::with_capacity(sys::DRM_EVENT_VBLANK_LEN * 4);
    // 4 KiB because the kernel says so: `drm_read` will not split an event
    // across reads, and if the next one does not fit in the buffer it is put
    // BACK on the queue and the read answers zero (`drm_file.c:581`). The
    // documentation's recommendation is a page, and a smaller buffer takes on
    // a forward-progress contract this code has no reason to accept.
    let mut chunk = [0u8; 4096];
    // Set when the deadline passes, and the loop then gets exactly ONE more
    // read before it gives up. Without it the sequence "read answers
    // WouldBlock, sleep, deadline passes, report timeout" never looks at the
    // descriptor again — so a completion delivered DURING that sleep is
    // sitting there unread while this reports that none arrived. On a loaded
    // TCG guest that is a false rejection of a flip that worked, on a row
    // every agent's boot runs.
    let mut expired = false;
    loop {
        // Parse everything already buffered before reading more: one read can
        // deliver several events, and a completion sitting behind a vblank in
        // the same read must not wait for another read to be noticed.
        let mut consumed = 0usize;
        while let Some((length, completion)) =
            sys::parse_drm_event(pending.get(consumed..).unwrap_or_default())
        {
            consumed = consumed.saturating_add(length);
            if let Some(completion) = completion {
                if completion.user_data == cookie {
                    return Ok(completion);
                }
            }
        }
        pending.drain(..consumed.min(pending.len()));
        // Only after the final drain above has had its chance to parse.
        if expired {
            return Err(format!(
                "the page flip tagged {cookie:#x} was accepted by the kernel but no completion \
                 arrived within {FLIP_TIMEOUT:?} — the flip was queued and the CRTC never \
                 reported it reaching the screen"
            ));
        }
        expired = deadline.is_none_or(|deadline| Instant::now() >= deadline);
        match (&*card).read(&mut chunk) {
            // NOT end-of-file, which an earlier revision called it. `drm_read`
            // answers zero when the next event does not fit in the buffer it
            // was given, having put that event back on the queue
            // (`drm_file.c:581`). With a page-sized buffer no in-tree DRM
            // event can provoke it, so reaching here means the kernel grew an
            // event larger than a page and this loop would spin forever
            // re-reading it. Refused with the reason rather than retried.
            Ok(0) => {
                return Err(
                    "the DRM card returned a short read while waiting for a flip completion: \
                     the next event does not fit in a 4 KiB buffer, so it was put back and \
                     re-reading it would not make progress"
                        .to_string(),
                )
            }
            Ok(read) => pending.extend_from_slice(chunk.get(..read).unwrap_or_default()),
            // No sleep once expired: the next pass reports the timeout, and
            // sleeping first would only delay saying so.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if !expired {
                    std::thread::sleep(FLIP_POLL_INTERVAL);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("read a flip completion from the card: {error}")),
        }
    }
}

/// Was this frame allocated at the size the mode is about to be set to?
///
/// Checked HERE rather than left to the kernel. `ADDFB2` is told a width, a
/// height, a pitch and a handle as four bare numbers and validates them against
/// the GEM object's size, so a frame allocated at some other size comes back as
/// "object too small" — an error about a handle, raised inside the kernel, for
/// a mistake that is visible right here as two sizes that disagree. Split out
/// for `crtc_shows`'s reason: the refusal is then testable without a card.
///
/// `probe_kms` allocates from the same mode it sets, so nothing reaches this
/// today. It is a precondition of `apply`, not of that one caller, and the
/// backend §M row 1 still owes will have a buffer pool that outlives any single
/// mode.
fn frame_fits_mode(
    frame_width: u32,
    frame_height: u32,
    mode_width: u32,
    mode_height: u32,
) -> Result<(), String> {
    if frame_width != mode_width || frame_height != mode_height {
        return Err(format!(
            "the frame is {frame_width}x{frame_height} but the mode to be set is \
             {mode_width}x{mode_height} — a framebuffer registered at the mode's size would \
             describe rows this buffer does not have"
        ));
    }
    Ok(())
}

/// Can this saved state be sent back as-is?
///
/// Split out of `capture` so the three shapes the kernel refuses are testable
/// against recorded CRTC states rather than against a card, for the reason
/// `crtc_shows` is split out of `verify`. All three have to hold at once: a
/// mode to set, a framebuffer to set it onto, and at least one connector to
/// send it to. Any one missing and `SETCRTC` answers `-EINVAL` or `-ENOENT`
/// rather than restoring anything.
fn is_restorable(saved: &sys::DrmModeCrtc, connectors: &[u32]) -> bool {
    saved.mode_valid != 0 && saved.fb_id != 0 && !connectors.is_empty()
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

/// One modeset: a framebuffer registered, a CRTC driving it, and mastership
/// held for as long as both are true.
///
/// Release order is declaration order again, and the reason is narrower than
/// it first looks. Only ONE of the three steps needs mastership: `SETCRTC`
/// carries `DRM_MASTER` in the kernel's ioctl table, while `GETCRTC`, `ADDFB2`
/// and `RMFB` carry no flag at all (`drm_ioctl.c:674`, `675`, `691`, `692`).
/// So the load-bearing part is that the restore -- which is a `SETCRTC` --
/// happens while mastership is still held, and mastership is therefore
/// released last.
///
/// Unregistering first would not FAIL, and an earlier revision of this comment
/// said it would. `drm_framebuffer_remove` scans the CRTCs and planes using a
/// framebuffer and disables them, because "drm ABI mandates that we remove any
/// deleted framebuffers from active usage" (`drm_framebuffer.c:1157`). The
/// consequence of the wrong order is a CRTC the kernel blanked and this code
/// then turned back on -- a flicker rather than an error, which is a weaker
/// reason to keep the order but still a reason.
///
/// Written as three fields with no `Drop` on this type, for the reason
/// `DumbFrame` records: a type's own destructor runs before its fields, so
/// putting the sequence there would run it in exactly the wrong place.
pub struct Modeset<'card, 'frame> {
    /// None of these three is ever READ, and that is what they are for: each
    /// exists so its `Drop` runs, and the order they run in is the order they
    /// are declared. Annotated individually rather than with one allowance on
    /// the type, so a fourth field has to say for itself why nothing reads it.
    #[allow(dead_code)]
    restore: CrtcRestore<'card>,
    #[allow(dead_code)]
    fb: FbGuard<'card>,
    #[allow(dead_code)]
    master: MasterGuard<'card>,
    /// The fourth field, saying for itself. Nothing reads it and it has no
    /// destructor; it is here so the COMPILER refuses a modeset that outlives
    /// the buffer it is scanning out. Without it `Modeset` borrows only the
    /// card, so dropping the frame first releases the GEM handle and unmaps
    /// the memory while the framebuffer is still registered and still on
    /// screen. The kernel survives that — a framebuffer holds its own
    /// reference to the object — but it is the one place the module's
    /// "released together and in that order" discipline stopped at a comment,
    /// and a borrow is cheaper than a comment.
    #[allow(dead_code)]
    frame: &'frame DumbFrame<'card>,
    fb_id: u32,
    crtc_id: u32,
}

impl<'card, 'frame> Modeset<'card, 'frame> {
    /// Take mastership, register `frame` as a framebuffer, and drive the
    /// scanout's connector from its CRTC.
    ///
    /// Each guard is built BEFORE the step that needs undoing can fail, so an
    /// early return unwinds exactly what was done: `?` after `drm_set_master`
    /// drops the master guard, `?` after `drm_add_fb` drops the framebuffer
    /// too, and so on. That is the same discipline `DumbFrame::allocate` uses
    /// and the reason neither needs a cleanup path written by hand.
    pub fn apply(
        card: &'card File,
        scanout: &Scanout,
        frame: &'frame DumbFrame<'card>,
    ) -> Result<Modeset<'card, 'frame>, String> {
        let width = u32::from(scanout.mode.hdisplay);
        let height = u32::from(scanout.mode.vdisplay);
        frame_fits_mode(frame.width(), frame.height(), width, height)?;
        sys::drm_set_master(card)
            .map_err(|error| format!("take DRM mastership to modeset: {error}"))?;
        let master = MasterGuard { card };
        let saved = sys::drm_get_crtc(card, scanout.crtc_id)?;
        // Read before anything is changed, and under the mastership just
        // taken: this is the routing the restore has to put back.
        let routed = connectors_on_crtc(card, scanout.crtc_id)?;
        let fb_id = sys::drm_add_fb(card, width, height, frame.pitch(), frame.handle())?;
        let fb = FbGuard { card, fb_id };
        let restore = CrtcRestore::capture(card, saved, routed);
        let mut connectors = vec![scanout.connector_id];
        let mut wanted = sys::DrmModeCrtc::empty();
        wanted.crtc_id = scanout.crtc_id;
        wanted.fb_id = fb_id;
        wanted.mode_valid = 1;
        wanted.mode = scanout.mode;
        sys::drm_set_crtc(card, &wanted, &mut connectors)?;
        Ok(Modeset {
            restore,
            fb,
            master,
            frame,
            fb_id,
            crtc_id: scanout.crtc_id,
        })
    }

    /// The CRTC this modeset is driving, for a flip to name.
    pub fn crtc_id(&self) -> u32 {
        self.crtc_id
    }

    /// Ask the CRTC what it is actually doing, and refuse anything but what was
    /// asked for.
    ///
    /// `SETCRTC` returning success is not the same claim. The kernel validates
    /// and can land on a different mode than the one requested, and a driver
    /// that quietly kept the previous framebuffer would report success while
    /// showing the old picture. Reading the CRTC back is the only statement
    /// about what is on the screen that this process can make without a camera.
    pub fn verify(&self, card: &File, wanted: &sys::DrmModeInfo) -> Result<(), String> {
        let live = sys::drm_get_crtc(card, self.crtc_id)?;
        crtc_shows(&live, self.crtc_id, self.fb_id, wanted)
    }

    /// One line, for a proof to match and a person to read.
    ///
    /// Deliberately does NOT repeat `crtc=`: `Discovery::describe` already
    /// emits that field, and a second one in the same whitespace-split line
    /// would be read by whichever `strip_prefix` ran first. They carry the same
    /// number today -- `apply` modesets the CRTC discovery chose -- so the
    /// duplicate would have been invisible until the day they differed, which
    /// is exactly when the check would need to be right.
    pub fn describe(&self) -> String {
        format!("fb={} modeset=ok", self.fb_id)
    }
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
/// the plain `open` above IS the acquisition.
///
/// What holding it costs is not what an earlier revision of this comment
/// claimed. The fbdev console keeps painting under a foreign master — the
/// vblank wait answers `-EBUSY` and `drm_fb_helper_fb_dirty` discards it
/// (`drm_fb_helper.c:237`, `:249`) — so the screen does NOT go stale. What
/// does happen is that no other process can become master while this
/// descriptor is one (`drm_auth.c:260`), which on a machine running a real DRM
/// compositor is the disturbance that matters. Dropping it here closes a
/// window measured in the whole length of the probe down to the two syscalls
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
    /// there. Nothing else in the probe would notice.
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

    /// A frame allocated at the mode's size is the only one that may be
    /// registered at it.
    #[test]
    fn a_frame_the_size_of_its_mode_is_accepted() {
        assert_eq!(frame_fits_mode(1280, 800, 1280, 800), Ok(()));
    }

    /// Either dimension is enough, and the two are checked separately rather
    /// than by area: 1280x800 and 800x1280 hold the same pixels and describe
    /// different rows.
    #[test]
    fn a_frame_that_is_not_the_size_of_its_mode_is_refused() {
        let error = frame_fits_mode(1024, 768, 1280, 800).unwrap_err();
        assert!(error.contains("1024x768"), "{error}");
        assert!(error.contains("1280x800"), "{error}");
        assert!(frame_fits_mode(800, 1280, 1280, 800).is_err());
        assert!(frame_fits_mode(1280, 768, 1280, 800).is_err());
        assert!(frame_fits_mode(1024, 800, 1280, 800).is_err());
    }

    /// A CRTC that was lit, onto a real framebuffer, with somewhere to send
    /// it: the only shape that can be replayed as it stands.
    #[test]
    fn a_lit_crtc_is_restored_as_it_was() {
        assert!(is_restorable(&live_crtc(7, 1280, 800, 1), &[31]));
    }

    /// A CRTC that was already off. Replaying it means switching it off, which
    /// `capture` expresses as the empty request rather than as this one.
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
