//! Exact named editor actions and bounded prefix completion; never evaluation.

use crate::menu::Item;
use crate::model::{Editor, RevisionPoint, Selection, TabId};
use crate::{Error, Result};

pub(crate) const BYTES: usize = 64;
const NAMES: &[(&str, Item)] = &[
    ("auto-fill-mode", Item::AutoFill),
    ("display-line-numbers-mode", Item::LineNumbers),
    ("fill-paragraph", Item::Fill),
    ("goto-line", Item::GoToLine),
    ("ispell-buffer", Item::Spell),
    ("next-misspelling", Item::NextMisspelling),
    ("previous-misspelling", Item::PreviousMisspelling),
    ("set-fill-column", Item::FillColumn),
];

pub(crate) struct Prompt {
    point: RevisionPoint,
    selection: Selection,
    text: String,
    refused: bool,
}

impl Prompt {
    pub fn new(editor: &Editor, tab: TabId, revision: u64) -> Result<Self> {
        let point = editor.revision_point(tab, revision)?;
        if editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            point,
            selection: editor.document(tab)?.selection(),
            text: String::new(),
            refused: false,
        })
    }

    pub fn target(&self, editor: &Editor) -> Result<(TabId, u64)> {
        editor.check_revision(&self.point)?;
        if editor.active() != Some(self.point.tab)
            || editor.document(self.point.tab)?.selection() != self.selection
        {
            return Err(Error::StaleRevision);
        }
        Ok((self.point.tab, self.point.revision))
    }

    pub fn entry(&self) -> (&str, bool) {
        (&self.text, self.refused)
    }

    pub fn action(&self) -> Result<Item> {
        NAMES
            .iter()
            .find(|(name, _)| *name == self.text)
            .map(|(_, item)| *item)
            .ok_or(Error::InvalidArgument)
    }

    pub fn refused(&mut self) {
        self.refused = true;
    }

    fn complete(&mut self) {
        let mut matches = NAMES
            .iter()
            .filter(|(name, _)| name.starts_with(&self.text));
        let Some((first, _)) = matches.next() else {
            self.refused = true;
            return;
        };
        let mut end = first.len();
        for (name, _) in matches {
            end = end.min(
                first
                    .bytes()
                    .zip(name.bytes())
                    .take_while(|(a, b)| a == b)
                    .count(),
            );
            if end == 0 {
                break;
            }
        }
        if let Some(prefix) = first.get(..end) {
            self.text.clear();
            self.text.push_str(prefix);
        }
        self.refused = false;
    }

    pub fn type_chord(&mut self, chord: &str) {
        match chord {
            "Tab" => self.complete(),
            "Backspace" => {
                self.text.pop();
                self.refused = false;
            }
            "C-u" => {
                self.text.clear();
                self.refused = false;
            }
            _ if matches!(chord.as_bytes(), [b] if b.is_ascii_lowercase() || *b == b'-')
                && self.text.len() < BYTES =>
            {
                self.text.push_str(chord);
                self.refused = false;
            }
            _ => {}
        }
    }

    pub fn notice(&self) -> String {
        let count = NAMES
            .iter()
            .filter(|(name, _)| name.starts_with(&self.text))
            .count();
        let matches = if count == 1 { "match" } else { "matches" };
        let mut notice = format!(
            "{}Command ({count} {matches}): {}|",
            if self.refused {
                "Unknown/incomplete command; use Tab.\n"
            } else {
                ""
            },
            self.text
        );
        for (name, _) in NAMES
            .iter()
            .filter(|(name, _)| name.starts_with(&self.text))
            .take(3)
        {
            notice.push('\n');
            notice.push_str(name);
        }
        notice.push_str(
            "\nReturn: run exact name; Tab: complete; Escape/Ctrl+G: cancel; Ctrl+U: clear",
        );
        notice
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::Command;
    use crate::ui::{Controller, Event};

    fn controller() -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"unchanged")).unwrap();
        ui
    }

    #[test]
    fn only_exact_registered_names_run_and_tab_completes_without_dispatch() {
        let ui = controller();
        let expected = [
            ("auto-fill-mode", Item::AutoFill),
            ("display-line-numbers-mode", Item::LineNumbers),
            ("fill-paragraph", Item::Fill),
            ("goto-line", Item::GoToLine),
            ("ispell-buffer", Item::Spell),
            ("next-misspelling", Item::NextMisspelling),
            ("previous-misspelling", Item::PreviousMisspelling),
            ("set-fill-column", Item::FillColumn),
        ];
        assert_eq!(NAMES, expected);
        assert!(NAMES
            .windows(2)
            .all(|pair| matches!(pair, [(a, _), (b, _)] if a < b)));
        let menu = crate::menu::Menu {
            directory: false,
            directory_entry: false,
            directory_sort: crate::directory::Sort::Name,
            directory_reverse: false,
            group: crate::menu::Group::Help,
            selected: 0,
            target: crate::dialog::Target {
                tab: 1,
                revision: 0,
            },
            profile: crate::keys::Profile::Windows,
            file_window: false,
            undo: false,
            redo: false,
            wrap: false,
            line_numbers: true,
            auto_fill: false,
            copy: false,
            copy_path: false,
            paste: false,
        };
        assert!(NAMES.iter().all(|(_, item)| menu.enabled(*item)));
        for (name, action) in expected {
            let mut prompt = Prompt::new(ui.editor(), 1, 0).unwrap();
            assert!(prompt.action().is_err());
            prompt.type_chord(name.get(..1).unwrap());
            assert!(prompt.action().is_err());
            prompt.type_chord("Tab");
            assert_eq!(prompt.text, name);
            assert!(prompt.notice().contains("1 match):"));
            assert_eq!(prompt.action(), Ok(action));
            assert_eq!(prompt.target(ui.editor()), Ok((1, 0)));
        }
        assert_eq!(ui.editor().document(1).unwrap().text(), "unchanged");
        assert_eq!(ui.editor().document(1).unwrap().revision(), 0);
        let mut prompt = Prompt::new(ui.editor(), 1, 0).unwrap();
        prompt.type_chord("Tab");
        assert!(prompt.text.is_empty());
        assert!(prompt.notice().contains("8 matches"));
        assert!(prompt.action().is_err());
        prompt.type_chord("z");
        prompt.type_chord("Tab");
        assert_eq!(prompt.text, "z");
        assert!(prompt.notice().contains("Unknown/incomplete"));
        assert!(prompt.action().is_err());
    }

    #[test]
    fn entry_is_bounded_and_target_bound_without_interpreting_any_text() {
        let mut ui = controller();
        let mut prompt = Prompt::new(ui.editor(), 1, 0).unwrap();
        for _ in 0..1000 {
            prompt.type_chord("x");
        }
        assert_eq!(prompt.text.len(), BYTES);
        for chord in ["é", "Space", "C-x", ";", "1", "A"] {
            prompt.type_chord(chord);
        }
        assert_eq!(prompt.text.len(), BYTES);
        assert!(prompt.action().is_err());
        prompt.type_chord("Backspace");
        assert_eq!(prompt.text.len(), BYTES - 1);
        prompt.type_chord("C-u");
        assert!(prompt.text.is_empty());
        for chord in ["a", "-"] {
            prompt.type_chord(chord);
        }
        assert_eq!(prompt.text, "a-");
        assert!(prompt.target(controller().editor()).is_err());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 1,
                caret: 2,
            }),
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
        let prompt = Prompt::new(ui.editor(), 1, 0).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("x".into()),
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
        let prompt = Prompt::new(ui.editor(), 1, 2).unwrap();
        ui.dispatch(Event::New).unwrap();
        assert!(prompt.target(ui.editor()).is_err());
    }
}
