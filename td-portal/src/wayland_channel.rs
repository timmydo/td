//! Bounded proof of the compositor's private portal channel.

use crate::wayland_wire::{self, Builder, Cursor, Message};
use std::collections::BTreeSet;
use std::io;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const REGISTRY_ID: u32 = 2;
const REGISTRY_CALLBACK_ID: u32 = 3;
const COMPOSITOR_ID: u32 = 4;
const XDG_WM_BASE_ID: u32 = 5;
const PORTAL_MANAGER_ID: u32 = 6;
const SURFACE_ID: u32 = 7;
const XDG_SURFACE_ID: u32 = 8;
const XDG_TOPLEVEL_ID: u32 = 9;
const DIALOG_CALLBACK_ID: u32 = 10;
const PORTAL_DIALOG_STATE: u16 = 0;
const PORTAL_DIALOG_STANDALONE: u32 = 0;
const PORTAL_DIALOG_DISMISSED: u32 = 2;
const MAX_CHANNEL_BYTES: usize = 256 * 1024;
const MAX_CHANNEL_MESSAGES: usize = 32;
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(20);

const EXPECTED_GLOBALS: [(&str, u32); 11] = [
    ("wl_compositor", 4),
    ("wl_subcompositor", 1),
    ("wl_shm", 1),
    ("wl_output", 4),
    ("xdg_wm_base", 1),
    ("zxdg_decoration_manager_v1", 1),
    ("wl_data_device_manager", 3),
    ("zxdg_exporter_v2", 1),
    ("zxdg_importer_v2", 1),
    ("wl_seat", 7),
    ("td_portal_manager_v1", 1),
];

pub fn ready_marker() -> String {
    format!(
        "TD-PORTAL-CHANNEL-READY globals={} privileged=1 dialog=2",
        EXPECTED_GLOBALS.len()
    )
}

#[derive(Debug, Eq, PartialEq)]
struct Global {
    name: u32,
    interface: String,
    version: u32,
}

#[derive(Debug, Eq, PartialEq)]
struct RequiredGlobals {
    compositor: u32,
    xdg_wm_base: u32,
    portal_manager: u32,
}

fn global(message: &Message) -> Result<Global, String> {
    let mut cursor = Cursor::new(&message.payload);
    let value = Global {
        name: cursor.u32()?,
        interface: cursor.string()?,
        version: cursor.u32()?,
    };
    cursor.finish()?;
    if value.name == 0 {
        return Err("private portal registry advertised global name zero".into());
    }
    Ok(value)
}

fn validate_globals(globals: &[Global]) -> Result<RequiredGlobals, String> {
    if globals.len() != EXPECTED_GLOBALS.len() {
        return Err(format!(
            "private portal registry advertised {} globals, expected {}",
            globals.len(),
            EXPECTED_GLOBALS.len()
        ));
    }
    let mut names = BTreeSet::new();
    for (observed, (interface, version)) in globals.iter().zip(EXPECTED_GLOBALS) {
        if !names.insert(observed.name) {
            return Err(format!(
                "private portal registry repeated global name {}",
                observed.name
            ));
        }
        if observed.interface != interface || observed.version != version {
            return Err(format!(
                "private portal registry advertised {} v{}, expected {interface} v{version}",
                observed.interface, observed.version
            ));
        }
    }
    let compositor = globals
        .first()
        .map(|global| global.name)
        .ok_or_else(|| "private portal registry omitted wl_compositor".to_string())?;
    let xdg_wm_base = globals
        .get(4)
        .map(|global| global.name)
        .ok_or_else(|| "private portal registry omitted xdg_wm_base".to_string())?;
    let portal_manager = globals
        .get(10)
        .map(|global| global.name)
        .ok_or_else(|| "private portal registry omitted td_portal_manager_v1".to_string())?;
    Ok(RequiredGlobals {
        compositor,
        xdg_wm_base,
        portal_manager,
    })
}

fn remaining(until: Instant) -> Result<Duration, String> {
    until
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "private portal registry probe timed out".to_string())
}

fn connect_until_with<F>(path: PathBuf, until: Instant, connect: F) -> Result<UnixStream, String>
where
    F: FnOnce(&Path) -> io::Result<UnixStream> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("portal-wayland-connect".into())
        .spawn(move || {
            let result = connect(&path);
            let _ = sender.send((path, result));
        })
        .map_err(|error| format!("spawn private portal Wayland connector: {error}"))?;
    match receiver.recv_timeout(remaining(until)?) {
        Ok((_, Ok(stream))) => Ok(stream),
        Ok((path, Err(error))) => Err(format!(
            "connect private portal Wayland socket {}: {error}",
            path.display()
        )),
        Err(RecvTimeoutError::Timeout) => {
            Err("private portal registry probe timed out during connect".into())
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err("private portal Wayland connector stopped without a result".into())
        }
    }
}

fn charge_channel_bytes(received: &mut usize, count: usize) -> Result<(), String> {
    *received = received
        .checked_add(count)
        .ok_or_else(|| "private portal channel byte count overflow".to_string())?;
    if *received > MAX_CHANNEL_BYTES {
        return Err(format!(
            "private portal channel exceeded {MAX_CHANNEL_BYTES} cumulative bytes"
        ));
    }
    Ok(())
}

enum RegistryProgress {
    Pending,
    Complete(RequiredGlobals),
}

fn consume_registry(
    bytes: &mut Vec<u8>,
    globals: &mut Vec<Global>,
    messages: &mut usize,
) -> Result<RegistryProgress, String> {
    while let Some(message) = wayland_wire::take(bytes)? {
        *messages = messages.saturating_add(1);
        if *messages > MAX_CHANNEL_MESSAGES {
            return Err(format!(
                "private portal registry exceeded {MAX_CHANNEL_MESSAGES} messages"
            ));
        }
        match (message.object, message.opcode) {
            (REGISTRY_ID, 0) => globals.push(global(&message)?),
            (REGISTRY_CALLBACK_ID, 0) => {
                let mut cursor = Cursor::new(&message.payload);
                if cursor.u32()? == 0 {
                    return Err("private portal registry sync used serial zero".into());
                }
                cursor.finish()?;
                let required = validate_globals(globals)?;
                // The callback is the requested protocol boundary. A following
                // delete_id belongs after it and may be split at any byte.
                return Ok(RegistryProgress::Complete(required));
            }
            _ => {
                return Err(format!(
                    "private portal registry sent unexpected object {} opcode {}",
                    message.object, message.opcode
                ))
            }
        }
    }
    Ok(RegistryProgress::Pending)
}

fn send_request(
    stream: &mut UnixStream,
    until: Instant,
    object: u32,
    opcode: u16,
    payload: Builder,
    operation: &str,
) -> Result<(), String> {
    stream
        .set_write_timeout(Some(remaining(until)?))
        .map_err(|error| format!("set private portal write timeout: {error}"))?;
    stream
        .write_all(&payload.message(object, opcode)?)
        .map_err(|error| format!("{operation}: {error}"))
}

fn bind_global(
    stream: &mut UnixStream,
    until: Instant,
    name: u32,
    interface: &str,
    object: u32,
) -> Result<(), String> {
    let mut request = Builder::new();
    request.u32(name);
    request.string(interface)?;
    request.u32(1);
    request.u32(object);
    send_request(
        stream,
        until,
        REGISTRY_ID,
        0,
        request,
        &format!("bind private portal global {interface}"),
    )
}

fn request_dialog_proof(
    stream: &mut UnixStream,
    until: Instant,
    globals: &RequiredGlobals,
) -> Result<(), String> {
    bind_global(
        stream,
        until,
        globals.compositor,
        "wl_compositor",
        COMPOSITOR_ID,
    )?;
    bind_global(
        stream,
        until,
        globals.xdg_wm_base,
        "xdg_wm_base",
        XDG_WM_BASE_ID,
    )?;
    bind_global(
        stream,
        until,
        globals.portal_manager,
        "td_portal_manager_v1",
        PORTAL_MANAGER_ID,
    )?;

    let mut create_surface = Builder::new();
    create_surface.u32(SURFACE_ID);
    send_request(
        stream,
        until,
        COMPOSITOR_ID,
        0,
        create_surface,
        "create private portal proof surface",
    )?;
    let mut create_xdg_surface = Builder::new();
    create_xdg_surface.u32(XDG_SURFACE_ID);
    create_xdg_surface.u32(SURFACE_ID);
    send_request(
        stream,
        until,
        XDG_WM_BASE_ID,
        2,
        create_xdg_surface,
        "assign private portal proof xdg_surface",
    )?;
    let mut create_toplevel = Builder::new();
    create_toplevel.u32(XDG_TOPLEVEL_ID);
    send_request(
        stream,
        until,
        XDG_SURFACE_ID,
        1,
        create_toplevel,
        "assign private portal proof toplevel",
    )?;

    let mut associate = Builder::new();
    associate.u32(SURFACE_ID);
    associate.string("")?;
    associate.u32(0);
    send_request(
        stream,
        until,
        PORTAL_MANAGER_ID,
        0,
        associate,
        "request standalone private portal dialog",
    )?;
    let mut dismiss = Builder::new();
    dismiss.u32(SURFACE_ID);
    send_request(
        stream,
        until,
        PORTAL_MANAGER_ID,
        1,
        dismiss,
        "dismiss private portal dialog",
    )?;
    let mut sync = Builder::new();
    sync.u32(DIALOG_CALLBACK_ID);
    send_request(
        stream,
        until,
        1,
        0,
        sync,
        "request private portal dialog boundary",
    )
}

enum DialogProgress {
    Pending,
    Complete,
}

fn consume_dialog(
    bytes: &mut Vec<u8>,
    states: &mut Vec<u32>,
    messages: &mut usize,
) -> Result<DialogProgress, String> {
    while let Some(message) = wayland_wire::take(bytes)? {
        *messages = messages.saturating_add(1);
        if *messages > MAX_CHANNEL_MESSAGES {
            return Err(format!(
                "private portal channel exceeded {MAX_CHANNEL_MESSAGES} messages"
            ));
        }
        match (message.object, message.opcode) {
            (PORTAL_MANAGER_ID, PORTAL_DIALOG_STATE) => {
                let mut cursor = Cursor::new(&message.payload);
                if cursor.u32()? != SURFACE_ID {
                    return Err("private portal manager answered for the wrong surface".into());
                }
                states.push(cursor.u32()?);
                cursor.finish()?;
                if states.len() > 2 {
                    return Err("private portal manager repeated dialog state".into());
                }
            }
            (DIALOG_CALLBACK_ID, 0) => {
                let mut cursor = Cursor::new(&message.payload);
                if cursor.u32()? == 0 {
                    return Err("private portal dialog sync used serial zero".into());
                }
                cursor.finish()?;
                if states.as_slice() != [PORTAL_DIALOG_STANDALONE, PORTAL_DIALOG_DISMISSED] {
                    return Err(format!(
                        "private portal manager returned dialog states {states:?}, expected [{PORTAL_DIALOG_STANDALONE}, {PORTAL_DIALOG_DISMISSED}]"
                    ));
                }
                return Ok(DialogProgress::Complete);
            }
            (1, 1) => {
                let mut cursor = Cursor::new(&message.payload);
                let deleted = cursor.u32()?;
                cursor.finish()?;
                if deleted != REGISTRY_CALLBACK_ID {
                    return Err(format!(
                        "private portal channel deleted unexpected object {deleted}"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "private portal dialog sent unexpected object {} opcode {}",
                    message.object, message.opcode
                ))
            }
        }
    }
    Ok(DialogProgress::Pending)
}

fn probe_with_timeout(path: &Path, timeout: Duration) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("private portal Wayland socket path must be absolute".into());
    }
    let until = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "private portal registry deadline overflow".to_string())?;
    let mut stream =
        connect_until_with(path.to_path_buf(), until, |path| UnixStream::connect(path))?;
    let mut registry = Builder::new();
    registry.u32(REGISTRY_ID);
    send_request(
        &mut stream,
        until,
        1,
        1,
        registry,
        "request private portal registry",
    )?;
    let mut sync = Builder::new();
    sync.u32(REGISTRY_CALLBACK_ID);
    send_request(
        &mut stream,
        until,
        1,
        0,
        sync,
        "request private portal registry boundary",
    )?;

    let mut bytes = Vec::new();
    let mut scratch = [0u8; 4096];
    let mut globals = Vec::new();
    let mut messages = 0usize;
    let mut received_bytes = 0usize;
    loop {
        stream
            .set_read_timeout(Some(remaining(until)?))
            .map_err(|error| format!("set private portal read timeout: {error}"))?;
        let count = stream
            .read(&mut scratch)
            .map_err(|error| format!("read private portal registry: {error}"))?;
        if count == 0 {
            return Err("private portal Wayland socket closed before registry sync".into());
        }
        charge_channel_bytes(&mut received_bytes, count)?;
        bytes.extend_from_slice(
            scratch
                .get(..count)
                .ok_or_else(|| "private portal read escaped its buffer".to_string())?,
        );
        if let RegistryProgress::Complete(required) =
            consume_registry(&mut bytes, &mut globals, &mut messages)?
        {
            request_dialog_proof(&mut stream, until, &required)?;
            break;
        }
    }

    let mut states = Vec::new();
    loop {
        if matches!(
            consume_dialog(&mut bytes, &mut states, &mut messages)?,
            DialogProgress::Complete
        ) {
            return Ok(());
        }
        stream
            .set_read_timeout(Some(remaining(until)?))
            .map_err(|error| format!("set private portal read timeout: {error}"))?;
        let count = stream
            .read(&mut scratch)
            .map_err(|error| format!("read private portal dialog: {error}"))?;
        if count == 0 {
            return Err("private portal Wayland socket closed before dialog sync".into());
        }
        charge_channel_bytes(&mut received_bytes, count)?;
        bytes.extend_from_slice(
            scratch
                .get(..count)
                .ok_or_else(|| "private portal read escaped its buffer".to_string())?,
        );
    }
}

pub fn probe(path: &Path) -> Result<(), String> {
    probe_with_timeout(path, CHANNEL_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn socket_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "td-portal-channel-{label}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn serve_registry(
        globals: Vec<(&'static str, u32)>,
    ) -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let path = socket_path("registry");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut requests = Vec::new();
            let mut seen = Vec::new();
            let mut scratch = [0u8; 256];
            loop {
                while let Some(message) = wayland_wire::take(&mut requests).unwrap() {
                    seen.push((message.object, message.opcode));
                }
                if seen == vec![(1, 1), (1, 0)] {
                    break;
                }
                assert!(seen.len() < 2, "unexpected requests {seen:?}");
                let count = stream.read(&mut scratch).unwrap();
                assert!(count > 0);
                requests.extend_from_slice(&scratch[..count]);
            }
            for (index, (interface, version)) in globals.into_iter().enumerate() {
                let mut event = Builder::new();
                event.u32(u32::try_from(index + 1).unwrap());
                event.string(interface).unwrap();
                event.u32(version);
                stream
                    .write_all(&event.message(REGISTRY_ID, 0).unwrap())
                    .unwrap();
            }
            let mut done = Builder::new();
            done.u32(1);
            stream
                .write_all(&done.message(REGISTRY_CALLBACK_ID, 0).unwrap())
                .unwrap();
        });
        (path, worker)
    }

    fn read_requests(stream: &mut UnixStream, expected: usize) -> Vec<Message> {
        let mut bytes = Vec::new();
        let mut messages = Vec::new();
        let mut scratch = [0u8; 512];
        while messages.len() < expected {
            while let Some(message) = wayland_wire::take(&mut bytes).unwrap() {
                messages.push(message);
            }
            if messages.len() == expected {
                break;
            }
            assert!(messages.len() < expected);
            let count = stream.read(&mut scratch).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&scratch[..count]);
        }
        assert!(bytes.is_empty());
        messages
    }

    fn serve_channel() -> (std::path::PathBuf, thread::JoinHandle<()>) {
        let path = socket_path("manager");
        let listener = UnixListener::bind(&path).unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let initial = read_requests(&mut stream, 2);
            assert_eq!(
                initial
                    .iter()
                    .map(|message| (message.object, message.opcode))
                    .collect::<Vec<_>>(),
                [(1, 1), (1, 0)]
            );
            for (index, (interface, version)) in EXPECTED_GLOBALS.into_iter().enumerate() {
                let mut event = Builder::new();
                event.u32(u32::try_from(index + 1).unwrap());
                event.string(interface).unwrap();
                event.u32(version);
                stream
                    .write_all(&event.message(REGISTRY_ID, 0).unwrap())
                    .unwrap();
            }
            let mut done = Builder::new();
            done.u32(1);
            stream
                .write_all(&done.message(REGISTRY_CALLBACK_ID, 0).unwrap())
                .unwrap();

            let requests = read_requests(&mut stream, 9);
            assert_eq!(
                requests
                    .iter()
                    .map(|message| (message.object, message.opcode))
                    .collect::<Vec<_>>(),
                [
                    (REGISTRY_ID, 0),
                    (REGISTRY_ID, 0),
                    (REGISTRY_ID, 0),
                    (COMPOSITOR_ID, 0),
                    (XDG_WM_BASE_ID, 2),
                    (XDG_SURFACE_ID, 1),
                    (PORTAL_MANAGER_ID, 0),
                    (PORTAL_MANAGER_ID, 1),
                    (1, 0),
                ]
            );
            let expected_bindings = [
                (1, "wl_compositor", COMPOSITOR_ID),
                (5, "xdg_wm_base", XDG_WM_BASE_ID),
                (11, "td_portal_manager_v1", PORTAL_MANAGER_ID),
            ];
            for (request, (name, interface, object)) in
                requests.iter().take(3).zip(expected_bindings)
            {
                let mut cursor = Cursor::new(&request.payload);
                assert_eq!(cursor.u32().unwrap(), name);
                assert_eq!(cursor.string().unwrap(), interface);
                assert_eq!(cursor.u32().unwrap(), 1);
                assert_eq!(cursor.u32().unwrap(), object);
                cursor.finish().unwrap();
            }
            let expected_words: [&[u32]; 4] = [
                &[SURFACE_ID],
                &[XDG_SURFACE_ID, SURFACE_ID],
                &[XDG_TOPLEVEL_ID],
                &[SURFACE_ID],
            ];
            for (request, words) in requests
                .iter()
                .skip(3)
                .take(3)
                .chain(requests.iter().skip(7).take(1))
                .zip(expected_words)
            {
                let mut cursor = Cursor::new(&request.payload);
                for expected in words {
                    assert_eq!(cursor.u32().unwrap(), *expected);
                }
                cursor.finish().unwrap();
            }
            let mut associate = Cursor::new(&requests[6].payload);
            assert_eq!(associate.u32().unwrap(), SURFACE_ID);
            assert_eq!(associate.string().unwrap(), "");
            assert_eq!(associate.u32().unwrap(), 0);
            associate.finish().unwrap();
            let mut callback = Cursor::new(&requests[8].payload);
            assert_eq!(callback.u32().unwrap(), DIALOG_CALLBACK_ID);
            callback.finish().unwrap();

            let mut deleted = Builder::new();
            deleted.u32(REGISTRY_CALLBACK_ID);
            stream.write_all(&deleted.message(1, 1).unwrap()).unwrap();
            for state in [PORTAL_DIALOG_STANDALONE, PORTAL_DIALOG_DISMISSED] {
                let mut event = Builder::new();
                event.u32(SURFACE_ID);
                event.u32(state);
                stream
                    .write_all(
                        &event
                            .message(PORTAL_MANAGER_ID, PORTAL_DIALOG_STATE)
                            .unwrap(),
                    )
                    .unwrap();
            }
            let mut done = Builder::new();
            done.u32(2);
            stream
                .write_all(&done.message(DIALOG_CALLBACK_ID, 0).unwrap())
                .unwrap();
        });
        (path, worker)
    }

    #[test]
    fn exact_private_registry_and_dialog_lifecycle_prove_the_channel() {
        let (path, worker) = serve_channel();
        probe(&path).unwrap();
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            ready_marker(),
            "TD-PORTAL-CHANNEL-READY globals=11 privileged=1 dialog=2"
        );
    }

    #[test]
    fn missing_privileged_or_changed_public_globals_are_refused() {
        let mut missing = EXPECTED_GLOBALS.to_vec();
        missing.pop();
        let (path, worker) = serve_registry(missing);
        assert!(probe(&path).is_err());
        worker.join().unwrap();
        fs::remove_file(path).unwrap();

        let mut changed = EXPECTED_GLOBALS.to_vec();
        changed.swap(0, 1);
        let (path, worker) = serve_registry(changed);
        assert!(probe(&path).is_err());
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_split_delete_id_after_sync_does_not_change_the_proof() {
        let mut bytes = Vec::new();
        for (index, (interface, version)) in EXPECTED_GLOBALS.into_iter().enumerate() {
            let mut event = Builder::new();
            event.u32(u32::try_from(index + 1).unwrap());
            event.string(interface).unwrap();
            event.u32(version);
            bytes.extend_from_slice(&event.message(REGISTRY_ID, 0).unwrap());
        }
        let mut done = Builder::new();
        done.u32(1);
        bytes.extend_from_slice(&done.message(REGISTRY_CALLBACK_ID, 0).unwrap());
        let mut deleted = Builder::new();
        deleted.u32(REGISTRY_CALLBACK_ID);
        let deleted = deleted.message(1, 1).unwrap();
        bytes.extend_from_slice(&deleted[..5]);

        let mut globals = Vec::new();
        let mut messages = 0;
        assert!(matches!(
            consume_registry(&mut bytes, &mut globals, &mut messages).unwrap(),
            RegistryProgress::Complete(_)
        ));
        assert_eq!(bytes, deleted[..5]);
    }

    #[test]
    fn complete_messages_still_share_one_cumulative_byte_budget() {
        let interface = "x".repeat(60 * 1024);
        let mut received = 0usize;
        let mut globals = Vec::new();
        let mut messages = 0usize;
        for name in 1..=4 {
            let mut event = Builder::new();
            event.u32(name);
            event.string(&interface).unwrap();
            event.u32(1);
            let mut bytes = event.message(REGISTRY_ID, 0).unwrap();
            charge_channel_bytes(&mut received, bytes.len()).unwrap();
            assert!(matches!(
                consume_registry(&mut bytes, &mut globals, &mut messages).unwrap(),
                RegistryProgress::Pending
            ));
            assert!(bytes.is_empty());
        }
        let mut event = Builder::new();
        event.u32(5);
        event.string(&interface).unwrap();
        event.u32(1);
        let bytes = event.message(REGISTRY_ID, 0).unwrap();
        let error = charge_channel_bytes(&mut received, bytes.len()).unwrap_err();
        assert!(error.contains("cumulative bytes"), "{error}");
        assert_eq!(globals.len(), 4);
    }

    #[test]
    fn dialog_boundary_requires_both_exact_states_in_order() {
        fn encoded(states: &[u32]) -> Vec<u8> {
            let mut bytes = Vec::new();
            for state in states {
                let mut event = Builder::new();
                event.u32(SURFACE_ID);
                event.u32(*state);
                bytes.extend_from_slice(
                    &event
                        .message(PORTAL_MANAGER_ID, PORTAL_DIALOG_STATE)
                        .unwrap(),
                );
            }
            let mut done = Builder::new();
            done.u32(2);
            bytes.extend_from_slice(&done.message(DIALOG_CALLBACK_ID, 0).unwrap());
            bytes
        }

        for states in [
            Vec::new(),
            vec![PORTAL_DIALOG_STANDALONE],
            vec![PORTAL_DIALOG_DISMISSED, PORTAL_DIALOG_STANDALONE],
        ] {
            let mut bytes = encoded(&states);
            let mut observed = Vec::new();
            assert!(consume_dialog(&mut bytes, &mut observed, &mut 0).is_err());
        }
    }

    #[test]
    fn a_blocked_connect_is_inside_the_channel_deadline() {
        let blocked = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_blocked = Arc::clone(&blocked);
        let until = Instant::now() + Duration::from_millis(30);
        let result = connect_until_with(PathBuf::from("/blocked"), until, move |_| {
            let (lock, wake) = &*worker_blocked;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            Err(io::Error::new(io::ErrorKind::WouldBlock, "released"))
        });
        assert!(result.unwrap_err().contains("timed out during connect"));
        let (lock, wake) = &*blocked;
        *lock.lock().unwrap() = true;
        wake.notify_one();
    }

    #[test]
    fn compositor_registry_stays_equal_to_the_probe_contract() {
        const SERVER: &str = include_str!("../../td-compositor/src/server.rs");
        const NAMES: [&str; 10] = [
            "GLOBAL_COMPOSITOR",
            "GLOBAL_SUBCOMPOSITOR",
            "GLOBAL_SHM",
            "GLOBAL_OUTPUT",
            "GLOBAL_XDG_WM_BASE",
            "GLOBAL_DECORATION",
            "GLOBAL_DATA_DEVICE_MANAGER",
            "GLOBAL_XDG_EXPORTER",
            "GLOBAL_XDG_IMPORTER",
            "GLOBAL_SEAT",
        ];
        const VERSIONS: [&str; 10] = [
            "4",
            "1",
            "1",
            "4",
            "XDG_WM_BASE_VERSION",
            "1",
            "DATA_DEVICE_MANAGER_VERSION",
            "XDG_FOREIGN_VERSION",
            "XDG_FOREIGN_VERSION",
            "SEAT_VERSION",
        ];

        let compact: String = SERVER
            .chars()
            .filter(|byte| !byte.is_whitespace())
            .collect();
        let start = compact.find("constPUBLIC_GLOBALS:").unwrap();
        let declaration = &compact[start..];
        let end = declaration.find("];").unwrap() + 2;
        let declaration = declaration[..end].replace(",),", "),");
        let mut expected = format!(
            "constPUBLIC_GLOBALS:[(u32,&str,u32);{}]=[",
            EXPECTED_GLOBALS.len() - 1
        );
        for (((name, version_token), (interface, version)), index) in NAMES
            .into_iter()
            .zip(VERSIONS)
            .zip(EXPECTED_GLOBALS.into_iter().take(10))
            .zip(0..)
        {
            let expected_version = match version_token {
                "XDG_WM_BASE_VERSION" | "XDG_FOREIGN_VERSION" => 1,
                "DATA_DEVICE_MANAGER_VERSION" => 3,
                "SEAT_VERSION" => 7,
                literal => literal.parse::<u32>().unwrap(),
            };
            assert_eq!(version, expected_version, "global {index}");
            expected.push_str(&format!("({name},\"{interface}\",{version_token}),"));
        }
        expected.push_str("];");
        assert_eq!(declaration, expected);
        for declaration in [
            "constXDG_WM_BASE_VERSION:u32=1;",
            "constDATA_DEVICE_MANAGER_VERSION:u32=3;",
            "constXDG_FOREIGN_VERSION:u32=1;",
            "constSEAT_VERSION:u32=7;",
        ] {
            assert!(compact.contains(declaration));
        }
        assert!(compact.contains(
            "constPORTAL_MANAGER_GLOBAL:(u32,&str,u32)=(GLOBAL_PORTAL_MANAGER,\"td_portal_manager_v1\",1);"
        ));
    }
}
