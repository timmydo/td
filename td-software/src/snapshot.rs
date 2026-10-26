//! What the compositor's control socket answers (td-compositor/DESIGN.md
//! §15, "Application requests"): the status line, and a status's body,
//! read as untrusted bounded text. Pure.

pub use crate::flathub_app::{
    Admitted, CacheStatus, DeployStatus, GenerationStatus, Part, Progress, Refusal, Stage, Status,
};

/// Why a request got no `ok`, and whether that says root did not act on
/// it: an `error` answer or a request never written does; an
/// `unavailable` answer, or a conversation cut after the request was
/// written, does not (td-compositor/DESIGN.md §15).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
    pub reason: String,
    pub definite: bool,
}

/// The most of an answer td-software reads: a status's three lines and
/// td-app's 4 KiB record, with room.
pub const MAX_ANSWER_BYTES: usize = 8192;

/// An operation root runs as the application's account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Install,
    Update,
    Remove,
    DeleteData,
}

const OPERATIONS: &[(Operation, &str)] = &[
    (Operation::Install, "install"),
    (Operation::Update, "update"),
    (Operation::Remove, "remove"),
    (Operation::DeleteData, "delete-data"),
];

impl Operation {
    /// The control socket's verb.
    pub fn word(self) -> &'static str {
        OPERATIONS
            .iter()
            .find(|(operation, _)| *operation == self)
            .map_or("install", |(_, word)| word)
    }

    fn parse(word: &str) -> Option<Self> {
        OPERATIONS
            .iter()
            .find(|(_, known)| *known == word)
            .map(|(operation, _)| *operation)
    }

    /// What it is doing, for a sentence: "Installing".
    pub fn doing(self) -> &'static str {
        match self {
            Self::Install => "Installing",
            Self::Update => "Checking for an update to",
            Self::Remove => "Removing",
            Self::DeleteData => "Deleting the data of",
        }
    }

    /// Its name as a noun: "install".
    pub fn noun(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Update => "check",
            Self::Remove => "removal",
            Self::DeleteData => "data deletion",
        }
    }
}

/// What root last read of the application's status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Record {
    /// No read has finished yet.
    Unread,
    /// Read after the last operation ended.
    Current(Status),
    /// Read before the last operation ended; a new read is under way.
    Older(Status),
    /// The last read failed.
    Failed,
}

/// One status answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// What root runs on the application now.
    pub operation: Option<Operation>,
    /// The last operation that ended, and its refusal when it failed.
    pub last: Option<(Operation, Option<String>)>,
    pub record: Record,
}

impl Snapshot {
    /// The record, current or older.
    pub fn status(&self) -> Option<&Status> {
        match &self.record {
            Record::Current(status) | Record::Older(status) => Some(status),
            Record::Unread | Record::Failed => None,
        }
    }

    /// The cache, for a Flathub application's record.
    pub fn cache(&self) -> Option<&CacheStatus> {
        self.status()?.cache.as_ref()
    }

    /// Whether anything is changing the cache: root's operation, or a
    /// writer td-app's lock shows, which another path may have started.
    pub fn busy(&self) -> bool {
        self.operation.is_some() || self.cache().is_some_and(|cache| cache.progress.is_some())
    }
}

/// A reason as the window shows one: no control characters, at most 200
/// characters.
pub fn shown(reason: &str) -> String {
    reason
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(200)
        .collect()
}

/// The body of an `ok` answer, or what a refusal said: an `error` is
/// definite, an `unavailable` not (td-compositor/DESIGN.md §15).
pub fn answer(text: &str) -> Result<&str, Failure> {
    let unknown = |reason: &str| Failure {
        reason: reason.to_string(),
        definite: false,
    };
    let (status, body) = text
        .split_once('\n')
        .ok_or_else(|| unknown("the compositor's answer was cut short"))?;
    if status == "ok" {
        return Ok(body);
    }
    if let Some(reason) = status.strip_prefix("error ") {
        return Err(Failure {
            reason: shown(reason),
            definite: true,
        });
    }
    if let Some(reason) = status.strip_prefix("unavailable ") {
        return Err(unknown(&shown(reason)));
    }
    Err(unknown("the compositor's answer has no status"))
}

fn field<'a>(line: Option<&'a str>, key: &str) -> Result<&'a str, String> {
    line.and_then(|line| line.strip_prefix(key))
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or_else(|| format!("the status has no {key}= line"))
}

/// A status's body for `name`: the three lines, then td-app's record,
/// which must name `name`.
pub fn parse(name: &str, body: &str) -> Result<Snapshot, String> {
    if body.len() > MAX_ANSWER_BYTES {
        return Err("the status is too large".into());
    }
    let mut lines = body.splitn(4, '\n');
    let operation = match field(lines.next(), "operation")? {
        "none" => None,
        word => Some(Operation::parse(word).ok_or("the status names no operation")?),
    };
    let last = match field(lines.next(), "last")? {
        "none" => None,
        line => {
            let mut parts = line.splitn(3, '\t');
            let verb = parts
                .next()
                .and_then(Operation::parse)
                .ok_or("the status's last operation is malformed")?;
            match (parts.next(), parts.next()) {
                (Some("ok"), None) => Some((verb, None)),
                (Some("failed"), Some(reason)) if !reason.is_empty() => {
                    Some((verb, Some(shown(reason))))
                }
                _ => return Err("the status's last operation is malformed".into()),
            }
        }
    };
    let state = field(lines.next(), "record")?;
    let record = lines.next().unwrap_or_default();
    let read = |record: &str| -> Result<Status, String> {
        let status = Status::parse(record)?;
        if status.name != name {
            return Err("the status is another application's".into());
        }
        Ok(status)
    };
    let record = match (state, record) {
        ("unread", "") => Record::Unread,
        ("failed", "") => Record::Failed,
        ("current", record) => Record::Current(read(record)?),
        ("older", record) => Record::Older(read(record)?),
        _ => return Err("the status's record is malformed".into()),
    };
    Ok(Snapshot {
        operation,
        last,
        record,
    })
}
