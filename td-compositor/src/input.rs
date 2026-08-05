use crate::keyboard::{
    KeyInput, KeyState, ModifierState, MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_NUM, MOD_SHIFT,
};
use crate::launcher::{LaunchOptions, LaunchProcesses, LaunchRequest, LauncherAction};
use crate::layout::{Axis, Command, Direction};
use crate::pointer::{
    PointerButtonInput, PointerButtonState, MAX_POINTER_BUTTON_TRANSITIONS_PER_FRAME,
};
use crate::runtime::Runtime;
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
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
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
    consumed: BTreeSet<(usize, u16)>,
    pointer_pressed: BTreeSet<(usize, u16)>,
    pointer_forwarded: BTreeSet<u16>,
}

#[derive(Debug, Eq, PartialEq)]
struct KeyDecision {
    command: Option<Command>,
    launcher: Option<LauncherAction>,
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
            KEY_V => Some(Command::SetSplit(Axis::Vertical)),
            KEY_H => Some(Command::SetSplit(Axis::Horizontal)),
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

#[derive(Default)]
struct PointerMotion {
    dx: i32,
    dy: i32,
    pressed: BTreeSet<u16>,
    buttons: Vec<PointerButtonTransition>,
    overflowed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PointerButtonTransition {
    code: u16,
    pressed: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct PointerFrame {
    time: u32,
    dx: i32,
    dy: i32,
    buttons: Vec<PointerButtonTransition>,
}

trait InputTarget {
    fn command(&mut self, command: Command) -> Result<(), String>;
    fn launcher(&mut self, action: LauncherAction) -> Result<bool, String>;
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

    fn flush(&mut self) -> Result<(), String> {
        self.runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .flush_paint()
    }
}

impl PointerMotion {
    fn feed(&mut self, event: Event) -> Option<PointerFrame> {
        match (event.kind, event.code) {
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(event.value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(event.value),
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
            (EV_SYN, SYN_REPORT) if self.dx != 0 || self.dy != 0 || !self.buttons.is_empty() => {
                return Some(PointerFrame {
                    time: event.time,
                    dx: std::mem::take(&mut self.dx),
                    dy: std::mem::take(&mut self.dy),
                    buttons: std::mem::take(&mut self.buttons),
                });
            }
            _ => {}
        }
        None
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
) -> Result<(), String> {
    let frame = pointer.feed(event);
    if event.kind != EV_KEY && frame.is_none() {
        return Ok(());
    }
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    let decision = bindings.feed_device(device, event);
    if decision.command.is_none()
        && decision.launcher.is_none()
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
        let mut buttons = bindings.pointer_device_changes(device, &frame.buttons, frame.time);
        if bindings.launcher_open {
            buttons.retain(|button| button.state == PointerButtonState::Released);
        }
        let delivery = if frame.dx != 0 || frame.dy != 0 || !buttons.is_empty() {
            runtime.pointer_frame(frame.time, frame.dx, frame.dy, &buttons)
        } else {
            Ok(())
        };
        bindings.commit_pointer_device(device, &pointer.pressed, &buttons);
        delivery?;
    }
    Ok(())
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

fn apply_device_event<T: InputTarget>(
    runtime: &Mutex<T>,
    event: Event,
    device: usize,
    bindings: &Mutex<KeyBindings>,
    pointer: &mut PointerMotion,
    dropped: &mut bool,
) -> Result<(), String> {
    if *dropped {
        if event.kind == EV_SYN && event.code == SYN_REPORT {
            *dropped = false;
        }
        return Ok(());
    }
    if event.kind == EV_SYN && event.code == SYN_DROPPED {
        *pointer = PointerMotion::default();
        release_device(runtime, device, bindings, event.time)?;
        *dropped = true;
        return Ok(());
    }
    apply(runtime, event, device, bindings, pointer)?;
    if pointer.overflowed {
        *pointer = PointerMotion::default();
        release_device(runtime, device, bindings, event.time)?;
        *dropped = true;
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
) -> Result<(), String> {
    let mut buffer = [0u8; READ_BATCH_BYTES];
    let mut filled = 0usize;
    let mut pointer = PointerMotion::default();
    let mut dropped = false;
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
            if let Err(error) =
                apply_device_event(target, event, device, bindings, &mut pointer, &mut dropped)
            {
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
                if let Err(error) =
                    read_device(&path, &mut file, device, target.as_ref(), bindings.as_ref())
                {
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
        pointer_error: Option<String>,
        flushes: usize,
        flush_error: Option<String>,
        launched: Vec<LaunchRequest>,
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
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut reader = ChunkedReader::new(data, chunks);
        let result = read_device(Path::new("event-test"), &mut reader, 0, &target, &bindings);
        (
            target
                .into_inner()
                .unwrap_or_else(|error| error.into_inner()),
            result,
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
        let result = read_device(Path::new("event-test"), &mut reader, 0, &target, &bindings);
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
            (KEY_V, Command::SetSplit(Axis::Vertical)),
            (KEY_H, Command::SetSplit(Axis::Horizontal)),
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
            apply(&target, event, 0, &bindings, &mut pointer).unwrap();
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
            Some(Command::SetSplit(Axis::Vertical))
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
        assert_eq!(press(&mut bindings, KEY_V), Some(Command::SetSplit(Axis::Vertical)));
        assert_eq!(bindings.feed(key(KEY_V, KEY_REPEAT)).command, None);
        bindings.feed(key(KEY_V, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_V),
            Some(Command::SetSplit(Axis::Vertical))
        );
    }

    #[test]
    fn pointer_motion_is_coalesced_at_syn_report_and_saturates() {
        let mut pointer = PointerMotion::default();
        assert_eq!(
            pointer.feed(Event {
                time: 0,
                kind: EV_REL,
                code: REL_X,
                value: i32::MAX
            }),
            None
        );
        pointer.feed(Event {
            time: 0,
            kind: EV_REL,
            code: REL_X,
            value: 2,
        });
        pointer.feed(Event {
            time: 0,
            kind: EV_REL,
            code: REL_Y,
            value: -7,
        });
        assert_eq!(
            pointer.feed(Event {
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            Some(PointerFrame {
                time: 9,
                dx: i32::MAX,
                dy: -7,
                buttons: Vec::new(),
            })
        );
        assert_eq!(
            pointer.feed(Event {
                time: 0,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            None
        );
    }

    #[test]
    fn oversized_pointer_report_is_dropped_and_recovers_at_next_sync() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let mut dropped = false;
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
                &mut pointer,
                &mut dropped,
            )
            .unwrap();
        }
        assert!(dropped);
        assert!(pointer.buttons.is_empty());
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
            apply_device_event(&target, event, 0, &bindings, &mut pointer, &mut dropped).unwrap();
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
            apply(target.as_ref(), event, device, &bindings, pointer).unwrap();
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
            apply(&target, event, 0, &bindings, pointer).unwrap();
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

        apply(&target, key(BTN_MOUSE, KEY_PRESS), 0, &bindings, &mut first).unwrap();
        apply(&target, syn(1), 0, &bindings, &mut first).unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            0,
            &bindings,
            &mut first,
        )
        .unwrap();
        apply(
            &target,
            key(BTN_MOUSE, KEY_PRESS),
            1,
            &bindings,
            &mut second,
        )
        .unwrap();
        apply(&target, syn(2), 1, &bindings, &mut second).unwrap();
        apply(&target, syn(3), 0, &bindings, &mut first).unwrap();
        assert_eq!(target.lock().unwrap().pointer_frames.len(), 1);

        apply(
            &target,
            key(BTN_MOUSE, KEY_RELEASE),
            1,
            &bindings,
            &mut second,
        )
        .unwrap();
        apply(&target, syn(4), 1, &bindings, &mut second).unwrap();
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

        apply(&target, key(BTN_MOUSE, KEY_PRESS), 0, &bindings, &mut first).unwrap();
        apply(&target, syn(1), 1, &bindings, &mut second).unwrap();
        assert!(target.lock().unwrap().pointer_frames.is_empty());

        apply(&target, syn(2), 0, &bindings, &mut first).unwrap();
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
            apply(target.as_ref(), event, 0, &bindings, &mut pointer).unwrap();
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
            apply(&target, event, 0, &bindings, &mut pointer).unwrap();
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
            apply(&target, event, 0, &bindings, &mut pointer).unwrap();
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
            apply(&target, event, 0, &bindings, &mut pointer).unwrap();
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
        apply(&target, key(KEY_LEFTMETA, KEY_PRESS), 0, &bindings, &mut pointer).unwrap();
        runtime.lock().unwrap().fail_next_repaint();
        assert!(apply(&target, key(KEY_ENTER, KEY_PRESS), 0, &bindings, &mut pointer).is_err());
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
        apply(&target, key(BTN_MOUSE, KEY_PRESS), 0, &bindings, &mut pointer).unwrap();
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
        assert_eq!(pointer.feed(key(BTN_MOUSE, KEY_PRESS)), None);
        assert_eq!(pointer.feed(key(BTN_MOUSE, KEY_REPEAT)), None);
        assert_eq!(pointer.feed(key(BTN_MOUSE, KEY_RELEASE)), None);
        assert_eq!(
            pointer.feed(Event {
                time: 9,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            }),
            Some(PointerFrame {
                time: 9,
                dx: 0,
                dy: 0,
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
            apply(&target, event, device, &bindings, pointer).unwrap();
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
        )
        .unwrap();
        release_device(&target, 2, &bindings, 17).unwrap();
        apply(&target, key(KEY_2, KEY_PRESS), 1, &bindings, &mut pointer).unwrap();
        assert_eq!(
            target.lock().unwrap().commands,
            [Command::SwitchWorkspace(2)]
        );
    }

    #[test]
    fn syn_dropped_releases_state_and_ignores_events_until_the_next_report() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        let mut dropped = false;
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
            apply_device_event(&target, event, 5, &bindings, &mut pointer, &mut dropped).unwrap();
        }

        let target = target.lock().unwrap();
        assert_eq!(target.commands, [Command::SetSplit(Axis::Vertical)]);
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
        let mut pointer = PointerMotion::default();
        let mut dropped = false;
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
            apply_device_event(&target, event, 5, &bindings, &mut pointer, &mut dropped).unwrap();
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
