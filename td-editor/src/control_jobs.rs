//! Bounded remote job outcomes. No text, filesystem, clocks or transport.

use crate::model::{Editor, TabId};
use crate::spelling::{ScanStatus, WindowState};
use crate::{Error, Result};
use std::collections::VecDeque;
use std::fmt::Write;

const RECORDS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReloadOutcome {
    Complete,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    Pending,
    Complete,
    Cancelled,
    Failed(Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Spelling,
    Open,
    Save,
    SaveAs,
    Reload,
    Dictionary,
    Rename,
    Delete,
}

#[derive(Clone, Copy, Debug)]
struct Record {
    id: u64,
    kind: Kind,
    tab: TabId,
    revision: u64,
    scan: Option<u64>,
    status: Status,
}

pub(crate) struct Jobs {
    last: u64,
    records: VecDeque<Record>,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            last: 0,
            records: VecDeque::with_capacity(RECORDS),
        }
    }
}

impl Jobs {
    /// Reserve before invoking the shared action. Failure cannot evict history.
    pub(crate) fn begin(&mut self, tab: TabId, revision: u64) -> Result<u64> {
        self.reserve(Kind::Spelling, tab, revision)
    }

    pub(crate) fn begin_open(&mut self) -> Result<u64> {
        self.reserve(Kind::Open, 0, 0)
    }

    pub(crate) fn begin_reload(&mut self, tab: TabId, revision: u64) -> Result<u64> {
        self.reserve(Kind::Reload, tab, revision)
    }

    pub(crate) fn begin_dictionary(&mut self) -> Result<u64> {
        self.reserve(Kind::Dictionary, 0, 0)
    }

    pub(crate) fn begin_rename(&mut self, tab: TabId, revision: u64) -> Result<u64> {
        self.reserve(Kind::Rename, tab, revision)
    }

    pub(crate) fn begin_delete(&mut self, tab: TabId, revision: u64) -> Result<u64> {
        self.reserve(Kind::Delete, tab, revision)
    }

    pub(crate) fn deleted(&mut self, id: u64, result: Result<()>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Delete || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        record.status = match result {
            Ok(()) => Status::Complete,
            Err(error) => Status::Failed(error),
        };
        Ok(())
    }

    pub(crate) fn renamed(&mut self, id: u64, result: Result<()>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Rename || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        record.status = match result {
            Ok(()) => Status::Complete,
            Err(error) => Status::Failed(error),
        };
        Ok(())
    }

    pub(crate) fn dictionary(&mut self, id: u64, result: Result<()>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Dictionary || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        record.status = match result {
            Ok(()) => Status::Complete,
            Err(error) => Status::Failed(error),
        };
        Ok(())
    }

    pub(crate) fn reloaded(&mut self, id: u64, result: Result<ReloadOutcome>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Reload || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        record.status = match result {
            Ok(ReloadOutcome::Complete) => Status::Complete,
            Ok(ReloadOutcome::Cancelled) => Status::Cancelled,
            Err(error) => Status::Failed(error),
        };
        Ok(())
    }

    pub(crate) fn begin_save(&mut self, tab: TabId, revision: u64, save_as: bool) -> Result<u64> {
        self.reserve(
            if save_as { Kind::SaveAs } else { Kind::Save },
            tab,
            revision,
        )
    }

    fn reserve(&mut self, kind: Kind, tab: TabId, revision: u64) -> Result<u64> {
        let id = self.last.checked_add(1).ok_or(Error::Exhausted)?;
        if self.records.len() == RECORDS {
            let terminal = self
                .records
                .iter()
                .position(|job| job.status != Status::Pending)
                .ok_or(Error::Limit)?;
            self.records.remove(terminal).ok_or(Error::Protocol)?;
        }
        self.records.push_back(Record {
            id,
            kind,
            tab,
            revision,
            scan: None,
            status: Status::Pending,
        });
        self.last = id;
        Ok(id)
    }

    pub(crate) fn started(&mut self, id: u64, result: Result<Option<u64>>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Spelling
            || record.status != Status::Pending
            || record.scan.is_some()
        {
            return Err(Error::InvalidArgument);
        }
        match result {
            Ok(Some(0)) => record.status = Status::Failed(Error::Protocol),
            Ok(Some(scan)) => record.scan = Some(scan),
            Ok(None) => record.status = Status::Failed(Error::Unavailable),
            Err(error) => record.status = Status::Failed(error),
        }
        Ok(())
    }

    pub(crate) fn opened(&mut self, id: u64, result: Result<crate::dialog::Target>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if record.kind != Kind::Open || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        match result {
            Ok(target) if target.tab != 0 => {
                record.tab = target.tab;
                record.revision = target.revision;
                record.status = Status::Complete;
            }
            Ok(_) => record.status = Status::Failed(Error::Protocol),
            Err(error) => record.status = Status::Failed(error),
        }
        Ok(())
    }

    pub(crate) fn saved(&mut self, id: u64, result: Result<()>) -> Result<()> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(Error::InvalidArgument)?;
        if !matches!(record.kind, Kind::Save | Kind::SaveAs) || record.status != Status::Pending {
            return Err(Error::InvalidArgument);
        }
        record.status = match result {
            Ok(()) => Status::Complete,
            Err(error) => Status::Failed(error),
        };
        Ok(())
    }

    /// Terminal outcomes are historical facts, not promises of current marks.
    pub(crate) fn observe(&mut self, editor: &Editor, spelling: &WindowState) -> bool {
        let mut changed = false;
        for record in &mut self.records {
            if record.kind != Kind::Spelling || record.status != Status::Pending {
                continue;
            }
            let Some(scan) = record.scan else {
                continue;
            };
            let status = match spelling.snapshot(editor, record.tab, record.revision) {
                Ok(snapshot) if snapshot.scan != scan => Status::Cancelled,
                Ok(snapshot) => match snapshot.status {
                    ScanStatus::Checking => Status::Pending,
                    ScanStatus::Complete => Status::Complete,
                    ScanStatus::NoDictionary | ScanStatus::NotChecked => Status::Cancelled,
                },
                Err(error) => Status::Failed(error),
            };
            changed |= record.status != status;
            record.status = status;
        }
        changed
    }

    pub(crate) fn fields(&self) -> Result<String> {
        let mut fields = format!("job-last={}", self.last);
        fields.reserve(self.records.len() * 128);
        for record in &self.records {
            let (status, error) = match record.status {
                Status::Pending => ("pending", "-"),
                Status::Complete => ("complete", "-"),
                Status::Cancelled => ("cancelled", "-"),
                Status::Failed(error) => ("error", error.code()),
            };
            write!(
                fields,
                "\tjob={},{},{},{},{},{status},{error}",
                record.id,
                match record.kind {
                    Kind::Spelling => "spelling",
                    Kind::Open => "open",
                    Kind::Save => "save",
                    Kind::SaveAs => "save-as",
                    Kind::Reload => "reload",
                    Kind::Dictionary => "dictionary",
                    Kind::Rename => "rename",
                    Kind::Delete => "delete",
                },
                record.tab,
                record.revision,
                record.scan.unwrap_or(0)
            )
            .map_err(|_| Error::Protocol)?;
        }
        Ok(fields)
    }

    #[cfg(test)]
    pub(crate) fn exhaust_for_test(&mut self) {
        self.last = u64::MAX;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Command, Selection};
    use crate::spelling::Dictionary;
    use crate::ui::{Controller, Event};

    #[test]
    fn dictionary_jobs_are_global_historical_and_kind_bound() {
        let mut jobs = Jobs::default();
        let id = jobs.begin_dictionary().unwrap();
        assert!(!jobs.observe(&Editor::default(), &WindowState::default()));
        assert_eq!(jobs.saved(id, Ok(())), Err(Error::InvalidArgument));
        assert_eq!(jobs.started(id, Ok(Some(1))), Err(Error::InvalidArgument));
        jobs.dictionary(id, Ok(())).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,dictionary,0,0,0,complete,-"));
        assert_eq!(
            jobs.dictionary(id, Err(Error::Unavailable)),
            Err(Error::InvalidArgument)
        );
        let failure = jobs.begin_dictionary().unwrap();
        jobs.dictionary(failure, Err(Error::Unavailable)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=2,dictionary,0,0,0,error,unavailable"));
        let save = jobs.begin_save(1, 0, false).unwrap();
        assert_eq!(jobs.dictionary(save, Ok(())), Err(Error::InvalidArgument));
    }

    #[test]
    fn reload_jobs_pin_requested_targets_and_cancellation_is_terminal() {
        let mut jobs = Jobs::default();
        let id = jobs.begin_reload(7, 9).unwrap();
        assert_eq!(jobs.saved(id, Ok(())), Err(Error::InvalidArgument));
        assert_eq!(jobs.started(id, Ok(Some(1))), Err(Error::InvalidArgument));
        assert!(!jobs.observe(&Editor::default(), &WindowState::default()));
        jobs.reloaded(id, Ok(ReloadOutcome::Cancelled)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,reload,7,9,0,cancelled,-"));
        assert_eq!(
            jobs.reloaded(id, Ok(ReloadOutcome::Complete)),
            Err(Error::InvalidArgument)
        );
        let next = jobs.begin_reload(7, 9).unwrap();
        jobs.reloaded(next, Ok(ReloadOutcome::Complete)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=2,reload,7,9,0,complete,-"));
        let error = jobs.begin_reload(7, 10).unwrap();
        jobs.reloaded(error, Err(Error::Unavailable)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=3,reload,7,10,0,error,unavailable"));
        let save = jobs.begin_save(7, 10, false).unwrap();
        assert_eq!(
            jobs.reloaded(save, Ok(ReloadOutcome::Cancelled)),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn save_jobs_keep_the_requested_revision_and_reject_cross_kind_completion() {
        let mut jobs = Jobs::default();
        let save = jobs.begin_save(3, 7, false).unwrap();
        assert_eq!(jobs.started(save, Ok(Some(1))), Err(Error::InvalidArgument));
        assert_eq!(
            jobs.opened(
                save,
                Ok(crate::dialog::Target {
                    tab: 4,
                    revision: 8
                })
            ),
            Err(Error::InvalidArgument)
        );
        assert!(!jobs.observe(&Editor::default(), &WindowState::default()));
        jobs.saved(save, Ok(())).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,save,3,7,0,complete,-"));
        assert_eq!(
            jobs.saved(save, Err(Error::StaleRevision)),
            Err(Error::InvalidArgument)
        );
        let save_as = jobs.begin_save(3, 8, true).unwrap();
        jobs.saved(save_as, Err(Error::StaleRevision)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=2,save-as,3,8,0,error,stale-revision"));
        let open = jobs.begin_open().unwrap();
        assert_eq!(jobs.saved(open, Ok(())), Err(Error::InvalidArgument));
        let spelling = jobs.begin(3, 8).unwrap();
        assert_eq!(jobs.saved(spelling, Ok(())), Err(Error::InvalidArgument));
    }

    #[test]
    fn open_outcomes_share_ids_but_cannot_be_finished_as_spelling() {
        let mut jobs = Jobs::default();
        let open = jobs.begin_open().unwrap();
        assert_eq!(jobs.started(open, Ok(Some(1))), Err(Error::InvalidArgument));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,open,0,0,0,pending,-"));
        assert!(!jobs.observe(&Editor::default(), &WindowState::default()));
        jobs.opened(
            open,
            Ok(crate::dialog::Target {
                tab: 7,
                revision: 8,
            }),
        )
        .unwrap();
        let before = jobs.fields().unwrap();
        assert!(before.contains("job=1,open,7,8,0,complete,-"));
        assert_eq!(
            jobs.opened(open, Err(Error::Unavailable)),
            Err(Error::InvalidArgument)
        );
        assert_eq!(jobs.fields().unwrap(), before);
        let spelling = jobs.begin(7, 8).unwrap();
        assert_eq!(
            jobs.opened(spelling, Err(Error::Unavailable)),
            Err(Error::InvalidArgument)
        );
        jobs.started(spelling, Err(Error::Limit)).unwrap();
        let failed = jobs.begin_open().unwrap();
        jobs.opened(failed, Err(Error::Unavailable)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=3,open,0,0,0,error,unavailable"));
        let pending = jobs.begin_open().unwrap();
        for _ in 0..65 {
            let id = jobs.begin_open().unwrap();
            jobs.opened(id, Err(Error::Unavailable)).unwrap();
        }
        assert!(jobs
            .fields()
            .unwrap()
            .contains(&format!("job={pending},open,0,0,0,pending,-")));
        jobs.exhaust_for_test();
        let before = jobs.fields().unwrap();
        assert_eq!(jobs.begin_open(), Err(Error::Exhausted));
        assert_eq!(jobs.fields().unwrap(), before);
    }

    #[test]
    fn job_outcomes_distinguish_complete_cancelled_stale_and_start_failure() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"bad")).unwrap();
        let mut spelling = WindowState::default();
        spelling.install(Dictionary::parse(b"known").unwrap());
        let mut jobs = Jobs::default();
        let first = jobs.begin(1, 0).unwrap();
        jobs.started(first, spelling.start(ui.editor(), 1, 0))
            .unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,spelling,1,0,1,pending,-"));
        spelling.step(ui.editor()).unwrap();
        assert!(jobs.observe(ui.editor(), &spelling));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,spelling,1,0,1,complete,-"));
        let second = jobs.begin(1, 0).unwrap();
        jobs.started(second, spelling.start(ui.editor(), 1, 0))
            .unwrap();
        spelling.cancel();
        assert!(jobs.observe(ui.editor(), &spelling));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=2,spelling,1,0,2,cancelled,-"));
        let third = jobs.begin(1, 0).unwrap();
        jobs.started(third, spelling.start(ui.editor(), 1, 0))
            .unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("x".into()),
        })
        .unwrap();
        assert!(jobs.observe(ui.editor(), &spelling));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=3,spelling,1,0,3,error,stale-revision"));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,spelling,1,0,1,complete,-"));
        let fourth = jobs.begin(1, 1).unwrap();
        jobs.started(fourth, Ok(None)).unwrap();
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=4,spelling,1,1,0,error,unavailable"));
        assert!(!jobs.observe(ui.editor(), &spelling));
        assert_eq!(
            jobs.started(fourth, Ok(Some(4))),
            Err(Error::InvalidArgument)
        );
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Select(Selection {
                anchor: 0,
                caret: 0,
            }),
        })
        .unwrap();
        assert!(!jobs.observe(ui.editor(), &spelling));
    }

    #[test]
    fn closing_a_pending_target_records_missing_tab() {
        let mut editor = Editor::default();
        let tab = editor.load_bytes(b"bad").unwrap();
        let mut spelling = WindowState::default();
        spelling.install(Dictionary::parse(b"known").unwrap());
        let mut jobs = Jobs::default();
        let id = jobs.begin(tab, 0).unwrap();
        jobs.started(id, spelling.start(&editor, tab, 0)).unwrap();
        editor.close_tab(tab, 0).unwrap();
        assert!(jobs.observe(&editor, &spelling));
        assert!(jobs
            .fields()
            .unwrap()
            .contains("job=1,spelling,1,0,1,error,missing-tab"));
    }

    #[test]
    fn bounded_history_preserves_pending_jobs_and_exhaustion_cannot_evict() {
        let mut jobs = Jobs::default();
        assert_eq!(jobs.fields().unwrap(), "job-last=0");
        let held = jobs.begin(1, 0).unwrap();
        jobs.started(held, Ok(Some(1))).unwrap();
        for _ in 1..RECORDS {
            let id = jobs.begin(2, 0).unwrap();
            jobs.started(id, Err(Error::Limit)).unwrap();
        }
        let next = jobs.begin(3, 0).unwrap();
        assert_eq!(next, 65);
        let fields = jobs.fields().unwrap();
        assert_eq!(fields.matches("\tjob=").count(), 64);
        assert!(fields.contains("job=1,spelling,"));
        assert!(!fields.contains("job=2,spelling,"));
        assert!(fields.contains("job=65,spelling,"));
        jobs.exhaust_for_test();
        let before = jobs.fields().unwrap();
        assert_eq!(jobs.begin(4, 0), Err(Error::Exhausted));
        assert_eq!(jobs.fields().unwrap(), before);
        let mut pending = Jobs::default();
        for _ in 0..RECORDS {
            pending.begin(1, 0).unwrap();
        }
        let before = pending.fields().unwrap();
        assert_eq!(pending.begin(1, 0), Err(Error::Limit));
        assert_eq!(pending.fields().unwrap(), before);

        let mut middle = Jobs::default();
        for _ in 0..RECORDS {
            let id = middle.begin(1, 0).unwrap();
            middle
                .started(
                    id,
                    if id == 32 {
                        Ok(Some(1))
                    } else {
                        Err(Error::Limit)
                    },
                )
                .unwrap();
        }
        for _ in 0..32 {
            let id = middle.begin(1, 0).unwrap();
            middle.started(id, Err(Error::Limit)).unwrap();
        }
        let fields = middle.fields().unwrap();
        assert!(fields.contains("job=32,spelling,1,0,1,pending,-"));
        assert!(!fields.contains("job=31,spelling,"));
        assert!(!fields.contains("job=33,spelling,"));
        assert_eq!(fields.matches("\tjob=").count(), 64);
    }
}
