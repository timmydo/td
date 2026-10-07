//! td-agent's notes to the human and the Messages window (DESIGN.md §4).
//! A note (a refusal said by name, a step done, a background
//! conversation's notice) is kept whole, with the time it came, in a log
//! of the last `MAX_NOTES`; the status row counts the unread ones and
//! keeps to items of a fixed width. The Messages window shows the log in
//! td-ui's message list, modal over the window's body, oldest first and
//! following the newest: a note's header is its time, and its text
//! wraps, selects and copies as the transcript's does. `Escape` or
//! `C-S-m` closes it. It always opens, so that what it holds is never
//! out of reach: a window too small for its list says so in it. A
//! repository workspace's card (`card`) is shown in the same panel,
//! closed by `Escape` or `C-S-w`.

use std::collections::VecDeque;

use td_ui::chrome::ROW;
use td_ui::messages::{self, Controller, Message};
use td_ui::raster::{
    self, Draw, GlyphStyle, Primitive, Rect, Surface, Weight, BORDER, CHROME, INK, PAPER,
};
use td_ui::window::{Clipboard, Input, NoClipboard, PointerPhase};
use td_ui::CELL_WIDTH;

/// The most notes kept; past it the oldest go.
pub const MAX_NOTES: usize = 500;
/// The longest note kept whole; a longer one is cut, which it says.
pub const MAX_NOTE_BYTES: usize = 16 * 1024;
/// The window's title row.
pub const TITLE: &str =
    "Messages: td-agent's notes, oldest first; C-c copies, C-S-c copies one, Escape closes";
/// What the window says where its list has no room.
pub const NO_ROOM: &str = "The window is too small to show the notes: widen it, or Escape closes.";
/// What a card's entry too large for its list says instead.
const TOO_LARGE: &str =
    "This entry is too large to show at this size: close this and open it again in a larger window.";
/// And a workspace card or a process output where its list has none.
pub const CARD_NO_ROOM: &str = "The window is too small to show this: widen it, or Escape closes.";
/// What a cut note ends with.
const CUT: &str = " \u{2026} (cut)";

/// The notes, oldest first, each with when it came in seconds since the
/// epoch, and how many have come since the window last showed them.
#[derive(Debug, Default)]
pub struct Log {
    notes: VecDeque<(u64, String)>,
    unread: usize,
}

impl Log {
    /// Keeps `text`, which came at `at`, cut to `MAX_NOTE_BYTES` on a
    /// character boundary, the oldest note going past `MAX_NOTES`.
    pub fn push(&mut self, at: u64, text: String) -> &str {
        let text = cut(text);
        if self.notes.len() >= MAX_NOTES {
            self.notes.pop_front();
        }
        self.notes.push_back((at, text));
        self.unread = self.unread.saturating_add(1).min(MAX_NOTES);
        self.notes.back().map_or("", |(_, text)| text.as_str())
    }

    /// The newest note.
    pub fn last(&self) -> Option<&str> {
        self.notes.back().map(|(_, text)| text.as_str())
    }

    /// Every note's text, oldest first.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.notes.iter().map(|(_, text)| text.as_str())
    }

    /// How many came since the window last showed them.
    pub fn unread(&self) -> usize {
        self.unread
    }

    /// The window has shown them.
    pub fn read(&mut self) {
        self.unread = 0;
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }
}

fn cut(mut text: String) -> String {
    if text.len() <= MAX_NOTE_BYTES {
        return text;
    }
    let mut end = MAX_NOTE_BYTES - CUT.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(CUT);
    text
}

/// An entry as the panel shows it: a note's header is its time.
fn message(header: &str, text: &str) -> Result<Message, messages::Error> {
    Message::new(header)?.text(text)
}

/// What the window made of an input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    /// Still open; whether it needs a paint.
    Stay(bool),
    /// Closed.
    Closed,
    /// The clipboard refused a copy, for this reason.
    Refused(String),
}

/// The Messages window over the body, or a workspace's card.
#[derive(Debug)]
pub struct Panel {
    surface: Surface,
    rect: Rect,
    list: Controller,
    title: String,
    /// The conversation whose workspace card, or process output, it is;
    /// none for the Messages window.
    card: Option<crate::store::Id>,
    /// What it is, as a note names it.
    name: &'static str,
}

/// A workspace card's name.
pub const WORKSPACE_CARD: &str = "workspace card";

/// The title row and the list below it within `rect`.
fn split(surface: Surface, rect: Rect) -> (Rect, Rect) {
    let title = ((ROW * surface.scale.value()) as u32).min(rect.height);
    (
        Rect {
            height: title,
            ..rect
        },
        Rect {
            y: rect.y + i64::from(title),
            height: rect.height - title,
            ..rect
        },
    )
}

impl Panel {
    /// The window over `body` showing `log`. A note the list refuses
    /// past its bounds takes the oldest shown with it, as the
    /// transcript's do; a body too small for the list leaves it unlaid
    /// until a resize gives it room.
    pub fn open(surface: Surface, body: Rect, log: &Log) -> Result<Self, String> {
        let (_, list_rect) = split(surface, body);
        let list =
            Controller::new(surface, list_rect).map_err(|e| format!("the Messages window: {e}"))?;
        let mut panel = Self {
            surface,
            rect: body,
            list,
            title: TITLE.to_string(),
            card: None,
            name: "Messages window",
        };
        for (at, text) in &log.notes {
            panel.add(&crate::history::utc(*at), text);
        }
        panel.focus(true);
        Ok(panel)
    }

    /// Conversation `id`'s workspace card over `body`, titled `title`,
    /// showing `entries` (`card::entries`) in order.
    pub fn card(
        surface: Surface,
        body: Rect,
        id: crate::store::Id,
        title: String,
        entries: &[(String, String)],
    ) -> Result<Self, String> {
        Self::carded(surface, body, id, title, entries, WORKSPACE_CARD)
    }

    /// A card named `name`: conversation `id`'s entries over `body`.
    fn carded(
        surface: Surface,
        body: Rect,
        id: crate::store::Id,
        title: String,
        entries: &[(String, String)],
        name: &'static str,
    ) -> Result<Self, String> {
        let (_, list_rect) = split(surface, body);
        let list = Controller::new(surface, list_rect).map_err(|e| format!("the {name}: {e}"))?;
        let mut panel = Self {
            surface,
            rect: body,
            list,
            title,
            card: Some(id),
            name,
        };
        for (header, text) in entries {
            panel.add(header, text);
        }
        panel.top();
        panel.focus(true);
        Ok(panel)
    }

    /// A card shows its first entry first: the list follows the newest.
    fn top(&mut self) {
        let home = messages::Event::Key {
            key: messages::Key::Home,
            repeat: false,
        };
        let _ = self.list.event(home, &mut NoClipboard);
    }

    /// The conversation whose workspace card it is; none while it is the
    /// Messages window.
    pub fn card_of(&self) -> Option<&crate::store::Id> {
        self.card.as_ref()
    }

    /// What it is, as a note names it.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Conversation `id`'s background process output over `body`, as
    /// a card shows its entries: read-only, first entry first.
    pub fn output(
        surface: Surface,
        body: Rect,
        id: crate::store::Id,
        title: String,
        entries: &[(String, String)],
    ) -> Result<Self, String> {
        Self::carded(surface, body, id, title, entries, "process output")
    }

    /// What a request and its reply carried (the Debug button), read-only
    /// as process output is.
    pub fn debug(
        surface: Surface,
        body: Rect,
        id: crate::store::Id,
        title: String,
        entries: &[(String, String)],
    ) -> Result<Self, String> {
        Self::carded(surface, body, id, title, entries, "request view")
    }

    /// Adds an entry, the oldest shown going while the list refuses it;
    /// one it refuses alone is left out.
    /// A card's entry is never evicted, its recommendation first of all:
    /// one the list refuses is shown saying so instead.
    fn add(&mut self, header: &str, text: &str) {
        if self.card.is_some() {
            let pushed = message(header, text).and_then(|note| self.list.push(note));
            if pushed.is_err() {
                if let Ok(said) = message(header, TOO_LARGE) {
                    let _ = self.list.push(said);
                }
            }
            return;
        }
        let Ok(note) = message(header, text) else {
            return;
        };
        while let Err(messages::Error::Limit) = self.list.push(note.clone()) {
            if self.list.is_empty() {
                return;
            }
            self.list.remove_first(self.list.len().div_ceil(8));
        }
    }

    /// A note that came while it is open, `log` having kept it: the list
    /// keeps no more notes than the log does.
    pub fn push(&mut self, at: u64, text: &str, log: &Log) {
        self.add(&crate::history::utc(at), text);
        let extra = self.list.len().saturating_sub(log.len());
        if extra > 0 {
            self.list.remove_first(extra);
        }
    }

    /// Lays it out over `body` of `surface`; a list that will not lay
    /// out at its size waits, unlaid, for one that will.
    pub fn resize(&mut self, surface: Surface, body: Rect) {
        let (_, list_rect) = split(surface, body);
        let laid = self.list.has_layout();
        while let Err(messages::Error::Limit) = self.list.resize(surface, list_rect) {
            // A card waits, unlaid and saying so, for room.
            if self.list.is_empty() || self.card.is_some() {
                break;
            }
            self.list.remove_first(self.list.len().div_ceil(8));
        }
        // A card never follows its newest: laid out for the first time,
        // or following because it fitted, it goes to its top.
        if self.card.is_some() && (!laid || self.list.following()) && self.list.has_layout() {
            self.top();
        }
        self.surface = surface;
        self.rect = body;
    }

    /// Whether its list shows the keyboard focus, which it does while the
    /// window has the focus and no modal lies over it.
    pub fn focus(&mut self, focused: bool) {
        let _ = self
            .list
            .event(messages::Event::Focus(focused), &mut NoClipboard);
    }

    /// Ends a drag without a release.
    pub fn cancel(&mut self) {
        let _ = self.list.event(messages::Event::Cancel, &mut NoClipboard);
    }

    /// An input while it is open: its keys, the pointer and the wheel; a
    /// resize, a focus change and a paste are the window's. `at_ms` is
    /// the window's clock, for double clicks. A held key closes nothing:
    /// only a press of `Escape`, or the chord that opened it, does.
    pub fn input(&mut self, input: &Input<'_>, clipboard: &mut dyn Clipboard, at_ms: u64) -> Reply {
        let closing = if self.card.is_some() {
            crate::card::CHORD
        } else {
            "C-S-m"
        };
        let event = match *input {
            Input::Key { chord, repeat } if chord == "Escape" || chord == closing => {
                return if repeat {
                    Reply::Stay(false)
                } else {
                    Reply::Closed
                };
            }
            Input::Key { chord, repeat } => match messages::Key::from_chord(chord) {
                Some(key) => messages::Event::Key { key, repeat },
                None => return Reply::Stay(false),
            },
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => match phase {
                PointerPhase::Press => messages::Event::Press {
                    x,
                    y,
                    extend,
                    at_ms,
                },
                PointerPhase::Move => messages::Event::Move { x, y },
                PointerPhase::Release => messages::Event::Release { x, y, at_ms },
            },
            Input::Wheel { rows, .. } => messages::Event::Wheel { rows },
            Input::CancelPointer => messages::Event::Cancel,
            Input::Resize(_)
            | Input::Focus(_)
            | Input::Paste(_)
            | Input::Hover(_)
            | Input::Context { .. }
            | Input::Close => return Reply::Stay(false),
        };
        match self.list.event(event, clipboard) {
            messages::Outcome::Ignored | messages::Outcome::Consumed => Reply::Stay(false),
            messages::Outcome::Refused(why) => Reply::Refused(why.to_string()),
            _ => Reply::Stay(true),
        }
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(clip) = self.rect.intersection(damage) else {
            return;
        };
        let s = self.surface.scale.value();
        let (title, below) = split(self.surface, self.rect);
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: self.rect,
                color: PAPER,
            },
        });
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: title,
                color: CHROME,
            },
        });
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: Rect {
                    y: title.y + i64::from(title.height) - s as i64,
                    height: (s as u32).min(title.height),
                    ..title
                },
                color: BORDER,
            },
        });
        let line = |text: &str, rect: Rect, background: u32, sink: &mut dyn FnMut(Draw)| {
            raster::text_run(
                self.surface.scale,
                text.chars(),
                (rect.x + (CELL_WIDTH * s) as i64, rect.y + (4 * s) as i64),
                rect,
                GlyphStyle {
                    ink: INK,
                    background,
                    weight: Weight::Regular,
                },
                damage,
                sink,
            );
        };
        line(&self.title, title, CHROME, sink);
        if self.list.has_layout() {
            self.list.emit(damage, sink);
        } else if self.card.is_some() {
            line(CARD_NO_ROOM, below, PAPER, sink);
        } else {
            line(NO_ROOM, below, PAPER, sink);
        }
    }

    /// The notes it shows.
    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Whether its list has room to show them.
    pub fn shows(&self) -> bool {
        self.list.has_layout()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use td_ui::raster::Scale;

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    /// A card longer than its panel opens on its first entry, not
    /// following the newest as the Messages window does.
    #[test]
    fn a_card_opens_at_its_first_entry() {
        let long = "a line of upstream's file\n".repeat(400);
        let entries = vec![
            (
                crate::card::BEFORE.to_string(),
                crate::card::OPENING.to_string(),
            ),
            ("td at aaaaaaaaaaaa".to_string(), long),
        ];
        let id = crate::store::Id::parse(&format!("{:032x}", 1)).unwrap();
        let body = Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 640,
        };
        let card = Panel::card(surface(), body, id, "Workspace td-1".into(), &entries).unwrap();
        assert_eq!(card.len(), 2);
        assert!(card.shows());
        assert!(!card.list.following());
        // One that fits a page, then shrunk, still shows its first.
        let short = vec![(
            crate::card::BEFORE.to_string(),
            crate::card::OPENING.to_string(),
        )];
        let mut fits = Panel::card(
            surface(),
            body,
            crate::store::Id::parse(&format!("{:032x}", 2)).unwrap(),
            "Workspace td-2".into(),
            &short,
        )
        .unwrap();
        let narrow = Surface::new(240, 140, Scale::default()).unwrap();
        fits.resize(
            narrow,
            Rect {
                width: 240,
                height: 140,
                ..body
            },
        );
        assert!(fits.shows());
        assert!(!fits.list.following());
        let log = Log::default();
        let notes = Panel::open(surface(), body, &log).unwrap();
        assert!(notes.list.following());
    }

    #[test]
    fn the_log_keeps_the_newest_whole_and_counts_the_unread() {
        let mut log = Log::default();
        for n in 0..MAX_NOTES + 3 {
            log.push(n as u64, format!("note {n}"));
        }
        assert_eq!(log.len(), MAX_NOTES);
        assert_eq!(log.texts().next(), Some("note 3"));
        assert_eq!(log.last(), Some(format!("note {}", MAX_NOTES + 2).as_str()));
        assert_eq!(log.unread(), MAX_NOTES);
        log.read();
        assert_eq!(log.unread(), 0);
        // A long note is cut on a character boundary, the walk back
        // taken (a byte before three-byte characters), and says so.
        let long = format!("a{}", "\u{20ac}".repeat(MAX_NOTE_BYTES));
        let kept = log.push(9, long).to_string();
        assert!(
            kept.len() <= MAX_NOTE_BYTES && kept.ends_with(CUT),
            "{}",
            kept.len()
        );
        assert!(kept.len() < MAX_NOTE_BYTES, "the cut walked back");
        assert_eq!(log.unread(), 1);
    }

    #[test]
    fn the_window_shows_the_log_follows_it_and_closes_on_a_press() {
        let mut log = Log::default();
        log.push(0, "first".into());
        log.push(60, "a refusal said whole, however long it runs".into());
        let surface = surface();
        let mut panel = Panel::open(surface, surface.bounds(), &log).unwrap();
        assert_eq!(panel.len(), 2);
        assert!(panel.shows());
        let text = log.push(120, "third".into()).to_string();
        panel.push(120, &text, &log);
        assert_eq!(panel.len(), 3);
        let mut clipboard = NoClipboard;
        let key = |chord, repeat| Input::Key { chord, repeat };
        assert!(matches!(
            panel.input(&key("PageUp", false), &mut clipboard, 0),
            Reply::Stay(_)
        ));
        assert_eq!(
            panel.input(&key("x", false), &mut clipboard, 0),
            Reply::Stay(false)
        );
        // A held chord closes nothing; a press does.
        for chord in ["Escape", "C-S-m"] {
            assert_eq!(
                panel.input(&key(chord, true), &mut clipboard, 0),
                Reply::Stay(false)
            );
            assert_eq!(
                panel.input(&key(chord, false), &mut clipboard, 0),
                Reply::Closed
            );
        }
        // A copy the clipboard refuses says why.
        let _ = panel.input(&key("C-a", false), &mut clipboard, 0);
        assert!(matches!(
            panel.input(&key("C-c", false), &mut clipboard, 0),
            Reply::Refused(_)
        ));
    }

    #[test]
    fn a_window_too_small_opens_says_so_and_shows_them_once_it_has_room() {
        let mut log = Log::default();
        log.push(0, "kept".into());
        let surface = surface();
        // A title row and a few pixels: no room for the list.
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: (ROW + 6) as u32,
        };
        let mut panel = Panel::open(surface, tiny, &log).unwrap();
        assert!(!panel.shows());
        let mut drawn = String::new();
        panel.emit(surface.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                drawn.push(scalar);
            }
        });
        assert!(drawn.starts_with("Messages:"), "{drawn}");
        panel.resize(surface, surface.bounds());
        assert!(panel.shows());
        assert_eq!(panel.len(), 1);
    }

    #[test]
    fn a_full_window_drops_its_oldest_as_the_log_does() {
        let mut log = Log::default();
        for n in 0..MAX_NOTES {
            log.push(n as u64, format!("note {n}"));
        }
        let surface = surface();
        let mut panel = Panel::open(surface, surface.bounds(), &log).unwrap();
        let text = log.push(999, "newest".into()).to_string();
        panel.push(999, &text, &log);
        assert_eq!(panel.len(), MAX_NOTES);
    }
}
