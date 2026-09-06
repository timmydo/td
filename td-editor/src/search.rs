//! Literal search history and explicit end-before-wrap admission.

use crate::model::{Command, Editor, RevisionPoint, Selection, TabId};
use crate::ui::{Controller, Event};
use crate::{Error, Result};

pub(crate) const QUERY_BYTES: usize = 4096;

struct Intent {
    point: RevisionPoint,
    selection: Selection,
    backward: bool,
}

impl Intent {
    fn capture(editor: &Editor, tab: TabId, revision: u64, backward: bool) -> Result<Self> {
        let point = editor.revision_point(tab, revision)?;
        if editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            point,
            selection: editor.document(tab)?.selection(),
            backward,
        })
    }

    fn matches(&self, editor: &Editor, backward: bool) -> bool {
        self.backward == backward && self.matches_target(editor)
    }

    fn matches_target(&self, editor: &Editor) -> bool {
        editor.check_revision(&self.point).is_ok()
            && editor.active() == Some(self.point.tab)
            && editor
                .document(self.point.tab)
                .is_ok_and(|doc| doc.selection() == self.selection)
    }
}

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
        Ok((self.intent.point.tab, self.intent.point.revision))
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

#[derive(Default)]
pub(crate) struct History {
    query: String,
    boundary: Option<Intent>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Found {
    Match,
    Wrapped,
    End,
    Missing,
}

impl History {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cancel_wrap(&mut self) {
        self.boundary = None;
    }

    pub fn observe(&mut self, editor: &Editor) {
        if self
            .boundary
            .as_ref()
            .is_some_and(|intent| !intent.matches_target(editor))
        {
            self.boundary = None;
        }
    }

    pub fn find(
        &mut self,
        ui: &mut Controller,
        tab: TabId,
        revision: u64,
        query: &str,
        backward: bool,
    ) -> Result<Found> {
        if query.is_empty() {
            return Err(Error::InvalidArgument);
        }
        if query.len() > QUERY_BYTES {
            return Err(Error::Limit);
        }
        let intent = Intent::capture(ui.editor(), tab, revision, backward)?;
        let wrap = query == self.query
            && self
                .boundary
                .as_ref()
                .is_some_and(|boundary| boundary.matches(ui.editor(), backward));
        let result = ui.dispatch(Event::Edit {
            tab,
            revision,
            command: Command::Find {
                needle: query.to_owned(),
                backward,
                wrap,
            },
        });
        match result {
            Ok(_) => {
                self.query = query.to_owned();
                self.boundary = None;
                Ok(if wrap { Found::Wrapped } else { Found::Match })
            }
            Err(Error::Unavailable) => {
                self.query = query.to_owned();
                self.boundary = if wrap { None } else { Some(intent) };
                Ok(if wrap { Found::Missing } else { Found::End })
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn controller(text: &str) -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        ui
    }

    #[test]
    fn literal_utf8_search_reaches_end_then_wraps_without_an_edit() {
        let mut ui = controller("é x é");
        let mut history = History::default();
        assert_eq!(
            history.find(&mut ui, 1, 0, "é", false).unwrap(),
            Found::Match
        );
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 0..2);
        assert_eq!(
            history.find(&mut ui, 1, 0, "é", false).unwrap(),
            Found::Match
        );
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 5..7);
        let generation = ui.generation();
        assert_eq!(history.find(&mut ui, 1, 0, "é", false).unwrap(), Found::End);
        assert_eq!(ui.generation(), generation);
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 5..7);
        assert_eq!(
            history.find(&mut ui, 1, 0, "é", false).unwrap(),
            Found::Wrapped
        );
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 0..2);
        assert_eq!(history.find(&mut ui, 1, 0, "é", true).unwrap(), Found::End);
        assert_eq!(
            history.find(&mut ui, 1, 0, "é", true).unwrap(),
            Found::Wrapped
        );
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 5..7);
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (0, 0));
        assert!(!ui.editor().document(1).unwrap().dirty());
    }

    #[test]
    fn missing_changed_query_direction_cancel_and_foreign_editor_reset_wrap_authority() {
        let mut ui = controller("x");
        let mut history = History::default();
        assert_eq!(history.find(&mut ui, 1, 0, "X", false).unwrap(), Found::End);
        assert_eq!(
            history.find(&mut ui, 1, 0, "X", false).unwrap(),
            Found::Missing
        );
        assert_eq!(history.find(&mut ui, 1, 0, "X", false).unwrap(), Found::End);
        assert_eq!(
            history.find(&mut ui, 1, 0, "none", false).unwrap(),
            Found::End
        );
        assert_eq!(
            history.find(&mut ui, 1, 0, "none", true).unwrap(),
            Found::End
        );
        history.cancel_wrap();
        assert_eq!(
            history.find(&mut ui, 1, 0, "none", true).unwrap(),
            Found::End
        );
        let mut replacement = controller("x");
        assert_eq!(
            history.find(&mut replacement, 1, 0, "none", true).unwrap(),
            Found::End
        );
        assert_eq!(
            ui.editor().document(1).unwrap().selection(),
            Selection::default()
        );
    }

    #[test]
    fn observed_selection_or_tab_transitions_cannot_revive_wrap() {
        let mut ui = controller("x");
        let mut history = History::default();
        history.find(&mut ui, 1, 0, "none", false).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 1,
                caret: 1,
            }),
        })
        .unwrap();
        history.observe(ui.editor());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection::default()),
        })
        .unwrap();
        assert_eq!(
            history.find(&mut ui, 1, 0, "none", false).unwrap(),
            Found::End
        );
        ui.dispatch(Event::New).unwrap();
        history.observe(ui.editor());
        ui.dispatch(Event::SelectTab(1)).unwrap();
        assert_eq!(
            history.find(&mut ui, 1, 0, "none", false).unwrap(),
            Found::End
        );
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("a".into()),
        })
        .unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert_eq!(
            history.find(&mut ui, 1, 2, "none", false).unwrap(),
            Found::End
        );
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
