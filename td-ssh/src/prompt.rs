//! Questions for the human: `SSH_ASKPASS`, or `/dev/tty` with echo controlled.
//! td-agent, td-review and td-pinentry depend on the routing and on the exact
//! text of the host-key question (DESIGN.md section 6).

use crate::error::{Error, Result};
use crate::sys::{self, Termios};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::process::{Command, Stdio};

const MAX_ANSWER: usize = 4096;

// The kernel's termbits, the values sys.rs's raw mode uses.
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHONL: u32 = 0o100;
const VTIME: usize = 5;
const VMIN: usize = 6;

const KEY_INTERRUPT: u8 = 0x03;
const KEY_END_OF_FILE: u8 = 0x04;
const KEY_BACKSPACE: u8 = 0x08;
const KEY_KILL_LINE: u8 = 0x15;
const KEY_WORD_ERASE: u8 = 0x17;
const KEY_DELETE: u8 = 0x7f;

pub struct Prompter {
    batch: bool,
}

/// `SSH_ASKPASS_REQUIRE`'s three settings and the absence of a terminal.
fn askpass_program(have_terminal: bool) -> Option<String> {
    let program = std::env::var("SSH_ASKPASS")
        .ok()
        .filter(|program| !program.is_empty())?;
    match std::env::var("SSH_ASKPASS_REQUIRE").as_deref() {
        Ok("force") | Ok("prefer") => Some(program),
        Ok("never") => None,
        _ if !have_terminal => Some(program),
        _ => None,
    }
}

fn open_terminal() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

impl Prompter {
    pub fn new(batch: bool) -> Self {
        Self { batch }
    }

    /// A secret: never echoed. The caller wipes the answer.
    pub fn secret(&self, prompt: &str) -> Result<Vec<u8>> {
        self.ask(prompt, false)
    }

    /// A question whose answer may be shown, such as yes or no.
    pub fn question(&self, prompt: &str) -> Result<String> {
        let answer = self.ask(prompt, true)?;
        Ok(String::from_utf8_lossy(&answer).trim().to_string())
    }

    fn ask(&self, prompt: &str, visible: bool) -> Result<Vec<u8>> {
        if self.batch {
            return Err(Error::refused(
                "read_passphrase: BatchMode refuses to prompt",
            ));
        }
        let terminal = open_terminal();
        if let Some(program) = askpass_program(terminal.is_ok()) {
            return run_askpass(&program, prompt);
        }
        let terminal = terminal.map_err(|error| {
            Error::refused(format!("read_passphrase: can't open /dev/tty: {error}"))
        })?;
        if visible {
            ask_visible(&terminal, prompt)
        } else {
            ask_hidden(&terminal, prompt)
        }
    }
}

fn run_askpass(program: &str, prompt: &str) -> Result<Vec<u8>> {
    let output = Command::new(program)
        .arg(prompt)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| Error::refused(format!("read_passphrase: {program}: {error}")))?;
    if !output.status.success() {
        return Err(Error::refused(format!(
            "read_passphrase: {program} failed: {}",
            output.status
        )));
    }
    let mut line = output.stdout;
    if line.len() > MAX_ANSWER {
        td_primitives::ct::zero(&mut line);
        return Err(Error::refused("read_passphrase: answer is too long"));
    }
    if let Some(end) = line.iter().position(|byte| *byte == b'\n') {
        td_primitives::ct::zero(line.get_mut(end..).unwrap_or_default());
        line.truncate(end);
    }
    Ok(line)
}

fn read_error(error: impl std::fmt::Display) -> Error {
    Error::refused(format!("read_passphrase: {error}"))
}

/// End of input with nothing typed, as at a hung-up terminal: no answer, never
/// an empty one, which `ssh-keygen` would take for "no passphrase".
fn no_answer() -> Error {
    Error::refused("read_passphrase: end of input at the prompt")
}

fn ask_visible(terminal: &File, prompt: &str) -> Result<Vec<u8>> {
    let mut writer = terminal;
    writer
        .write_all(prompt.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(read_error)?;
    let mut reader = BufReader::new(terminal);
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).map_err(read_error)? == 0 {
        return Err(no_answer());
    }
    if line.len() > MAX_ANSWER {
        return Err(Error::refused("read_passphrase: answer is too long"));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}

/// The prompt's own line discipline: no echo, no kernel line editing and no
/// signals, so Ctrl-C reaches `edit_line` and nothing can leave echo off.
fn hidden_mode(saved: Termios) -> Termios {
    let mut mode = saved;
    mode.lflag &= !(ECHO | ECHONL | ICANON | ISIG);
    if let Some(slot) = mode.cc.get_mut(VMIN) {
        *slot = 1;
    }
    if let Some(slot) = mode.cc.get_mut(VTIME) {
        *slot = 0;
    }
    mode
}

fn ask_hidden(terminal: &File, prompt: &str) -> Result<Vec<u8>> {
    let saved = sys::terminal_attributes(terminal).map_err(read_error)?;
    sys::set_terminal_attributes(terminal, &hidden_mode(saved)).map_err(read_error)?;
    let result = (|| {
        let mut writer = terminal;
        writer
            .write_all(prompt.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(read_error)?;
        let mut reader = terminal;
        edit_line(&mut reader)
    })();
    let restored = sys::set_terminal_attributes(terminal, &saved);
    // Nothing echoed the line ending.
    let mut writer = terminal;
    let _ = writer.write_all(b"\n");
    match (result, restored) {
        (Err(error), _) => Err(error),
        (Ok(mut line), Err(error)) => {
            td_primitives::ct::zero(&mut line);
            Err(read_error(error))
        }
        (Ok(line), Ok(())) => Ok(line),
    }
}

/// A line being typed unseen, wiped unless it is handed over.
struct LineEditor {
    line: Vec<u8>,
}

impl LineEditor {
    fn new() -> Self {
        Self {
            // Sized once, so growing leaves no copy behind.
            line: Vec::with_capacity(MAX_ANSWER),
        }
    }

    fn erase_last(&mut self) {
        // A whole UTF-8 character: its continuation bytes, then its first byte.
        while let Some(last) = self.line.last_mut() {
            let continuation = *last & 0xc0 == 0x80;
            *last = 0;
            self.line.pop();
            if !continuation {
                break;
            }
        }
    }

    /// One key. True when the line has ended.
    fn key(&mut self, byte: u8) -> Result<bool> {
        match byte {
            b'\n' | b'\r' => return Ok(true),
            KEY_INTERRUPT => return Err(Error::Aborted),
            KEY_END_OF_FILE if self.line.is_empty() => return Err(no_answer()),
            KEY_END_OF_FILE => return Ok(true),
            KEY_DELETE | KEY_BACKSPACE => self.erase_last(),
            KEY_WORD_ERASE => {
                let is_space = |byte: &u8| byte.is_ascii_whitespace();
                while self.line.last().is_some_and(is_space) {
                    self.erase_last();
                }
                while self.line.last().is_some_and(|byte| !is_space(byte)) {
                    self.erase_last();
                }
            }
            KEY_KILL_LINE => {
                td_primitives::ct::zero(&mut self.line);
                self.line.clear();
            }
            _ if self.line.len() >= MAX_ANSWER => {
                return Err(Error::refused("read_passphrase: answer is too long"))
            }
            _ => self.line.push(byte),
        }
        Ok(false)
    }
}

impl Drop for LineEditor {
    fn drop(&mut self) {
        td_primitives::ct::zero(&mut self.line);
    }
}

/// Reads byte by byte so nothing beyond the line sits in a buffer.
fn edit_line<R: Read>(input: &mut R) -> Result<Vec<u8>> {
    let mut editor = LineEditor::new();
    loop {
        let mut byte = [0u8];
        match input.read(&mut byte) {
            Ok(0) => return Err(no_answer()),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_error(error)),
        }
        let [value] = byte;
        if editor.key(value)? {
            return Ok(std::mem::take(&mut editor.line));
        }
    }
}

/// The host-key question, whose shape td-pinentry recognizes to show the answer.
pub fn host_key_question(host: &str, address: &str, fingerprint: &str) -> String {
    format!(
        "The authenticity of host '{host} ({address})' can't be established.\n\
         ED25519 key fingerprint is {fingerprint}.\n\
         Are you sure you want to continue connecting (yes/no/[fingerprint])? "
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_question_has_the_shape_td_pinentry_recognizes() {
        let question = host_key_question("h", "10.0.0.1", "SHA256:abc");
        assert!(question.starts_with("The authenticity of host "));
        assert!(question.ends_with("(yes/no/[fingerprint])? "));
    }

    #[test]
    fn batch_mode_refuses_with_the_word_td_agent_matches() {
        let error = Prompter::new(true).secret("x").err();
        let text = error.map(|error| error.to_string()).unwrap_or_default();
        assert!(text.contains("read_passphrase"), "{text}");
    }

    fn typed(keys: &[u8]) -> Result<Vec<u8>> {
        edit_line(&mut &*keys)
    }

    #[test]
    fn the_hidden_prompt_edits_its_own_line() {
        assert_eq!(typed(b"abc\n").unwrap(), b"abc");
        assert_eq!(typed(b"abc\r").unwrap(), b"abc");
        assert_eq!(typed(b"abc\x7f\x08d\n").unwrap(), b"ad");
        assert_eq!(typed(b"abc\x15d\n").unwrap(), b"d");
        assert_eq!(typed(b"\x7f\x08a\n").unwrap(), b"a");
        // Erase takes a whole UTF-8 character.
        assert_eq!(
            typed("a\u{00e9}\u{20ac}\x7f\n".as_bytes()).unwrap(),
            "a\u{00e9}".as_bytes()
        );
        // Ctrl-D ends a line with text on it.
        assert_eq!(typed(b"abc\x04").unwrap(), b"abc");
        assert_eq!(typed(b"\n").unwrap(), b"");
    }

    #[test]
    fn word_erase_takes_the_last_word_and_the_space_after_it() {
        assert_eq!(typed(b"one two\x17\n").unwrap(), b"one ");
        assert_eq!(typed(b"one two  \x17\n").unwrap(), b"one ");
        assert_eq!(typed(b"one\x17\n").unwrap(), b"");
        assert_eq!(typed(b"\x17x\n").unwrap(), b"x");
        // Whole characters, as a backspace takes them.
        assert_eq!(typed("a \u{e9}\u{20ac}\x17\n".as_bytes()).unwrap(), b"a ");
    }

    #[test]
    fn interrupt_aborts_and_end_of_input_is_no_answer() {
        assert!(matches!(typed(b"abc\x03"), Err(Error::Aborted)));
        for keys in [&b""[..], b"\x04", b"abc"] {
            let error = typed(keys).err().unwrap().to_string();
            assert!(error.contains("read_passphrase"), "{error}");
        }
        assert!(typed(&vec![b'x'; MAX_ANSWER + 1]).is_err());
    }

    #[test]
    fn hidden_mode_takes_keys_one_by_one_and_leaves_output_alone() {
        let saved = Termios {
            oflag: 0o5,
            iflag: 0o2400,
            lflag: ECHO | ECHONL | ICANON | ISIG | 0o100000,
            ..Termios::default()
        };
        let mode = hidden_mode(saved);
        assert_eq!(mode.lflag, 0o100000);
        assert_eq!(mode.oflag, saved.oflag);
        assert_eq!(mode.iflag, saved.iflag);
        assert_eq!(mode.cc.get(VMIN), Some(&1));
        assert_eq!(mode.cc.get(VTIME), Some(&0));
    }
}
