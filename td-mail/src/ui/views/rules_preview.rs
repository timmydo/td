use crate::backend::BackendResponse;
use crate::backend::RulesDryRunResult;
use crate::ui::input::Key;
use crate::ui::views::{text_scroll, wrap_text, Body, Scene, Scroll, View, ViewAction};

const LABELS: &[&str] = &["Close"];
const KEYS: &[Key] = &[Key::Char('q')];

pub struct RulesPreviewView {
    lines: Vec<String>,
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

        RulesPreviewView { lines }
    }
}

impl View for RulesPreviewView {
    fn scene(&self) -> Scene<'_> {
        Scene {
            title: "Rules dry-run".to_string(),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body: Body::Text {
                key: self.lines.first().cloned().unwrap_or_default(),
                text: Box::new(|columns| wrap_text(&self.lines.join("\n"), columns)),
            },
            status: "Rules dry-run | q/Esc/Enter:close j/k:scroll".to_string(),
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
