use crate::scene::SurfaceKey;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME: usize = 64;
const MAX_POINTER_ROUTING_EVENTS_PER_FRAME: usize = 3;
pub const MAX_POINTER_FRAME_EVENTS: usize =
    MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME + MAX_POINTER_ROUTING_EVENTS_PER_FRAME;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerTarget {
    pub surface: SurfaceKey,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerButtonState {
    Released,
    Pressed,
}

impl PointerButtonState {
    pub fn wire(self) -> u32 {
        match self {
            PointerButtonState::Released => 0,
            PointerButtonState::Pressed => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerButtonInput {
    pub time: u32,
    pub button: u32,
    pub state: PointerButtonState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PointerEvent {
    Enter {
        target: PointerTarget,
    },
    Leave {
        surface: SurfaceKey,
    },
    Motion {
        time: u32,
        target: PointerTarget,
    },
    Button {
        surface: SurfaceKey,
        input: PointerButtonInput,
    },
}

impl PointerEvent {
    pub fn surface(&self) -> SurfaceKey {
        match self {
            PointerEvent::Enter { target } | PointerEvent::Motion { target, .. } => target.surface,
            PointerEvent::Leave { surface } | PointerEvent::Button { surface, .. } => *surface,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedPointerFrame {
    pub revision: u64,
    pub client: u64,
    pub events: Vec<PointerEvent>,
}

/// What one report produced: the routed events, and — separately — the surface
/// a press in it established a grab on, which is what click-to-focus needs and
/// what the events alone do not say.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PointerFrameResult {
    pub frames: Vec<RoutedPointerFrame>,
    pub pressed_on: Option<SurfaceKey>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerSnapshot {
    pub revision: u64,
    pub focus: Option<PointerTarget>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PointerState {
    focus: Option<PointerTarget>,
    hover: Option<PointerTarget>,
    pressed: BTreeSet<u32>,
    delivered: BTreeSet<u32>,
    grab: Option<SurfaceKey>,
    revision: u64,
    last_time: u32,
}

impl PointerState {
    pub fn snapshot(&self) -> PointerSnapshot {
        PointerSnapshot {
            revision: self.revision,
            focus: self.focus,
        }
    }

    /// Drive the counter to its last value so the next frame that produces
    /// any event fails. `refresh_focus` has no other injectable failure, and
    /// the restore paths that turn on one are otherwise untestable.
    #[cfg(test)]
    pub fn exhaust_revision(&mut self) {
        self.revision = u64::MAX;
    }

    pub fn grab_surface(&self) -> Option<SurfaceKey> {
        self.grab
    }

    pub fn frame(
        &mut self,
        time: u32,
        hover: Option<PointerTarget>,
        grab_target: Option<PointerTarget>,
        buttons: &[PointerButtonInput],
    ) -> Result<PointerFrameResult, String> {
        if buttons.len() > MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
            return Err(format!(
                "pointer frame exceeds {MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME} button transitions"
            ));
        }
        let mut next = self.clone();
        next.last_time = time;
        next.hover = hover;
        let mut events = Vec::new();
        next.reconcile_grab(grab_target);
        let target = if next.grab.is_some() {
            grab_target
        } else {
            next.hover
        };
        next.transition(target, time, &mut events);
        let mut pressed_on = None;
        for input in buttons {
            // Every establishing press in one frame names the SAME surface: a
            // press establishes only when no grab is held, and with no grab
            // held `transition` has already pointed focus at `hover`. So which
            // of them is recorded cannot differ.
            if let Some(surface) = next.button(*input, &mut events) {
                pressed_on = Some(surface);
            }
            if next.grab.is_none() {
                next.transition(next.hover, time, &mut events);
            }
        }
        if events.len() > MAX_POINTER_FRAME_EVENTS {
            return Err(format!(
                "pointer frame exceeds {MAX_POINTER_FRAME_EVENTS} routed events"
            ));
        }
        let frames = self.finish(next, events)?;
        Ok(PointerFrameResult { frames, pressed_on })
    }

    pub fn refresh(
        &mut self,
        hover: Option<PointerTarget>,
        grab_target: Option<PointerTarget>,
    ) -> Result<Vec<RoutedPointerFrame>, String> {
        // No buttons, so no press: the caller has nothing to learn from it.
        Ok(self.frame(self.last_time, hover, grab_target, &[])?.frames)
    }

    fn reconcile_grab(&mut self, grab_target: Option<PointerTarget>) {
        if self.grab.is_some() && grab_target.is_none() {
            self.grab = None;
            self.pressed.clear();
            self.delivered.clear();
        }
    }

    fn transition(
        &mut self,
        target: Option<PointerTarget>,
        time: u32,
        events: &mut Vec<PointerEvent>,
    ) {
        match (self.focus, target) {
            (Some(current), Some(next)) if current.surface == next.surface => {
                if current.x != next.x || current.y != next.y {
                    events.push(PointerEvent::Motion { time, target: next });
                    self.focus = Some(next);
                }
            }
            (Some(current), Some(next)) => {
                events.push(PointerEvent::Leave {
                    surface: current.surface,
                });
                events.push(PointerEvent::Enter { target: next });
                self.focus = Some(next);
            }
            (Some(current), None) => {
                events.push(PointerEvent::Leave {
                    surface: current.surface,
                });
                self.focus = None;
            }
            (None, Some(next)) => {
                events.push(PointerEvent::Enter { target: next });
                self.focus = Some(next);
            }
            (None, None) => {}
        }
    }

    /// Answers the surface a press ESTABLISHED a grab on, which is the one
    /// this button event was routed to. Sampling `grab` before and after a
    /// frame cannot ask that: a press and release together end with no grab,
    /// and a release-then-press replaces one grab with another.
    fn button(
        &mut self,
        input: PointerButtonInput,
        events: &mut Vec<PointerEvent>,
    ) -> Option<SurfaceKey> {
        let changed = match input.state {
            PointerButtonState::Pressed => self.pressed.insert(input.button),
            PointerButtonState::Released => self.pressed.remove(&input.button),
        };
        if !changed {
            return None;
        }
        let mut established = None;
        match input.state {
            PointerButtonState::Pressed => {
                if let Some(target) = self.focus {
                    self.delivered.insert(input.button);
                    events.push(PointerEvent::Button {
                        surface: target.surface,
                        input,
                    });
                    if self.grab.is_none() {
                        self.grab = Some(target.surface);
                        established = Some(target.surface);
                    }
                }
            }
            PointerButtonState::Released => {
                if self.delivered.remove(&input.button) {
                    if let Some(target) = self.focus {
                        events.push(PointerEvent::Button {
                            surface: target.surface,
                            input,
                        });
                    }
                }
            }
        }
        if input.state == PointerButtonState::Released && self.delivered.is_empty() {
            self.grab = None;
        }
        established
    }

    fn finish(
        &mut self,
        mut next: PointerState,
        events: Vec<PointerEvent>,
    ) -> Result<Vec<RoutedPointerFrame>, String> {
        if events.is_empty() {
            *self = next;
            return Ok(Vec::new());
        }
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "pointer revision exhausted".to_string())?;
        let mut clients: BTreeMap<u64, Vec<PointerEvent>> = BTreeMap::new();
        for event in events {
            clients
                .entry(event.surface().client)
                .or_default()
                .push(event);
        }
        let routed = clients
            .into_iter()
            .map(|(client, events)| RoutedPointerFrame {
                revision: next.revision,
                client,
                events,
            })
            .collect();
        *self = next;
        Ok(routed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl PointerState {
        /// The routed events alone, for the tests that are not about which
        /// surface a press established a grab on.
        fn frames(
            &mut self,
            time: u32,
            hover: Option<PointerTarget>,
            grab_target: Option<PointerTarget>,
            buttons: &[PointerButtonInput],
        ) -> Result<Vec<RoutedPointerFrame>, String> {
            Ok(self.frame(time, hover, grab_target, buttons)?.frames)
        }
    }

    fn target(client: u64, object: u32, x: i32, y: i32) -> PointerTarget {
        PointerTarget {
            surface: SurfaceKey { client, object },
            x,
            y,
        }
    }

    fn button(time: u32, button: u32, state: PointerButtonState) -> PointerButtonInput {
        PointerButtonInput {
            time,
            button,
            state,
        }
    }

    #[test]
    fn enter_motion_leave_and_snapshot_are_explicit() {
        let mut state = PointerState::default();
        let first = target(1, 10, 4, 5);
        let frames = state.frames(7, Some(first), None, &[]).unwrap();
        assert_eq!(
            frames,
            vec![RoutedPointerFrame {
                revision: 1,
                client: 1,
                events: vec![PointerEvent::Enter { target: first }],
            }]
        );
        assert_eq!(state.snapshot().focus, Some(first));

        let moved = target(1, 10, 8, 9);
        let frames = state.frames(11, Some(moved), None, &[]).unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Motion {
                time: 11,
                target: moved,
            }]
        );
        assert!(state.frames(12, Some(moved), None, &[]).unwrap().is_empty());
        let frames = state.frames(13, None, None, &[]).unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Leave {
                surface: moved.surface,
            }]
        );
        assert_eq!(state.snapshot().revision, 3);
    }

    #[test]
    fn cross_client_focus_routes_one_frame_to_each_client() {
        let mut state = PointerState::default();
        let first = target(1, 10, 4, 5);
        let second = target(2, 20, 1, 2);
        state.frames(1, Some(first), None, &[]).unwrap();
        let frames = state.frames(2, Some(second), None, &[]).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Leave {
                surface: first.surface,
            }]
        );
        assert_eq!(
            frames.get(1).unwrap().events,
            vec![PointerEvent::Enter { target: second }]
        );
        assert_eq!(
            frames.first().unwrap().revision,
            frames.get(1).unwrap().revision
        );
    }

    #[test]
    fn implicit_grab_keeps_button_and_motion_on_the_pressed_surface() {
        let mut state = PointerState::default();
        let first = target(1, 10, 4, 5);
        let outside = target(2, 20, 1, 2);
        state.frames(1, Some(first), None, &[]).unwrap();
        let press = button(2, 272, PointerButtonState::Pressed);
        let frames = state.frames(2, Some(first), None, &[press]).unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Button {
                surface: first.surface,
                input: press,
            }]
        );
        assert_eq!(state.grab_surface(), Some(first.surface));

        let grabbed = target(1, 10, 40, 50);
        let frames = state.frames(3, Some(outside), Some(grabbed), &[]).unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Motion {
                time: 3,
                target: grabbed,
            }]
        );

        let release = button(4, 272, PointerButtonState::Released);
        let frames = state
            .frames(4, Some(outside), Some(grabbed), &[release])
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            frames.first().unwrap().events,
            vec![
                PointerEvent::Button {
                    surface: first.surface,
                    input: release,
                },
                PointerEvent::Leave {
                    surface: first.surface,
                },
            ]
        );
        assert_eq!(
            frames.get(1).unwrap().events,
            vec![PointerEvent::Enter { target: outside }]
        );
        assert_eq!(state.grab_surface(), None);
    }

    #[test]
    fn duplicate_buttons_and_unmatched_releases_are_suppressed() {
        let mut state = PointerState::default();
        let focus = target(1, 10, 0, 0);
        state.frames(1, Some(focus), None, &[]).unwrap();
        let press = button(2, 272, PointerButtonState::Pressed);
        assert_eq!(
            state.frames(2, Some(focus), None, &[press]).unwrap().len(),
            1
        );
        assert!(state
            .frames(3, Some(focus), Some(focus), &[press])
            .unwrap()
            .is_empty());
        let other_release = button(4, 273, PointerButtonState::Released);
        assert!(state
            .frames(4, Some(focus), Some(focus), &[other_release])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_frame_reports_the_surface_a_press_established_its_grab_on() {
        let mut state = PointerState::default();
        let first = target(1, 10, 4, 5);
        let second = target(2, 20, 8, 9);
        let press = |time, code| button(time, code, PointerButtonState::Pressed);
        let release = |time, code| button(time, code, PointerButtonState::Released);

        // No press, no report — and the answer is not the CURRENT hover.
        assert_eq!(
            state.frame(1, Some(first), None, &[]).unwrap().pressed_on,
            None
        );
        // A press over nothing establishes nothing.
        let mut empty = PointerState::default();
        assert_eq!(
            empty
                .frame(1, None, None, &[press(1, 272)])
                .unwrap()
                .pressed_on,
            None
        );

        assert_eq!(
            state
                .frame(2, Some(first), None, &[press(2, 272)])
                .unwrap()
                .pressed_on,
            Some(first.surface)
        );
        // A second button DURING the grab establishes nothing, even though
        // the pointer has moved on to another surface.
        let grabbed = target(1, 10, 14, 15);
        assert_eq!(
            state
                .frame(3, Some(second), Some(grabbed), &[press(3, 273)])
                .unwrap()
                .pressed_on,
            None
        );
        // Release both and press again in one frame: the grab moves to where
        // the pointer now is, and that is what is reported.
        let ended = state
            .frame(
                4,
                Some(second),
                Some(grabbed),
                &[release(4, 272), release(4, 273), press(4, 274)],
            )
            .unwrap();
        assert_eq!(ended.pressed_on, Some(second.surface));
        assert_eq!(state.grab_surface(), Some(second.surface));

        // TWO establishing presses in one frame name the same surface, since
        // a press establishes only with no grab held and focus is `hover`
        // whenever none is. So recording the first or the last cannot differ.
        let mut twice = PointerState::default();
        twice.frame(1, Some(first), None, &[]).unwrap();
        let both = twice
            .frame(
                2,
                Some(first),
                None,
                &[press(2, 272), release(2, 272), press(2, 273)],
            )
            .unwrap();
        assert_eq!(both.pressed_on, Some(first.surface));

        // A press and its release in ONE frame still reports the press: it
        // ends with no grab at all, which sampling `grab` could not tell from
        // a frame that had no press in it.
        let mut quick = PointerState::default();
        quick.frame(1, Some(first), None, &[]).unwrap();
        let clicked = quick
            .frame(2, Some(first), None, &[press(2, 272), release(2, 272)])
            .unwrap();
        assert_eq!(clicked.pressed_on, Some(first.surface));
        assert_eq!(quick.grab_surface(), None);
    }

    #[test]
    fn a_release_then_press_in_one_frame_retargets_after_the_grab() {
        let mut state = PointerState::default();
        let first = target(1, 10, 4, 5);
        let second = target(2, 20, 8, 9);
        state.frames(1, Some(first), None, &[]).unwrap();
        state
            .frames(
                2,
                Some(first),
                None,
                &[button(2, 272, PointerButtonState::Pressed)],
            )
            .unwrap();
        let grabbed = target(1, 10, 14, 15);
        let release = button(3, 272, PointerButtonState::Released);
        let press = button(3, 273, PointerButtonState::Pressed);
        let frames = state
            .frames(3, Some(second), Some(grabbed), &[release, press])
            .unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![
                PointerEvent::Motion {
                    time: 3,
                    target: grabbed,
                },
                PointerEvent::Button {
                    surface: first.surface,
                    input: release,
                },
                PointerEvent::Leave {
                    surface: first.surface,
                },
            ]
        );
        assert_eq!(
            frames.get(1).unwrap().events,
            vec![
                PointerEvent::Enter { target: second },
                PointerEvent::Button {
                    surface: second.surface,
                    input: press,
                },
            ]
        );
        assert_eq!(state.grab_surface(), Some(second.surface));
    }

    #[test]
    fn a_press_without_focus_cannot_leak_a_release_to_later_focus() {
        let mut state = PointerState::default();
        let press = button(1, 272, PointerButtonState::Pressed);
        assert!(state.frames(1, None, None, &[press]).unwrap().is_empty());
        let focus = target(1, 10, 2, 3);
        assert_eq!(state.frames(2, Some(focus), None, &[]).unwrap().len(), 1);
        let release = button(3, 272, PointerButtonState::Released);
        assert!(state
            .frames(3, Some(focus), None, &[release])
            .unwrap()
            .is_empty());
        assert_eq!(state.grab_surface(), None);
    }

    #[test]
    fn removing_a_grabbed_surface_cancels_held_state_and_enters_hover() {
        let mut state = PointerState::default();
        let first = target(1, 10, 0, 0);
        let second = target(2, 20, 2, 3);
        state.frames(1, Some(first), None, &[]).unwrap();
        state
            .frames(
                2,
                Some(first),
                None,
                &[button(2, 272, PointerButtonState::Pressed)],
            )
            .unwrap();
        let frames = state.refresh(Some(second), None).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(state.grab_surface(), None);
        assert_eq!(state.snapshot().focus, Some(second));
    }

    #[test]
    fn maximum_button_frame_fits_the_composed_event_bound() {
        let mut state = PointerState::default();
        let grabbed = target(1, 10, 4, 5);
        state.frames(1, Some(grabbed), None, &[]).unwrap();
        state
            .frames(
                2,
                Some(grabbed),
                None,
                &[button(2, 272, PointerButtonState::Pressed)],
            )
            .unwrap();

        let moved_grab = target(1, 10, 14, 15);
        let hover = target(1, 20, 8, 9);
        let transitions: Vec<PointerButtonInput> = (0..MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME)
            .map(|index| {
                let state = if index % 2 == 0 {
                    PointerButtonState::Released
                } else {
                    PointerButtonState::Pressed
                };
                button(3, 272, state)
            })
            .collect();
        let frames = state
            .frames(3, Some(hover), Some(moved_grab), &transitions)
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames.first().unwrap().events.len(),
            MAX_POINTER_FRAME_EVENTS
        );

        let before = state.clone();
        let oversized = vec![
            button(4, 272, PointerButtonState::Released);
            MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME + 1
        ];
        assert!(state
            .frames(4, Some(hover), Some(hover), &oversized)
            .is_err());
        assert_eq!(state, before);
    }

    #[test]
    fn revision_exhaustion_does_not_mutate_state() {
        let mut state = PointerState {
            revision: u64::MAX,
            ..PointerState::default()
        };
        let before = state.clone();
        assert!(state
            .frames(1, Some(target(1, 10, 0, 0)), None, &[])
            .is_err());
        assert_eq!(state, before);
    }

    #[test]
    fn wire_states_cover_both_values() {
        assert_eq!(PointerButtonState::Released.wire(), 0);
        assert_eq!(PointerButtonState::Pressed.wire(), 1);
    }
}
