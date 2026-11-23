//! The outputs the daemon offers, and the one that plays.
//!
//! Every output is a sink to a controller: its own index, name, description
//! and level, which the mixer applies to the summed mix while that output is
//! the active one (APPLICATIONS.md §K.4). A player is shown none of this: one
//! sink, `td-audio`, at unity, which follows whichever output plays.
//!
//! A daemon handed one opened device and no list — every test that builds a
//! `Mixer` alone — has a single stand-in output under the player's name, so
//! a controller's view of it is the one sink it always was.

use crate::device::{Output, Playback};
use crate::mixer::{VOLUME_MAX, VOLUME_NORM};
use crate::session::{SINK_DESCRIPTION, SINK_INDEX, SINK_NAME};
use std::borrow::Cow;
use std::io;

/// What `@DEFAULT_SINK@` names: the active output.
pub const DEFAULT_SINK: &str = "@DEFAULT_SINK@";

/// One output as a controller sees it.
#[derive(Debug)]
pub struct Entry {
    /// The sink index a controller addresses it by. Fixed for the
    /// daemon's life.
    pub index: u32,
    /// The Pulse sink name, e.g. `pch-0`.
    pub name: Cow<'static, str>,
    /// The PCM's own description, sanitised when it was read.
    pub description: Cow<'static, str>,
    /// What opening it opens; `None` for the stand-in.
    pub playback: Option<Playback>,
    /// Its level, in protocol volume units, and its mute: a flag beside the
    /// level, so unmuting restores what was set.
    pub volume: u32,
    pub muted: bool,
}

/// Every output, in preference order, and which one plays.
#[derive(Debug)]
pub struct OutputSet {
    stand_in: Entry,
    discovered: Vec<Entry>,
    active: usize,
}

impl Default for OutputSet {
    fn default() -> Self {
        Self {
            stand_in: Entry {
                index: SINK_INDEX,
                name: Cow::Borrowed(SINK_NAME),
                description: Cow::Borrowed(SINK_DESCRIPTION),
                playback: None,
                volume: VOLUME_NORM,
                muted: false,
            },
            discovered: Vec::new(),
            active: 0,
        }
    }
}

impl OutputSet {
    /// The outputs discovery found, preferred first, with `active` the one
    /// opened. Each is indexed by its place, at unity and unmuted.
    pub fn discovered(outputs: Vec<Output>, active: usize) -> io::Result<Self> {
        if active >= outputs.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the active output is not one of the outputs",
            ));
        }
        let refused = |_| io::Error::from(io::ErrorKind::OutOfMemory);
        let mut discovered = crate::allocation::with_capacity(outputs.len()).map_err(refused)?;
        for (position, output) in outputs.into_iter().enumerate() {
            let index = u32::try_from(position).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "more outputs than indices")
            })?;
            let description = crate::allocation::string(&output.playback.name).map_err(refused)?;
            crate::allocation::push(
                &mut discovered,
                Entry {
                    index,
                    name: Cow::Owned(output.name),
                    description: Cow::Owned(description),
                    playback: Some(output.playback),
                    volume: VOLUME_NORM,
                    muted: false,
                },
            )
            .map_err(refused)?;
        }
        Ok(Self {
            discovered,
            active,
            ..Self::default()
        })
    }

    /// Every output, preferred first.
    pub fn entries(&self) -> &[Entry] {
        if self.discovered.is_empty() {
            std::slice::from_ref(&self.stand_in)
        } else {
            &self.discovered
        }
    }

    fn entries_mut(&mut self) -> &mut [Entry] {
        if self.discovered.is_empty() {
            std::slice::from_mut(&mut self.stand_in)
        } else {
            &mut self.discovered
        }
    }

    /// Where the active output is in `entries`.
    pub fn active_position(&self) -> usize {
        self.active
    }

    /// The output that plays.
    pub fn active(&self) -> &Entry {
        self.entries().get(self.active).unwrap_or(&self.stand_in)
    }

    /// Make the output at `position` the one that plays. Its level is the
    /// mixer's from the next mixed period.
    pub fn set_active(&mut self, position: usize) -> bool {
        if position < self.entries().len() {
            self.active = position;
            true
        } else {
            false
        }
    }

    /// The output a controller's selector names, by its place in `entries`.
    /// The forms are Pulse's: an index and no name, or the invalid index and
    /// a name, where the name is a sink's, `@DEFAULT_SINK@` or the player's
    /// `td-audio` for the active one, or a decimal index, which `pactl`
    /// sends for a numbered sink.
    pub fn resolve(&self, index: u32, name: Option<&str>) -> Option<usize> {
        match (index, name) {
            (crate::tag::INVALID_INDEX, Some(name)) => {
                if name == DEFAULT_SINK || name == SINK_NAME {
                    return Some(self.active);
                }
                if let Some(position) = self.entries().iter().position(|entry| entry.name == name) {
                    return Some(position);
                }
                // Plain decimal digits: upstream refuses a sign, and so does
                // this, where `parse` would take one.
                if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                let index = name.parse::<u32>().ok()?;
                self.by_index(index)
            }
            (crate::tag::INVALID_INDEX, None) | (_, Some(_)) => None,
            (index, None) => self.by_index(index),
        }
    }

    fn by_index(&self, index: u32) -> Option<usize> {
        self.entries().iter().position(|entry| entry.index == index)
    }

    /// Set an output's level; whether it changed.
    pub fn set_volume(&mut self, position: usize, volume: u32) -> bool {
        let Some(entry) = self.entries_mut().get_mut(position) else {
            return false;
        };
        let volume = volume.min(VOLUME_MAX);
        let changed = entry.volume != volume;
        entry.volume = volume;
        changed
    }

    /// Set an output's mute; whether it changed.
    pub fn set_muted(&mut self, position: usize, muted: bool) -> bool {
        let Some(entry) = self.entries_mut().get_mut(position) else {
            return false;
        };
        let changed = entry.muted != muted;
        entry.muted = muted;
        changed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::tag::INVALID_INDEX;

    /// An output whose PCM line names it `description`; its id differs, so
    /// the description is shown to come from the name.
    fn output(card: u32, device: u32, key: &str, name: &str, description: &str) -> Output {
        Output {
            playback: Playback {
                card,
                device,
                id: key.to_owned(),
                name: description.to_owned(),
                subdevices: 1,
                capture_subdevices: 0,
            },
            key: key.to_owned(),
            name: name.to_owned(),
        }
    }

    fn pair() -> OutputSet {
        OutputSet::discovered(
            vec![
                output(0, 0, "PCH:0", "pch-0", "ALC892 Analog"),
                output(1, 0, "Device:0", "device-0", "USB Audio"),
            ],
            0,
        )
        .unwrap()
    }

    /// Without discovery there is one output, the sink a player sees.
    #[test]
    fn the_stand_in_is_the_players_one_sink() {
        let set = OutputSet::default();
        assert_eq!(set.entries().len(), 1);
        let active = set.active();
        assert_eq!(
            (active.index, &*active.name, &*active.description),
            (SINK_INDEX, SINK_NAME, SINK_DESCRIPTION)
        );
        assert_eq!((active.volume, active.muted), (VOLUME_NORM, false));
    }

    /// Discovered outputs are indexed by place, described by their PCM,
    /// and start at unity with the named one active.
    #[test]
    fn discovered_outputs_are_indexed_and_described() {
        let set = pair();
        let shown: Vec<_> = set
            .entries()
            .iter()
            .map(|entry| (entry.index, &*entry.name, &*entry.description))
            .collect();
        assert_eq!(
            shown,
            [(0, "pch-0", "ALC892 Analog"), (1, "device-0", "USB Audio")]
        );
        assert_eq!(set.active().name, "pch-0");
        assert!(OutputSet::discovered(Vec::new(), 0).is_err());
        assert!(OutputSet::discovered(vec![output(0, 0, "PCH:0", "pch-0", "x")], 1).is_err());
    }

    /// Every selector form Pulse sends finds its output; anything else
    /// finds none.
    #[test]
    fn selectors_resolve_as_pulse_names_them() {
        let mut set = pair();
        assert_eq!(set.resolve(1, None), Some(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some("device-0")), Some(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some("1")), Some(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some("+1")), None, "a sign");
        assert_eq!(set.resolve(INVALID_INDEX, Some(" 1")), None, "a space");
        assert_eq!(set.resolve(INVALID_INDEX, Some("01")), Some(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some(DEFAULT_SINK)), Some(0));
        assert_eq!(set.resolve(INVALID_INDEX, Some(SINK_NAME)), Some(0));
        assert!(set.set_active(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some(DEFAULT_SINK)), Some(1));
        assert_eq!(set.resolve(INVALID_INDEX, Some(SINK_NAME)), Some(1));
        for (index, name) in [
            (2, None),
            (INVALID_INDEX, Some("hdmi-3")),
            (INVALID_INDEX, Some("7")),
            (INVALID_INDEX, None),
            (0, Some("pch-0")),
        ] {
            assert_eq!(set.resolve(index, name), None, "{index} {name:?}");
        }
        assert!(!set.set_active(2));
        assert_eq!(set.active_position(), 1);
    }

    /// Each output keeps its own level and mute, a write that changes
    /// nothing says so, and nothing rises above unity.
    #[test]
    fn each_output_keeps_its_own_level() {
        let mut set = pair();
        assert!(set.set_volume(1, VOLUME_NORM / 2));
        assert!(!set.set_volume(1, VOLUME_NORM / 2));
        assert!(set.set_muted(0, true));
        assert!(!set.set_muted(0, true));
        assert!(!set.set_volume(0, VOLUME_NORM * 3), "capped at unity");
        assert_eq!(
            (set.entries()[0].volume, set.entries()[0].muted),
            (VOLUME_NORM, true)
        );
        assert_eq!(
            (set.entries()[1].volume, set.entries()[1].muted),
            (VOLUME_NORM / 2, false)
        );
        assert!(!set.set_volume(5, 0));
        assert!(!set.set_muted(5, true));
    }
}
