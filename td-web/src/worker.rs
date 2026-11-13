//! Bounded render handoff. Four reusable request/result slots are admitted
//! once at startup. Payloads own nested storage separately. Thread creation
//! remains std/OS machinery; ordinary handoffs allocate no backing storage.

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use td_alloc::{Budget, BudgetError, BudgetShared};

pub(crate) const QUEUED: usize = 4;

enum Phase<I, O> {
    Free,
    Queued(I),
    Running,
    Done(Option<O>),
}
struct Slot<I, O> {
    phase: Phase<I, O>,
    abandoned: bool,
    waiting: bool,
}
struct State<I, O> {
    slots: [Slot<I, O>; QUEUED],
    queue: [usize; QUEUED],
    head: usize,
    len: usize,
    closed: bool,
    worker_waiting: bool,
    producers_waiting: usize,
    #[cfg(test)]
    active: bool,
}
struct Mailbox<I, O> {
    state: Mutex<State<I, O>>,
    work: Condvar,
    space: Condvar,
    results: [Condvar; QUEUED],
}

impl<I, O> Mailbox<I, O> {
    fn space<'a>(&self, mut state: MutexGuard<'a, State<I, O>>) -> MutexGuard<'a, State<I, O>> {
        state.producers_waiting += 1;
        state = self
            .space
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner);
        state.producers_waiting -= 1;
        state
    }

    fn finish(&self, at: usize, output: Option<O>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        {
            state.active = false;
        }
        let Some(slot) = state.slots.get_mut(at) else {
            return;
        };
        let waiting = slot.waiting;
        // An abandoned result is destroyed on this worker before it takes
        // another request. The local output keeps its own admission until then.
        let discarded = if slot.abandoned {
            slot.phase = Phase::Free;
            output
        } else {
            slot.phase = Phase::Done(output);
            None
        };
        let producers = state.producers_waiting != 0;
        drop(state);
        drop(discarded);
        if waiting {
            if let Some(result) = self.results.get(at) {
                result.notify_one();
            }
        }
        if producers {
            self.space.notify_all();
        }
    }

    fn close(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.closed = true;
            state.len = 0;
        }
        self.work.notify_all();
        self.space.notify_all();
        for at in 0..QUEUED {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(slot) = state.slots.get_mut(at) else {
                continue;
            };
            if !matches!(slot.phase, Phase::Queued(_)) {
                continue;
            }
            let waiting = slot.waiting;
            let replacement = if slot.abandoned {
                Phase::Free
            } else {
                Phase::Done(None)
            };
            let input = std::mem::replace(&mut slot.phase, replacement);
            drop(state);
            drop(input);
            if waiting {
                if let Some(result) = self.results.get(at) {
                    result.notify_one();
                }
            }
        }
    }
}

/// One result owner. Dropping it abandons only its own request/result;
/// a queued/running slot cannot be reused before the worker releases it.
pub(crate) struct Ticket<I, O> {
    mailbox: BudgetShared<Mailbox<I, O>>,
    at: usize,
    taken: bool,
}
impl<I, O> Ticket<I, O> {
    pub(crate) fn wait(mut self) -> Option<O> {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            let slot = state.slots.get_mut(self.at)?;
            if matches!(slot.phase, Phase::Done(_)) {
                let Phase::Done(output) = std::mem::replace(&mut slot.phase, Phase::Free) else {
                    return None;
                };
                self.taken = true;
                let producers = state.producers_waiting != 0;
                drop(state);
                if producers {
                    self.mailbox.space.notify_all();
                }
                return output;
            }
            slot.waiting = true;
            let result = self.mailbox.results.get(self.at)?;
            state = result.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }
}
impl<I, O> Drop for Ticket<I, O> {
    fn drop(&mut self) {
        if self.taken {
            return;
        }
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(slot) = state.slots.get_mut(self.at) else {
            return;
        };
        slot.waiting = false;
        let discarded = if matches!(slot.phase, Phase::Done(_)) {
            Some(std::mem::replace(&mut slot.phase, Phase::Free))
        } else {
            slot.abandoned = true;
            None
        };
        let producers = state.producers_waiting != 0;
        drop(state);
        drop(discarded);
        if producers {
            self.mailbox.space.notify_all();
        }
    }
}

// Even an unexpected unwind outside the rendering's catch completes its
// active ticket; the outer exit guard cancels queued tickets too.
struct Active<'a, I, O> {
    mailbox: &'a Mailbox<I, O>,
    at: usize,
    finished: bool,
}
impl<I, O> Active<'_, I, O> {
    fn finish(mut self, output: Option<O>) {
        self.finished = true;
        self.mailbox.finish(self.at, output);
    }
}
impl<I, O> Drop for Active<'_, I, O> {
    fn drop(&mut self) {
        if !self.finished {
            self.mailbox.finish(self.at, None);
        }
    }
}
struct Exit<I, O>(BudgetShared<Mailbox<I, O>>);
impl<I, O> Drop for Exit<I, O> {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Debug)]
pub(crate) enum SubmitError<I> {
    Closed(I),
    Memory(BudgetError, I),
}

/// One worker; drop closes its queue and joins its bounded active operation.
/// Four slots include queued, running and completed-but-unclaimed results.
pub(crate) struct Worker<I, O> {
    mailbox: BudgetShared<Mailbox<I, O>>,
    thread: Option<JoinHandle<()>>,
}
impl<I: Send + 'static, O: Send + 'static> Worker<I, O> {
    pub(crate) fn start(memory: &Budget, process: fn(I, &Budget) -> O) -> Result<Self, ()> {
        let mailbox = BudgetShared::new(
            memory,
            Mailbox {
                state: Mutex::new(State {
                    slots: std::array::from_fn(|_| Slot {
                        phase: Phase::Free,
                        abandoned: false,
                        waiting: false,
                    }),
                    queue: [0; QUEUED],
                    head: 0,
                    len: 0,
                    closed: false,
                    worker_waiting: false,
                    producers_waiting: 0,
                    #[cfg(test)]
                    active: false,
                }),
                work: Condvar::new(),
                space: Condvar::new(),
                results: std::array::from_fn(|_| Condvar::new()),
            },
        )
        .map_err(|_| ())?;
        let exit = Exit(mailbox.try_clone().map_err(|_| ())?);
        let name = td_alloc::string("td-web render").map_err(|_| ())?;
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || loop {
                let (at, input, abandoned) = {
                    let mut state = exit.0.state.lock().unwrap_or_else(PoisonError::into_inner);
                    while state.len == 0 && !state.closed {
                        state.worker_waiting = true;
                        state = exit
                            .0
                            .work
                            .wait(state)
                            .unwrap_or_else(PoisonError::into_inner);
                        state.worker_waiting = false;
                    }
                    if state.closed {
                        return;
                    }
                    let Some(&at) = state.queue.get(state.head) else {
                        return;
                    };
                    state.head = (state.head + 1) % QUEUED;
                    state.len -= 1;
                    #[cfg(test)]
                    {
                        state.active = true;
                    }
                    let Some(slot) = state.slots.get_mut(at) else {
                        return;
                    };
                    let input = std::mem::replace(&mut slot.phase, Phase::Running);
                    (at, input, slot.abandoned)
                };
                let active = Active {
                    mailbox: &exit.0,
                    at,
                    finished: false,
                };
                let output = match input {
                    Phase::Queued(input) if !abandoned => {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            process(input, exit.0.budget())
                        }))
                        .ok()
                    }
                    _ => None,
                };
                active.finish(output);
            })
            .map_err(|_| ())?;
        Ok(Self {
            mailbox,
            thread: Some(thread),
        })
    }

    /// Wait for a free slot; closed/refcount refusal returns the untouched
    /// input. Replacing a dead worker can retry it without another copy.
    pub(crate) fn submit(&self, input: I) -> Result<Ticket<I, O>, SubmitError<I>> {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let at = loop {
            if state.closed {
                return Err(SubmitError::Closed(input));
            }
            if let Some(at) = state
                .slots
                .iter()
                .position(|slot| matches!(slot.phase, Phase::Free))
            {
                break at;
            }
            state = self.mailbox.space(state);
        };
        let mailbox = match self.mailbox.try_clone() {
            Ok(mailbox) => mailbox,
            Err(error) => return Err(SubmitError::Memory(error, input)),
        };
        let tail = (state.head + state.len) % QUEUED;
        let Some(queued) = state.queue.get_mut(tail) else {
            return Err(SubmitError::Memory(BudgetError::Limit, input));
        };
        *queued = at;
        let Some(slot) = state.slots.get_mut(at) else {
            return Err(SubmitError::Memory(BudgetError::Limit, input));
        };
        *slot = Slot {
            phase: Phase::Queued(input),
            abandoned: false,
            waiting: false,
        };
        state.len += 1;
        let waiting = state.worker_waiting;
        drop(state);
        if waiting {
            self.mailbox.work.notify_one();
        }
        Ok(Ticket {
            mailbox,
            at,
            taken: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn idle(&self) {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !state.closed && (state.active || state.len != 0) {
            state = self.mailbox.space(state);
        }
    }
    #[cfg(test)]
    pub(crate) fn stop(&mut self) {
        self.mailbox.close();
        if let Some(thread) = self.thread.take() {
            let _ = JoinHandle::join(thread);
        }
    }
}
impl<I, O> Drop for Worker<I, O> {
    fn drop(&mut self) {
        self.mailbox.close();
        if let Some(thread) = self.thread.take() {
            let _ = JoinHandle::join(thread);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    fn memory() -> Budget {
        Budget::new(1 << 20).unwrap()
    }

    #[test]
    fn startup_refusals_return_credit_and_handoffs_reuse_admission() {
        let root = memory();
        let baseline = root.used();
        for allowance in 0..2 {
            assert!(
                td_alloc::with_reservation_limit(allowance, || Worker::start(
                    &root,
                    |x: u8, _: &Budget| x
                ))
                .is_err()
            );
            assert_eq!(root.used(), baseline);
        }
        let worker = Worker::start(&root, |x: u8, _: &Budget| x).unwrap();
        let idle = root.used();
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        for n in 0..100 {
            let ticket = td_alloc::with_reservation_limit(0, || worker.submit(n)).unwrap();
            assert_eq!(
                td_alloc::with_reservation_limit(0, || ticket.wait()),
                Some(n)
            );
        }
        drop(blocker);
        assert_eq!(root.used(), idle);
        drop(worker);
        assert_eq!(root.used(), baseline);
    }

    #[test]
    fn all_calls_use_one_thread_and_a_panic_does_not_replace_it() {
        let root = memory();
        let worker = Worker::start(&root, |panic: bool, _: &Budget| {
            assert!(!panic, "a rendering panics");
            std::thread::current().id()
        })
        .unwrap();
        let id = worker.submit(false).unwrap().wait().unwrap();
        assert_ne!(id, std::thread::current().id());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..30 {
                        assert_eq!(worker.submit(false).unwrap().wait(), Some(id));
                    }
                });
            }
        });
        assert_eq!(worker.submit(true).unwrap().wait(), None);
        assert_eq!(worker.submit(false).unwrap().wait(), Some(id));
        drop(worker);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    fn held((value, gate): (u8, Arc<Barrier>), _: &Budget) -> u8 {
        if value == 0 {
            gate.wait();
            gate.wait();
        }
        value
    }

    #[test]
    fn all_slots_include_active_and_completed_results_and_apply_backpressure() {
        let root = memory();
        let worker = Worker::start(&root, held).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let active = worker.submit((0, gate.clone())).unwrap();
        gate.wait();
        let queued: Vec<_> = (1..QUEUED)
            .map(|n| worker.submit((n as u8, gate.clone())).unwrap())
            .collect();
        let (send, recv) = mpsc::channel();
        std::thread::scope(|scope| {
            let submit = scope.spawn(|| {
                let ticket = worker.submit((9, gate.clone())).unwrap();
                send.send(()).unwrap();
                ticket.wait()
            });
            let was_blocked = recv.recv_timeout(Duration::from_millis(30));
            gate.wait();
            worker.idle();
            // Completed answers still hold every slot until a caller takes one.
            let still_blocked = recv.recv_timeout(Duration::from_millis(30));
            assert_eq!(active.wait(), Some(0));
            assert_eq!(was_blocked, Err(mpsc::RecvTimeoutError::Timeout));
            assert_eq!(still_blocked, Err(mpsc::RecvTimeoutError::Timeout));
            recv.recv_timeout(Duration::from_secs(2)).unwrap();
            for (n, ticket) in queued.into_iter().enumerate() {
                assert_eq!(ticket.wait(), Some((n + 1) as u8));
            }
            assert_eq!(submit.join().unwrap(), Some(9));
        });
        drop(worker);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn closure_wakes_submitters_and_cancels_only_queued_calls() {
        let root = memory();
        let worker = Worker::start(&root, held).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let active = worker.submit((0, gate.clone())).unwrap();
        gate.wait();
        let queued: Vec<_> = (1..QUEUED)
            .map(|n| worker.submit((n as u8, gate.clone())).unwrap())
            .collect();
        std::thread::scope(|scope| {
            let submit = scope.spawn(|| worker.submit((9, gate.clone())));
            worker.mailbox.close();
            let refused = submit.join().unwrap();
            let cancelled: Vec<_> = queued.into_iter().map(Ticket::wait).collect();
            gate.wait();
            assert!(matches!(refused, Err(SubmitError::Closed((9, _)))));
            assert_eq!(cancelled, vec![None; QUEUED - 1]);
            assert_eq!(active.wait(), Some(0));
        });
        drop(worker);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn unexpected_worker_exit_completes_active_and_queued_tickets() {
        struct BadPayload;
        impl Drop for BadPayload {
            fn drop(&mut self) {
                panic!("panic payload destructor");
            }
        }
        let root = memory();
        let mut worker = Worker::start(&root, |(n, gate): (u8, Arc<Barrier>), _: &Budget| -> u8 {
            if n == 0 {
                gate.wait();
                gate.wait();
                std::panic::panic_any(BadPayload);
            }
            n
        })
        .unwrap();
        let gate = Arc::new(Barrier::new(2));
        let active = worker.submit((0, gate.clone())).unwrap();
        gate.wait();
        let queued = worker.submit((1, gate.clone())).unwrap();
        gate.wait();
        assert_eq!(active.wait(), None);
        assert_eq!(queued.wait(), None);
        assert!(worker.thread.take().unwrap().join().is_err());
        assert!(matches!(
            worker.submit((2, gate)),
            Err(SubmitError::Closed((2, _)))
        ));
        drop(worker);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn abandoned_queued_running_and_completed_slots_are_reusable() {
        let root = memory();
        let worker = Worker::start(&root, held).unwrap();
        let gate = Arc::new(Barrier::new(2));
        for _ in 0..20 {
            let active = worker.submit((0, gate.clone())).unwrap();
            gate.wait();
            let queued = worker.submit((1, gate.clone())).unwrap();
            drop((active, queued));
            gate.wait();
            worker.idle();
            let done = worker.submit((2, gate.clone())).unwrap();
            worker.idle();
            drop(done);
            assert_eq!(worker.submit((3, gate.clone())).unwrap().wait(), Some(3));
        }
        drop(worker);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }

    #[test]
    fn ticket_and_output_keep_their_respective_owners_after_worker_drop() {
        let root = memory();
        let worker =
            Worker::start(&root, |bytes: td_alloc::BudgetVec<u8>, _: &Budget| bytes).unwrap();
        let bytes = td_alloc::BudgetVec::filled(&root, 4096, 7).unwrap();
        let ticket = worker.submit(bytes).unwrap();
        worker.idle();
        drop(worker);
        let retained = root.used();
        assert!(retained > Budget::metadata_bytes() + 4096);
        let output = ticket.wait().unwrap();
        assert_eq!(root.used(), Budget::metadata_bytes() + output.capacity());
        drop(output);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }
}
