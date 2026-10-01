//! What the window and the vault's thread say to each other. The window
//! names keys by their place in the list it was given and never holds a
//! credential; every title and body travels in a clearing owner.

use crate::plain::{Bytes, Text};

pub type EntryId = [u8; 16];

/// One operation the window may abandon: its prompts and its answer carry
/// the same number, so a late answer is told from the current one.
pub type Op = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Primary,
    Backup,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Backup => "backup",
        }
    }
}

/// An enrolled key as a person tells it apart: its role and fingerprint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyLabel {
    pub role: Role,
    pub fingerprint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PinUse {
    Authorize,
    Enroll,
    Proof,
}

/// A token presentation the operation waits on: `pin` is `None` while it
/// asks for the key to be connected, then the PIN's use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ask {
    pub operation: &'static str,
    pub role: Role,
    pub key: Option<String>,
    pub pin: Option<PinUse>,
}

/// An entry as the list shows it.
#[derive(Debug)]
pub struct Item {
    pub id: EntryId,
    pub revision: u64,
    pub title: Text,
}

/// A change against the entry revision the window last read.
#[derive(Debug)]
pub enum Change {
    Create {
        title: Text,
        body: Text,
    },
    Edit {
        id: EntryId,
        base: u64,
        title: Text,
        body: Text,
    },
    Rename {
        id: EntryId,
        base: u64,
        title: Text,
    },
    Delete {
        id: EntryId,
        base: u64,
    },
}

#[derive(Debug)]
pub enum Command {
    /// Protect the process, admit the vault directory and list its keys.
    Open,
    Create {
        op: Op,
    },
    Unlock {
        op: Op,
        key: usize,
    },
    Read {
        id: EntryId,
    },
    Apply {
        op: Op,
        change: Change,
    },
    /// Drop the unlocked vault.
    Lock,
}

/// The person's answer to the current `Ask`.
#[derive(Debug)]
pub enum Answer {
    Proceed,
    Pin(Bytes),
    Decline,
}

/// Why an operation failed, in words without notebook content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
    pub text: String,
    pub stale: bool,
    pub uncertain: bool,
    pub cancelled: bool,
}

#[derive(Debug)]
pub enum Reply {
    /// The enrolled keys, `None` when the account holds no vault yet.
    Opened {
        keys: Option<Vec<KeyLabel>>,
    },
    /// This host cannot keep the notebook; nothing further is possible.
    Refused {
        text: String,
    },
    Ask {
        op: Op,
        ask: Ask,
    },
    Unlocked {
        op: Op,
        entries: Vec<Item>,
    },
    Entry {
        id: EntryId,
        revision: u64,
        title: Text,
        body: Bytes,
    },
    /// The entry asked for is gone.
    Missing {
        id: EntryId,
    },
    Committed {
        op: Op,
        id: EntryId,
        revision: Option<u64>,
    },
    Failed {
        op: Op,
        failure: Failure,
    },
    /// The vault is dropped; the keys are listed again for the next unlock.
    Locked {
        keys: Option<Vec<KeyLabel>>,
    },
}
