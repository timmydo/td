//! The frame over the widget window: the top view's scene laid out as the
//! toolkit's action bar, an optional text entry, a list or td-editor's
//! read-only document pane, and the status row; and the pane itself,
//! which holds a document per stacked view that shows a text, so a
//! view's text is its own and the view under a popped one is back where
//! it was read to.

use td_editor::model::TabId;
use td_editor::ui::{Controller, Event, Outcome, PointerPhase as PanePhase};
use td_ui::chrome::{Bar, Field, Item, List, Status, TextEntry, ROW};
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Surface, PAPER};
use td_ui::window::PointerPhase;

use super::views::{Body, Scene};

/// The rows a page key moves by when the layout has none to say: the
/// terminal's fifteen.
pub const PAGE_ROWS: usize = 15;

/// The scene's regions over the surface.
#[derive(Clone, Copy)]
pub struct Layout {
    pub bar: Bar<'static>,
    pub entry: Option<TextEntry>,
    pub list: Option<List>,
    pub pane: Option<Rect>,
    pub status: Status,
}

impl Layout {
    /// The bar at the top, the status at the bottom, and between them
    /// the entry's band when the scene has one, then the list or the
    /// pane.
    pub fn new(surface: Surface, scene: &Scene<'_>) -> Self {
        let s = surface.scale.value();
        let row = (ROW * s) as u32;
        let bar = Bar::new(surface, scene.labels);
        let status = Status::new(surface);
        let top = i64::from(row);
        let bottom = status.rect().y.max(top);
        let mut body = Rect {
            x: 0,
            y: top,
            width: surface.width as u32,
            height: (bottom - top) as u32,
        };
        let mut layout = Layout {
            bar,
            entry: None,
            list: None,
            pane: None,
            status,
        };
        if scene.entry.is_some() {
            // A body too short for the band gets none of it, and a band
            // the widget refuses leaves the body its rows.
            let height = row.min(body.height);
            layout.entry = TextEntry::new(surface, Rect { height, ..body });
            if layout.entry.is_some() {
                body = Rect {
                    y: body.y + i64::from(height),
                    height: body.height - height,
                    ..body
                };
            }
        }
        match scene.body {
            Body::List { .. } => layout.list = List::new(surface, body),
            Body::Text { .. } => {
                layout.pane = (body.width > 0 && body.height > 0).then_some(body);
            }
        }
        layout
    }

    /// The rows the body shows, which the page keys move by.
    pub fn page(&self, pane: &mut Pane, surface: Surface) -> usize {
        if let Some(list) = self.list {
            return list.rows().max(1);
        }
        if let Some(rect) = self.pane {
            pane.place(rect, surface);
            return pane.grid().0.max(1);
        }
        PAGE_ROWS
    }
}

/// A view's document in the pane: its tab, the key of the text it
/// holds, and the columns the text was wrapped for. Kept by the view's
/// slot on the stack, so two views that name their texts alike hold two
/// documents, and a view's is closed with it.
pub struct Shown {
    tab: TabId,
    key: String,
    columns: usize,
}

/// td-editor's document pane, holding a document per stacked view that
/// shows a text; the active one is the top view's.
pub struct Pane {
    controller: Controller,
    /// The document shown: the top view's, once its frame is prepared.
    tab: Option<TabId>,
    /// A press landed in the pane and has not been released.
    pub drag: bool,
}

impl Pane {
    pub fn new() -> Result<Self, String> {
        Ok(Pane {
            controller: Controller::pane().map_err(|e| e.to_string())?,
            tab: None,
            drag: false,
        })
    }

    #[cfg(test)]
    pub fn editor(&self) -> &td_editor::model::Editor {
        self.controller.editor()
    }

    #[cfg(test)]
    pub fn tab(&self) -> Option<TabId> {
        self.tab
    }

    /// The first document row the shown document's viewport holds.
    #[cfg(test)]
    pub fn first_row(&self) -> Option<usize> {
        let tab = self.tab?;
        Some(self.controller.tab_view(tab).ok()?.viewport.origin().row)
    }

    /// Dispatches to the pane; an error is a diagnostic, not the client's
    /// state, and reads as ignored.
    fn event(&mut self, event: Event<'_>) -> Outcome {
        match self.controller.dispatch(event) {
            Ok(outcome) => outcome,
            Err(error) => {
                crate::log_error!("document pane: {}", error);
                Outcome::Ignored
            }
        }
    }

    fn target(&self) -> Option<(TabId, u64)> {
        let tab = self.tab?;
        let revision = self.controller.editor().document(tab).ok()?.revision();
        Some((tab, revision))
    }

    /// The pane's rectangle on the surface.
    pub fn place(&mut self, rect: Rect, surface: Surface) {
        self.event(Event::Frame { rect, surface });
    }

    /// The document rows and text columns the pane shows once placed, as
    /// td-editor lays its document out in the rect.
    pub fn grid(&self) -> (usize, usize) {
        let (columns, rows) = self.controller.geometry().grid();
        (rows, columns)
    }

    /// Shows a view's text: the document in `shown` when it holds the
    /// text `key` names wrapped for `columns`, selected again if another
    /// view's was active, where it was read to; otherwise a document
    /// loaded from `text`, asked only then, in place of the one `shown`
    /// held. A text the pane refuses is shown as refused, in a document
    /// of its own, rather than as nothing.
    pub fn show(
        &mut self,
        shown: &mut Option<Shown>,
        key: &str,
        columns: usize,
        text: impl FnOnce() -> String,
    ) {
        if let Some(held) = shown.as_ref() {
            if held.key == key && held.columns == columns {
                if self.tab != Some(held.tab) {
                    self.event(Event::SelectTab(held.tab));
                    self.tab = Some(held.tab);
                }
                return;
            }
        }
        self.close(shown.take());
        self.tab = None;
        let source = pane_source(&text());
        let mut loaded = self.event(Event::Load(source.as_bytes()));
        if !matches!(loaded, Outcome::Created(_)) {
            loaded = self.event(Event::Load(b"This text cannot be shown."));
        }
        if let Outcome::Created(tab) = loaded {
            self.event(Event::ReadOnly { tab, enabled: true });
            self.tab = Some(tab);
            *shown = Some(Shown {
                tab,
                key: key.to_string(),
                columns,
            });
        }
    }

    /// Closes a view's document, with the view or for its next text.
    pub fn close(&mut self, shown: Option<Shown>) {
        let Some(held) = shown else {
            return;
        };
        if let Ok(document) = self.controller.editor().document(held.tab) {
            let revision = document.revision();
            self.event(Event::Close {
                tab: held.tab,
                revision,
            });
        }
        if self.tab == Some(held.tab) {
            self.tab = None;
        }
    }

    pub fn scroll(&mut self, rows: isize) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        self.event(Event::Scroll {
            tab,
            revision,
            rows,
            columns: 0,
        }) == Outcome::Changed
    }

    pub fn chord(&mut self, chord: &str) -> bool {
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        if chord.is_empty() {
            return false;
        }
        self.event(Event::Key {
            tab,
            revision,
            chord,
        }) == Outcome::Changed
    }

    /// A press in the pane is handed on at the pointer's pixel as both
    /// the caret and the cell coordinate, as the toolkit's replay does,
    /// so the caret lands before the glyph under the pointer rather than
    /// at its nearer edge.
    pub fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> bool {
        match phase {
            PointerPhase::Press => self.drag = true,
            PointerPhase::Release => self.drag = false,
            PointerPhase::Move => {}
        }
        let phase = match phase {
            PointerPhase::Press => PanePhase::Press,
            PointerPhase::Move => PanePhase::Move,
            PointerPhase::Release => PanePhase::Release,
        };
        let Some((tab, revision)) = self.target() else {
            return false;
        };
        self.event(Event::Pointer {
            tab,
            revision,
            phase,
            x,
            cell_x: x,
            y,
            extend,
        }) == Outcome::Changed
    }

    pub fn cancel_pointer(&mut self) {
        self.drag = false;
        self.event(Event::CancelPointer);
    }

    pub fn focus(&mut self, focused: bool) {
        self.event(Event::Focus(focused));
    }

    /// The clock, for the caret's blink: whether the pane changed.
    pub fn tick(&mut self, now: u64) -> bool {
        self.event(Event::Tick(now)) == Outcome::Changed
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Ok(scene) = self.controller.scene(&[]) {
            scene.emit(damage, sink);
        }
    }
}

/// `text` as the pane admits it: a leading byte order mark dropped, CRLF
/// one newline, a control scalar other than newline and tab (which the
/// editor refuses) the replacement character, and at most the editor's
/// file ceiling in bytes.
pub fn pane_source(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut source = String::with_capacity(text.len());
    for c in text.replace("\r\n", "\n").chars() {
        let shown = match c {
            '\n' | '\t' => c,
            c if c <= '\u{1f}' || c == '\u{7f}' => '\u{fffd}',
            c => c,
        };
        if source.len() + shown.len_utf8() > td_editor::text::MAX_FILE_BYTES {
            break;
        }
        source.push(shown);
    }
    source
}

/// The frame as a composition: what the window paints and what a test
/// reads back.
pub struct Frame<'a> {
    pub surface: Surface,
    pub scene: &'a Scene<'a>,
    pub first: usize,
    pub entry_first: usize,
    pub pane: &'a Pane,
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let layout = Layout::new(self.surface, self.scene);
        if let Some(clip) = damage.intersection(self.surface.bounds()) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: self.surface.bounds(),
                    color: PAPER,
                },
            });
        }
        layout.bar.emit(damage, sink);
        if let (Some(field), Some(entry)) = (layout.entry, &self.scene.entry) {
            let caret = entry.text.chars().count();
            field.emit(
                Field {
                    text: entry.text,
                    placeholder: entry.placeholder,
                    caret,
                    anchor: None,
                    first: self.entry_first,
                    masked: false,
                    focused: true,
                    caret_visible: true,
                },
                damage,
                sink,
            );
        }
        match &self.scene.body {
            Body::List {
                total,
                selected,
                row,
            } => {
                if let Some(list) = layout.list {
                    let end = (*total).min(self.first.saturating_add(list.rows()));
                    let rows: Vec<_> = (self.first..end).map(row).collect();
                    list.emit(
                        rows.iter().map(|row| Item {
                            label: row.label.as_str(),
                            meta: row.meta.as_str(),
                            enabled: true,
                            marked: row.marked,
                        }),
                        self.first,
                        *selected,
                        *total,
                        damage,
                        sink,
                    );
                }
            }
            Body::Text { .. } => {
                if layout.pane.is_some() {
                    self.pane.emit(damage, sink);
                }
            }
        }
        layout.status.emit(self.scene.status.chars(), damage, sink);
    }
}

/// Paints the frame whole into the raster.
pub fn paint(raster: &mut Raster<'_, '_>, frame: &Frame<'_>) -> Result<(), String> {
    raster
        .paint(frame, frame.surface.bounds())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A view's text is admitted as the pane takes it and reloaded only
    /// when its key or the columns change; a second view's text with
    /// the same key is its own document, and showing the first again
    /// selects its document where it was scrolled to.
    #[test]
    fn a_views_text_is_its_own_document_reloaded_when_its_key_or_columns_change() {
        assert_eq!(
            pane_source("\u{feff}A\u{7f}B\r\nC\u{1}\tD\n"),
            "A\u{fffd}B\nC\u{fffd}\tD\n"
        );
        let mut pane = Pane::new().unwrap();
        let mut asked = 0;
        let mut text = |s: &str| {
            asked += 1;
            s.to_string()
        };
        let mut first = None;
        pane.show(&mut first, "a", 40, || text("x\u{7f}y"));
        let tab = pane.tab().expect("loaded");
        let document = pane.editor().document(tab).unwrap();
        assert_eq!(document.text(), "x\u{fffd}y");
        assert!(document.read_only());
        pane.show(&mut first, "a", 40, || text("other"));
        assert_eq!(pane.tab(), Some(tab), "the same key and columns: kept");
        pane.show(&mut first, "a", 30, || text("narrow"));
        let tab = pane.tab().expect("reloaded");
        assert_eq!(pane.editor().document(tab).unwrap().text(), "narrow");
        assert_eq!(pane.editor().tabs().count(), 1, "one document per view");
        drop(text);
        assert_eq!(asked, 2);
        // Another view's text under the same key is another document;
        // the first, scrolled, is back where it was when shown again.
        let long = (1..100)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        pane.show(&mut first, "b", 30, || long.clone());
        let tab = pane.tab().expect("the first view's");
        pane.place(
            Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 64,
            },
            Surface::new(400, 64, Default::default()).unwrap(),
        );
        assert!(pane.scroll(5));
        let origin = pane.controller.tab_view(tab).unwrap().viewport.origin();
        let mut second = None;
        pane.show(&mut second, "b", 30, || "help".to_string());
        let other = pane.tab().expect("the second view's");
        assert_ne!(other, tab);
        assert_eq!(pane.editor().tabs().count(), 2);
        pane.show(&mut first, "b", 30, || unreachable!("held"));
        assert_eq!(pane.tab(), Some(tab));
        assert_eq!(
            pane.controller.tab_view(tab).unwrap().viewport.origin(),
            origin
        );
        pane.close(second);
        assert_eq!(pane.editor().tabs().count(), 1);
        assert_eq!(pane.tab(), Some(tab), "the active document stays");
        pane.close(first);
        assert_eq!(pane.tab(), None);
        assert_eq!(pane.editor().tabs().count(), 0);
        pane.close(None);
    }
}
