//! What the window and the vault's thread say to each other. The window
//! names keys by their place in the list it was given and never holds a
//! credential; every title and body travels in a clearing owner.

use std::path::PathBuf;

use crate::plain::{Bytes, Text};

pub type EntryId = [u8; 16];

/// The largest encrypted copy, td-secret's `pass::MAX_COPY`; a file
/// larger is not offered for import.
pub const MAX_COPY: usize = 4 * 1024 * 1024 + 16 + 65 + 8 * 1196;

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

/// The keys an unlocked notebook holds, and the one that authorizes adding
/// a key: the key it was unlocked with, or that key's replacement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Keys {
    pub labels: Vec<KeyLabel>,
    pub using: Option<usize>,
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
    /// Open again, the person having accepted the swap the last `Swap`
    /// reply named, for this process.
    AcceptSwap,
    /// Create a notebook, with a backup key or, when `backup` is false,
    /// the primary alone, as the person chose.
    Create {
        op: Op,
        backup: bool,
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
    /// Enroll another backup key.
    AddKey {
        op: Op,
    },
    /// Revoke the unlocked notebook's keys `revoked` and enroll one
    /// replacement; the vault key rotates.
    ReplaceKeys {
        op: Op,
        revoked: Vec<usize>,
    },
    /// Write the unlocked notebook's encrypted copy, as a new file, into
    /// `folder`.
    Export {
        op: Op,
        folder: PathBuf,
    },
    /// Read an encrypted copy for import and list the keys it opens with.
    ReadCopy {
        op: Op,
        path: PathBuf,
    },
    /// Place the copy read, authenticated with its key `key`, into this
    /// account, which holds no vault.
    Import {
        op: Op,
        key: usize,
    },
    /// Drop the unlocked vault, and any copy read for import.
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
    /// Swap on these devices can put memory on storage; nothing is open
    /// until the person accepts it with `AcceptSwap`.
    Swap {
        devices: Vec<String>,
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
        keys: Keys,
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
    /// A key operation committed; the unlocked notebook's keys now.
    Keys {
        op: Op,
        keys: Keys,
    },
    /// The encrypted copy is written, at `path`.
    Exported {
        op: Op,
        path: String,
    },
    /// A copy is read; it opens with any of `keys`.
    Copy {
        op: Op,
        keys: Vec<KeyLabel>,
    },
    /// The vault is dropped; the keys are listed again for the next unlock.
    Locked {
        keys: Option<Vec<KeyLabel>>,
    },
}

/// What the host did, as the window learns it.
#[derive(Debug, Eq, PartialEq)]
pub enum HostEvent {
    /// The host's lock and sleep are watched from now on.
    Watched,
    /// The session was locked.
    Lock,
    /// The system is about to sleep; sleep waits, where it can, until
    /// the window has locked.
    Suspend,
    /// The host's lock and sleep can no longer be watched, and why.
    Lost(String),
    /// They could not be watched at all, and why.
    Unwatched(String),
    /// They are watched, but sleep did not wait for the lock: refused
    /// at the start, or at the first sleep without a delay.
    Undelayed,
}
