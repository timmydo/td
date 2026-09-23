//! The client's views, and the scene each shows.
//!
//! A view keeps its state, its keys and its backend traffic, and describes
//! what it shows as a `Scene`: the window's title, an action bar whose
//! labels stand for keys, an optional text entry, a body that is a list of
//! rows, a text or a draft to edit, and the status row. The session
//! (`super`) lays the scene out with the toolkit's widgets and td-editor's
//! document pane, and hands the view its keys, a press on a row and the
//! wheel's travel over a list; scrolling a text is the pane's, which a
//! view asks for with `ViewAction::Scroll`. A draft's pane takes every
//! chord, and what it asks of its host (a save, a close) reaches the view
//! through `View::request`, with the draft in hand.

pub mod compose;
pub mod email_list;
pub mod email_view;
pub mod help;
pub mod mailbox_list;
pub mod retention_preview;
pub mod rules_preview;
pub mod thread_view;

use super::input::Key;
use crate::backend::BackendResponse;
use crate::civil::{self, Zone};
use std::sync::OnceLock;
use std::time::SystemTime;

/// The system zone, read once. `Zone::local` opens and parses `/etc/localtime`,
/// and this is called from a render path.
static ZONE: OnceLock<Zone> = OnceLock::new();

pub fn format_system_time(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let (local, _offset) = ZONE.get_or_init(Zone::local).to_local(secs);
    civil::format_hms(&local)
}

/// One row of a list: its label, the note at its right, and whether it is
/// marked (unread, or otherwise wanting the eye).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub meta: String,
    pub marked: bool,
}

/// A text entry above the body: what is typed, and what the empty field
/// says. The caret is at the text's end.
pub struct Entry<'a> {
    pub placeholder: &'static str,
    pub text: &'a str,
}

/// What fills the body of the window.
pub enum Body<'a> {
    /// `total` rows, `row(index)` naming each, the cursor on `selected`.
    List {
        total: usize,
        selected: usize,
        row: Box<dyn Fn(usize) -> Row + 'a>,
    },
    /// A text in the read-only document pane: `text(columns)` wrapped for
    /// the pane's columns, reloaded only when `key` or the columns change.
    Text {
        key: String,
        text: Box<dyn Fn(usize) -> String + 'a>,
    },
    /// A draft in the editable pane: `text()` loaded once for `key`, and
    /// then the pane's, edited in place. While `focused`, every chord
    /// is the pane's; the view's keys are then the bar's labels.
    Edit {
        key: String,
        text: Box<dyn Fn() -> String + 'a>,
        focused: bool,
    },
}

impl Body<'static> {
    /// A message in the pane: a loading note, an error, an empty list's
    /// word.
    pub fn message(text: String) -> Self {
        let key = format!("message:{text}");
        Body::Text {
            key,
            text: Box::new(move |columns| wrap_text(&text, columns)),
        }
    }
}

/// What a view shows this frame.
pub struct Scene<'a> {
    pub title: String,
    /// The action bar's labels, and the key each stands for, in order.
    pub labels: &'static [&'static str],
    pub keys: &'static [Key],
    pub entry: Option<Entry<'a>>,
    pub body: Body<'a>,
    pub status: String,
}

/// How a view moves the pane its text is shown in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    Lines(isize),
    Pages(isize),
    /// One of the pane's own chords: an arrow, Home or End move its caret
    /// and the view follows.
    Chord(&'static str),
}

/// The reading keys over a text body: j, k and the wheel a line, Space
/// and the page keys a page, the arrows, Home and End the pane's own.
pub fn text_scroll(key: Key) -> Option<Scroll> {
    Some(match key {
        Key::Char('j') | Key::ScrollDown => Scroll::Lines(1),
        Key::Char('k') | Key::ScrollUp => Scroll::Lines(-1),
        Key::PageDown | Key::Char(' ') => Scroll::Pages(1),
        Key::PageUp => Scroll::Pages(-1),
        Key::Up => Scroll::Chord("Up"),
        Key::Down => Scroll::Chord("Down"),
        Key::Home => Scroll::Chord("Home"),
        Key::End => Scroll::Chord("End"),
        _ => return None,
    })
}

pub enum ViewAction {
    Continue,
    Push(Box<dyn View>),
    Pop,
    Quit,
    /// Retain the draft and edit it in the pane.
    Compose(crate::compose::ComposeDraft),
    SwitchAccount(String),
    Scroll(Scroll),
    /// A request of the pane's kind, served as the pane's own are: the
    /// session answers the clipboard's, and hands the view the rest
    /// through `View::request`.
    Request(&'static str),
    /// Open the finder over the body for a file to attach to the view's
    /// draft; what is chosen, or that nothing was, reaches the view
    /// through `View::attach`.
    ChooseAttachment,
}

pub trait View {
    /// What the view shows.
    fn scene(&self) -> Scene<'_>;
    /// A key, a press on a row or the wheel's travel; `page` is the rows
    /// the body shows, which the page keys move by.
    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction;
    /// A request the pane raised from a chord, or a bar label stood
    /// for, with the draft the pane shows: `save`, `close-tab`, `quit`,
    /// or one the view ignores.
    fn request(&mut self, _name: &str, _draft: &mut super::frame::Draft<'_>) -> ViewAction {
        ViewAction::Continue
    }
    /// The file the finder a `ChooseAttachment` opened was closed on,
    /// to attach to the draft the pane holds for the view; none when it
    /// was closed without one.
    fn attach(
        &mut self,
        _chosen: Option<&std::path::Path>,
        _draft: &mut super::frame::Draft<'_>,
    ) -> ViewAction {
        ViewAction::Continue
    }
    /// Handle a response from the backend thread.
    /// Returns true if the view consumed the response and should re-render.
    fn on_response(&mut self, response: &BackendResponse) -> bool;
    /// Check for a pending action triggered by an async response.
    fn take_pending_action(&mut self) -> Option<ViewAction> {
        None
    }
    /// Trigger a background sync after the UI has been idle.
    /// Returns true if this changed view state and should re-render.
    fn trigger_idle_sync(&mut self) -> bool {
        false
    }
    /// Called when this view becomes the top of the stack again after a child
    /// view was popped (e.g. returning from a folder). Lets a view refresh
    /// state that may have changed while it was hidden. Returns true if it
    /// changed state and should re-render.
    fn on_reveal(&mut self) -> bool {
        false
    }
    /// Whether the view is waiting on the backend for what it holds, so
    /// the window's close request is put to it rather than taken.
    fn waiting(&self) -> bool {
        false
    }
    /// Whether the view's draft is not to change: read-only in the pane
    /// while the view waits on the backend for it, and once the server
    /// has taken it, so the file retired is the file sent.
    fn held(&self) -> bool {
        false
    }
}

/// A view on the stack with what the frame keeps for it: the first row
/// its list shows, the first scalar its entry shows, and its document
/// in the pane.
pub struct Slot {
    pub view: Box<dyn View>,
    pub first: usize,
    pub entry_first: usize,
    pub text: Option<super::frame::Shown>,
}

pub struct ViewStack {
    slots: Vec<Slot>,
}

impl ViewStack {
    pub fn new(initial: Box<dyn View>) -> Self {
        ViewStack {
            slots: vec![Slot {
                view: initial,
                first: 0,
                entry_first: 0,
                text: None,
            }],
        }
    }

    pub fn handle_key(&mut self, key: Key, page: usize) -> Option<ViewAction> {
        self.slots
            .last_mut()
            .map(|slot| slot.view.handle_key(key, page))
    }

    /// Route a backend response to all views (top-most can trigger re-render).
    pub fn handle_response(&mut self, response: &BackendResponse) -> bool {
        let top = self.slots.len().saturating_sub(1);
        let mut needs_render = false;
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.view.on_response(response) && idx == top {
                needs_render = true;
            }
        }
        needs_render
    }

    pub fn current_mut(&mut self) -> Option<&mut Box<dyn View>> {
        self.slots.last_mut().map(|slot| &mut slot.view)
    }

    pub fn current(&self) -> Option<&dyn View> {
        self.slots.last().map(|slot| slot.view.as_ref())
    }

    pub fn top(&self) -> Option<&Slot> {
        self.slots.last()
    }

    pub fn top_mut(&mut self) -> Option<&mut Slot> {
        self.slots.last_mut()
    }

    pub fn push(&mut self, view: Box<dyn View>) {
        self.slots.push(Slot {
            view,
            first: 0,
            entry_first: 0,
            text: None,
        });
    }

    /// The top view, popped; none for the last, which is not.
    pub fn pop(&mut self) -> Option<Slot> {
        if self.slots.len() > 1 {
            self.slots.pop()
        } else {
            None
        }
    }

    pub fn into_slots(self) -> Vec<Slot> {
        self.slots
    }

    #[cfg(test)]
    pub fn depth(&self) -> usize {
        self.slots.len()
    }
}

/// `text` with each line wrapped at `columns` scalars, broken at the last
/// space that fits or, failing one, at the column; a line that fits, and
/// every line at no columns, is itself.
pub fn wrap_text(text: &str, columns: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut remaining = line;
        loop {
            // The scalar past the columns, if there is one: a line of
            // any length is walked once, not counted whole per piece.
            let past = if columns == 0 {
                None
            } else {
                remaining.char_indices().nth(columns)
            };
            let Some((end, _)) = past else {
                out.push_str(remaining);
                break;
            };
            let head = remaining.get(..end).unwrap_or_default();
            let tail = remaining.get(end..).unwrap_or_default();
            // A space just past the columns is the break itself; the
            // spaces leading the line are its indentation, not a break.
            let indented = |space: usize| {
                head.get(..space)
                    .is_some_and(|piece| piece.chars().any(|c| c != ' '))
            };
            let (piece, rest) = match tail.strip_prefix(' ') {
                Some(rest) => (head, rest),
                None => match head.rfind(' ').filter(|&space| indented(space)) {
                    Some(space) => (
                        remaining.get(..space).unwrap_or_default(),
                        remaining.get(space + 1..).unwrap_or_default(),
                    ),
                    None => (head, tail),
                },
            };
            out.push_str(piece);
            out.push('\n');
            remaining = rest;
        }
    }
    out
}

/// `s` on one line: a newline or a tab is a space.
pub fn strip_newlines(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_wraps_at_a_space_that_fits_or_at_the_column() {
        assert_eq!(wrap_text("", 4), "");
        assert_eq!(wrap_text("ab cd", 0), "ab cd");
        assert_eq!(wrap_text("ab cd", 5), "ab cd");
        assert_eq!(wrap_text("ab cd", 4), "ab\ncd");
        assert_eq!(wrap_text("abcdef", 4), "abcd\nef");
        assert_eq!(wrap_text("a\n\nbb cc dd", 5), "a\n\nbb cc\ndd");
        assert_eq!(wrap_text("héllo wörld", 5), "héllo\nwörld");
        assert_eq!(wrap_text("a b c d e", 3), "a b\nc d\ne");
        assert_eq!(wrap_text(" hello there", 8), " hello\nthere");
        assert_eq!(wrap_text(" hello", 5), " hell\no");
        assert_eq!(
            wrap_text("    long_token_here", 12),
            "    long_tok\nen_here"
        );
        assert_eq!(wrap_text("  a bb", 4), "  a\nbb");
        assert_eq!(wrap_text("> quoted text", 9), "> quoted\ntext");
        assert_eq!(wrap_text(" > text", 4), " >\ntext");
    }

    #[test]
    fn the_reading_keys_scroll_a_text_and_others_are_the_views() {
        assert_eq!(text_scroll(Key::Char('j')), Some(Scroll::Lines(1)));
        assert_eq!(text_scroll(Key::ScrollUp), Some(Scroll::Lines(-1)));
        assert_eq!(text_scroll(Key::Char(' ')), Some(Scroll::Pages(1)));
        assert_eq!(text_scroll(Key::PageUp), Some(Scroll::Pages(-1)));
        assert_eq!(text_scroll(Key::End), Some(Scroll::Chord("End")));
        assert_eq!(text_scroll(Key::Char('q')), None);
        assert_eq!(text_scroll(Key::Enter), None);
        assert_eq!(text_scroll(Key::Click(0)), None);
    }

    #[test]
    fn a_message_body_is_its_own_key() {
        let Body::Text { key, text } = Body::message("Loading...".to_string()) else {
            panic!("a text");
        };
        assert_eq!(key, "message:Loading...");
        assert_eq!(text(4), "Load\ning.\n..");
    }
}
