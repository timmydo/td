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

use crate::scene::Scene;

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

/// What `present` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submission {
    /// The pixels are on glass. fbdev's write returned and there is no later
    /// moment for it to report.
    Presented,
    /// Submitted, and not yet visible. Completion arrives later as
    /// `OutputEvent::Presented` — a KMS page flip. No caller may read this as
    /// pixels on glass, which is the whole reason the two are distinguished
    /// before either backend that needs the distinction exists.
    // Constructed by the KMS backend §M plans, not by fbdev. Named now so
    // that `paint`'s contract is submit from the start rather than being
    // widened later, under callers written against the narrower one.
    #[allow(dead_code)]
    Queued,
}

/// Something the backend originates rather than something a caller asked for.
///
/// Nothing consumes these yet, and the allowances below are per-variant so a
/// third one has to justify itself rather than inheriting an exemption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputEvent {
    /// A submission that answered `Queued` reached the screen.
    #[allow(dead_code)]
    Presented,
    /// The mode or connection changed; `output()` must be read again, and
    /// every value computed from a previous one is stale. Not only the
    /// sizes: the scale and the transform are `output()`'s own fields and
    /// change with it.
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

    /// Prepare a frame and hand back the bytes to render into.
    fn begin_frame(&mut self, damage: Damage) -> Result<FrameTarget<'_>, String>;

    /// SUBMIT the frame prepared by the last `begin_frame`. Not "the pixels
    /// are now visible": the return value says which of the two happened.
    fn present(&mut self) -> Result<Submission, String>;

    /// Backend-originated events since the last call, appended to `events`.
    ///
    /// An out-parameter rather than a returned `Vec` so the caller can keep
    /// one buffer across frames: a KMS backend reports a flip every frame,
    /// and this is the frame loop.
    ///
    /// NOTHING CALLS THIS YET, deliberately. It is on the trait because §M
    /// names it and because a backend's events must have somewhere to go, but
    /// the delivery path is not the frame loop's to invent, and an earlier
    /// draft of this commit got it wrong in a way three reviewers caught:
    ///
    /// - a page flip arrives on the card descriptor asynchronously, so
    ///   draining only from a repaint means an idle screen never observes
    ///   one — the client waits for a frame callback that waits for a flip
    ///   completion that waits for a repaint that waits for the client. The
    ///   descriptor has to join the event loop.
    /// - neither `Submission` nor `OutputEvent` carries a frame identity, so
    ///   a completion drained after submitting frame N cannot be told apart
    ///   from N-1's. Pairing them needs an identity that does not exist yet.
    /// - `Changed` invalidates everything computed from a previous
    ///   `output()`, not just the damage: the shadow copy, the frame storage
    ///   and the layout. The scale and the transform change with it too, and
    ///   neither is a size.
    ///
    /// Writing those down is the point; guessing at the response is what §M
    /// calls painting into the corner.
    #[allow(dead_code)]
    fn poll_events(&mut self, events: &mut Vec<OutputEvent>) -> Result<(), String>;

    /// Render `scene` into this backend's target and submit it.
    ///
    /// Named `paint` because that is what every caller has always called it,
    /// and returning `Submission` because submit is all it can honestly
    /// promise.
    fn paint(&mut self, scene: &Scene, damage: Damage) -> Result<Submission, String> {
        let target = self.begin_frame(damage)?;
        scene.render(target.pixels, target.width, target.height, target.stride);
        self.present()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
