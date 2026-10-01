//! The fixed reference-renderer preview: a demonstration document painted
//! through the shared editor scene, written as binary P6 PPM. Not file
//! input or a window.

use crate::Result;
use td_ui::editor_keys::Profile;
use td_ui::editor_model::{Command, Editor, Selection};
use td_ui::editor_render::{Geometry, Label, Scene, View};
use td_ui::raster::{Raster, Scale};

/// Writes the preview to `output`.
pub fn write(output: &mut impl std::io::Write) -> std::io::Result<()> {
    let font = crate::font::pinned().map_err(std::io::Error::other)?;
    let fixture = || -> Result<Vec<u8>> {
        let mut editor = Editor::default();
        let notes = editor.new_tab()?;
        editor.dispatch(notes, 0, Command::Insert(
            "A small text editor\n\nBitmap text, tabs, and a plain document area.\n\nWindows and Emacs key profiles share the same commands.\nParagraph filling inserts real line breaks; soft wrapping does not.\n\nUnicode scalars: café, naïve, λ.\n\tTabs advance to eight-column stops.\n\nThis is the reference-renderer preview, not a Wayland window.\nOpen, Save, clipboard and spelling adapters come next.\n".into()))?;
        let readme = editor.load_bytes(b"td-editor\n")?;
        editor.select_tab(notes)?;
        let revision = editor.document(notes)?.revision();
        editor.dispatch(
            notes,
            revision,
            Command::Select(Selection {
                anchor: 2,
                caret: 7,
            }),
        )?;
        let geometry = Geometry::new(800, 600, Scale::new(1)?)?;
        let labels = [
            Label {
                tab: notes,
                title: "notes.txt",
            },
            Label {
                tab: readme,
                title: "README.md",
            },
        ];
        let scene = Scene::new(
            &editor,
            geometry,
            View::default(),
            &labels,
            Profile::Windows,
        )?;
        let mut pixels = vec![0; 800 * 600 * 4];
        Raster::new(&mut pixels, &font, geometry.surface(), 800 * 4)?
            .paint(&scene, geometry.bounds())?;
        let rgb = td_ui::raster::rgb(&pixels, geometry.surface(), 800 * 4)?;
        Ok(td_ui::raster::ppm(geometry.surface(), &rgb))
    };
    let ppm = fixture().map_err(std::io::Error::other)?;
    output.write_all(&ppm)?;
    Ok(())
}
