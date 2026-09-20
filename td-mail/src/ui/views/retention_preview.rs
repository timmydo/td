use crate::backend::BackendResponse;
use crate::backend::RetentionCandidate;
use crate::ui::input::Key;
use crate::ui::views::{text_scroll, wrap_text, Body, Scene, Scroll, View, ViewAction};

const LABELS: &[&str] = &["Close"];
const KEYS: &[Key] = &[Key::Char('q')];

pub struct RetentionPreviewView {
    lines: Vec<String>,
}

impl RetentionPreviewView {
    pub fn new(candidates: Vec<RetentionCandidate>) -> Self {
        let mut lines = Vec::new();
        lines.push(format!(
            "Retention expiry preview ({} messages)",
            candidates.len()
        ));
        lines.push(String::new());

        if candidates.is_empty() {
            lines.push("No messages would be expired by current policies.".to_string());
        } else {
            for c in candidates {
                lines.push(format!(
                    "[{}] {} | {} | {} | {}",
                    c.policy, c.received_at, c.mailbox, c.from, c.subject
                ));
            }
        }

        RetentionPreviewView { lines }
    }
}

impl View for RetentionPreviewView {
    fn scene(&self) -> Scene<'_> {
        Scene {
            title: "Retention expiry preview".to_string(),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body: Body::Text {
                key: self.lines.first().cloned().unwrap_or_default(),
                text: Box::new(|columns| wrap_text(&self.lines.join("\n"), columns)),
            },
            status: "Preview | q/Esc/Enter:close j/k:scroll".to_string(),
        }
    }

    fn handle_key(&mut self, key: Key, _page: usize) -> ViewAction {
        if let Some(scroll) = text_scroll(key) {
            return ViewAction::Scroll(scroll);
        }
        match key {
            Key::Char('q') | Key::Escape | Key::Enter => ViewAction::Pop,
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
