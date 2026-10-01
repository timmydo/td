//! Close decisions are explicit, editor/revision-bound and deferred until the
//! entire close request is resolved. Dropping a request is cancellation.

use crate::editor::{Controller, Event};
use crate::editor_error::{Error, Result};
use crate::editor_model::{Editor, RevisionPoint, TabId};
use std::collections::BTreeSet;

/// No public constructor: only a completed close dialog can approve discard.
pub struct Discard {
    point: RevisionPoint,
}
impl Discard {
    pub(crate) fn apply(self, editor: &mut Editor) -> Result<()> {
        editor.discard_tab(self.point)
    }
}

/// Only an answer to a live conflict dialog can authorize replacement.
pub struct Reload {
    point: RevisionPoint,
}
impl Reload {
    /// Whether the permit's tab is still at the revision it was granted for.
    pub fn check(&self, editor: &Editor) -> Result<()> {
        editor.check_revision(&self.point)
    }
    /// The tab whose replacement this permit authorizes.
    pub fn tab(&self) -> TabId {
        self.point.tab
    }
    pub(crate) fn apply(self, editor: &mut Editor, bytes: &[u8], missing: bool) -> Result<()> {
        editor.reload_bytes(self.point, bytes, missing)
    }
}

/// A reload offered for one tab at one revision, answered once: a clean tab
/// yields its `Reload` at once, a dirty one only after a second, discarding
/// answer. Any edit in between makes it stale.
pub struct Conflict {
    point: Option<RevisionPoint>,
    discard: bool,
}
impl Conflict {
    /// A conflict over `target`, refused unless it is current.
    pub fn new(editor: &Editor, target: Target) -> Result<Self> {
        Ok(Self {
            point: Some(editor.revision_point(target.tab, target.revision)?),
            discard: false,
        })
    }
    /// The tab and revision asked about, refused once stale or answered.
    pub fn target(&self, editor: &Editor) -> Result<Target> {
        let point = self.point.as_ref().ok_or(Error::InvalidArgument)?;
        editor.check_revision(point)?;
        Ok(Target {
            tab: point.tab,
            revision: point.revision,
        })
    }
    /// Whether the dirty tab's discard is now the question.
    pub fn needs_discard(&self) -> bool {
        self.discard
    }
    /// The answer to the question asked: `discard` only once
    /// `needs_discard` says so. The permit, when the answer grants it.
    pub fn answer(&mut self, editor: &Editor, discard: bool) -> Result<Option<Reload>> {
        let target = self.target(editor)?;
        if discard && !self.discard {
            return Err(Error::InvalidArgument);
        }
        if editor.document(target.tab)?.dirty() && !discard {
            self.discard = true;
            return Ok(None);
        }
        Ok(Some(Reload {
            point: self.point.take().ok_or(Error::InvalidArgument)?,
        }))
    }
}

#[derive(Clone, Copy)]
/// What a close request covers: one tab at a revision, or every tab.
pub enum Scope {
    Tab { tab: TabId, revision: u64 },
    Window,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A tab at a revision a dialog asks about.
pub struct Target {
    pub tab: TabId,
    pub revision: u64,
}
#[derive(Debug, Eq, PartialEq)]
/// What a completed close request closed.
pub enum Closed {
    Tab(TabId),
    Window,
}

/// A close request: the dirty tabs it covers are asked about one at a
/// time, the active one first, and nothing closes until every one is
/// approved. Any edit to a covered tab makes it stale.
pub struct Close {
    scope: Scope,
    points: Vec<RevisionPoint>,
    discarded: BTreeSet<TabId>,
}

impl Close {
    /// What the request covers.
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// A request over `scope`, binding each covered tab's revision.
    pub fn new(editor: &Editor, scope: Scope) -> Result<Self> {
        let mut points = match scope {
            Scope::Tab { tab, revision } => vec![editor.revision_point(tab, revision)?],
            Scope::Window => editor
                .tabs()
                .map(|(tab, doc)| editor.revision_point(tab, doc.revision()))
                .collect::<Result<Vec<_>>>()?,
        };
        // Ask about the active document first without changing the selection,
        // current tab or any view state that Cancel must preserve.
        if let Some(index) = points.iter().position(|p| Some(p.tab) == editor.active()) {
            points.rotate_left(index);
        }
        Ok(Self {
            scope,
            points,
            discarded: BTreeSet::new(),
        })
    }

    fn validate(&self, editor: &Editor) -> Result<()> {
        if matches!(self.scope, Scope::Window) && editor.tabs().count() != self.points.len() {
            return Err(Error::StaleRevision);
        }
        for point in &self.points {
            editor.check_revision(point)?;
        }
        Ok(())
    }

    /// The next dirty tab to ask about, or none once all are approved.
    pub fn next(&self, editor: &Editor) -> Result<Option<Target>> {
        self.validate(editor)?;
        for point in &self.points {
            if editor.document(point.tab)?.dirty() && !self.discarded.contains(&point.tab) {
                return Ok(Some(Target {
                    tab: point.tab,
                    revision: point.revision,
                }));
            }
        }
        Ok(None)
    }

    /// Approves discarding `target`, which must be the one `next` names.
    pub fn discard(&mut self, editor: &Editor, target: Target) -> Result<()> {
        if self.next(editor)? != Some(target) {
            return Err(Error::StaleRevision);
        }
        self.discarded.insert(target.tab);
        Ok(())
    }

    /// Closes what was approved, refused while a dirty tab is unanswered.
    pub fn complete(self, ui: &mut Controller) -> Result<Closed> {
        if self.next(ui.editor())?.is_some() {
            return Err(Error::Dirty);
        }
        match self.scope {
            Scope::Window => Ok(Closed::Window),
            Scope::Tab { tab, revision } => {
                if ui.editor().document(tab)?.dirty() {
                    // Tab scope has one point: next == None already proves
                    // that this still-dirty document was explicitly approved.
                    let point = self.points.into_iter().next().ok_or(Error::MissingTab)?;
                    ui.dispatch(Event::Discard(Discard { point }))?;
                } else {
                    ui.dispatch(Event::Close { tab, revision })?;
                }
                Ok(Closed::Tab(tab))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::editor_model::Command;

    fn dirty(ui: &mut Controller) -> TabId {
        ui.dispatch(Event::New).unwrap();
        let tab = ui.editor().active().unwrap();
        ui.dispatch(Event::Edit {
            tab,
            revision: 0,
            command: Command::Insert("unsaved".into()),
        })
        .unwrap();
        tab
    }

    #[test]
    fn conflict_reload_requires_live_explicit_discard_and_cannot_repeat_a_permit() {
        let mut ui = Controller::default();
        let tab = dirty(&mut ui);
        let target = Target { tab, revision: 1 };
        let mut conflict = Conflict::new(ui.editor(), target).unwrap();
        assert!(conflict.answer(ui.editor(), true).is_err());
        assert!(conflict.answer(ui.editor(), false).unwrap().is_none());
        assert!(conflict.needs_discard());
        let permit = conflict.answer(ui.editor(), true).unwrap().unwrap();
        assert!(conflict.answer(ui.editor(), true).is_err());
        ui.dispatch(Event::Reload {
            permit,
            bytes: b"disk",
            missing: false,
        })
        .unwrap();
        assert_eq!(ui.editor().document(tab).unwrap().text(), "disk");
        assert!(!ui.editor().document(tab).unwrap().dirty());
        let target = Target { tab, revision: 2 };
        let mut clean = Conflict::new(ui.editor(), target).unwrap();
        assert!(clean.answer(ui.editor(), false).unwrap().is_some());
        let mut stale = Conflict::new(ui.editor(), target).unwrap();
        ui.dispatch(Event::Edit {
            tab,
            revision: 2,
            command: Command::Insert("new".into()),
        })
        .unwrap();
        assert!(stale.answer(ui.editor(), false).is_err());
        assert_eq!(ui.editor().document(tab).unwrap().text(), "newdisk");
    }

    #[test]
    fn reload_reveals_origin_preserves_wrap_and_leaves_other_views_alone() {
        let mut ui = Controller::default();
        let text = format!("{}\n", "x".repeat(120)).repeat(40);
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        let tab = ui.editor().active().unwrap();
        ui.dispatch(Event::Resize {
            width: 272,
            height: 160,
            scale: 1,
        })
        .unwrap();
        ui.dispatch(Event::Wrap {
            tab,
            revision: 0,
            enabled: false,
        })
        .unwrap();
        ui.dispatch(Event::Scroll {
            tab,
            revision: 0,
            rows: 10,
            columns: 10,
        })
        .unwrap();
        assert!(ui.tab_view(tab).unwrap().viewport.origin().row > 0);
        ui.dispatch(Event::New).unwrap();
        let other = ui.editor().active().unwrap();
        let other_view = ui.tab_view(other).unwrap();
        let mut conflict = Conflict::new(ui.editor(), Target { tab, revision: 0 }).unwrap();
        let permit = conflict.answer(ui.editor(), false).unwrap().unwrap();
        ui.dispatch(Event::Reload {
            permit,
            bytes: b"new",
            missing: false,
        })
        .unwrap();
        let view = ui.tab_view(tab).unwrap();
        assert!(!view.soft_wrap);
        assert_eq!(
            view.viewport.origin(),
            crate::editor_layout::Position { row: 0, column: 0 }
        );
        assert_eq!(ui.editor().active(), Some(other));
        assert_eq!(ui.tab_view(other).unwrap(), other_view);
    }

    #[test]
    fn window_discard_is_deferred_and_cancel_keeps_every_tab() {
        let mut ui = Controller::default();
        let first = dirty(&mut ui);
        let second = dirty(&mut ui);
        let mut close = Close::new(ui.editor(), Scope::Window).unwrap();
        let target = close.next(ui.editor()).unwrap().unwrap();
        assert_eq!(target.tab, second);
        close.discard(ui.editor(), target).unwrap();
        assert_eq!(close.next(ui.editor()).unwrap().unwrap().tab, first);
        assert_eq!(ui.editor().tabs().count(), 2);
        drop(close);
        assert_eq!(ui.editor().active(), Some(second));
        for tab in [first, second] {
            assert_eq!(ui.editor().document(tab).unwrap().text(), "unsaved");
            assert!(ui.editor().document(tab).unwrap().dirty());
        }
    }

    #[test]
    fn tab_discard_requires_current_explicit_approval() {
        let mut ui = Controller::default();
        let tab = dirty(&mut ui);
        let scope = Scope::Tab { tab, revision: 1 };
        assert_eq!(
            Close::new(ui.editor(), scope).unwrap().complete(&mut ui),
            Err(Error::Dirty)
        );
        let mut close = Close::new(ui.editor(), scope).unwrap();
        assert_eq!(
            close.discard(ui.editor(), Target { tab, revision: 0 }),
            Err(Error::StaleRevision)
        );
        close
            .discard(ui.editor(), Target { tab, revision: 1 })
            .unwrap();
        assert_eq!(close.complete(&mut ui), Ok(Closed::Tab(tab)));
        assert_eq!(ui.editor().tabs().count(), 0);
    }

    #[test]
    fn clean_close_needs_no_discard_and_window_cannot_skip_dirty_choices() {
        let mut ui = Controller::default();
        ui.dispatch(Event::New).unwrap();
        let tab = ui.editor().active().unwrap();
        let close = Close::new(ui.editor(), Scope::Tab { tab, revision: 0 }).unwrap();
        assert_eq!(close.next(ui.editor()), Ok(None));
        assert_eq!(close.complete(&mut ui), Ok(Closed::Tab(tab)));
        let tab = dirty(&mut ui);
        assert_eq!(
            Close::new(ui.editor(), Scope::Window)
                .unwrap()
                .complete(&mut ui),
            Err(Error::Dirty)
        );
        assert!(ui.editor().document(tab).unwrap().dirty());
    }

    #[test]
    fn edits_new_tabs_and_other_editors_invalidate_decisions() {
        let mut ui = Controller::default();
        let tab = dirty(&mut ui);
        let mut close = Close::new(ui.editor(), Scope::Window).unwrap();
        close
            .discard(ui.editor(), Target { tab, revision: 1 })
            .unwrap();
        ui.dispatch(Event::Edit {
            tab,
            revision: 1,
            command: Command::Insert("later".into()),
        })
        .unwrap();
        assert_eq!(close.complete(&mut ui), Err(Error::StaleRevision));
        let close = Close::new(ui.editor(), Scope::Window).unwrap();
        ui.dispatch(Event::New).unwrap();
        assert_eq!(close.complete(&mut ui), Err(Error::StaleRevision));
        let mut other = Controller::default();
        dirty(&mut other);
        let close = Close::new(other.editor(), Scope::Tab { tab, revision: 1 }).unwrap();
        assert_eq!(close.next(ui.editor()), Err(Error::InvalidArgument));
    }

    #[test]
    fn saves_stay_saved_when_a_later_close_decision_is_cancelled() {
        let mut ui = Controller::default();
        let first = dirty(&mut ui);
        let second = dirty(&mut ui);
        let close = Close::new(ui.editor(), Scope::Window).unwrap();
        let (point, _) = ui.editor().save_snapshot(second).unwrap();
        ui.dispatch(Event::Saved(point)).unwrap();
        assert_eq!(close.next(ui.editor()).unwrap().unwrap().tab, first);
        drop(close);
        assert!(!ui.editor().document(second).unwrap().dirty());
        assert!(ui.editor().document(first).unwrap().dirty());
        assert_eq!(ui.editor().tabs().count(), 2);
    }
}
