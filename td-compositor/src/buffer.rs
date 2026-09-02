//! What td holds for one committed client image.
//!
//! A surface's contents are a BUFFER, not a pixel array. Today exactly one
//! kind exists — `ShmSnapshot`, the client's bytes copied out of a wl_shm
//! pool — and the hardware-rendering landing `APPLICATIONS.md` §M plans adds
//! `Dmabuf { planes, fourcc, modifier, fences }` beside it, which td can
//! neither read nor free the way it reads and frees a copy.
//!
//! The enum exists while it has one variant because the code around it must
//! stop assuming there is only one. Every accessor here answers by an
//! exhaustive `match`, so adding a variant is a compile error in THIS module
//! at each accessor that has to answer for it. Be exact about the reach of
//! that: consumers see the answer rather than the kind, so it forces the
//! answers to be written, not the callers to be revisited. Where a caller
//! genuinely must change — the byte ceilings, which cannot charge a card's
//! memory in CPU bytes — the seam is the per-buffer-type accounting §M asks
//! for next, and it is not this module's to claim.
//!
//! The one question a dmabuf cannot answer without a mapping — give me linear
//! CPU bytes — says so in its return type rather than by convention.

/// wl_shm's `ARGB8888`, whose alpha is honoured when a surface is composited.
pub const SHM_ARGB8888: u32 = 0;
/// wl_shm's `XRGB8888`, whose fourth byte is ignored.
pub const SHM_XRGB8888: u32 = 1;

/// Client pixels copied into td's own memory: tightly packed 4-byte rows,
/// `width` of them per row and `height` rows, in the format the client's
/// `wl_shm` buffer declared.
///
/// The fields are private and the constructor is fallible because three
/// things about them are INVARIANTS something else already relies on, rather
/// than descriptions of what happens to be there: the packing, a non-zero
/// dimension, and the format being one of the two. `Scene::render` walks
/// `chunks_exact(width * 4)` and takes `height` of them, so a value whose
/// length disagreed would silently draw a short or sheared image — the kind
/// of wrong that looks like a rendering bug for a week — and
/// `chunks_exact(0)` panics outright. `pixel_is_opaque` is total only over
/// the two formats.
///
/// No derives: the old `Surface` had none either, and the three that look
/// free are not. `Clone` would put a whole-window deep copy behind a `.`
/// in a compositor whose ingestion story is that td owns exactly one copy;
/// `Debug` would let a stray `{:?}` print megabytes; `PartialEq` would hide
/// a multi-megabyte `memcmp`. Add one when a caller needs it.
pub struct ShmSnapshot {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    format: u32,
}

impl ShmSnapshot {
    /// The copy the ingestion point produces, checked against its own
    /// geometry and against the format roster. `Err` rather than a clamp: a
    /// caller that miscounted has a bug, and drawing its best guess would
    /// hide it.
    pub fn new(
        width: usize,
        height: usize,
        pixels: Vec<u8>,
        format: u32,
    ) -> Result<ShmSnapshot, String> {
        // A zero dimension is refused, not merely empty. `Scene::render`
        // walks `chunks_exact(width * 4)`, and `chunks_exact(0)` PANICS —
        // today unreachable only because every crop derives its width from
        // the surface's own dimensions and `visible_span` returns `None`
        // first, which is a property of a different function. `create_buffer`
        // already rejects both, so this costs nothing and takes the panicking
        // shape out of the crate rather than leaving it fenced off elsewhere.
        if width == 0 || height == 0 {
            return Err(format!(
                "shm snapshot {width}x{height} has a zero dimension"
            ));
        }
        // The format roster is this type's too, for the same reason as the
        // packing: `pixel_is_opaque` is total only over these two, and a
        // third value would BLEND rather than being drawn opaque as the old
        // renderer did — a silent change of appearance rather than an error.
        // `server.rs` admits no others either, and that check staying there
        // is what makes a client's bad format a protocol error; this one is
        // what makes the invariant hold at every construction site.
        if !matches!(format, SHM_ARGB8888 | SHM_XRGB8888) {
            return Err(format!("shm snapshot format {format} is not XRGB or ARGB"));
        }
        let row = width
            .checked_mul(4)
            .ok_or_else(|| "shm snapshot row overflow".to_string())?;
        let total = row
            .checked_mul(height)
            .ok_or_else(|| "shm snapshot size overflow".to_string())?;
        if pixels.len() != total {
            return Err(format!(
                "shm snapshot {width}x{height} needs {total} bytes, not {}",
                pixels.len()
            ));
        }
        Ok(ShmSnapshot {
            width,
            height,
            pixels,
            format,
        })
    }
}

/// One committed client image, by kind. See the module comment for why this
/// is an enum with a single variant.
pub enum SurfaceBuffer {
    /// Client bytes copied out of a `wl_shm` pool. td owns the copy, so the
    /// client's buffer is released at commit and its lifetime ends there.
    Shm(ShmSnapshot),
}

/// What a surface holds. A newtype over the buffer rather than the buffer
/// itself, so the scene's vocabulary stays "surface" while the thing it holds
/// gains kinds.
pub struct Surface {
    buffer: SurfaceBuffer,
}

impl Surface {
    /// The copied-pixel surface, which is every surface in the software
    /// phase.
    pub fn shm(snapshot: ShmSnapshot) -> Surface {
        Surface {
            buffer: SurfaceBuffer::Shm(snapshot),
        }
    }

    /// The same thing the ingestion point builds, in one step, for callers
    /// that have the four pieces and no snapshot to hand.
    pub fn from_shm_pixels(
        width: usize,
        height: usize,
        pixels: Vec<u8>,
        format: u32,
    ) -> Result<Surface, String> {
        Ok(Surface::shm(ShmSnapshot::new(
            width, height, pixels, format,
        )?))
    }

    pub fn width(&self) -> usize {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => shm.width,
        }
    }

    pub fn height(&self) -> usize {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => shm.height,
        }
    }

    /// The format the client declared, as the number the client named. It is
    /// for diagnostics and for the protocol, which speaks in exactly these
    /// numbers. It is NOT the thing to compare when the question is really
    /// about compositing — see `pixel_is_opaque`.
    pub fn format(&self) -> u32 {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => shm.format,
        }
    }

    /// Whether a pixel carrying this alpha byte is fully opaque.
    ///
    /// The compositing question, asked of the BUFFER rather than answered by
    /// comparing `format()` against `wl_shm` enumerants at the call site.
    /// That distinction is the point: a dmabuf's format is a DRM fourcc, and
    /// a raw comparison against `SHM_ARGB8888` would read a fourcc as a
    /// `wl_shm` number and answer confidently in the wrong namespace — as it
    /// happens, one caller treating every dmabuf as opaque and another
    /// treating none of them so. Here a second variant has to answer or it
    /// does not compile.
    ///
    /// Total by construction: `ShmSnapshot::new` admits only the two formats
    /// a `wl_shm` buffer may declare, so this is not answering for a value it
    /// has not met. Over those two it is exactly the pair of conditions the
    /// renderer and the content scan each asked separately before.
    pub fn pixel_is_opaque(&self, alpha: u8) -> bool {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => {
                shm.format == SHM_XRGB8888 || (shm.format == SHM_ARGB8888 && alpha == u8::MAX)
            }
        }
    }

    /// Bytes of td's OWN memory this surface occupies — what the scene and
    /// the per-client ceilings charge today.
    ///
    /// A single number is the wrong shape the moment a buffer lives on a
    /// card, which is exactly §M's fourth row, and this accessor deliberately
    /// does NOT pre-commit what a second kind would answer here. Answering
    /// zero would make every ceiling that consumes it silently unbounded;
    /// answering the card's bytes would make it a different unit under one
    /// name. The per-buffer-type accounting is where that is decided, and
    /// until it lands this counts one kind because there is one kind.
    pub fn resident_bytes(&self) -> usize {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => shm.pixels.len(),
        }
    }

    /// The pixels, tightly packed, if this kind of buffer has any without a
    /// mapping. `None` is not an error: it is a dmabuf saying that reading it
    /// on the CPU costs an `mmap` plus `DMA_BUF_IOCTL_SYNC`, so a caller that
    /// only wanted to blit must take the other path rather than assume.
    pub fn linear_bytes(&self) -> Option<&[u8]> {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => Some(&shm.pixels),
        }
    }

    /// The same bytes to write into. Test-only: the compositor never edits a
    /// client's image, and a production caller that wanted to would be
    /// changing what the client believes it committed.
    #[cfg(test)]
    pub fn linear_bytes_mut(&mut self) -> Option<&mut [u8]> {
        match &mut self.buffer {
            SurfaceBuffer::Shm(shm) => Some(&mut shm.pixels),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_must_carry_exactly_its_own_geometry() {
        assert!(ShmSnapshot::new(2, 2, vec![0; 16], SHM_XRGB8888).is_ok());
        let short = ShmSnapshot::new(2, 2, vec![0; 12], SHM_XRGB8888);
        assert!(short.is_err(), "a short image was accepted");
        let long = ShmSnapshot::new(2, 2, vec![0; 20], SHM_XRGB8888);
        assert!(long.is_err(), "an oversized image was accepted");
    }

    #[test]
    fn geometry_that_cannot_be_counted_is_refused_rather_than_wrapped() {
        let huge = usize::MAX / 2;
        assert!(ShmSnapshot::new(huge, 1, Vec::new(), SHM_XRGB8888).is_err());
        assert!(ShmSnapshot::new(1, usize::MAX, Vec::new(), SHM_XRGB8888).is_err());
    }

    #[test]
    fn a_zero_dimension_is_refused_because_the_renderer_cannot_walk_it() {
        // `chunks_exact(0)` panics, so a zero-width surface is a panicking
        // shape rather than an empty picture.
        assert!(ShmSnapshot::new(0, 0, Vec::new(), SHM_XRGB8888).is_err());
        assert!(ShmSnapshot::new(0, 4, Vec::new(), SHM_XRGB8888).is_err());
        assert!(ShmSnapshot::new(4, 0, Vec::new(), SHM_XRGB8888).is_err());
        assert!(ShmSnapshot::new(1, 1, vec![0; 4], SHM_XRGB8888).is_ok());
    }

    #[test]
    fn a_format_the_opacity_rule_cannot_answer_for_is_refused() {
        assert!(ShmSnapshot::new(1, 1, vec![0; 4], SHM_XRGB8888).is_ok());
        assert!(ShmSnapshot::new(1, 1, vec![0; 4], SHM_ARGB8888).is_ok());
        assert!(ShmSnapshot::new(1, 1, vec![0; 4], 2).is_err());
        assert!(ShmSnapshot::new(1, 1, vec![0; 4], u32::MAX).is_err());
    }

    #[test]
    fn opacity_is_the_buffers_answer_for_both_admitted_formats() {
        let opaque = |format, alpha| {
            Surface::from_shm_pixels(1, 1, vec![0; 4], format)
                .unwrap()
                .pixel_is_opaque(alpha)
        };
        // XRGB ignores its fourth byte, so every alpha is opaque.
        assert!(opaque(SHM_XRGB8888, 0));
        assert!(opaque(SHM_XRGB8888, 0x7f));
        assert!(opaque(SHM_XRGB8888, u8::MAX));
        // ARGB is opaque only at full alpha; everything below it blends.
        assert!(!opaque(SHM_ARGB8888, 0));
        assert!(!opaque(SHM_ARGB8888, u8::MAX - 1));
        assert!(opaque(SHM_ARGB8888, u8::MAX));
    }

    #[test]
    fn the_accessors_answer_from_the_buffer_rather_than_from_a_copy() {
        let mut surface = Surface::from_shm_pixels(2, 1, vec![9; 8], SHM_ARGB8888).unwrap();
        assert_eq!(surface.width(), 2);
        assert_eq!(surface.height(), 1);
        assert_eq!(surface.format(), SHM_ARGB8888);
        assert_eq!(surface.resident_bytes(), 8);
        if let Some(byte) = surface.linear_bytes_mut().and_then(<[u8]>::first_mut) {
            *byte = 1;
        }
        assert_eq!(surface.linear_bytes().and_then(<[u8]>::first), Some(&1));
        assert_eq!(surface.resident_bytes(), 8);
    }
}
