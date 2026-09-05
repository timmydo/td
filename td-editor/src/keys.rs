//! Logical keys after layout translation. No evdev positions or XKB masks
//! enter this layer. UI requests are explicit and are not successful edits.

use crate::model::{Command, Motion};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    #[default]
    Windows,
    Emacs,
}

#[derive(Debug)]
pub enum Action {
    Edit(Command),
    New,
    NextTab(bool),
    SelectAll,
    SetMark,
    Cancel,
    Prefix,
    Request(&'static str),
}

#[derive(Clone, Default, Debug)]
pub struct Keymap {
    profile: Profile,
    prefix: bool,
}

impl Keymap {
    pub fn profile(&self) -> Profile {
        self.profile
    }
    pub fn pending(&self) -> bool {
        self.prefix
    }
    pub fn reset(&mut self) {
        self.prefix = false;
    }
    pub fn set_profile(&mut self, profile: Profile) {
        self.profile = profile;
        self.reset();
    }

    pub fn translate(&mut self, chord: &str) -> Result<Action> {
        if chord.is_empty() || chord.len() > 32 {
            return Err(Error::InvalidArgument);
        }
        if matches!(chord, "Escape" | "C-g") {
            self.reset();
            return Ok(Action::Cancel);
        }
        if self.prefix {
            self.reset();
            return match chord {
                "C-f" => Ok(Action::Request("open")),
                "C-s" => Ok(Action::Request("save")),
                "C-w" => Ok(Action::Request("save-as")),
                "k" => Ok(Action::Request("close-tab")),
                "C-c" => Ok(Action::Request("quit")),
                _ => Err(Error::InvalidArgument),
            };
        }
        let movement = match chord {
            "Left" | "S-Left" => Some(Motion::Left),
            "Right" | "S-Right" => Some(Motion::Right),
            "Home" | "S-Home" => Some(Motion::Home),
            "End" | "S-End" => Some(Motion::End),
            "C-Home" | "C-S-Home" => Some(Motion::DocumentStart),
            "C-End" | "C-S-End" => Some(Motion::DocumentEnd),
            "C-S-Left" if self.profile == Profile::Windows => Some(Motion::WordLeft),
            "C-S-Right" if self.profile == Profile::Windows => Some(Motion::WordRight),
            _ => None,
        };
        if let Some(motion) = movement {
            return Ok(Action::Edit(Command::Move {
                motion,
                extend: chord.contains("S-"),
            }));
        }
        let common = match chord {
            "Backspace" => Some(Action::Edit(Command::Backspace)),
            "Delete" => Some(Action::Edit(Command::Delete)),
            "Return" => Some(Action::Edit(Command::Type('\n'))),
            "Tab" => Some(Action::Edit(Command::Type('\t'))),
            "Space" => Some(Action::Edit(Command::Type(' '))),
            "C-Tab" => Some(Action::NextTab(false)),
            "C-S-Tab" => Some(Action::NextTab(true)),
            "F7" => Some(Action::Request("check-spelling")),
            "Up" => Some(Action::Request("up")),
            "Down" => Some(Action::Request("down")),
            "S-Up" => Some(Action::Request("select-up")),
            "S-Down" => Some(Action::Request("select-down")),
            "PageUp" => Some(Action::Request("page-up")),
            "PageDown" => Some(Action::Request("page-down")),
            "S-PageUp" => Some(Action::Request("select-page-up")),
            "S-PageDown" => Some(Action::Request("select-page-down")),
            _ => None,
        };
        if let Some(action) = common {
            return Ok(action);
        }
        let binding = match (self.profile, chord) {
            (Profile::Windows, "C-n") => Some(Action::New),
            (Profile::Windows, "C-o") => Some(Action::Request("open")),
            (Profile::Windows, "C-s") => Some(Action::Request("save")),
            (Profile::Windows, "C-S-s") => Some(Action::Request("save-as")),
            (Profile::Windows, "C-w") => Some(Action::Request("close-tab")),
            (Profile::Windows, "C-z") | (Profile::Emacs, "C-/") => {
                Some(Action::Edit(Command::Undo))
            }
            (Profile::Windows, "C-y") => Some(Action::Edit(Command::Redo)),
            (Profile::Windows, "C-x") | (Profile::Emacs, "C-w") => Some(Action::Request("cut")),
            (Profile::Windows, "C-c") | (Profile::Emacs, "M-w") => Some(Action::Request("copy")),
            (Profile::Windows, "C-v") | (Profile::Emacs, "C-y") => Some(Action::Request("paste")),
            (Profile::Windows, "C-a") => Some(Action::SelectAll),
            (Profile::Windows, "C-f") | (Profile::Emacs, "C-s") => Some(Action::Request("find")),
            (Profile::Windows, "C-h") => Some(Action::Request("replace")),
            (Profile::Windows, "F3") => Some(Action::Request("find-next")),
            (Profile::Windows, "S-F3") => Some(Action::Request("find-previous")),
            (Profile::Windows, "C-Left") | (Profile::Emacs, "M-b") => {
                Some(motion(Motion::WordLeft))
            }
            (Profile::Windows, "C-Right") | (Profile::Emacs, "M-f") => {
                Some(motion(Motion::WordRight))
            }
            (Profile::Emacs, "C-x") => {
                self.prefix = true;
                Some(Action::Prefix)
            }
            (Profile::Emacs, "C-Space") => Some(Action::SetMark),
            (Profile::Emacs, "C-a") => Some(motion(Motion::Home)),
            (Profile::Emacs, "C-e") => Some(motion(Motion::End)),
            (Profile::Emacs, "C-b") => Some(motion(Motion::Left)),
            (Profile::Emacs, "C-f") => Some(motion(Motion::Right)),
            (Profile::Emacs, "C-p") => Some(Action::Request("up")),
            (Profile::Emacs, "C-n") => Some(Action::Request("down")),
            (Profile::Emacs, "C-r") => Some(Action::Request("find-backward")),
            (Profile::Emacs, "M-q") => Some(Action::Edit(Command::FillParagraph)),
            (Profile::Emacs, "M-x") => Some(Action::Request("command-prompt")),
            _ => None,
        };
        if let Some(action) = binding {
            return Ok(action);
        }
        let mut chars = chord.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c >= ' ' && c != '\u{7f}' => Ok(Action::Edit(Command::Type(c))),
            _ => Err(Error::InvalidArgument),
        }
    }
}

fn motion(motion: Motion) -> Action {
    Action::Edit(Command::Move {
        motion,
        extend: false,
    })
}
