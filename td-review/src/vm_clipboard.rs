//! Explicit host-terminal clipboard actions. No clipboard queries or watches.
use crate::{term, vm_wire};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::thread;
use std::time::{Duration, Instant};

const BEGIN: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

#[derive(Default)]
struct Paste {
    bytes: Vec<u8>,
    active: bool,
    oversized: bool,
}

impl Paste {
    fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>, String> {
        self.bytes.push(byte);
        if !self.active {
            if self.bytes == BEGIN {
                self.bytes.clear();
                self.active = true;
            } else if !BEGIN.starts_with(&self.bytes) {
                return Err("Paste cancelled; use the host terminal's paste action".into());
            }
        } else {
            if self.bytes.ends_with(END) {
                if self.oversized {
                    return Err("Paste exceeds 64 KiB".into());
                }
                self.bytes
                    .truncate(self.bytes.len().saturating_sub(END.len()));
                vm_wire::text(&self.bytes)?;
                if self.bytes.is_empty() {
                    return Err("clipboard import needs nonempty text".into());
                }
                return Ok(Some(std::mem::take(&mut self.bytes)));
            }
            if self.bytes.len() > vm_wire::MAX_TEXT + END.len() {
                self.oversized = true;
            }
            if self.oversized {
                // Consume the rest of this paste frame before returning to
                // the menu; a delayed tail must not become TUI commands.
                let keep_from = self.bytes.len().saturating_sub(END.len());
                self.bytes.drain(..keep_from);
            }
        }
        Ok(None)
    }
}

pub fn capture(terminal: &mut term::Terminal, name: &str) -> Result<Vec<u8>, String> {
    terminal.drain_input().map_err(|e| e.to_string())?;
    let (rows, cols) = terminal.size();
    let mut frame = term::Frame::new(rows, cols);
    frame.push_text(&format!("Paste text for {name}"), term::Style::bold());
    frame.push_text(
        "Use your host terminal's Paste action. Ctrl+C cancels while waiting. No text is sent yet.",
        term::Style::PLAIN,
    );
    terminal.draw(frame.finish()).map_err(|e| e.to_string())?;
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(0x800)
        .open("/dev/tty")
        .map_err(|e| e.to_string())?;
    terminal
        .draw("\x1b[?2004h".into())
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut paste = Paste::default();
        let mut buffer = [0; 4096];
        loop {
            if Instant::now() >= deadline {
                return Err("Paste prompt timed out".into());
            }
            match input.read(&mut buffer) {
                Ok(0) => return Err("Host terminal closed".into()),
                Ok(n) => {
                    for byte in buffer.get(..n).ok_or("paste input length")? {
                        if let Some(bytes) = paste.push(*byte)? {
                            return Ok(bytes);
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(format!("read host paste: {e}")),
            }
        }
    })();
    let reset = terminal
        .draw("\x1b[?2004l".into())
        .map_err(|e| e.to_string());
    reset?;
    result
}

pub fn copy(terminal: &mut term::Terminal, bytes: &[u8]) -> Result<(), String> {
    vm_wire::text(bytes)?;
    // Only this explicit action emits OSC 52. Guest bytes are base64 data,
    // never interpreted terminal escapes, and no OSC clipboard read is used.
    terminal
        .draw(format!("\x1b]52;c;{}\x07", base64(bytes)?))
        .map_err(|e| e.to_string())
}

fn base64(bytes: &[u8]) -> Result<String, String> {
    const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let a = *chunk.first().ok_or("base64 input")?;
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        for (index, value) in [
            a >> 2,
            ((a & 3) << 4) | (b >> 4),
            ((b & 15) << 2) | (c >> 6),
            c & 63,
        ]
        .into_iter()
        .enumerate()
        {
            encoded.push(if index > chunk.len() {
                '='
            } else {
                char::from(*DIGITS.get(usize::from(value)).ok_or("base64 digit")?)
            });
        }
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn paste_keeps_split_utf8_multiline_and_marker_boundaries() {
        let bytes = "hello\n世界\tlast".as_bytes();
        let mut paste = Paste::default();
        let mut result = None;
        for byte in BEGIN.iter().chain(bytes).chain(END) {
            result = paste.push(*byte).unwrap();
        }
        assert_eq!(result.unwrap(), bytes);
        let mut empty = Paste::default();
        for byte in BEGIN.iter().chain(END.iter().take(5)) {
            assert!(empty.push(*byte).unwrap().is_none());
        }
        assert!(empty.push(b'~').unwrap_err().contains("nonempty"));
        let mut rejected = Paste::default();
        assert!(rejected.push(b'X').is_err());
        let mut oversized = Paste::default();
        for byte in BEGIN {
            oversized.push(*byte).unwrap();
        }
        for _ in 0..vm_wire::MAX_TEXT + END.len() {
            oversized.push(b'x').unwrap();
        }
        assert!(oversized.push(b'x').unwrap().is_none());
        for byte in b"qDXyes\r" {
            assert!(oversized.push(*byte).unwrap().is_none());
            assert!(oversized.bytes.len() <= END.len());
        }
        let mut end = END.iter().peekable();
        while let Some(byte) = end.next() {
            let result = oversized.push(*byte);
            if end.peek().is_none() {
                assert!(result.unwrap_err().contains("64 KiB"));
            } else {
                assert!(result.unwrap().is_none());
            }
        }
        let mut control = Paste::default();
        for byte in BEGIN
            .iter()
            .chain(b"bad\x03qDXyes\r")
            .chain(END.iter().take(5))
        {
            assert!(control.push(*byte).unwrap().is_none());
        }
        assert!(control.push(b'~').unwrap_err().contains("control"));
    }

    #[test]
    fn clipboard_encoding_cannot_inject_terminal_controls() {
        for (bytes, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(bytes.as_bytes()).unwrap(), expected);
        }
    }
}
