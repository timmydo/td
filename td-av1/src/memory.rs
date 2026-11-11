//! Shared fallible codec buffers.
#[cfg(test)]
pub(crate) use td_alloc::with_reservation_limit as failing;
pub(crate) use td_alloc::{copy, filled, reserve_exact as reserve};
