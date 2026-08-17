use std::collections::VecDeque;

pub(crate) const MAX_OUTSTANDING: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToplevelState {
    pub width: i32,
    pub height: i32,
    pub activated: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewStatus {
    Unmapped,
    Hidden(ToplevelState),
    Visible(ToplevelState),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Configure {
    pub serial: u32,
    pub state: ToplevelState,
}

pub struct ConfigureTracker {
    initial_sent: bool,
    configured: bool,
    outstanding: VecDeque<u32>,
    retired: VecDeque<u32>,
    initial_serial: Option<u32>,
    last_layout: Option<ToplevelState>,
}

impl ConfigureTracker {
    pub fn new() -> ConfigureTracker {
        ConfigureTracker {
            initial_sent: false,
            configured: false,
            outstanding: VecDeque::new(),
            retired: VecDeque::new(),
            initial_serial: None,
            last_layout: None,
        }
    }

    pub fn initial_sent(&self) -> bool {
        self.initial_sent
    }

    pub fn can_attach(&self) -> bool {
        self.configured
    }

    pub fn initial(&mut self, serial: u32) -> Result<Configure, String> {
        if self.initial_sent {
            return Err("xdg_surface initial configure was already sent".into());
        }
        let state = ToplevelState {
            width: 0,
            height: 0,
            activated: false,
            fullscreen: false,
        };
        self.push(serial)?;
        self.initial_sent = true;
        self.initial_serial = Some(serial);
        Ok(Configure { serial, state })
    }

    pub fn update(&mut self, status: ViewStatus, serial: u32) -> Result<Option<Configure>, String> {
        if !self.initial_sent || !self.configured {
            return Ok(None);
        }
        let desired = match status {
            ViewStatus::Unmapped => {
                self.last_layout = None;
                return Ok(None);
            }
            ViewStatus::Hidden(mut state) => {
                state.activated = false;
                state.fullscreen = false;
                state
            }
            ViewStatus::Visible(state) => state,
        };
        if desired.width <= 0 || desired.height <= 0 {
            return Err(format!(
                "refusing non-positive XDG layout {}x{}",
                desired.width, desired.height
            ));
        }
        if self.last_layout == Some(desired) {
            return Ok(None);
        }
        if self.outstanding.len() >= MAX_OUTSTANDING {
            return Ok(None);
        }
        self.push(serial)?;
        self.last_layout = Some(desired);
        Ok(Some(Configure {
            serial,
            state: desired,
        }))
    }

    /// Make the next `update` emit even though the layout has not moved.
    ///
    /// A decoration mode is carried to the client by an `xdg_surface.configure`
    /// it acknowledges, and this compositor's answer never changes — so the
    /// deduplication that stops a still window being reconfigured forever would
    /// also swallow the one event `set_mode` is required to produce, leaving a
    /// client waiting to apply a mode it has already been told.
    pub fn reconfigure(&mut self) {
        self.last_layout = None;
    }

    pub fn acknowledge(&mut self, serial: u32) -> Result<(), String> {
        if let Some(position) = self
            .outstanding
            .iter()
            .position(|candidate| *candidate == serial)
        {
            let initial_acknowledged = self.initial_serial.is_some_and(|initial| {
                self.outstanding
                    .iter()
                    .take(position.saturating_add(1))
                    .any(|candidate| *candidate == initial)
            });
            for _ in 0..=position {
                self.outstanding.pop_front();
            }
            if initial_acknowledged {
                self.configured = true;
                self.initial_serial = None;
                self.retired.clear();
            }
            return Ok(());
        }
        if let Some(position) = self
            .retired
            .iter()
            .position(|candidate| *candidate == serial)
        {
            for _ in 0..=position {
                self.retired.pop_front();
            }
            return Ok(());
        }
        Err(format!("acknowledged unknown configure {serial}"))
    }

    pub fn unmap(&mut self) -> Result<(), String> {
        let retired = self
            .retired
            .len()
            .checked_add(self.outstanding.len())
            .ok_or_else(|| "retired configure count overflow".to_string())?;
        if retired > MAX_OUTSTANDING {
            return Err(format!(
                "retired configure count {retired} exceeds {MAX_OUTSTANDING}"
            ));
        }
        self.initial_sent = false;
        self.configured = false;
        self.retired.append(&mut self.outstanding);
        self.initial_serial = None;
        self.last_layout = None;
        Ok(())
    }

    #[cfg(test)]
    fn outstanding(&self) -> Vec<u32> {
        self.outstanding.iter().copied().collect()
    }

    fn push(&mut self, serial: u32) -> Result<(), String> {
        if serial == 0 {
            return Err("configure serial must not be zero".into());
        }
        if self.outstanding.len() >= MAX_OUTSTANDING {
            return Err(format!(
                "client left {MAX_OUTSTANDING} XDG configures unacknowledged"
            ));
        }
        self.outstanding.push_back(serial);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(width: i32, height: i32, activated: bool, fullscreen: bool) -> ViewStatus {
        ViewStatus::Visible(ToplevelState {
            width,
            height,
            activated,
            fullscreen,
        })
    }

    #[test]
    fn initial_configure_is_zero_sized_and_gates_buffers() {
        let mut tracker = ConfigureTracker::new();
        assert!(!tracker.initial_sent());
        assert!(!tracker.can_attach());
        assert_eq!(
            tracker.initial(7).unwrap(),
            Configure {
                serial: 7,
                state: ToplevelState {
                    width: 0,
                    height: 0,
                    activated: false,
                    fullscreen: false,
                },
            }
        );
        assert!(tracker.initial_sent());
        assert!(!tracker.can_attach());
        assert!(tracker.initial(8).is_err());
        tracker.acknowledge(7).unwrap();
        assert!(tracker.can_attach());
    }

    #[test]
    fn layout_updates_are_deduplicated_but_focus_and_fullscreen_count() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        let ordinary = visible(320, 200, false, false);
        assert!(tracker.update(ordinary, 2).unwrap().is_some());
        assert!(tracker.update(ordinary, 3).unwrap().is_none());
        assert!(tracker
            .update(visible(320, 200, true, false), 4)
            .unwrap()
            .is_some());
        assert!(tracker
            .update(visible(320, 200, true, true), 5)
            .unwrap()
            .is_some());
        assert_eq!(tracker.outstanding(), vec![2, 4, 5]);
    }

    #[test]
    fn hidden_views_keep_their_size_and_drop_active_states_once() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        tracker.update(visible(640, 480, true, true), 2).unwrap();
        let hidden = tracker
            .update(
                ViewStatus::Hidden(ToplevelState {
                    width: 640,
                    height: 480,
                    activated: true,
                    fullscreen: true,
                }),
                3,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            hidden.state,
            ToplevelState {
                width: 640,
                height: 480,
                activated: false,
                fullscreen: false,
            }
        );
        assert!(tracker
            .update(
                ViewStatus::Hidden(ToplevelState {
                    width: 640,
                    height: 480,
                    activated: false,
                    fullscreen: false,
                }),
                4,
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_first_hidden_snapshot_has_an_explicit_size() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        let hidden = tracker
            .update(
                ViewStatus::Hidden(ToplevelState {
                    width: 320,
                    height: 200,
                    activated: false,
                    fullscreen: false,
                }),
                2,
            )
            .unwrap()
            .unwrap();
        assert_eq!(hidden.state.width, 320);
        assert_eq!(hidden.state.height, 200);
    }

    #[test]
    fn an_unmapped_snapshot_clears_prior_layout_state() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        tracker
            .update(visible(320, 200, false, false), 2)
            .unwrap();
        assert!(tracker.update(ViewStatus::Unmapped, 3).unwrap().is_none());
        assert!(tracker
            .update(visible(320, 200, false, false), 4)
            .unwrap()
            .is_some());
    }

    #[test]
    fn an_unmap_requires_a_fresh_initial_handshake() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        let state = visible(80, 60, true, false);
        tracker.update(state, 2).unwrap();
        tracker.unmap().unwrap();
        assert!(!tracker.initial_sent());
        assert!(!tracker.can_attach());
        assert!(tracker.outstanding().is_empty());
        assert!(tracker.update(state, 3).unwrap().is_none());
        tracker.initial(4).unwrap();
        tracker.acknowledge(2).unwrap();
        assert!(!tracker.can_attach());
        tracker.acknowledge(4).unwrap();
        assert!(tracker.can_attach());
        assert!(tracker.update(state, 5).unwrap().is_some());
        assert!(tracker.acknowledge(2).is_err());
    }

    #[test]
    fn repeated_unmaps_keep_older_in_flight_serials_acknowledgeable() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        tracker.update(visible(80, 60, false, false), 2).unwrap();
        tracker.unmap().unwrap();
        tracker.initial(3).unwrap();
        tracker.unmap().unwrap();
        tracker.acknowledge(2).unwrap();
        tracker.acknowledge(3).unwrap();
    }

    #[test]
    fn retired_serials_are_bounded_inside_the_tracker() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        for offset in 0..MAX_OUTSTANDING {
            let serial = u32::try_from(offset).unwrap().saturating_add(2);
            let width = i32::try_from(offset).unwrap().saturating_add(1);
            tracker
                .update(visible(width, 10, false, false), serial)
                .unwrap();
        }
        tracker.unmap().unwrap();
        tracker.initial(1000).unwrap();
        assert!(tracker.unmap().is_err());
        tracker.acknowledge(1000).unwrap();
        assert!(tracker.can_attach());
    }

    #[test]
    fn acknowledging_a_serial_supersedes_every_older_configure() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        tracker.update(visible(100, 100, false, false), 2).unwrap();
        tracker.update(visible(90, 100, false, false), 3).unwrap();
        tracker.update(visible(80, 100, false, false), 4).unwrap();
        tracker.acknowledge(3).unwrap();
        assert_eq!(tracker.outstanding(), vec![4]);
        assert!(tracker.acknowledge(2).is_err());
        tracker.acknowledge(4).unwrap();
    }

    /// A decoration mode is applied on the `xdg_surface.configure` that follows
    /// it, so `set_mode` on a window nothing has resized must still produce
    /// one — and the deduplication that keeps a still window still is exactly
    /// what would swallow it.
    #[test]
    fn reconfigure_makes_an_unmoved_layout_configure_again() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        assert!(tracker
            .update(visible(100, 100, false, false), 2)
            .unwrap()
            .is_some());
        // Unchanged: deduplicated, which is the behaviour being worked around.
        assert!(tracker
            .update(visible(100, 100, false, false), 3)
            .unwrap()
            .is_none());

        tracker.reconfigure();
        let configure = tracker
            .update(visible(100, 100, false, false), 4)
            .unwrap()
            .expect("a reconfigure owes a configure for the layout already in force");
        // The SAME layout, at a fresh serial the client can acknowledge — not a
        // resize, which would move a window because its titlebar was discussed.
        assert_eq!(configure.serial, 4);
        assert_eq!(configure.state.width, 100);
        assert_eq!(configure.state.height, 100);

        // One re-send, not a permanent end to deduplication.
        assert!(tracker
            .update(visible(100, 100, false, false), 5)
            .unwrap()
            .is_none());
    }

    #[test]
    fn updates_wait_for_the_initial_ack_and_validate_dimensions() {
        let mut tracker = ConfigureTracker::new();
        assert!(tracker
            .update(visible(10, 10, false, false), 1)
            .unwrap()
            .is_none());
        tracker.initial(1).unwrap();
        assert!(tracker
            .update(visible(10, 10, false, false), 2)
            .unwrap()
            .is_none());
        tracker.acknowledge(1).unwrap();
        assert!(tracker.update(visible(0, 10, false, false), 3).is_err());
        assert!(tracker.update(visible(10, -1, false, false), 3).is_err());
        assert!(tracker
            .update(
                ViewStatus::Hidden(ToplevelState {
                    width: -1,
                    height: 10,
                    activated: false,
                    fullscreen: false,
                }),
                3,
            )
            .is_err());
    }

    #[test]
    fn outstanding_configures_are_bounded() {
        let mut tracker = ConfigureTracker::new();
        tracker.initial(1).unwrap();
        tracker.acknowledge(1).unwrap();
        for offset in 0..MAX_OUTSTANDING {
            let serial = u32::try_from(offset).unwrap().saturating_add(2);
            let width = i32::try_from(offset).unwrap().saturating_add(1);
            assert!(tracker
                .update(visible(width, 10, false, false), serial)
                .unwrap()
                .is_some());
        }
        assert!(tracker
            .update(visible(1000, 10, false, false), 1000)
            .unwrap()
            .is_none());
        tracker.acknowledge(2).unwrap();
        assert!(tracker
            .update(visible(1000, 10, false, false), 1001)
            .unwrap()
            .is_some());
    }

    #[test]
    fn serial_zero_and_unknown_acknowledgements_fail_closed() {
        let mut tracker = ConfigureTracker::new();
        assert!(tracker.initial(0).is_err());
        tracker.initial(1).unwrap();
        assert!(tracker.acknowledge(2).is_err());
    }
}
