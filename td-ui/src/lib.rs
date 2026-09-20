#![deny(unsafe_code)]

//! td's dependency-free UI toolkit, shared by td-owned graphical programs
//! as a Cargo path dependency (td-editor first). It carries the
//! display-independent input layer, the shared font, the hint face and
//! wire codecs and the clipped XRGB raster with its palette and scrollbar
//! geometry, the Wayland
//! client connection over its own raw descriptor transport (UNSAFE.md §19)
//! and the client over it (`client`: the object table, one toplevel surface
//! with its buffers and pointer image, the seat with its devices and the
//! clipboard, and the turn loop that drives a consumer's `App`), and the
//! chrome bands over the raster (`chrome`: the menu bar and its panel, the
//! wrapped text block, the tab strip, the button strip, the slider, the
//! status row, the paged list and the single-line text entry), and the
//! driving layer an agent or a test operates a consumer through (`control`:
//! the frame, envelope, codecs and response lines; `control_socket`:
//! private listener publication; `control_worker`: the bounded transport
//! thread handing typed jobs to the consumer's turn; `replay`: the
//! consecutive-frame runner behind a headless `--replay`; `driven`: the
//! semantic seam, a `Controller` over a consumer's action table with the
//! generic verbs routed over the envelope, text read back from the draw
//! stream and the painted frame), the widget window (`window`: the window
//! that presents what a program paints over its surface and hands it
//! chords, button phases, wheel travel in surface pixels and the
//! clipboard), and the clipboard's transfer owners (`clipboard`: the
//! bounded nonblocking writer of an offered text over the send's right and
//! the reader of the selection's text). Outside `wayland`, `client`,
//! `clipboard`, the private raw module beneath them, the driving adapters
//! `control_socket`, `control_worker` and `replay`, and the widget window
//! `window`, nothing reads the environment, a clock, a descriptor or the
//! filesystem: adapters supply explicit inputs, and `notices` embeds the
//! face's licence texts at compile time.

/// The bitmap cell every consumer lays text out on. The pinned Unifont face
/// is 8x16 and `font::pinned` is held to these by a test, so pointer hit
/// testing, layout and painting agree on one grid.
pub const CELL_WIDTH: usize = 8;
pub const CELL_HEIGHT: usize = 16;

pub mod charts;
pub mod chrome;
pub mod client;
pub mod clipboard;
pub mod confirmations;
pub mod control;
pub mod control_socket;
pub mod control_worker;
pub mod data;
pub mod driven;
#[path = "../../td-compositor/src/filter.rs"]
pub mod filter;
pub mod finder;
pub mod hint;
#[path = "../../td-compositor/src/font.rs"]
pub mod font;
#[path = "../../td-compositor/src/font_data.rs"]
mod font_data;
pub mod keyboard;
pub mod menus;
pub mod notices;
pub mod pointer;
pub mod raster;
pub mod repeat;
pub mod replay;
pub mod split;
mod sys;
pub mod tree_table;
mod tree_table_geometry;
mod tree_table_model;
mod tree_table_paint;
pub mod wayland;
pub mod window;
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
