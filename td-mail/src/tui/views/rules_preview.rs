use crate::backend::BackendResponse;
use crate::backend::RulesDryRunResult;
use crate::tui::input::Key;
use crate::tui::screen::Terminal;
use crate::tui::views::{View, ViewAction};
use std::io;

pub struct RulesPreviewView {
    lines: Vec<String>,
    scroll: usize,
}

impl RulesPreviewView {
    pub fn new(mailbox_name: String, preview: RulesDryRunResult) -> Self {
        let mut lines = Vec::new();
        lines.push(format!(
            "Rules dry-run for '{}' (scanned: {}, matches: {}, actions: {})",
            mailbox_name, preview.scanned, preview.matched_rules, preview.actions
        ));
        lines.push(String::new());

        if preview.entries.is_empty() {
            lines.push("No rule actions would be applied.".to_string());
        } else {
            for entry in preview.entries {
                lines.push(format!(
                    "{} | {} | {}",
                    entry.received_at, entry.from, entry.subject
                ));
                lines.push(format!(
                    "  rule={} actions={}",
                    entry.rule_name,
                    entry.actions.join(", ")
                ));
                lines.push(String::new());
            }
        }

        RulesPreviewView { lines, scroll: 0 }
    }
}

impl View for RulesPreviewView {
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
            if i == 0 && self.scroll == 0 {
                term.set_header()?;
                term.write_truncated(line, term.cols)?;
                term.reset_attr()?;
            } else {
                term.write_truncated(line, term.cols)?;
            }
        }

        term.move_to(term.rows, 1)?;
        term.set_status()?;
        let status = format!(
            " Rules dry-run | line {}/{} | q/Esc/Enter:close n/p:scroll",
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
            Key::Char('q') | Key::Escape | Key::Enter => ViewAction::Pop,
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
