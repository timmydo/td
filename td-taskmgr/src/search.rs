//! Bounded search entry editing in UTF-8 byte coordinates.
use crate::format::Text;
use std::fmt::Write;
#[derive(Debug, Default)]
pub struct Search {
    text: Text<256>,
    caret: usize,
    all: bool,
}
impl Search {
    pub fn text(&self) -> &str {
        self.text.as_str()
    }
    pub fn caret(&self) -> usize {
        self.text()
            .get(..self.caret)
            .map(|s| s.chars().count())
            .unwrap_or(0)
    }
    pub fn anchor(&self) -> Option<usize> {
        self.all.then_some(0)
    }
    pub fn place(&mut self, column: usize) {
        self.caret = self
            .text()
            .char_indices()
            .nth(column)
            .map(|(i, _)| i)
            .unwrap_or(self.text().len());
        self.all = false;
    }
    pub fn key(&mut self, key: &str) -> bool {
        match key {
            "C-a" => {
                self.all = true;
                self.caret = self.text().len();
                return false;
            }
            "Home" => {
                self.caret = 0;
                self.all = false;
                return false;
            }
            "End" => {
                self.caret = self.text().len();
                self.all = false;
                return false;
            }
            "Left" => {
                self.caret = self
                    .text()
                    .get(..self.caret)
                    .and_then(|s| s.char_indices().last())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.all = false;
                return false;
            }
            "Right" => {
                self.caret = self
                    .text()
                    .get(self.caret..)
                    .and_then(|s| s.chars().next())
                    .map(|c| self.caret + c.len_utf8())
                    .unwrap_or(self.text().len());
                self.all = false;
                return false;
            }
            _ => {}
        }
        let character = if key == "Space" {
            Some(' ')
        } else {
            let mut chars = key.chars();
            chars
                .next()
                .filter(|c| !c.is_control() && chars.next().is_none())
        };
        let (start, end, insert) =
            if self.all && (character.is_some() || matches!(key, "Backspace" | "Delete")) {
                (0, self.text().len(), character)
            } else if key == "Backspace" {
                (
                    self.text()
                        .get(..self.caret)
                        .and_then(|s| s.char_indices().last())
                        .map(|(i, _)| i)
                        .unwrap_or(0),
                    self.caret,
                    None,
                )
            } else if key == "Delete" {
                (
                    self.caret,
                    self.text()
                        .get(self.caret..)
                        .and_then(|s| s.chars().next())
                        .map(|c| self.caret + c.len_utf8())
                        .unwrap_or(self.text().len()),
                    None,
                )
            } else if character.is_some() {
                (self.caret, self.caret, character)
            } else {
                return false;
            };
        let mut next = Text::default();
        let Some(before) = self.text().get(..start) else {
            return false;
        };
        let Some(after) = self.text().get(end..) else {
            return false;
        };
        if next.write_str(before).is_err() {
            return false;
        }
        if let Some(ch) = insert {
            if next.write_char(ch).is_err() {
                return false;
            }
        }
        let caret = next.as_str().len();
        if next.write_str(after).is_err() {
            return false;
        }
        let changed = next.as_str() != self.text();
        self.text = next;
        self.caret = caret;
        self.all = false;
        changed
    }
}
