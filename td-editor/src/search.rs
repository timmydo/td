//! The find prompt: the query typed in the minibuffer and the intent it
//! was opened against. The history and end-before-wrap admission are
//! td-ui's (`td_ui::editor_search`).

use crate::{Error, Result};
use td_ui::editor_model::{Editor, TabId};
use td_ui::editor_search::{Intent, QUERY_BYTES};

pub(crate) struct Prompt {
    intent: Intent,
    pub text: String,
    pub backward: bool,
}

impl Prompt {
    pub fn new(
        editor: &Editor,
        tab: TabId,
        revision: u64,
        text: String,
        backward: bool,
    ) -> Result<Self> {
        if text.len() > QUERY_BYTES {
            return Err(Error::Limit);
        }
        Ok(Self {
            intent: Intent::capture(editor, tab, revision, backward)?,
            text,
            backward,
        })
    }

    pub fn target(&self, editor: &Editor) -> Result<(TabId, u64)> {
        // Query entry does not move the document selection. External changes
        // cannot silently choose a different search start when submitted.
        if !self.intent.matches_target(editor) {
            return Err(Error::StaleRevision);
        }
        Ok((self.intent.point().tab(), self.intent.point().revision()))
    }

    pub fn notice(&self) -> String {
        let start = self
            .text
            .char_indices()
            .rev()
            .nth(159)
            .map_or(0, |(at, _)| at);
        format!(
            "Find {}: literal, case-sensitive\nReturn: search; Escape/Ctrl+G: cancel; Ctrl+U: clear\n{}{}|",
            if self.backward { "backward" } else { "forward" },
            if start != 0 { "..." } else { "" },
            self.text.get(start..).unwrap_or_default()
        )
    }

    pub fn type_chord(&mut self, chord: &str) {
        match chord {
            "Backspace" => {
                self.text.pop();
            }
            "C-u" => self.text.clear(),
            _ => {
                let text = if chord == "Space" { " " } else { chord };
                let mut chars = text.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if !c.is_control() && self.text.len() + c.len_utf8() <= QUERY_BYTES {
                        self.text.push(c);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_ui::editor::{Controller, Event};
    use td_ui::editor_model::{Command, Selection};
    use td_ui::editor_search::History;

    fn controller(text: &str) -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        ui
    }

    #[test]
    fn prompt_is_scalar_bounded_and_submits_only_its_original_intent() {
        let mut ui = controller("λ");
        let mut prompt = Prompt::new(ui.editor(), 1, 0, String::new(), false).unwrap();
        for chord in ["λ", "Space", "C-x", "Return", "F10"] {
            prompt.type_chord(chord);
        }
        assert_eq!(prompt.text, "λ ");
        prompt.type_chord("Backspace");
        assert_eq!(prompt.text, "λ");
        prompt.type_chord("Backspace");
        assert!(prompt.text.is_empty());
        prompt.text = "x".repeat(QUERY_BYTES - 1);
        prompt.type_chord("λ");
        assert_eq!(prompt.text.len(), QUERY_BYTES - 1);
        prompt.type_chord("y");
        prompt.type_chord("z");
        assert_eq!(prompt.text.len(), QUERY_BYTES);
        assert!(prompt.notice().contains("..."));
        assert!(prompt.target(ui.editor()).is_ok());
        prompt.backward = true;
        assert!(prompt.target(ui.editor()).is_ok());
        let foreign = controller("λ");
        assert!(prompt.target(foreign.editor()).is_err());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 2,
                caret: 2,
            }),
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
        assert!(Prompt::new(ui.editor(), 1, 0, "x".repeat(QUERY_BYTES + 1), false).is_err());
        assert!(History::default().find(&mut ui, 1, 0, "", false).is_err());
        assert_eq!(
            History::default().find(&mut ui, 1, 0, &"x".repeat(QUERY_BYTES + 1), false),
            Err(Error::Limit)
        );
    }
}
