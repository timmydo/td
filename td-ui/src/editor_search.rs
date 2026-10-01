//! Literal search history and explicit end-before-wrap admission: a find
//! that reaches the end stops there, and only the same query, direction,
//! tab, revision and selection asked again wraps.

use crate::editor::{Controller, Event};
use crate::editor_error::{Error, Result};
use crate::editor_model::{Command, Editor, RevisionPoint, Selection, TabId};

/// The longest query a find admits, in bytes.
pub const QUERY_BYTES: usize = 4096;

/// What a find was asked against: a tab at a revision, its selection and
/// the direction. A prompt holds one so its submission cannot search from
/// a selection or document that changed while the query was typed.
pub struct Intent {
    point: RevisionPoint,
    selection: Selection,
    backward: bool,
}

impl Intent {
    /// The active `tab` at `revision`, refused otherwise.
    pub fn capture(editor: &Editor, tab: TabId, revision: u64, backward: bool) -> Result<Self> {
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

    /// The tab and revision asked against.
    pub fn point(&self) -> &RevisionPoint {
        &self.point
    }

    fn matches(&self, editor: &Editor, backward: bool) -> bool {
        self.backward == backward && self.matches_target(editor)
    }

    /// Whether the tab is still active at its revision and selection.
    pub fn matches_target(&self, editor: &Editor) -> bool {
        editor.check_revision(&self.point).is_ok()
            && editor.active() == Some(self.point.tab)
            && editor
                .document(self.point.tab)
                .is_ok_and(|doc| doc.selection() == self.selection)
    }
}

#[derive(Default)]
/// The last query and the stop at an end a repeated find may wrap past.
pub struct History {
    query: String,
    boundary: Option<Intent>,
}

// A query names what was looked for, so it is zeroed when it is freed, as
// a document's text is: a host that replaces its history at lock leaves no
// query behind, and a new query wipes the last.
impl Drop for History {
    fn drop(&mut self) {
        crate::editor_model::wipe(&mut self.query);
    }
}

#[derive(Debug, Eq, PartialEq)]
/// What a find did: matched, matched after wrapping, stopped at the end,
/// or found nothing even wrapping.
pub enum Found {
    Match,
    Wrapped,
    End,
    Missing,
}

impl History {
    /// The last query a find was asked for.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Forgets the stop at an end, so the next find stops there again.
    pub fn cancel_wrap(&mut self) {
        self.boundary = None;
    }

    /// Forgets the stop once the tab, revision or selection it was asked
    /// against has moved.
    pub fn observe(&mut self, editor: &Editor) {
        if self
            .boundary
            .as_ref()
            .is_some_and(|intent| !intent.matches_target(editor))
        {
            self.boundary = None;
        }
    }

    /// Finds `query` from the selection, selecting the match. Reaching
    /// the end stops there; the same find asked again wraps.
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
                crate::editor_model::wipe(&mut self.query);
                self.query = query.to_owned();
                self.boundary = None;
                Ok(if wrap { Found::Wrapped } else { Found::Match })
            }
            Err(Error::Unavailable) => {
                crate::editor_model::wipe(&mut self.query);
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
}
