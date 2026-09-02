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
    /// The mode or connection changed; `dimensions` must be read again, and
    /// so must every size derived from them.
    #[allow(dead_code)]
    Changed,
}

/// Scanout, behind one interface.
pub trait OutputBackend {
    /// The output's size in pixels.
    fn dimensions(&self) -> OutputDimensions;

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
    /// - `Changed` invalidates every size derived from `dimensions`, not just
    ///   the damage: the shadow copy, the frame storage and the layout.
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
}
