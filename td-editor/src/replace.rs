//! Bounded literal replacement entry; edits use the ordinary controller.

use crate::model::{Command, Editor, RevisionPoint, Selection, TabId};
use crate::search::{Found, History, QUERY_BYTES};
use crate::ui::{Controller, Event};
use crate::{Error, Result};

#[derive(Clone, Copy)]
pub(crate) enum Action {
    Find,
    One,
    All,
}

pub(crate) struct Prompt {
    point: RevisionPoint,
    selection: Selection,
    query: String,
    replacement: String,
    with: bool,
    status: String,
}

impl Prompt {
    pub fn new(editor: &Editor, tab: TabId, revision: u64, history: &History) -> Result<Self> {
        let point = editor.revision_point(tab, revision)?;
        if editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        let doc = editor.document(tab)?;
        let selected = doc.text().get(doc.selection().range()).unwrap_or_default();
        let query = if !selected.is_empty()
            && selected.len() <= QUERY_BYTES
            && !selected.chars().any(char::is_control)
        {
            selected
        } else {
            history.query()
        };
        Ok(Self {
            point,
            selection: doc.selection(),
            query: query.to_owned(),
            replacement: String::new(),
            with: false,
            status: "Literal, case-sensitive".into(),
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

    pub fn apply(
        &mut self,
        ui: &mut Controller,
        history: &mut History,
        action: Action,
    ) -> Result<()> {
        let (tab, revision) = self.target(ui.editor())?;
        if self.query.is_empty() {
            self.status = "Enter a nonempty Find string.".into();
            return Ok(());
        }
        match action {
            Action::Find => {
                self.status = match history.find(ui, tab, revision, &self.query, false)? {
                    Found::Match => "Match selected.",
                    Found::Wrapped => "Wrapped; match selected.",
                    Found::End => "End; Return again to wrap.",
                    Found::Missing => "No matches in this document.",
                }
                .into();
            }
            Action::One | Action::All => {
                let doc = ui.editor().document(tab)?;
                let (count, command) = if matches!(action, Action::All) {
                    (
                        doc.text().matches(&self.query).count(),
                        Command::ReplaceAll {
                            needle: self.query.clone(),
                            replacement: self.replacement.clone(),
                        },
                    )
                } else {
                    if doc.text().get(doc.selection().range()) != Some(self.query.as_str()) {
                        self.status = "Select a match with Return first.".into();
                        return Ok(());
                    }
                    (1, Command::Insert(self.replacement.clone()))
                };
                if count == 0 {
                    self.status = "No matches in this document.".into();
                    history.cancel_wrap();
                    return Ok(());
                }
                match ui.dispatch(Event::Edit {
                    tab,
                    revision,
                    command,
                }) {
                    Ok(_) => {
                        history.cancel_wrap();
                        let noun = if count == 1 { "match" } else { "matches" };
                        self.status = if self.query == self.replacement {
                            format!("{count} {noun}; text unchanged.")
                        } else {
                            format!("Replaced {count} {noun}.")
                        };
                    }
                    Err(detail) => {
                        self.status = format!("Replace refused: {detail}");
                        return Ok(());
                    }
                }
            }
        }
        let doc = ui.editor().document(tab)?;
        self.point = ui.editor().revision_point(tab, doc.revision())?;
        self.selection = doc.selection();
        Ok(())
    }

    pub fn type_chord(&mut self, chord: &str, history: &mut History) {
        if matches!(chord, "Tab" | "S-Tab") {
            self.with = !self.with;
            return;
        }
        let text = if self.with {
            &mut self.replacement
        } else {
            &mut self.query
        };
        let before = text.len();
        match chord {
            "Backspace" => {
                text.pop();
            }
            "C-u" => text.clear(),
            _ => {
                let scalar = if chord == "Space" { " " } else { chord };
                let mut chars = scalar.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    if !c.is_control() && text.len() + c.len_utf8() <= QUERY_BYTES {
                        text.push(c);
                    }
                }
            }
        }
        if text.len() != before {
            self.status = "Literal, case-sensitive".into();
            if !self.with {
                history.cancel_wrap();
            }
        }
    }

    pub fn notice(&self, paused: Option<&str>) -> String {
        fn tail(text: &str) -> String {
            let start = text.char_indices().rev().nth(23).map_or(0, |(at, _)| at);
            format!(
                "{}{}",
                if start == 0 { "" } else { "..." },
                text.get(start..).unwrap_or_default()
            )
        }
        format!(
            "{}\nFind {} {}\nWith {} {}\nTab: field; Return: find next\nAlt+R: replace; Alt+A: replace all\nEsc/Ctrl+G: close; Ctrl+U: clear",
            paused.unwrap_or(&self.status),
            if self.with { " " } else { ">" }, tail(&self.query),
            if self.with { ">" } else { " " }, tail(&self.replacement),
        )
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
    fn explicit_find_replace_and_wrap_keep_one_edit_per_confirmation() {
        let mut ui = controller("é é");
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        prompt.type_chord("é", &mut history);
        prompt.type_chord("Tab", &mut history);
        prompt.type_chord("λ", &mut history);
        prompt.apply(&mut ui, &mut history, Action::One).unwrap();
        assert!(prompt.notice(None).contains("Select a match"));
        assert_eq!(ui.editor().document(1).unwrap().text(), "é é");
        prompt.apply(&mut ui, &mut history, Action::Find).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 0..2);
        prompt.apply(&mut ui, &mut history, Action::One).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "λ é");
        assert_eq!(prompt.target(ui.editor()), Ok((1, 1)));
        prompt.apply(&mut ui, &mut history, Action::Find).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 3..5);
        prompt.apply(&mut ui, &mut history, Action::Find).unwrap();
        assert!(prompt.notice(None).contains("End;"));
        prompt.apply(&mut ui, &mut history, Action::Find).unwrap();
        assert!(prompt.notice(None).contains("Wrapped;"));
        prompt.type_chord("C-u", &mut history); // Empty replacement deletes.
        prompt.apply(&mut ui, &mut history, Action::One).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "λ ");
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (2, 0));
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 2,
            command: Command::Undo,
        })
        .unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "λ é");
        assert_eq!(ui.editor().document(1).unwrap().selection().range(), 3..5);
        assert!(prompt.target(ui.editor()).is_err());
    }

    #[test]
    fn replace_all_is_nonoverlapping_atomic_and_undo_restores_directed_selection() {
        let mut ui = controller("aaaaa");
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 2,
                caret: 0,
            }),
        })
        .unwrap();
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        assert_eq!(prompt.query, "aa");
        prompt.type_chord("Tab", &mut history);
        prompt.type_chord("λ", &mut history);
        prompt.apply(&mut ui, &mut history, Action::All).unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "λλa");
        assert_eq!(ui.editor().document(1).unwrap().selection().caret, 5);
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (1, 0));
        assert!(prompt.notice(None).contains("Replaced 2"));
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert_eq!(
            ui.editor().document(1).unwrap().selection(),
            Selection {
                anchor: 2,
                caret: 0
            }
        );
        assert_eq!(ui.editor().document(1).unwrap().text(), "aaaaa");
    }

    #[test]
    fn entries_are_scalar_bounded_and_unknown_empty_identical_or_missing_are_nonediting() {
        let mut ui = controller("x");
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        for action in [Action::One, Action::All, Action::Find] {
            prompt.apply(&mut ui, &mut history, action).unwrap();
            assert!(prompt.notice(None).contains("nonempty"));
        }
        for _ in 0..QUERY_BYTES {
            prompt.type_chord("λ", &mut history);
        }
        assert_eq!(prompt.query.len(), QUERY_BYTES);
        prompt.type_chord("Backspace", &mut history);
        assert_eq!(prompt.query.len(), QUERY_BYTES - 2);
        prompt.type_chord("x", &mut history);
        prompt.type_chord("λ", &mut history);
        assert_eq!(prompt.query.len(), QUERY_BYTES - 1);
        let before = prompt.query.clone();
        for chord in ["F10", "C-x", "Return", "\n", "\0"] {
            prompt.type_chord(chord, &mut history);
        }
        assert_eq!(prompt.query, before);
        prompt.type_chord("S-Tab", &mut history);
        for _ in 0..QUERY_BYTES + 1 {
            prompt.type_chord("x", &mut history);
        }
        assert_eq!(prompt.replacement.len(), QUERY_BYTES);
        prompt.type_chord("C-u", &mut history);
        prompt.type_chord("x", &mut history);
        prompt.type_chord("Tab", &mut history);
        prompt.type_chord("C-u", &mut history);
        prompt.type_chord("X", &mut history);
        let before = ui.generation();
        prompt.apply(&mut ui, &mut history, Action::All).unwrap();
        assert_eq!(ui.generation(), before);
        assert!(prompt.notice(None).contains("No matches"));
        prompt.type_chord("C-u", &mut history);
        prompt.type_chord("x", &mut history);
        prompt.apply(&mut ui, &mut history, Action::All).unwrap();
        assert!(prompt.notice(None).contains("text unchanged"));
        assert_eq!(ui.editor().document(1).unwrap().text(), "x");
        assert_eq!(ui.editor().document(1).unwrap().revision(), 0);
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (0, 0));
        assert!(!ui.editor().document(1).unwrap().dirty());
    }

    #[test]
    fn foreign_tab_selection_revision_and_document_limit_never_retarget_or_partially_edit() {
        let mut ui = controller(&"x".repeat(4097));
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        prompt.query = "x".into();
        prompt.replacement = "y".repeat(QUERY_BYTES);
        let before = format!("{:?}", ui.editor());
        prompt.apply(&mut ui, &mut history, Action::All).unwrap(); // > 16 MiB.
        assert!(prompt.notice(None).contains("refused: limit"));
        assert_eq!(format!("{:?}", ui.editor()), before);
        assert!(prompt.target(controller("x").editor()).is_err());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 0,
                caret: 1,
            }),
        })
        .unwrap();
        assert!(prompt.apply(&mut ui, &mut history, Action::All).is_err());
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        ui.dispatch(Event::New).unwrap();
        assert!(prompt.apply(&mut ui, &mut history, Action::All).is_err());
        ui.dispatch(Event::SelectTab(1)).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("z".into()),
        })
        .unwrap();
        assert!(prompt.apply(&mut ui, &mut history, Action::All).is_err());
        assert_eq!(ui.editor().document(1).unwrap().revision(), 1);
    }

    #[test]
    fn identical_single_replacement_collapses_without_editing_and_field_switch_keeps_result() {
        let mut ui = controller("x x");
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 1,
                caret: 0,
            }),
        })
        .unwrap();
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        prompt.type_chord("Tab", &mut history);
        prompt.type_chord("x", &mut history);
        prompt.apply(&mut ui, &mut history, Action::One).unwrap();
        let doc = ui.editor().document(1).unwrap();
        assert_eq!(doc.text(), "x x");
        assert_eq!(doc.revision(), 0);
        assert_eq!(doc.history_depth(), (0, 0));
        assert!(!doc.dirty());
        assert_eq!(
            doc.selection(),
            Selection {
                anchor: 1,
                caret: 1
            }
        );
        assert!(prompt.notice(None).contains("1 match; text unchanged."));
        prompt.type_chord("Tab", &mut history);
        assert!(prompt.notice(None).contains("1 match; text unchanged."));
        prompt.type_chord("y", &mut history);
        assert!(!prompt.notice(None).contains("text unchanged"));
    }

    #[test]
    fn long_scalar_tails_and_pause_preserve_six_rows_of_action_guidance() {
        let ui = controller("");
        let mut history = History::default();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, &history).unwrap();
        for _ in 0..25 {
            prompt.type_chord("λ", &mut history);
        }
        prompt.type_chord("Tab", &mut history);
        for _ in 0..24 {
            prompt.type_chord("é", &mut history);
        }
        let notice = prompt.notice(None);
        assert!(notice.contains(&format!("Find   ...{}", "λ".repeat(24))));
        assert!(notice.contains(&format!("With > {}", "é".repeat(24))));
        assert!(!notice.contains(&"é".repeat(25)));
        for pause in [
            None,
            Some("Replace paused: restore seat/keymap."),
            Some("Replace paused: focus; tap Shift."),
        ] {
            let notice = prompt.notice(pause);
            assert_eq!(notice.lines().count(), 6);
            assert!(notice.lines().all(|line| line.chars().count() <= 38));
            assert!(notice.ends_with("Esc/Ctrl+G: close; Ctrl+U: clear"));
        }
    }
}
