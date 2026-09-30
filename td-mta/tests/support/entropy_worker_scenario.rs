//! Worker startup and teardown observed by two separate allocator processes.
use std::{
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
use td_crypto::Entropy;

pub const PHASES: [&str; 7] = [
    "baseline",
    "spawned",
    "first_warm",
    "all_warm",
    "repeated",
    "joined",
    "dropped",
];
const WORKERS: usize = 8;
const ALL: usize = (1 << WORKERS) - 1;

#[derive(Default)]
struct State {
    spawned: AtomicUsize,
    start: AtomicUsize,
    warm: AtomicUsize,
    repeat: AtomicBool,
    repeated: AtomicUsize,
    exit: AtomicBool,
}

struct ExitOnDrop<'a>(&'a State);
impl Drop for ExitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.exit.store(true, Ordering::Release);
    }
}

fn wait_for(state: &State, ready: impl Fn() -> bool) -> bool {
    while !ready() {
        if state.exit.load(Ordering::Acquire) {
            return false;
        }
        thread::yield_now();
    }
    true
}

fn checkpoint(mask: &AtomicUsize, expected: usize) {
    let start = Instant::now();
    while mask.load(Ordering::Acquire) != expected {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker timed out"
        );
        thread::yield_now();
    }
}

fn worker(state: &State, bit: usize) {
    state.spawned.fetch_or(bit, Ordering::Release);
    if !wait_for(state, || state.start.load(Ordering::Acquire) & bit != 0) {
        return;
    }
    let mut entropy = td_crypto::SystemEntropy::try_new().unwrap();
    let mut bytes = [0u8; 32];
    entropy.fill(black_box(&mut bytes)).unwrap();
    black_box(&bytes);
    state.warm.fetch_or(bit, Ordering::Release);
    if !wait_for(state, || state.repeat.load(Ordering::Acquire)) {
        return;
    }
    for _ in 0..64 {
        entropy.fill(black_box(&mut bytes)).unwrap();
        black_box(&bytes);
    }
    state.repeated.fetch_or(bit, Ordering::Release);
    wait_for(state, || state.exit.load(Ordering::Acquire));
}

pub fn run(mut observe: impl FnMut()) {
    let state = State::default();
    observe();
    thread::scope(|scope| {
        // Release waiting workers even if spawning or a checkpoint panics.
        let exit = ExitOnDrop(&state);
        let mut handles: [Option<thread::ScopedJoinHandle<'_, ()>>; WORKERS] =
            std::array::from_fn(|_| None);
        for (index, handle) in handles.iter_mut().enumerate() {
            let state = &state;
            *handle = Some(
                thread::Builder::new()
                    .stack_size(240 * 1024)
                    .spawn_scoped(scope, move || worker(state, 1 << index))
                    .unwrap(),
            );
        }
        checkpoint(&state.spawned, ALL);
        observe();
        state.start.store(1, Ordering::Release);
        checkpoint(&state.warm, 1);
        observe();
        state.start.store(ALL, Ordering::Release);
        checkpoint(&state.warm, ALL);
        observe();
        state.repeat.store(true, Ordering::Release);
        checkpoint(&state.repeated, ALL);
        observe();
        drop(exit);
        // Explicit joins include the provider's thread-local destructors.
        for handle in handles.into_iter().flatten() {
            handle.join().unwrap();
        }
        observe();
    });
    observe();
}
