use crate::scene::SurfaceKey;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME: usize = 64;
/// A leave, an enter, a motion, and one axis event per axis. The two axes are
/// counted separately because a wheel that tilts reports both in one frame.
const MAX_POINTER_ROUTING_EVENTS_PER_FRAME: usize = 5;
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

/// The two axes a wheel can turn, named rather than numbered because the wire
/// values are 0 and 1 and a swapped pair scrolls a well-formed distance along
/// the wrong one — which nothing in a byte stream distinguishes from the right
/// one. `wire()` is where the numbers live, and it is pinned by a test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerAxis {
    Vertical,
    Horizontal,
}

impl PointerAxis {
    pub fn wire(self) -> u32 {
        match self {
            PointerAxis::Vertical => 0,
            PointerAxis::Horizontal => 1,
        }
    }
}

/// What one report's wheel said, in DETENTS and in evdev's own signs — the
/// unit the kernel reports and the only one in which "one notch" is a whole
/// number. Turning that into the protocol's units is `PointerScroll::steps`,
/// and it happens once, here, rather than at each layer that carries a scroll.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PointerScroll {
    pub vertical: i32,
    pub horizontal: i32,
}

impl PointerScroll {
    /// A report whose wheel did not move. Not the same as `Default::default()`
    /// being asked for: this is the question a frame asks to decide whether it
    /// has anything to say.
    pub fn is_still(self) -> bool {
        self.vertical == 0 && self.horizontal == 0
    }

    /// The axes that moved, in the PROTOCOL's units and signs, vertical first.
    ///
    /// Both conversions happen here, and the SIGN one is why `detents` comes
    /// back rather than being recovered later: evdev counts a wheel pushed
    /// away from the operator as positive, while `wl_pointer.axis` is a
    /// movement of the surface's own content, where positive is downward — so
    /// a notch away from the operator is a NEGATIVE Wayland value, which is
    /// what libinput does with the same two conventions. Horizontal agrees in
    /// both (positive is rightward) and is carried through, which is why this
    /// is not one negation applied to a pair. `axis_discrete` must agree in
    /// sign with the value beside it, so the flipped count is what this
    /// answers and no caller flips anything.
    ///
    /// The SCALE is `AXIS_STEP`, the distance a compositor declares a notch to
    /// be worth. The protocol gives no unit for a wheel — the value is
    /// "a length in the same coordinate space as motion" — so it is a choice
    /// rather than a conversion, and this one is weston's.
    pub fn steps(self) -> Vec<AxisStep> {
        let mut steps = Vec::new();
        if self.vertical != 0 {
            steps.push(AxisStep::of(
                PointerAxis::Vertical,
                self.vertical.saturating_neg(),
            ));
        }
        if self.horizontal != 0 {
            steps.push(AxisStep::of(PointerAxis::Horizontal, self.horizontal));
        }
        steps
    }
}

/// One axis of one report, converted. Both numbers are the protocol's and
/// carry the same sign, which is the property `axis_discrete` needs and which
/// a pair passed separately would not enforce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AxisStep {
    pub axis: PointerAxis,
    pub value: i32,
    pub detents: i32,
}

impl AxisStep {
    /// Clamped, because the wheel is the first input to reach `wl_fixed`
    /// UNBOUNDED. A delta is clamped to the framebuffer before it is encoded
    /// and enter/motion coordinates are surface-local, but detents are summed
    /// straight off the device — and `saturating_add` in the reader turns a
    /// device spamming the wheel into exactly `i32::MAX`, the one value that
    /// cannot be encoded. That is not a panic but it is worse than one here:
    /// the encoder's error propagates out of the seat worker, so a single
    /// malformed report would take down a client's whole event delivery.
    pub fn of(axis: PointerAxis, detents: i32) -> Self {
        let detents = detents.clamp(-MAX_DETENTS_PER_REPORT, MAX_DETENTS_PER_REPORT);
        AxisStep {
            axis,
            value: detents.saturating_mul(AXIS_STEP),
            detents,
        }
    }
}

/// Surface units one detent is worth. The protocol declines to say, so every
/// compositor picks one and clients scale from what they are given; ten is
/// weston's.
pub const AXIS_STEP: i32 = 10;

/// The most detents one report may carry. Not a PHYSICAL bound — a wheel
/// reports single digits, and no real one comes near this — but an ENCODING
/// one, derived from the only thing that makes a value unsendable: `wl_fixed`
/// is 24.8, so `pointer_fixed` multiplies by 256 and refuses what leaves an
/// `i32`. Derived rather than written out, so it cannot drift from `AXIS_STEP`
/// if that is ever retuned.
const MAX_DETENTS_PER_REPORT: i32 = i32::MAX / 256 / AXIS_STEP;

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
    /// Already converted: the step carries the protocol's units and signs, so
    /// nothing downstream of the routing layer knows evdev's.
    Axis {
        surface: SurfaceKey,
        time: u32,
        step: AxisStep,
    },
}

impl PointerEvent {
    pub fn surface(&self) -> SurfaceKey {
        match self {
            PointerEvent::Enter { target } | PointerEvent::Motion { target, .. } => target.surface,
            PointerEvent::Leave { surface }
            | PointerEvent::Button { surface, .. }
            | PointerEvent::Axis { surface, .. } => *surface,
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
/// a press in it established a grab on, which is what the click half of the
/// focus policy needs and what the events alone do not say, and whether a
/// press in it was CLAIMED by
/// the compositor rather than delivered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PointerFrameResult {
    pub frames: Vec<RoutedPointerFrame>,
    pub pressed_on: Option<SurfaceKey>,
    pub claimed: bool,
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

    /// `claim` names presses the COMPOSITOR takes for itself — a gesture of
    /// its own rather than a click for a client. It is asked here, walking the
    /// transitions in order, rather than by the caller over the whole report:
    /// a report can carry several, so the grab a press must not steal may be
    /// established or dropped by an earlier transition IN THE SAME ONE. An
    /// answer computed once for the report would take a button a client had
    /// just grabbed, and refuse one whose grab had just ended.
    pub fn frame(
        &mut self,
        time: u32,
        hover: Option<PointerTarget>,
        grab_target: Option<PointerTarget>,
        buttons: &[PointerButtonInput],
        scroll: PointerScroll,
        claim: impl Fn(PointerButtonInput) -> bool,
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
        let mut claimed = false;
        for input in buttons {
            // A claimed press enters neither `pressed` nor `delivered`, so
            // its release stops at the `changed` check below and is inert.
            if input.state == PointerButtonState::Pressed && next.grab.is_none() && claim(*input) {
                claimed = true;
                continue;
            }
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
        // After the buttons, so the client sees the two in the order the
        // report carried them, and so a release that ENDS a grab has already
        // moved focus: the notch then goes where the pointer now is rather
        // than to the surface that was being dragged a moment ago. Sent to
        // `focus` and not to `hover` for the same reason a button is: while a
        // grab IS held, the surface being dragged owns the pointer, and a
        // wheel turned mid-drag belongs to it even if the cursor has left it.
        // A scroll over nothing is DROPPED rather than queued — there is no
        // surface to owe it to, and the next enter is not a place the
        // operator scrolled.
        if let Some(target) = next.focus {
            for step in scroll.steps() {
                events.push(PointerEvent::Axis {
                    surface: target.surface,
                    time,
                    step,
                });
            }
        }
        if events.len() > MAX_POINTER_FRAME_EVENTS {
            return Err(format!(
                "pointer frame exceeds {MAX_POINTER_FRAME_EVENTS} routed events"
            ));
        }
        let frames = self.finish(next, events)?;
        Ok(PointerFrameResult {
            frames,
            pressed_on,
            claimed,
        })
    }

    pub fn refresh(
        &mut self,
        hover: Option<PointerTarget>,
        grab_target: Option<PointerTarget>,
    ) -> Result<Vec<RoutedPointerFrame>, String> {
        // No buttons and no wheel, so no press: the caller has nothing to
        // learn from it.
        Ok(self
            .frame(
                self.last_time,
                hover,
                grab_target,
                &[],
                PointerScroll::default(),
                |_| false,
            )?
            .frames)
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
            Ok(self
                .frame(
                    time,
                    hover,
                    grab_target,
                    buttons,
                    PointerScroll::default(),
                    |_| false,
                )?
                .frames)
        }

        /// The whole result, for a report in which the compositor claims
        /// nothing — which is every report but an Alt gesture's.
        fn unclaimed(
            &mut self,
            time: u32,
            hover: Option<PointerTarget>,
            grab_target: Option<PointerTarget>,
            buttons: &[PointerButtonInput],
        ) -> Result<PointerFrameResult, String> {
            self.frame(
                time,
                hover,
                grab_target,
                buttons,
                PointerScroll::default(),
                |_| false,
            )
        }

        /// A report whose only content is the wheel, which is the ordinary
        /// scroll: a notch arrives with no motion and no button.
        fn scrolled(
            &mut self,
            time: u32,
            hover: Option<PointerTarget>,
            scroll: PointerScroll,
        ) -> Result<Vec<RoutedPointerFrame>, String> {
            Ok(self
                .frame(time, hover, None, &[], scroll, |_| false)?
                .frames)
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
            state
                .unclaimed(1, Some(first), None, &[])
                .unwrap()
                .pressed_on,
            None
        );
        // A press over nothing establishes nothing.
        let mut empty = PointerState::default();
        assert_eq!(
            empty
                .unclaimed(1, None, None, &[press(1, 272)])
                .unwrap()
                .pressed_on,
            None
        );

        assert_eq!(
            state
                .unclaimed(2, Some(first), None, &[press(2, 272)])
                .unwrap()
                .pressed_on,
            Some(first.surface)
        );
        // A second button DURING the grab establishes nothing, even though
        // the pointer has moved on to another surface.
        let grabbed = target(1, 10, 14, 15);
        assert_eq!(
            state
                .unclaimed(3, Some(second), Some(grabbed), &[press(3, 273)])
                .unwrap()
                .pressed_on,
            None
        );
        // Release both and press again in one frame: the grab moves to where
        // the pointer now is, and that is what is reported.
        let ended = state
            .unclaimed(
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
        twice.unclaimed(1, Some(first), None, &[]).unwrap();
        let both = twice
            .unclaimed(
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
        quick.unclaimed(1, Some(first), None, &[]).unwrap();
        let clicked = quick
            .unclaimed(2, Some(first), None, &[press(2, 272), release(2, 272)])
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
        // The worst case carries a wheel too, and on BOTH axes: the bound is
        // the routing events plus the buttons, and a report that saturated
        // the buttons while scrolling would exceed a bound that counted only
        // the other three. Driven through `frame` rather than `frames`
        // because the scroll is the point.
        let scroll = PointerScroll {
            vertical: 1,
            horizontal: -1,
        };
        let frames = state
            .frame(
                3,
                Some(hover),
                Some(moved_grab),
                &transitions,
                scroll,
                |_| false,
            )
            .unwrap()
            .frames;
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

    #[test]
    fn the_axis_numbers_are_the_protocols_and_not_each_others() {
        assert_eq!(PointerAxis::Vertical.wire(), 0);
        assert_eq!(PointerAxis::Horizontal.wire(), 1);
    }

    #[test]
    fn a_wheel_turned_away_from_the_operator_scrolls_a_surface_downwards() {
        // The one conversion nothing observable would catch: both directions
        // are well-formed scrolls, so a missing negation reads as a compositor
        // whose wheel is upside down and as nothing at all to a test that only
        // checked a value arrived.
        let away = PointerScroll {
            vertical: 1,
            horizontal: 0,
        };
        let steps = away.steps();
        assert_eq!(steps.len(), 1);
        let step = *steps.first().unwrap();
        assert_eq!(step.axis, PointerAxis::Vertical);
        // evdev's +1 comes back as the protocol's -1. Both numbers flip, so
        // the pair still agrees in sign — which `axis_discrete` requires, and
        // which is asserted where it can FAIL, on the wire in
        // `a_tilting_wheel_names_its_source_once_for_the_whole_frame`.
        // Asserting it here would only restate the constructor.
        assert_eq!(step.detents, -1, "the protocol's sign, not evdev's");
        assert_eq!(step.value, -AXIS_STEP);

        // Horizontal keeps evdev's sign: both call rightward positive.
        let right = PointerScroll {
            vertical: 0,
            horizontal: 1,
        };
        let step = *right.steps().first().unwrap();
        assert_eq!(step.axis, PointerAxis::Horizontal);
        assert_eq!(step.detents, 1);
        assert_eq!(step.value, AXIS_STEP);

        // A still wheel produces no step at all rather than a zero one: an
        // axis event carrying no distance is a scroll the client did not get.
        assert!(PointerScroll::default().is_still());
        assert!(PointerScroll::default().steps().is_empty());
        assert!(!away.is_still());

        // Several notches in one report are one event of several steps, not
        // several events — the wheel was turned once, quickly.
        let flick = PointerScroll {
            vertical: -3,
            horizontal: 0,
        };
        let step = *flick.steps().first().unwrap();
        assert_eq!(flick.steps().len(), 1);
        assert_eq!(step.detents, 3);
        assert_eq!(step.value, 3 * AXIS_STEP);
    }

    #[test]
    fn a_wheel_no_report_could_carry_is_clamped_to_something_sendable() {
        // The wheel is the first input to reach `wl_fixed` unbounded, and the
        // reader's `saturating_add` makes the unsendable value the one a
        // spamming device lands on exactly. Encoding is where it would show:
        // `pointer_fixed` refuses what leaves an `i32`, and that error
        // propagates out of the seat worker — one malformed report taking
        // down a client's whole delivery rather than one absurd scroll.
        for detents in [i32::MAX, i32::MIN, MAX_DETENTS_PER_REPORT + 1] {
            let step = AxisStep::of(PointerAxis::Vertical, detents);
            assert!(
                step.value.checked_mul(256).is_some(),
                "{detents} detents encoded to an unsendable {}",
                step.value
            );
            // The SIGN survives the clamp, so an absurd scroll is still a
            // scroll the right way rather than one the other way.
            assert_eq!(step.detents.signum(), detents.signum());
            assert_eq!(step.value.signum(), detents.signum());
        }

        // And the bound is not so tight that a real wheel meets it: an
        // ordinary flick is single digits.
        let step = AxisStep::of(PointerAxis::Vertical, 12);
        assert_eq!(step.detents, 12);
        assert_eq!(step.value, 12 * AXIS_STEP);
    }

    #[test]
    fn a_notch_is_routed_to_the_surface_under_the_pointer_and_to_no_other() {
        let mut state = PointerState::default();
        let under = target(1, 10, 4, 5);
        // The enter comes first and in the same frame: a client is told where
        // the pointer is before it is told the wheel turned there.
        let frames = state
            .scrolled(
                7,
                Some(under),
                PointerScroll {
                    vertical: 2,
                    horizontal: 0,
                },
            )
            .unwrap();
        assert_eq!(frames.len(), 1);
        let frame = frames.first().unwrap();
        assert_eq!(frame.client, 1);
        assert_eq!(
            frame.events,
            vec![
                PointerEvent::Enter { target: under },
                PointerEvent::Axis {
                    surface: under.surface,
                    time: 7,
                    step: AxisStep::of(PointerAxis::Vertical, -2),
                },
            ]
        );

        // A second notch over the same surface is the axis ALONE: nothing
        // entered or moved, so a frame that re-sent an enter would be telling
        // the client its pointer had left and come back.
        let frames = state
            .scrolled(
                8,
                Some(under),
                PointerScroll {
                    vertical: 0,
                    horizontal: 1,
                },
            )
            .unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Axis {
                surface: under.surface,
                time: 8,
                step: AxisStep::of(PointerAxis::Horizontal, 1),
            }]
        );
    }

    #[test]
    fn a_notch_over_nothing_is_dropped_rather_than_kept_for_the_next_surface() {
        let mut state = PointerState::default();
        // Over the desktop: no surface owes anything, and a scroll queued
        // here would land on whatever the pointer next entered — a surface
        // the operator never scrolled over.
        let frames = state
            .scrolled(
                3,
                None,
                PointerScroll {
                    vertical: 5,
                    horizontal: -5,
                },
            )
            .unwrap();
        assert!(frames.is_empty(), "{frames:?}");

        let under = target(1, 10, 0, 0);
        let frames = state.frames(4, Some(under), None, &[]).unwrap();
        assert_eq!(
            frames.first().unwrap().events,
            vec![PointerEvent::Enter { target: under }],
            "the dropped scroll came back with the enter"
        );
    }

    #[test]
    fn a_notch_follows_a_release_that_ended_a_grab_rather_than_preceding_it() {
        // The axis is emitted AFTER the buttons, which is what puts it in the
        // order the report carried them and what makes a release that ENDS a
        // grab move focus first. Emitted before, the notch would go to the
        // surface that had just stopped being dragged.
        let mut state = PointerState::default();
        let held = target(1, 10, 2, 3);
        let under = target(2, 20, 4, 5);
        state
            .frames(
                1,
                Some(held),
                Some(held),
                &[button(1, 272, PointerButtonState::Pressed)],
            )
            .unwrap();
        assert_eq!(state.grab_surface(), Some(held.surface));

        // One report: the release that ends the grab, and a notch. The cursor
        // is over the OTHER surface by now, which is what a drag ending
        // somewhere else looks like.
        let frames = state
            .frame(
                2,
                Some(under),
                Some(held),
                &[button(2, 272, PointerButtonState::Released)],
                PointerScroll {
                    vertical: 1,
                    horizontal: 0,
                },
                |_| false,
            )
            .unwrap()
            .frames;
        let axis = frames
            .iter()
            .flat_map(|frame| frame.events.iter())
            .find_map(|event| match event {
                PointerEvent::Axis { surface, .. } => Some(*surface),
                _ => None,
            });
        assert_eq!(
            axis,
            Some(under.surface),
            "the notch went to the surface the release let go of"
        );

        // And the button reached the surface that HAD the grab, so the two
        // halves of the report are not simply both going to the new focus.
        let released = frames
            .iter()
            .flat_map(|frame| frame.events.iter())
            .find_map(|event| match event {
                PointerEvent::Button { surface, .. } => Some(*surface),
                _ => None,
            });
        assert_eq!(released, Some(held.surface));
    }

    #[test]
    fn a_notch_during_a_drag_goes_to_the_surface_being_dragged() {
        // A grab owns the pointer, so the wheel goes where the buttons go
        // even once the cursor has left the surface holding it. Delivering to
        // whatever is under the cursor instead would scroll a window the
        // operator is in the middle of dragging something out of.
        let mut state = PointerState::default();
        let held = target(1, 10, 2, 3);
        let elsewhere = target(2, 20, 1, 1);
        state
            .frames(
                1,
                Some(held),
                Some(held),
                &[button(1, 272, PointerButtonState::Pressed)],
            )
            .unwrap();
        assert_eq!(state.grab_surface(), Some(held.surface));

        let frames = state
            .frame(
                2,
                Some(elsewhere),
                Some(held),
                &[],
                PointerScroll {
                    vertical: 1,
                    horizontal: 0,
                },
                |_| false,
            )
            .unwrap()
            .frames;
        assert_eq!(frames.len(), 1, "{frames:?}");
        let frame = frames.first().unwrap();
        assert_eq!(frame.client, held.surface.client);
        assert_eq!(
            frame.events,
            vec![PointerEvent::Axis {
                surface: held.surface,
                time: 2,
                step: AxisStep::of(PointerAxis::Vertical, -1),
            }]
        );
    }
}
