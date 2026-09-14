//! Optional local UI driving uses the window's same input dispatcher.
use crate::ui::{Outcome, Phase, State};
use td_ui::control::{Error, ErrorCode};
use td_ui::driven::{self, Binding, Controller, Input, PointerPhase};
use td_ui::raster::Composition;
#[derive(Clone, Copy, Debug)]
pub enum Refusal {
    Protocol,
    Limit,
    Unavailable,
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
            Self::Unavailable => "unavailable",
        }
    }
}
const BINDINGS: [Binding; 4] = [
    Binding {
        name: "live",
        chord: Some("C-l"),
        arguments: "",
        help: "Return to live observations.",
    },
    Binding {
        name: "interval",
        chord: Some("C-i"),
        arguments: "",
        help: "Cycle the sampling interval.",
    },
    Binding {
        name: "search",
        chord: Some("C-f"),
        arguments: "",
        help: "Focus process search.",
    },
    Binding {
        name: "quit",
        chord: Some("C-q"),
        arguments: "",
        help: "Close the task manager.",
    },
];
pub struct Remote<'a> {
    pub state: &'a mut State,
    pub pointer: &'a mut (i64, i64),
    pub effect: Option<Outcome>,
    pub presentations: u64,
}
impl Remote<'_> {
    fn outcome(&mut self, outcome: Outcome) -> driven::Outcome {
        self.effect = Some(outcome);
        match outcome {
            Outcome::Ignored => driven::Outcome::Ignored,
            Outcome::Quit => driven::Outcome::Quit,
            _ => driven::Outcome::Changed,
        }
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
        let outcome = self.state.key(chord, false);
        Ok(self.outcome(outcome))
    }
    fn input(&mut self, input: Input<'_>) -> Result<driven::Outcome, Refusal> {
        let outcome = match input {
            Input::Key { chord } => self.state.key(chord, false),
            Input::Pointer { phase, x, y } => {
                let (x, y) = (i64::from(x), i64::from(y));
                *self.pointer = (x, y);
                self.state.pointer(
                    match phase {
                        PointerPhase::Press => Phase::Press,
                        PointerPhase::Move => Phase::Move,
                        PointerPhase::Release => Phase::Release,
                    },
                    x,
                    y,
                )
            }
            Input::Wheel { rows, columns } => self.state.scroll(
                self.pointer.0,
                self.pointer.1,
                i64::from(rows),
                i64::from(columns),
            ),
            Input::Focus(false) => {
                self.state.cancel();
                Outcome::Changed
            }
            Input::Focus(true) => {
                self.state.restore_focus();
                Outcome::Changed
            }
            Input::Resize { .. } | Input::Tick(_) => return Err(Refusal::Unavailable),
        };
        Ok(self.outcome(outcome))
    }
    fn state(&self) -> Result<String, Refusal> {
        Ok(format!(
            "{}\tpresentations={}\tdirty={}",
            self.state.report(),
            self.presentations,
            self.state.dirty()
        ))
    }
    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Refusal> {
        Ok(view(self.state))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::budget::{Budget, LIMIT};
    use td_ui::raster::Surface;
    #[test]
    fn optional_driver_uses_shared_actions_and_refuses_unavailable_inputs() {
        driven::check(&BINDINGS).unwrap();
        let budget = Budget::new(LIMIT).unwrap();
        let mut state =
            State::new(&budget, Surface::new(800, 600, Default::default()).unwrap()).unwrap();
        let mut pointer = (0, 0);
        let mut remote = Remote {
            state: &mut state,
            pointer: &mut pointer,
            effect: None,
            presentations: 4,
        };
        assert_eq!(
            driven::request(&mut remote, b"1\t1\taction\tsearch"),
            "1\t1\tok\tchanged"
        );
        assert_eq!(remote.state.focus(), crate::ui::Focus::Search);
        assert_eq!(
            remote
                .input(Input::Pointer {
                    phase: PointerPhase::Move,
                    x: 100,
                    y: 100
                })
                .unwrap(),
            driven::Outcome::Ignored
        );
        assert_eq!(
            driven::request(&mut remote, b"1\t2\tkey\t78"),
            "1\t2\tok\tchanged"
        );
        assert_eq!(remote.state.query(), "x");
        assert!(remote
            .input(Input::Resize {
                width: 10,
                height: 10,
                scale: 1,
            })
            .is_err());
        assert!(remote.action("live", &["unexpected"]).is_err());
        assert_eq!(
            driven::request(&mut remote, b"1\t3\taction\tquit"),
            "1\t3\tok\tquit"
        );
        assert_eq!(remote.effect, Some(Outcome::Quit));
        assert!(remote.state().unwrap().contains("presentations=4"));
    }
}
