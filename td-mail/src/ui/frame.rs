//! The frame over the widget window: the top view's scene laid out as the
//! toolkit's action bar, an optional text entry, a list or td-editor's
//! document pane, read-only for a text and editable for a draft, and the
//! status row; and the pane itself, which holds a document per stacked
//! view that shows a text or a draft, so a view's text is its own and
//! the view under a popped one is back where it was read to. The pane
//! keeps the kill ring too: a selection cut or copied in any of its
//! documents, pasted into an editable one; the session offers the same
//! selection to the window's clipboard and pastes what it answers. The
//! finder a draft's Attach opens is painted over the body in its place.

use std::sync::Arc;
use td_editor::clipboard::{Paste, Snapshot};
use td_editor::model::{Command, SavePoint, Selection, TabId};
use td_editor::ui::{Controller, Event, Outcome, PointerPhase as PanePhase};
use td_ui::chrome::{Bar, Field, Item, List, Status, TextEntry, ROW};
use td_ui::finder;
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Surface, PAPER};
use td_ui::window::PointerPhase;

use super::input::{Key, Menu};
use super::views::{Body, Scene};

/// A bar label's dropdown as the session holds it: the toolkit's menu
/// controller over the view's keys, its revision the dropdown it is.
pub type Dropdown = td_ui::menus::Controller<'static, Key, Menu>;

/// The rows a page key moves by when the layout has none to say: the
/// terminal's fifteen.
pub const PAGE_ROWS: usize = 15;

/// The editor's clipboard error as a status row says it: its ceiling is
/// the one a person can act on, the rest are the editor's own words.
fn describe(error: td_editor::Error, what: &str) -> String {
    match error {
        td_editor::Error::Limit => format!(
            "{what} is past the clipboard's {} KiB ceiling",
            td_editor::clipboard::MAX_BYTES / 1024
        ),
        other => other.to_string(),
    }
}

/// The scene's regions over the surface.
#[derive(Clone, Copy)]
pub struct Layout {
    pub bar: Bar<'static>,
    pub entry: Option<TextEntry>,
    pub list: Option<List>,
    pub pane: Option<Rect>,
    /// The band between the entry, or the bar, and the status row, which
    /// the list or the pane fills, and the finder when one is open.
    pub body: Rect,
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
            body,
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
        layout.body = body;
        match scene.body {
            Body::List { .. } => layout.list = List::new(surface, body),
            Body::Text { .. } | Body::Edit { .. } => {
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
/// holds, and the columns the text was wrapped for (none for a draft,
/// which the pane wraps as it is typed). Kept by the view's slot on the
/// stack, so two views that name their texts alike hold two documents,
/// and a view's is closed with it.
pub struct Shown {
    tab: TabId,
    key: String,
    columns: usize,
}

impl Shown {
    /// The document's tab, for the draft handle over it.
    pub fn tab(&self) -> TabId {
        self.tab
    }
}

/// td-editor's document pane, holding a document per stacked view that
/// shows a text or a draft; the active one is the top view's.
pub struct Pane {
    controller: Controller,
    /// The document shown: the top view's, once its frame is prepared.
    tab: Option<TabId>,
    /// A press landed in the pane and has not been released.
    pub drag: bool,
    /// The kill ring: the last selection cut or copied, for a paste.
    kill: Option<Arc<str>>,
}

impl Pane {
    pub fn new() -> Result<Self, String> {
        Ok(Pane {
            controller: Controller::pane().map_err(|e| e.to_string())?,
            tab: None,
            drag: false,
            kill: None,
        })
    }

    #[cfg(test)]
    pub fn editor(&self) -> &td_editor::model::Editor {
        self.controller.editor()
    }

    /// The document shown, the top view's: what a paste is asked for.
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
        self.target_of(self.tab?)
    }

    /// The document's current revision, for an event bound to it.
    fn target_of(&self, tab: TabId) -> Option<(TabId, u64)> {
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

    /// Shows a view's draft for editing: the document in `shown` when it
    /// holds the text `key` names, selected again if another view's was
    /// active; otherwise a document loaded from `text`, asked only then,
    /// editable, filling its paragraphs as a mail draft is typed. A
    /// draft the pane refuses (one past its ceiling) leaves the view
    /// without a document, which its save then reports; nothing is
    /// shortened to fit.
    pub fn edit(&mut self, shown: &mut Option<Shown>, key: &str, text: impl FnOnce() -> String) {
        if let Some(held) = shown.as_ref() {
            if held.key == key {
                if self.tab != Some(held.tab) {
                    self.event(Event::SelectTab(held.tab));
                    self.tab = Some(held.tab);
                }
                return;
            }
        }
        self.close(shown.take());
        self.tab = None;
        let Some(source) = draft_source(&text()) else {
            crate::log_error!("document pane: the draft is larger than the pane's ceiling");
            return;
        };
        if let Outcome::Created(tab) = self.event(Event::Load(source.as_bytes())) {
            if let Some((tab, revision)) = self.target_of(tab) {
                self.event(Event::Edit {
                    tab,
                    revision,
                    command: Command::AutoFill(true),
                });
            }
            self.tab = Some(tab);
            *shown = Some(Shown {
                tab,
                key: key.to_string(),
                columns: 0,
            });
        }
    }

    /// Closes a view's document, with the view or for its next text. A
    /// document closed dirty (its view decided that, or a stack thrown
    /// away closed it) is given up as it is rather than left open with
    /// no view to reach it, and the log says so.
    pub fn close(&mut self, shown: Option<Shown>) {
        let Some(held) = shown else {
            return;
        };
        if let Ok(document) = self.controller.editor().document(held.tab) {
            if document.dirty() {
                crate::log_error!("document pane: a document is closed with unsaved changes");
                if let Ok((point, _)) = self.controller.editor().save_snapshot(held.tab) {
                    self.event(Event::Saved(point));
                }
            }
            if let Some((tab, revision)) = self.target_of(held.tab) {
                self.event(Event::Close { tab, revision });
            }
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

    /// A chord to the shown document: what the pane made of it, a
    /// request being the host's to serve.
    pub fn chord(&mut self, chord: &str) -> Outcome {
        let Some((tab, revision)) = self.target() else {
            return Outcome::Ignored;
        };
        if chord.is_empty() {
            return Outcome::Ignored;
        }
        self.event(Event::Key {
            tab,
            revision,
            chord,
        })
    }

    /// Whether the shown document may be edited: a read-only text is
    /// left alone by a cut or a paste, without a refusal logged.
    pub fn editable(&self) -> bool {
        self.tab.is_some_and(|tab| {
            self.controller
                .editor()
                .document(tab)
                .is_ok_and(|document| !document.read_only())
        })
    }

    /// The selection into the kill ring and out of the document: whether
    /// one was, or why the selection could not be captured (past the
    /// clipboard's ceiling).
    pub fn cut(&mut self) -> Result<bool, String> {
        if !self.editable() {
            return Ok(false);
        }
        let Some(snapshot) = self.selection()? else {
            return Ok(false);
        };
        let text = snapshot.text();
        if self.event(Event::Cut(snapshot)) != Outcome::Changed {
            return Ok(false);
        }
        self.kill = Some(text);
        Ok(true)
    }

    /// The selection into the kill ring: whether one was kept, or why it
    /// could not be captured.
    pub fn copy(&mut self) -> Result<bool, String> {
        match self.selection()? {
            Some(snapshot) => {
                self.kill = Some(snapshot.text());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The kill ring's text: the last selection cut or copied.
    pub fn killed(&self) -> Option<Arc<str>> {
        self.kill.clone()
    }

    /// `text` on a line of its own in the document `tab`, as a paste is
    /// put: below the line `body` (the draft's separator) at the caret's
    /// line when the caret is there (`line_at`; of a range, its lower
    /// end's line, so the text never goes inside it), else, and without
    /// `body`, at the end. The caret is put there first by selection, not
    /// by a chord (which the pane's keymap may take otherwise, or refuse
    /// unfocused), and the text goes in only when it is there; a newline
    /// goes before the text at the end of a document that does not end
    /// in one, and after it when it has none. The
    /// selection is then put back where it was, as it is when the text is
    /// refused, so typing goes on where it was going: text before the
    /// place does not move and text after it moves with it; a caret at
    /// the place keeps to its own line, staying before the newline put in
    /// at a document's end, else moving after the text, never into its
    /// line, and a range ending at the place stays before the text rather
    /// than taking it in. Whether the document changed (a read-only one
    /// does not), or why the text was refused.
    fn put_line(&mut self, tab: TabId, text: &str, body: Option<&str>) -> Result<bool, String> {
        let Ok(document) = self.controller.editor().document(tab) else {
            return Ok(false);
        };
        if document.read_only() {
            return Ok(false);
        }
        let end = document.text().len();
        let before = document.selection();
        let high = before.anchor.max(before.caret);
        let at = body.map_or(end, |separator| line_at(document.text(), high, separator));
        let lined = document.text().is_empty() || document.text().ends_with('\n');
        let mut text = if at == end && !lined {
            format!("\n{text}")
        } else {
            text.to_string()
        };
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.select(
            tab,
            Selection {
                anchor: at,
                caret: at,
            },
        );
        let placed = self
            .controller
            .editor()
            .document(tab)
            .is_ok_and(|document| {
                let selection = document.selection();
                document.text().len() == end && selection.anchor == at && selection.caret == at
            });
        if !placed {
            self.select(tab, before);
            return Err("the draft's place for it could not be reached".to_string());
        }
        let Some((tab, revision)) = self.target_of(tab) else {
            return Ok(false);
        };
        let paste = Paste::begin(self.controller.editor(), tab, revision)
            .and_then(|mut paste| {
                paste.push(text.as_bytes())?;
                Ok(paste)
            })
            .map_err(|error| describe(error, "the text"));
        let changed = match paste {
            Ok(paste) => self.event(Event::Paste(paste)) == Outcome::Changed,
            Err(why) => {
                self.select(tab, before);
                return Err(why);
            }
        };
        if !changed {
            self.select(tab, before);
            return Ok(false);
        }
        let len = text.len();
        let range = before.anchor != before.caret;
        let back = |p: usize| {
            if p < at || (p == at && ((at == end && !lined) || (range && p == high))) {
                p
            } else {
                p.saturating_add(len)
            }
        };
        self.select(
            tab,
            Selection {
                anchor: back(before.anchor),
                caret: back(before.caret),
            },
        );
        Ok(true)
    }

    /// The document `tab`'s selection set, by the editor's own command,
    /// at its current revision.
    fn select(&mut self, tab: TabId, selection: Selection) {
        if let Some((tab, revision)) = self.target_of(tab) {
            self.event(Event::Edit {
                tab,
                revision,
                command: Command::Select(selection),
            });
        }
    }

    /// The kill ring into the document, over its selection: whether the
    /// document changed, or why the paste was refused.
    pub fn paste(&mut self) -> Result<bool, String> {
        let Some(text) = self.kill.clone() else {
            return Ok(false);
        };
        self.insert(&text)
    }

    /// `text` into the document, over its selection, as a paste is: the
    /// kill ring's or the system clipboard's. Whether the document
    /// changed (a read-only document, no document, or an empty text
    /// changes nothing), or why the paste was refused.
    pub fn insert(&mut self, text: &str) -> Result<bool, String> {
        if !self.editable() {
            return Ok(false);
        }
        let Some((tab, revision)) = self.target() else {
            return Ok(false);
        };
        let paste = Paste::begin(self.controller.editor(), tab, revision)
            .and_then(|mut paste| {
                paste.push(text.as_bytes())?;
                Ok(paste)
            })
            .map_err(|error| describe(error, "the text"))?;
        Ok(self.event(Event::Paste(paste)) == Outcome::Changed)
    }

    /// The shown document's selection, bounded as the editor's clipboard
    /// bounds it; none when nothing is selected, and the reason when it
    /// cannot be captured.
    fn selection(&self) -> Result<Option<Snapshot>, String> {
        let Some((tab, revision)) = self.target() else {
            return Ok(None);
        };
        Snapshot::capture(self.controller.editor(), tab, revision)
            .map_err(|error| describe(error, "the selection"))
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

/// A view's draft, as the view saves or gives it up: the document its
/// slot holds, whether or not the pane shows it this frame, so a save
/// never snapshots another view's document; none when the pane refused
/// the draft.
pub struct Draft<'a> {
    pane: &'a mut Pane,
    tab: Option<TabId>,
}

impl<'a> Draft<'a> {
    pub fn new(pane: &'a mut Pane, tab: Option<TabId>) -> Self {
        Draft { pane, tab }
    }

    /// Whether the document has changed since it was loaded or saved.
    pub fn dirty(&self) -> bool {
        let Some(tab) = self.tab else {
            return false;
        };
        self.pane
            .controller
            .editor()
            .document(tab)
            .is_ok_and(|document| document.dirty())
    }

    /// The document's text as bytes to write, with the token that marks
    /// the document saved at that state once they are written.
    pub fn snapshot(&self) -> Option<(SavePoint, Vec<u8>)> {
        let tab = self.tab?;
        self.pane.controller.editor().save_snapshot(tab).ok()
    }

    /// The bytes of `snapshot` were written.
    pub fn saved(&mut self, point: SavePoint) {
        self.pane.event(Event::Saved(point));
    }

    /// The document's changes are given up: it counts as saved as it
    /// is, without a write, so its view can close it.
    pub fn discard(&mut self) {
        if let Some((point, _)) = self.snapshot() {
            self.saved(point);
        }
    }

    /// `text` on a line of its own at the caret's line when the caret is
    /// below the line `body`, else at the end of the document, the
    /// selection kept (`Pane::put_line`): whether the document changed,
    /// or why it was refused.
    pub fn put_line(&mut self, text: &str, body: Option<&str>) -> Result<bool, String> {
        match self.tab {
            Some(tab) => self.pane.put_line(tab, text, body),
            None => Ok(false),
        }
    }

    /// Whether the document is held read-only: the pane keeps its keys,
    /// selection, motion and find included, and typing is ignored.
    pub fn hold(&mut self, held: bool) {
        if let Some(tab) = self.tab {
            self.pane.event(Event::ReadOnly { tab, enabled: held });
        }
    }
}

/// Where a line goes in `text` for a caret at byte `caret`: below the
/// line `separator`, in the body, the start of the caret's line when the
/// caret starts it, else the start of the next line (the end when there
/// is none), and past a `<#/part>` line that would follow a `<#part`
/// line there, so a tag never goes between another and its close; with
/// the caret on or above the separator, or no separator, the end.
fn line_at(text: &str, caret: usize, separator: &str) -> usize {
    let end = text.len();
    let mut start = 0;
    let mut body = None;
    for line in text.split_inclusive('\n') {
        start += line.len();
        if line.trim_end() == separator {
            body = Some(start);
            break;
        }
    }
    let Some(body) = body.filter(|body| caret >= *body && caret <= end) else {
        return end;
    };
    let starts = caret == body
        || caret
            .checked_sub(1)
            .and_then(|i| text.as_bytes().get(i))
            .is_some_and(|b| *b == b'\n');
    let at = if starts {
        caret
    } else {
        match text.get(caret..).and_then(|rest| rest.find('\n')) {
            Some(i) => caret + i + 1,
            None => end,
        }
    };
    let previous = text
        .get(..at)
        .and_then(|head| head.strip_suffix('\n'))
        .map(|head| head.rsplit('\n').next().unwrap_or(head));
    let next = text
        .get(at..)
        .map(|rest| rest.split('\n').next().unwrap_or(rest));
    let tag = |line: &str| {
        line.trim_end()
            .strip_prefix("<#part")
            .is_some_and(|rest| rest.starts_with([' ', '\t', '>']))
    };
    if previous.is_some_and(tag) && next.is_some_and(|line| line.trim_end() == "<#/part>") {
        return match text.get(at..).and_then(|rest| rest.find('\n')) {
            Some(i) => at + i + 1,
            None => end,
        };
    }
    at
}

/// `text` as the pane admits it: a leading byte order mark dropped, CRLF
/// one newline, a control scalar other than newline and tab (which the
/// editor refuses) the replacement character, and at most the editor's
/// file ceiling in bytes, the rest dropped: a text to read is shown as
/// far as it fits.
pub fn pane_source(text: &str) -> String {
    admit(text, true).unwrap_or_default()
}

/// `text` as an editable document: admitted as `pane_source` admits a
/// text, but none past the editor's ceiling rather than shortened, so
/// a save can never write a shortened draft over the file.
pub fn draft_source(text: &str) -> Option<String> {
    admit(text, false)
}

fn admit(text: &str, truncate: bool) -> Option<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut source = String::with_capacity(text.len().min(td_editor::text::MAX_FILE_BYTES));
    for c in text.replace("\r\n", "\n").chars() {
        let shown = match c {
            '\n' | '\t' => c,
            c if c <= '\u{1f}' || c == '\u{7f}' => '\u{fffd}',
            c => c,
        };
        if source.len() + shown.len_utf8() > td_editor::text::MAX_FILE_BYTES {
            if truncate {
                break;
            }
            return None;
        }
        source.push(shown);
    }
    Some(source)
}

/// The frame as a composition: what the window paints and what a test
/// reads back.
pub struct Frame<'a> {
    pub surface: Surface,
    pub scene: &'a Scene<'a>,
    pub first: usize,
    pub entry_first: usize,
    pub pane: &'a Pane,
    /// The finder open over the body, painted in its place.
    pub chooser: Option<&'a finder::Controller>,
    /// The dropdown open over the frame, painted last.
    pub menu: Option<&'a Dropdown>,
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
            _ if self.chooser.is_some() => {}
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
            Body::Text { .. } | Body::Edit { .. } => {
                if layout.pane.is_some() {
                    self.pane.emit(damage, sink);
                }
            }
        }
        if let Some(chooser) = self.chooser {
            chooser.emit(damage, sink);
        }
        layout.status.emit(self.scene.status.chars(), damage, sink);
        if let Some(menu) = self.menu {
            menu.emit(damage, sink);
        }
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

    #[test]
    fn draft_pane_uses_reflow_and_center_shortcuts() {
        let mut pane = Pane::new().unwrap();
        pane.place(
            Rect {
                x: 0,
                y: 0,
                width: 240,
                height: 64,
            },
            Surface::new(240, 64, Default::default()).unwrap(),
        );
        let mut draft = None;
        pane.edit(&mut draft, "shortcuts", || {
            "alpha bravo\ncharlie delta\n\none\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten"
                .to_string()
        });
        let tab = pane.tab().unwrap();
        assert_eq!(pane.chord("M-q"), Outcome::Changed);
        assert!(pane
            .editor()
            .document(tab)
            .unwrap()
            .text()
            .starts_with("alpha bravo charlie delta"));
        assert_eq!(pane.chord("C-Home"), Outcome::Changed);
        for _ in 0..7 {
            assert_eq!(pane.chord("Down"), Outcome::Changed);
        }
        let before = pane.first_row().unwrap();
        let selection = pane.editor().document(tab).unwrap().selection();
        let revision = pane.editor().document(tab).unwrap().revision();
        assert_eq!(pane.chord("C-l"), Outcome::Changed);
        assert_eq!(pane.first_row(), Some(5));
        assert_ne!(before, 5);
        assert_eq!(pane.editor().document(tab).unwrap().selection(), selection);
        assert_eq!(pane.editor().document(tab).unwrap().revision(), revision);
    }

    /// A draft is loaded once for its key, editable and auto-filled; the
    /// kill ring carries a selection between documents; and the draft
    /// handle reads the document's state, marks it saved and gives it up.
    #[test]
    fn a_draft_is_edited_in_place_and_the_kill_ring_carries_a_selection() {
        let mut pane = Pane::new().unwrap();
        pane.place(
            Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 64,
            },
            Surface::new(400, 64, Default::default()).unwrap(),
        );
        let mut draft = None;
        pane.edit(&mut draft, "d", || "To: \n".to_string());
        let tab = pane.tab().expect("the draft");
        let document = pane.editor().document(tab).unwrap();
        assert!(!document.read_only());
        assert!(document.auto_fill());
        assert!(!document.dirty(), "loaded is saved");
        pane.edit(&mut draft, "d", || unreachable!("loaded once"));
        assert_eq!(pane.tab(), Some(tab));
        assert_eq!(pane.chord("C-End"), Outcome::Changed);
        assert_eq!(pane.chord("x"), Outcome::Changed);
        assert!(matches!(
            pane.chord("C-s"),
            Outcome::Request { name: "save", .. }
        ));
        assert_eq!(pane.editor().document(tab).unwrap().text(), "To: \nx");
        assert!(Draft::new(&mut pane, Some(tab)).dirty());
        // Nothing selected: nothing cut, copied or pasted.
        assert_eq!(pane.cut(), Ok(false));
        assert_eq!(pane.copy(), Ok(false));
        assert_eq!(pane.paste(), Ok(false));
        // A selection copied from a read-only text is pasted into the draft.
        let mut text = None;
        pane.show(&mut text, "t", 40, || "quoted".to_string());
        assert_eq!(pane.chord("C-a"), Outcome::Changed);
        assert!(matches!(
            pane.chord("C-c"),
            Outcome::Request { name: "copy", .. }
        ));
        assert_eq!(pane.copy(), Ok(true), "the selection is kept");
        assert_eq!(pane.paste(), Ok(false), "read-only: nothing pasted");
        assert_eq!(pane.cut(), Ok(false), "read-only: nothing cut");
        let shown = pane.tab().expect("the text");
        assert_eq!(pane.editor().document(shown).unwrap().text(), "quoted");
        pane.edit(&mut draft, "d", || unreachable!("held"));
        assert_eq!(pane.paste(), Ok(true));
        assert_eq!(pane.editor().document(tab).unwrap().text(), "To: \nxquoted");
        // Cut takes the selection out and paste brings it back; an empty
        // text inserted changes nothing.
        assert_eq!(pane.chord("C-a"), Outcome::Changed);
        assert_eq!(pane.cut(), Ok(true));
        assert_eq!(pane.editor().document(tab).unwrap().text(), "");
        assert_eq!(pane.insert(""), Ok(false));
        assert_eq!(pane.paste(), Ok(true));
        assert_eq!(pane.editor().document(tab).unwrap().text(), "To: \nxquoted");
        // A selection past the clipboard's ceiling is refused with the
        // reason, and the kill ring keeps what it had.
        let mut big = None;
        pane.show(&mut big, "big", 40, || {
            "x".repeat(td_editor::clipboard::MAX_BYTES + 1)
        });
        assert_eq!(pane.chord("C-a"), Outcome::Changed);
        let refused = pane.copy().unwrap_err();
        assert_eq!(
            refused,
            "the selection is past the clipboard's 1024 KiB ceiling"
        );
        assert_eq!(pane.killed().as_deref(), Some("To: \nxquoted"));
        pane.close(big);
        pane.edit(&mut draft, "d", || unreachable!("held"));
        // Saved at a snapshot, the draft is clean; edited, dirty; given up, clean again.
        let (point, bytes) = Draft::new(&mut pane, Some(tab)).snapshot().unwrap();
        assert_eq!(bytes, b"To: \nxquoted");
        Draft::new(&mut pane, Some(tab)).saved(point);
        assert!(!Draft::new(&mut pane, Some(tab)).dirty());
        assert_eq!(pane.chord("y"), Outcome::Changed);
        assert!(Draft::new(&mut pane, Some(tab)).dirty());
        // The handle is the view's document, whichever is shown.
        pane.show(&mut text, "t", 40, || unreachable!("held"));
        assert!(Draft::new(&mut pane, Some(tab)).dirty());
        let shown = pane.tab();
        assert!(!Draft::new(&mut pane, shown).dirty(), "the text is clean");
        assert!(Draft::new(&mut pane, None).snapshot().is_none());
        Draft::new(&mut pane, Some(tab)).discard();
        assert!(!Draft::new(&mut pane, Some(tab)).dirty());
        // A draft past the ceiling is refused whole; a text is cut to it.
        let long = "x".repeat(td_editor::text::MAX_FILE_BYTES + 1);
        assert_eq!(draft_source(&long), None);
        assert_eq!(pane_source(&long).len(), td_editor::text::MAX_FILE_BYTES);
        assert_eq!(
            draft_source("a\r\nb\u{1}"),
            Some("a\nb\u{fffd}".to_string())
        );
        let mut refused = None;
        pane.edit(&mut refused, "big", || long.clone());
        assert!(refused.is_none());
        assert_eq!(pane.tab(), None);
        pane.close(draft);
        pane.close(text);
        assert_eq!(pane.editor().tabs().count(), 0);
    }

    /// Appended text goes on a line of its own at the end of the draft,
    /// wherever the caret was, the selection kept, and a caret at the end
    /// kept out of the appended line; a held draft, a read-only text and
    /// no document take nothing.
    #[test]
    fn appended_text_goes_on_its_own_line_at_the_drafts_end() {
        let mut pane = Pane::new().unwrap();
        let mut draft = None;
        pane.edit(&mut draft, "d", || {
            "To: a\n--text follows this line--\nhi".to_string()
        });
        let tab = pane.tab().expect("the draft");
        assert_eq!(pane.chord("C-Home"), Outcome::Changed);
        // Unfocused, where the pane takes no chord, the end is still
        // reached.
        pane.focus(false);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#part>\n", None),
            Ok(true)
        );
        pane.focus(true);
        let text = |pane: &Pane| pane.editor().document(tab).unwrap().text().to_string();
        assert_eq!(
            text(&pane),
            "To: a\n--text follows this line--\nhi\n<#part>\n"
        );
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#two>\n", None),
            Ok(true)
        );
        assert_eq!(pane.chord("x"), Outcome::Changed);
        assert_eq!(
            text(&pane),
            "xTo: a\n--text follows this line--\nhi\n<#part>\n<#two>\n",
            "the caret stays where it was"
        );
        // A selection before the end is kept as it was.
        let select = |pane: &mut Pane, anchor: usize, caret: usize| {
            let revision = pane.editor().document(tab).unwrap().revision();
            pane.event(Event::Edit {
                tab,
                revision,
                command: Command::Select(Selection { anchor, caret }),
            });
        };
        select(&mut pane, 1, 3);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#three>\n", None),
            Ok(true)
        );
        let selection = pane.editor().document(tab).unwrap().selection();
        assert_eq!((selection.anchor, selection.caret), (1, 3));
        // At the end after a newline, the caret goes past the appended
        // line rather than into it.
        let end = text(&pane).len();
        select(&mut pane, end, end);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#four>\n", None),
            Ok(true)
        );
        assert_eq!(pane.chord("y"), Outcome::Changed);
        assert!(
            text(&pane).ends_with("<#three>\n<#four>\ny"),
            "{}",
            text(&pane)
        );
        // At the end of a last line, it stays on that line.
        assert_eq!(pane.chord("C-End"), Outcome::Changed);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#five>\n", None),
            Ok(true)
        );
        assert_eq!(pane.chord("z"), Outcome::Changed);
        assert!(
            text(&pane).ends_with("<#four>\nyz\n<#five>\n"),
            "{}",
            text(&pane)
        );
        // A range reaching the end stays before the appended line, so
        // what is typed over it does not take the line with it.
        let end = text(&pane).len();
        select(&mut pane, end - 3, end);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#six>\n", None),
            Ok(true)
        );
        let selection = pane.editor().document(tab).unwrap().selection();
        assert_eq!((selection.anchor, selection.caret), (end - 3, end));
        assert_eq!(pane.chord("Backspace"), Outcome::Changed);
        assert!(
            text(&pane).ends_with("<#four>\nyz\n<#fiv<#six>\n"),
            "{}",
            text(&pane)
        );
        Draft::new(&mut pane, Some(tab)).hold(true);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("no", None),
            Ok(false)
        );
        let mut shown = None;
        pane.show(&mut shown, "t", 40, || "read".to_string());
        let read = pane.tab();
        assert_eq!(Draft::new(&mut pane, read).put_line("no", None), Ok(false));
        assert_eq!(Draft::new(&mut pane, None).put_line("no", None), Ok(false));
        assert_eq!(
            pane.editor().document(read.unwrap()).unwrap().text(),
            "read"
        );
        pane.close(draft);
        pane.close(shown);
    }

    /// A line's place: in the body at the caret's line, before it when the
    /// caret starts it, after it otherwise, and never between a tag and
    /// its close; in the headers, or with no separator, at the end.
    #[test]
    fn a_line_goes_at_the_carets_line_in_the_body() {
        let sep = "--text follows this line--";
        let text = "To: a\n--text follows this line--\none\ntwo\n<#part x>\n<#/part>\nend";
        let at = |needle: &str| text.find(needle).unwrap();
        // In the headers and on the separator: the end.
        assert_eq!(line_at(text, 2, sep), text.len());
        assert_eq!(line_at(text, at("--text") + 3, sep), text.len());
        // The body's first line: at its start, before it; inside it, after.
        assert_eq!(line_at(text, at("one"), sep), at("one"));
        assert_eq!(line_at(text, at("one") + 1, sep), at("two"));
        assert_eq!(line_at(text, at("two") + 3, sep), at("<#part"));
        // On a tag, or at its close's start: past the close.
        assert_eq!(line_at(text, at("<#part") + 2, sep), at("end"));
        assert_eq!(line_at(text, at("<#/part>"), sep), at("end"));
        assert_eq!(line_at(text, at("<#/part>") + 1, sep), at("end"));
        // The last line, unended: the end.
        assert_eq!(line_at(text, at("end") + 1, sep), text.len());
        assert_eq!(line_at(text, text.len(), sep), text.len());
        // A line that only looks like a tag is none.
        let party = "S: x\n--text follows this line--\n<#party>\n<#/part>\nz";
        assert_eq!(
            line_at(party, party.find("<#party>").unwrap() + 2, sep),
            party.find("<#/part>").unwrap()
        );
        // Past the text, or no separator: the end.
        assert_eq!(line_at(text, text.len() + 5, sep), text.len());
        assert_eq!(line_at("one\ntwo\n", 1, sep), 8);
    }

    /// With the caret in the body the line goes at the caret's line and
    /// the selection keeps to the text it was on; in the headers it goes
    /// at the end.
    #[test]
    fn a_put_line_goes_where_the_caret_is_in_the_body() {
        let sep = "--text follows this line--";
        let mut pane = Pane::new().unwrap();
        let mut draft = None;
        pane.edit(&mut draft, "d", || {
            "To: a\n--text follows this line--\nhello world\nbye\n".to_string()
        });
        let tab = pane.tab().expect("the draft");
        let text = |pane: &Pane| pane.editor().document(tab).unwrap().text().to_string();
        let select = |pane: &mut Pane, anchor: usize, caret: usize| {
            let revision = pane.editor().document(tab).unwrap().revision();
            pane.event(Event::Edit {
                tab,
                revision,
                command: Command::Select(Selection { anchor, caret }),
            });
        };
        let hello = text(&pane).find("hello").unwrap();
        // Mid-line: after the line, the caret still mid-word.
        select(&mut pane, hello + 5, hello + 5);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#a>\n", Some(sep)),
            Ok(true)
        );
        assert_eq!(
            text(&pane),
            "To: a\n--text follows this line--\nhello world\n<#a>\nbye\n"
        );
        assert_eq!(pane.chord("x"), Outcome::Changed);
        assert!(
            text(&pane).contains("hellox world\n<#a>\n"),
            "{}",
            text(&pane)
        );
        // At a line's start: before the line, the caret staying with it.
        let bye = text(&pane).find("bye").unwrap();
        select(&mut pane, bye, bye);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#b>", Some(sep)),
            Ok(true)
        );
        assert_eq!(pane.chord("y"), Outcome::Changed);
        assert!(
            text(&pane).ends_with("<#a>\n<#b>\nybye\n"),
            "{}",
            text(&pane)
        );
        // A range ending where the line goes stays before it; one after
        // it moves with its text.
        let a = text(&pane).find("<#a>").unwrap();
        select(&mut pane, a - 6, a);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#c>\n", Some(sep)),
            Ok(true)
        );
        let selection = pane.editor().document(tab).unwrap().selection();
        assert_eq!((selection.anchor, selection.caret), (a - 6, a));
        assert!(
            text(&pane).contains("world\n<#c>\n<#a>\n"),
            "{}",
            text(&pane)
        );
        // A range reversed over a line boundary (anchor below the caret)
        // is placed after by its lower end's line, and not taken in.
        let world = text(&pane).find("world").unwrap();
        let ybye = text(&pane).find("ybye").unwrap();
        select(&mut pane, ybye + 2, world);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#r>\n", Some(sep)),
            Ok(true)
        );
        let selection = pane.editor().document(tab).unwrap().selection();
        assert_eq!((selection.anchor, selection.caret), (ybye + 2, world));
        assert!(text(&pane).contains("ybye\n<#r>\n"), "{}", text(&pane));
        // In the headers: at the end.
        select(&mut pane, 1, 1);
        assert_eq!(
            Draft::new(&mut pane, Some(tab)).put_line("<#d>\n", Some(sep)),
            Ok(true)
        );
        assert!(
            text(&pane).ends_with("ybye\n<#r>\n<#d>\n"),
            "{}",
            text(&pane)
        );
        let selection = pane.editor().document(tab).unwrap().selection();
        assert_eq!((selection.anchor, selection.caret), (1, 1));
        pane.close(draft);
    }
}
