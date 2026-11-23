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
//!
//! Outputs come and go while the daemon runs (§K.4, "Hotplug"): a rescan
//! adds each new one under an index never used before and drops each one
//! gone, remembering its level by key against its return. The human's
//! choices are kept as a preference list of keys, most recent first, which
//! orders a fallback and brings a chosen output back when it reappears.

use crate::device::{Output, Playback};
use crate::mixer::{VOLUME_MAX, VOLUME_NORM};
use crate::session::{SINK_DESCRIPTION, SINK_INDEX, SINK_NAME};
use std::borrow::Cow;
use std::io;

/// What `@DEFAULT_SINK@` names: the active output.
pub const DEFAULT_SINK: &str = "@DEFAULT_SINK@";

/// The most outputs offered, and the most keys remembered: the seat's
/// playback-node cap.
pub const MAX_OUTPUTS: usize = 64;

/// One output as a controller sees it.
#[derive(Debug)]
pub struct Entry {
    /// The sink index a controller addresses it by. Never another
    /// output's in the daemon's life.
    pub index: u32,
    /// The Pulse sink name, e.g. `pch-0`.
    pub name: Cow<'static, str>,
    /// What follows the card across plugs, e.g. `PCH:0`; empty for the
    /// stand-in.
    pub key: Cow<'static, str>,
    /// The PCM's own description, sanitised when it was read.
    pub description: Cow<'static, str>,
    /// What opening it opens; `None` for the stand-in.
    pub playback: Option<Playback>,
    /// Its level, in protocol volume units, and its mute: a flag beside the
    /// level, so unmuting restores what was set.
    pub volume: u32,
    pub muted: bool,
}

/// A departed output's level, kept by key against its return.
#[derive(Debug)]
struct Remembered {
    key: String,
    volume: u32,
    muted: bool,
}

/// What a rescan changed.
#[derive(Debug, Default)]
pub struct Rescan {
    /// The outputs gone, out of the set.
    pub removed: Vec<Entry>,
    /// The indices of the outputs new to the set, in its order.
    pub added: Vec<u32>,
    /// The active output is no longer listed. It stays in the set until
    /// the caller has moved off it and removes it.
    pub active_gone: bool,
    /// Outputs listed but not offered: past `MAX_OUTPUTS`, out of indices,
    /// or under a name another output holds.
    pub refused: usize,
}

/// Every output, in discovery order, which one plays, and what the human
/// chose.
#[derive(Debug)]
pub struct OutputSet {
    stand_in: Entry,
    discovered: Vec<Entry>,
    active: usize,
    /// The index the next new output takes; none is used twice.
    next_index: u32,
    /// Keys the human chose, most recent first.
    preference: Vec<String>,
    remembered: Vec<Remembered>,
}

impl Default for OutputSet {
    fn default() -> Self {
        Self {
            stand_in: Entry {
                index: SINK_INDEX,
                name: Cow::Borrowed(SINK_NAME),
                key: Cow::Borrowed(""),
                description: Cow::Borrowed(SINK_DESCRIPTION),
                playback: None,
                volume: VOLUME_NORM,
                muted: false,
            },
            discovered: Vec::new(),
            active: 0,
            next_index: 0,
            preference: Vec::new(),
            remembered: Vec::new(),
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
        // Room for every output a rescan may add, so adding never grows it.
        let mut discovered =
            crate::allocation::with_capacity(outputs.len().max(MAX_OUTPUTS)).map_err(refused)?;
        let next_index = u32::try_from(outputs.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "more outputs than indices")
        })?;
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
                    key: Cow::Owned(output.key),
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
            next_index,
            preference: crate::allocation::with_capacity(MAX_OUTPUTS).map_err(refused)?,
            remembered: crate::allocation::with_capacity(MAX_OUTPUTS).map_err(refused)?,
            ..Self::default()
        })
    }

    /// Take a rescan's outputs: drop each output no longer listed but the
    /// active one, remembering its level, and add each newly listed one
    /// under the next index, at the level it had when it left. A daemon
    /// with the stand-in alone has no discovery to follow and changes
    /// nothing.
    pub fn rescan(&mut self, found: Vec<Output>) -> Result<Rescan, crate::allocation::Error> {
        let mut change = Rescan {
            removed: crate::allocation::with_capacity(self.discovered.len())?,
            added: crate::allocation::with_capacity(found.len())?,
            ..Rescan::default()
        };
        if self.discovered.is_empty() {
            return Ok(change);
        }
        let listed = |entry: &Entry| found.iter().any(|output| same(entry, output));
        change.active_gone = !self.discovered.get(self.active).is_some_and(listed);
        let mut position = self.discovered.len();
        while position > 0 {
            position -= 1;
            if position != self.active && !self.discovered.get(position).is_some_and(listed) {
                if let Some(entry) = self.remove(position) {
                    crate::allocation::push(&mut change.removed, entry)?;
                }
            }
        }
        // The playing output gone, its card may be back under another
        // number in this same table: that output takes its name and level.
        let heir = change.active_gone.then_some(self.active);
        for output in found {
            if self.discovered.iter().any(|entry| same(entry, &output)) {
                continue;
            }
            match self.add(output, heir) {
                Some(index) => crate::allocation::push(&mut change.added, index)?,
                None => change.refused += 1,
            }
        }
        Ok(change)
    }

    /// A new output at the end of the set, or `None` when it cannot be one.
    /// `gone` is the place of a playing output the table no longer lists:
    /// its name is free, and its level is the one an output under its key
    /// returns at.
    fn add(&mut self, output: Output, gone: Option<usize>) -> Option<u32> {
        if self.discovered.len() >= MAX_OUTPUTS {
            return None;
        }
        let taken = |name: &str| {
            self.discovered
                .iter()
                .enumerate()
                .any(|(position, entry)| Some(position) != gone && entry.name == name)
        };
        let name = if taken(&output.name) {
            // As discovery names a second card whose name folds alike.
            crate::allocation::format(format_args!("{}.{}", output.name, output.playback.card))
                .ok()
                .filter(|name| !taken(name))?
        } else {
            output.name
        };
        let description = crate::allocation::string(&output.playback.name).ok()?;
        let index = self.next_index;
        self.next_index = index.checked_add(1)?;
        let heir = gone
            .and_then(|position| self.discovered.get(position))
            .filter(|entry| entry.key == output.key.as_str())
            .map(|entry| (entry.volume, entry.muted));
        let remembered = self
            .remembered
            .iter()
            .position(|known| known.key == output.key);
        let (volume, muted) = match (heir, remembered) {
            (Some(level), _) => level,
            (None, Some(at)) => {
                let known = self.remembered.remove(at);
                (known.volume, known.muted)
            }
            (None, None) => (VOLUME_NORM, false),
        };
        crate::allocation::push(
            &mut self.discovered,
            Entry {
                index,
                name: Cow::Owned(name),
                key: Cow::Owned(output.key),
                description: Cow::Owned(description),
                playback: Some(output.playback),
                volume,
                muted,
            },
        )
        .ok()?;
        Some(index)
    }

    /// Take the output at `position` out of the set, remembering its level
    /// by key; never the active one, which must be moved off first.
    pub fn remove(&mut self, position: usize) -> Option<Entry> {
        if position == self.active || position >= self.discovered.len() {
            return None;
        }
        let entry = self.discovered.remove(position);
        if position < self.active {
            self.active -= 1;
        }
        match self
            .remembered
            .iter_mut()
            .find(|known| known.key == *entry.key)
        {
            Some(known) => (known.volume, known.muted) = (entry.volume, entry.muted),
            None => {
                if let Ok(key) = crate::allocation::string(&entry.key) {
                    if self.remembered.len() >= MAX_OUTPUTS {
                        self.remembered.remove(0);
                    }
                    let _ = crate::allocation::push(
                        &mut self.remembered,
                        Remembered {
                            key,
                            volume: entry.volume,
                            muted: entry.muted,
                        },
                    );
                }
            }
        }
        Some(entry)
    }

    /// The human chose the output at `position`: its key leads the
    /// preference list.
    pub fn chosen(&mut self, position: usize) -> Result<(), crate::allocation::Error> {
        let Some(entry) = self.discovered.get(position) else {
            return Ok(());
        };
        if let Some(at) = self.preference.iter().position(|key| *key == *entry.key) {
            if let Some(earlier) = self.preference.get_mut(..=at) {
                earlier.rotate_right(1);
            }
            return Ok(());
        }
        let key = crate::allocation::string(&entry.key)?;
        if self.preference.len() >= MAX_OUTPUTS {
            self.preference.pop();
        }
        crate::allocation::push(&mut self.preference, key)?;
        self.preference.rotate_right(1);
        Ok(())
    }

    /// The `n`th output to fall back to: those the human chose, most recent
    /// first, then the rest in discovery order.
    pub fn preferred(&self, n: usize) -> Option<usize> {
        let rank = |position: usize, entry: &Entry| {
            let chosen = self.preference.iter().position(|key| *key == *entry.key);
            (chosen.unwrap_or(MAX_OUTPUTS), position)
        };
        let entries = self.entries();
        entries
            .iter()
            .enumerate()
            .find(|(position, entry)| {
                let own = rank(*position, entry);
                let before = entries
                    .iter()
                    .enumerate()
                    .filter(|(other, entry)| rank(*other, entry) < own)
                    .count();
                before == n
            })
            .map(|(position, _)| position)
    }

    /// The output to bring back: going down the human's choices, the
    /// first present output that is `fresh` (newly listed, or awaiting a
    /// retry), unless one chosen more recently plays. A chosen output that
    /// is listed but neither, such as one whose device failed, is passed
    /// over.
    pub fn returning(&self, fresh: impl Fn(u32) -> bool) -> Option<usize> {
        for key in &self.preference {
            let Some(position) = self
                .discovered
                .iter()
                .position(|entry| entry.key == key.as_str())
            else {
                continue;
            };
            if position == self.active {
                return None;
            }
            if self
                .discovered
                .get(position)
                .is_some_and(|entry| fresh(entry.index))
            {
                return Some(position);
            }
        }
        None
    }

    /// Where the output with sink index `index` is.
    pub fn position_of(&self, index: u32) -> Option<usize> {
        self.by_index(index)
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

/// Whether a listed output is this entry: the same key on the same PCM. A
/// card plugged back in under another number is a new output.
fn same(entry: &Entry, output: &Output) -> bool {
    entry.key == output.key.as_str()
        && entry.playback.as_ref().is_some_and(|playback| {
            (playback.card, playback.device) == (output.playback.card, output.playback.device)
        })
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

    fn levels(set: &OutputSet) -> Vec<(u32, &str, u32, bool)> {
        set.entries()
            .iter()
            .map(|entry| (entry.index, &*entry.name, entry.volume, entry.muted))
            .collect()
    }

    /// A rescan drops each output gone but the active one, adds each new
    /// one under an index never used before, and gives a returning output
    /// the level it left with.
    #[test]
    fn a_rescan_follows_outputs_and_remembers_their_levels() {
        let mut set = pair();
        let pch = || output(0, 0, "PCH:0", "pch-0", "ALC892 Analog");
        let usb = |card| output(card, 0, "Device:0", "device-0", "USB Audio");
        assert!(set.set_volume(1, VOLUME_NORM / 2));
        assert!(set.set_muted(1, true));
        let change = set.rescan(vec![pch()]).unwrap();
        let removed: Vec<_> = change.removed.iter().map(|entry| entry.index).collect();
        assert_eq!(
            (removed, change.added, change.active_gone),
            (vec![1], vec![], false)
        );
        assert_eq!(levels(&set), [(0, "pch-0", VOLUME_NORM, false)]);
        // Back under another card number: the same key, a new index, the
        // level it left with.
        let change = set.rescan(vec![pch(), usb(3)]).unwrap();
        assert_eq!((change.added, change.removed.len()), (vec![2], 0));
        assert_eq!(
            levels(&set),
            [
                (0, "pch-0", VOLUME_NORM, false),
                (2, "device-0", VOLUME_NORM / 2, true)
            ]
        );
        // The same key on another PCM is another output.
        let change = set.rescan(vec![pch(), usb(4)]).unwrap();
        assert_eq!(change.added, [3]);
        assert_eq!(change.removed.len(), 1);
        // The playing output gone and its card back under another number
        // in the same table: the new output takes its name and level.
        assert!(set.set_active(1));
        assert!(set.set_muted(1, false));
        assert!(set.set_volume(1, VOLUME_NORM / 4));
        let change = set.rescan(vec![pch(), usb(5)]).unwrap();
        assert!(change.active_gone);
        assert_eq!(change.added, [4]);
        assert_eq!(
            levels(&set)[2],
            (4, "device-0", VOLUME_NORM / 4, false),
            "its name, not device-0.5, and its level"
        );
        assert!(set.set_active(2));
        assert_eq!(set.remove(1).map(|entry| entry.index), Some(3));
        // The active output gone stays until the caller moves off it.
        assert!(set.set_active(1));
        let change = set.rescan(vec![pch()]).unwrap();
        assert!(change.active_gone && change.removed.is_empty());
        assert_eq!(set.active().index, 4);
        assert!(set.set_active(0));
        assert_eq!(set.remove(1).map(|entry| entry.index), Some(4));
        assert!(set.remove(0).is_none(), "never the active one");
        assert_eq!(levels(&set).len(), 1);
        // The stand-in has no discovery to follow.
        let mut stand_in = OutputSet::default();
        let change = stand_in.rescan(vec![pch()]).unwrap();
        assert!(change.added.is_empty() && !change.active_gone);
        assert_eq!(stand_in.entries().len(), 1);
    }

    /// A new output whose name another holds takes its card's number, as
    /// discovery names one; one that still collides, and any past the cap,
    /// is not offered.
    #[test]
    fn a_rescan_refuses_a_taken_name_and_outputs_past_the_cap() {
        let mut set = pair();
        let mut found = vec![
            output(0, 0, "PCH:0", "pch-0", "a"),
            output(1, 0, "Device:0", "device-0", "b"),
            output(3, 0, "pch:0", "pch-0", "c"),
            output(3, 1, "pch:1", "pch-0", "d"),
        ];
        let change = set.rescan(found.clone()).unwrap();
        assert_eq!((change.added, change.refused), (vec![2], 1));
        assert_eq!(set.entries()[2].name, "pch-0.3");
        for card in 10..10 + MAX_OUTPUTS as u32 {
            found.push(output(
                card,
                0,
                &format!("C{card}:0"),
                &format!("c{card}-0"),
                "e",
            ));
        }
        let change = set.rescan(found).unwrap();
        assert_eq!(set.entries().len(), MAX_OUTPUTS);
        assert_eq!(change.added.len(), MAX_OUTPUTS - 3);
        assert_eq!(change.refused, 4);
    }

    /// The human's choices order a fallback, most recent first, then the
    /// rest in discovery order; the latest chosen of those present returns
    /// when it is not the one playing.
    #[test]
    fn choices_order_the_fallback_and_bring_an_output_back() {
        let mut set = OutputSet::discovered(
            vec![
                output(0, 0, "A:0", "a-0", "a"),
                output(1, 0, "B:0", "b-0", "b"),
                output(2, 0, "C:0", "c-0", "c"),
            ],
            0,
        )
        .unwrap();
        let order = |set: &OutputSet| (0..4).map(|n| set.preferred(n)).collect::<Vec<_>>();
        assert_eq!(order(&set), [Some(0), Some(1), Some(2), None]);
        let any = |_| true;
        assert_eq!(set.returning(any), None, "nothing chosen");
        set.chosen(2).unwrap();
        set.chosen(1).unwrap();
        assert_eq!(order(&set), [Some(1), Some(2), Some(0), None]);
        assert_eq!(set.returning(any), Some(1));
        // B, chosen last, is listed but not fresh, a device that failed:
        // C, chosen before and fresh, returns past it.
        assert_eq!(set.returning(|index| index == 2), Some(2));
        assert_eq!(set.returning(|_| false), None);
        set.chosen(2).unwrap();
        assert_eq!(order(&set), [Some(2), Some(1), Some(0), None]);
        assert!(set.set_active(2));
        assert_eq!(set.returning(any), None, "it plays");
        assert!(set.set_active(0));
        set.chosen(1).unwrap();
        assert!(set.set_active(1));
        assert_eq!(
            set.returning(|index| index == 2),
            None,
            "B, chosen later, plays: C fresh does not take over"
        );
        set.chosen(2).unwrap();
        assert!(set.set_active(2));
        // Gone, the next chosen is the one to return; removal shifts the
        // active place with it.
        assert_eq!(set.remove(1).map(|entry| entry.index), Some(1));
        assert_eq!(set.active().index, 2);
        assert!(set.set_active(0));
        assert_eq!(set.returning(any), Some(1), "C, now at 1");
        assert_eq!(order(&set), [Some(1), Some(0), None, None]);
        // The list holds the latest `MAX_OUTPUTS` keys.
        for card in 0..MAX_OUTPUTS as u32 + 1 {
            set.preference.push(format!("X{card}:0"));
        }
        set.preference.truncate(MAX_OUTPUTS);
        set.chosen(0).unwrap();
        assert_eq!(set.preference.len(), MAX_OUTPUTS);
        assert_eq!(set.preference[0], "A:0");
        assert!(set.chosen(9).is_ok(), "no output there, nothing chosen");
    }
}
