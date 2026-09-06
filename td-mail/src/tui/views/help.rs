use crate::backend::BackendResponse;
use crate::tui::input::Key;
use crate::tui::screen::Terminal;
use crate::tui::views::{View, ViewAction};
use std::io;

pub struct HelpView {
    lines: Vec<String>,
    scroll: usize,
}

impl HelpView {
    pub fn new() -> Self {
        let lines = vec![
            "tmc - Timmy's Mail Console".to_string(),
            "=========================".to_string(),
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
        ];

        HelpView { lines, scroll: 0 }
    }
}

impl View for HelpView {
    fn render(&self, term: &mut Terminal) -> io::Result<()> {
        term.clear()?;

        let visible_rows = (term.rows as usize).saturating_sub(1);

        for (i, line) in self
            .lines
            .iter()
            .skip(self.scroll)
            .enumerate()
            .take(visible_rows)
        {
            let row = 1 + i as u16;
            term.move_to(row, 1)?;

            // Bold section headers (lines that are followed by dashes or equal signs)
            let is_header = !line.is_empty()
                && !line.starts_with(' ')
                && !line.starts_with('-')
                && !line.starts_with('=');

            if is_header {
                term.set_header()?;
                term.write_truncated(line, term.cols)?;
                term.reset_attr()?;
            } else {
                term.write_truncated(line, term.cols)?;
            }
        }

        // Status bar
        term.move_to(term.rows, 1)?;
        term.set_status()?;
        let status = format!(
            " Help | line {}/{} | q:close n/j:down p/k:up",
            self.scroll + 1,
            self.lines.len()
        );
        term.write_truncated(&status, term.cols)?;
        let remaining = (term.cols as usize).saturating_sub(status.len());
        for _ in 0..remaining {
            term.write_str(" ")?;
        }
        term.reset_attr()?;

        term.flush()
    }

    fn handle_key(&mut self, key: Key, term_rows: u16) -> ViewAction {
        let page = (term_rows as usize).saturating_sub(1);
        match key {
            Key::Char('q') | Key::Char('?') | Key::Escape => ViewAction::Pop,
            Key::Char('n') | Key::Char('j') | Key::Down => {
                if self.scroll + 1 < self.lines.len() {
                    self.scroll += 1;
                }
                ViewAction::Continue
            }
            Key::Char('p') | Key::Char('k') | Key::Up => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                }
                ViewAction::Continue
            }
            Key::PageDown | Key::Char(' ') => {
                self.scroll = (self.scroll + page).min(self.lines.len().saturating_sub(1));
                ViewAction::Continue
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                ViewAction::Continue
            }
            Key::Home => {
                self.scroll = 0;
                ViewAction::Continue
            }
            Key::End => {
                self.scroll = self.lines.len().saturating_sub(1);
                ViewAction::Continue
            }
            Key::ScrollUp => {
                if self.scroll > 0 {
                    self.scroll -= 1;
                }
                ViewAction::Continue
            }
            Key::ScrollDown => {
                if self.scroll + 1 < self.lines.len() {
                    self.scroll += 1;
                }
                ViewAction::Continue
            }
            _ => ViewAction::Continue,
        }
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }
}
