#![forbid(unsafe_code)]

//! td's dependency-free UI toolkit, shared by td-owned graphical programs as
//! a Cargo path dependency (td-editor first). It carries the
//! display-independent input layer, the shared font and wire codecs and the
//! clipped XRGB raster with its palette and scrollbar geometry; the Wayland
//! client transport and chrome widgets follow in the order DESIGN.md
//! schedules. Nothing here reads the environment, a clock, a descriptor or
//! the filesystem: adapters supply explicit inputs, and `notices` embeds
//! the face's licence texts at compile time.

/// The bitmap cell every consumer lays text out on. The pinned Unifont face
/// is 8x16 and `font::pinned` is held to these by a test, so pointer hit
/// testing, layout and painting agree on one grid.
pub const CELL_WIDTH: usize = 8;
pub const CELL_HEIGHT: usize = 16;

#[path = "../../td-compositor/src/font.rs"]
pub mod font;
#[path = "../../td-compositor/src/font_data.rs"]
mod font_data;
pub mod keyboard;
pub mod notices;
pub mod pointer;
pub mod raster;
pub mod repeat;
#[allow(clippy::new_without_default)]
#[path = "../../td-compositor/src/wire.rs"]
pub mod wire;
pub mod xkb;
mod xkb_compat;
mod xkb_keys;
mod xkb_symbols;
mod xkb_syntax;

#[cfg(test)]
mod tests {
    #[test]
    #[allow(clippy::unwrap_used)]
    fn the_pinned_face_is_the_cell() {
        let face = crate::font::pinned().unwrap();
        assert_eq!(
            (face.width(), face.height()),
            (crate::CELL_WIDTH, crate::CELL_HEIGHT)
        );
    }
}
