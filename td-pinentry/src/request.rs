//! What one prompt asks and what the person answered, shared by the
//! pinentry protocol, the askpass convention and the window.

use std::hint::black_box;

/// The most bytes an answer holds. gpg-agent's own passphrase ceiling is
/// lower; a longer paste is refused whole by the field.
pub const MAX_SECRET: usize = 1024;

/// One prompt: the texts the window shows and what it asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The window's title.
    pub title: String,
    /// The explanation, a newline starting a row of its own.
    pub description: String,
    /// The field's label.
    pub prompt: String,
    /// Why the last answer was refused, shown once above the field.
    pub error: Option<String>,
    pub ok: String,
    pub cancel: String,
    /// A third button answering "no" rather than "cancel" (pinentry's
    /// `SETNOTOK`).
    pub not_ok: Option<String>,
    pub kind: Kind,
    /// Seconds the window waits for an answer before giving up.
    pub timeout: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// A text to type: masked unless it is no secret (a user name, a
    /// host key's yes or no), and typed twice when `repeat` labels the
    /// second field.
    Text {
        masked: bool,
        repeat: Option<String>,
        repeat_error: String,
    },
    /// A question answered by its buttons alone; `one_button` shows OK
    /// without Cancel.
    Confirm { one_button: bool },
}

impl Request {
    /// The texts pinentry starts from before the agent sets any.
    pub fn new(kind: Kind) -> Self {
        Self {
            title: "td-pinentry".to_owned(),
            description: String::new(),
            prompt: "PIN:".to_owned(),
            error: None,
            ok: "OK".to_owned(),
            cancel: "Cancel".to_owned(),
            not_ok: None,
            kind,
            timeout: None,
        }
    }

    /// A masked text asked once.
    #[cfg(test)]
    pub fn secret() -> Self {
        Self::new(Kind::Text {
            masked: true,
            repeat: None,
            repeat_error: DEFAULT_REPEAT_ERROR.to_owned(),
        })
    }
}

/// What the window says when the two fields differ and the agent named
/// nothing else.
pub const DEFAULT_REPEAT_ERROR: &str = "The two entries do not match.";

/// How a prompt ended.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// The field's text.
    Text(Secret),
    /// OK on a question.
    Confirmed,
    /// Cancel, Escape or closing the window.
    Cancelled,
    /// The `not_ok` button.
    Declined,
    /// The request's timeout passed.
    TimedOut,
    /// The caller went away while the window was up.
    Hangup,
    /// No window could be shown, and why.
    Failed(String),
}

/// An answer's text, zeroed when dropped and never shown by `Debug`.
/// Copies the compiler, allocator or a pipe made are outside its reach.
#[derive(PartialEq)]
pub struct Secret(String);

impl Secret {
    /// A copy of `text` in a buffer of exactly its size, so nothing later
    /// reallocates it and leaves a copy behind.
    pub fn copy(text: &str) -> Self {
        let mut owned = String::with_capacity(text.len());
        owned.push_str(text);
        Self(owned)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(std::mem::take(&mut self.0).into_bytes());
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

/// Zeroes a buffer, its spare capacity included, before it is freed.
pub fn wipe(mut bytes: Vec<u8>) {
    bytes.fill(0);
    for byte in bytes.spare_capacity_mut() {
        byte.write(0);
    }
    black_box(bytes.as_mut_slice());
    black_box(bytes.spare_capacity_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_shows_through_debug() {
        let secret = Secret::copy("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(7 bytes)");
        assert_eq!(secret.as_str(), "hunter2");
        assert_eq!(
            format!("{:?}", Answer::Text(secret)),
            "Text(Secret(7 bytes))"
        );
    }
}
