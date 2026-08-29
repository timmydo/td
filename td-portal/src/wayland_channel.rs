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
const CALLBACK_ID: u32 = 3;
const MAX_CHANNEL_BYTES: usize = 256 * 1024;
const MAX_CHANNEL_MESSAGES: usize = 32;
const CHANNEL_TIMEOUT: Duration = Duration::from_secs(20);

const EXPECTED_GLOBALS: [(&str, u32); 10] = [
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
];

pub fn ready_marker() -> String {
    format!(
        "TD-PORTAL-CHANNEL-READY globals={} privileged=0",
        EXPECTED_GLOBALS.len()
    )
}

#[derive(Debug, Eq, PartialEq)]
struct Global {
    name: u32,
    interface: String,
    version: u32,
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

fn validate_globals(globals: &[Global]) -> Result<(), String> {
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
    Ok(())
}

fn remaining(until: Instant) -> Result<Duration, String> {
    until
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| "private portal registry probe timed out".to_string())
}

fn connect_until_with<F>(path: PathBuf, until: Instant, connect: F) -> Result<UnixStream, String>
where
    F: FnOnce(PathBuf) -> io::Result<UnixStream> + Send + 'static,
{
    let display = path.display().to_string();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("portal-wayland-connect".into())
        .spawn(move || {
            let _ = sender.send(connect(path));
        })
        .map_err(|error| format!("spawn private portal Wayland connector: {error}"))?;
    match receiver.recv_timeout(remaining(until)?) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(format!(
            "connect private portal Wayland socket {display}: {error}"
        )),
        Err(RecvTimeoutError::Timeout) => {
            Err("private portal registry probe timed out during connect".into())
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err("private portal Wayland connector stopped without a result".into())
        }
    }
}

enum RegistryProgress {
    Pending,
    Complete,
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
            (CALLBACK_ID, 0) => {
                let mut cursor = Cursor::new(&message.payload);
                if cursor.u32()? == 0 {
                    return Err("private portal registry sync used serial zero".into());
                }
                cursor.finish()?;
                validate_globals(globals)?;
                // The callback is the requested protocol boundary. A following
                // delete_id belongs after it and may be split at any byte.
                return Ok(RegistryProgress::Complete);
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

fn probe_with_timeout(path: &Path, timeout: Duration) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("private portal Wayland socket path must be absolute".into());
    }
    let until = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "private portal registry deadline overflow".to_string())?;
    let mut stream = connect_until_with(path.to_path_buf(), until, UnixStream::connect)?;
    stream
        .set_write_timeout(Some(remaining(until)?))
        .map_err(|error| format!("set private portal write timeout: {error}"))?;

    let mut registry = Builder::new();
    registry.u32(REGISTRY_ID);
    stream
        .write_all(&registry.message(1, 1)?)
        .map_err(|error| format!("request private portal registry: {error}"))?;
    let mut sync = Builder::new();
    sync.u32(CALLBACK_ID);
    stream
        .write_all(&sync.message(1, 0)?)
        .map_err(|error| format!("request private portal registry boundary: {error}"))?;

    let mut bytes = Vec::new();
    let mut scratch = [0u8; 4096];
    let mut globals = Vec::new();
    let mut messages = 0usize;
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
        let next = bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| "private portal registry byte count overflow".to_string())?;
        if next > MAX_CHANNEL_BYTES {
            return Err(format!(
                "private portal registry exceeded {MAX_CHANNEL_BYTES} bytes"
            ));
        }
        bytes.extend_from_slice(
            scratch
                .get(..count)
                .ok_or_else(|| "private portal read escaped its buffer".to_string())?,
        );
        if matches!(
            consume_registry(&mut bytes, &mut globals, &mut messages)?,
            RegistryProgress::Complete
        ) {
            return Ok(());
        }
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
                .write_all(&done.message(CALLBACK_ID, 0).unwrap())
                .unwrap();
        });
        (path, worker)
    }

    #[test]
    fn exact_public_registry_proves_the_private_channel() {
        let (path, worker) = serve_registry(EXPECTED_GLOBALS.to_vec());
        probe(&path).unwrap();
        worker.join().unwrap();
        fs::remove_file(path).unwrap();
        assert_eq!(
            ready_marker(),
            "TD-PORTAL-CHANNEL-READY globals=10 privileged=0"
        );
    }

    #[test]
    fn premature_privileged_or_changed_public_globals_are_refused() {
        let mut privileged = EXPECTED_GLOBALS.to_vec();
        privileged.push(("td_portal_manager_v1", 1));
        let (path, worker) = serve_registry(privileged);
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
        bytes.extend_from_slice(&done.message(CALLBACK_ID, 0).unwrap());
        let mut deleted = Builder::new();
        deleted.u32(CALLBACK_ID);
        let deleted = deleted.message(1, 1).unwrap();
        bytes.extend_from_slice(&deleted[..5]);

        let mut globals = Vec::new();
        let mut messages = 0;
        assert!(matches!(
            consume_registry(&mut bytes, &mut globals, &mut messages).unwrap(),
            RegistryProgress::Complete
        ));
        assert_eq!(bytes, deleted[..5]);
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
        let start = compact.find("constADVERTISED_GLOBALS:").unwrap();
        let declaration = &compact[start..];
        let end = declaration.find("];").unwrap() + 2;
        let declaration = declaration[..end].replace(",),", "),");
        let mut expected = format!(
            "constADVERTISED_GLOBALS:[(u32,&str,u32);{}]=[",
            EXPECTED_GLOBALS.len()
        );
        for (((name, version_token), (interface, version)), index) in NAMES
            .into_iter()
            .zip(VERSIONS)
            .zip(EXPECTED_GLOBALS)
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
    }
}
