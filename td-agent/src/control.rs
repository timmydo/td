//! The window through td-ui's driven seam (td-ui/DESIGN.md, "The semantic
//! seam"; DESIGN.md §4): the same key and pointer paths the window's own
//! inputs take, its state as tab-separated facts, and what it shows as
//! a composition, so an agent or a test can operate it. The window
//! serves it on a control socket when started with one.

use td_ui::control::{Error, ErrorCode};
use td_ui::driven::{self, Binding, Controller, Input, PointerPhase};
use td_ui::raster::{Composition, Scale, Surface};
use td_ui::window::{self, NoClipboard};

use crate::ui::App;
#[cfg(test)]
use crate::ui::Request;

#[derive(Clone, Copy, Debug)]
pub enum Refusal {
    Protocol,
    Limit,
}

impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        match error {
            Error::Protocol => Self::Protocol,
            Error::Limit => Self::Limit,
        }
    }
}

impl ErrorCode for Refusal {
    fn code(&self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Limit => "limit",
        }
    }
}

/// The window's actions, each its default chord.
pub const BINDINGS: [Binding; 6] = [
    Binding {
        name: "new",
        chord: Some("C-n"),
        arguments: "",
        help: "Start a new conversation and open it.",
    },
    Binding {
        name: "previous",
        chord: Some("C-PageUp"),
        arguments: "",
        help: "Open the conversation above the open one.",
    },
    Binding {
        name: "next",
        chord: Some("C-PageDown"),
        arguments: "",
        help: "Open the conversation below the open one.",
    },
    Binding {
        name: "send",
        chord: Some("C-Return"),
        arguments: "",
        help: "Send the composer's text; Return is a newline.",
    },
    Binding {
        name: "focus-next",
        chord: Some("F6"),
        arguments: "",
        help: "Move the focus: list, transcript, composer.",
    },
    Binding {
        name: "focus-previous",
        chord: Some("S-F6"),
        arguments: "",
        help: "Move the focus back.",
    },
];

/// The window as the seam drives it. A copy has no press to be offered
/// at, so the seam's inputs reach the clipboard as no clipboard.
pub struct Remote<'a> {
    pub app: &'a mut App,
}

impl Remote<'_> {
    /// Whether the input changed anything: the window asked for something
    /// or must paint again.
    fn outcome(&mut self, before: u64) -> driven::Outcome {
        if self.app.generation() != before || self.app.has_requests() {
            driven::Outcome::Changed
        } else {
            driven::Outcome::Ignored
        }
    }

    fn deliver(&mut self, input: window::Input<'_>) -> driven::Outcome {
        let before = self.app.generation();
        self.app.input(input, &mut NoClipboard);
        self.outcome(before)
    }
}

impl Controller for Remote<'_> {
    type Error = Refusal;

    fn bindings(&self) -> &'static [Binding] {
        &BINDINGS
    }

    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<driven::Outcome, Refusal> {
        if !arguments.is_empty() {
            return Err(Refusal::Protocol);
        }
        let chord = BINDINGS
            .iter()
            .find(|b| b.name == name)
            .and_then(|b| b.chord)
            .ok_or(Refusal::Protocol)?;
        Ok(self.deliver(window::Input::Key {
            chord,
            repeat: false,
        }))
    }

    fn input(&mut self, input: Input<'_>) -> Result<driven::Outcome, Refusal> {
        Ok(match input {
            Input::Key { chord } => self.deliver(window::Input::Key {
                chord,
                repeat: false,
            }),
            Input::Pointer { phase, x, y } => self.deliver(window::Input::Pointer {
                phase: match phase {
                    PointerPhase::Press => window::PointerPhase::Press,
                    PointerPhase::Move => window::PointerPhase::Move,
                    PointerPhase::Release => window::PointerPhase::Release,
                },
                x: i64::from(x),
                y: i64::from(y),
                extend: false,
                follow: false,
            }),
            Input::Wheel { rows, columns } => self.deliver(window::Input::Wheel {
                rows: rows as isize,
                columns: columns as isize,
            }),
            Input::Resize {
                width,
                height,
                scale,
            } => {
                let scale = Scale::new(scale).map_err(|_| Refusal::Protocol)?;
                let surface = Surface::new(width, height, scale).map_err(|_| Refusal::Limit)?;
                self.deliver(window::Input::Resize(surface))
            }
            Input::Focus(focused) => self.deliver(window::Input::Focus(focused)),
            Input::Tick(now) => {
                let before = self.app.generation();
                self.app.tick(now);
                self.outcome(before)
            }
            // No hints to show: the roles held are nothing to it.
            Input::Held(_) => driven::Outcome::Ignored,
        })
    }

    fn state(&self) -> Result<String, Refusal> {
        let app = &*self.app;
        let active = app.active();
        let state = active
            .and_then(|id| app.rows().iter().find(|r| &r.id == id))
            .map_or("none", |r| r.state.word());
        Ok(format!(
            "conversations={}\tactive={}\tstate={state}\tfocus={}\tmessages={}\tcomposer={}\tstatus={}",
            app.rows().len(),
            active.map_or("none", |id| id.as_str()),
            app.focus().word(),
            app.transcript().len(),
            app.composed().len(),
            app.status_line().replace(['\t', '\n'], " "),
        ))
    }

    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Refusal> {
        Ok(view(&*self.app))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_action_table_is_well_formed_and_drives_the_window() {
        driven::check(&BINDINGS).unwrap();
        let mut app = crate::ui::tests::app();
        let mut remote = Remote { app: &mut app };
        assert_eq!(
            driven::request(&mut remote, b"1\t1\taction\tfocus-next"),
            "1\t1\tok\tchanged"
        );
        assert!(remote.state().unwrap().contains("focus=list"));
        assert_eq!(
            driven::request(&mut remote, b"1\t2\taction\tnew"),
            "1\t2\tok\tchanged"
        );
        assert_eq!(remote.app.take_requests(), [Request::New]);
        // Typed through the seam's key verb, then sent.
        driven::request(&mut remote, b"1\t3\taction\tfocus-next");
        driven::request(&mut remote, b"1\t4\taction\tfocus-next");
        for (n, hex) in ["6f", "6b"].iter().enumerate() {
            let line = format!("1\t{}\tkey\t{hex}", 10 + n);
            assert!(driven::request(&mut remote, line.as_bytes()).ends_with("changed"));
        }
        assert!(remote.state().unwrap().contains("composer=2"));
        driven::request(&mut remote, b"1\t20\taction\tsend");
        assert_eq!(remote.app.take_requests(), [Request::Send("ok".into())]);
        assert!(driven::request(&mut remote, b"1\t21\taction\tnope").contains("protocol"));
        assert!(driven::request(&mut remote, b"1\t22\taction\tnew\tx").contains("protocol"));
        let text = driven::request(&mut remote, b"1\t23\ttext");
        assert!(text.contains("\tok\t"), "{text}");
    }
}
