//! The elevation principal table (APPLICATIONS.md §L.1, "Principal
//! table"): which accounts may perform which v1 operation, read from the
//! deployment image before any description exists. A missing, unreadable
//! or malformed table grants nothing.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const DIRECTORY: &str = "/etc";
const TABLE: &str = "td-elevation.tsv";
const HEADER: &str = "td-elevation-v1";
const LIMIT: u64 = 4096;
const ROWS: usize = 64;
const NOFOLLOW: i32 = 0x20000;
const NONBLOCK: i32 = 0x800;
const DIRECTORY_ONLY: i32 = 0x10000;

/// A v1 operation, in the table's canonical order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    DeployRollback,
    SetHostname,
    DeployPublish,
}

impl Operation {
    const ALL: &'static [Self] = &[Self::DeployRollback, Self::SetHostname, Self::DeployPublish];

    fn name(self) -> &'static str {
        match self {
            Self::DeployRollback => "deploy-rollback",
            Self::SetHostname => "set-hostname",
            Self::DeployPublish => "deploy-publish",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|operation| operation.name() == name)
    }
}

/// Each row's account and the operations it may perform.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Table {
    rows: Vec<(u32, Vec<Operation>)>,
}

impl Table {
    /// The table in the running deployment's `/etc`, root's.
    pub fn load() -> Result<Self, String> {
        Self::load_from(Path::new(DIRECTORY), (0, 0))
    }

    /// `TABLE` in `directory`: the directory owned by `owner` and writable
    /// by no other, and the table one link, mode 0444, of the same owner,
    /// read without following a link or waiting on a FIFO.
    pub(crate) fn load_from(directory: &Path, owner: (u32, u32)) -> Result<Self, String> {
        let etc = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | DIRECTORY_ONLY)
            .open(directory)
            .map_err(|e| format!("open the elevation table's directory: {e}"))?;
        let metadata = etc.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_dir()
            || (metadata.uid(), metadata.gid()) != owner
            || metadata.mode() & 0o022 != 0
        {
            return Err("the elevation table's directory is not protected".into());
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW | NONBLOCK)
            .open(format!("/proc/self/fd/{}/{TABLE}", etc.as_raw_fd()))
            .map_err(|e| format!("open {TABLE}: {e}"))?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || (metadata.uid(), metadata.gid()) != owner
            || metadata.mode() & 0o7777 != 0o444
            || metadata.nlink() != 1
            || metadata.len() > LIMIT
        {
            return Err(format!(
                "{TABLE} must be one bounded mode-0444 regular file of its directory's owner"
            ));
        }
        Self::parse(&read(file)?)
    }

    /// `td-elevation-v1`, then one row per account in strictly increasing
    /// UID order: a canonical decimal UID from 1000 to 65533, then a tab
    /// before each operation it may perform, each at most once and in the
    /// canonical order; every line newline-terminated.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let body = text
            .strip_suffix('\n')
            .ok_or("the elevation table does not end its last line")?;
        let mut lines = body.split('\n');
        if lines.next() != Some(HEADER) {
            return Err("the elevation table is not td-elevation-v1".into());
        }
        let mut rows: Vec<(u32, Vec<Operation>)> = Vec::new();
        for line in lines {
            if rows.len() == ROWS {
                return Err("the elevation table has too many rows".into());
            }
            let mut fields = line.split('\t');
            let uid = fields
                .next()
                .and_then(account)
                .ok_or("an elevation row does not begin with a human UID")?;
            if rows.last().is_some_and(|(last, _)| *last >= uid) {
                return Err("elevation rows are not in increasing UID order".into());
            }
            let mut operations: Vec<Operation> = Vec::new();
            for name in fields {
                let operation =
                    Operation::named(name).ok_or("an elevation row names an unknown operation")?;
                let order = |operation: Operation| {
                    Operation::ALL.iter().position(|known| *known == operation)
                };
                if operations
                    .last()
                    .is_some_and(|last| order(*last) >= order(operation))
                {
                    return Err("an elevation row's operations are not in canonical order".into());
                }
                operations.push(operation);
            }
            if operations.is_empty() {
                return Err("an elevation row grants nothing".into());
            }
            rows.push((uid, operations));
        }
        Ok(Self { rows })
    }

    /// Whether a row grants `operation` to `uid`.
    pub fn grants(&self, uid: u32, operation: Operation) -> bool {
        self.rows
            .iter()
            .any(|(row, operations)| *row == uid && operations.contains(&operation))
    }
}

fn read(file: File) -> Result<String, String> {
    let mut text = String::new();
    file.take(LIMIT + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("read {TABLE}: {e}"))?;
    if u64::try_from(text.len()).map_or(true, |length| length > LIMIT) {
        return Err(format!("{TABLE} grew past its bound"));
    }
    Ok(text)
}

/// A human UID's canonical decimal spelling.
fn account(text: &str) -> Option<u32> {
    if text.is_empty()
        || text.len() > 5
        || text.starts_with('0')
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok().filter(|uid| (1000..=65533).contains(uid))
}

#[cfg(test)]
#[path = "../tests/elevation.rs"]
pub(crate) mod tests;
