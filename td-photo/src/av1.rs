//! AV1 export codec with the photo tool's shared worker pool.

pub use td_av1::intra::{
    av1c, qindex, Error, Geometry, Reconstruction, COLOUR_PRIMARIES, FULL_RANGE,
    MATRIX_COEFFICIENTS, MAX_AXIS, TRANSFER_CHARACTERISTICS,
};

pub struct PhotoRunner;
impl td_av1::intra::Runner for PhotoRunner {
    fn run<T: Send>(items: Vec<T>, threads: usize, work: impl Fn(T) + Sync) {
        crate::develop::bands(items, threads, work);
    }
}

pub type Encoder = td_av1::intra::Encoder<PhotoRunner>;

const _: () = assert!(MAX_AXIS == crate::image::MAX_AXIS);
const _: () = assert!(td_av1::intra::MAX_THREADS == crate::develop::MAX_THREADS);
