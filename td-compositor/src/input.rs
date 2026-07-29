use crate::layout::{Axis, Command, Direction};
use crate::runtime::Runtime;
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
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const KEY_RELEASE: i32 = 0;
const KEY_PRESS: i32 = 1;
#[cfg(test)]
const KEY_REPEAT: i32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    kind: u16,
    code: u16,
    value: i32,
}

#[derive(Default)]
struct KeyBindings {
    left_meta: bool,
    right_meta: bool,
    left_shift: bool,
    right_shift: bool,
    prefix: bool,
}

impl KeyBindings {
    fn feed(&mut self, event: Event) -> Option<Command> {
        if event.kind != EV_KEY {
            return None;
        }
        match (event.code, event.value) {
            (KEY_LEFTMETA, KEY_PRESS) => self.left_meta = true,
            (KEY_LEFTMETA, KEY_RELEASE) => self.left_meta = false,
            (KEY_RIGHTMETA, KEY_PRESS) => self.right_meta = true,
            (KEY_RIGHTMETA, KEY_RELEASE) => self.right_meta = false,
            (KEY_LEFTSHIFT, KEY_PRESS) => self.left_shift = true,
            (KEY_LEFTSHIFT, KEY_RELEASE) => self.left_shift = false,
            (KEY_RIGHTSHIFT, KEY_PRESS) => self.right_shift = true,
            (KEY_RIGHTSHIFT, KEY_RELEASE) => self.right_shift = false,
            _ => {}
        }
        if matches!(
            event.code,
            KEY_LEFTCTRL
                | KEY_RIGHTCTRL
                | KEY_LEFTMETA
                | KEY_RIGHTMETA
                | KEY_LEFTSHIFT
                | KEY_RIGHTSHIFT
        ) {
            return None;
        }
        if event.value != KEY_PRESS {
            return None;
        }
        let meta = self.left_meta || self.right_meta;
        if meta && event.code == KEY_X {
            self.prefix = true;
            return None;
        }
        if self.prefix {
            self.prefix = false;
            return match event.code {
                KEY_1 => Some(Command::ToggleFullscreen),
                KEY_2 => Some(Command::SetSplit(Axis::Vertical)),
                KEY_3 => Some(Command::SetSplit(Axis::Horizontal)),
                _ => None,
            };
        }
        if !meta {
            return None;
        }
        let shift = self.left_shift || self.right_shift;
        if let Some(direction) = direction(event.code) {
            return if shift {
                Some(Command::Move(direction))
            } else {
                Some(Command::Focus(direction))
            };
        }
        workspace(event.code).map(|number| {
            if shift {
                Command::MoveToWorkspace(number)
            } else {
                Command::SwitchWorkspace(number)
            }
        })
    }
}

#[derive(Default)]
struct PointerMotion {
    dx: i32,
    dy: i32,
}

trait InputTarget {
    fn command(&mut self, command: Command) -> Result<(), String>;
    fn move_pointer(&mut self, dx: i32, dy: i32) -> Result<(), String>;
}

impl InputTarget for Runtime {
    fn command(&mut self, command: Command) -> Result<(), String> {
        Runtime::command(self, command)
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

fn parse(bytes: &[u8]) -> Result<Event, String> {
    if bytes.len() != EVENT_SIZE {
        return Err(format!(
            "input_event is {} bytes, expected {EVENT_SIZE}",
            bytes.len()
        ));
    }
    Ok(Event {
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
    bindings: &mut KeyBindings,
    pointer: &mut PointerMotion,
) -> Result<(), String> {
    if let Some(command) = bindings.feed(event) {
        return runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .command(command);
    }
    if let Some((dx, dy)) = pointer.feed(event) {
        return runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .move_pointer(dx, dy);
    }
    Ok(())
}

fn read_device(path: &Path, mut file: File, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let mut bytes = [0u8; EVENT_SIZE];
    let mut bindings = KeyBindings::default();
    let mut pointer = PointerMotion::default();
    loop {
        match file.read_exact(&mut bytes) {
            Ok(()) => {
                let event = parse(&bytes)?;
                apply(runtime.as_ref(), event, &mut bindings, &mut pointer)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(format!("read input {}: {error}", path.display())),
        }
    }
}

pub fn start(input_dir: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<usize, String> {
    let paths = event_paths(input_dir)?;
    for path in &paths {
        let file = File::open(path).map_err(|e| format!("open input {}: {e}", path.display()))?;
        let path = path.clone();
        let label = path.display().to_string();
        let runtime = Arc::clone(&runtime);
        thread::Builder::new()
            .name(format!(
                "input-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("event")
            ))
            .spawn(move || {
                if let Err(error) = read_device(&path, file, runtime) {
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
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn press(bindings: &mut KeyBindings, code: u16) -> Option<Command> {
        bindings.feed(key(code, KEY_PRESS))
    }

    #[derive(Default)]
    struct RecordingTarget {
        commands: Vec<Command>,
        motions: Vec<(i32, i32)>,
    }

    impl InputTarget for RecordingTarget {
        fn command(&mut self, command: Command) -> Result<(), String> {
            self.commands.push(command);
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
                kind: EV_KEY,
                code: KEY_F,
                value: KEY_PRESS
            }
        );
        assert!(parse(bytes.get(..23).unwrap()).is_err());
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
            assert_eq!(press(&mut bindings, code), None);
            assert_eq!(press(&mut bindings, KEY_LEFTMETA), None);
            assert_eq!(press(&mut bindings, code), Some(Command::Focus(direction)));
            assert_eq!(press(&mut bindings, KEY_RIGHTSHIFT), None);
            assert_eq!(press(&mut bindings, code), Some(Command::Move(direction)));
            assert_eq!(bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE)), None);
            assert_eq!(press(&mut bindings, code), Some(Command::Focus(direction)));
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
            press(&mut bindings, KEY_F),
            Some(Command::Move(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTSHIFT, KEY_RELEASE));
        assert_eq!(
            press(&mut bindings, KEY_F),
            Some(Command::Focus(Direction::Right))
        );
        bindings.feed(key(KEY_RIGHTMETA, KEY_RELEASE));
        assert_eq!(press(&mut bindings, KEY_F), None);
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
                press(&mut bindings, code),
                Some(Command::SwitchWorkspace(number))
            );
            press(&mut bindings, KEY_LEFTSHIFT);
            assert_eq!(
                press(&mut bindings, code),
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
            assert_eq!(bindings.feed(key(KEY_X, KEY_RELEASE)), None);
            assert_eq!(press(&mut bindings, code), Some(expected));
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
            press(&mut bindings, KEY_2),
            Some(Command::SetSplit(Axis::Vertical))
        );
        press(&mut bindings, KEY_LEFTMETA);
        press(&mut bindings, KEY_X);
        assert_eq!(press(&mut bindings, 99), None);
        assert_eq!(
            press(&mut bindings, KEY_2),
            Some(Command::SwitchWorkspace(2))
        );
    }

    #[test]
    fn autorepeat_neither_runs_a_command_nor_consumes_a_prefix() {
        let mut bindings = KeyBindings::default();
        press(&mut bindings, KEY_LEFTMETA);
        assert_eq!(bindings.feed(key(KEY_F, KEY_REPEAT)), None);
        press(&mut bindings, KEY_X);
        assert_eq!(bindings.feed(key(KEY_2, KEY_REPEAT)), None);
        assert_eq!(
            press(&mut bindings, KEY_2),
            Some(Command::SetSplit(Axis::Vertical))
        );
    }

    #[test]
    fn pointer_motion_is_coalesced_at_syn_report_and_saturates() {
        let mut pointer = PointerMotion::default();
        assert_eq!(
            pointer.feed(Event {
                kind: EV_REL,
                code: REL_X,
                value: i32::MAX
            }),
            None
        );
        pointer.feed(Event {
            kind: EV_REL,
            code: REL_X,
            value: 2,
        });
        pointer.feed(Event {
            kind: EV_REL,
            code: REL_Y,
            value: -7,
        });
        assert_eq!(
            pointer.feed(Event {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0
            }),
            Some((i32::MAX, -7))
        );
        assert_eq!(
            pointer.feed(Event {
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
        let mut bindings = KeyBindings::default();
        let mut pointer = PointerMotion::default();
        for event in [
            key(KEY_LEFTMETA, KEY_PRESS),
            key(KEY_B, KEY_PRESS),
            Event {
                kind: EV_REL,
                code: REL_X,
                value: 3,
            },
            Event {
                kind: EV_REL,
                code: REL_Y,
                value: -2,
            },
            Event {
                kind: EV_SYN,
                code: SYN_REPORT,
                value: 0,
            },
        ] {
            apply(target.as_ref(), event, &mut bindings, &mut pointer).unwrap();
        }
        let target = target.lock().unwrap();
        assert_eq!(target.commands, [Command::Focus(Direction::Left)]);
        assert_eq!(target.motions, [(3, -2)]);
    }
}
