//! Host-owned PIN input for an explicit manual hardware check, never td consent.

use crate::{fido_pin::Pin, fido_transaction::PinPurpose, pin_sys};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

pub(super) struct Console(File);
impl Console {
    pub(super) fn open() -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0x800 | 0x100 | 0x20000) // NONBLOCK | NOCTTY | NOFOLLOW
            .open("/dev/tty")
            .map_err(|_| "a controlling host terminal is required")?;
        pin_sys::mode(file.as_fd()).map_err(|_| "read host terminal mode")?;
        Ok(Self(file))
    }

    pub(super) fn message(&self, text: &str) -> Result<(), String> {
        (&self.0)
            .write_all(text.as_bytes())
            .map_err(|_| "write host terminal".into())
    }

    pub(super) fn pin(&self, purpose: PinPurpose, deadline: Instant) -> Result<Pin, String> {
        let mut quiet = Quiet::start(&self.0)?;
        let result = (|| {
            let label = match purpose {
                PinPurpose::Creation => "Host token check: creation PIN (Ctrl+C cancels): ",
                PinPurpose::EnrollmentProof => "Host token check: proof PIN (Ctrl+C cancels): ",
                PinPurpose::Assertion => "Host token check: repeat PIN (Ctrl+C cancels): ",
            };
            self.message(label)?;
            let pin = read_pin(&self.0, deadline)?.into_pin()?;
            self.message("\nTouch the token when it requests presence.\n")?;
            Ok(pin)
        })();
        if let Err(error) = quiet.restore() {
            restoration_failed();
            return Err(error);
        }
        result
    }
}

struct Quiet<'a> {
    file: &'a File,
    saved: [u8; 36],
    restored: bool,
}
impl<'a> Quiet<'a> {
    fn start(file: &'a File) -> Result<Self, String> {
        let saved = pin_sys::mode(file.as_fd()).map_err(|_| "read host terminal mode")?;
        let guard = Self {
            file,
            saved,
            restored: false,
        };
        let mut mode = saved;
        for (at, clear, set) in [(0, 0x1deb_u32, 0), (8, 0x130, 0x30), (12, 0x8e7b, 0)] {
            let slot = mode.get_mut(at..at + 4).ok_or("terminal mode layout")?;
            let word = u32::from_ne_bytes(slot.try_into().map_err(|_| "terminal mode word")?);
            slot.copy_from_slice(&((word & !clear) | set).to_ne_bytes());
        }
        *mode.get_mut(22).ok_or("terminal VTIME")? = 0;
        *mode.get_mut(23).ok_or("terminal VMIN")? = 1;
        pin_sys::set_mode(file.as_fd(), &mode).map_err(|_| "set private PIN terminal mode")?;
        if pin_sys::mode(file.as_fd()).map_err(|_| "check PIN terminal mode")? != mode {
            return Err("host terminal refused private PIN mode".into());
        }
        Ok(guard)
    }
    fn restore(&mut self) -> Result<(), String> {
        pin_sys::set_mode(self.file.as_fd(), &self.saved)
            .map_err(|_| "restore host terminal mode")?;
        if pin_sys::mode(self.file.as_fd()).map_err(|_| "check restored terminal mode")?
            != self.saved
        {
            return Err("host terminal mode was not restored".into());
        }
        self.restored = true;
        Ok(())
    }
}
impl Drop for Quiet<'_> {
    fn drop(&mut self) {
        if !self.restored && pin_sys::set_mode(self.file.as_fd(), &self.saved).is_err() {
            restoration_failed();
        }
    }
}

fn restoration_failed() {
    let _ = writeln!(io::stderr().lock(), "td-secret: host terminal restoration failed; check terminal settings before entering more input");
}

struct Input(Vec<u8>);
impl Input {
    fn new() -> Self {
        Self(Vec::with_capacity(63))
    }
    fn byte(&mut self, byte: u8) -> Result<bool, String> {
        match byte {
            3 | 4 | 26 => Err("PIN input cancelled".into()),
            b'\r' | b'\n' => Ok(true),
            8 | 127 => {
                if let Some(last) = self.0.last_mut() {
                    *last = 0;
                }
                self.0.pop();
                Ok(false)
            }
            0x20..=0x7e if self.0.len() < 63 => {
                self.0.push(byte);
                Ok(false)
            }
            _ => Err("PIN requires 4 through 63 printable ASCII bytes".into()),
        }
    }
    fn into_pin(self) -> Result<Pin, String> {
        // Keep the input owner alive to clear it; shrinking its allocation
        // could otherwise free a copied PIN without clearing the old bytes.
        Pin::new(self.0.as_slice().into())
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

fn read_pin(mut file: &File, deadline: Instant) -> Result<Input, String> {
    let mut input = Input::new();
    loop {
        if Instant::now() >= deadline {
            return Err("PIN input expired".into());
        }
        if !pin_sys::readable(file.as_fd()).map_err(|_| "wait for PIN input")? {
            continue;
        }
        let mut byte = [0];
        let result = match file.read(&mut byte) {
            Ok(1) => {
                let [value] = byte;
                input.byte(value)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(false)
            }
            _ => Err("PIN terminal disconnected".into()),
        };
        byte.fill(0);
        std::hint::black_box(&mut byte);
        if result? {
            if Instant::now() >= deadline {
                return Err("PIN input expired".into());
            }
            return Ok(input);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    #[test]
    fn input_is_exact_bounded_and_cancellation_never_submits() {
        let mut input = Input::new();
        for byte in b" 123x\x7f4 " {
            assert!(!input.byte(*byte).unwrap());
        }
        assert_eq!(input.0, b" 1234 ");
        assert!(input.byte(b'\r').unwrap());
        assert!(input.into_pin().is_ok());
        for byte in [0, 3, 4, 26, 0x1b, 0x80, 0xff] {
            assert!(Input::new().byte(byte).is_err());
        }
        let mut input = Input::new();
        for _ in 0..63 {
            input.byte(b'x').unwrap();
        }
        assert!(input.byte(b'x').is_err());
        assert!(Input::new().into_pin().is_err());
    }

    fn pair() -> (File, File) {
        let master = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0x100 | 0x800)
            .open("/dev/ptmx")
            .unwrap();
        let slave = crate::terminal_fixture_sys::relay_pty_peer(master.as_fd()).unwrap();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).unwrap();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0x800 | 0x100 | 0x20000)
            .open(path)
            .unwrap();
        (master, slave)
    }

    #[test]
    fn a_full_nonblocking_output_queue_refuses_the_message() {
        let (_master, slave) = pair();
        let console = Console(slave);
        let message = "x".repeat(1024);
        let refused = (0..4096).find_map(|_| console.message(&message).err());
        assert_eq!(
            refused,
            Some("write host terminal".into()),
            "PTY output queue never refused the message"
        );
    }

    #[test]
    fn real_terminal_has_no_echo_and_restores_after_success_cancel_and_timeout() {
        for bytes in [
            Some(&b"1234\r"[..]),
            Some(&b"1234\rNEVER_ECHO\r"[..]),
            Some(&b"12\x03"[..]),
            Some(&b"12\x04"[..]),
            Some(&b"12\x1a"[..]),
            Some(&b"123\r"[..]),
            None,
        ] {
            let (mut master, slave) = pair();
            let original = pin_sys::mode(slave.as_fd()).unwrap();
            let console = Console(slave.try_clone().unwrap());
            let reader = std::thread::spawn(move || {
                console
                    .pin(
                        PinPurpose::Creation,
                        Instant::now() + Duration::from_secs(2),
                    )
                    .map(|_| ())
            });
            let mut output = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while !output.ends_with(b": ") {
                assert!(Instant::now() < deadline);
                let mut buffer = [0; 128];
                match master.read(&mut buffer) {
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    other => panic!("terminal fixture read: {other:?}"),
                }
            }
            let quiet = pin_sys::mode(slave.as_fd()).unwrap();
            let local = u32::from_ne_bytes(quiet[12..16].try_into().unwrap());
            for (name, bit) in [("ISIG", 1), ("ICANON", 2), ("ECHO", 8)] {
                assert_eq!(local & bit, 0, "{name} remained enabled");
            }
            if let Some(bytes) = bytes {
                master.write_all(bytes).unwrap();
            }
            let expected = match bytes {
                Some(text) if text.starts_with(b"1234\r") => Ok(()),
                Some(b"123\r") => {
                    Err("portable PIN must be 4 through 63 printable ASCII bytes".into())
                }
                Some(_) => Err("PIN input cancelled".into()),
                None => Err("PIN input expired".into()),
            };
            assert_eq!(reader.join().unwrap(), expected);
            assert_eq!(pin_sys::mode(slave.as_fd()).unwrap(), original);
            assert!(
                !pin_sys::readable(slave.as_fd()).unwrap(),
                "PIN tail survived restoration"
            );
            let mut buffer = [0; 256];
            if let Ok(n) = master.read(&mut buffer) {
                output.extend_from_slice(&buffer[..n]);
            }
            assert!(!output.windows(2).any(|text| text == b"12"));
            assert!(!output
                .windows(b"NEVER_ECHO".len())
                .any(|text| text == b"NEVER_ECHO"));
        }
    }
}
