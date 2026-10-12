//! Body-part property arrays reject explicit null.
pub use crate::property_selection::json::{Argument, Cell, Error, Status};
pub type Cursor<'i, 'c, 'k, 'm> = crate::property_selection::json::Cursor<'i, 'c, 'k, 'm, false>;
#[cfg(test)]
pub use crate::property_selection::json::probe_allocations;
