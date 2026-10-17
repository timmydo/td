//! Finite per-turn protocol windows inside a retained connection budget.
use std::sync::{Arc, Mutex};

const INPUT: usize = 1024 * 1024;
const OUTPUT: usize = 64 * 1024;
const FRAMES: usize = 1088;

#[derive(Clone)]
pub(super) struct Windows(Arc<Mutex<State>>);
struct State {
    bytes: usize,
    output: usize,
    frames: usize,
    left: usize,
    phase: Phase,
    failed: bool,
}
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    First,
    Active,
    Closed,
}
impl Windows {
    pub(super) fn new(turns: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            bytes: INPUT,
            output: OUTPUT,
            frames: FRAMES,
            left: turns,
            phase: Phase::First,
            failed: false,
        })))
    }
    fn apply(&self, f: impl FnOnce(&mut State) -> Result<(), ()>) -> Result<(), ()> {
        let mut state = self.0.lock().map_err(|_| ())?;
        if state.failed {
            return Err(());
        }
        let result = f(&mut state);
        state.failed = result.is_err();
        result
    }
    pub(super) fn begin(&self) -> Result<(), ()> {
        self.apply(|state| {
            if state.phase == Phase::Active {
                return Err(());
            }
            state.left = state.left.checked_sub(1).ok_or(())?;
            if state.phase == Phase::Closed {
                state.bytes = INPUT;
                state.output = OUTPUT;
                state.frames = FRAMES;
            }
            state.phase = Phase::Active;
            Ok(())
        })
    }
    pub(super) fn finish(&self) -> Result<(), ()> {
        self.apply(|state| {
            if state.phase != Phase::Active {
                return Err(());
            }
            state.phase = Phase::Closed;
            Ok(())
        })
    }
    pub(super) fn input(&self, bytes: usize) -> Result<(), ()> {
        self.apply(|state| {
            state.bytes = state.bytes.checked_sub(bytes).ok_or(())?;
            Ok(())
        })
    }
    pub(super) fn frame(&self) -> Result<(), ()> {
        self.apply(|state| {
            state.frames = state.frames.checked_sub(1).ok_or(())?;
            Ok(())
        })
    }
    pub(super) fn admit_output(&self, bytes: usize) -> Result<(), ()> {
        self.apply(|state| {
            if bytes <= state.output {
                Ok(())
            } else {
                Err(())
            }
        })
    }
    pub(super) fn output(&self, bytes: usize) -> Result<(), ()> {
        self.apply(|state| {
            state.output = state.output.checked_sub(bytes).ok_or(())?;
            Ok(())
        })
    }
    pub(super) fn fail(&self) {
        let _ = self.apply(|_| Err(()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn authentication_and_first_turn_share_the_initial_window() {
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        windows.input(INPUT - 1).unwrap();
        windows.output(OUTPUT - 1).unwrap();
        windows.begin().unwrap();
        windows.input(1).unwrap();
        windows.output(1).unwrap();
        assert!(windows.input(1).is_err());
        assert!(windows.finish().is_err());
        assert!(windows.begin().is_err());
    }

    #[test]
    fn only_completed_turns_admit_the_next_finite_window() {
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        for _ in 0..super::super::BODY_CHECK_TURNS {
            windows.begin().unwrap();
            windows.input(INPUT).unwrap();
            windows.admit_output(OUTPUT).unwrap();
            windows.output(OUTPUT).unwrap();
            for _ in 0..FRAMES {
                windows.frame().unwrap();
            }
            windows.finish().unwrap();
        }
        assert!(windows.begin().is_err());
        assert!(windows.input(0).is_err());
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        windows.begin().unwrap();
        assert!(windows.begin().is_err());
        assert!(windows.finish().is_err());
    }

    #[test]
    fn idle_messages_consume_the_closed_window_and_failure_is_terminal() {
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        windows.begin().unwrap();
        windows.input(INPUT - 1).unwrap();
        windows.finish().unwrap();
        windows.input(1).unwrap();
        assert!(windows.input(1).is_err());
        assert!(windows.begin().is_err());
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        assert!(windows.admit_output(OUTPUT + 1).is_err());
        assert!(windows.begin().is_err());
        let windows = Windows::new(super::super::BODY_CHECK_TURNS);
        windows.fail();
        assert!(windows.begin().is_err());
    }
}
