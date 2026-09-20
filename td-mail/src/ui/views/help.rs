//! The key reference, as text in the pane.

use crate::backend::BackendResponse;
use crate::ui::input::Key;
use crate::ui::views::{text_scroll, wrap_text, Body, Scene, Scroll, View, ViewAction};

const LABELS: &[&str] = &["Back"];
const KEYS: &[Key] = &[Key::Char('q')];

pub struct HelpView {
    lines: Vec<String>,
}

impl HelpView {
    pub fn new() -> Self {
        let lines = vec![
            "td-mail - Timmy's Mail Console".to_string(),
            "==============================".to_string(),
            String::new(),
            "Global".to_string(),
            "------".to_string(),
            "  ?           Show this help".to_string(),
            "  c           Compose new email".to_string(),
            String::new(),
            "Mailbox List".to_string(),
            "------------".to_string(),
            "  q           Quit".to_string(),
            "  n/j/Down    Next mailbox".to_string(),
            "  p/k/Up      Previous mailbox".to_string(),
            "  Enter       Open mailbox".to_string(),
            "  g           Refresh".to_string(),
            "  a           Next account; with one, reopen it (reconnect)".to_string(),
            "  +           Create folder".to_string(),
            "  d           Delete selected folder".to_string(),
            "  u           Mark all mail in selected folder read".to_string(),
            "  x           Preview retention expiry list".to_string(),
            "  X           Expire retained mail now".to_string(),
            "  PgDn        Page down".to_string(),
            "  PgUp        Page up".to_string(),
            "  Home        Jump to top".to_string(),
            "  End         Jump to bottom".to_string(),
            String::new(),
            "Email List".to_string(),
            "----------".to_string(),
            "  q           Back to mailbox list".to_string(),
            "  n/j/Down    Next email".to_string(),
            "  p/k/Up      Previous email".to_string(),
            "  Enter       Open email / thread reading view".to_string(),
            "  t           Open thread list view (same folder)".to_string(),
            "  T           Open thread list view (all folders)".to_string(),
            "  g           Refresh".to_string(),
            "  r           Reply to selected email".to_string(),
            "  R           Reply all to selected email".to_string(),
            "  e           Dry-run rules on loaded messages".to_string(),
            "  E           Run rules on loaded messages".to_string(),
            "  a           Archive selected email/thread".to_string(),
            "  d           Move selected email/thread to deleted folder".to_string(),
            "  D           Expire selected email/thread now (deleted folder only)".to_string(),
            "  J           Mark spam: train classifier and move to Junk".to_string(),
            "  H           Mark not-spam (ham): train classifier and move to Inbox".to_string(),
            "  S           Score selected message and tag it (S=spam, ?=unsure)".to_string(),
            "  f           Toggle flagged".to_string(),
            "  u           Toggle read/unread".to_string(),
            "  m           Move to folder".to_string(),
            "  s           Search in mailbox".to_string(),
            "  l           Load more messages".to_string(),
            "  Escape      Clear search".to_string(),
            "  PgDn        Page down".to_string(),
            "  PgUp        Page up".to_string(),
            "  Home        Jump to top".to_string(),
            "  End         Jump to bottom".to_string(),
            String::new(),
            "Thread View".to_string(),
            "-----------".to_string(),
            "  q           Back to email list".to_string(),
            "  n/j/Down    Next email".to_string(),
            "  p/k/Up      Previous email".to_string(),
            "  Enter       Open email".to_string(),
            "  g           Refresh".to_string(),
            "  a           Archive selected email".to_string(),
            "  d           Move selected email to deleted folder".to_string(),
            "  D           Expire selected email now (deleted folder only)".to_string(),
            "  f           Toggle flagged".to_string(),
            "  u           Toggle read/unread".to_string(),
            "  PgDn        Page down".to_string(),
            "  PgUp        Page up".to_string(),
            "  Home        Jump to top".to_string(),
            "  End         Jump to bottom".to_string(),
            String::new(),
            "Email View".to_string(),
            "----------".to_string(),
            "  q           Back to email list".to_string(),
            "  n           Open next unread email".to_string(),
            "  p           Open previous unread email".to_string(),
            "  j/Down      Scroll down".to_string(),
            "  k/Up        Scroll up".to_string(),
            "  Space/PgDn  Page down".to_string(),
            "  PgUp        Page up".to_string(),
            "  Home        Jump to top".to_string(),
            "  End         Jump to bottom".to_string(),
            "  r           Reply".to_string(),
            "  R           Reply all".to_string(),
            "  F           Forward as attachment (preserves HTML)".to_string(),
            "  f           Forward as inline quoted text".to_string(),
            "  A           Download/open attachment".to_string(),
            "  h           Toggle HTML vs plain text body".to_string(),
            "  v           Toggle raw headers (DKIM, Received, etc)".to_string(),
            "  *           Toggle flagged".to_string(),
            "  u           Toggle read/unread".to_string(),
            "  J           Mark spam: train classifier and move to Junk".to_string(),
            "  H           Mark not-spam (ham): train classifier and move to Inbox".to_string(),
            "  S           Show this message's spam score and verdict".to_string(),
            "  D           Expire now (deleted folder only)".to_string(),
            String::new(),
            "Compose".to_string(),
            "-------".to_string(),
            "  Ctrl-S      Save the draft over its retained file".to_string(),
            "  Ctrl-W      Close (asks when unsaved: y saves, n keeps the file as saved)"
                .to_string(),
            "  Ctrl-X/C/V  Cut, copy, paste within td-mail (a message's selection too)".to_string(),
            "  Ctrl-Z/Y    Undo, redo".to_string(),
            "  Ctrl-A      Select all".to_string(),
            String::new(),
        ];

        HelpView { lines }
    }
}

impl View for HelpView {
    fn scene(&self) -> Scene<'_> {
        Scene {
            title: "Help".to_string(),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body: Body::Text {
                key: "help".to_string(),
                text: Box::new(|columns| wrap_text(&self.lines.join("\n"), columns)),
            },
            status: "Help | q:close j/k:scroll".to_string(),
        }
    }

    fn handle_key(&mut self, key: Key, _page: usize) -> ViewAction {
        if let Some(scroll) = text_scroll(key) {
            return ViewAction::Scroll(scroll);
        }
        match key {
            Key::Char('q') | Key::Char('?') | Key::Escape => ViewAction::Pop,
            // The terminal's n and p, a line each; no unread to go to here.
            Key::Char('n') => ViewAction::Scroll(Scroll::Lines(1)),
            Key::Char('p') => ViewAction::Scroll(Scroll::Lines(-1)),
            _ => ViewAction::Continue,
        }
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }
}
