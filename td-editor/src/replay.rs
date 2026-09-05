//! Bounded headless adapter for the editor dispatcher. Framing matches the
//! planned control transport; replay accepts consecutive frames until EOF.

use crate::keys::{Action, Keymap, Profile};
use crate::model::{Command, Editor, Selection, TabId};
use crate::{Error, Result};
use std::io::{self, Read, Write};

pub const MAX_FRAME: usize = 1024 * 1024;
pub const PAGE_BYTES: usize = 256 * 1024;

pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for index in [usize::from(byte >> 4), usize::from(byte & 15)] {
            if let Some(&digit) = DIGITS.get(index) {
                out.push(char::from(digit));
            }
        }
    }
    out
}

pub fn unhex(value: &str) -> Result<Vec<u8>> {
    if value == "-" {
        return Ok(Vec::new());
    }
    if value.is_empty() || value.len() > MAX_FRAME || !value.len().is_multiple_of(2) {
        return Err(Error::Protocol);
    }
    let digit = |b: u8| -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(Error::Protocol),
        }
    };
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = digit(*pair.first().ok_or(Error::Protocol)?)?;
            let low = digit(*pair.get(1).ok_or(Error::Protocol)?)?;
            Ok(high * 16 + low)
        })
        .collect()
}

fn number(value: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    value.parse().map_err(|_| Error::Protocol)
}
fn size(value: &str) -> Result<usize> {
    usize::try_from(number(value)?).map_err(|_| Error::Protocol)
}
fn boolean(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(Error::Protocol),
    }
}
fn string(value: &str) -> Result<String> {
    String::from_utf8(unhex(value)?).map_err(|_| Error::InvalidText)
}

#[derive(Default)]
pub struct Session {
    pub editor: Editor,
    pub keys: Keymap,
    mark: Option<TabId>,
}

impl Session {
    /// Payload errors produce a framed error and allow the next request.
    /// No valid request ID can be recovered from a bad envelope: use 0.
    pub fn request(&mut self, input: &[u8]) -> String {
        let mut request = 0u64;
        let result = (|| {
            if input.len() > MAX_FRAME {
                return Err(Error::Limit);
            }
            let input = std::str::from_utf8(input).map_err(|_| Error::Protocol)?;
            if !input.is_ascii() || input.bytes().any(|b| b < b' ' && b != b'\t' || b == 127) {
                return Err(Error::Protocol);
            }
            let mut fields = input.split('\t');
            if fields.next() != Some("1") {
                return Err(Error::Protocol);
            }
            request = number(fields.next().ok_or(Error::Protocol)?)?;
            let command = fields.next().ok_or(Error::Protocol)?;
            // No command has more than six arguments. Refuse before collecting
            // a hostile tab-delimited frame into hundreds of thousands of entries.
            let args: Vec<&str> = fields.take(7).collect();
            if args.len() > 6 {
                return Err(Error::Protocol);
            }
            self.command(command, &args)
        })();
        match result {
            Ok(body) => format!("1\t{request}\tok\t{body}"),
            Err(error) => format!(
                "1\t{request}\terror\t{}\t{}",
                error.code(),
                hex(error.code().as_bytes())
            ),
        }
    }

    fn command(&mut self, name: &str, args: &[&str]) -> Result<String> {
        match (name, args) {
            ("new", []) => {
                let id = self.editor.new_tab()?;
                self.keys.reset();
                self.mark = None;
                Ok(id.to_string())
            }
            ("load", [bytes]) => {
                let id = self.editor.load_bytes(&unhex(bytes)?)?;
                self.keys.reset();
                self.mark = None;
                Ok(id.to_string())
            }
            ("state", []) => {
                let mut out = format!(
                    "active={}\tkeys={}\tprefix={}",
                    self.editor.active().unwrap_or(0),
                    if self.keys.profile() == Profile::Windows {
                        "windows"
                    } else {
                        "emacs"
                    },
                    u8::from(self.keys.pending())
                );
                for (id, doc) in self.editor.tabs() {
                    let sel = doc.selection();
                    out.push_str(&format!(
                        "\ttab={id},{},{},{},{},{},{},{},{},{}",
                        doc.revision(),
                        u8::from(doc.dirty()),
                        doc.text().len(),
                        sel.anchor,
                        sel.caret,
                        u8::from(doc.auto_fill()),
                        doc.fill_column(),
                        u8::from(doc.format().bom),
                        if doc.format().ending == crate::text::LineEnding::Lf {
                            "lf"
                        } else {
                            "crlf"
                        }
                    ));
                }
                Ok(out)
            }
            ("text", [tab, revision, offset, limit]) => {
                let doc = self.editor.document(number(tab)?)?;
                if doc.revision() != number(revision)? {
                    return Err(Error::StaleRevision);
                }
                let offset = size(offset)?;
                let limit = size(limit)?;
                if !(4..=PAGE_BYTES).contains(&limit) {
                    return Err(Error::InvalidArgument);
                }
                doc.text().get(offset..).ok_or(Error::InvalidPosition)?;
                let mut end = offset.saturating_add(limit).min(doc.text().len());
                while !doc.text().is_char_boundary(end) {
                    end = end.saturating_sub(1);
                }
                Ok(format!(
                    "{end}\t{}",
                    hex(doc
                        .text()
                        .get(offset..end)
                        .ok_or(Error::InvalidPosition)?
                        .as_bytes())
                ))
            }
            ("select-tab", [tab]) => {
                self.editor.select_tab(number(tab)?)?;
                self.keys.reset();
                self.mark = None;
                Ok(String::new())
            }
            ("set-key-profile", [profile]) => {
                let profile = match *profile {
                    "windows" => Profile::Windows,
                    "emacs" => Profile::Emacs,
                    _ => return Err(Error::InvalidArgument),
                };
                self.keys.set_profile(profile);
                self.mark = None;
                Ok(String::new())
            }
            ("close-tab", [tab, rev]) => {
                self.editor.close_tab(number(tab)?, number(rev)?)?;
                self.keys.reset();
                self.mark = None;
                Ok(String::new())
            }
            ("key", [tab, rev, key]) => {
                let id = number(tab)?;
                let revision = number(rev)?;
                if self.editor.document(id)?.revision() != revision {
                    return Err(Error::StaleRevision);
                }
                if self.editor.active() != Some(id) {
                    return Err(Error::InvalidArgument);
                }
                let chord = string(key)?;
                match self.keys.translate(&chord)? {
                    Action::Edit(mut command) => {
                        if let Command::Move { extend, .. } = &mut command {
                            *extend |= self.mark == Some(id);
                        }
                        let moving = matches!(command, Command::Move { .. });
                        self.editor.dispatch(id, revision, command)?;
                        if !moving {
                            self.mark = None;
                        }
                    }
                    Action::New => {
                        self.editor.new_tab()?;
                        self.mark = None;
                    }
                    Action::NextTab(backward) => {
                        self.editor.next_tab(backward)?;
                        self.mark = None;
                    }
                    Action::SelectAll => {
                        let end = self.editor.document(id)?.text().len();
                        self.editor.dispatch(
                            id,
                            revision,
                            Command::Select(Selection {
                                anchor: 0,
                                caret: end,
                            }),
                        )?;
                    }
                    Action::Cancel if self.keys.profile() == Profile::Windows => {
                        self.mark = None;
                    }
                    Action::SetMark | Action::Cancel => {
                        self.mark = if matches!(chord.as_str(), "C-Space") {
                            Some(id)
                        } else {
                            None
                        };
                        let caret = self.editor.document(id)?.selection().caret;
                        self.editor.dispatch(
                            id,
                            revision,
                            Command::Select(Selection {
                                anchor: caret,
                                caret,
                            }),
                        )?;
                    }
                    Action::Prefix => return Ok("prefix".into()),
                    Action::Request(request) => return Ok(format!("request\t{request}")),
                }
                Ok(String::new())
            }
            (_, [tab, revision, rest @ ..]) => {
                let command = match (name, rest) {
                    ("select-range", [anchor, caret]) => Command::Select(Selection {
                        anchor: size(anchor)?,
                        caret: size(caret)?,
                    }),
                    ("insert", [value]) => Command::Insert(string(value)?),
                    ("delete", []) => Command::Delete,
                    ("backspace", []) => Command::Backspace,
                    ("undo", []) => Command::Undo,
                    ("redo", []) => Command::Redo,
                    ("fill-paragraph", []) => Command::FillParagraph,
                    ("set-auto-fill", [value]) => Command::AutoFill(boolean(value)?),
                    ("set-fill-column", [value]) => Command::FillColumn(size(value)?),
                    ("find", [needle, backward, wrap]) => Command::Find {
                        needle: string(needle)?,
                        backward: boolean(backward)?,
                        wrap: boolean(wrap)?,
                    },
                    ("replace", [needle, replacement]) => Command::ReplaceAll {
                        needle: string(needle)?,
                        replacement: string(replacement)?,
                    },
                    _ => return Err(Error::Protocol),
                };
                let id = number(tab)?;
                self.editor.dispatch(id, number(revision)?, command)?;
                self.mark = None;
                Ok(self.editor.document(id)?.revision().to_string())
            }
            _ => Err(Error::Protocol),
        }
    }
}

pub fn run(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let mut session = Session::default();
    loop {
        let mut header = [0u8; 4];
        // EOF between frames is normal. A partial header is an error.
        loop {
            match input.read(
                header
                    .get_mut(..1)
                    .ok_or_else(|| io::Error::other("header"))?,
            ) {
                Ok(0) => return Ok(()),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        input.read_exact(
            header
                .get_mut(1..)
                .ok_or_else(|| io::Error::other("header"))?,
        )?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(io::Error::other("replay frame length outside 1..=1048576"));
        }
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes)?;
        let reply = session.request(&bytes);
        let length =
            u32::try_from(reply.len()).map_err(|_| io::Error::other("response too large"))?;
        output.write_all(&length.to_be_bytes())?;
        output.write_all(reply.as_bytes())?;
        output.flush()?;
    }
}
