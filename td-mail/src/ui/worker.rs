//! td-mail's page thread (README.md, "HTML mail" and "Memory"): a
//! message's pages are made, laid out again for its pictures, and its
//! pictures decoded here, one job at a time, never on the window's
//! thread, so hostile HTML or a large picture does not stop the window.
//! The window asks and goes on painting what it has; it takes the answer
//! when its loop polls (`Pending::take`). A job's payload waits in its
//! slot, not in the queue, so a job let go gives its payload up at once;
//! its cancel flag, which td-page reads between steps, stops it if it
//! runs, and its answer, should one come, is dropped here, never handed
//! over. One thread, so one more allocator arena beside the window's,
//! which the jail's rule counts.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use td_alloc::{Budget, BudgetShared};
use td_page::inline::Inline;
use td_page::pipeline::{Page, Refusal};

use super::page::{LayJob, MakeJob, Paged};
use super::pictures::Decode;

/// What the jobs' slots are charged to: a slot and its lock each, about
/// a kilobyte, one alive for each job asked and not yet run or taken.
const SLOTS: usize = 1 << 20;

/// A job queued: the thread's end of its slot.
pub enum Job {
    Make(Answering<MakeJob, Option<Paged>>),
    Lay(Answering<LayJob, Result<Page, Refusal>>),
    Decode(Answering<Decode, Option<BudgetShared<Inline>>>),
    #[cfg(test)]
    Test(Answering<tests::Probe, tests::Answer>),
}

type Slot<J, T> = BudgetShared<Shared<J, T>>;

/// One job's payload, then its answer, shared by the window and the
/// thread.
pub struct Shared<J, T> {
    cancel: AtomicBool,
    state: Mutex<State<J, T>>,
    done: Condvar,
}

enum State<J, T> {
    Queued(J),
    Running,
    Done(T),
    /// Let go, or ended without an answer (unrun, or panicked).
    Lost,
    Taken,
}

/// The thread's end of a job's slot: dropped without an answer, as when
/// the job panics or is never run, it marks the job lost, so the window
/// never waits on it.
pub struct Answering<J, T> {
    slot: Slot<J, T>,
}

/// What the window holds of a job it asked: taking its answer, or letting
/// it go, which cancels it.
pub struct Pending<J, T> {
    slot: Slot<J, T>,
}

/// A job's answer as the window finds it.
pub enum Taken<T> {
    Waiting,
    Done(T),
    Lost,
}

struct Worker {
    jobs: Mutex<Option<Sender<Job>>>,
    slots: Budget,
}

static WORKER: OnceLock<Option<Worker>> = OnceLock::new();

fn worker() -> Option<&'static Worker> {
    WORKER
        .get_or_init(|| {
            Some(Worker {
                jobs: Mutex::new(None),
                slots: Budget::new(SLOTS).ok()?,
            })
        })
        .as_ref()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Asks the thread for `job` run on `payload`. None when td-alloc refuses
/// the slot or no thread can be started: the window shows what it has, as
/// when a page is refused.
pub fn ask<J, T>(payload: J, job: fn(Answering<J, T>) -> Job) -> Option<Pending<J, T>> {
    let worker = worker()?;
    let shared = Shared {
        cancel: AtomicBool::new(false),
        state: Mutex::new(State::Queued(payload)),
        done: Condvar::new(),
    };
    let slot = BudgetShared::new(&worker.slots, shared).ok()?;
    let queued = job(Answering {
        slot: slot.try_clone().ok()?,
    });
    let mut jobs = lock(&worker.jobs);
    // A thread that is gone, as after a panic in an unwinding build, is
    // started again for the job.
    let queued = match jobs.as_ref() {
        Some(sender) => match sender.send(queued) {
            Ok(()) => return Some(Pending { slot }),
            Err(mpsc::SendError(queued)) => queued,
        },
        None => queued,
    };
    let (send, receive) = mpsc::channel();
    let name = td_alloc::string("td-mail page").ok()?;
    std::thread::Builder::new()
        .name(name)
        .spawn(move || serve(receive))
        .ok()?;
    send.send(queued).ok()?;
    *jobs = Some(send);
    Some(Pending { slot })
}

fn serve(jobs: Receiver<Job>) {
    for job in jobs {
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| run(job)));
    }
}

fn run(job: Job) {
    match job {
        Job::Make(job) => job.answer(|job, cancel| job.run(cancel)),
        Job::Lay(job) => job.answer(|job, cancel| job.run(cancel)),
        Job::Decode(job) => job.answer(|job, _| job.run()),
        #[cfg(test)]
        Job::Test(job) => job.answer(tests::Probe::run),
    }
}

impl<J, T> Answering<J, T> {
    /// Runs `work` on the payload, unless the job was let go, and answers
    /// it; cancelled meanwhile, the answer is dropped here.
    fn answer(self, work: impl FnOnce(J, &AtomicBool) -> T) {
        let slot = &self.slot;
        let payload = {
            let mut state = lock(&slot.state);
            match std::mem::replace(&mut *state, State::Running) {
                State::Queued(payload) => payload,
                other => {
                    *state = other;
                    return;
                }
            }
        };
        let value = work(payload, &slot.cancel);
        let mut state = lock(&slot.state);
        if !slot.cancel.load(Ordering::Relaxed) && matches!(*state, State::Running) {
            *state = State::Done(value);
            slot.done.notify_all();
        } else {
            drop(state);
            drop(value);
        }
    }
}

impl<J, T> Drop for Answering<J, T> {
    fn drop(&mut self) {
        let lost = {
            let mut state = lock(&self.slot.state);
            if matches!(*state, State::Queued(_) | State::Running) {
                std::mem::replace(&mut *state, State::Lost)
            } else {
                State::Lost
            }
        };
        self.slot.done.notify_all();
        drop(lost);
    }
}

impl<J, T> Pending<J, T> {
    /// The answer, once: `Waiting` until it comes.
    pub fn take(&self) -> Taken<T> {
        let mut state = lock(&self.slot.state);
        match std::mem::replace(&mut *state, State::Taken) {
            State::Done(value) => Taken::Done(value),
            State::Lost | State::Taken => Taken::Lost,
            waiting => {
                *state = waiting;
                Taken::Waiting
            }
        }
    }

    /// Waits at most `most` for the answer to come, taking nothing.
    pub fn wait_for(&self, most: Duration) {
        let until = Instant::now() + most;
        let mut state = lock(&self.slot.state);
        while matches!(*state, State::Queued(_) | State::Running) {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                return;
            };
            state = match self.slot.done.wait_timeout(state, left) {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Waits until the answer has come, or the job ended without one.
    #[cfg(test)]
    pub fn wait_ready(&self) {
        self.wait_for(Duration::from_secs(600));
    }
}

impl<J, T> Drop for Pending<J, T> {
    /// Let go: the job is cancelled, and its payload, or an answer come,
    /// dropped.
    fn drop(&mut self) {
        let state = {
            let mut state = lock(&self.slot.state);
            self.slot.cancel.store(true, Ordering::Relaxed);
            std::mem::replace(&mut *state, State::Lost)
        };
        drop(state);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    type Gate = Arc<(Mutex<bool>, Condvar)>;

    /// A test job: what it does, after the gate it waits on opens.
    pub struct Probe {
        act: fn(&AtomicBool) -> Answer,
        gate: Option<Gate>,
    }

    /// A test answer that says where it was dropped.
    pub struct Answer(u32);

    static DROPPED_ON: Mutex<Vec<(u32, Option<String>)>> = Mutex::new(Vec::new());

    impl Drop for Answer {
        fn drop(&mut self) {
            let name = std::thread::current().name().map(str::to_string);
            lock(&DROPPED_ON).push((self.0, name));
        }
    }

    impl Probe {
        pub fn run(self, cancel: &AtomicBool) -> Answer {
            if let Some(gate) = self.gate.as_ref() {
                let mut open = lock(&gate.0);
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
            }
            (self.act)(cancel)
        }
    }

    fn dropped(id: u32) -> Option<Option<String>> {
        lock(&DROPPED_ON)
            .iter()
            .find(|(had, _)| *had == id)
            .map(|(_, name)| name.clone())
    }

    fn gate() -> Gate {
        Arc::new((Mutex::new(false), Condvar::new()))
    }

    fn open(gate: &Gate) {
        *lock(&gate.0) = true;
        gate.1.notify_all();
    }

    fn probe(act: fn(&AtomicBool) -> Answer, gate: Option<Gate>) -> Pending<Probe, Answer> {
        ask(Probe { act, gate }, Job::Test).unwrap()
    }

    fn answer(pending: &Pending<Probe, Answer>) -> Option<u32> {
        match pending.take() {
            Taken::Done(answer) => Some(answer.0),
            Taken::Waiting | Taken::Lost => None,
        }
    }

    /// An answer is taken once, on the window; a job that panics is lost,
    /// and the thread goes on to the next.
    #[test]
    fn an_answer_is_taken_once_and_a_panic_is_lost() {
        let pending = probe(|_| Answer(101), None);
        pending.wait_ready();
        assert_eq!(answer(&pending), Some(101));
        assert!(matches!(pending.take(), Taken::Lost));
        let panics = probe(|_| panic!("a job that panics"), None);
        panics.wait_ready();
        assert!(matches!(panics.take(), Taken::Lost));
        let next = probe(|_| Answer(102), None);
        next.wait_ready();
        assert_eq!(answer(&next), Some(102));
    }

    /// A job let go before it runs is never run; one let go while it runs
    /// sees its flag raised, and its answer is dropped on the page thread,
    /// never handed over.
    #[test]
    fn a_job_let_go_is_not_run_or_its_answer_dropped_on_the_thread() {
        let held = gate();
        let running = probe(
            |cancel| {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                Answer(201)
            },
            Some(Arc::clone(&held)),
        );
        let queued = probe(|_| Answer(202), None);
        drop(queued);
        // The first has begun once its payload has left its slot.
        while matches!(*lock(&running.slot.state), State::Queued(_)) {
            std::thread::yield_now();
        }
        drop(running);
        open(&held);
        let after = probe(|_| Answer(203), None);
        after.wait_ready();
        assert_eq!(answer(&after), Some(203));
        assert_eq!(dropped(201), Some(Some("td-mail page".to_string())));
        assert_eq!(dropped(202), None, "never run");
    }

    /// A job's slot dropped unrun, as when the thread is gone with it
    /// queued, is lost rather than waited on for ever.
    #[test]
    fn a_job_dropped_unrun_is_lost() {
        let shared = Shared {
            cancel: AtomicBool::new(false),
            state: Mutex::new(State::Queued(Probe {
                act: |_| Answer(301),
                gate: None,
            })),
            done: Condvar::new(),
        };
        let slot = BudgetShared::new(&worker().unwrap().slots, shared).unwrap();
        let pending: Pending<Probe, Answer> = Pending {
            slot: slot.try_clone().unwrap(),
        };
        drop(Job::Test(Answering { slot }));
        pending.wait_for(Duration::from_secs(5));
        assert!(matches!(pending.take(), Taken::Lost));
    }

    /// A wait for an answer is bounded.
    #[test]
    fn a_wait_for_an_answer_is_bounded() {
        let held = gate();
        let pending = probe(|_| Answer(401), Some(Arc::clone(&held)));
        let started = Instant::now();
        pending.wait_for(Duration::from_millis(30));
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(matches!(pending.take(), Taken::Waiting));
        open(&held);
        pending.wait_ready();
        assert_eq!(answer(&pending), Some(401));
    }
}
