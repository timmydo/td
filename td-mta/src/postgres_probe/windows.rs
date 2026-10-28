//! Finite per-turn protocol windows inside a retained connection budget.
use std::sync::{Arc, Mutex};

const INPUT: usize = 1024 * 1024;
const OUTPUT: usize = 64 * 1024;
const FRAMES: usize = 1088;

#[derive(Clone)]
pub(super) struct Windows(Arc<Mutex<State>>);
struct State {
    limits: (usize, usize, usize),
    maximum: (usize, usize, usize),
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
        Self::bounded(turns, INPUT, OUTPUT, FRAMES)
    }
    pub(super) fn replica(turns: usize) -> Self {
        Self::bounded(turns, 4 * 1024 * 1024, 72 * 1024 * 1024, 4096)
    }
    pub(super) fn mutations(turns: usize) -> Self {
        Self::bounded(turns, INPUT, 128 * 1024, FRAMES)
    }
    pub(super) fn body(turns: usize) -> Self {
        Self::bounded(turns, 256 * 1024 * 1024, 512 * 1024, 8192)
    }
    pub(super) fn fanout(turns: usize, accounts: usize) -> Self {
        Self::bounded(
            turns,
            INPUT * accounts,
            40 * 1024 * 1024 * accounts,
            FRAMES * accounts,
        )
    }
    fn bounded(turns: usize, input: usize, output: usize, frames: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            limits: (input, output, frames),
            maximum: (input, output, frames),
            bytes: input,
            output,
            frames,
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
        self.begin_limits(None)
    }
    pub(super) fn begin_fanout(&self, accounts: usize) -> Result<(), ()> {
        if !(1..=crate::config::identities::MAX_ACCOUNTS).contains(&accounts) {
            return Err(());
        }
        self.begin_limits(Some((
            INPUT * accounts,
            40 * 1024 * 1024 * accounts,
            FRAMES * accounts,
        )))
    }
    fn begin_limits(&self, limits: Option<(usize, usize, usize)>) -> Result<(), ()> {
        self.apply(|state| {
            if state.phase == Phase::Active {
                return Err(());
            }
            state.left = state.left.checked_sub(1).ok_or(())?;
            if let Some(limits) = limits {
                if limits.0 > state.maximum.0
                    || limits.1 > state.maximum.1
                    || limits.2 > state.maximum.2
                {
                    return Err(());
                }
                if state.phase == Phase::First {
                    state.bytes = limits
                        .0
                        .checked_sub(state.limits.0 - state.bytes)
                        .ok_or(())?;
                    state.output = limits
                        .1
                        .checked_sub(state.limits.1 - state.output)
                        .ok_or(())?;
                    state.frames = limits
                        .2
                        .checked_sub(state.limits.2 - state.frames)
                        .ok_or(())?;
                }
                state.limits = limits;
            }
            if state.phase == Phase::Closed {
                state.bytes = state.limits.0;
                state.output = state.limits.1;
                state.frames = state.limits.2;
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
    fn replica_windows_bound_qualification_bytes_frames_and_turns() {
        let window = Windows::replica(2);
        window.input(100).unwrap();
        window.output(200).unwrap();
        window.begin().unwrap();
        window.input(4 * 1024 * 1024 - 100).unwrap();
        window.output(72 * 1024 * 1024 - 200).unwrap();
        for _ in 0..4096 {
            window.frame().unwrap();
        }
        window.finish().unwrap();
        window.begin().unwrap();
        window.input(4 * 1024 * 1024).unwrap();
        window.output(72 * 1024 * 1024).unwrap();
        window.finish().unwrap();
        assert!(window.begin().is_err());
        for (input, output, frames) in [
            (4 * 1024 * 1024 + 1, 0, 0),
            (0, 72 * 1024 * 1024 + 1, 0),
            (0, 0, 4097),
        ] {
            let window = Windows::replica(2);
            window.begin().unwrap();
            assert!(window
                .input(input)
                .and_then(|()| window.output(output))
                .and_then(|()| (0..frames).try_for_each(|_| window.frame()))
                .is_err());
            assert!(window.finish().is_err());
            assert!(window.begin().is_err());
        }
    }

    #[test]
    fn fanout_windows_charge_first_qualification_and_actual_account_count() {
        let window = Windows::fanout(3, 16);
        window.input(100).unwrap();
        window.output(200).unwrap();
        window.frame().unwrap();
        window.begin_fanout(1).unwrap();
        window.input(INPUT - 100).unwrap();
        window.output(40 * 1024 * 1024 - 200).unwrap();
        for _ in 1..FRAMES {
            window.frame().unwrap();
        }
        window.finish().unwrap();
        window.begin_fanout(16).unwrap();
        window.input(16 * INPUT).unwrap();
        window.output(16 * 40 * 1024 * 1024).unwrap();
        window.finish().unwrap();
        window.begin_fanout(1).unwrap();
        assert!(window.output(40 * 1024 * 1024 + 1).is_err());
        assert!(window.finish().is_err());
        assert!(window.begin_fanout(16).is_err());
        let window = Windows::fanout(1, 1);
        window.begin_fanout(1).unwrap();
        window.finish().unwrap();
        assert!(window.begin_fanout(1).is_err());
        let window = Windows::fanout(2, 1);
        assert!(window.begin_fanout(2).is_err());
    }

    #[test]
    fn body_turns_keep_the_original_per_job_allowances_and_finite_count() {
        let window = Windows::body(2);
        for _ in 0..2 {
            window.begin().unwrap();
            window.input(256 * 1024 * 1024).unwrap();
            window.output(512 * 1024).unwrap();
            for _ in 0..8192 {
                window.frame().unwrap();
            }
            window.finish().unwrap();
        }
        assert!(window.begin().is_err());
        for (input, output, frames) in [
            (256 * 1024 * 1024 + 1, 0, 0),
            (0, 512 * 1024 + 1, 0),
            (0, 0, 8193),
        ] {
            let window = Windows::body(2);
            window.begin().unwrap();
            let accepted = window
                .input(input)
                .and_then(|()| window.output(output))
                .and_then(|()| (0..frames).try_for_each(|_| window.frame()));
            assert!(accepted.is_err());
            assert!(window.finish().is_err());
            assert!(window.begin().is_err());
        }
    }

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
