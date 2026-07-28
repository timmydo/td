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
const KEY_LEFT: u16 = 105;
const KEY_RIGHT: u16 = 106;
const KEY_PRESS: i32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Event {
    kind: u16,
    code: u16,
    value: i32,
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

fn apply(
    runtime: &Arc<Mutex<Runtime>>,
    event: Event,
    dx: &mut i32,
    dy: &mut i32,
) -> Result<(), String> {
    match (event.kind, event.code, event.value) {
        (EV_KEY, KEY_LEFT, KEY_PRESS) => runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .focus_left(),
        (EV_KEY, KEY_RIGHT, KEY_PRESS) => runtime
            .lock()
            .map_err(|_| "runtime lock poisoned".to_string())?
            .focus_right(),
        (EV_REL, REL_X, value) => {
            *dx = dx.saturating_add(value);
            Ok(())
        }
        (EV_REL, REL_Y, value) => {
            *dy = dy.saturating_add(value);
            Ok(())
        }
        (EV_SYN, SYN_REPORT, _) if *dx != 0 || *dy != 0 => {
            let pending_x = std::mem::take(dx);
            let pending_y = std::mem::take(dy);
            runtime
                .lock()
                .map_err(|_| "runtime lock poisoned".to_string())?
                .move_pointer(pending_x, pending_y)
        }
        _ => Ok(()),
    }
}

fn read_device(path: &Path, mut file: File, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let mut bytes = [0u8; EVENT_SIZE];
    let mut dx = 0i32;
    let mut dy = 0i32;
    loop {
        match file.read_exact(&mut bytes) {
            Ok(()) => {
                let event = parse(&bytes)?;
                apply(&runtime, event, &mut dx, &mut dy)?;
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
            .copy_from_slice(&KEY_RIGHT.to_ne_bytes());
        bytes
            .get_mut(20..24)
            .unwrap()
            .copy_from_slice(&KEY_PRESS.to_ne_bytes());
        assert_eq!(
            parse(&bytes).unwrap(),
            Event {
                kind: EV_KEY,
                code: KEY_RIGHT,
                value: KEY_PRESS
            }
        );
    }

    #[test]
    fn input_node_names_are_narrow() {
        assert!(event_name("event0"));
        assert!(!event_name("event"));
        assert!(!event_name("event0-old"));
    }
}
