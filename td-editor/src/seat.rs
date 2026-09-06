//! Explicit-clock held-key policy. No socket, descriptor or ambient clock.

use crate::keyboard::{Keymap, Modifiers, Stroke};
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct Input {
    pub map: Option<Keymap>,
    pub modifiers: Modifiers,
    pub focused: bool,
    pub synchronized: bool,
    down: BTreeSet<u32>,
    rate: u64,
    delay: u64,
    repeat: Option<(u32, u64)>,
}

impl Input {
    pub fn cancel_repeat(&mut self) {
        self.repeat = None;
    }

    pub fn focus(&mut self, keys: &[u32], focused: bool) -> Result<(), String> {
        if keys.len() > 768 {
            return Err("held-key budget".into());
        }
        self.cancel_repeat();
        self.down.clear();
        self.down.extend(keys.iter().copied());
        self.focused = focused;
        self.synchronized = false;
        self.modifiers = Modifiers::default();
        Ok(())
    }

    pub fn modifiers(&mut self, modifiers: Modifiers) {
        if self.modifiers != modifiers {
            self.cancel_repeat();
        }
        self.modifiers = modifiers;
        self.synchronized = true;
    }

    pub fn timing(&mut self, rate: i32, delay: i32, now: u64) -> Result<(), String> {
        if rate < 0 || delay < 0 {
            return Err("negative repeat rate or delay".into());
        }
        // At most one repeat per millisecond; excess compositor rates clamp.
        self.rate = (rate as u64).min(1000);
        self.delay = delay as u64;
        self.repeat = self.repeat.and_then(|(key, _)| {
            (self.rate != 0)
                .then(|| now.checked_add(self.delay).map(|due| (key, due)))
                .flatten()
        });
        Ok(())
    }

    pub fn key(&mut self, key: u32, pressed: bool) -> Result<Option<Stroke>, String> {
        if !pressed {
            self.down.remove(&key);
            if self.repeat.is_some_and(|(held, _)| held == key) {
                self.cancel_repeat();
            }
            return Ok(None);
        }
        if !self.focused || self.down.contains(&key) {
            return Ok(None);
        }
        if self.down.len() >= 768 {
            return Err("held-key budget".into());
        }
        self.down.insert(key);
        self.cancel_repeat();
        self.translate(key)
    }

    fn translate(&self, key: u32) -> Result<Option<Stroke>, String> {
        if !self.focused || !self.synchronized {
            return Ok(None);
        }
        self.map.as_ref().map_or(Ok(None), |map| {
            map.translate(key, self.modifiers)
                .map_err(|e| e.to_string())
        })
    }

    pub fn arm(&mut self, key: u32, now: u64) {
        if self.rate != 0 {
            self.repeat = now.checked_add(self.delay).map(|due| (key, due));
        }
    }

    pub fn repeat(&mut self, now: u64) -> Result<Option<Stroke>, String> {
        let Some((key, due)) = self.repeat.filter(|(_, due)| now >= *due) else {
            return Ok(None);
        };
        let interval = 1000u64.div_ceil(self.rate.max(1));
        // Drop missed repetitions after a stall; never burst old keystrokes.
        self.repeat = now.max(due).checked_add(interval).map(|next| (key, next));
        self.translate(key)
    }

    pub fn wait_ms(&self, now: u64) -> u64 {
        self.repeat
            .map_or(100, |(_, due)| due.saturating_sub(now).clamp(1, 100))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn input() -> Input {
        let mut input = Input {
            map: Some(Keymap::parse(include_str!("../tests/fixtures/us.xkb")).unwrap()),
            ..Input::default()
        };
        input.focus(&[], true).unwrap();
        input.modifiers(Modifiers::default());
        input.timing(25, 600, 0).unwrap();
        input
    }

    #[test]
    fn repeat_has_exact_delay_no_catchup_and_matching_release() {
        let mut input = input();
        assert_eq!(input.key(30, true).unwrap().unwrap().chord, "a");
        input.arm(30, 10);
        assert!(input.repeat(609).unwrap().is_none());
        assert_eq!(input.repeat(610).unwrap().unwrap().chord, "a");
        assert!(input.repeat(649).unwrap().is_none());
        assert!(input.repeat(5000).unwrap().is_some());
        assert!(input.repeat(5000).unwrap().is_none());
        input.key(99, false).unwrap();
        assert!(input.repeat(5040).unwrap().is_some());
        input.key(30, false).unwrap();
        assert!(input.repeat(6000).unwrap().is_none());
    }

    #[test]
    fn held_enter_keys_duplicates_and_unsynchronized_focus_do_not_type() {
        let mut input = input();
        input.focus(&[30], true).unwrap();
        assert!(input.key(30, true).unwrap().is_none());
        assert!(input.key(48, true).unwrap().is_none());
        input.modifiers(Modifiers::default());
        assert!(input.key(30, true).unwrap().is_none());
        input.key(30, false).unwrap();
        assert!(input.key(30, true).unwrap().is_some());
        input.arm(30, 0);
        input.focus(&[], false).unwrap();
        assert!(input.key(30, true).unwrap().is_none());
        assert!(input.repeat(1000).unwrap().is_none());
        assert!(input.focus(&vec![1; 769], true).is_err());
    }

    #[test]
    fn modifier_change_cancels_repeat_and_transient_refusal_keeps_map() {
        let mut input = input();
        input.key(30, true).unwrap();
        input.arm(30, 0);
        input.modifiers(Modifiers {
            depressed: 1,
            ..Modifiers::default()
        });
        assert!(input.repeat(1000).unwrap().is_none());
        assert_eq!(input.key(48, true).unwrap().unwrap().chord, "B");
        input.modifiers(Modifiers {
            group: 1,
            ..Modifiers::default()
        });
        assert!(input.key(46, true).is_err());
        assert!(input.map.is_some());
        input.modifiers(Modifiers::default());
        assert_eq!(input.key(32, true).unwrap().unwrap().chord, "d");
    }

    #[test]
    fn rate_zero_negative_and_extreme_clock_are_bounded() {
        let mut input = input();
        assert!(input.timing(-1, 0, 0).is_err());
        assert!(input.timing(1, -1, 0).is_err());
        input.key(30, true).unwrap();
        input.arm(30, 0);
        input.timing(10, 200, 100).unwrap();
        assert!(input.repeat(299).unwrap().is_none());
        assert!(input.repeat(300).unwrap().is_some());
        input.timing(0, 0, 300).unwrap();
        assert!(input.repeat(5000).unwrap().is_none());
        input.timing(i32::MAX, 0, 0).unwrap();
        input.arm(30, 0);
        assert!(input.repeat(0).unwrap().is_some());
        assert!(input.repeat(0).unwrap().is_none());
        input.arm(30, u64::MAX);
        assert!(input.repeat(u64::MAX).unwrap().is_some());
        assert!(input.repeat(u64::MAX).unwrap().is_none());
    }
}
