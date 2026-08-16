use crate::help::HelpAction;
use crate::keyboard::{
    KeyInput, KeyState, ModifierState, MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_NUM, MOD_SHIFT,
};
use crate::launcher::{LaunchOptions, LaunchProcesses, LaunchRequest, LauncherAction};
use crate::layout::{Command, Direction, Presentation};
use crate::pointer::{
    PointerButtonInput, PointerButtonState, MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME,
};
use crate::runtime::Runtime;
use crate::scene::Fraction;
use crate::sys;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

const EVENT_SIZE: usize = 24;
/// Records drained per read. A batch is what makes a full-speed pointer cost
/// one paint instead of one per report; the ceiling only bounds the buffer,
/// since a reader that falls further behind simply takes another batch.
const READ_BATCH_RECORDS: usize = 64;
const READ_BATCH_BYTES: usize = EVENT_SIZE * READ_BATCH_RECORDS;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const KEY_ESC: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_0: u16 = 11;
const KEY_MINUS: u16 = 12;
const KEY_BACKSPACE: u16 = 14;
const KEY_Q: u16 = 16;
const KEY_W: u16 = 17;
const KEY_E: u16 = 18;
const KEY_R: u16 = 19;
const KEY_T: u16 = 20;
const KEY_Y: u16 = 21;
const KEY_U: u16 = 22;
const KEY_I: u16 = 23;
const KEY_O: u16 = 24;
const KEY_P: u16 = 25;
const KEY_ENTER: u16 = 28;
const KEY_LEFTCTRL: u16 = 29;
const KEY_A: u16 = 30;
const KEY_S: u16 = 31;
const KEY_D: u16 = 32;
const KEY_F: u16 = 33;
const KEY_G: u16 = 34;
const KEY_H: u16 = 35;
const KEY_J: u16 = 36;
const KEY_K: u16 = 37;
const KEY_L: u16 = 38;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_Z: u16 = 44;
const KEY_X: u16 = 45;
const KEY_C: u16 = 46;
const KEY_V: u16 = 47;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_M: u16 = 50;
const KEY_SLASH: u16 = 53;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_SPACE: u16 = 57;
const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;
const KEY_KPENTER: u16 = 96;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_UP: u16 = 103;
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_DOWN: u16 = 108;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const BTN_MOUSE: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
const MAX_XKB_EVDEV_KEY: u16 = 247;
const KEY_RELEASE: i32 = 0;
const KEY_PRESS: i32 = 1;
const KEY_REPEAT: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    time: u32,
    kind: u16,
    code: u16,
    value: i32,
}

#[derive(Default)]
struct KeyBindings {
    pressed: BTreeSet<(usize, u16)>,
    forwarded: BTreeSet<(usize, u16)>,
    caps_lock: bool,
    num_lock: bool,
    launcher_open: bool,
    help_open: bool,
    consumed: BTreeSet<(usize, u16)>,
    pointer_pressed: BTreeSet<(usize, u16)>,
    pointer_forwarded: BTreeSet<u16>,
}

#[derive(Debug, Eq, PartialEq)]
struct KeyDecision {
    command: Option<Command>,
    launcher: Option<LauncherAction>,
    help: Option<HelpAction>,
    launch: Option<LaunchRequest>,
    forward: Option<KeyInput>,
    modifiers: Option<ModifierState>,
}

impl KeyBindings {
    #[cfg(test)]
    fn feed(&mut self, event: Event) -> KeyDecision {
        self.feed_device(0, event)
    }

    fn feed_device(&mut self, device: usize, event: Event) -> KeyDecision {
        let mut decision = KeyDecision {
            command: None,
            launcher: None,
            help: None,
            launch: None,
            forward: None,
            modifiers: None,
        };
        if event.kind != EV_KEY
            || event.code > MAX_XKB_EVDEV_KEY
            || event.value == KEY_REPEAT
            || (event.value != KEY_PRESS && event.value != KEY_RELEASE)
        {
            return decision;
        }
        let physical = (device, event.code);
        let before = self.modifiers();
        let logical_pressed = self.pressed(event.code);
        let changed = if event.value == KEY_PRESS {
            self.pressed.insert(physical)
        } else {
            self.pressed.remove(&physical)
        };
        if !changed {
            return decision;
        }
        if event.value == KEY_PRESS && logical_pressed {
            let consumed = self.consumed.iter().any(|(_, code)| *code == event.code);
            let forwarded = self.forwarded.iter().any(|(_, code)| *code == event.code);
            if consumed {
                self.consumed.insert(physical);
            } else if forwarded {
                decision.forward = self.forward(physical, event);
            } else {
                self.consumed.insert(physical);
            }
            return decision;
        }
        match (event.code, event.value, logical_pressed) {
            (KEY_CAPSLOCK, KEY_PRESS, false) => self.caps_lock = !self.caps_lock,
            (KEY_NUMLOCK, KEY_PRESS, false) => self.num_lock = !self.num_lock,
            _ => {}
        }
        let after = self.modifiers();
        if before != after {
            decision.modifiers = Some(after);
        }
        if event.value == KEY_RELEASE && self.consumed.remove(&physical) {
            return decision;
        }
        if matches!(
            event.code,
            KEY_LEFTCTRL
                | KEY_RIGHTCTRL
                | KEY_LEFTMETA
                | KEY_RIGHTMETA
                | KEY_LEFTSHIFT
                | KEY_RIGHTSHIFT
                | KEY_LEFTALT
                | KEY_RIGHTALT
                | KEY_CAPSLOCK
                | KEY_NUMLOCK
        ) {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        if event.value == KEY_RELEASE {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        // Any NON-MODIFIER key dismisses the sheet: there is nothing to type
        // into it and nothing to select, so such a key can only mean "seen
        // it". Modifiers returned above, which is what lets someone release
        // Super to read and then press a whole chord that is swallowed whole.
        // Checked before the chords so `Super+t` closes rather than launching,
        // and before the launcher so a sheet is always dismissable.
        if self.help_open {
            self.consumed.insert(physical);
            decision.help = Some(HelpAction::Close);
            return decision;
        }
        if self.launcher_open {
            self.consumed.insert(physical);
            let control = self.pressed(KEY_LEFTCTRL) || self.pressed(KEY_RIGHTCTRL);
            let alt = self.pressed(KEY_LEFTALT) || self.pressed(KEY_RIGHTALT);
            let meta = self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA);
            decision.launcher = match event.code {
                KEY_DOWN => Some(LauncherAction::Next),
                KEY_UP => Some(LauncherAction::Previous),
                KEY_N if control => Some(LauncherAction::Next),
                KEY_P if control => Some(LauncherAction::Previous),
                KEY_ENTER | KEY_KPENTER => Some(LauncherAction::Activate),
                KEY_ESC => Some(LauncherAction::Close),
                KEY_G if control => Some(LauncherAction::Close),
                KEY_BACKSPACE if !control && !alt && !meta => Some(LauncherAction::Backspace),
                code if !control && !alt && !meta => {
                    launcher_character(code).map(LauncherAction::Insert)
                }
                _ => None,
            };
            return decision;
        }
        let meta = self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA);
        if !meta {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        let shift = self.pressed(KEY_LEFTSHIFT) || self.pressed(KEY_RIGHTSHIFT);
        // One chord per operation: no prefix, so a chord is read entirely
        // from what is held at this press.
        let chord = match event.code {
            KEY_F => Some(Command::ToggleFullscreen),
            KEY_S => Some(Command::ToggleGrouped),
            // V and H name the direction the BANDS run, not the container's
            // axis: stacked bands go down the column, tabs across it.
            KEY_V => Some(Command::SetPresentation(Presentation::Stacked)),
            KEY_H => Some(Command::SetPresentation(Presentation::Tabbed)),
            _ => None,
        };
        if let Some(command) = chord {
            self.consumed.insert(physical);
            decision.command = Some(command);
            return decision;
        }
        // Both Enters, because the OPEN overlay already activates on either
        // and a keypad that opens nothing while it activates is a coin toss.
        if event.code == KEY_ENTER || event.code == KEY_KPENTER {
            self.consumed.insert(physical);
            decision.launcher = Some(LauncherAction::Open);
            return decision;
        }
        // `?` is Shift+/ on this keymap, and Shift is not required: the sheet
        // is what someone reaches for when they do not know the bindings, so
        // demanding an exact one to see them would be the wrong way round.
        if event.code == KEY_SLASH {
            self.consumed.insert(physical);
            decision.help = Some(HelpAction::Toggle);
            return decision;
        }
        // The terminal without going through the launcher: it is the one entry
        // anybody opens repeatedly, and the registry still carries it.
        if event.code == KEY_T {
            self.consumed.insert(physical);
            decision.launch = Some(LaunchRequest::Terminal);
            return decision;
        }
        if let Some(direction) = direction(event.code) {
            self.consumed.insert(physical);
            decision.command = if shift {
                Some(Command::Move(direction))
            } else {
                Some(Command::Focus(direction))
            };
            return decision;
        }
        decision.command = workspace(event.code).map(|number| {
            if shift {
                Command::MoveToWorkspace(number)
            } else {
                Command::SwitchWorkspace(number)
            }
        });
        if decision.command.is_some() {
            self.consumed.insert(physical);
        } else {
            decision.forward = self.forward(physical, event);
        }
        decision
    }

    fn forward(&mut self, physical: (usize, u16), event: Event) -> Option<KeyInput> {
        if event.value == KEY_PRESS {
            let already_forwarded = self.forwarded.iter().any(|(_, code)| *code == event.code);
            self.forwarded.insert(physical);
            (!already_forwarded).then(|| event.key_input())
        } else if self.forwarded.remove(&physical)
            && !self.forwarded.iter().any(|(_, code)| *code == event.code)
        {
            Some(event.key_input())
        } else {
            None
        }
    }

    fn pressed(&self, code: u16) -> bool {
        self.pressed.iter().any(|(_, pressed)| *pressed == code)
    }

    fn settle_launcher(&mut self, visible: Option<bool>) {
        if let Some(visible) = visible {
            self.launcher_open = visible;
        }
    }

    fn settle_help(&mut self, visible: Option<bool>) {
        if let Some(visible) = visible {
            self.help_open = visible;
        }
    }

    fn modifiers(&self) -> ModifierState {
        let mut depressed = 0;
        if self.pressed(KEY_LEFTSHIFT) || self.pressed(KEY_RIGHTSHIFT) {
            depressed |= MOD_SHIFT;
        }
        if self.pressed(KEY_LEFTCTRL) || self.pressed(KEY_RIGHTCTRL) {
            depressed |= MOD_CONTROL;
        }
        if self.pressed(KEY_LEFTALT) || self.pressed(KEY_RIGHTALT) {
            depressed |= MOD_ALT;
        }
        if self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA) {
            depressed |= MOD_LOGO;
        }
        let mut locked = 0;
        if self.caps_lock {
            locked |= MOD_CAPS;
        }
        if self.num_lock {
            locked |= MOD_NUM;
        }
        ModifierState {
            depressed,
            latched: 0,
            locked,
            group: 0,
        }
    }

    fn pointer_changes(&self, time: u32) -> Vec<PointerButtonInput> {
        self.pointer_changes_with(time, |code| {
            self.pointer_pressed
                .iter()
                .any(|(_, pressed)| *pressed == code)
        })
    }

    fn pointer_device_changes(
        &self,
        device: usize,
        transitions: &[PointerButtonTransition],
        time: u32,
    ) -> Vec<PointerButtonInput> {
        if transitions.is_empty() {
            return Vec::new();
        }
        let mut device_pressed: BTreeSet<u16> = self
            .pointer_pressed
            .iter()
            .filter_map(|(owner, code)| (*owner == device).then_some(*code))
            .collect();
        let mut forwarded = self.pointer_forwarded.clone();
        let mut changes = Vec::new();
        for transition in transitions {
            if transition.pressed {
                device_pressed.insert(transition.code);
            } else {
                device_pressed.remove(&transition.code);
            }
            let physical = device_pressed.contains(&transition.code)
                || self
                    .pointer_pressed
                    .iter()
                    .any(|(owner, code)| *owner != device && *code == transition.code);
            if physical == forwarded.contains(&transition.code) {
                continue;
            }
            let state = if physical {
                forwarded.insert(transition.code);
                PointerButtonState::Pressed
            } else {
                forwarded.remove(&transition.code);
                PointerButtonState::Released
            };
            changes.push(PointerButtonInput {
                time,
                button: u32::from(transition.code),
                state,
            });
        }
        changes
    }

    fn pointer_changes_with(
        &self,
        time: u32,
        mut physical: impl FnMut(u16) -> bool,
    ) -> Vec<PointerButtonInput> {
        let mut changes = Vec::new();
        for code in BTN_MOUSE..=BTN_TASK {
            let pressed = physical(code);
            if pressed == self.pointer_forwarded.contains(&code) {
                continue;
            }
            changes.push(PointerButtonInput {
                time,
                button: u32::from(code),
                state: if pressed {
                    PointerButtonState::Pressed
                } else {
                    PointerButtonState::Released
                },
            });
        }
        changes
    }

    fn commit_pointer(&mut self, buttons: &[PointerButtonInput]) {
        for button in buttons {
            let Ok(code) = u16::try_from(button.button) else {
                continue;
            };
            match button.state {
                PointerButtonState::Pressed => {
                    self.pointer_forwarded.insert(code);
                }
                PointerButtonState::Released => {
                    self.pointer_forwarded.remove(&code);
                }
            }
        }
    }

    fn commit_pointer_device(
        &mut self,
        device: usize,
        pressed: &BTreeSet<u16>,
        buttons: &[PointerButtonInput],
    ) {
        self.remove_pointer_device(device);
        self.pointer_pressed
            .extend(pressed.iter().map(|code| (device, *code)));
        self.commit_pointer(buttons);
    }

    fn remove_pointer_device(&mut self, device: usize) {
        self.pointer_pressed.retain(|(owner, _)| *owner != device);
    }
}

impl Event {
    fn key_input(self) -> KeyInput {
        KeyInput {
            time: self.time,
            key: u32::from(self.code),
            state: if self.value == KEY_RELEASE {
                KeyState::Released
            } else {
                KeyState::Pressed
            },
        }
    }
}

/// Where a device's absolute axes are and what they report over, read once per
/// device when its reader starts. A device with neither is relative and carries
/// `None`, which is what makes an ordinary mouse cost no ioctl and no branch it
/// does not use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AbsoluteAxes {
    x: sys::AbsInfo,
    y: sys::AbsInfo,
}

impl AbsoluteAxes {
    /// A device is absolute exactly when BOTH its axes have a span to place a
    /// value in. Asked of `fraction` rather than of the numbers, so the
    /// admission test and the scaling cannot come to different conclusions
    /// about a span.
    fn declared(x: sys::AbsInfo, y: sys::AbsInfo) -> Option<Self> {
        (Self::fraction(x, x.maximum).denominator > 0
            && Self::fraction(y, y.maximum).denominator > 0)
            .then_some(Self { x, y })
    }

    /// Where along the axis a raw value sits, as the EXACT ratio of the
    /// device's own offset to the device's own span — nothing is rescaled
    /// here, since a second division is a second flooring.
    ///
    /// Values outside the declared range are CLAMPED rather than refused: a
    /// device may report past its own bounds, and the honest reading of one
    /// that does is the edge it went past. A span that is not positive answers
    /// a zero DENOMINATOR rather than an error, which is `declared`'s question
    /// and which `across` reads as the near edge.
    fn fraction(axis: sys::AbsInfo, value: i32) -> Fraction {
        let span = i64::from(axis.maximum).saturating_sub(i64::from(axis.minimum));
        let offset = i64::from(value)
            .saturating_sub(i64::from(axis.minimum))
            .clamp(0, span.max(0));
        Fraction {
            numerator: u32::try_from(offset).unwrap_or(u32::MAX),
            denominator: u32::try_from(span).unwrap_or(0),
        }
    }
}

#[derive(Default)]
struct PointerMotion {
    dx: i32,
    dy: i32,
    /// The absolute value each axis reported in the frame being built. A
    /// tablet sends a POSITION rather than a movement, so the frame carries
    /// the newest one rather than a sum. Separate options because the kernel
    /// omits an axis whose value has not changed, so a report can name one and
    /// say nothing about the other.
    abs_x: Option<i32>,
    abs_y: Option<i32>,
    /// Where THIS DEVICE last was, which is what an omitted axis means. The
    /// cursor's own coordinate will not do: it is shared, so a relative mouse
    /// moving between two tablet reports would leave the axis the tablet did
    /// not mention wherever the MOUSE put it, which is nowhere the stylus is.
    /// `None` until the first report, and answered from `input_absinfo.value`
    /// then — the axis's position at open, which is the only way to know where
    /// a device is before it has said anything.
    held_x: Option<i32>,
    held_y: Option<i32>,
    pressed: BTreeSet<u16>,
    buttons: Vec<PointerButtonTransition>,
    overflowed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerButtonTransition {
    code: u16,
    pressed: bool,
}

/// Where a frame says the pointer is. A relative device can only say how far
/// it moved; an absolute one says where it IS, as a fraction of its own span
/// along BOTH axes. Both even though a report may name only one: the reader
/// holds where the device last was, so the axis a report omits is answered
/// from that device rather than from the shared cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PointerPlace {
    By { dx: i32, dy: i32 },
    At { x: Fraction, y: Fraction },
}

#[derive(Debug, Eq, PartialEq)]
struct PointerFrame {
    time: u32,
    place: PointerPlace,
    buttons: Vec<PointerButtonTransition>,
}

trait InputTarget {
    fn command(&mut self, command: Command) -> Result<(), String>;
    fn launcher(&mut self, action: LauncherAction) -> Result<bool, String>;
    /// Answers whether the sheet is up afterwards, as `launcher` does: the
    /// adapter must know to route the NEXT key to it.
    fn help(&mut self, action: HelpAction) -> Result<bool, String>;
    /// Spawn a registry entry WITHOUT opening the overlay — `Super+t`'s whole
    /// point. Separate from `launcher` because that one is about the overlay's
    /// model and returns its visibility, which this never changes.
    fn launch(&mut self, request: LaunchRequest) -> Result<(), String>;
    fn key(&mut self, input: KeyInput) -> Result<(), String>;
    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String>;
    fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String>;

    /// The absolute form of the same report. Separate rather than one method
    /// with a sum type, because the two are different questions to everything
    /// downstream: one composes with where the pointer was and the other
    /// replaces it.
    fn pointer_frame_at(
        &mut self,
        time: u32,
        x: Fraction,
        y: Fraction,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String>;

    /// Take any paint the delivered reports left owing.
    fn flush(&mut self) -> Result<(), String>;
}

struct LiveInputTarget {
    runtime: Arc<Mutex<Runtime>>,
    launches: LaunchProcesses,
}

impl LiveInputTarget {
    /// A launch failure is REPORTED, never fatal: the evdev reader must keep
    /// serving so the operator can close the overlay or try again.
    fn spawn(&mut self, request: LaunchRequest) {
        match self.launches.launch(request) {
            Ok(failures) => {
                for failure in failures {
                    eprintln!("td-compositor: {failure}");
                }
            }
            Err(error) => eprintln!("td-compositor: {error}"),
        }
    }
}

impl InputTarget for LiveInputTarget {
    fn command(&mut self, command: Command) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .command(command)
    }

    fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
        self.spawn(request);
        Ok(())
    }

    fn help(&mut self, action: HelpAction) -> Result<bool, String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .help(action)
    }

    fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
        let (request, visible) = {
            let mut runtime = self
                .runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?;
            let request = runtime.launcher(action)?;
            (request, runtime.launcher_visible())
        };
        if let Some(request) = request {
            self.spawn(request);
        }
        Ok(visible)
    }

    fn key(&mut self, input: KeyInput) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .key(input)
    }

    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .modifiers(modifiers)
    }

    fn pointer_frame(
        &mut self,
        time: u32,
        dx: i32,
        dy: i32,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_frame(time, dx, dy, buttons)
    }

    fn pointer_frame_at(
        &mut self,
        time: u32,
        x: Fraction,
        y: Fraction,
        buttons: &[PointerButtonInput],
    ) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .pointer_frame_at(time, x, y, buttons)
    }

    fn flush(&mut self) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .flush_paint()
    }
}

impl PointerMotion {
    fn feed(&mut self, event: Event, axes: Option<AbsoluteAxes>) -> Option<PointerFrame> {
        match (event.kind, event.code) {
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(event.value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(event.value),
            // Recorded raw. Whether it MEANS anything is the frame's question,
            // since only a declared range turns a value into a place.
            (EV_ABS, ABS_X) => self.abs_x = Some(event.value),
            (EV_ABS, ABS_Y) => self.abs_y = Some(event.value),
            (EV_KEY, BTN_MOUSE..=BTN_TASK)
                if event.value == KEY_PRESS || event.value == KEY_RELEASE =>
            {
                let pressed = event.value == KEY_PRESS;
                let changed = if pressed {
                    self.pressed.insert(event.code)
                } else {
                    self.pressed.remove(&event.code)
                };
                if changed {
                    if self.buttons.len() >= MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
                        self.dx = 0;
                        self.dy = 0;
                        self.abs_x = None;
                        self.abs_y = None;
                        self.pressed.clear();
                        self.buttons.clear();
                        self.overflowed = true;
                    } else {
                        self.buttons.push(PointerButtonTransition {
                            code: event.code,
                            pressed,
                        });
                    }
                }
            }
            (EV_SYN, SYN_REPORT) => return self.frame(event.time, axes),
            _ => {}
        }
        None
    }

    /// A device with no absolute axes, which is every test about deltas and
    /// buttons — and the ordinary mouse those tests are written for.
    #[cfg(test)]
    fn feed_relative(&mut self, event: Event) -> Option<PointerFrame> {
        self.feed(event, None)
    }

    /// Abandon the frame being accumulated and every button believed held,
    /// which is what a dropped batch and a button overflow both mean.
    ///
    /// The HELD POSITION survives, deliberately: it is not frame state but the
    /// last thing this device said about where it is, and the alternative on
    /// the next one-axis report is the position the kernel gave at OPEN — a
    /// jump to wherever the device was when the compositor started. The
    /// kernel's advice after `SYN_DROPPED` is to re-query the device, which
    /// this reader cannot do: it takes an `impl Read` so its tests can drive
    /// it from a byte slice, and a slice has no descriptor to ask. Keeping
    /// the last known position is the closest thing available, and it is what
    /// every ordinary report between two frames already relies on.
    fn reset(&mut self) {
        let (held_x, held_y) = (self.held_x, self.held_y);
        *self = PointerMotion::default();
        self.held_x = held_x;
        self.held_y = held_y;
    }

    /// Adopt a position the DEVICE reported out of band, which only the
    /// recovery may do: it has just re-read the device, so this is where it
    /// is, and anything remembered from before the gap is a guess.
    fn hold(&mut self, x: i32, y: i32) {
        self.held_x = Some(x);
        self.held_y = Some(y);
    }

    /// Close the frame being accumulated, or answer `None` where it would say
    /// nothing. A position WINS over a delta in the same frame rather than
    /// composing with it: the two are different claims about the same pointer,
    /// and a device that sends both — a touchpad in absolute mode — means the
    /// place, with the deltas its own smoothing of the way there.
    ///
    /// An absolute device's frame is a PLACE unless the only thing in it is a
    /// distance. A BUTTON needs somewhere to land as much as a motion does,
    /// and a click that did not move is the ordinary case rather than a corner
    /// one: the kernel drops both axes as unchanged, so a tablet tapped twice
    /// in the same spot sends nothing but `BTN_*`. Read as a zero delta it
    /// would click wherever another device last left the shared cursor.
    fn frame(&mut self, time: u32, axes: Option<AbsoluteAxes>) -> Option<PointerFrame> {
        let buttons = std::mem::take(&mut self.buttons);
        let (dx, dy) = (std::mem::take(&mut self.dx), std::mem::take(&mut self.dy));
        let (raw_x, raw_y) = (self.abs_x.take(), self.abs_y.take());
        let place = match axes {
            Some(axes) if raw_x.is_some() || raw_y.is_some() || !buttons.is_empty() => {
                let x = raw_x.or(self.held_x).unwrap_or(axes.x.value);
                let y = raw_y.or(self.held_y).unwrap_or(axes.y.value);
                self.held_x = Some(x);
                self.held_y = Some(y);
                PointerPlace::At {
                    x: AbsoluteAxes::fraction(axes.x, x),
                    y: AbsoluteAxes::fraction(axes.y, y),
                }
            }
            _ => PointerPlace::By { dx, dy },
        };
        // A frame that neither moves the pointer nor changes a button is one
        // the compositor owes nothing for. An absolute frame is never that:
        // the arm above is taken only when an axis reported or a button
        // changed, and the kernel drops an axis whose value did not change.
        let silent = match place {
            PointerPlace::By { dx, dy } => dx == 0 && dy == 0,
            PointerPlace::At { .. } => false,
        };
        if silent && buttons.is_empty() {
            return None;
        }
        Some(PointerFrame {
            time,
            place,
            buttons,
        })
    }
}

fn direction(code: u16) -> Option<Direction> {
    match code {
        KEY_LEFT => Some(Direction::Left),
        KEY_RIGHT => Some(Direction::Right),
        KEY_UP => Some(Direction::Up),
        KEY_DOWN => Some(Direction::Down),
        _ => None,
    }
}

fn workspace(code: u16) -> Option<u8> {
    match code {
        KEY_1 => Some(1),
        KEY_2 => Some(2),
        KEY_3 => Some(3),
        KEY_4 => Some(4),
        KEY_5 => Some(5),
        KEY_6 => Some(6),
        KEY_7 => Some(7),
        KEY_8 => Some(8),
        KEY_9 => Some(9),
        _ => None,
    }
}

fn launcher_character(code: u16) -> Option<char> {
    match code {
        KEY_A => Some('a'),
        KEY_B => Some('b'),
        KEY_C => Some('c'),
        KEY_D => Some('d'),
        KEY_E => Some('e'),
        KEY_F => Some('f'),
        KEY_G => Some('g'),
        KEY_H => Some('h'),
        KEY_I => Some('i'),
        KEY_J => Some('j'),
        KEY_K => Some('k'),
        KEY_L => Some('l'),
        KEY_M => Some('m'),
        KEY_N => Some('n'),
        KEY_O => Some('o'),
        KEY_P => Some('p'),
        KEY_Q => Some('q'),
        KEY_R => Some('r'),
        KEY_S => Some('s'),
        KEY_T => Some('t'),
        KEY_U => Some('u'),
        KEY_V => Some('v'),
        KEY_W => Some('w'),
        KEY_X => Some('x'),
        KEY_Y => Some('y'),
        KEY_Z => Some('z'),
        KEY_1 => Some('1'),
        KEY_2 => Some('2'),
        KEY_3 => Some('3'),
        KEY_4 => Some('4'),
        KEY_5 => Some('5'),
        KEY_6 => Some('6'),
        KEY_7 => Some('7'),
        KEY_8 => Some('8'),
        KEY_9 => Some('9'),
        KEY_0 => Some('0'),
        KEY_MINUS => Some('-'),
        KEY_SPACE => Some(' '),
        _ => None,
    }
}

fn read_u16(bytes: &[u8]) -> Result<u16, String> {
    let raw: [u8; 2] = bytes
        .get(..2)
        .ok_or_else(|| "truncated input u16".to_string())?
        .try_into()
        .map_err(|_| "truncated input u16".to_string())?;
    Ok(u16::from_ne_bytes(raw))
}

fn read_i32(bytes: &[u8]) -> Result<i32, String> {
    let raw: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "truncated input i32".to_string())?
        .try_into()
        .map_err(|_| "truncated input i32".to_string())?;
    Ok(i32::from_ne_bytes(raw))
}

fn read_i64(bytes: &[u8]) -> Result<i64, String> {
    let raw: [u8; 8] = bytes
        .get(..8)
        .ok_or_else(|| "truncated input i64".to_string())?
        .try_into()
        .map_err(|_| "truncated input i64".to_string())?;
    Ok(i64::from_ne_bytes(raw))
}

fn event_time(bytes: &[u8]) -> Result<u32, String> {
    let seconds = read_i64(bytes)?;
    let micros = read_i64(
        bytes
            .get(8..16)
            .ok_or_else(|| "input_event lacks microseconds".to_string())?,
    )?;
    let seconds = seconds.max(0);
    let micros = micros.clamp(0, 999_999);
    let millis = i128::from(seconds) * 1_000 + i128::from(micros / 1_000);
    let modulo = i128::from(u32::MAX) + 1;
    u32::try_from(millis % modulo).map_err(|_| "input timestamp conversion failed".to_string())
}

fn parse(bytes: &[u8]) -> Result<Event, String> {
    if bytes.len() != EVENT_SIZE {
        return Err(format!(
            "input_event is {} bytes, expected {EVENT_SIZE}",
            bytes.len()
        ));
    }
    Ok(Event {
        time: event_time(bytes)?,
        kind: read_u16(
            bytes
                .get(16..18)
                .ok_or_else(|| "input_event lacks type".to_string())?,
        )?,
        code: read_u16(
            bytes
                .get(18..20)
                .ok_or_else(|| "input_event lacks code".to_string())?,
        )?,
        value: read_i32(
            bytes
                .get(20..24)
                .ok_or_else(|| "input_event lacks value".to_string())?,
        )?,
    })
}

fn event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn event_paths(input_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(input_dir).map_err(|e| format!("read {}: {e}", input_dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read input entry: {e}"))?;
        let name = entry.file_name();
        if name.to_str().is_some_and(event_name) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(format!("{} has no event devices", input_dir.display()));
    }
    Ok(paths)
}

fn apply<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    pointer: &mut PointerMotion,
    axes: Option<AbsoluteAxes>,
) -> Result<(), String> {
    let frame = pointer.feed(event, axes);
    if event.kind != EV_KEY && frame.is_none() {
        return Ok(());
    }
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    let decision = bindings.feed_device(device, event);
    if decision.command.is_none()
        && decision.launcher.is_none()
        && decision.help.is_none()
        && decision.launch.is_none()
        && decision.forward.is_none()
        && decision.modifiers.is_none()
        && frame.is_none()
    {
        return Ok(());
    }
    // Keep seat decisions and delivery in one cross-device order.
    let mut runtime = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?;
    deliver_key_decision(&mut *runtime, &mut bindings, decision)?;
    if let Some(frame) = frame {
        deliver_pointer_frame(
            &mut *runtime,
            &mut bindings,
            device,
            &frame,
            &pointer.pressed,
        )?;
    }
    Ok(())
}

/// Put one pointer frame through the button bookkeeping, the overlay filter,
/// and the place or distance itself. Two callers: an ordinary report, and the
/// recovery below, which has a position to publish and no event to hang it on.
fn deliver_pointer_frame<T: InputTarget>(
    runtime: &mut T,
    bindings: &mut KeyBindings,
    device: usize,
    frame: &PointerFrame,
    pressed: &BTreeSet<u16>,
) -> Result<(), String> {
    let mut buttons = bindings.pointer_device_changes(device, &frame.buttons, frame.time);
    if bindings.launcher_open || bindings.help_open {
        buttons.retain(|button| button.state == PointerButtonState::Released);
    }
    let delivery = match frame.place {
        PointerPlace::By { dx, dy } if dx == 0 && dy == 0 && buttons.is_empty() => Ok(()),
        PointerPlace::By { dx, dy } => runtime.pointer_frame(frame.time, dx, dy, &buttons),
        PointerPlace::At { x, y } => runtime.pointer_frame_at(frame.time, x, y, &buttons),
    };
    bindings.commit_pointer_device(device, pressed, &buttons);
    delivery
}

fn deliver_key_decision<T: InputTarget>(
    runtime: &mut T,
    bindings: &mut KeyBindings,
    decision: KeyDecision,
) -> Result<(), String> {
    if let Some(command) = decision.command {
        runtime.command(command)?;
    }
    if let Some(action) = decision.launcher {
        let visible = runtime.launcher(action)?;
        bindings.settle_launcher(Some(visible));
    }
    if let Some(action) = decision.help {
        let visible = runtime.help(action)?;
        bindings.settle_help(Some(visible));
    }
    if let Some(request) = decision.launch {
        runtime.launch(request)?;
    }
    if let Some(input) = decision.forward {
        runtime.key(input)?;
    }
    if let Some(modifiers) = decision.modifiers {
        runtime.modifiers(modifiers)?;
    }
    Ok(())
}

fn retain_failure(failure: &mut Option<String>, error: String) {
    match failure {
        Some(current) => {
            current.push_str("; ");
            current.push_str(&error);
        }
        None => *failure = Some(error),
    }
}

fn deliver_key_cleanup<T: InputTarget>(
    target: &mut T,
    decision: KeyDecision,
    failure: &mut Option<String>,
) {
    if let Some(input) = decision.forward {
        if let Err(error) = target.key(input) {
            retain_failure(failure, error);
        }
    }
    if let Some(modifiers) = decision.modifiers {
        if let Err(error) = target.modifiers(modifiers) {
            retain_failure(failure, error);
        }
    }
}

fn release_device<T: InputTarget>(
    runtime: &Mutex<T>,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    time: u32,
) -> Result<(), String> {
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    let codes: Vec<u16> = bindings
        .pressed
        .iter()
        .filter_map(|(owner, code)| (*owner == device).then_some(*code))
        .collect();
    let had_pointer_state = bindings
        .pointer_pressed
        .iter()
        .any(|(owner, _)| *owner == device);
    if codes.is_empty() && !had_pointer_state && bindings.pointer_forwarded.is_empty() {
        return Ok(());
    }
    let mut failure = None;
    let mut runtime = match runtime.lock() {
        Ok(runtime) => Some(runtime),
        Err(_) => {
            retain_failure(&mut failure, "runtime lock poisoned".to_string());
            None
        }
    };
    for code in codes {
        let decision = bindings.feed_device(
            device,
            Event {
                time,
                kind: EV_KEY,
                code,
                value: KEY_RELEASE,
            },
        );
        if let Some(target) = runtime.as_deref_mut() {
            deliver_key_cleanup(target, decision, &mut failure);
        }
    }
    bindings.remove_pointer_device(device);
    let mut buttons = bindings.pointer_changes(time);
    buttons.retain(|button| button.state == PointerButtonState::Released);
    if !buttons.is_empty() {
        let delivery = runtime
            .as_deref_mut()
            .map(|target| target.pointer_frame(time, 0, 0, &buttons));
        bindings.commit_pointer(&buttons);
        if let Some(Err(error)) = delivery {
            retain_failure(&mut failure, error);
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// What a reader carries between one device's events: the frame being
/// accumulated, whether a dropped batch is still being discarded, what the
/// device said about its absolute axes, and how to ask it again.
struct DeviceState<'a> {
    pointer: PointerMotion,
    dropped: bool,
    axes: Option<AbsoluteAxes>,
    resync: &'a mut dyn FnMut() -> Option<AbsoluteAxes>,
}

impl DeviceState<'_> {
    fn new(
        axes: Option<AbsoluteAxes>,
        resync: &mut dyn FnMut() -> Option<AbsoluteAxes>,
    ) -> DeviceState<'_> {
        DeviceState {
            pointer: PointerMotion::default(),
            dropped: false,
            axes,
            resync,
        }
    }

    /// The recovery boundary, and the one moment the device can be asked.
    /// `SYN_DROPPED` means reports were lost, and the kernel does not re-send
    /// an axis it believes unchanged — it compares against the value IT last
    /// emitted, not the one that arrived — so an axis that moved inside the
    /// gap would stay stale until it moved again. Only the device still knows.
    /// A resync that fails leaves the held position standing, which is the
    /// best remaining answer rather than a jump back to the position at open.
    ///
    /// Answers the frame that PUBLISHES the fresh position, because nothing
    /// else will: from here the kernel sends only changes, so a device that
    /// moved during the gap and then stopped would leave the cursor wherever
    /// it was until it happened to move again. Buttonless — the drop already
    /// released everything this device held.
    fn recover(&mut self, time: u32) -> Option<PointerFrame> {
        self.axes?;
        let fresh = (self.resync)()?;
        self.axes = Some(fresh);
        self.pointer.hold(fresh.x.value, fresh.y.value);
        Some(PointerFrame {
            time,
            place: PointerPlace::At {
                x: AbsoluteAxes::fraction(fresh.x, fresh.x.value),
                y: AbsoluteAxes::fraction(fresh.y, fresh.y.value),
            },
            buttons: Vec::new(),
        })
    }
}

fn apply_device_event<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    state: &mut DeviceState<'_>,
) -> Result<(), String> {
    if state.dropped {
        if event.kind == EV_SYN && event.code == SYN_REPORT {
            state.dropped = false;
            if let Some(frame) = state.recover(event.time) {
                // Same lock order as `apply`: bindings, then the runtime.
                let mut bindings = bindings
                    .lock()
                    .map_err(|_| "input bindings lock poisoned".to_string())?;
                let mut runtime = runtime
                    .lock()
                    .map_err(|_| "runtime lock poisoned".to_string())?;
                deliver_pointer_frame(
                    &mut *runtime,
                    &mut bindings,
                    device,
                    &frame,
                    &state.pointer.pressed,
                )?;
            }
        }
        return Ok(());
    }
    if event.kind == EV_SYN && event.code == SYN_DROPPED {
        state.pointer.reset();
        release_device(runtime, device, bindings, event.time)?;
        state.dropped = true;
        return Ok(());
    }
    apply(
        runtime,
        event,
        device,
        bindings,
        &mut state.pointer,
        state.axes,
    )?;
    if state.pointer.overflowed {
        state.pointer.reset();
        release_device(runtime, device, bindings, event.time)?;
        state.dropped = true;
    }
    Ok(())
}

/// Move the bytes after `consumed` to the front, returning how many were kept.
/// Evdev hands out whole records, so this only ever carries a short tail. Both
/// bounds are clamped so `copy_within` cannot be handed a range that panics.
fn carry_remainder(buffer: &mut [u8], consumed: usize, filled: usize) -> usize {
    let filled = filled.min(buffer.len());
    let consumed = consumed.min(filled);
    let kept = filled.saturating_sub(consumed);
    if kept > 0 && consumed > 0 {
        buffer.copy_within(consumed..filled, 0);
    }
    kept
}

fn flush_target<T: InputTarget>(target: &Mutex<T>) -> Result<(), String> {
    target
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?
        .flush()
}

fn read_device<T: InputTarget>(
    path: &Path,
    file: &mut impl Read,
    device: usize,
    target: &Mutex<T>,
    bindings: &Mutex<KeyBindings>,
    axes: Option<AbsoluteAxes>,
    resync: &mut dyn FnMut() -> Option<AbsoluteAxes>,
) -> Result<(), String> {
    let mut buffer = [0u8; READ_BATCH_BYTES];
    let mut filled = 0usize;
    // The boot oracle's only evidence that a real device answered, since the
    // gate machine has none to ask. Printed off the argument `state` is built
    // from rather than beside the `EVIOCGABS` in `start`: being ASKED is not
    // the property, being USED is, and an answer dropped between the two would
    // leave this line printed over a device read as relative.
    if let Some(axes) = axes {
        eprintln!(
            "TD-POINTER-ABSOLUTE device={} x={}..{} y={}..{}",
            path.display(),
            axes.x.minimum,
            axes.x.maximum,
            axes.y.minimum,
            axes.y.maximum
        );
    }
    let mut state = DeviceState::new(axes, resync);
    let mut last_time = 0;
    let result = loop {
        // An empty tail, not just an out-of-range one: `get_mut(len..)` yields
        // `Some(&mut [])`, and reading into that returns `Ok(0)`, which the
        // arm below cannot tell from the device closing. A reader that retired
        // silently is exactly what this refuses to do.
        let tail = match buffer.get_mut(filled..) {
            Some(tail) if !tail.is_empty() => tail,
            _ => break Err(format!("input {} overran its batch buffer", path.display())),
        };
        let read = match file.read(tail) {
            Ok(0) => break Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(format!("read input {}: {error}", path.display())),
        };
        filled = filled.saturating_add(read);
        let records = filled / EVENT_SIZE;
        let mut failure = None;
        for index in 0..records {
            let at = index.saturating_mul(EVENT_SIZE);
            let Some(record) = buffer.get(at..at.saturating_add(EVENT_SIZE)) else {
                failure = Some(format!("input {} lost a record", path.display()));
                break;
            };
            let event = match parse(record) {
                Ok(event) => event,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            last_time = event.time;
            if let Err(error) = apply_device_event(target, event, device, bindings, &mut state) {
                failure = Some(error);
                break;
            }
        }
        filled = carry_remainder(&mut buffer, records.saturating_mul(EVENT_SIZE), filled);
        if let Some(error) = failure {
            break Err(error);
        }
        // One paint for the whole batch: while the compositor was painting, the
        // kernel queued these reports, and only the last one is on screen now.
        // A read too short to complete a record owes nothing, so it takes no
        // lock.
        if records > 0 {
            if let Err(error) = flush_target(target) {
                break Err(error);
            }
        }
    };
    // Both run: a release that failed is the case where the screen is most
    // likely stale, so it must not be the case that skips the final paint.
    let cleanup = match (
        release_device(target, device, bindings, last_time),
        flush_target(target),
    ) {
        (Ok(()), flushed) => flushed,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(flush_error)) => Err(format!("{error}; final paint: {flush_error}")),
    };
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            Err(format!("{error}; release input state: {cleanup_error}"))
        }
    }
}

/// Ask a device whether it reports an absolute position, and over what span.
///
/// Asked at open, and again only at a recovery — after a dropped batch or a
/// button overflow, which discard alike. The SPAN is a property
/// of the device, so asking per frame would be a syscall per motion for an
/// answer that cannot change; the `value` beside it is not, and a
/// discarded report is the one moment it can have moved without a report saying
/// so. Both callers hand it a real `File`, which `read_device` cannot: it
/// takes an `impl Read` so its tests can drive it from a byte slice, and a
/// slice has no descriptor to ask.
///
/// A device that refuses either axis is RELATIVE, not broken: an evdev node
/// with no absinfo table at all answers `EINVAL`, which is what an ordinary
/// mouse is and not worth a diagnostic. One that HAS the table answers for
/// every axis, zeroed where the device has none — so the refusal is not the
/// whole test, and `declared`'s span is what actually separates the two.
fn absolute_axes(device: &File) -> Option<AbsoluteAxes> {
    let x = sys::absolute_info(device, sys::AbsAxis::X).ok()?;
    let y = sys::absolute_info(device, sys::AbsAxis::Y).ok()?;
    AbsoluteAxes::declared(x, y)
}

pub fn start(
    input_dir: &Path,
    runtime: Arc<Mutex<Runtime>>,
    launch_options: LaunchOptions,
) -> Result<usize, String> {
    let launches = LaunchProcesses::new(launch_options)?;
    let paths = event_paths(input_dir)?;
    let bindings = Arc::new(Mutex::new(KeyBindings::default()));
    let target = Arc::new(Mutex::new(LiveInputTarget { runtime, launches }));
    for (device, path) in paths.iter().enumerate() {
        let mut file =
            File::open(path).map_err(|e| format!("open input {}: {e}", path.display()))?;
        let axes = absolute_axes(&file);
        // A second handle purely so a dropped batch can ask the device where
        // it is now. The reader takes an `impl Read` so its tests can drive it
        // from a byte slice, and a slice has no descriptor to ask.
        //
        // `try_clone` rather than opening the node again: it is `dup(2)`, so
        // both handles are the same open file and the same evdev CLIENT. A
        // second `open` would make a second client with a buffer of its own,
        // and the kernel writes every event to every client — so the reader
        // would be racing a queue nothing drains, which is what produces the
        // dropped batches this exists to recover from.
        let resync_handle = axes.and_then(|_| match file.try_clone() {
            Ok(handle) => Some(handle),
            // Reported rather than swallowed: the device still works, but a
            // dropped batch can no longer be recovered from, and every other
            // failure on this path says so.
            Err(error) => {
                eprintln!(
                    "td-compositor: no resync handle for {}: {error}",
                    path.display()
                );
                None
            }
        });
        let path = path.clone();
        let label = path.display().to_string();
        let target = Arc::clone(&target);
        let bindings = Arc::clone(&bindings);
        thread::Builder::new()
            .name(format!(
                "input-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("event")
            ))
            .spawn(move || {
                let mut resync = move || resync_handle.as_ref().and_then(absolute_axes);
                if let Err(error) = read_device(
                    &path,
                    &mut file,
                    device,
                    target.as_ref(),
                    bindings.as_ref(),
                    axes,
                    &mut resync,
                ) {
                    eprintln!("td-compositor: {error}");
                }
            })
            .map_err(|e| format!("spawn input reader for {label}: {e}"))?;
    }
    Ok(paths.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn key(code: u16, value: i32) -> Event {
        Event {
            time: 0,
            kind: EV_KEY,
            code,
            value,
        }
    }

    /// One axis of QEMU's `virtio-tablet-pci`, which reports 0..=32767 and is
    /// the device the image attaches. `value` is where the kernel says it is
    /// now.
    fn axis(value: i32, minimum: i32, maximum: i32) -> sys::AbsInfo {
        sys::AbsInfo {
            value,
            minimum,
            maximum,
        }
    }

    fn tablet() -> AbsoluteAxes {
        AbsoluteAxes {
            x: axis(0, 0, 32767),
            y: axis(0, 0, 32767),
        }
    }

    /// The ratio a place crosses as. Written out at every assertion rather
    /// than reduced, because the whole claim is that nothing rescales it.
    fn over(numerator: u32, denominator: u32) -> Fraction {
        Fraction {
            numerator,
            denominator,
        }
    }

    fn abs(time: u32, code: u16, value: i32) -> Event {
        Event {
            time,
            kind: EV_ABS,
            code,
            value,
        }
    }

    fn syn(time: u32) -> Event {
        Event {
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }
    }

    #[test]
    fn an_absolute_report_names_a_place_and_a_relative_one_a_distance() {
        let even = Some(AbsoluteAxes {
            x: axis(0, 0, 1000),
            y: axis(0, 0, 1000),
        });
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed(abs(1, ABS_X, 500), even), None);
        assert_eq!(pointer.feed(abs(1, ABS_Y, 0), even), None);
        let frame = pointer.feed(syn(1), even).unwrap();
        // Halfway along, and hard against the near edge — as the device's own
        // numbers, neither reduced nor rescaled.
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(500, 1000),
                y: over(0, 1000)
            }
        );

        // The far edge is reported EXACTLY, which is the whole complaint an
        // absolute pointer answers: a relative one on a warped host cursor
        // cannot be relied on to arrive at the last column.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed(abs(2, ABS_X, 32767), axes), None);
        assert_eq!(pointer.feed(abs(2, ABS_Y, 32767), axes), None);
        let frame = pointer.feed(syn(2), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(32767, 32767),
                y: over(32767, 32767)
            }
        );

        // The SAME events on a device that declared no range are not a
        // position at all: without a span there is nothing to scale against,
        // and a raw 32767 read as a pixel is thousands of columns off screen.
        let mut relative = PointerMotion::default();
        assert_eq!(relative.feed(abs(3, ABS_X, 32767), None), None);
        assert_eq!(
            relative.feed(syn(3), None),
            None,
            "an absolute report moved a device with no absolute axes"
        );
    }

    #[test]
    fn an_axis_a_report_leaves_out_is_where_that_device_last_was() {
        // The kernel drops an axis whose value has not changed, so a stylus
        // moved along one axis reports only that one. Answering the other from
        // the CURSOR would be wrong the moment anything else moved it, which is
        // why the reader holds the device's own position.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(8000, 32767),
                y: over(4000, 32767)
            }
        );
        // X alone, and the row is still the one the stylus is on.
        pointer.feed(abs(2, ABS_X, 9000), axes);
        assert_eq!(
            pointer.feed(syn(2), axes).unwrap().place,
            PointerPlace::At {
                x: over(9000, 32767),
                y: over(4000, 32767)
            }
        );
        // Y alone, likewise.
        pointer.feed(abs(3, ABS_Y, 5000), axes);
        assert_eq!(
            pointer.feed(syn(3), axes).unwrap().place,
            PointerPlace::At {
                x: over(9000, 32767),
                y: over(5000, 32767)
            }
        );
    }

    #[test]
    fn a_first_report_completes_itself_from_the_position_the_kernel_gave() {
        // Before any report there is no held position, and `input_absinfo`'s
        // `value` is the only account of where the device is. Without it a
        // one-axis first report would place the other axis at the near edge,
        // which is a corner of the screen the stylus is not in.
        let axes = Some(AbsoluteAxes {
            x: axis(1000, 0, 32767),
            y: axis(24000, 0, 32767),
        });
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 16000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(16000, 32767),
                y: over(24000, 32767)
            }
        );

        // The other way round, because the two arms are separate lines and
        // one reading the OTHER axis's `value` would survive the case above.
        // The seeds differ from each other and from both reported values.
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_Y, 16000), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(1000, 32767),
                y: over(16000, 32767)
            }
        );

        // And a device whose FIRST frame is a button alone — a stylus already
        // resting where it was left, tapped without moving.
        let mut pointer = PointerMotion::default();
        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        assert_eq!(
            pointer.feed(syn(1), axes).unwrap().place,
            PointerPlace::At {
                x: over(1000, 32767),
                y: over(24000, 32767)
            }
        );
    }

    /// `absolute_axes` needs a real descriptor, so the gate can only ask it
    /// about something that is not an evdev device. What that excludes is a
    /// failed ioctl read as a POSITIVE span; a zeroed one `declared` refuses
    /// anyway, so this is the outer half of a defence whose inner half is
    /// `a_device_whose_axes_declare_no_span_is_relative`.
    #[test]
    fn a_file_that_is_not_an_evdev_device_declares_no_absolute_axes() {
        let file = File::open("/dev/null").unwrap();
        assert_eq!(absolute_axes(&file), None);
    }

    #[test]
    fn a_position_wins_over_a_delta_in_the_same_report() {
        // A device that sends both means the PLACE; the deltas are its own
        // account of the way there, and adding them would move the pointer
        // twice for one report.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(
            Event {
                time: 4,
                kind: EV_REL,
                code: REL_X,
                value: 40,
            },
            axes,
        );
        pointer.feed(abs(4, ABS_X, 8192), axes);
        let frame = pointer.feed(syn(4), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(8192, 32767),
                y: over(0, 32767)
            }
        );
    }

    #[test]
    fn an_absolute_value_outside_the_declared_range_is_the_edge_it_went_past() {
        let axes = tablet();
        assert_eq!(AbsoluteAxes::fraction(axes.x, -5), over(0, 32767));
        assert_eq!(AbsoluteAxes::fraction(axes.x, 999_999), over(32767, 32767));
        // A span of nothing cannot be scaled against, so it answers a zero
        // DENOMINATOR — which `across` reads as the near edge rather than
        // dividing by it. `declared` refuses such a device outright.
        assert_eq!(AbsoluteAxes::fraction(axis(7, 7, 7), 7).denominator, 0);
        assert_eq!(AbsoluteAxes::fraction(axis(5, 9, 1), 5).denominator, 0);
    }

    #[test]
    fn a_range_that_does_not_start_at_zero_is_read_from_its_own_base() {
        // A device may report over a window starting anywhere, and taking the
        // value as it stands would offset every report by that base.
        let shifted = axis(1000, 1000, 3000);
        assert_eq!(AbsoluteAxes::fraction(shifted, 1000), over(0, 2000));
        assert_eq!(AbsoluteAxes::fraction(shifted, 2000), over(1000, 2000));
        assert_eq!(AbsoluteAxes::fraction(shifted, 3000), over(2000, 2000));
        // Below the base is the near edge rather than a wrap.
        assert_eq!(AbsoluteAxes::fraction(shifted, 0), over(0, 2000));
    }

    #[test]
    fn each_axis_is_scaled_against_its_own_declared_range() {
        // The two rarely share a range, and one report scaled against the
        // other axis is a well-formed position somewhere else entirely.
        let axes = Some(AbsoluteAxes {
            x: axis(0, 0, 1000),
            y: axis(0, 0, 4000),
        });
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(5, ABS_X, 500), axes);
        pointer.feed(abs(5, ABS_Y, 500), axes);
        let frame = pointer.feed(syn(5), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(500, 1000),
                y: over(500, 4000)
            }
        );
    }

    #[test]
    fn a_device_whose_axes_declare_no_span_is_relative() {
        // The admission asks `fraction`, so a device it lets in is one every
        // later report can actually be placed against.
        let flat = axis(7, 7, 7);
        let real = axis(0, 0, 32767);
        assert_eq!(AbsoluteAxes::declared(flat, real), None);
        assert_eq!(AbsoluteAxes::declared(real, flat), None);
        assert_eq!(AbsoluteAxes::declared(real, real), Some(tablet()));
    }

    #[test]
    fn a_button_alone_from_an_absolute_device_lands_where_that_device_is() {
        // Tapping twice in one spot sends NOTHING but the button: the kernel
        // drops both axes as unchanged. Read as a zero delta, the second tap
        // would click wherever another device last left the shared cursor.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        pointer.feed(syn(1), axes);

        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        let frame = pointer.feed(syn(2), axes).unwrap();
        assert_eq!(
            frame.place,
            PointerPlace::At {
                x: over(8000, 32767),
                y: over(4000, 32767)
            },
            "a button-only frame was not placed where the device is"
        );
        assert_eq!(frame.buttons.len(), 1);

        // The same frame from a RELATIVE device is still a zero delta: there
        // is no position to place it at, and the shared cursor is the answer.
        let mut mouse = PointerMotion::default();
        mouse.feed_relative(key(BTN_MOUSE, KEY_PRESS));
        assert_eq!(
            mouse.feed_relative(syn(2)).unwrap().place,
            PointerPlace::By { dx: 0, dy: 0 }
        );
    }

    #[test]
    fn a_recovery_asks_the_device_once_and_only_where_the_kernel_says_to() {
        // Three properties one batch can prove, and each is a mutation the
        // rest of the suite cannot see: a SECOND `SYN_DROPPED` must not end
        // the discard window (nor may any other EV_SYN code), the device is
        // asked exactly once per recovery rather than per report, and the
        // frame that publishes the answer carries the recovery's own time.
        let drop = |time| Event {
            time,
            kind: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        };
        let mut data = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            drop(2),
            // Inside the window: neither ends it, and the recovery below must
            // still be the FIRST place anything is published.
            drop(3),
            Event {
                time: 4,
                kind: EV_SYN,
                code: 1,
                value: 0,
            },
            abs(5, ABS_X, 999),
            syn(6),
            // After it: an ordinary report, which must ask nothing.
            abs(7, ABS_X, 7000),
            syn(7),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };
        let (target, result, asked) =
            drain_counting(data.clone(), Vec::new(), Some(tablet()), Some(moved), None);
        assert_eq!(result, Ok(()));
        assert_eq!(asked, 1, "the device was asked {asked} times, not once");
        assert_eq!(
            target.pointer_places,
            [
                (1, over(100, 32767), over(100, 32767), Vec::new()),
                // The recovery, at the time of the SYN_REPORT that ended the
                // window — 6, not 3 or 4, which is what an over-eager guard
                // would answer.
                (6, over(20000, 32767), over(30000, 40000), Vec::new()),
                // And on afterwards, the row still the one the resync gave.
                (7, over(7000, 32767), over(30000, 40000), Vec::new()),
            ]
        );

        // A refused delivery on the recovery path is the reader's failure,
        // not something it carries on past. The batch starts AT the drop so
        // the recovery frame is the first delivery there is: with a report
        // before it, that one would consume the injected error and the
        // assertion would hold whatever the recovery did with its own.
        let mut alone = Vec::new();
        for event in [drop(2), syn(6)] {
            alone.extend_from_slice(&encode(event));
        }
        let (_, result, _) = drain_counting(
            alone,
            Vec::new(),
            Some(tablet()),
            Some(moved),
            Some("paint refused".to_string()),
        );
        assert!(result.is_err(), "a refused recovery paint was swallowed");
    }

    #[test]
    fn a_dropped_batch_forgets_the_frame_but_not_where_the_device_is() {
        // `reset` is what a SYN_DROPPED and a button overflow both do. The
        // half-built frame and the buttons go; the device's POSITION is not
        // frame state, and losing it would send the next one-axis report back
        // to wherever the device was when the compositor started.
        let axes = Some(tablet());
        let mut pointer = PointerMotion::default();
        pointer.feed(abs(1, ABS_X, 8000), axes);
        pointer.feed(abs(1, ABS_Y, 4000), axes);
        pointer.feed(syn(1), axes);

        pointer.feed(abs(2, ABS_X, 9000), axes);
        pointer.feed(key(BTN_MOUSE, KEY_PRESS), axes);
        pointer.reset();
        assert!(pointer.buttons.is_empty());
        assert!(pointer.pressed.is_empty());
        assert_eq!(pointer.abs_x, None);
        // The row the stylus is on survived, so an X-only report still names
        // it rather than the 0 the range was opened at.
        pointer.feed(abs(3, ABS_X, 12000), axes);
        assert_eq!(
            pointer.feed(syn(3), axes).unwrap().place,
            PointerPlace::At {
                x: over(12000, 32767),
                y: over(4000, 32767)
            }
        );
    }

    fn press(bindings: &mut KeyBindings, code: u16) -> Option<Command> {
        bindings.feed(key(code, KEY_PRESS)).command
    }

    fn tap(bindings: &mut KeyBindings, code: u16) -> Option<Command> {
        let command = press(bindings, code);
        bindings.feed(key(code, KEY_RELEASE));
        command
    }

    #[derive(Debug, Eq, PartialEq)]
    enum KeyboardCall {
        Key(KeyInput),
        Modifiers(ModifierState),
    }

    /// What an absolute report reached the target as: the time, each axis as
    /// a fraction of the device's own span, and the buttons that came with it.
    type RecordedPlace = (u32, Fraction, Fraction, Vec<PointerButtonInput>);

    #[derive(Default)]
    struct RecordingTarget {
        commands: Vec<Command>,
        launcher_actions: Vec<LauncherAction>,
        launcher_visible: bool,
        keys: Vec<KeyInput>,
        key_error: Option<String>,
        modifiers: Vec<ModifierState>,
        keyboard_calls: Vec<KeyboardCall>,
        pointer_frames: Vec<(u32, i32, i32, Vec<PointerButtonInput>)>,
        pointer_places: Vec<RecordedPlace>,
        pointer_error: Option<String>,
        flushes: usize,
        flush_error: Option<String>,
        launched: Vec<LaunchRequest>,
        help_actions: Vec<HelpAction>,
        help: crate::help::Help,
    }

    impl InputTarget for RecordingTarget {
        fn command(&mut self, command: Command) -> Result<(), String> {
            self.commands.push(command);
            Ok(())
        }

        fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
            self.launched.push(request);
            Ok(())
        }

        fn help(&mut self, action: HelpAction) -> Result<bool, String> {
            self.help_actions.push(action);
            // The real model, not a second copy of its rule: a fake that
            // drifted would let the adapter test agree with a sheet that no
            // longer behaves this way.
            self.help.set(action.target(self.help.visible()));
            Ok(self.help.visible())
        }

        fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
            self.launcher_actions.push(action);
            match action {
                LauncherAction::Open => self.launcher_visible = true,
                LauncherAction::Close | LauncherAction::Activate => self.launcher_visible = false,
                LauncherAction::Next
                | LauncherAction::Previous
                | LauncherAction::Insert(_)
                | LauncherAction::Backspace => {}
            }
            Ok(self.launcher_visible)
        }

        fn key(&mut self, input: KeyInput) -> Result<(), String> {
            self.keys.push(input);
            self.keyboard_calls.push(KeyboardCall::Key(input));
            match self.key_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
            self.modifiers.push(modifiers);
            self.keyboard_calls.push(KeyboardCall::Modifiers(modifiers));
            Ok(())
        }

        fn pointer_frame(
            &mut self,
            time: u32,
            dx: i32,
            dy: i32,
            buttons: &[PointerButtonInput],
        ) -> Result<(), String> {
            self.pointer_frames.push((time, dx, dy, buttons.to_vec()));
            match self.pointer_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn pointer_frame_at(
            &mut self,
            time: u32,
            x: Fraction,
            y: Fraction,
            buttons: &[PointerButtonInput],
        ) -> Result<(), String> {
            // Kept apart from the relative list on purpose: a test that
            // asserted a tablet report as a delta would be reading the wrong
            // question answered the wrong way.
            self.pointer_places.push((time, x, y, buttons.to_vec()));
            match self.pointer_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }

        fn flush(&mut self) -> Result<(), String> {
            self.flushes = self.flushes.saturating_add(1);
            match self.flush_error.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    struct LauncherModelTarget {
        launcher: crate::launcher::Launcher,
        recording: RecordingTarget,
    }

    impl LauncherModelTarget {
        fn new() -> Self {
            Self {
                launcher: crate::launcher::Launcher::new(),
                recording: RecordingTarget::default(),
            }
        }
    }

    impl InputTarget for LauncherModelTarget {
        fn command(&mut self, command: Command) -> Result<(), String> {
            self.recording.command(command)
        }

        fn launch(&mut self, request: LaunchRequest) -> Result<(), String> {
            self.recording.launch(request)
        }

        fn help(&mut self, action: HelpAction) -> Result<bool, String> {
            self.recording.help(action)
        }

        fn launcher(&mut self, action: LauncherAction) -> Result<bool, String> {
            self.recording.launcher_actions.push(action);
            self.launcher.apply(action);
            Ok(self.launcher.visible())
        }

        fn key(&mut self, input: KeyInput) -> Result<(), String> {
            self.recording.key(input)
        }

        fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
            self.recording.modifiers(modifiers)
        }

        fn pointer_frame(
            &mut self,
            time: u32,
            dx: i32,
            dy: i32,
            buttons: &[PointerButtonInput],
        ) -> Result<(), String> {
            self.recording.pointer_frame(time, dx, dy, buttons)
        }

        fn pointer_frame_at(
            &mut self,
            time: u32,
            x: Fraction,
            y: Fraction,
            buttons: &[PointerButtonInput],
        ) -> Result<(), String> {
            self.recording.pointer_frame_at(time, x, y, buttons)
        }

        fn flush(&mut self) -> Result<(), String> {
            self.recording.flush()
        }
    }

    fn encode(event: Event) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(EVENT_SIZE);
        bytes.extend_from_slice(&i64::from(event.time / 1_000).to_ne_bytes());
        bytes.extend_from_slice(&(i64::from(event.time % 1_000) * 1_000).to_ne_bytes());
        bytes.extend_from_slice(&event.kind.to_ne_bytes());
        bytes.extend_from_slice(&event.code.to_ne_bytes());
        bytes.extend_from_slice(&event.value.to_ne_bytes());
        bytes
    }

    #[test]
    fn a_tablet_reaches_the_target_as_a_place_and_a_mouse_as_a_distance() {
        // End to end through the reader, which is the join this feature is:
        // the range comes from the device at open, the events come off the
        // wire, and what the compositor is told has to be a POSITION.
        let mut data = Vec::new();
        for event in [abs(1, ABS_X, 32767), abs(1, ABS_Y, 0), syn(1)] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_device(data.clone(), Vec::new(), Some(tablet()));
        assert_eq!(result, Ok(()));
        assert!(
            target.pointer_frames.is_empty(),
            "a tablet was reported as a delta"
        );
        assert_eq!(
            target.pointer_places,
            [(1, over(32767, 32767), over(0, 32767), Vec::new())]
        );

        // The same bytes from a device that answered no range reach nobody:
        // there is nothing to scale against, and a raw value taken as a
        // delta would fling the pointer across the screen.
        let (target, result) = drain_device(data, Vec::new(), None);
        assert_eq!(result, Ok(()));
        assert!(target.pointer_places.is_empty());
        assert!(target.pointer_frames.is_empty());

        // A dropped batch re-asks the device, and the answer replaces the
        // position rather than being merged with it. Only the device knows
        // where it went while its reports were being discarded: the kernel
        // compares an axis against the value IT last emitted, so one that
        // moved inside the gap is never re-sent.
        let mut data = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            Event {
                time: 2,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            syn(2),
            key(BTN_MOUSE, KEY_PRESS),
            syn(3),
        ] {
            data.extend_from_slice(&encode(event));
        }
        // Deliberately DIFFERENT spans. The recovery composes its own pair of
        // fractions, a second call site for the axis mapping, and one scaled
        // against the other axis is a well-formed position somewhere else —
        // which a fixture sharing one range cannot tell from the right answer.
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };

        // The recovery PUBLISHES the fresh position rather than only caching
        // it. This batch ends at the resynchronising SYN_REPORT, so nothing
        // after it can consume the re-read: a device that moved during the gap
        // and then stopped must still reach the screen.
        let mut quiet = Vec::new();
        for event in [
            abs(1, ABS_X, 100),
            abs(1, ABS_Y, 100),
            syn(1),
            Event {
                time: 2,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            syn(2),
        ] {
            quiet.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_resyncing(quiet, Vec::new(), Some(tablet()), Some(moved));
        assert_eq!(result, Ok(()));
        assert_eq!(
            target.pointer_places.last(),
            Some(&(2, over(20000, 32767), over(30000, 40000), Vec::new())),
            "a recovery re-read the device and told nobody"
        );
        let (target, result) =
            drain_resyncing(data.clone(), Vec::new(), Some(tablet()), Some(moved));
        assert_eq!(result, Ok(()));
        let (_, x, y, buttons) = target.pointer_places.last().unwrap();
        assert_eq!(
            (*x, *y),
            (over(20000, 32767), over(30000, 40000)),
            "a recovered device was placed at where it used to be"
        );
        // The press rides the frame that placed it. Asserted because this is
        // the only path that carries one: once a device declares axes, an
        // absolute frame is the ONLY way a button of its reaches the target.
        assert_eq!(
            buttons,
            &vec![PointerButtonInput {
                button: u32::from(BTN_MOUSE),
                state: PointerButtonState::Pressed,
                time: 3,
            }]
        );

        // A device that cannot be asked keeps what it last said, which beats
        // the position it was opened at even though it may be stale. This is
        // also what pins the RESET rather than `reset` itself: a recovery that
        // rebuilt the whole `PointerMotion` would answer 0 here, the value the
        // range was opened at, and no test of `reset` alone can see which of
        // the two a call site made.
        let (target, result) = drain_resyncing(data, Vec::new(), Some(tablet()), None);
        assert_eq!(result, Ok(()));
        let (_, x, y, _) = target.pointer_places.last().unwrap();
        assert_eq!((*x, *y), (over(100, 32767), over(100, 32767)));

        // And the other half of the name: an ORDINARY MOUSE over the same
        // reader is a distance, on the list a tablet never reaches.
        let mut data = Vec::new();
        for event in [
            Event {
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 7,
            },
            Event {
                time: 2,
                kind: EV_REL,
                code: REL_Y,
                value: -3,
            },
            syn(2),
        ] {
            data.extend_from_slice(&encode(event));
        }
        let (target, result) = drain_device(data, Vec::new(), None);
        assert_eq!(result, Ok(()));
        assert!(
            target.pointer_places.is_empty(),
            "a mouse was reported as a place"
        );
        assert_eq!(target.pointer_frames, [(2, 7, -3, Vec::new())]);
    }

    fn motion(time: u32, dx: i32) -> Vec<u8> {
        let mut bytes = encode(Event {
            time,
            kind: EV_REL,
            code: REL_X,
            value: dx,
        });
        bytes.extend_from_slice(&encode(Event {
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        }));
        bytes
    }

    /// A reader that hands out a scripted sequence of short reads, the way a
    /// character device may but a regular file never does.
    struct ChunkedReader {
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        at: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunks: Vec<std::io::Result<usize>>) -> Self {
            Self {
                data,
                chunks,
                at: 0,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let want = if self.chunks.is_empty() {
                self.data.len() - self.at
            } else {
                self.chunks.remove(0)?
            };
            let count = want.min(self.data.len() - self.at).min(buffer.len());
            buffer[..count].copy_from_slice(&self.data[self.at..self.at + count]);
            self.at += count;
            Ok(count)
        }
    }

    fn drain(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
    ) -> (RecordingTarget, Result<(), String>) {
        drain_device(data, chunks, None)
    }

    fn drain_device(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
    ) -> (RecordingTarget, Result<(), String>) {
        drain_resyncing(data, chunks, axes, None)
    }

    /// `resync` is what a real device answers after a dropped batch. `None`
    /// stands for a device that could not be asked, which is every reader
    /// driven from a byte slice.
    fn drain_resyncing(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
        resync: Option<AbsoluteAxes>,
    ) -> (RecordingTarget, Result<(), String>) {
        let (target, result, _) = drain_counting(data, chunks, axes, resync, None);
        (target, result)
    }

    /// The same, counting how many times the device was asked — the property
    /// `absolute_axes` claims and nothing else could check, since a closure
    /// answering a constant looks the same whether it ran once or every frame.
    fn drain_counting(
        data: Vec<u8>,
        chunks: Vec<std::io::Result<usize>>,
        axes: Option<AbsoluteAxes>,
        resync: Option<AbsoluteAxes>,
        pointer_error: Option<String>,
    ) -> (RecordingTarget, Result<(), String>, usize) {
        let target = Mutex::new(RecordingTarget::default());
        target
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pointer_error = pointer_error;
        let bindings = Mutex::new(KeyBindings::default());
        let mut reader = ChunkedReader::new(data, chunks);
        let asked = std::cell::Cell::new(0usize);
        let mut resync = || {
            asked.set(asked.get().saturating_add(1));
            resync
        };
        let result = read_device(
            Path::new("event-test"),
            &mut reader,
            0,
            &target,
            &bindings,
            axes,
            &mut resync,
        );
        (
            target
                .into_inner()
                .unwrap_or_else(|error| error.into_inner()),
            result,
            asked.get(),
        )
    }

    #[test]
    fn carry_remainder_moves_only_the_tail() {
        let mut buffer = [1u8, 2, 3, 4, 5, 6];
        assert_eq!(carry_remainder(&mut buffer, 4, 6), 2);
        assert_eq!(&buffer[..2], &[5, 6]);
        assert_eq!(carry_remainder(&mut buffer, 2, 2), 0);
        assert_eq!(carry_remainder(&mut buffer, 0, 3), 3);
        assert_eq!(&buffer[..3], &[5, 6, 3]);
        // `read_device` consumes a whole multiple of a record and so cannot ask
        // for more than it filled; out of that domain the bounds are clamped
        // rather than allowed to panic inside `copy_within`.
        assert_eq!(carry_remainder(&mut buffer, 9, 3), 0);
        assert_eq!(carry_remainder(&mut buffer, 1, 99), 5);
    }

    #[test]
    fn a_batch_of_reports_costs_one_flush_not_one_per_report() {
        let mut data = Vec::new();
        for step in 0..32u32 {
            data.extend_from_slice(&motion(step, 1));
        }
        let (target, result) = drain(data, Vec::new());
        assert_eq!(result, Ok(()));
        // Every report still reaches the seat -- clients see the motion path.
        assert_eq!(target.pointer_frames.len(), 32);
        // One paint for the batch, plus the teardown flush after EOF.
        assert_eq!(target.flushes, 2);
    }

    #[test]
    fn a_report_that_arrives_alone_is_painted_without_waiting() {
        let (target, result) = drain(motion(1, 1), vec![Ok(EVENT_SIZE), Ok(EVENT_SIZE)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames.len(), 1);
        // The bare motion read flushes nothing to paint; the report's own read
        // flushes it, and EOF flushes again.
        assert_eq!(target.flushes, 3);
    }

    #[test]
    fn a_record_split_across_reads_is_carried_to_the_next() {
        let data = motion(4, 3);
        let (target, result) = drain(data, vec![Ok(EVENT_SIZE + 7), Ok(EVENT_SIZE - 7)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames, vec![(4, 3, 0, Vec::new())]);
    }

    #[test]
    fn an_interrupted_read_resumes_the_batch() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "signal");
        let (target, result) = drain(motion(6, 5), vec![Err(interrupted)]);
        assert_eq!(result, Ok(()));
        assert_eq!(target.pointer_frames, vec![(6, 5, 0, Vec::new())]);
    }

    #[test]
    fn a_flush_failure_closes_the_device_after_releasing_its_pressed_buttons() {
        let target = Mutex::new(RecordingTarget::default());
        target.lock().unwrap().flush_error = Some("paint refused".to_string());
        let bindings = Mutex::new(KeyBindings::default());
        let mut data = encode(Event {
            time: 1,
            kind: EV_KEY,
            code: BTN_MOUSE,
            value: KEY_PRESS,
        });
        data.extend_from_slice(&motion(1, 2));
        let mut reader = ChunkedReader::new(data, Vec::new());
        let result = read_device(
            Path::new("event-test"),
            &mut reader,
            0,
            &target,
            &bindings,
            None,
            &mut || None,
        );
        assert_eq!(result, Err("paint refused".to_string()));
        assert!(bindings.lock().unwrap().pointer_pressed.is_empty());
        let target = target.into_inner().unwrap();
        let released = target
            .pointer_frames
            .last()
            .map(|(_, _, _, buttons)| buttons.clone())
            .unwrap_or_default();
        assert_eq!(
            released
                .iter()
                .map(|button| (button.button, button.state))
                .collect::<Vec<_>>(),
            vec![(u32::from(BTN_MOUSE), PointerButtonState::Released)]
        );
    }

    #[test]
    fn parses_x86_64_input_event_tail() {
        let mut bytes = [0u8; EVENT_SIZE];
        bytes
            .get_mut(..8)
            .unwrap()
            .copy_from_slice(&12i64.to_ne_bytes());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&345_000i64.to_ne_bytes());
        bytes
            .get_mut(16..18)
            .unwrap()
            .copy_from_slice(&EV_KEY.to_ne_bytes());
        bytes
            .get_mut(18..20)
            .unwrap()
            .copy_from_slice(&KEY_RIGHT.to_ne_bytes());
        bytes
            .get_mut(20..24)
            .unwrap()
            .copy_from_slice(&KEY_PRESS.to_ne_bytes());
        assert_eq!(
            parse(&bytes).unwrap(),
            Event {
                time: 12_345,
                kind: EV_KEY,
                code: KEY_RIGHT,
                value: KEY_PRESS
            }
        );
        assert!(parse(bytes.get(..23).unwrap()).is_err());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&1_000_000i64.to_ne_bytes());
        assert_eq!(parse(&bytes).unwrap().time, 12_999);
        bytes
            .get_mut(..8)
            .unwrap()
            .copy_from_slice(&(-1i64).to_ne_bytes());
        bytes
            .get_mut(8..16)
            .unwrap()
            .copy_from_slice(&(-1i64).to_ne_bytes());
        assert_eq!(parse(&bytes).unwrap().time, 0);
    }

    #[test]
    fn input_node_names_are_narrow() {
        assert!(event_name("event0"));
        assert!(!event_name("event"));
        assert!(!event_name("event0-old"));
    }

    #[test]
    fn arrow_keys_focus_and_shift_moves_in_every_direction() {
        for (code, direction) in [
            (KEY_LEFT, Direction::Left),
            (KEY_RIGHT, Direction::Right),
            (KEY_UP, Direction::Up),
            (KEY_DOWN, Direction::Down),
        ] {
            let mut bindings = KeyBindings::default();
            assert_eq!(tap(&mut bindings, code), None);
            assert_eq!(press(&mut bindings, KEY_LEFTMETA), None);
            assert_eq!(tap(&mut bindings, code), Some(Command::Focus(direction)));
            assert_eq!(press(&mut bindings, KEY_RIGHTSHIFT), None);
            assert_eq!(tap(&mut bindings, code), Some(Command::Move(direction)));
            assert_eq!(
                bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE)).command,
                None
            );
            assert_eq!(tap(&mut bindings, code), Some(Command::Focus(direction)));
        }
    }

    #[test]
    fn either_meta_and_shift_are_tracked_until_both_sides_release() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        press(&mut bindings, KEY_RIGHTMETA);
        press(&mut bindings, KEY_LEFTSHIFT);
        press(&mut bindings, KEY_RIGHTSHIFT);
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_RIGHT),
            Some(Command::Move(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_RIGHT),
            Some(Command::Focus(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(tap(&mut bindings, KEY_RIGHT), None);
    }

    #[test]
    fn all_nine_workspace_keys_switch_or_move_the_focused_tile() {
        for (code, number) in [
            (KEY_1, 1),
            (KEY_2, 2),
            (KEY_3, 3),
            (KEY_4, 4),
            (KEY_5, 5),
            (KEY_6, 6),
            (KEY_7, 7),
            (KEY_8, 8),
            (KEY_9, 9),
        ] {
            let mut bindings = KeyBindings::default();
            press(&mut bindings, KEY_RIGHTMETA);
            assert_eq!(
                tap(&mut bindings, code),
                Some(Command::SwitchWorkspace(number))
            );
            press(&mut bindings, KEY_LEFTSHIFT);
            assert_eq!(
                tap(&mut bindings, code),
                Some(Command::MoveToWorkspace(number))
            );
        }
    }

    #[test]
    fn super_chords_select_fullscreen_and_both_split_axes() {
        for (code, expected) in [
            (KEY_F, Command::ToggleFullscreen),
            (KEY_V, Command::SetPresentation(Presentation::Stacked)),
            (KEY_H, Command::SetPresentation(Presentation::Tabbed)),
        ] {
            let mut bindings = KeyBindings::default();
            // Bare, the key is the client's text: only the modifier makes it
            // a command.
            let bare = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(bare.command, None);
            assert!(bare.forward.is_some());
            bindings.feed(key(code, KEY_RELEASE));
            press(&mut bindings, KEY_LEFTMETA);
            assert_eq!(tap(&mut bindings, code), Some(expected));
        }
    }

    /// What a help row's chord actually produces. The sheet is PAINTED text,
    /// so nothing but a test can stop it describing bindings that no longer
    /// exist — the compiler sees two unrelated string literals.
    #[derive(Debug, Eq, PartialEq)]
    enum Bound {
        Command(Command),
        Launcher(LauncherAction),
        Launch(LaunchRequest),
        Help(HelpAction),
        /// Documented but not a key, so this row is exercised elsewhere. The
        /// SPELLING is carried because the mouse has three gestures here and
        /// the sheet names them all; the WORDS come from `action`, so a mouse
        /// row still cannot invent its own.
        ///
        /// What is NOT checked, and cannot be from this table, is that the
        /// gesture named produces the effect claimed: a keyboard row derives
        /// its effect from the dispatch that just ran, and a mouse row has no
        /// dispatch to derive from, so a row and its probe changed TOGETHER
        /// would agree about something untrue. The gestures themselves are
        /// proved where they happen — `runtime.rs`'s hover and click focus
        /// tests, and `dragging_a_title_band_drops_the_window_beside_where_
        /// it_was_released`.
        Pointer(&'static str, Pointing),
    }

    /// What a mouse row claims the pointer does, as an effect rather than a
    /// string — the sheet's words live in exactly one table either way.
    #[derive(Debug, Eq, PartialEq)]
    enum Pointing {
        Focus,
        Move,
    }

    impl Bound {
        /// The words the sheet must use for this effect. Checking the ACTION
        /// column is the half that makes a row honest: without it a row can
        /// name the right chord beside a description of something else.
        fn action(&self) -> &'static str {
            match self {
                Bound::Command(Command::Focus(_)) | Bound::Pointer(_, Pointing::Focus) => {
                    "FOCUS A TILE"
                }
                // Across the grain a tile LEAVES its container, which "MOVE A
                // TILE" did not say and is the only way out of one.
                Bound::Command(Command::Move(_)) | Bound::Pointer(_, Pointing::Move) => {
                    "MOVE A TILE / SPLIT OUT"
                }
                Bound::Command(Command::SwitchWorkspace(_)) => "SWITCH WORKSPACE",
                Bound::Command(Command::MoveToWorkspace(_)) => "MOVE TO WORKSPACE",
                Bound::Command(Command::SetPresentation(Presentation::Stacked)) => "STACK A COLUMN",
                Bound::Command(Command::SetPresentation(Presentation::Tabbed)) => "TAB A COLUMN",
                // Not bound to a chord: `Super+s` ungroups, and a second way
                // to say the same thing is a second row on the help sheet
                // nobody can reach.
                Bound::Command(Command::SetPresentation(Presentation::Split)) => "UNGROUP",
                Bound::Command(Command::ToggleFullscreen) => "TOGGLE FULLSCREEN",
                Bound::Command(Command::ToggleGrouped) => "GROUP A COLUMN",
                Bound::Launch(LaunchRequest::Terminal) => "NEW TERMINAL",
                Bound::Launch(LaunchRequest::UiDemo) => "NEW INPUT MONITOR",
                Bound::Launcher(_) => "OPEN LAUNCHER",
                Bound::Help(_) => "THIS HELP",
            }
        }
    }

    /// How a chord is SPELLED on the sheet, derived from the codes a probe
    /// actually pressed. Key FAMILIES rather than one row each, because four
    /// bindings are a range and printing every member would be a worse cheat
    /// sheet than naming the range.
    fn spelling(modifiers: &[u16], code: u16) -> String {
        let label = match code {
            KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => "ARROWS",
            KEY_1..=KEY_9 => "1..9",
            KEY_V => "V",
            KEY_H => "H",
            KEY_F => "F",
            KEY_S => "S",
            KEY_T => "T",
            KEY_ENTER => "ENTER",
            // `?` IS the shifted `/`, so the glyph absorbs the modifier
            // rather than the sheet naming it twice.
            KEY_SLASH => return "SUPER+?".to_string(),
            other => panic!("no help spelling for evdev {other}"),
        };
        let shift = if modifiers.contains(&KEY_LEFTSHIFT) {
            "SHIFT+"
        } else {
            ""
        };
        format!("SUPER+{shift}{label}")
    }

    #[test]
    fn every_painted_help_row_is_the_binding_it_claims() {
        // One entry per row, IN ORDER, so a row added without a probe fails
        // the length check rather than going unchecked.
        let probes: &[(&[u16], u16, Bound)] = &[
            (
                &[KEY_LEFTMETA],
                KEY_LEFT,
                Bound::Command(Command::Focus(Direction::Left)),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_UP,
                Bound::Command(Command::Move(Direction::Up)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_3,
                Bound::Command(Command::SwitchWorkspace(3)),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_9,
                Bound::Command(Command::MoveToWorkspace(9)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_V,
                Bound::Command(Command::SetPresentation(Presentation::Stacked)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_H,
                Bound::Command(Command::SetPresentation(Presentation::Tabbed)),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_F,
                Bound::Command(Command::ToggleFullscreen),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_S,
                Bound::Command(Command::ToggleGrouped),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_T,
                Bound::Launch(LaunchRequest::Terminal),
            ),
            (
                &[KEY_LEFTMETA],
                KEY_ENTER,
                Bound::Launcher(LauncherAction::Open),
            ),
            (
                &[KEY_LEFTMETA, KEY_LEFTSHIFT],
                KEY_SLASH,
                Bound::Help(HelpAction::Toggle),
            ),
            (&[], 0, Bound::Pointer("HOVER", Pointing::Focus)),
            (&[], 0, Bound::Pointer("CLICK", Pointing::Focus)),
            (&[], 0, Bound::Pointer("DRAG A TITLE", Pointing::Move)),
        ];
        assert_eq!(
            probes.len(),
            crate::help::ROWS.len(),
            "every help row needs a probe"
        );
        for (probe, row) in probes.iter().zip(crate::help::ROWS) {
            let (modifiers, code, expected) = probe;
            if let Bound::Pointer(keys, _) = expected {
                assert_eq!(row.keys, *keys);
                assert_eq!(row.action, expected.action());
                continue;
            }
            let mut bindings = KeyBindings::default();
            for modifier in *modifiers {
                bindings.feed(key(*modifier, KEY_PRESS));
            }
            let decision = bindings.feed(key(*code, KEY_PRESS));
            let actual = match (
                decision.command,
                decision.launcher,
                decision.launch,
                decision.help,
            ) {
                (Some(command), None, None, None) => Bound::Command(command),
                (None, Some(action), None, None) => Bound::Launcher(action),
                (None, None, Some(request), None) => Bound::Launch(request),
                (None, None, None, Some(action)) => Bound::Help(action),
                other => panic!("{} produced {other:?}", row.keys),
            };
            assert_eq!(&actual, expected, "{} / {}", row.keys, row.action);
            // Both COLUMNS are derived from what the dispatch just did, so a
            // row cannot drift in either direction: the keys from the chord
            // that was pressed, the action from the effect it produced.
            assert_eq!(row.keys, spelling(modifiers, *code));
            assert_eq!(row.action, actual.action(), "{}", row.keys);
            assert!(
                decision.forward.is_none(),
                "{} reached the client",
                row.keys
            );
        }
    }

    #[test]
    fn super_slash_toggles_the_sheet_with_or_without_shift() {
        for modifiers in [&[KEY_LEFTMETA][..], &[KEY_LEFTMETA, KEY_LEFTSHIFT][..]] {
            let mut bindings = KeyBindings::default();
            // Bare, the key is the client's text.
            let bare = bindings.feed(key(KEY_SLASH, KEY_PRESS));
            assert!(bare.help.is_none());
            assert!(bare.forward.is_some());
            bindings.feed(key(KEY_SLASH, KEY_RELEASE));
            for modifier in modifiers {
                bindings.feed(key(*modifier, KEY_PRESS));
            }
            let opened = bindings.feed(key(KEY_SLASH, KEY_PRESS));
            assert_eq!(opened.help, Some(HelpAction::Toggle));
            assert!(opened.forward.is_none());
            assert!(bindings.feed(key(KEY_SLASH, KEY_RELEASE)).forward.is_none());
        }
    }

    #[test]
    fn any_non_modifier_key_dismisses_the_sheet_and_runs_no_command() {
        for code in [KEY_T, KEY_V, KEY_2, KEY_ESC, KEY_A, KEY_ENTER, KEY_SLASH] {
            let mut bindings = KeyBindings::default();
            press(&mut bindings, KEY_LEFTMETA);
            bindings.feed(key(KEY_SLASH, KEY_PRESS));
            bindings.settle_help(Some(true));
            bindings.feed(key(KEY_SLASH, KEY_RELEASE));
            // Super still held: a chord behind the sheet closes it and does
            // NOT also run, or reading the bindings would launch a terminal.
            let dismissed = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(dismissed.help, Some(HelpAction::Close), "{code}");
            assert!(dismissed.command.is_none(), "{code}");
            assert!(dismissed.launch.is_none(), "{code}");
            assert!(dismissed.launcher.is_none(), "{code}");
            assert!(dismissed.forward.is_none(), "{code}");
            assert!(bindings.feed(key(code, KEY_RELEASE)).forward.is_none());
        }
    }

    #[test]
    fn a_modifier_does_not_dismiss_the_sheet() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_SLASH, KEY_PRESS));
        bindings.settle_help(Some(true));
        bindings.feed(key(KEY_SLASH, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        // Reading the sheet means letting go of the keyboard, and pressing a
        // modifier is how the next chord STARTS: dismissing on it would eat
        // the modifier and leave the chord's key to act on its own.
        for code in [
            KEY_LEFTMETA,
            KEY_RIGHTMETA,
            KEY_LEFTSHIFT,
            KEY_LEFTCTRL,
            KEY_LEFTALT,
        ] {
            let held = bindings.feed(key(code, KEY_PRESS));
            assert!(held.help.is_none(), "{code} dismissed the sheet");
            assert!(bindings.help_open, "{code}");
        }
        // The chord's own key then closes it and does not run.
        let dismissed = bindings.feed(key(KEY_T, KEY_PRESS));
        assert_eq!(dismissed.help, Some(HelpAction::Close));
        assert!(dismissed.launch.is_none());
    }

    #[test]
    fn the_launcher_outranks_the_sheet_so_both_are_never_up() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_ENTER, KEY_PRESS));
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        // The launcher branch runs first, and `/` is not a character it
        // accepts, so the chord neither opens the sheet nor types.
        let slash = bindings.feed(key(KEY_SLASH, KEY_PRESS));
        assert!(slash.help.is_none());
        assert!(slash.launcher.is_none());
        assert!(slash.forward.is_none());
        assert!(!bindings.help_open);
    }

    #[test]
    fn the_adapter_routes_the_sheet_and_settles_its_capture() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_SLASH, KEY_PRESS),
            key(KEY_SLASH, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(bindings.lock().unwrap().help_open);
        assert_eq!(target.lock().unwrap().help_actions, [HelpAction::Toggle]);

        for event in [key(KEY_T, KEY_PRESS), key(KEY_T, KEY_RELEASE)] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(!bindings.lock().unwrap().help_open);
        let target = target.lock().unwrap();
        assert_eq!(target.help_actions, [HelpAction::Toggle, HelpAction::Close]);
        assert_eq!(target.launched, []);
        assert_eq!(target.commands, []);
    }

    #[test]
    fn super_t_starts_a_terminal_without_raising_the_overlay() {
        let mut bindings = KeyBindings::default();
        // Bare, the key is the client's text, not a launch.
        let bare = bindings.feed(key(KEY_T, KEY_PRESS));
        assert!(bare.launch.is_none());
        assert!(bare.forward.is_some());
        bindings.feed(key(KEY_T, KEY_RELEASE));
        press(&mut bindings, KEY_RIGHTMETA);
        let chord = bindings.feed(key(KEY_T, KEY_PRESS));
        assert_eq!(chord.launch, Some(LaunchRequest::Terminal));
        assert!(chord.launcher.is_none());
        assert!(chord.command.is_none());
        assert!(chord.forward.is_none());
        // Consumed, so the release does not reach the client either.
        assert!(bindings.feed(key(KEY_T, KEY_RELEASE)).forward.is_none());
    }

    #[test]
    fn an_open_overlay_swallows_every_super_chord() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        // Super is still DOWN. The overlay owns every non-modifier key, so a
        // chord neither runs behind it nor types into its query: `Super+t`
        // must not start a second terminal, `Super+v` must not split.
        for code in [KEY_T, KEY_V, KEY_H, KEY_F, KEY_2, KEY_RIGHT] {
            let held = bindings.feed(key(code, KEY_PRESS));
            assert!(held.launch.is_none(), "{code}");
            assert!(held.command.is_none(), "{code}");
            assert!(held.launcher.is_none(), "{code}");
            assert!(held.forward.is_none(), "{code}");
            bindings.feed(key(code, KEY_RELEASE));
        }
    }

    #[test]
    fn an_unbound_key_under_super_reaches_the_client() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        // Only the bound chords are stolen; everything else is the client's,
        // which is what the terminal's own untranslated-chord rule turns on.
        let unbound = bindings.feed(key(KEY_Q, KEY_PRESS));
        assert!(unbound.command.is_none());
        assert!(unbound.launch.is_none());
        assert!(unbound.launcher.is_none());
        assert!(unbound.forward.is_some());
        assert!(bindings.feed(key(KEY_Q, KEY_RELEASE)).forward.is_some());
    }

    #[test]
    fn the_adapter_hands_a_terminal_chord_to_the_target_without_a_launcher_action() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_T, KEY_PRESS),
            key(KEY_T, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        let target = target.lock().unwrap();
        assert_eq!(target.launched, [LaunchRequest::Terminal]);
        assert_eq!(target.launcher_actions, []);
        assert_eq!(target.commands, []);
    }

    #[test]
    fn launcher_navigation_activation_and_cancel_are_consumed() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        let opened = bindings.feed(key(KEY_ENTER, KEY_PRESS));
        assert_eq!(opened.launcher, Some(LauncherAction::Open));
        assert!(opened.forward.is_none());
        bindings.settle_launcher(Some(true));
        assert!(bindings.feed(key(KEY_ENTER, KEY_RELEASE)).forward.is_none());
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));

        // A key that WOULD be a workspace command becomes text while the
        // overlay is up: the overlay owns every non-modifier key.
        let blocked_command = bindings.feed(key(KEY_2, KEY_PRESS));
        assert!(blocked_command.command.is_none());
        assert!(blocked_command.forward.is_none());
        assert_eq!(blocked_command.launcher, Some(LauncherAction::Insert('2')));
        bindings.feed(key(KEY_2, KEY_RELEASE));

        press(&mut bindings, KEY_LEFTCTRL);
        let next = bindings.feed(key(KEY_N, KEY_PRESS));
        assert_eq!(next.launcher, Some(LauncherAction::Next));
        assert!(next.forward.is_none());
        bindings.feed(key(KEY_N, KEY_RELEASE));
        let previous = bindings.feed(key(KEY_P, KEY_PRESS));
        assert_eq!(previous.launcher, Some(LauncherAction::Previous));
        bindings.feed(key(KEY_P, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTCTRL, KEY_RELEASE));

        let activated = bindings.feed(key(KEY_ENTER, KEY_PRESS));
        assert_eq!(activated.launcher, Some(LauncherAction::Activate));
        bindings.settle_launcher(Some(false));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        assert!(bindings.feed(key(KEY_N, KEY_PRESS)).forward.is_some());

        let mut keypad = KeyBindings {
            launcher_open: true,
            ..KeyBindings::default()
        };
        let activated = keypad.feed(key(KEY_KPENTER, KEY_PRESS));
        assert_eq!(activated.launcher, Some(LauncherAction::Activate));
        assert!(activated.forward.is_none());
        // Both Enters OPEN as well, or the keypad activates something it
        // cannot raise.
        let mut keypad = KeyBindings::default();
        press(&mut keypad, KEY_LEFTMETA);
        let opened = keypad.feed(key(KEY_KPENTER, KEY_PRESS));
        assert_eq!(opened.launcher, Some(LauncherAction::Open));
        assert!(opened.forward.is_none());

        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_RIGHTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_DOWN, KEY_PRESS)).launcher,
            Some(LauncherAction::Next)
        );
        bindings.feed(key(KEY_DOWN, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_UP, KEY_PRESS)).launcher,
            Some(LauncherAction::Previous)
        );
        bindings.feed(key(KEY_UP, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_ESC, KEY_PRESS)).launcher,
            Some(LauncherAction::Close)
        );

        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        bindings.feed(key(KEY_ENTER, KEY_PRESS));
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        press(&mut bindings, KEY_LEFTCTRL);
        assert_eq!(
            bindings.feed(key(KEY_G, KEY_PRESS)).launcher,
            Some(LauncherAction::Close)
        );
        assert!(bindings.feed(key(KEY_G, KEY_RELEASE)).forward.is_none());
    }

    #[test]
    fn launcher_text_backspace_and_modified_keys_are_consumed() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            bindings.feed(key(KEY_ENTER, KEY_PRESS)).launcher,
            Some(LauncherAction::Open)
        );
        bindings.settle_launcher(Some(true));
        bindings.feed(key(KEY_ENTER, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));

        for (code, expected) in [
            (KEY_T, 't'),
            (KEY_E, 'e'),
            (KEY_R, 'r'),
            (KEY_M, 'm'),
            (KEY_SPACE, ' '),
            (KEY_1, '1'),
            (KEY_MINUS, '-'),
        ] {
            let decision = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(decision.launcher, Some(LauncherAction::Insert(expected)));
            assert!(decision.forward.is_none());
            assert!(bindings.feed(key(code, KEY_RELEASE)).forward.is_none());
        }
        assert_eq!(
            bindings.feed(key(KEY_BACKSPACE, KEY_PRESS)).launcher,
            Some(LauncherAction::Backspace)
        );
        bindings.feed(key(KEY_BACKSPACE, KEY_RELEASE));

        press(&mut bindings, KEY_LEFTCTRL);
        let modified = bindings.feed(key(KEY_A, KEY_PRESS));
        assert!(modified.launcher.is_none());
        assert!(modified.forward.is_none());
        bindings.feed(key(KEY_A, KEY_RELEASE));
        bindings.feed(key(KEY_LEFTCTRL, KEY_RELEASE));
        assert_eq!(
            bindings.feed(key(KEY_A, KEY_PRESS)).launcher,
            Some(LauncherAction::Insert('a'))
        );
    }

    #[test]
    fn launcher_character_map_covers_ascii_registry_input() {
        let letters = [
            KEY_A, KEY_B, KEY_C, KEY_D, KEY_E, KEY_F, KEY_G, KEY_H, KEY_I, KEY_J, KEY_K, KEY_L,
            KEY_M, KEY_N, KEY_O, KEY_P, KEY_Q, KEY_R, KEY_S, KEY_T, KEY_U, KEY_V, KEY_W, KEY_X,
            KEY_Y, KEY_Z,
        ];
        let mapped: String = letters.into_iter().filter_map(launcher_character).collect();
        assert_eq!(mapped, "abcdefghijklmnopqrstuvwxyz");
        let digits: String = [
            KEY_1, KEY_2, KEY_3, KEY_4, KEY_5, KEY_6, KEY_7, KEY_8, KEY_9, KEY_0,
        ]
        .into_iter()
        .filter_map(launcher_character)
        .collect();
        assert_eq!(digits, "1234567890");
        assert_eq!(launcher_character(KEY_SPACE), Some(' '));
        assert_eq!(launcher_character(KEY_MINUS), Some('-'));
        assert_eq!(launcher_character(KEY_ENTER), None);
    }

    #[test]
    fn a_chord_is_read_from_the_modifier_held_now_not_from_one_released_earlier() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(
            tap(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        // With Super up the same key is the client's to type: nothing
        // outlives the modifier.
        assert_eq!(tap(&mut bindings, KEY_V), None);
        assert_eq!(tap(&mut bindings, KEY_2), None);
        press(&mut bindings, KEY_RIGHTMETA);
        assert_eq!(tap(&mut bindings, KEY_2), Some(Command::SwitchWorkspace(2)));
    }

    #[test]
    fn autorepeat_never_runs_a_command() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        // A held chord repeats at the driver, and a compositor acting on that
        // would split the layout once per repeat interval.
        assert_eq!(bindings.feed(key(KEY_RIGHT, KEY_REPEAT)).command, None);
        assert_eq!(bindings.feed(key(KEY_V, KEY_REPEAT)).command, None);
        assert_eq!(
            press(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
        assert_eq!(bindings.feed(key(KEY_V, KEY_REPEAT)).command, None);
        bindings.feed(key(KEY_V, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_V),
            Some(Command::SetPresentation(Presentation::Stacked))
        );
    }

    #[test]
    fn pointer_motion_is_coalesced_at_syn_report_and_saturates() {
        let mut pointer = PointerMotion::default();
        assert_eq!(
            pointer.feed_relative(Event {
                time: 0,
                kind: EV_REL,
                code: REL_X,
                value: i32::MAX
            }),
            None
        );
        pointer.feed_relative(Event {
            time: 0,
            kind: EV_REL,
            code: REL_X,
            value: 2,
        });
        pointer.feed_relative(Event {
            time: 0,
            kind: EV_REL,
            code: REL_Y,
            value: -7,
        });
        assert_eq!(
            pointer.feed_relative(Event {
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            Some(PointerFrame {
                time: 9,
                place: PointerPlace::By {
                    dx: i32::MAX,
                    dy: -7
                },
                buttons: Vec::new(),
            })
        );
        assert_eq!(
            pointer.feed_relative(Event {
                time: 0,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            None
        );
    }

    #[test]
    fn an_absolute_overflow_recovers_where_the_device_is_rather_than_where_it_was() {
        // An overflow reaches the same recovery a dropped batch does, and for
        // the same reason: the report it abandons took that report's own
        // EV_ABS values with it, so the held position is stale in a way no
        // later record announces. The sibling test below drives a RELATIVE
        // device, so nothing else covers the absolute arm of that path.
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let moved = AbsoluteAxes {
            x: axis(20000, 0, 32767),
            y: axis(30000, 0, 40000),
        };
        let asked = std::cell::Cell::new(0usize);
        let mut resync = || {
            asked.set(asked.get().saturating_add(1));
            Some(moved)
        };
        let mut state = DeviceState::new(Some(tablet()), &mut resync);
        // A place first, so the recovery has a stale position to replace
        // rather than an absent one.
        for event in [abs(1, ABS_X, 100), abs(1, ABS_Y, 100), syn(1)] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        for index in 0..=MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
            let value = if index % 2 == 0 {
                KEY_PRESS
            } else {
                KEY_RELEASE
            };
            apply_device_event(&target, key(BTN_MOUSE, value), 0, &bindings, &mut state).unwrap();
        }
        assert!(state.dropped);
        assert_eq!(
            asked.get(),
            0,
            "the device was asked before the window ended"
        );

        apply_device_event(&target, syn(9), 0, &bindings, &mut state).unwrap();
        assert!(!state.dropped);
        assert_eq!(
            asked.get(),
            1,
            "an overflow recovery asked {} times",
            asked.get()
        );
        assert_eq!(
            target.lock().unwrap().pointer_places.last(),
            Some(&(9, over(20000, 32767), over(30000, 40000), Vec::new())),
            "an overflow left the cursor where the device used to be"
        );
    }

    #[test]
    fn oversized_pointer_report_is_dropped_and_recovers_at_next_sync() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, &mut resync);
        for index in 0..=MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME {
            apply_device_event(
                &target,
                key(
                    BTN_MOUSE,
                    if index % 2 == 0 {
                        KEY_PRESS
                    } else {
                        KEY_RELEASE
                    },
                ),
                0,
                &bindings,
                &mut state,
            )
            .unwrap();
        }
        assert!(state.dropped);
        assert!(state.pointer.buttons.is_empty());
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        for event in [
            Event {
                time: 7,
                kind: EV_REL,
                code: REL_X,
                value: 99,
            },
            Event {
                time: 8,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            Event {
                time: 9,
                kind: EV_REL,
                code: REL_X,
                value: 3,
            },
            Event {
                time: 10,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 0, &bindings, &mut state).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(10, 3, 0, Vec::new())]
        );
    }

    #[test]
    fn pointer_buttons_are_logical_across_devices_and_flush_with_syn_report() {
        let target = Arc::new(Mutex::new(RecordingTarget::default()));
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        for (device, event) in [
            (0, key(BTN_MOUSE, KEY_PRESS)),
            (1, key(BTN_MOUSE, KEY_PRESS)),
            (
                0,
                Event {
                    time: 5,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (
                1,
                Event {
                    time: 5,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (0, key(BTN_MOUSE, KEY_RELEASE)),
            (
                0,
                Event {
                    time: 6,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
            (1, key(BTN_MOUSE, KEY_RELEASE)),
            (
                1,
                Event {
                    time: 7,
                    kind: EV_SYN,
                    code: SYN_REPORT,
                    value: 0,
                },
            ),
        ] {
            let pointer = if device == 0 { &mut first } else { &mut second };
            apply(target.as_ref(), event, device, &bindings, pointer, None).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    5,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 5,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    7,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 7,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn launcher_capture_drops_new_pointer_buttons_but_keeps_motion_local() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let apply_event = |event, pointer: &mut PointerMotion| {
            apply(&target, event, 0, &bindings, pointer, None).unwrap();
        };
        apply_event(key(KEY_LEFTMETA, KEY_PRESS), &mut pointer);
        apply_event(key(KEY_ENTER, KEY_PRESS), &mut pointer);
        apply_event(key(KEY_ENTER, KEY_PRESS), &mut pointer);
        assert!(bindings.lock().unwrap().launcher_open);

        apply_event(key(BTN_MOUSE, KEY_PRESS), &mut pointer);
        apply_event(
            Event {
                time: 1,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        apply_event(
            Event {
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 5,
            },
            &mut pointer,
        );
        apply_event(
            Event {
                time: 2,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(2, 5, 0, Vec::new())]
        );

        apply_event(key(BTN_MOUSE, KEY_RELEASE), &mut pointer);
        apply_event(
            Event {
                time: 3,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            &mut pointer,
        );
        assert_eq!(target.lock().unwrap().pointer_frames.len(), 1);
    }

    #[test]
    fn replacement_button_press_survives_cross_device_syn_reordering() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        let syn = |time| Event {
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };

        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(&target, syn(1), 0, &bindings, &mut first, None).unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            1,
            &bindings,
            &mut second,
            None,
        )
        .unwrap();
        apply(&target, syn(2), 1, &bindings, &mut second, None).unwrap();
        apply(&target, syn(3), 0, &bindings, &mut first, None).unwrap();
        assert_eq!(target.lock().unwrap().pointer_frames.len(), 1);

        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            1,
            &bindings,
            &mut second,
            None,
        )
        .unwrap();
        apply(&target, syn(4), 1, &bindings, &mut second, None).unwrap();
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    1,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 1,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    4,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 4,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn a_button_edge_waits_for_its_own_device_syn_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first = PointerMotion::default();
        let mut second = PointerMotion::default();
        let syn = |time| Event {
            time,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };

        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut first,
            None,
        )
        .unwrap();
        apply(&target, syn(1), 1, &bindings, &mut second, None).unwrap();
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        apply(&target, syn(2), 0, &bindings, &mut first, None).unwrap();
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [(
                2,
                0,
                0,
                vec![PointerButtonInput {
                    time: 2,
                    button: u32::from(BTN_MOUSE),
                    state: PointerButtonState::Pressed,
                }],
            )]
        );
    }

    #[test]
    fn adapter_dispatches_parsed_commands_and_complete_pointer_frames() {
        let target = Arc::new(Mutex::new(RecordingTarget::default()));
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_LEFT, KEY_PRESS),
            Event {
                time: 0,
                kind: EV_REL,
                code: REL_X,
                value: 3,
            },
            Event {
                time: 0,
                kind: EV_REL,
                code: REL_Y,
                value: -2,
            },
            Event {
                time: 17,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply(target.as_ref(), event, 0, &bindings, &mut pointer, None).unwrap();
        }
        let target = target.lock().unwrap();
        assert_eq!(target.commands, [Command::Focus(Direction::Left)]);
        assert_eq!(
            target.keys,
            [KeyInput {
                time: 0,
                key: u32::from(KEY_LEFTMETA),
                state: KeyState::Pressed,
            }]
        );
        assert_eq!(
            target.modifiers,
            [ModifierState {
                depressed: MOD_LOGO,
                ..ModifierState::default()
            }]
        );
        assert_eq!(
            target.keyboard_calls,
            [
                KeyboardCall::Key(KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Pressed,
                }),
                KeyboardCall::Modifiers(ModifierState {
                    depressed: MOD_LOGO,
                    ..ModifierState::default()
                }),
            ]
        );
        assert_eq!(target.pointer_frames, [(17, 3, -2, Vec::new())]);
    }

    #[test]
    fn adapter_delivers_launcher_actions_in_input_order() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
            key(KEY_DOWN, KEY_PRESS),
            key(KEY_DOWN, KEY_RELEASE),
            key(KEY_ENTER, KEY_PRESS),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().launcher_actions,
            [
                LauncherAction::Open,
                LauncherAction::Next,
                LauncherAction::Activate,
            ]
        );
    }

    #[test]
    fn empty_activation_keeps_input_captured_until_backspace_recovers() {
        let target = Mutex::new(LauncherModelTarget::new());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
            key(KEY_Z, KEY_PRESS),
            key(KEY_Z, KEY_RELEASE),
            key(KEY_ENTER, KEY_PRESS),
            key(KEY_ENTER, KEY_RELEASE),
            key(KEY_BACKSPACE, KEY_PRESS),
            key(KEY_BACKSPACE, KEY_RELEASE),
        ] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        {
            let target = target.lock().unwrap();
            assert!(target.launcher.visible());
            assert_eq!(target.launcher.query(), "");
            assert_eq!(
                target.recording.launcher_actions,
                [
                    LauncherAction::Open,
                    LauncherAction::Insert('z'),
                    LauncherAction::Activate,
                    LauncherAction::Backspace,
                ]
            );
            assert!(target.recording.keys.iter().all(|input| {
                !matches!(
                    input.key,
                    key if key == u32::from(KEY_Z)
                        || key == u32::from(KEY_ENTER)
                        || key == u32::from(KEY_BACKSPACE)
                )
            }));
        }
        assert!(bindings.lock().unwrap().launcher_open);

        for event in [key(KEY_ENTER, KEY_PRESS), key(KEY_ENTER, KEY_RELEASE)] {
            apply(&target, event, 0, &bindings, &mut pointer, None).unwrap();
        }
        assert!(!target.lock().unwrap().launcher.visible());
        assert!(!bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn failed_open_repaint_never_enables_launcher_capture() {
        let path = std::env::temp_dir().join(format!(
            "td-input-launcher-open-failure-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 640, 240, 640 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        })
        .unwrap();
        let target = Mutex::new(LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches,
        });
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(KEY_LEFTMETA, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        runtime.lock().unwrap().fail_next_repaint();
        assert!(apply(
            &target,
            key(KEY_ENTER, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None
        )
        .is_err());
        assert!(!runtime.lock().unwrap().launcher_visible());
        assert!(!bindings.lock().unwrap().launcher_open);
    }

    #[test]
    fn failed_pointer_repaint_retains_state_for_device_cleanup() {
        let path = std::env::temp_dir().join(format!(
            "td-input-pointer-repaint-failure-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = Cleanup(path);
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&cleanup.0, 120, 80, 120 * 4).unwrap();
        let runtime = Arc::new(Mutex::new(Runtime::new(framebuffer)));
        runtime.lock().unwrap().repaint().unwrap();
        let launches = LaunchProcesses::new(LaunchOptions {
            socket: PathBuf::from("/run/user/1000/wayland-0"),
            client: PathBuf::from("/bin/td-ui-demo"),
            terminal: PathBuf::from("/bin/td-term"),
        })
        .unwrap();
        let target = Mutex::new(LiveInputTarget {
            runtime: Arc::clone(&runtime),
            launches,
        });
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        apply(
            &target,
            Event {
                time: 3,
                kind: EV_REL,
                code: REL_X,
                value: 1,
            },
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        runtime.lock().unwrap().fail_next_repaint();
        // The report itself now only owes the paint, so the failure surfaces at
        // the batch flush -- and must still leave the press for cleanup.
        apply(
            &target,
            Event {
                time: 4,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            0,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        assert!(flush_target(&target).is_err());
        {
            let bindings = bindings.lock().unwrap();
            assert!(bindings.pointer_pressed.contains(&(0, BTN_MOUSE)));
            assert!(bindings.pointer_forwarded.contains(&BTN_MOUSE));
        }

        release_device(&target, 0, &bindings, 5).unwrap();

        let bindings = bindings.lock().unwrap();
        assert!(bindings.pointer_pressed.is_empty());
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn ordinary_keys_and_modifiers_forward_but_shortcut_pairs_do_not() {
        let mut bindings = KeyBindings::default();
        let ordinary = bindings.feed(Event {
            time: 44,
            kind: EV_KEY,
            code: 30,
            value: KEY_PRESS,
        });
        assert_eq!(
            ordinary.forward,
            Some(KeyInput {
                time: 44,
                key: 30,
                state: KeyState::Pressed,
            })
        );
        assert_eq!(ordinary.command, None);

        let meta = bindings.feed(key(KEY_LEFTMETA, KEY_PRESS));
        assert!(meta.forward.is_some());
        assert_eq!(
            meta.modifiers,
            Some(ModifierState {
                depressed: MOD_LOGO,
                ..ModifierState::default()
            })
        );
        let shortcut = bindings.feed(key(KEY_F, KEY_PRESS));
        assert_eq!(shortcut.command, Some(Command::ToggleFullscreen));
        assert_eq!(shortcut.forward, None);
        assert_eq!(bindings.feed(key(KEY_F, KEY_RELEASE)).forward, None);
        let released = bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        assert!(released.forward.is_some());
        assert_eq!(released.modifiers, Some(ModifierState::default()));
    }

    #[test]
    fn mouse_buttons_and_keys_outside_the_xkb_range_are_not_keyboard_events() {
        let mut bindings = KeyBindings::default();
        for code in [MAX_XKB_EVDEV_KEY + 1, 0x100, 0x110] {
            let decision = bindings.feed(key(code, KEY_PRESS));
            assert_eq!(decision.command, None);
            assert_eq!(decision.forward, None);
            assert_eq!(decision.modifiers, None);
        }
        let mut pointer = PointerMotion::default();
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_PRESS)), None);
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_REPEAT)), None);
        assert_eq!(pointer.feed_relative(key(BTN_MOUSE, KEY_RELEASE)), None);
        assert_eq!(
            pointer.feed_relative(Event {
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }),
            Some(PointerFrame {
                time: 9,
                place: PointerPlace::By { dx: 0, dy: 0 },
                buttons: vec![
                    PointerButtonTransition {
                        code: BTN_MOUSE,
                        pressed: true,
                    },
                    PointerButtonTransition {
                        code: BTN_MOUSE,
                        pressed: false,
                    },
                ],
            })
        );
    }

    #[test]
    fn special_evdev_codes_match_the_bundled_xkb_keycodes() {
        let maximum = format!("maximum = {};", u32::from(MAX_XKB_EVDEV_KEY) + 8);
        assert!(crate::keyboard::XKB_KEYMAP.contains(&maximum), "{maximum}");
        for (name, code) in [
            ("AE01", KEY_1),
            ("AE09", KEY_9),
            ("AD05", KEY_T),
            ("AD10", KEY_P),
            ("AC04", KEY_F),
            ("AC06", KEY_H),
            ("AB02", KEY_X),
            ("AB04", KEY_V),
            ("AB05", KEY_B),
            ("AB06", KEY_N),
            ("RTRN", KEY_ENTER),
            ("UP", KEY_UP),
            ("LEFT", KEY_LEFT),
            ("RGHT", KEY_RIGHT),
            ("DOWN", KEY_DOWN),
            ("LCTL", KEY_LEFTCTRL),
            ("RCTL", KEY_RIGHTCTRL),
            ("LFSH", KEY_LEFTSHIFT),
            ("RTSH", KEY_RIGHTSHIFT),
            ("LALT", KEY_LEFTALT),
            ("RALT", KEY_RIGHTALT),
            ("CAPS", KEY_CAPSLOCK),
            ("NMLK", KEY_NUMLOCK),
            ("KPEN", KEY_KPENTER),
            ("LWIN", KEY_LEFTMETA),
            ("RWIN", KEY_RIGHTMETA),
        ] {
            let declaration = format!("<{name}> = {};", u32::from(code) + 8);
            assert!(
                crate::keyboard::XKB_KEYMAP.contains(&declaration),
                "{declaration}"
            );
        }
    }

    #[test]
    fn both_sides_contribute_to_the_xkb_modifier_mask() {
        let mut bindings = KeyBindings::default();
        assert_eq!(
            bindings.feed(key(KEY_LEFTSHIFT, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTCTRL, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_CONTROL,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTMETA, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_CONTROL | MOD_LOGO,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_RIGHTCTRL, KEY_RELEASE)).modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_LOGO,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn event_devices_contribute_to_one_logical_keyboard_state() {
        let mut bindings = KeyBindings::default();
        let first_shift = bindings.feed_device(3, key(KEY_LEFTSHIFT, KEY_PRESS));
        assert!(first_shift.forward.is_some());
        assert_eq!(
            first_shift.modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
        );
        let second_shift = bindings.feed_device(8, key(KEY_LEFTSHIFT, KEY_PRESS));
        assert_eq!(second_shift.forward, None);
        assert_eq!(second_shift.modifiers, None);

        let alt = bindings.feed_device(8, key(KEY_RIGHTALT, KEY_PRESS));
        assert!(alt.forward.is_some());
        assert_eq!(
            alt.modifiers,
            Some(ModifierState {
                depressed: MOD_SHIFT | MOD_ALT,
                ..ModifierState::default()
            })
        );
        let first_release = bindings.feed_device(3, key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert_eq!(first_release.forward, None);
        assert_eq!(first_release.modifiers, None);
        let last_release = bindings.feed_device(8, key(KEY_LEFTSHIFT, KEY_RELEASE));
        assert!(last_release.forward.is_some());
        assert_eq!(
            last_release.modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn duplicate_and_unmatched_device_transitions_are_suppressed() {
        let mut bindings = KeyBindings::default();
        let press = key(30, KEY_PRESS);
        assert!(bindings.feed_device(1, press).forward.is_some());
        assert_eq!(bindings.feed_device(1, press).forward, None);
        assert_eq!(bindings.feed_device(2, press).forward, None);
        assert_eq!(bindings.feed_device(9, key(31, KEY_RELEASE)).forward, None);
        assert_eq!(bindings.feed_device(1, key(30, KEY_RELEASE)).forward, None);
        assert!(bindings
            .feed_device(2, key(30, KEY_RELEASE))
            .forward
            .is_some());
    }

    #[test]
    fn a_second_physical_press_cannot_retrigger_a_logical_chord() {
        let mut bindings = KeyBindings::default();
        bindings.feed_device(1, key(KEY_LEFTMETA, KEY_PRESS));
        assert_eq!(
            bindings.feed_device(1, key(KEY_F, KEY_PRESS)).command,
            Some(Command::ToggleFullscreen)
        );
        let duplicate = bindings.feed_device(2, key(KEY_F, KEY_PRESS));
        assert_eq!(duplicate.command, None);
        assert_eq!(duplicate.forward, None);
        assert_eq!(
            bindings.feed_device(1, key(KEY_F, KEY_RELEASE)).forward,
            None
        );
        assert_eq!(
            bindings.feed_device(2, key(KEY_F, KEY_RELEASE)).forward,
            None
        );

        bindings.feed_device(1, key(KEY_2, KEY_PRESS));
        let duplicate = bindings.feed_device(2, key(KEY_2, KEY_PRESS));
        assert_eq!(duplicate.command, None);
        assert_eq!(duplicate.forward, None);
        assert_eq!(
            bindings.feed_device(2, key(KEY_3, KEY_PRESS)).command,
            Some(Command::SwitchWorkspace(3))
        );
    }

    #[test]
    fn closing_an_event_device_releases_only_its_contribution() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut first_pointer = PointerMotion::default();
        let mut second_pointer = PointerMotion::default();
        for (device, event) in [
            (1, key(KEY_LEFTSHIFT, KEY_PRESS)),
            (1, key(30, KEY_PRESS)),
            (2, key(KEY_RIGHTALT, KEY_PRESS)),
        ] {
            let pointer = if device == 1 {
                &mut first_pointer
            } else {
                &mut second_pointer
            };
            apply(&target, event, device, &bindings, pointer, None).unwrap();
        }
        release_device(&target, 1, &bindings, 77).unwrap();

        let target = target.lock().unwrap();
        assert_eq!(
            target.keys,
            [
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTSHIFT),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 0,
                    key: 30,
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_RIGHTALT),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 77,
                    key: 30,
                    state: KeyState::Released,
                },
                KeyInput {
                    time: 77,
                    key: u32::from(KEY_LEFTSHIFT),
                    state: KeyState::Released,
                },
            ]
        );
        assert_eq!(
            target.modifiers.last(),
            Some(&ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn device_teardown_never_forwards_another_devices_press() {
        let target = Mutex::new(RecordingTarget::default());
        let mut state = KeyBindings {
            launcher_open: true,
            ..KeyBindings::default()
        };
        state.pointer_pressed.insert((1, BTN_MOUSE));
        state.pointer_pressed.insert((2, BTN_MOUSE));
        state.settle_launcher(Some(false));
        let bindings = Mutex::new(state);

        release_device(&target, 1, &bindings, 77).unwrap();

        assert!(target.lock().unwrap().pointer_frames.is_empty());
        let bindings = bindings.lock().unwrap();
        assert_eq!(bindings.pointer_pressed, BTreeSet::from([(2, BTN_MOUSE)]));
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn device_teardown_commits_releases_before_delivery_failure() {
        let target = Mutex::new(RecordingTarget {
            key_error: Some("injected key failure".into()),
            pointer_error: Some("injected pointer failure".into()),
            ..RecordingTarget::default()
        });
        let mut state = KeyBindings::default();
        state.pressed.insert((1, KEY_LEFTSHIFT));
        state.forwarded.insert((1, KEY_LEFTSHIFT));
        state.pointer_pressed.insert((1, BTN_MOUSE));
        state.pointer_forwarded.insert(BTN_MOUSE);
        let bindings = Mutex::new(state);

        let error = release_device(&target, 1, &bindings, 77).unwrap_err();
        assert!(error.contains("injected key failure"));
        assert!(error.contains("injected pointer failure"));

        let target = target.lock().unwrap();
        assert_eq!(
            target.pointer_frames,
            [(
                77,
                0,
                0,
                vec![PointerButtonInput {
                    time: 77,
                    button: u32::from(BTN_MOUSE),
                    state: PointerButtonState::Released,
                }],
            )]
        );
        assert_eq!(
            target.keys.last(),
            Some(&KeyInput {
                time: 77,
                key: u32::from(KEY_LEFTSHIFT),
                state: KeyState::Released,
            })
        );
        assert_eq!(target.modifiers.last(), Some(&ModifierState::default()));
        drop(target);
        let bindings = bindings.lock().unwrap();
        assert!(bindings.pressed.is_empty());
        assert!(bindings.forwarded.is_empty());
        assert!(bindings.pointer_pressed.is_empty());
        assert!(bindings.pointer_forwarded.is_empty());
    }

    #[test]
    fn unrelated_device_teardown_does_not_release_a_held_modifier() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        apply(
            &target,
            key(KEY_LEFTMETA, KEY_PRESS),
            1,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        release_device(&target, 2, &bindings, 17).unwrap();
        apply(
            &target,
            key(KEY_2, KEY_PRESS),
            1,
            &bindings,
            &mut pointer,
            None,
        )
        .unwrap();
        assert_eq!(
            target.lock().unwrap().commands,
            [Command::SwitchWorkspace(2)]
        );
    }

    #[test]
    fn syn_dropped_releases_state_and_ignores_events_until_the_next_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, &mut resync);
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_V, KEY_PRESS),
            Event {
                time: 3,
                kind: EV_REL,
                code: REL_X,
                value: 9,
            },
            Event {
                time: 4,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            key(30, KEY_PRESS),
            Event {
                time: 6,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            key(KEY_2, KEY_PRESS),
            Event {
                time: 8,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 5, &bindings, &mut state).unwrap();
        }

        let target = target.lock().unwrap();
        assert_eq!(
            target.commands,
            [Command::SetPresentation(Presentation::Stacked)]
        );
        assert_eq!(
            target.keys,
            [
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Pressed,
                },
                KeyInput {
                    time: 4,
                    key: u32::from(KEY_LEFTMETA),
                    state: KeyState::Released,
                },
                KeyInput {
                    time: 0,
                    key: u32::from(KEY_2),
                    state: KeyState::Pressed,
                },
            ]
        );
        assert_eq!(
            target.modifiers,
            [
                ModifierState {
                    depressed: MOD_LOGO,
                    ..ModifierState::default()
                },
                ModifierState::default(),
            ]
        );
        assert!(target.pointer_frames.is_empty());
    }

    #[test]
    fn syn_dropped_discards_partial_pointer_data_and_releases_forwarded_buttons() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut resync = || None;
        let mut state = DeviceState::new(None, &mut resync);
        for event in [
            key(BTN_MOUSE, KEY_PRESS),
            Event {
                time: 1,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
            key(BTN_MOUSE, KEY_RELEASE),
            Event {
                time: 2,
                kind: EV_REL,
                code: REL_X,
                value: 20,
            },
            Event {
                time: 3,
                kind: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            Event {
                time: 4,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply_device_event(&target, event, 5, &bindings, &mut state).unwrap();
        }
        assert_eq!(
            target.lock().unwrap().pointer_frames,
            [
                (
                    1,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 1,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Pressed,
                    }],
                ),
                (
                    3,
                    0,
                    0,
                    vec![PointerButtonInput {
                        time: 3,
                        button: u32::from(BTN_MOUSE),
                        state: PointerButtonState::Released,
                    }],
                ),
            ]
        );
    }

    #[test]
    fn alt_is_depressed_and_caps_and_num_are_locked_modifiers() {
        let mut bindings = KeyBindings::default();
        assert_eq!(
            bindings.feed(key(KEY_LEFTALT, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_CAPS,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_NUMLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_CAPS | MOD_NUM,
                ..ModifierState::default()
            })
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_RELEASE)).modifiers,
            None
        );
        assert_eq!(
            bindings.feed(key(KEY_CAPSLOCK, KEY_PRESS)).modifiers,
            Some(ModifierState {
                depressed: MOD_ALT,
                locked: MOD_NUM,
                ..ModifierState::default()
            })
        );
    }

    #[test]
    fn lock_keys_toggle_once_across_multiple_event_devices() {
        for (code, mask) in [(KEY_CAPSLOCK, MOD_CAPS), (KEY_NUMLOCK, MOD_NUM)] {
            let mut bindings = KeyBindings::default();
            let first = bindings.feed_device(1, key(code, KEY_PRESS));
            assert!(first.forward.is_some());
            assert_eq!(first.modifiers.unwrap().locked, mask);
            let second = bindings.feed_device(2, key(code, KEY_PRESS));
            assert_eq!(second.forward, None);
            assert_eq!(second.modifiers, None);
            let first_release = bindings.feed_device(1, key(code, KEY_RELEASE));
            assert_eq!(first_release.forward, None);
            assert_eq!(first_release.modifiers, None);
            let second_release = bindings.feed_device(2, key(code, KEY_RELEASE));
            assert!(second_release.forward.is_some());
            assert_eq!(second_release.modifiers, None);

            bindings.feed_device(1, key(code, KEY_PRESS));
            assert_eq!(bindings.modifiers().locked, 0);
        }
    }
}
