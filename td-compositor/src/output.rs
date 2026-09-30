//! Where a frame goes.
//!
//! `Framebuffer` was the only answer, and three of its properties had become
//! the renderer's: one implicit output, a `write(2)`-able device, and a
//! `paint` that returned when the pixels were on glass. None survives KMS
//! (`APPLICATIONS.md` §M), and the third is the expensive one to discover
//! late — a caller that reads "paint returned" as "the frame is visible" is
//! correct against fbdev and wrong against a page flip, and nothing in the
//! type would have said so.
//!
//! So scanout sits behind `OutputBackend`, and `paint` is defined as SUBMIT.
//! `Submission` is how a backend says which of the two it did.

use crate::scene::{Scene, SurfaceKey};

/// A format a backend can put on glass, as a DRM fourcc.
///
/// Deliberately NOT `wl_shm`'s enumerant namespace, where XRGB8888 is 1 and
/// ARGB8888 is 0. Those describe what a client may hand td to COPY; these
/// describe what hardware may scan out, and they are the numbers KMS and
/// `zwp_linux_dmabuf_v1` both speak. The moment a client can name a format td
/// did not copy, the two namespaces have to be separate types or one of them
/// is silently reinterpreted as the other — which is the same mistake
/// `buffer.rs`'s `pixel_is_opaque` exists to prevent one layer down.
///
/// A newtype rather than an alias, because an alias would provide exactly no
/// separation: `SHM_XRGB8888` is a `u32` too, and the compiler would accept
/// one wherever the other belongs — the mix-up this type exists to name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fourcc(u32);

impl Fourcc {
    /// The four characters, in the order the code names them.
    pub fn code(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
}

/// `DRM_FORMAT_XRGB8888` — `fourcc_code('X', 'R', '2', '4')`.
pub const DRM_FORMAT_XRGB8888: Fourcc = Fourcc(0x3432_5258);

/// What the caller knows about what changed since the last frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Damage {
    /// The caller does not know. A backend that can discover it cheaply may —
    /// fbdev compares its own shadow copy — and one that cannot must treat
    /// this as the whole output rather than as nothing.
    Unknown,
    /// Everything changed, or what the backend believes the device holds
    /// cannot be trusted. A tiling command is the caller's reason: it is what
    /// an operator reaches for when the screen looks wrong, so it repairs
    /// pixels the compositor did not write and its shadow copy cannot see.
    Whole,
}

/// One output's dimensions in pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputDimensions {
    pub width: usize,
    pub height: usize,
}

/// Which output. One exists, and that is why it gets a name now: "the
/// output" is a fact about this code's shape rather than about the machine,
/// and a KMS backend enumerates connectors.
///
/// A newtype rather than a bare `u32` because the number a client sees is a
/// DIFFERENT one: `wl_output` is bound per client and carries that client's
/// object id, so a server-side output identity and a protocol object id are
/// two namespaces that would otherwise both be `u32`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputId(u32);

impl OutputId {
    /// The first output. Named rather than written as a literal at the one
    /// construction site, so a second output is a value and not an edit.
    pub const FIRST: OutputId = OutputId(1);

    /// Any output. A backend enumerating connectors names them with this;
    /// without it `FIRST` would be the only id that exists and a second
    /// output would mean editing this file, which is what the constant above
    /// claims it does not.
    #[allow(dead_code)]
    pub const fn new(id: u32) -> OutputId {
        OutputId(id)
    }

    /// The number, for a name a client can read.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// `wl_output.scale`: how many device pixels one logical pixel occupies.
///
/// Integer, because that is what `wl_output` carries; fractional scaling is
/// `wp_fractional_scale_v1` and a separate decision. Zero is refused at
/// construction rather than guarded at every division.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputScale(u32);

impl OutputScale {
    /// One device pixel per logical pixel, which is td's only scale today.
    pub const ONE: OutputScale = OutputScale(1);

    /// `None` for zero, which would make a logical size meaningless rather
    /// than merely wrong, and `None` beyond `i32::MAX`, which `wl_output`
    /// cannot carry — a scale that no client can be told about would fail
    /// every bind rather than the one construction that was wrong.
    ///
    /// Nothing in the software phase constructs a scale other than `ONE`;
    /// what will is a backend reading a connector's scale, and the checked
    /// constructor is how it will do that. Kept rather than deferred because
    /// the alternative is a public field and the invariant that the divisor
    /// is non-zero, which `logical_dimensions` relies on, would then be
    /// nobody's.
    #[allow(dead_code)]
    pub fn new(factor: u32) -> Option<OutputScale> {
        match factor {
            0 => None,
            factor if factor > i32::MAX as u32 => None,
            factor => Some(OutputScale(factor)),
        }
    }

    /// The factor, for the wire and for dividing a pixel size by.
    pub fn factor(self) -> u32 {
        self.0
    }
}

/// How the output's contents sit relative to its native scanout.
///
/// The eight `wl_output.transform` values, complete because the protocol
/// enumerant is what goes on the wire and a partial set would have to encode
/// something it does not name. td drives `Normal` today; the others exist so
/// a rotated connector is a VALUE a backend reports rather than a case the
/// wire encoder has to invent.
///
/// Only `Normal` is constructed in the software phase — fbdev has no
/// connector to ask — so the seven below are dead until a KMS backend reads
/// one. The alternative to carrying them is `to_wl` taking a raw `u32` nobody
/// checked, which is the mix-up `Fourcc` above exists to refuse one layer up.
/// No `Default`: a transform nobody chose is exactly what this row removes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputTransform {
    Normal,
    #[allow(dead_code)]
    Rotate90,
    #[allow(dead_code)]
    Rotate180,
    #[allow(dead_code)]
    Rotate270,
    #[allow(dead_code)]
    Flipped,
    #[allow(dead_code)]
    FlippedRotate90,
    #[allow(dead_code)]
    FlippedRotate180,
    #[allow(dead_code)]
    FlippedRotate270,
}

impl OutputTransform {
    /// The `wl_output.transform` enumerant.
    pub fn to_wl(self) -> u32 {
        match self {
            OutputTransform::Normal => 0,
            OutputTransform::Rotate90 => 1,
            OutputTransform::Rotate180 => 2,
            OutputTransform::Rotate270 => 3,
            OutputTransform::Flipped => 4,
            OutputTransform::FlippedRotate90 => 5,
            OutputTransform::FlippedRotate180 => 6,
            OutputTransform::FlippedRotate270 => 7,
        }
    }

    /// Whether this transform exchanges the two axes, which is the whole of
    /// what a transform means to a LAYOUT: a quarter turn makes a landscape
    /// output portrait, and every size derived from it swaps with it.
    ///
    /// Dead for the same reason as `logical_dimensions`, its only caller.
    #[allow(dead_code)]
    pub fn exchanges_axes(self) -> bool {
        match self {
            OutputTransform::Rotate90
            | OutputTransform::Rotate270
            | OutputTransform::FlippedRotate90
            | OutputTransform::FlippedRotate270 => true,
            OutputTransform::Normal
            | OutputTransform::Rotate180
            | OutputTransform::Flipped
            | OutputTransform::FlippedRotate180 => false,
        }
    }
}

/// One output, named — `APPLICATIONS.md` §M's sixth row.
///
/// `dimensions` is the SCANOUT size in device pixels, which is what a backend
/// allocates and what `wl_output.mode` carries. `logical_dimensions` is what
/// a layout places windows in. They are equal today, at `Normal` and scale 1,
/// and the reason to separate them before they differ is that every caller
/// currently reads one number and means whichever of the two it happens to
/// need.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Output {
    pub id: OutputId,
    pub dimensions: OutputDimensions,
    pub scale: OutputScale,
    pub transform: OutputTransform,
}

impl Output {
    /// The size a layout works in: scanout pixels, axes exchanged if the
    /// transform turns the output, divided by the scale.
    ///
    /// NOTHING CONSUMES THIS YET, and that is the honest state rather than an
    /// oversight. Consuming a logical size requires a renderer that scales and
    /// rotates: td composites into a target of SCANOUT dimensions with no
    /// transform anywhere, so a layout in logical pixels would be painted
    /// small into a corner, and a reported rotation would tell clients td
    /// turns a picture it does not turn. Layout, input and rendering are all
    /// in scanout pixels today and agree only because the one backend reports
    /// `Normal` at scale 1. This function is the definition they will have to
    /// move to, not a switch that has been thrown. Hence the allowance: it is
    /// a definition waiting for the renderer that can honour it, and writing
    /// it while there is one buffer kind and one backend is cheaper than
    /// deriving it later from callers that have each assumed something.
    ///
    /// Truncating division, and then clamped to at least one pixel per axis.
    /// An output smaller than its own scale is not a configuration td
    /// produces, and a zero-width layout is a worse answer than a
    /// one-pixel one: it would divide by zero somewhere further away from
    /// the cause.
    #[allow(dead_code)]
    pub fn logical_dimensions(&self) -> OutputDimensions {
        let OutputDimensions { width, height } = self.dimensions;
        let (width, height) = match self.transform.exchanges_axes() {
            true => (height, width),
            false => (width, height),
        };
        // At least 1 by construction, so the division below cannot trap.
        let factor = usize::try_from(self.scale.factor()).unwrap_or(usize::MAX);
        OutputDimensions {
            width: (width / factor).max(1),
            height: (height / factor).max(1),
        }
    }
}

/// The bytes a frame is rendered into, with the geometry describing them.
///
/// `stride` is the TARGET's, not the output's: a dumb buffer's pitch is the
/// kernel's to choose and need not be `width * 4`, exactly as fbdev's need
/// not be. Callers get it from here rather than from the output, because it
/// is a property of the memory and not of the screen.
pub struct FrameTarget<'a> {
    pub pixels: &'a mut [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

/// Bytes a backend already holds, read-only, with the geometry describing
/// them. The read side of `FrameTarget`: what the readbacks below compare,
/// so they belong to no one backend.
#[derive(Clone, Copy)]
pub struct FrameView<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

impl FrameView<'_> {
    /// The view, if its geometry describes its bytes: a non-empty frame, a
    /// pitch that holds a row, and rows that fit. The readbacks slice by
    /// this geometry, so a backend's view is checked rather than trusted.
    fn checked(self) -> Result<Self, String> {
        let row = self.width.checked_mul(4).ok_or("frame view row overflow")?;
        let rows = self
            .stride
            .checked_mul(self.height)
            .ok_or("frame view size overflow")?;
        if self.width == 0 || self.height == 0 || self.stride < row || rows > self.pixels.len() {
            return Err(format!(
                "frame view {}x{} at stride {} does not describe {} bytes",
                self.width,
                self.height,
                self.stride,
                self.pixels.len()
            ));
        }
        Ok(self)
    }
}

/// Which frame — minted when one is queued, compared when it completes.
///
/// This is the identity a completion is matched on. It is NOT invented
/// out of band: `DRM_IOCTL_MODE_PAGE_FLIP` takes a `u64 user_data` and the
/// kernel copies it into the completion event, so the correlation channel is
/// the ABI's and this newtype is what keeps it from being a bare integer that
/// any other number could be passed as.
///
/// A backend mints these; nothing else may. That is why there is no
/// `From<u64>`: a value arriving from the kernel is turned back into one by
/// `from_cookie` only where a completion event is parsed, and a `FrameId`
/// appearing anywhere else came from a backend that queued a frame.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FrameId(u64);

impl FrameId {
    /// The first frame a backend queues.
    ///
    /// One rather than zero, because zero is what an uninitialised `user_data`
    /// reads as. A completion carrying zero should be distinguishable from a
    /// completion for the first frame, and starting at one is the whole cost
    /// of that.
    pub const FIRST: FrameId = FrameId(1);

    /// The next frame's id.
    ///
    /// Wrapping rather than checked: a `u64` at sixty frames a second wraps
    /// after about ten billion years, so the overflow branch could never be
    /// taken and a `Result` here would be a lie the caller has to handle.
    pub const fn next(self) -> FrameId {
        FrameId(self.0.wrapping_add(1))
    }

    /// This id as the `u64` the kernel carries.
    pub fn cookie(self) -> u64 {
        self.0
    }

    /// Rebuild an id from a completion's `user_data`.
    ///
    /// Called only where a completion event has been parsed:
    /// `FlipEvents::next`. That is what makes the paragraph above a property
    /// rather than a wish.
    pub fn from_cookie(cookie: u64) -> FrameId {
        FrameId(cookie)
    }
}

/// What `present` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submission {
    /// The pixels are on glass. fbdev's write returned and there is no later
    /// moment for it to report.
    Presented,
    /// Submitted, and not yet visible. Completion arrives later as
    /// `OutputEvent::Presented` carrying THE SAME `FrameId`. No caller may
    /// read this as pixels on glass, which is the whole reason the two are
    /// distinguished before either backend that needs the distinction exists.
    ///
    /// The id is what makes the pair usable rather than merely present: a
    /// caller with two frames in flight can tell which completion is which,
    /// instead of assuming completions arrive in the order frames were
    /// queued.
    Queued(FrameId),
}

/// Something the backend originates rather than something a caller asked for.
///
/// The allowance below is per-variant so a third one has to justify itself
/// rather than inheriting an exemption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputEvent {
    /// The submission that answered `Queued` with this id reached the screen.
    Presented(FrameId),
    /// The mode or connection changed; `output()` must be read again, and
    /// every value computed from a previous one is stale. Not only the
    /// sizes: the scale and the transform are `output()`'s own fields and
    /// change with it. Nothing originates one yet: the KMS backend drives
    /// the mode it found at start and does not watch for hotplug.
    #[allow(dead_code)]
    Changed,
}

/// Scanout, behind one interface.
pub trait OutputBackend {
    /// This output, named: id, scanout size, scale and transform.
    ///
    /// Not defaulted. A backend that reported `Normal` at scale 1 by
    /// inheriting a default would be claiming something about its connector
    /// that it never checked, which is the answer §M's sixth row exists to
    /// stop being implicit.
    fn output(&self) -> Output;

    /// The output's size in pixels — what this backend scans out, and what a
    /// frame is allocated at.
    ///
    /// A view of `output()` rather than a second thing to implement. Two
    /// independent answers to one question is how a backend ends up
    /// advertising one size and rendering another, and nothing would have
    /// compared them.
    fn dimensions(&self) -> OutputDimensions {
        self.output().dimensions
    }

    /// The formats this backend can scan out. §M's rule is that nothing here
    /// may be advertised to a CLIENT until a linear CPU composition fallback
    /// exists; what a backend can put on glass and what td is willing to
    /// accept from a client are separate questions, and this answers the
    /// first one only.
    fn supported_formats(&self) -> &[Fourcc];

    /// The row pitch the next `begin_frame` target will carry, at least
    /// `width * 4` and so never zero. A property of this backend's memory,
    /// not of the output: a dumb buffer's pitch is the kernel's to choose.
    /// It is asked for ahead of a frame only by what pre-renders one at the
    /// exact target geometry, the trusted prompt.
    fn target_stride(&self) -> usize;

    /// The bytes the device is known to hold, or `None` while that is not
    /// established: before the first submission completes, and across a
    /// failed one, and while a submission is queued. The public capture and
    /// the application observer read only this, and only each row's
    /// visible `width * 4` bytes: row padding is never compared or captured,
    /// so a backend need not keep it in any particular state.
    fn completed(&self) -> Option<FrameView<'_>>;

    /// A test's way back to the concrete backend it built, for the fault
    /// hooks that are that backend's own rather than the trait's.
    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;

    /// Prepare a frame and hand back the bytes to render into.
    fn begin_frame(&mut self, damage: Damage) -> Result<FrameTarget<'_>, String>;

    /// SUBMIT the frame prepared by the last `begin_frame`. Not "the pixels
    /// are now visible": the return value says which of the two happened.
    fn present(&mut self) -> Result<Submission, String>;

    /// The frame this backend answered `Queued(frame)` for reached the
    /// screen. Told, not asked: completions arrive on a descriptor the
    /// backend does not read, from a thread of their own, because a flip
    /// drained only from a repaint is never observed on an idle screen.
    /// A backend that queues nothing refuses every call.
    fn frame_presented(&mut self, frame: FrameId) -> Result<(), String>;

    /// The frame queued as `frame` never completed. Put it on glass some
    /// other way, so the output stops waiting on an event that did not
    /// come; the completion that may still arrive afterwards is stale.
    fn recover_stalled_frame(&mut self, frame: FrameId) -> Result<(), String>;

    /// Render `scene` into this backend's target and submit it.
    ///
    /// Named `paint` because that is what every caller has always called it,
    /// and returning `Submission` because submit is all it can honestly
    /// promise.
    fn paint(&mut self, scene: &Scene, damage: Damage) -> Result<Submission, String> {
        let target = self.begin_frame(damage)?;
        scene.render_display(target.pixels, target.width, target.height, target.stride)?;
        self.present()
    }
}

/// Only completed bytes that match a public-scene render may be captured.
/// In particular, neither a failed write nor private attention pixels can be
/// relabeled as a public frame by the caller. `comparison` is the caller's
/// scratch frame, kept across captures so it is allocated once.
pub(crate) fn completed_public_ppm(
    completed: Option<FrameView<'_>>,
    comparison: &mut Vec<u8>,
    scene: &Scene,
    stamp: crate::headless::OutputStamp,
) -> Result<Vec<u8>, String> {
    let completed = completed
        .ok_or("output completion is not established")?
        .checked()?;
    fit(
        comparison,
        completed.pixels.len(),
        "reserve public capture comparison",
    )?;
    scene.render(
        comparison,
        completed.width,
        completed.height,
        completed.stride,
    );
    // Visible bytes only: the renderer never writes row padding, and what a
    // backend's padding holds is not part of the picture.
    let visible = completed
        .width
        .checked_mul(4)
        .ok_or("capture row overflow")?;
    let rows = comparison
        .chunks_exact(completed.stride)
        .zip(completed.pixels.chunks_exact(completed.stride))
        .take(completed.height);
    for (rendered, held) in rows {
        if rendered.get(..visible) != held.get(..visible) {
            return Err("completed output does not match the public scene".into());
        }
    }
    let pixels = completed
        .width
        .checked_mul(completed.height)
        .and_then(|n| n.checked_mul(3))
        .ok_or("capture byte count overflow")?;
    let header = format!(
        "P6\n# td-output-v1 {}\n{} {}\n255\n",
        stamp.record(),
        completed.width,
        completed.height
    );
    let length = pixels
        .checked_add(header.len())
        .ok_or("capture size overflow")?;
    let mut ppm = Vec::new();
    ppm.try_reserve_exact(length)
        .map_err(|_| "reserve public capture")?;
    ppm.extend_from_slice(header.as_bytes());
    let row_bytes = completed
        .width
        .checked_mul(4)
        .ok_or("capture row overflow")?;
    for row in completed
        .pixels
        .chunks_exact(completed.stride)
        .take(completed.height)
    {
        let row = row.get(..row_bytes).ok_or("capture row is truncated")?;
        for [blue, green, red, _] in row.as_chunks::<4>().0 {
            ppm.extend_from_slice(&[*red, *green, *blue]);
        }
    }
    if ppm.len() != length {
        return Err("capture frame is truncated".into());
    }
    Ok(ppm)
}

/// Exact final-output pixels attributable to `surface`. Re-rendering with
/// only that surface omitted makes hidden, clipped and occluded pixels
/// disappear from the count while leaving its descendants in place.
pub(crate) fn surface_rgb_pixel_counts(
    composed: FrameView<'_>,
    comparison: &mut Vec<u8>,
    scene: &Scene,
    surface: SurfaceKey,
    rgbs: [[u8; 3]; 2],
) -> Result<[usize; 2], String> {
    if scene.attention_visible() {
        return Ok([0; 2]);
    }
    let composed = composed.checked()?;
    fit(
        comparison,
        composed.pixels.len(),
        "reserve application comparison frame",
    )?;
    scene.render_omitting(
        comparison,
        composed.width,
        composed.height,
        composed.stride,
        Some(surface),
    );
    Ok(attributed_rgb_counts(composed, comparison, rgbs))
}

/// Resize the caller's scratch frame to exactly `length` without a
/// panicking allocation. It changes size only when the frame does.
fn fit(scratch: &mut Vec<u8>, length: usize, what: &str) -> Result<(), String> {
    if scratch.len() != length {
        let additional = length.saturating_sub(scratch.len());
        scratch
            .try_reserve_exact(additional)
            .map_err(|_| what.to_string())?;
        scratch.resize(length, 0);
    }
    Ok(())
}

pub(crate) fn attributed_rgb_counts(
    rendered: FrameView<'_>,
    omitted: &[u8],
    rgbs: [[u8; 3]; 2],
) -> [usize; 2] {
    let mut counts = [0usize; 2];
    if rendered.stride == 0 {
        return counts;
    }
    let visible = rendered.width.saturating_mul(4);
    for (rendered_row, omitted_row) in rendered
        .pixels
        .chunks(rendered.stride)
        .zip(omitted.chunks(rendered.stride))
        .take(rendered.height)
    {
        let (Some(rendered_pixels), Some(omitted_pixels)) =
            (rendered_row.get(..visible), omitted_row.get(..visible))
        else {
            return [0; 2];
        };
        for (pixel, without) in rendered_pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(omitted_pixels.as_chunks::<4>().0)
        {
            if pixel == without {
                continue;
            }
            let [blue, green, red, _] = pixel;
            let rgb = [*red, *green, *blue];
            for (index, expected) in rgbs.iter().enumerate() {
                if rgb == *expected {
                    if let Some(count) = counts.get_mut(index) {
                        *count = count.saturating_add(1);
                    }
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_color_counts_exclude_stride_padding() {
        let rendered = [
            0xff, 0x00, 0xff, 0, 0x00, 0xff, 0x00, 0, 0x00, 0xff, 0x00, 0,
        ];
        let mut omitted = [0u8; 12];
        let colors = [[0xff, 0x00, 0xff], [0x00, 0xff, 0x00]];
        let view = FrameView {
            pixels: &rendered,
            width: 2,
            height: 1,
            stride: 12,
        };
        assert_eq!(attributed_rgb_counts(view, &omitted, colors), [1, 1]);
        omitted[4..8].copy_from_slice(&rendered[4..8]);
        assert_eq!(attributed_rgb_counts(view, &omitted, colors), [1, 0]);
    }

    const STAMP: crate::headless::OutputStamp = crate::headless::OutputStamp {
        session: 7,
        output: 1,
    };

    #[test]
    fn a_view_whose_geometry_does_not_describe_its_bytes_is_refused_not_sliced() {
        let bytes = [0u8; 64];
        let view = |width, height, stride| FrameView {
            pixels: &bytes,
            width,
            height,
            stride,
        };
        for (width, height, stride) in [(0, 4, 16), (4, 0, 16), (4, 4, 0), (4, 4, 12), (4, 5, 16)] {
            let bad = view(width, height, stride);
            assert!(
                completed_public_ppm(Some(bad), &mut Vec::new(), &Scene::new(), STAMP).is_err()
            );
            let key = SurfaceKey {
                client: 1,
                object: 1,
            };
            let counts =
                surface_rgb_pixel_counts(bad, &mut Vec::new(), &Scene::new(), key, [[0; 3]; 2]);
            assert!(counts.is_err());
        }
        assert!(view(4, 4, 16).checked().is_ok());
        assert_eq!(
            attributed_rgb_counts(view(4, 4, 0), &bytes, [[0; 3]; 2]),
            [0; 2]
        );
    }

    /// Row padding is not the picture: a backend whose padding holds
    /// anything still captures, and a changed visible byte still refuses.
    #[test]
    fn a_public_capture_compares_visible_bytes_and_ignores_row_padding() {
        let (width, height, stride) = (4, 3, 24);
        let scene = Scene::new();
        let mut clean = vec![0u8; stride * height];
        scene.render(&mut clean, width, height, stride);
        let mut padded = clean.clone();
        for row in padded.chunks_exact_mut(stride) {
            row[width * 4..].fill(0xaa);
        }
        let capture = |pixels: &[u8]| {
            let view = FrameView {
                pixels,
                width,
                height,
                stride,
            };
            completed_public_ppm(Some(view), &mut Vec::new(), &scene, STAMP)
        };
        let expected = capture(&clean).unwrap();
        assert_eq!(capture(&padded).unwrap(), expected);
        padded[stride + 1] ^= 0xff;
        assert!(capture(&padded).is_err());
    }

    #[test]
    fn a_mismatched_trusted_raster_never_reaches_present() {
        use crate::authority::consent::{Operation, Request, Role};
        struct Backend {
            frame: Vec<u8>,
            width: usize,
            presents: usize,
        }
        impl OutputBackend for Backend {
            fn output(&self) -> Output {
                Output {
                    id: OutputId::FIRST,
                    dimensions: OutputDimensions {
                        width: 800,
                        height: 600,
                    },
                    scale: OutputScale::ONE,
                    transform: OutputTransform::Normal,
                }
            }
            fn supported_formats(&self) -> &[Fourcc] {
                &[DRM_FORMAT_XRGB8888]
            }
            fn begin_frame(&mut self, _: Damage) -> Result<FrameTarget<'_>, String> {
                Ok(FrameTarget {
                    pixels: &mut self.frame,
                    width: self.width,
                    height: 600,
                    stride: 3200,
                })
            }
            fn target_stride(&self) -> usize {
                3200
            }
            fn completed(&self) -> Option<FrameView<'_>> {
                None
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
            fn present(&mut self) -> Result<Submission, String> {
                self.presents += 1;
                Ok(Submission::Presented)
            }
            fn frame_presented(&mut self, _: FrameId) -> Result<(), String> {
                Err("this backend queues nothing".into())
            }
            fn recover_stalled_frame(&mut self, _: FrameId) -> Result<(), String> {
                Err("this backend queues nothing".into())
            }
        }
        let request = Request::new(
            [1; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap();
        let mut scene = Scene::new();
        scene.set_attention(true);
        scene
            .prepare_attention_request(request.clone(), 800, 600, 3200)
            .unwrap();
        for (width, length) in [
            (799, 3200 * 600),
            (800, 3200 * 600 + 4096),
            (800, 3200 * 600 - 1),
        ] {
            let mut backend = Backend {
                frame: vec![0; length],
                width,
                presents: 0,
            };
            assert!(backend.paint(&scene, Damage::Whole).is_err());
            assert_eq!(backend.presents, 0);
        }
        let mut backend = Backend {
            frame: vec![0; 3200 * 600],
            width: 800,
            presents: 0,
        };
        assert_eq!(
            backend.paint(&scene, Damage::Whole).unwrap(),
            Submission::Presented
        );
        let prepared = crate::attention::Prepared::new(request, 800, 600, 3200).unwrap();
        let mut expected = vec![0; backend.frame.len()];
        assert!(prepared.paint(&mut expected, 800, 600, 3200));
        assert_eq!(backend.frame, expected);
        assert_eq!(backend.presents, 1);
    }

    /// Zero is what an uninitialised `user_data` reads as, so the first id a
    /// backend mints must not be zero — a completion carrying zero has to be
    /// distinguishable from a completion for the first frame.
    #[test]
    fn the_first_frame_id_is_not_zero() {
        assert_eq!(FrameId::FIRST.cookie(), 1);
    }

    /// Ids advance, and the cookie is what advances with them.
    #[test]
    fn frame_ids_advance_and_round_trip_through_a_cookie() {
        let first = FrameId::FIRST;
        let second = first.next();
        assert_ne!(first, second);
        assert!(second > first);
        assert_eq!(FrameId::from_cookie(second.cookie()), second);
    }

    /// The trait is the seam, so it has to be usable as one. A second backend
    /// that cannot be held behind a `dyn` would make every caller generic
    /// over which one it has, which is the coupling the split removes.
    #[test]
    fn the_backend_is_object_safe() {
        fn _accepts(_: &mut dyn OutputBackend) {}
    }

    #[test]
    fn the_xrgb_fourcc_is_the_four_characters_it_names() {
        assert_eq!(&DRM_FORMAT_XRGB8888.code(), b"XR24");
    }

    fn output_of(transform: OutputTransform, scale: u32) -> Output {
        Output {
            id: OutputId::FIRST,
            dimensions: OutputDimensions {
                width: 1920,
                height: 1080,
            },
            scale: OutputScale::new(scale).expect("test scale is not zero"),
            transform,
        }
    }

    /// The whole of what a transform means to a layout. A landscape screen
    /// turned a quarter is a PORTRAIT one, and every size derived from it has
    /// to turn with it — the half-turn does not, which is why this cannot be
    /// "is the transform Normal".
    #[test]
    fn a_quarter_turn_exchanges_the_layouts_axes_and_a_half_turn_does_not() {
        for turned in [
            OutputTransform::Rotate90,
            OutputTransform::Rotate270,
            OutputTransform::FlippedRotate90,
            OutputTransform::FlippedRotate270,
        ] {
            let logical = output_of(turned, 1).logical_dimensions();
            assert_eq!((logical.width, logical.height), (1080, 1920), "{turned:?}");
        }
        for upright in [
            OutputTransform::Normal,
            OutputTransform::Rotate180,
            OutputTransform::Flipped,
            OutputTransform::FlippedRotate180,
        ] {
            let logical = output_of(upright, 1).logical_dimensions();
            assert_eq!((logical.width, logical.height), (1920, 1080), "{upright:?}");
        }
    }

    /// A scale divides the logical size and leaves the scanout size alone:
    /// they are different questions, and a client is told the pixel mode and
    /// the scale and does the division once itself. td lays out in scanout
    /// pixels and is right to while its one backend reports scale 1 — a
    /// backend that reported otherwise is what would make the two differ.
    #[test]
    fn a_scale_divides_the_logical_size_and_the_scanout_size_is_untouched() {
        let output = output_of(OutputTransform::Normal, 2);
        assert_eq!(output.dimensions.width, 1920);
        let logical = output.logical_dimensions();
        assert_eq!((logical.width, logical.height), (960, 540));
    }

    /// Both at once. Not an ordering claim — exchanging a pair and dividing
    /// each component commute, so there is no order here to get wrong — but
    /// the combination is what a rotated HiDPI panel reports and it is worth
    /// one value.
    #[test]
    fn a_turned_and_scaled_output_gives_both_effects() {
        let logical = output_of(OutputTransform::Rotate90, 2).logical_dimensions();
        assert_eq!((logical.width, logical.height), (540, 960));
    }

    /// A zero-size layout would divide by zero somewhere further from the
    /// cause, so the clamp is here where the reason is legible.
    #[test]
    fn an_output_smaller_than_its_own_scale_still_has_a_pixel() {
        let output = Output {
            id: OutputId::FIRST,
            dimensions: OutputDimensions {
                width: 1,
                height: 1,
            },
            scale: OutputScale::new(4).expect("test scale is not zero"),
            transform: OutputTransform::Normal,
        };
        let logical = output.logical_dimensions();
        assert_eq!((logical.width, logical.height), (1, 1));
    }

    /// These are wire values, so they are pinned as wire values rather than
    /// trusted to the declaration order of an enum somebody may reorder.
    #[test]
    fn the_transform_enumerants_are_the_ones_wl_output_defines() {
        assert_eq!(OutputTransform::Normal.to_wl(), 0);
        assert_eq!(OutputTransform::Rotate90.to_wl(), 1);
        assert_eq!(OutputTransform::Rotate180.to_wl(), 2);
        assert_eq!(OutputTransform::Rotate270.to_wl(), 3);
        assert_eq!(OutputTransform::Flipped.to_wl(), 4);
        assert_eq!(OutputTransform::FlippedRotate90.to_wl(), 5);
        assert_eq!(OutputTransform::FlippedRotate180.to_wl(), 6);
        assert_eq!(OutputTransform::FlippedRotate270.to_wl(), 7);
    }

    /// Refused at construction, so no divide has to ask.
    #[test]
    fn a_zero_scale_is_refused_rather_than_guarded_at_every_division() {
        assert!(OutputScale::new(0).is_none());
        assert_eq!(OutputScale::new(3).map(OutputScale::factor), Some(3));
        assert_eq!(OutputScale::ONE.factor(), 1);
    }
}
