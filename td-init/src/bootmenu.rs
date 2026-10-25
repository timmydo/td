//! Physical boot choice on the keyboard console and serial console.
//! Std owns I/O; term owns the existing saved/read-back terminal settings.
use crate::{console, term};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(15);
const STEP: Duration = Duration::from_millis(20);
const O_NOCTTY: i32 = 0o400;
const O_NONBLOCK: i32 = 0o4000;
const CONSOLE: &str = "/dev/console";
const VT: &str = "/dev/tty1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Choice {
    Automatic,
    Selected(usize),
}

struct Menu {
    labels: Vec<String>,
    selected: usize,
    paused: bool,
    escape: u8,
}

impl Menu {
    fn parse(args: &[String]) -> Result<Self, String> {
        let Some((default, labels)) = args.split_first() else {
            return Err("boot-menu needs DEFAULT and one or two labels".into());
        };
        let selected = match default.as_str() {
            "0" => 0,
            "1" => 1,
            _ => return Err("invalid default boot entry".into()),
        };
        if labels.is_empty()
            || labels.len() > 2
            || selected >= labels.len()
            || labels.iter().any(|label| {
                label.is_empty()
                    || label.len() > 160
                    || !label.bytes().all(|b| (0x20..=0x7e).contains(&b))
            })
        {
            return Err("invalid boot menu labels".into());
        }
        Ok(Self {
            labels: labels.to_vec(),
            selected,
            paused: false,
            escape: 0,
        })
    }

    fn key(&mut self, byte: u8) -> Option<Choice> {
        self.paused = true;
        if self.escape == 1 {
            self.escape = 0;
            if matches!(byte, b'[' | b'O') {
                self.escape = 2;
                return None;
            }
        } else if self.escape == 2 {
            if (0x40..=0x7e).contains(&byte) {
                self.escape = 0;
                match byte {
                    b'A' => self.selected = self.selected.saturating_sub(1),
                    b'B' => {
                        self.selected = (self.selected + 1).min(self.labels.len().saturating_sub(1))
                    }
                    _ => {}
                }
                return None;
            }
            if !matches!(byte, b'\r' | b'\n' | 0x1b) {
                return None;
            }
            self.escape = 0;
        }
        match byte {
            0x1b => self.escape = 1,
            b'1' => self.selected = 0,
            b'2' if self.labels.len() > 1 => self.selected = 1,
            b'\r' | b'\n' => return Some(Choice::Selected(self.selected)),
            _ => {}
        }
        None
    }

    fn tick(&self, elapsed: Duration) -> Option<Choice> {
        (!self.paused && elapsed >= TIMEOUT).then_some(Choice::Automatic)
    }

    fn render(&self, elapsed: Duration) -> Vec<u8> {
        let mut text = String::from("\x1b[2J\x1b[Htd system startup\n\n");
        for (index, label) in self.labels.iter().enumerate() {
            let marker = if index == self.selected { ">" } else { " " };
            text.push_str(&format!("{marker} {}. {label}\n\n", index + 1));
        }
        if self.paused {
            text.push_str("Countdown stopped.\n");
        } else {
            let left = TIMEOUT.saturating_sub(elapsed).as_millis().div_ceil(1000);
            text.push_str(&format!(
                "Booting the default in {left} seconds. Any key stops the countdown.\n"
            ));
        }
        text.push_str("Use Up/Down or 1/2 to choose, then Enter to boot.\n");
        text.into_bytes()
    }
}

struct Line {
    file: File,
    saved: Option<term::Saved>,
    queue: Vec<u8>,
    sent: usize,
    next: Option<Vec<u8>>,
    active: bool,
}
impl Line {
    fn open(path: &str) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("open {path}: {e}"))?;
        if !file
            .metadata()
            .map_err(|e| e.to_string())?
            .file_type()
            .is_char_device()
        {
            return Err("boot menu requires a terminal".into());
        }
        let saved = term::save(&mut term::Kernel::new(file.as_raw_fd()))?;
        Ok(Self {
            file,
            saved: Some(saved),
            queue: vec![],
            sent: 0,
            next: None,
            active: true,
        })
    }
    fn prepare(&mut self) -> Result<(), String> {
        term::menu(
            &mut term::Kernel::new(self.file.as_raw_fd()),
            self.saved.as_ref().ok_or("missing terminal state")?,
        )
    }
    fn restore(&mut self) -> Result<(), String> {
        match self.saved.take() {
            Some(saved) => term::put_back(&mut term::Kernel::new(self.file.as_raw_fd()), &saved),
            None => Ok(()),
        }
    }
    fn queue(&mut self, bytes: &[u8]) {
        // A stopped output keeps only the latest complete view.
        if self.sent == 0 || self.sent >= self.queue.len() {
            self.queue.clear();
            self.queue.extend_from_slice(bytes);
            self.sent = 0;
            self.next = None;
        } else {
            self.next = Some(bytes.to_vec());
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.sent >= self.queue.len() {
            if let Some(next) = self.next.take() {
                self.queue = next;
                self.sent = 0;
            }
        }
        let Some(bytes) = self
            .queue
            .get(self.sent..)
            .filter(|bytes| !bytes.is_empty())
        else {
            return Ok(());
        };
        match self.file.write(bytes) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "boot menu output closed",
            )),
            Ok(n) => {
                self.sent += n;
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}
impl Drop for Line {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn session(menu: &mut Menu, lines: &mut [Line]) -> Result<Choice, String> {
    let start = Instant::now();
    let mut display = None;
    loop {
        let elapsed = start.elapsed();
        let state = (elapsed.as_secs(), menu.paused, menu.selected);
        if display != Some(state) {
            let bytes = menu.render(elapsed);
            for line in lines.iter_mut() {
                line.queue(&bytes);
            }
            display = Some(state);
        }
        let mut selected = None;
        for line in lines.iter_mut().filter(|line| line.active) {
            let mut bytes = [0; 64];
            if line.flush().is_err() {
                line.active = false;
                line.restore()?;
                continue;
            }
            match line.file.read(&mut bytes) {
                Ok(0) => {
                    line.active = false;
                    line.restore()?;
                }
                Ok(n) => {
                    for byte in bytes.get(..n).ok_or("invalid console read length")? {
                        if let Some(choice) = menu.key(*byte) {
                            selected = Some(choice);
                            break;
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    line.active = false;
                    line.restore()?;
                }
            }
            if selected.is_some() {
                break;
            }
        }
        if let Some(choice) = selected {
            return Ok(choice);
        }
        if !lines.iter().any(|line| line.active) {
            return if menu.paused {
                Err("boot menu input was lost after countdown stopped".into())
            } else {
                Ok(Choice::Automatic)
            };
        }
        if let Some(choice) = menu.tick(start.elapsed()) {
            return Ok(choice);
        }
        std::thread::sleep(STEP);
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut menu = Menu::parse(args)?;
    let names: &[&str] = if console::read(Path::new(console::ACTIVE)).vt_is_second() {
        &[CONSOLE, VT]
    } else {
        &[CONSOLE]
    };
    // Save every terminal before changing either; aliases cannot save raw state.
    let opened: Vec<Line> = names
        .iter()
        .filter_map(|name| match Line::open(name) {
            Ok(line) => Some(line),
            Err(error) => {
                eprintln!("boot-menu: {error}");
                None
            }
        })
        .collect();
    let mut lines = vec![];
    for mut line in opened {
        match line.prepare() {
            Ok(()) => lines.push(line),
            Err(error) => eprintln!("boot-menu: console unavailable: {error}"),
        }
    }
    let result = session(&mut menu, &mut lines);
    let mut restore = Ok(());
    for line in &mut lines {
        if let Err(error) = line.restore() {
            restore = Err(error);
        }
    }
    restore?;
    let reply = match result? {
        Choice::Automatic => "auto\n".into(),
        Choice::Selected(index) => format!("{index}\n"),
    };
    crate::emit(&reply)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    fn menu() -> Menu {
        Menu::parse(&[
            "0".into(),
            "Current commit 2026-10-25 aaaaaaa built 2026-10-26".into(),
            "Previous commit unknown built unknown".into(),
        ])
        .unwrap()
    }
    #[test]
    fn timeout_is_fifteen_seconds_and_any_key_cancels_it() {
        let mut value = menu();
        assert_eq!(value.tick(Duration::from_millis(14_999)), None);
        assert_eq!(value.tick(Duration::from_secs(15)), Some(Choice::Automatic));
        assert_eq!(value.key(b'x'), None);
        assert_eq!(value.tick(Duration::from_secs(3600)), None);
        assert_eq!(value.key(b'\n'), Some(Choice::Selected(0)));
    }
    #[test]
    fn choosing_never_boots_until_enter_and_arrow_fragments_work() {
        let mut value = menu();
        for key in b"\x1b[B" {
            assert_eq!(value.key(*key), None);
        }
        assert_eq!(value.selected, 1);
        assert_eq!(value.key(b'\r'), Some(Choice::Selected(1)));
        for key in b"\x1b[A" {
            assert_eq!(value.key(*key), None);
        }
        assert_eq!(value.selected, 0);
        assert_eq!(value.key(b'2'), None);
        assert_eq!(value.key(b'\n'), Some(Choice::Selected(1)));
    }
    #[test]
    fn escape_then_enter_works_and_function_key_digits_do_not_select() {
        let mut value = menu();
        assert_eq!(value.key(0x1b), None);
        assert_eq!(value.key(b'\n'), Some(Choice::Selected(0)));
        for byte in b"\x1b[12~" {
            assert_eq!(value.key(*byte), None);
        }
        assert_eq!(value.selected, 0);
    }
    #[test]
    fn labels_and_default_are_bounded_and_never_terminal_commands() {
        for args in [
            vec![],
            vec!["2".into(), "system".into()],
            vec!["1".into(), "system".into()],
            vec!["0".into(), "bad\x1b[2J".into()],
            vec!["0".into(), "x".repeat(161)],
        ] {
            assert!(Menu::parse(&args).is_err());
        }
        let value = menu();
        let text = String::from_utf8(value.render(Duration::ZERO)).unwrap();
        assert!(
            text.contains("15 seconds")
                && text.contains("Previous")
                && text.contains("built 2026-10-26")
        );
    }
    #[test]
    fn real_keyboard_byte_pauses_before_enter_and_both_lines_restore() {
        use std::sync::mpsc;
        use term::Termios;
        let ptys = [
            td_ui::pty::Pty::open().unwrap(),
            td_ui::pty::Pty::open().unwrap(),
        ];
        let peers: Vec<File> = ptys.iter().map(|pty| pty.peer().unwrap()).collect();
        let settings = |file: &File| {
            let mut bytes = [0u8; std::mem::size_of::<term::Bytes>()];
            term::Kernel::new(file.as_raw_fd()).get(&mut bytes).unwrap();
            bytes
        };
        let before: Vec<_> = peers.iter().map(settings).collect();
        let mut lines: Vec<_> = peers
            .iter()
            .map(|file| Line::open(&format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap())
            .collect();
        for line in &mut lines {
            line.prepare().unwrap();
        }
        let mut output = ptys[1].master().try_clone().unwrap();
        let mut keyboard = output.try_clone().unwrap();
        let (send, receive) = mpsc::channel();
        let output_thread = std::thread::spawn(move || {
            let mut bytes = [0u8; 1024];
            while let Ok(count) = output.read(&mut bytes) {
                if count == 0 || send.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        let worker = std::thread::spawn(move || {
            let result = session(&mut menu(), &mut lines);
            for line in &mut lines {
                line.restore().unwrap();
            }
            result
        });
        let mut shown = Vec::new();
        let mut wait_for = |needle: &[u8]| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !shown.windows(needle.len()).any(|part| part == needle) {
                shown.extend(
                    receive
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .unwrap(),
                );
            }
        };
        wait_for(b"15 seconds");
        keyboard.write_all(b"2").unwrap();
        wait_for(b"Countdown stopped.");
        assert!(!worker.is_finished(), "a selection must wait for Enter");
        keyboard.write_all(b"\n").unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), Choice::Selected(1));
        assert_eq!(peers.iter().map(settings).collect::<Vec<_>>(), before);
        drop(peers);
        drop(ptys);
        drop(keyboard);
        output_thread.join().unwrap();
    }
}
