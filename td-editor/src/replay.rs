//! Bounded headless adapter for the editor dispatcher. Framing matches the
//! planned control transport; replay accepts consecutive frames until EOF.

use crate::keys::Profile;
use crate::model::{Command, Selection};
use crate::ui::{Controller, Event, Outcome, PointerPhase};
use crate::{Error, Result};
use std::io::{self, Read, Write};

use crate::control::{boolean, decimal as number, size};
pub use crate::control::{hex, unhex, MAX_FRAME, PAGE_BYTES};

fn string(value: &str) -> Result<String> {
    String::from_utf8(unhex(value)?).map_err(|_| Error::InvalidText)
}

#[derive(Default)]
pub struct Session {
    pub ui: Controller,
}

impl Session {
    /// Payload errors produce a framed error and allow the next request.
    /// Errors before a recoverable request ID use 0.
    pub fn request(&mut self, input: &[u8]) -> String {
        let envelope = match crate::control::envelope(input) {
            Ok(envelope) => envelope,
            Err(refusal) => return refusal.response(),
        };
        let request = envelope.id;
        // No command has more than six arguments. Never collect an unbounded
        // number of tab-separated fields from an untrusted frame.
        let args: Vec<&str> = envelope.args.take(7).collect();
        let result = if args.len() > 6 {
            Err(Error::Protocol)
        } else {
            self.command(envelope.name, &args)
        };
        match result {
            Ok(body) => format!("1\t{request}\tok\t{body}"),
            Err(error) => crate::control::Refusal { id: request, error }.response(),
        }
    }

    fn command(&mut self, name: &str, args: &[&str]) -> Result<String> {
        match (name, args) {
            ("new", []) => reply(self.ui.dispatch(Event::New)?),
            ("load", [bytes]) => reply(self.ui.dispatch(Event::Load(&unhex(bytes)?))?),
            ("state", []) => crate::control::state(&self.ui),
            ("text", [tab, revision, offset, limit]) => crate::control::page(
                &self.ui,
                number(tab)?,
                number(revision)?,
                size(offset)?,
                size(limit)?,
            ),
            ("select-tab", [tab]) => reply(self.ui.dispatch(Event::SelectTab(number(tab)?))?),
            ("set-key-profile", [profile]) => {
                let profile = match *profile {
                    "windows" => Profile::Windows,
                    "emacs" => Profile::Emacs,
                    _ => return Err(Error::InvalidArgument),
                };
                reply(self.ui.dispatch(Event::Profile(profile))?)
            }
            ("close-tab", [tab, rev]) => reply(self.ui.dispatch(Event::Close {
                tab: number(tab)?,
                revision: number(rev)?,
            })?),
            ("key", [tab, rev, key]) => {
                let chord = string(key)?;
                reply(self.ui.dispatch(Event::Key {
                    tab: number(tab)?,
                    revision: number(rev)?,
                    chord: &chord,
                })?)
            }
            ("resize", [width, height, scale]) => reply(self.ui.dispatch(Event::Resize {
                width: size(width)?,
                height: size(height)?,
                scale: u8::try_from(number(scale)?).map_err(|_| Error::InvalidArgument)?,
            })?),
            ("focus", [value]) => reply(self.ui.dispatch(Event::Focus(boolean(value)?))?),
            ("tick", [now]) => reply(self.ui.dispatch(Event::Tick(number(now)?))?),
            ("set-soft-wrap", [tab, rev, value]) => reply(self.ui.dispatch(Event::Wrap {
                tab: number(tab)?,
                revision: number(rev)?,
                enabled: boolean(value)?,
            })?),
            ("scroll", [tab, rev, axis, direction, amount]) => {
                let amount =
                    isize::try_from(number(amount)?).map_err(|_| Error::InvalidArgument)?;
                let delta = match *direction {
                    "forward" => amount,
                    "backward" => -amount,
                    _ => return Err(Error::InvalidArgument),
                };
                let (rows, columns) = match *axis {
                    "rows" => (delta, 0),
                    "columns" => (0, delta),
                    _ => return Err(Error::InvalidArgument),
                };
                reply(self.ui.dispatch(Event::Scroll {
                    tab: number(tab)?,
                    revision: number(rev)?,
                    rows,
                    columns,
                })?)
            }
            ("pointer", [tab, rev, phase, x, y, extend]) => {
                let phase = match *phase {
                    "press" => PointerPhase::Press,
                    "move" => PointerPhase::Move,
                    "release" => PointerPhase::Release,
                    _ => return Err(Error::InvalidArgument),
                };
                // Replay coordinates are unsigned surface pixels. A native
                // adapter may supply signed out-of-surface motion directly.
                let x = i64::try_from(number(x)?).map_err(|_| Error::InvalidArgument)?;
                let y = i64::try_from(number(y)?).map_err(|_| Error::InvalidArgument)?;
                reply(self.ui.dispatch(Event::Pointer {
                    tab: number(tab)?,
                    revision: number(rev)?,
                    phase,
                    x,
                    y,
                    extend: boolean(extend)?,
                })?)
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
                    ("go-to-line", [value]) => Command::GoToLine(size(value)?),
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
                self.ui.dispatch(Event::Edit {
                    tab: id,
                    revision: number(revision)?,
                    command,
                })?;
                Ok(self.ui.editor().document(id)?.revision().to_string())
            }
            _ => Err(Error::Protocol),
        }
    }
}

fn reply(outcome: Outcome) -> Result<String> {
    Ok(match outcome {
        Outcome::Created(id) => id.to_string(),
        Outcome::Prefix => "prefix".into(),
        Outcome::Request {
            name,
            tab,
            revision,
        } => format!("request\t{name}\t{tab}\t{revision}"),
        Outcome::Changed | Outcome::Ignored => String::new(),
    })
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
        let framed = crate::control::frame(reply.as_bytes()).map_err(io::Error::other)?;
        output.write_all(&framed)?;
        output.flush()?;
    }
}
