//! One action worker, retained preparation, and a single bounded request/reply slot.
use crate::action_linux::{Linux, Prepared};
use crate::actions::{Command, Details, Intent, Results, Update};
use crate::budget::{Budget, Charge};
use crate::format::Text;
use std::fmt::Write;
use std::io;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Starting,
    Idle,
    Preparing(u64),
    Ready(u64),
    Sending(u64),
    Cancelling(u64),
    Disabled,
}
impl Stage {
    fn revision(self) -> Option<u64> {
        match self {
            Self::Preparing(id) | Self::Ready(id) | Self::Sending(id) | Self::Cancelling(id) => {
                Some(id)
            }
            _ => None,
        }
    }
}
fn revision(command: Command) -> u64 {
    match command {
        Command::Prepare(intent) => intent.revision,
        Command::Confirm(id) | Command::Cancel(id) => id,
    }
}
#[derive(Debug)]
struct Slot {
    stage: Stage,
    command: Option<Command>,
    update: Option<Update>,
    last: u64,
}
#[derive(Debug)]
struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
    active: AtomicU64,
    stop: AtomicBool,
    _charge: Charge,
}
#[derive(Debug)]
pub struct Worker {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}
trait Backend: Send + 'static {
    type Prepared;
    fn prepare(
        &mut self,
        intent: Intent,
        active: &AtomicU64,
    ) -> io::Result<(Self::Prepared, Details)>;
    fn deliver(&mut self, prepared: Self::Prepared, active: &AtomicU64) -> Results;
}
impl Backend for Linux {
    type Prepared = Prepared;
    fn prepare(&mut self, intent: Intent, active: &AtomicU64) -> io::Result<(Prepared, Details)> {
        Linux::prepare(self, intent, active)
    }
    fn deliver(&mut self, prepared: Prepared, active: &AtomicU64) -> Results {
        Linux::deliver(self, prepared, active)
    }
}
fn reason(error: impl std::fmt::Display) -> Text<256> {
    let mut text = Text::default();
    let _ = write!(text, "{error}");
    text
}
fn finish(shared: &Shared, id: u64, update: Update) {
    if let Ok(mut slot) = shared.slot.lock() {
        if slot.command.is_some_and(|command| revision(command) == id) {
            slot.command = None;
        }
        slot.stage = Stage::Idle;
        slot.update = Some(update);
        let _ = shared
            .active
            .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}
fn run<B: Backend>(shared: &Shared, setup: impl FnOnce() -> io::Result<B>) {
    let mut backend = match setup() {
        Ok(backend) => backend,
        Err(error) => {
            if let Ok(mut slot) = shared.slot.lock() {
                slot.stage = Stage::Disabled;
                slot.update = Some(Update::Unavailable(reason(error)));
            }
            return;
        }
    };
    if let Ok(mut slot) = shared.slot.lock() {
        slot.stage = Stage::Idle;
        slot.update = Some(Update::Available);
    } else {
        return;
    }
    let mut prepared: Option<(u64, B::Prepared)> = None;
    loop {
        let mut slot = match shared.slot.lock() {
            Ok(slot) => slot,
            Err(_) => return,
        };
        while slot.command.is_none() && !shared.stop.load(Ordering::Acquire) {
            slot = match shared.wake.wait(slot) {
                Ok(slot) => slot,
                Err(_) => return,
            };
        }
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let command = slot.command.take();
        drop(slot);
        let Some(command) = command else { continue };
        let id = revision(command);
        match command {
            Command::Prepare(intent) => {
                let result = backend.prepare(intent, &shared.active);
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                match result {
                    Ok((owner, details)) => {
                        let mut slot = match shared.slot.lock() {
                            Ok(slot) => slot,
                            Err(_) => return,
                        };
                        if shared.active.load(Ordering::Acquire) == id
                            && slot.stage == Stage::Preparing(id)
                        {
                            prepared = Some((id, owner));
                            slot.stage = Stage::Ready(id);
                            slot.update = Some(Update::Prepared(details));
                        } else {
                            drop(slot);
                            drop(owner);
                            finish(shared, id, Update::Cancelled(id));
                        }
                    }
                    Err(error) => {
                        let update = if shared.active.load(Ordering::Acquire) != id {
                            Update::Cancelled(id)
                        } else {
                            Update::Failed {
                                revision: id,
                                reason: reason(error),
                            }
                        };
                        finish(shared, id, update);
                    }
                }
            }
            Command::Confirm(_) => {
                let Some((owner_id, owner)) = prepared.take() else {
                    finish(
                        shared,
                        id,
                        Update::Failed {
                            revision: id,
                            reason: Text::new("request is no longer prepared"),
                        },
                    );
                    continue;
                };
                if owner_id != id {
                    drop(owner);
                    finish(shared, id, Update::Cancelled(id));
                    continue;
                }
                let results = backend.deliver(owner, &shared.active);
                finish(shared, id, Update::Finished(results));
            }
            Command::Cancel(_) => {
                if prepared
                    .as_ref()
                    .is_some_and(|(owner_id, _)| *owner_id == id)
                {
                    prepared = None;
                }
                finish(shared, id, Update::Cancelled(id));
            }
        }
    }
}
impl Worker {
    pub fn start(budget: &Arc<Budget>, generation: u64) -> io::Result<Self> {
        let owned = Arc::clone(budget);
        Self::start_with(budget, move || Linux::new(&owned, generation))
    }
    fn start_with<B: Backend>(
        budget: &Arc<Budget>,
        setup: impl FnOnce() -> io::Result<B> + Send + 'static,
    ) -> io::Result<Self> {
        let charge = budget
            .charge(
                std::mem::size_of::<Shared>()
                    + std::mem::size_of::<Self>()
                    + 2 * std::mem::size_of::<usize>(),
            )
            .map_err(io::Error::other)?;
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                stage: Stage::Starting,
                command: None,
                update: None,
                last: 0,
            }),
            wake: Condvar::new(),
            active: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            _charge: charge,
        });
        let work = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("td-taskmgr-actions".into())
            .spawn(move || run(&work, setup))?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }
    pub fn submit(&self, command: Command) -> io::Result<()> {
        let mut slot = self
            .shared
            .slot
            .lock()
            .map_err(|_| io::Error::other("action handoff poisoned"))?;
        let id = revision(command);
        let refuse = || io::Error::other("action request is busy, stale or unavailable");
        match command {
            Command::Prepare(_) => {
                if id == 0
                    || id <= slot.last
                    || slot.stage != Stage::Idle
                    || slot.update.is_some()
                    || slot.command.is_some()
                {
                    return Err(refuse());
                }
                slot.last = id;
                self.shared.active.store(id, Ordering::Release);
                slot.stage = Stage::Preparing(id);
            }
            Command::Confirm(_) => {
                if slot.stage != Stage::Ready(id) || slot.command.is_some() || slot.update.is_some()
                {
                    return Err(refuse());
                }
                slot.stage = Stage::Sending(id);
            }
            Command::Cancel(_) => {
                if slot.stage.revision() != Some(id) {
                    return Err(refuse());
                }
                self.shared.active.store(0, Ordering::Release);
                slot.stage = Stage::Cancelling(id);
                slot.update = None;
            }
        }
        slot.command = Some(command);
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn take(&mut self) -> io::Result<Option<Update>> {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            if let Some(handle) = self.handle.take() {
                handle
                    .join()
                    .map_err(|_| io::Error::other("process action worker failed"))?;
            }
        }
        Ok(self
            .shared
            .slot
            .lock()
            .map_err(|_| io::Error::other("action handoff poisoned"))?
            .update
            .take())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.active.store(0, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        self.handle.take();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::actions::{Delivery, Scope, Signal};
    use crate::budget::MemoryVec;
    use crate::hierarchy::ProcessKey;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    struct Fake {
        budget: Arc<Budget>,
        prepared: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        sent: mpsc::Sender<()>,
    }
    impl Backend for Fake {
        type Prepared = Intent;
        fn prepare(&mut self, intent: Intent, _: &AtomicU64) -> io::Result<(Intent, Details)> {
            self.prepared.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(3)).unwrap();
            let mut rows = MemoryVec::new(&self.budget, 1).unwrap();
            rows.push(Text::new("owned fixture")).unwrap();
            Ok((intent, Details { intent, rows }))
        }
        fn deliver(&mut self, intent: Intent, active: &AtomicU64) -> Results {
            let mut deliveries = MemoryVec::new(&self.budget, 1).unwrap();
            let result = if active.load(Ordering::Acquire) == intent.revision {
                self.sent.send(()).unwrap();
                Delivery::Sent
            } else {
                Delivery::Cancelled
            };
            deliveries.push(result).unwrap();
            Results {
                revision: intent.revision,
                deliveries,
            }
        }
    }
    struct Fixture {
        worker: Worker,
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
        sent: mpsc::Receiver<()>,
    }
    fn take(worker: &mut Worker) -> Update {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(update) = worker.take().unwrap() {
                return update;
            }
            assert!(Instant::now() < until, "worker reply deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn fixture() -> Fixture {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let (prepared, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (sent, deliveries) = mpsc::channel();
        let owned = Arc::clone(&budget);
        let mut worker = Worker::start_with(&budget, move || {
            Ok(Fake {
                budget: owned,
                prepared,
                release: wait,
                sent,
            })
        })
        .unwrap();
        assert!(matches!(take(&mut worker), Update::Available));
        Fixture {
            worker,
            entered,
            release,
            sent: deliveries,
        }
    }
    fn intent() -> Intent {
        Intent {
            revision: 1,
            key: ProcessKey {
                generation: 1,
                pid: 42,
                start_ticks: 9,
            },
            scope: Scope::Selected,
            signal: Signal::Stop,
        }
    }
    #[test]
    fn confirmation_is_bound_drained_and_delivered_once() {
        let mut f = fixture();
        assert!(f.worker.submit(Command::Confirm(1)).is_err());
        f.worker.submit(Command::Prepare(intent())).unwrap();
        f.entered.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(f.worker.submit(Command::Confirm(1)).is_err());
        assert!(f.worker.submit(Command::Prepare(intent())).is_err());
        f.release.send(()).unwrap();
        assert!(matches!(take(&mut f.worker), Update::Prepared(_)));
        assert!(f.worker.submit(Command::Confirm(2)).is_err());
        f.worker.submit(Command::Confirm(1)).unwrap();
        assert!(f.worker.submit(Command::Confirm(1)).is_err());
        let Update::Finished(results) = take(&mut f.worker) else {
            panic!("missing results")
        };
        assert_eq!(&*results.deliveries, &[Delivery::Sent]);
        f.sent.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(f.sent.try_recv().is_err());
        assert!(f.worker.submit(Command::Prepare(intent())).is_err());
    }
    #[test]
    fn cancel_during_stalled_preparation_keeps_slot_bounded_and_never_sends() {
        let mut f = fixture();
        f.worker.submit(Command::Prepare(intent())).unwrap();
        f.entered.recv_timeout(Duration::from_secs(3)).unwrap();
        f.worker.submit(Command::Cancel(1)).unwrap();
        for revision in 2..100 {
            let mut request = intent();
            request.revision = revision;
            assert!(f.worker.submit(Command::Prepare(request)).is_err());
        }
        f.release.send(()).unwrap();
        assert!(matches!(take(&mut f.worker), Update::Cancelled(1)));
        assert!(f.worker.submit(Command::Confirm(1)).is_err());
        assert!(f.sent.try_recv().is_err());
        let mut request = intent();
        request.revision = 2;
        f.worker.submit(Command::Prepare(request)).unwrap();
        f.entered.recv_timeout(Duration::from_secs(3)).unwrap();
        f.release.send(()).unwrap();
        assert!(matches!(take(&mut f.worker), Update::Prepared(_)));
        f.worker.submit(Command::Cancel(2)).unwrap();
        assert!(matches!(take(&mut f.worker), Update::Cancelled(2)));
    }
    #[test]
    fn dropping_stalled_worker_does_not_join_or_publish_authority() {
        let f = fixture();
        f.worker.submit(Command::Prepare(intent())).unwrap();
        f.entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let shared = Arc::clone(&f.worker.shared);
        let now = Instant::now();
        drop(f.worker);
        assert!(now.elapsed() < Duration::from_millis(100));
        assert_eq!(shared.active.load(Ordering::Acquire), 0);
        f.release.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while Arc::strong_count(&shared) > 1 {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(f.sent.try_recv().is_err());
    }
    struct Partial {
        budget: Arc<Budget>,
        first: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }
    impl Backend for Partial {
        type Prepared = Intent;
        fn prepare(&mut self, intent: Intent, _: &AtomicU64) -> io::Result<(Intent, Details)> {
            let mut rows = MemoryVec::new(&self.budget, 2).unwrap();
            rows.push(Text::new("first captured process")).unwrap();
            rows.push(Text::new("second captured process")).unwrap();
            Ok((intent, Details { intent, rows }))
        }
        fn deliver(&mut self, intent: Intent, active: &AtomicU64) -> Results {
            let mut deliveries = MemoryVec::new(&self.budget, 2).unwrap();
            deliveries.push(Delivery::Sent).unwrap();
            self.first.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(3)).unwrap();
            deliveries
                .push(if active.load(Ordering::Acquire) == intent.revision {
                    Delivery::Sent
                } else {
                    Delivery::Cancelled
                })
                .unwrap();
            Results {
                revision: intent.revision,
                deliveries,
            }
        }
    }
    #[test]
    fn cancelling_during_delivery_retains_the_completed_member_report() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let (first, receive) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let owned = Arc::clone(&budget);
        let mut worker = Worker::start_with(&budget, move || {
            Ok(Partial {
                budget: owned,
                first,
                resume: wait,
            })
        })
        .unwrap();
        assert!(matches!(take(&mut worker), Update::Available));
        worker.submit(Command::Prepare(intent())).unwrap();
        assert!(matches!(take(&mut worker), Update::Prepared(_)));
        worker.submit(Command::Confirm(1)).unwrap();
        receive.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.submit(Command::Cancel(1)).unwrap();
        assert!(worker.submit(Command::Confirm(1)).is_err());
        resume.send(()).unwrap();
        let Update::Finished(results) = take(&mut worker) else {
            panic!("partial results lost")
        };
        assert_eq!(&*results.deliveries, &[Delivery::Sent, Delivery::Cancelled]);
        assert!(worker.take().unwrap().is_none());
        assert_eq!(worker.shared.slot.lock().unwrap().stage, Stage::Idle);
    }
}
