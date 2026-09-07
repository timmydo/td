//! Bounded numeric entry for logical lines and per-document fill columns.

use crate::model::{Command, Editor, RevisionPoint, Selection, TabId};
use crate::{Error, Result};

pub(crate) const DIGITS: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Line,
    FillColumn,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Line => "Go To Line",
            Self::FillColumn => "Fill Column",
        }
    }
    pub fn success(self) -> &'static str {
        match self {
            Self::Line => "Moved to logical line.",
            Self::FillColumn => "Fill column set; existing text is unchanged.",
        }
    }
    pub fn changed(self) -> &'static str {
        match self {
            Self::Line => "document or selection changed",
            Self::FillColumn => "document, selection or setting changed",
        }
    }
}

pub(crate) struct Prompt {
    point: RevisionPoint,
    selection: Selection,
    text: String,
    invalid: bool,
    kind: Kind,
    column: usize,
}

impl Prompt {
    pub fn new(editor: &Editor, tab: TabId, revision: u64, kind: Kind) -> Result<Self> {
        let point = editor.revision_point(tab, revision)?;
        if editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        let doc = editor.document(tab)?;
        Ok(Self {
            point,
            selection: doc.selection(),
            text: String::new(),
            invalid: false,
            kind,
            column: doc.fill_column(),
        })
    }

    pub fn target(&self, editor: &Editor) -> Result<(TabId, u64)> {
        editor.check_revision(&self.point)?;
        let doc = editor.document(self.point.tab)?;
        if editor.active() != Some(self.point.tab)
            || doc.selection() != self.selection
            || (self.kind == Kind::FillColumn && doc.fill_column() != self.column)
        {
            return Err(Error::StaleRevision);
        }
        Ok((self.point.tab, self.point.revision))
    }

    pub fn entry(&self) -> (&str, bool) {
        (&self.text, self.invalid)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn command(&self) -> Result<Command> {
        let value = self.parse_value()?;
        match self.kind {
            Kind::Line => Ok(Command::GoToLine(value)),
            Kind::FillColumn if (20..=240).contains(&value) => Ok(Command::FillColumn(value)),
            Kind::FillColumn => Err(Error::InvalidArgument),
        }
    }

    fn parse_value(&self) -> Result<usize> {
        let line = self
            .text
            .parse::<usize>()
            .map_err(|_| Error::InvalidArgument)?;
        if line == 0 {
            return Err(Error::InvalidArgument);
        }
        Ok(line)
    }

    pub fn refused(&mut self) {
        self.invalid = true;
    }

    pub fn type_chord(&mut self, chord: &str) {
        match chord {
            "Backspace" => {
                self.text.pop();
                self.invalid = false;
            }
            "C-u" => {
                self.text.clear();
                self.invalid = false;
            }
            _ if matches!(chord.as_bytes(), [b] if b.is_ascii_digit())
                && self.text.len() < DIGITS =>
            {
                self.text.push_str(chord);
                self.invalid = false;
            }
            _ => {}
        }
    }

    pub fn notice(&self) -> String {
        if self.kind == Kind::FillColumn {
            return format!(
                "{}Fill Column: {}|\nAt open: {}; enter 20 through 240\nReturn: set; Escape/Ctrl+G: cancel; Ctrl+U: clear",
                if self.invalid { "Invalid fill column; use 20 through 240.\n" } else { "" },
                self.text, self.column,
            );
        }
        format!(
            "{}Go To Line: {}|\nOne-based logical line\nReturn: go; Escape/Ctrl+G: cancel; Ctrl+U: clear",
            if self.invalid { "Invalid line number; use an existing line.\n" } else { "" },
            self.text,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::Command;
    use crate::ui::{Controller, Event};

    #[test]
    fn numeric_fill_validation_matches_model_admission_at_the_bounds() {
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        for value in (0..=260).chain([1000, usize::MAX]) {
            let mut prompt = Prompt::new(ui.editor(), 1, 0, Kind::FillColumn).unwrap();
            for c in value.to_string().chars() {
                prompt.type_chord(&c.to_string());
            }
            let parsed = prompt.command().is_ok();
            let admitted = ui
                .dispatch(Event::Edit {
                    tab: 1,
                    revision: 0,
                    command: Command::FillColumn(value),
                })
                .is_ok();
            assert_eq!(parsed, admitted, "{value}");
        }
    }

    #[test]
    fn fill_column_entry_preserves_text_and_refuses_stale_settings() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"one two three")).unwrap();
        for value in ["20", "72", "240", "0024"] {
            let mut prompt = Prompt::new(ui.editor(), 1, 0, Kind::FillColumn).unwrap();
            for c in value.chars() {
                prompt.type_chord(&c.to_string());
            }
            prompt.target(ui.editor()).unwrap();
            ui.dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: prompt.command().unwrap(),
            })
            .unwrap();
            let doc = ui.editor().document(1).unwrap();
            assert_eq!(doc.fill_column(), value.parse::<usize>().unwrap());
            assert_eq!(doc.text(), "one two three");
            assert_eq!(doc.revision(), 0);
            assert_eq!(doc.history_depth(), (0, 0));
            assert!(!doc.dirty());
            assert_eq!(
                doc.selection(),
                Selection {
                    anchor: 0,
                    caret: 0
                }
            );
        }
        for value in ["", "0", "19", "241", "2400", "99999999999999999999"] {
            let mut prompt = Prompt::new(ui.editor(), 1, 0, Kind::FillColumn).unwrap();
            for c in value.chars() {
                prompt.type_chord(&c.to_string());
            }
            assert!(matches!(prompt.command(), Err(Error::InvalidArgument)));
            prompt.refused();
            assert!(prompt.notice().starts_with("Invalid fill column"));
        }
        let prompt = Prompt::new(ui.editor(), 1, 0, Kind::FillColumn).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::FillColumn(40),
        })
        .unwrap();
        assert_eq!(prompt.target(ui.editor()), Err(Error::StaleRevision));
    }

    #[test]
    fn logical_lines_include_empty_final_line_and_refuse_missing_lines() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load("é long line\n\nlast\n".as_bytes()))
            .unwrap();
        let before = ui.generation();
        for (line, at) in [(1, 0), (2, 13), (3, 14), (4, 19)] {
            ui.dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::GoToLine(line),
            })
            .unwrap();
            assert_eq!(
                ui.editor().document(1).unwrap().selection(),
                Selection {
                    anchor: at,
                    caret: at
                }
            );
        }
        assert!(ui.generation() > before);
        let generation = ui.generation();
        for (line, error) in [
            (0, Error::InvalidArgument),
            (5, Error::InvalidPosition),
            (usize::MAX, Error::InvalidPosition),
        ] {
            assert_eq!(
                ui.dispatch(Event::Edit {
                    tab: 1,
                    revision: 0,
                    command: Command::GoToLine(line)
                }),
                Err(error)
            );
            assert_eq!(ui.generation(), generation);
            assert_eq!(ui.editor().document(1).unwrap().selection().caret, 19);
        }
        assert!(!ui.editor().document(1).unwrap().dirty());
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (0, 0));
        ui.dispatch(Event::New).unwrap();
        ui.dispatch(Event::Edit {
            tab: 2,
            revision: 0,
            command: Command::GoToLine(1),
        })
        .unwrap();
        assert_eq!(
            ui.dispatch(Event::Edit {
                tab: 2,
                revision: 0,
                command: Command::GoToLine(2)
            }),
            Err(Error::InvalidPosition)
        );
    }

    #[test]
    fn logical_line_destination_is_independent_of_visual_wrapping() {
        let mut ui = Controller::default();
        let text = format!("é{}\nnext", "x".repeat(200));
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        ui.dispatch(Event::Resize {
            width: 320,
            height: 240,
            scale: 1,
        })
        .unwrap();
        for enabled in [false, true] {
            ui.dispatch(Event::Wrap {
                tab: 1,
                revision: 0,
                enabled,
            })
            .unwrap();
            ui.dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::GoToLine(2),
            })
            .unwrap();
            assert_eq!(
                ui.editor().document(1).unwrap().selection(),
                Selection {
                    anchor: 203,
                    caret: 203
                }
            );
        }
    }

    #[test]
    fn numeric_entry_is_bounded_and_bound_to_original_document_selection() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"a\nb")).unwrap();
        let mut prompt = Prompt::new(ui.editor(), 1, 0, Kind::Line).unwrap();
        for chord in ["-", "Space", "x", "λ", "C-x"] {
            prompt.type_chord(chord);
        }
        assert!(prompt.parse_value().is_err());
        prompt.type_chord("0");
        assert!(prompt.parse_value().is_err());
        prompt.type_chord("2");
        assert_eq!(prompt.parse_value(), Ok(2));
        prompt.type_chord("C-u");
        for _ in 0..30 {
            prompt.type_chord("9");
        }
        assert_eq!(prompt.text.len(), 20);
        assert!(prompt.parse_value().is_err());
        prompt.type_chord("Backspace");
        assert_eq!(prompt.text.len(), 19);
        assert!(prompt.target(ui.editor()).is_ok());
        let mut foreign = Controller::default();
        foreign.dispatch(Event::Load(b"a\nb")).unwrap();
        assert!(prompt.target(foreign.editor()).is_err());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::GoToLine(2),
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
        let prompt = Prompt::new(ui.editor(), 1, 0, Kind::Line).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("c".into()),
        })
        .unwrap();
        assert!(prompt.target(ui.editor()).is_err());
    }

    #[test]
    fn replay_uses_the_same_logical_line_command_and_refuses_bad_arguments() {
        let mut session = crate::replay::Session::default();
        session
            .ui
            .dispatch(Event::Load("é\nnext".as_bytes()))
            .unwrap();
        assert!(session
            .request(b"1\t1\tgo-to-line\t1\t0\t2")
            .contains("\tok\t"));
        assert_eq!(
            session.ui.editor().document(1).unwrap().selection().caret,
            3
        );
        for command in [
            "1\t2\tgo-to-line\t1\t0\t0",
            "1\t3\tgo-to-line\t1\t0\t3",
            "1\t4\tgo-to-line\t1\t1\t1",
            "1\t5\tgo-to-line\t1\t0\t-1",
            "1\t6\tgo-to-line\t1\t0\t18446744073709551616",
            "1\t7\tgo-to-line\t1\t0\t1\textra",
        ] {
            assert!(session.request(command.as_bytes()).contains("\terror\t"));
            assert_eq!(
                session.ui.editor().document(1).unwrap().selection().caret,
                3
            );
        }
    }
}
