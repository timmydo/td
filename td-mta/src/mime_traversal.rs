//! Service ownership adapters around the shared MIME parser.
pub use crate::structure::*;
#[path = "mime_traversal/bound.rs"]
pub mod bound;
impl From<&crate::limits::Limits> for crate::structure::Limits {
    fn from(limits: &crate::limits::Limits) -> Self {
        Self {
            header_bytes: limits.header_bytes,
            mime_depth: limits.mime_depth,
            mime_parts: limits.mime_parts,
        }
    }
}
