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
//! answers to be written, not the callers to be revisited.
//!
//! The callers that genuinely must change are the CEILINGS, which cannot
//! charge a card's memory in CPU bytes, and that seam is here too:
//! `BufferCharge` is what every buffer ledger counts, and `BufferCeiling` is
//! how each ceiling names its limit per kind. A ceiling is where the compile
//! error for a second kind finally lands, after this module has been made to
//! answer for it first.
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

/// What a set of buffers costs, kept PER KIND rather than as one number.
///
/// `APPLICATIONS.md` §M's fourth row: counting CPU bytes is the wrong unit the
/// moment a buffer lives on a card. A dmabuf occupies no compositor memory and
/// some quantity of device memory, so a ceiling that added the two would be
/// adding different things, and one that kept counting only compositor bytes
/// would be unbounded in the resource that actually ran out.
///
/// Two quantities per kind, which is the row's "per buffer type and per
/// outstanding lifetime": how much is held, and how many holdings there are.
/// The second is not derivable from the first — a thousand one-pixel cursors
/// and one window can cost the same bytes and are not the same problem — and
/// it is what a lease-based release will be counted in when §M's second row
/// lands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BufferCharge {
    shm_bytes: usize,
    shm_held: usize,
}

impl BufferCharge {
    pub fn none() -> BufferCharge {
        BufferCharge::default()
    }

    /// One `wl_shm` holding of a known size, for the reservation a commit
    /// takes BEFORE the copy exists. Naming the kind at the call site is the
    /// point: a caller reserving for a kind whose cost is not host bytes has
    /// to say so rather than passing a number that looks like any other.
    pub fn shm(bytes: usize) -> BufferCharge {
        BufferCharge {
            shm_bytes: bytes,
            shm_held: 1,
        }
    }

    /// Bytes of td's OWN address space. Named for the resource rather than
    /// for the kind, because that is what a ceiling on it bounds, and a kind
    /// whose bytes are not td's contributes nothing here BY DESIGN — it needs
    /// a ceiling of its own rather than a share of this one.
    pub fn host_bytes(self) -> usize {
        let BufferCharge {
            shm_bytes,
            shm_held: _,
        } = self;
        shm_bytes
    }

    /// How many buffers this charge is holding, across every kind.
    pub fn held(self) -> usize {
        let BufferCharge {
            shm_bytes: _,
            shm_held,
        } = self;
        shm_held
    }

    pub fn checked_add(self, other: BufferCharge) -> Option<BufferCharge> {
        Some(BufferCharge {
            shm_bytes: self.shm_bytes.checked_add(other.shm_bytes)?,
            shm_held: self.shm_held.checked_add(other.shm_held)?,
        })
    }

    pub fn checked_sub(self, other: BufferCharge) -> Option<BufferCharge> {
        Some(BufferCharge {
            shm_bytes: self.shm_bytes.checked_sub(other.shm_bytes)?,
            shm_held: self.shm_held.checked_sub(other.shm_held)?,
        })
    }

    /// Never past the maximum. For the running totals that have no caller to
    /// fail to, where an add that silently kept its previous value would
    /// UNDER-count, which is the direction that admits.
    ///
    /// Saturating is the safe direction only where the total is then
    /// compared against a ceiling — `cursor_fits`, which over-estimates and
    /// so refuses. The refund sweeps in `remove_client` accumulate with this
    /// too, and there saturation would over-refund; that is fail-open, and it
    /// is unreachable rather than guarded, because a refund is bounded by the
    /// total it was admitted under and every ceiling is far below `usize`.
    /// `commit_cursor`'s add is neither: it maintains a total nothing admits
    /// on, so saturation there costs a report rather than a decision.
    pub fn saturating_add(self, other: BufferCharge) -> BufferCharge {
        BufferCharge {
            shm_bytes: self.shm_bytes.saturating_add(other.shm_bytes),
            shm_held: self.shm_held.saturating_add(other.shm_held),
        }
    }

    /// Never below zero. For the teardown sweeps, which must not fail.
    pub fn saturating_sub(self, other: BufferCharge) -> BufferCharge {
        BufferCharge {
            shm_bytes: self.shm_bytes.saturating_sub(other.shm_bytes),
            shm_held: self.shm_held.saturating_sub(other.shm_held),
        }
    }

    /// Whether this charge fits under `ceiling`, every kind at once.
    ///
    /// Both sides are destructured without `..` deliberately, and that is
    /// what carries §M's fourth row out to the callers. A second kind adds a
    /// field to the charge, which stops THIS function compiling; answering
    /// for it means giving `BufferCeiling` a limit of its own, which stops
    /// every BUFFER ceiling compiling until its author says what that ceiling
    /// allows of the new resource. Ceilings over things that are not buffers
    /// — a `wl_shm` pool's bytes, a deferred-event queue's — are unaffected,
    /// and should be: they bound one resource that has no kind.
    ///
    /// An accessor returning one total could not do that: summing the fields
    /// would compile, and would quietly bound a card's memory in CPU bytes at
    /// every ceiling at once.
    /// `shm_held` is discarded HERE and nowhere silently: no ceiling bounds
    /// holdings today, because every copy is released at commit, so a
    /// client's holdings track its live surfaces and `MAX_OBJECTS` already
    /// bounds those. The quantity that needs a bound of its own is an
    /// outstanding LEASE, which §M's second row introduces. A kind that
    /// carries a lifetime cost adds a field the pattern below must mention,
    /// so discarding it stays a written decision rather than a default.
    pub fn fits(self, ceiling: BufferCeiling) -> bool {
        let BufferCharge {
            shm_bytes,
            shm_held: _,
        } = self;
        let BufferCeiling { host_bytes } = ceiling;
        shm_bytes <= host_bytes
    }
}

/// What one ceiling allows, per kind.
///
/// Written as a struct literal at each ceiling rather than passed as a number,
/// so that adding a kind is a question each ceiling has to answer separately.
/// No `BufferCharge` can answer it on a ceiling's behalf: a limit on device
/// memory has nothing to do with how much of td's own address space the same
/// ceiling was protecting, and the two are not exchangeable at any rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BufferCeiling {
    /// Bytes of td's own address space this ceiling allows.
    pub host_bytes: usize,
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

    /// What this surface costs, by kind.
    ///
    /// The match is the cost table: a second variant does not compile until
    /// it says what holding one costs. It reaches `BufferCharge` through the
    /// same constructor a pre-copy reservation does, which single-sources the
    /// HOLDING; the byte counts agree for a different reason, because
    /// `ShmSnapshot::new` refuses a buffer whose length is not exactly the
    /// geometry the reservation was computed from.
    pub fn charge(&self) -> BufferCharge {
        match &self.buffer {
            SurfaceBuffer::Shm(shm) => BufferCharge::shm(shm.pixels.len()),
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
    fn a_charge_counts_bytes_and_holdings_separately() {
        let big = Surface::from_shm_pixels(4, 4, vec![0; 64], SHM_XRGB8888).unwrap();
        let small = Surface::from_shm_pixels(1, 1, vec![0; 4], SHM_XRGB8888).unwrap();

        let charge = BufferCharge::none()
            .checked_add(big.charge())
            .and_then(|charge| charge.checked_add(small.charge()))
            .unwrap();
        assert_eq!(charge.host_bytes(), 68);
        assert_eq!(charge.held(), 2);

        // Holdings are not derivable from bytes: the same total, twice the
        // buffers, is a different fact about the same client.
        let many = (0..16).fold(BufferCharge::none(), |charge, _| {
            charge.checked_add(small.charge()).unwrap()
        });
        assert_eq!(many.host_bytes(), 64);
        assert_eq!(many.held(), 16);
        assert_eq!(big.charge().host_bytes(), 64);
        assert_eq!(big.charge().held(), 1);
    }

    #[test]
    fn a_charge_returns_to_nothing_when_its_buffers_go() {
        let surface = Surface::from_shm_pixels(2, 2, vec![0; 16], SHM_ARGB8888).unwrap();
        let charge = BufferCharge::none().checked_add(surface.charge()).unwrap();
        assert_ne!(charge, BufferCharge::none());
        assert_eq!(
            charge.checked_sub(surface.charge()),
            Some(BufferCharge::none())
        );
        // And a subtraction that would go negative is refused rather than
        // wrapping into an enormous charge.
        assert_eq!(BufferCharge::none().checked_sub(surface.charge()), None);
        assert_eq!(
            BufferCharge::none().saturating_sub(surface.charge()),
            BufferCharge::none()
        );
    }

    #[test]
    fn a_ledger_that_cannot_add_saturates_rather_than_forgetting() {
        let surface = Surface::from_shm_pixels(1, 1, vec![0; 4], SHM_XRGB8888).unwrap();
        let brim = BufferCharge::shm(usize::MAX);
        // A sweep that cannot represent its own total holds the maximum, so
        // the next ceiling check REFUSES. Keeping the previous total instead
        // would under-count and admit one more — the ledger failing open.
        let over = brim.saturating_add(surface.charge());
        assert_eq!(over.host_bytes(), usize::MAX);
        // The HOLDING still went up: an add that answered the previous total
        // would leave this at one, and that is the fail-open shape.
        assert_eq!(over.held(), 2);
        assert!(!over.fits(BufferCeiling {
            host_bytes: 32 * 1024 * 1024
        }));
        // Where a caller can answer an error, it gets one instead.
        assert_eq!(brim.checked_add(surface.charge()), None);
    }

    #[test]
    fn a_ceiling_bounds_host_memory_rather_than_a_total() {
        let surface = Surface::from_shm_pixels(2, 2, vec![0; 16], SHM_XRGB8888).unwrap();
        let charge = surface.charge();
        assert!(charge.fits(BufferCeiling { host_bytes: 16 }));
        assert!(!charge.fits(BufferCeiling { host_bytes: 15 }));
        assert!(BufferCharge::none().fits(BufferCeiling { host_bytes: 0 }));
    }

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
        assert_eq!(surface.charge().host_bytes(), 8);
        if let Some(byte) = surface.linear_bytes_mut().and_then(<[u8]>::first_mut) {
            *byte = 1;
        }
        assert_eq!(surface.linear_bytes().and_then(<[u8]>::first), Some(&1));
        assert_eq!(surface.charge().host_bytes(), 8);
    }
}
