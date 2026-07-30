use crate::keyboard::{
    KeyInput, KeyState, ModifierState, MOD_ALT, MOD_CAPS, MOD_CONTROL, MOD_LOGO, MOD_NUM, MOD_SHIFT,
};
use crate::layout::{Axis, Command, Direction};
use crate::runtime::Runtime;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

const EVENT_SIZE: usize = 24;
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const KEY_1: u16 = 2;
const KEY_2: u16 = 3;
const KEY_3: u16 = 4;
const KEY_4: u16 = 5;
const KEY_5: u16 = 6;
const KEY_6: u16 = 7;
const KEY_7: u16 = 8;
const KEY_8: u16 = 9;
const KEY_9: u16 = 10;
const KEY_P: u16 = 25;
const KEY_LEFTCTRL: u16 = 29;
const KEY_F: u16 = 33;
const KEY_X: u16 = 45;
const KEY_B: u16 = 48;
const KEY_N: u16 = 49;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_CAPSLOCK: u16 = 58;
const KEY_NUMLOCK: u16 = 69;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
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
    prefix_device: Option<usize>,
    consumed: BTreeSet<(usize, u16)>,
}

#[derive(Debug, Eq, PartialEq)]
struct KeyDecision {
    command: Option<Command>,
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
        let meta = self.pressed(KEY_LEFTMETA) || self.pressed(KEY_RIGHTMETA);
        if meta && event.code == KEY_X {
            self.prefix_device = Some(device);
            self.consumed.insert(physical);
            return decision;
        }
        if self.prefix_device.is_some() {
            self.prefix_device = None;
            self.consumed.insert(physical);
            decision.command = match event.code {
                KEY_1 => Some(Command::ToggleFullscreen),
                KEY_2 => Some(Command::SetSplit(Axis::Vertical)),
                KEY_3 => Some(Command::SetSplit(Axis::Horizontal)),
                _ => None,
            };
            return decision;
        }
        if !meta {
            decision.forward = self.forward(physical, event);
            return decision;
        }
        let shift = self.pressed(KEY_LEFTSHIFT) || self.pressed(KEY_RIGHTSHIFT);
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
}

trait InputTarget {
    fn command(&mut self, command: Command) -> Result<(), String>;
    fn key(&mut self, input: KeyInput) -> Result<(), String>;
    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String>;
    fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String>;
}

impl InputTarget for Runtime {
    fn command(&mut self, command: Command) -> Result<(), String> {
        Runtime::command(self, command)
    }

    fn key(&mut self, input: KeyInput) -> Result<(), String> {
        Runtime::key(self, input)
    }

    fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
        Runtime::modifiers(self, modifiers)
    }

    fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        Runtime::move_pointer(self, dx, dy)
    }
}

impl PointerMotion {
    fn feed(&mut self, event: Event) -> Option<(i32, i32)> {
        match (event.kind, event.code) {
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(event.value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(event.value),
            (EV_SYN, SYN_REPORT) if self.dx != 0 || self.dy != 0 => {
                return Some((std::mem::take(&mut self.dx), std::mem::take(&mut self.dy)));
            }
            _ => {}
        }
        None
    }
}

fn direction(code: u16) -> Option<Direction> {
    match code {
        KEY_B => Some(Direction::Left),
        KEY_F => Some(Direction::Right),
        KEY_P => Some(Direction::Up),
        KEY_N => Some(Direction::Down),
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
    let motion = pointer.feed(event);
    if event.kind != EV_KEY && motion.is_none() {
        return Ok(());
    }
    let mut bindings = bindings
        .lock()
        .map_err(|_| "input bindings lock poisoned".to_string())?;
    let decision = bindings.feed_device(device, event);
    if decision.command.is_none()
        && decision.forward.is_none()
        && decision.modifiers.is_none()
        && motion.is_none()
    {
        return Ok(());
    }
    // Keep seat decisions and delivery in one cross-device order.
    let mut runtime = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?;
    deliver_key_decision(&mut *runtime, decision)?;
    if let Some((dx, dy)) = motion {
        runtime.move_pointer(dx, dy)?;
    }
    Ok(())
}

fn deliver_key_decision<T: InputTarget>(
    runtime: &mut T,
    decision: KeyDecision,
) -> Result<(), String> {
    if let Some(command) = decision.command {
        runtime.command(command)?;
    }
    if let Some(input) = decision.forward {
        runtime.key(input)?;
    }
    if let Some(modifiers) = decision.modifiers {
        runtime.modifiers(modifiers)?;
    }
    Ok(())
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
    if bindings.prefix_device == Some(device) {
        bindings.prefix_device = None;
    }
    let codes: Vec<u16> = bindings
        .pressed
        .iter()
        .filter_map(|(owner, code)| (*owner == device).then_some(*code))
        .collect();
    if codes.is_empty() {
        return Ok(());
    }
    let mut runtime = runtime
        .lock()
        .map_err(|_| "runtime lock poisoned".to_string())?;
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
        deliver_key_decision(&mut *runtime, decision)?;
    }
    Ok(())
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
        release_device(runtime, device, bindings, event.time)?;
        *pointer = PointerMotion::default();
        *dropped = true;
        return Ok(());
    }
    apply(runtime, event, device, bindings, pointer)
}

fn read_device(
    path: &Path,
    mut file: File,
    device: usize,
    runtime: Arc<Mutex<Runtime>>,
    bindings: Arc<Mutex<KeyBindings>>,
) -> Result<(), String> {
    let mut bytes = [0u8; EVENT_SIZE];
    let mut pointer = PointerMotion::default();
    let mut dropped = false;
    let mut last_time = 0;
    let result = loop {
        match file.read_exact(&mut bytes) {
            Ok(()) => {
                let event = match parse(&bytes) {
                    Ok(event) => event,
                    Err(error) => break Err(error),
                };
                last_time = event.time;
                if let Err(error) = apply_device_event(
                    runtime.as_ref(),
                    event,
                    device,
                    bindings.as_ref(),
                    &mut pointer,
                    &mut dropped,
                ) {
                    break Err(error);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(format!("read input {}: {error}", path.display())),
        }
    };
    let cleanup = release_device(runtime.as_ref(), device, bindings.as_ref(), last_time);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            Err(format!("{error}; release input state: {cleanup_error}"))
        }
    }
}

pub fn start(input_dir: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<usize, String> {
    let paths = event_paths(input_dir)?;
    let bindings = Arc::new(Mutex::new(KeyBindings::default()));
    for (device, path) in paths.iter().enumerate() {
        let file = File::open(path).map_err(|e| format!("open input {}: {e}", path.display()))?;
        let path = path.clone();
        let label = path.display().to_string();
        let runtime = Arc::clone(&runtime);
        let bindings = Arc::clone(&bindings);
        thread::Builder::new()
            .name(format!(
                "input-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("event")
            ))
            .spawn(move || {
                if let Err(error) = read_device(&path, file, device, runtime, bindings) {
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
        keys: Vec<KeyInput>,
        modifiers: Vec<ModifierState>,
        keyboard_calls: Vec<KeyboardCall>,
        motions: Vec<(i32, i32)>,
    }

    impl InputTarget for RecordingTarget {
        fn command(&mut self, command: Command) -> Result<(), String> {
            self.commands.push(command);
            Ok(())
        }

        fn key(&mut self, input: KeyInput) -> Result<(), String> {
            self.keys.push(input);
            self.keyboard_calls.push(KeyboardCall::Key(input));
            Ok(())
        }

        fn modifiers(&mut self, modifiers: ModifierState) -> Result<(), String> {
            self.modifiers.push(modifiers);
            self.keyboard_calls.push(KeyboardCall::Modifiers(modifiers));
            Ok(())
        }

        fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String> {
            self.motions.push((dx, dy));
            Ok(())
        }
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
            .copy_from_slice(&KEY_F.to_ne_bytes());
        bytes
            .get_mut(20..24)
            .unwrap()
            .copy_from_slice(&KEY_PRESS.to_ne_bytes());
        assert_eq!(
            parse(&bytes).unwrap(),
            Event {
                time: 12_345,
                kind: EV_KEY,
                code: KEY_F,
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
    fn emacs_direction_keys_focus_and_shift_moves_in_every_direction() {
        for (code, direction) in [
            (KEY_B, Direction::Left),
            (KEY_F, Direction::Right),
            (KEY_P, Direction::Up),
            (KEY_N, Direction::Down),
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
            tap(&mut bindings, KEY_F),
            Some(Command::Move(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_F),
            Some(Command::Focus(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(tap(&mut bindings, KEY_F), None);
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
    fn emacs_prefix_selects_fullscreen_and_both_split_axes() {
        for (code, expected) in [
            (KEY_1, Command::ToggleFullscreen),
            (KEY_2, Command::SetSplit(Axis::Vertical)),
            (KEY_3, Command::SetSplit(Axis::Horizontal)),
        ] {
            let mut bindings = KeyBindings::default();
            press(&mut bindings, KEY_LEFTMETA);
            assert_eq!(press(&mut bindings, KEY_X), None);
            assert_eq!(bindings.feed(key(KEY_X, KEY_RELEASE)).command, None);
            assert_eq!(tap(&mut bindings, code), Some(expected));
        }
    }

    #[test]
    fn prefix_survives_modifier_release_and_is_cancelled_by_an_unknown_key() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        press(&mut bindings, KEY_X);
        bindings.feed(key(KEY_LEFTMETA, KEY_RELEASE));
        press(&mut bindings, KEY_LEFTCTRL);
        press(&mut bindings, KEY_RIGHTCTRL);
        bindings.feed(key(KEY_LEFTCTRL, KEY_RELEASE));
        bindings.feed(key(KEY_RIGHTCTRL, KEY_RELEASE));
        assert_eq!(
            tap(&mut bindings, KEY_2),
            Some(Command::SetSplit(Axis::Vertical))
        );
        press(&mut bindings, KEY_LEFTMETA);
        press(&mut bindings, KEY_X);
        assert_eq!(tap(&mut bindings, 99), None);
        assert_eq!(tap(&mut bindings, KEY_2), Some(Command::SwitchWorkspace(2)));
    }

    #[test]
    fn autorepeat_neither_runs_a_command_nor_consumes_a_prefix() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(bindings.feed(key(KEY_F, KEY_REPEAT)).command, None);
        press(&mut bindings, KEY_X);
        assert_eq!(bindings.feed(key(KEY_2, KEY_REPEAT)).command, None);
        assert_eq!(
            tap(&mut bindings, KEY_2),
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
                time: 0,
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            Some((i32::MAX, -7))
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
    fn adapter_dispatches_parsed_commands_and_complete_pointer_frames() {
        let target = Arc::new(Mutex::new(RecordingTarget::default()));
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_B, KEY_PRESS),
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
                time: 0,
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
        assert_eq!(target.motions, [(3, -2)]);
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
        assert_eq!(shortcut.command, Some(Command::Focus(Direction::Right)));
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
    }

    #[test]
    fn special_evdev_codes_match_the_bundled_xkb_keycodes() {
        let maximum = format!("maximum = {};", u32::from(MAX_XKB_EVDEV_KEY) + 8);
        assert!(crate::keyboard::XKB_KEYMAP.contains(&maximum), "{maximum}");
        for (name, code) in [
            ("AE01", KEY_1),
            ("AE09", KEY_9),
            ("AD10", KEY_P),
            ("AC04", KEY_F),
            ("AB02", KEY_X),
            ("AB05", KEY_B),
            ("AB06", KEY_N),
            ("LCTL", KEY_LEFTCTRL),
            ("RCTL", KEY_RIGHTCTRL),
            ("LFSH", KEY_LEFTSHIFT),
            ("RTSH", KEY_RIGHTSHIFT),
            ("LALT", KEY_LEFTALT),
            ("RALT", KEY_RIGHTALT),
            ("CAPS", KEY_CAPSLOCK),
            ("NMLK", KEY_NUMLOCK),
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
            Some(Command::Focus(Direction::Right))
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
        bindings.feed_device(1, key(KEY_X, KEY_PRESS));
        let duplicate = bindings.feed_device(2, key(KEY_2, KEY_PRESS));
        assert_eq!(duplicate.command, None);
        assert_eq!(duplicate.forward, None);
        assert_eq!(
            bindings.feed_device(2, key(KEY_3, KEY_PRESS)).command,
            Some(Command::SetSplit(Axis::Horizontal))
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
    fn unrelated_device_teardown_does_not_cancel_a_prefix() {
        let target = Mutex::new(RecordingTarget::default());
        let bindings = Mutex::new(KeyBindings::default());
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_X, KEY_PRESS),
            key(KEY_X, KEY_RELEASE),
            key(KEY_LEFTMETA, KEY_RELEASE),
        ] {
            apply(&target, event, 1, &bindings, &mut pointer).unwrap();
        }
        release_device(&target, 2, &bindings, 17).unwrap();
        apply(&target, key(KEY_2, KEY_PRESS), 1, &bindings, &mut pointer).unwrap();
        assert_eq!(
            target.lock().unwrap().commands,
            [Command::SetSplit(Axis::Vertical)]
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
            key(KEY_X, KEY_PRESS),
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
        assert_eq!(target.commands, []);
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
        assert!(target.motions.is_empty());
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
