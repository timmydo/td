//! An SSH helper holds one admitted primary-account connection. Each
//! exchange takes a snapshot under the runtime mutex and writes after release.
use crate::desktop_wire::{Output, Reply, Request, MAX_PIXELS, MAX_REQUEST};
use crate::runtime::Runtime;
use crate::session::SocketPolicy;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) fn error(e: impl std::fmt::Display) -> String {
    crate::allocation::format(format_args!("{e}")).unwrap_or_default()
}

#[derive(Default)]
pub(crate) struct Seat {
    clock: std::sync::OnceLock<Instant>,
    pub attached: bool,
    pub paused: bool,
    pub protected: usize,
    pub lock_epoch: u64,
    pub remote_keys: [u64; 4],
    pub physical_keys: [u64; 4],
    pub physical_modifiers: crate::keyboard::ModifierState,
    pub locks: u32,
    pub buttons: u8,
    pub physical_buttons: u8,
}

impl Seat {
    pub(crate) fn time(&self) -> u32 {
        self.clock.get_or_init(Instant::now).elapsed().as_millis() as u32
    }
    pub(crate) fn held(keys: &[u64; 4], code: u32) -> bool {
        keys.get((code / 64) as usize)
            .is_some_and(|word| word & (1 << (code % 64)) != 0)
    }
    pub(crate) fn set(keys: &mut [u64; 4], code: u32, down: bool) {
        if let Some(word) = keys.get_mut((code / 64) as usize) {
            let mask = 1 << (code % 64);
            if down {
                *word |= mask;
            } else {
                *word &= !mask;
            }
        }
    }
    pub(crate) fn modifiers(&self) -> crate::keyboard::ModifierState {
        use crate::keyboard::*;
        let mut state = self.physical_modifiers;
        for (code, mask) in [
            (42, MOD_SHIFT),
            (54, MOD_SHIFT),
            (29, MOD_CONTROL),
            (97, MOD_CONTROL),
            (56, MOD_ALT),
            (100, MOD_ALT),
            (125, MOD_LOGO),
            (126, MOD_LOGO),
        ] {
            if Self::held(&self.remote_keys, code) {
                state.depressed |= mask;
            }
        }
        ModifierState {
            locked: state.locked ^ self.locks,
            ..state
        }
    }
}

#[derive(PartialEq, Eq)]
pub(crate) struct Image {
    pub width: u16,
    pub height: u16,
    pub output: Output,
    pub pixels: Vec<u8>,
}

pub(crate) fn geometry(width: usize, height: usize) -> Option<(u16, u16, Output)> {
    if width == 0 || height == 0 || width.checked_mul(height)? > MAX_PIXELS {
        return None;
    }
    let w = u16::try_from(width).ok()?;
    let h = u16::try_from(height).ok()?;
    // A compositor generation has a fixed output target.
    let id = std::num::NonZeroU64::new((u64::from(w) << 16) | u64::from(h))?;
    Some((w, h, Output(id)))
}

pub(crate) fn installed_boot(mut cmdline: impl Read) -> bool {
    let mut bytes = [0u8; 4097];
    let Ok(count) = std::io::copy(&mut cmdline.by_ref().take(4097), &mut bytes.as_mut_slice())
    else {
        return false;
    };
    let Some(line) = (count <= 4096)
        .then(|| bytes.get(..count as usize))
        .flatten()
        .and_then(|s| std::str::from_utf8(s).ok())
    else {
        return false;
    };
    let mut installed = false;
    for token in line.split_ascii_whitespace() {
        if token.starts_with("td.live=") {
            return false;
        }
        if let Some(id) = token.strip_prefix("td.deployment=") {
            if installed || id.is_empty() {
                return false;
            }
            installed = true;
        }
    }
    installed
}

pub(crate) fn start(path: &Path, runtime: Arc<Mutex<Runtime>>) -> Result<(), String> {
    let policy = SocketPolicy::HumanSession;
    let listener = crate::server::bind_listener(path, "remote desktop", policy)?;
    std::thread::Builder::new()
        .spawn(move || {
            for accepted in listener.incoming() {
                let Ok(stream) = accepted else {
                    break;
                };
                if policy.admit(&stream).is_err() {
                    continue;
                }
                let attach = runtime
                    .lock()
                    .ok()
                    .is_some_and(|mut rt| rt.attach_desktop());
                if !attach {
                    continue;
                }
                let active = Arc::clone(&runtime);
                if std::thread::Builder::new()
                    .spawn(move || {
                        let _ = session(stream, &active);
                        if let Ok(mut rt) = active.lock() {
                            let _ = rt.detach_desktop();
                        }
                    })
                    .is_err()
                {
                    if let Ok(mut rt) = runtime.lock() {
                        let _ = rt.detach_desktop();
                    }
                }
            }
        })
        .map_err(error)?;
    Ok(())
}

fn answer(stream: &mut UnixStream, reply: Reply<'_>) -> Result<(), String> {
    let size = match reply {
        Reply::Frame { pixels, .. } => 13 + pixels.len(),
        Reply::Revision(_) => 9,
        Reply::Text(s) => 1 + s.len(),
        _ => 1,
    };
    stream
        .write_all(&u32::try_from(size).map_err(error)?.to_be_bytes())
        .map_err(error)?;
    reply.write(stream).map_err(error)
}

pub(crate) fn session(mut stream: UnixStream, runtime: &Mutex<Runtime>) -> Result<(), String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(error)?;
    let mut body = Vec::new();
    let mut previous: Option<Image> = None;
    loop {
        let mut head = [0; 4];
        // A slow SSH picture may leave the helper idle between complete
        // requests. EOF releases its seat; only incomplete exchanges time out.
        let (first, rest) = head.split_at_mut(1);
        stream.set_read_timeout(None).map_err(error)?;
        stream.read_exact(first).map_err(error)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .map_err(error)?;
        stream.read_exact(rest).map_err(error)?;
        let size = usize::try_from(u32::from_be_bytes(head)).map_err(error)?;
        if !(1..=MAX_REQUEST).contains(&size) {
            return Err(error("invalid desktop request length"));
        }
        crate::allocation::resize(&mut body, size, 0).map_err(error)?;
        stream.read_exact(&mut body).map_err(error)?;
        let request = Request::decode(&body).map_err(error)?;
        match request {
            Request::Frame => {
                let image = runtime
                    .lock()
                    .map_err(|_| error("runtime lock poisoned"))?
                    .desktop_image()?;
                let reply = match image.as_ref() {
                    Some(i) if previous.as_ref() == Some(i) => Reply::Applied,
                    Some(i) => Reply::Frame {
                        output: i.output,
                        width: i.width,
                        height: i.height,
                        pixels: &i.pixels,
                    },
                    None => Reply::Unavailable,
                };
                answer(&mut stream, reply)?;
                previous = image;
            }
            Request::ReadText => {
                let revision = runtime
                    .lock()
                    .map_err(|_| error("runtime lock poisoned"))?
                    .desktop_snapshot();
                let bytes = revision.ok().and_then(|r| {
                    crate::vm_bridge::read_text(
                        runtime,
                        r,
                        Instant::now() + crate::vm_wire::TIMEOUT,
                        true,
                    )
                    .ok()
                });
                let reply = bytes
                    .as_deref()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .map_or(Reply::Unavailable, Reply::Text);
                answer(&mut stream, reply)?;
            }
            other => {
                let reply = runtime
                    .lock()
                    .map_err(|_| error("runtime lock poisoned"))?
                    .desktop_input(other)?;
                answer(&mut stream, reply)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_an_installed_boot_can_host_the_desktop() {
        assert!(installed_boot("quiet td.deployment=abc".as_bytes()));
        for line in [
            "",
            "quiet",
            "td.deployment=",
            "td.deployment=a td.deployment=b",
            "td.live=1 td.deployment=a",
            "td.deployment=a td.live=0",
            "td.live= td.deployment=a",
        ] {
            assert!(!installed_boot(line.as_bytes()));
        }
        assert!(!installed_boot([b'x'; 4097].as_slice()));
    }
}
