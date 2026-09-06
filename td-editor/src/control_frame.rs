//! Native redraw generations and immutable submitted/callback snapshots.

use crate::render::Geometry;
use crate::ui::Controller;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Stamp {
    generation: u64,
    controller: u64,
    tab: u64,
    revision: u64,
    geometry: Geometry,
}

impl Stamp {
    pub(crate) fn generation(self) -> u64 {
        self.generation
    }

    pub(crate) fn fields(self) -> String {
        let (width, height) = self.geometry.dimensions();
        format!(
            "{},{},{},{},{width},{height},{}",
            self.generation,
            self.controller,
            self.tab,
            self.revision,
            self.geometry.scale().value(),
        )
    }
}

pub(crate) struct Frames {
    requested: Option<u64>,
    submitted: Option<Stamp>,
    completed: Option<Stamp>,
    dirty: bool,
}

impl Default for Frames {
    fn default() -> Self {
        Self {
            requested: Some(1),
            submitted: None,
            completed: None,
            dirty: true,
        }
    }
}

impl Frames {
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Overflow poisons the window; the adapter must exit, never wrap a fence.
    pub(crate) fn invalidate(&mut self, changed: bool) {
        if changed {
            self.requested = self.requested.and_then(|n| n.checked_add(1));
            self.dirty = true;
        }
    }

    pub(crate) fn generation(&self) -> Result<u64> {
        self.requested.ok_or(Error::Exhausted)
    }

    pub(crate) fn capture(&self, ui: &Controller) -> Result<Stamp> {
        let generation = self.generation()?;
        let tab = ui.editor().active().unwrap_or(0);
        let revision = if tab == 0 {
            0
        } else {
            ui.editor().document(tab)?.revision()
        };
        Ok(Stamp {
            generation,
            controller: ui.generation(),
            tab,
            revision,
            geometry: ui.geometry(),
        })
    }

    pub(crate) fn submit(&mut self, stamp: Stamp) -> Result<()> {
        if stamp.generation() != self.generation()? {
            return Err(Error::StaleRevision);
        }
        self.submitted = Some(stamp);
        self.dirty = false;
        Ok(())
    }

    /// Called only for the outstanding main-surface callback, not a release.
    pub(crate) fn complete(&mut self) -> Result<()> {
        self.generation()?;
        self.completed = Some(self.submitted.ok_or(Error::Protocol)?);
        Ok(())
    }

    pub(crate) fn wait(&self, target: u64) -> Result<Option<Stamp>> {
        if target == 0 || target > self.generation()? {
            return Err(Error::InvalidArgument);
        }
        Ok(self.completed.filter(|stamp| stamp.generation >= target))
    }

    pub(crate) fn fields(&self) -> Result<String> {
        Ok(format!(
            "window-generation={}\tframe-submitted={}\tframe-completed={}",
            self.generation()?,
            self.submitted.map_or_else(|| "-".into(), Stamp::fields),
            self.completed.map_or_else(|| "-".into(), Stamp::fields),
        ))
    }

    #[cfg(test)]
    pub(crate) fn clear_damage_for_test(&mut self) {
        // Arrange a clean flag without inventing a Wayland commit.
        self.dirty = false;
    }

    #[cfg(test)]
    pub(crate) fn exhaust_for_test(&mut self) {
        self.requested = Some(u64::MAX);
        self.invalidate(true);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::Command;
    use crate::ui::Event;

    #[test]
    fn callbacks_retain_rendered_revision_not_the_newer_controller() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"a")).unwrap();
        let mut frames = Frames::default();
        let old = frames.capture(&ui).unwrap();
        frames.submit(old).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("b".into()),
        })
        .unwrap();
        frames.invalidate(true);
        assert_eq!(frames.wait(1), Ok(None));
        frames.complete().unwrap();
        assert_eq!(frames.wait(1), Ok(Some(old)));
        assert_eq!(frames.wait(2), Ok(None));
        assert_eq!(old.revision, 0);
        let new = frames.capture(&ui).unwrap();
        assert_eq!(new.revision, 1);
        frames.submit(new).unwrap();
        assert_eq!(frames.wait(2), Ok(None));
        frames.complete().unwrap();
        assert_eq!(frames.wait(2), Ok(Some(new)));
        assert_eq!(frames.wait(1), Ok(Some(new)));
    }

    #[test]
    fn snapshot_fields_are_fixed_and_empty_documents_are_explicit() {
        let ui = Controller::default();
        let mut frames = Frames::default();
        assert_eq!(
            frames.fields().unwrap(),
            "window-generation=1\tframe-submitted=-\tframe-completed=-"
        );
        let stamp = frames.capture(&ui).unwrap();
        assert_eq!(stamp.fields(), "1,0,0,0,800,600,1");
        frames.submit(stamp).unwrap();
        assert_eq!(
            frames.fields().unwrap(),
            "window-generation=1\tframe-submitted=1,0,0,0,800,600,1\tframe-completed=-"
        );
        frames.complete().unwrap();
        assert_eq!(frames.fields().unwrap(), "window-generation=1\tframe-submitted=1,0,0,0,800,600,1\tframe-completed=1,0,0,0,800,600,1");
    }

    #[test]
    fn generations_do_not_wrap_and_native_only_damage_changes_the_fence() {
        let ui = Controller::default();
        let mut frames = Frames::default();
        assert_eq!(frames.wait(0), Err(Error::InvalidArgument));
        assert_eq!(frames.wait(2), Err(Error::InvalidArgument));
        assert_eq!(frames.complete(), Err(Error::Protocol));
        let before = frames.capture(&ui).unwrap();
        frames.invalidate(false);
        assert_eq!(frames.capture(&ui).unwrap(), before);
        frames.invalidate(true);
        let after = frames.capture(&ui).unwrap();
        assert_eq!(after.controller, before.controller);
        assert_eq!(after.generation, before.generation + 1);
        assert_eq!(frames.submit(before), Err(Error::StaleRevision));
        frames.requested = Some(u64::MAX);
        frames.invalidate(true);
        assert_eq!(frames.generation(), Err(Error::Exhausted));
        assert_eq!(frames.wait(1), Err(Error::Exhausted));
        assert_eq!(frames.capture(&ui), Err(Error::Exhausted));
        assert_eq!(frames.complete(), Err(Error::Exhausted));
        frames.invalidate(true);
        assert_eq!(frames.generation(), Err(Error::Exhausted));
    }
}
