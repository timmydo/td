//! The workload account ledger (td-authd/DESIGN.md, "Workload account
//! ledger"; WORKLOADS.md, "Accounts"): the uid each workload account holds,
//! keyed by kind and name. Rows are never removed, so no uid is reused,
//! after an application's removal included. The file is std-only and uses
//! no other module, so target programs include it by path: td-firstboot
//! seeds the ledger at boot and allocates in it when td-authd installs an
//! application or root asks for a sandbox's account, and `load` is the one
//! live reader td-login and the bus policy's includers share. Parsing,
//! seeding and allocation perform no I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::ops::RangeInclusive;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAGIC: &str = "td-accounts-v1\n";
/// The ledger's name in td's persistent state directory, `DIRECTORY`.
pub const FILE_NAME: &str = "accounts.tsv";
/// td's persistent state directory, root-owned without other writers.
pub const DIRECTORY: &str = "/var/lib/td";
/// The published ledger's exact mode: every account reads it.
pub const MODE: u32 = 0o644;
/// Linux x86-64 open flags, which std does not name; the ledger's readers
/// and the application policy's declarations reader share them.
pub const O_NOFOLLOW: i32 = 0x20000;
pub const O_DIRECTORY: i32 = 0x10000;
pub const O_NONBLOCK: i32 = 0x800;
/// Rows are permanent, so the v1 bounds hold every row a later deployment
/// may add, or rolling back to this one would refuse its ledger: 4096 rows
/// at a 64-byte average.
pub const MAX_BYTES: usize = 256 * 1024;
const MAX_ROWS: usize = 4096;
/// The sandbox and workspace rows allocation adds, together, at most. Rows
/// are permanent and a session's programs can ask root for any sandbox
/// name, so without this bound they could spend every row and leave no
/// application install or carried row a place. Parsing does not apply it:
/// a ledger that already holds more stays readable.
pub const WORKLOAD_ROWS: usize = 2048;
/// Application accounts.
pub const APPLICATION_UIDS: RangeInclusive<u32> = 65536..=98303;
/// The application block the image reserves: the carried rows, the retained
/// reservations seeding imports (Claude Code's 65539), and the uid of any
/// application the image will ship. Allocation never enters it, so neither
/// a later carried row nor a reservation a rolled-back deployment retains
/// can meet a uid an install took.
pub const CARRIED_UIDS: RangeInclusive<u32> = 65536..=66559;
/// The application uids an install allocates: the rest of the range.
pub const ALLOCATED_UIDS: RangeInclusive<u32> = 66560..=98303;
/// Sandbox and workspace accounts, which share one range. td-firstboot
/// allocates sandbox rows; v1 parses workspace rows so a deployment that
/// writes them can roll back.
pub const WORKLOAD_UIDS: RangeInclusive<u32> = 98304..=131071;
/// Every uid the ledger allocates, application through workspace: no
/// passwd or group entry may hold one.
pub const LEDGER_UIDS: RangeInclusive<u32> = 65536..=131071;
/// The reserved name of an application account: this, then its uid.
pub const APPLICATION_ACCOUNT_PREFIX: &str = "tda";
/// The reserved name of a sandbox or workspace account: this, then its uid.
/// The two kinds share one range, so one form names either.
pub const WORKLOAD_ACCOUNT_PREFIX: &str = "tdw";
/// An application account's cgroup delegation below the unified root is
/// this, then its uid; a sandbox's or workspace's the next.
pub const APPLICATION_DELEGATION_PREFIX: &str = "td-app-";
pub const WORKLOAD_DELEGATION_PREFIX: &str = "td-workload-";

/// The digits after `prefix`, when `name` is `prefix` and decimal digits.
fn account_digits<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    name.strip_prefix(prefix)
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The uid `name` spells canonically after `prefix`, if it does.
fn account_uid(name: &str, prefix: &str) -> Option<u32> {
    let digits = account_digits(name, prefix)?;
    digits
        .parse::<u32>()
        .ok()
        .filter(|uid| uid.to_string() == digits)
}

/// Whether `name` has the reserved application account form, `tda` and
/// decimal digits. Only the ledger answers for such a name; another name
/// that merely begins `tda` is an ordinary one.
pub fn application_account_name(name: &str) -> bool {
    account_digits(name, APPLICATION_ACCOUNT_PREFIX).is_some()
}

/// The uid an application account name spells canonically, if it is one.
pub fn application_account_uid(name: &str) -> Option<u32> {
    account_uid(name, APPLICATION_ACCOUNT_PREFIX)
}

/// Whether `name` has the reserved sandbox and workspace account form,
/// `tdw` and decimal digits, which only the ledger answers for.
pub fn workload_account_name(name: &str) -> bool {
    account_digits(name, WORKLOAD_ACCOUNT_PREFIX).is_some()
}

/// The uid a sandbox or workspace account name spells canonically, if it is
/// one.
pub fn workload_account_uid(name: &str) -> Option<u32> {
    account_uid(name, WORKLOAD_ACCOUNT_PREFIX)
}

/// Whether `name` has either reserved form: a ledger account's, which no
/// passwd or group entry may take.
pub fn ledger_account_name(name: &str) -> bool {
    application_account_name(name) || workload_account_name(name)
}

/// The cgroup delegation of the ledger account holding `uid`, by its range:
/// `td-app-UID` for an application, `td-workload-UID` for a sandbox or
/// workspace; none outside the ledger's ranges.
pub fn delegation(uid: u32) -> Option<DelegationName> {
    let prefix = if APPLICATION_UIDS.contains(&uid) {
        APPLICATION_DELEGATION_PREFIX
    } else if WORKLOAD_UIDS.contains(&uid) {
        WORKLOAD_DELEGATION_PREFIX
    } else {
        return None;
    };
    Some(DelegationName { prefix, uid })
}

/// A delegation's name, its range's prefix then its uid. It allocates
/// nothing: the caller formats it into storage of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationName {
    prefix: &'static str,
    uid: u32,
}

impl std::fmt::Display for DelegationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Through the parts' own Display: td-authd's confinement holds this
        // file's production text free of the word for writing files.
        std::fmt::Display::fmt(self.prefix, f)?;
        std::fmt::Display::fmt(&self.uid, f)
    }
}

/// The uids the image reserved before the ledger existed. Every seeded
/// ledger holds them, whether or not the deployment ships the application.
/// Each lies in `CARRIED_UIDS`, as does an application the image adds.
const CARRIED: &[(&str, u32)] = &[
    ("firefox", 65536),
    ("mail", 65537),
    ("news", 65538),
    ("web", 65540),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Application,
    Sandbox,
    Workspace,
}

impl Kind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "application" => Some(Self::Application),
            "sandbox" => Some(Self::Sandbox),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Sandbox => "sandbox",
            Self::Workspace => "workspace",
        }
    }

    pub fn uids(self) -> RangeInclusive<u32> {
        match self {
            Self::Application => APPLICATION_UIDS,
            Self::Sandbox | Self::Workspace => WORKLOAD_UIDS,
        }
    }

    /// The directory below `DIRECTORY` holding this kind's account homes,
    /// each named by its uid.
    pub fn homes(self) -> &'static str {
        match self {
            Self::Application => "applications",
            Self::Sandbox | Self::Workspace => "workloads",
        }
    }

    /// The part of `uids` allocation draws from.
    fn allocated(self) -> RangeInclusive<u32> {
        match self {
            Self::Application => ALLOCATED_UIDS,
            Self::Sandbox | Self::Workspace => WORKLOAD_UIDS,
        }
    }
}

/// Assignment data, not an authority token: admission must come from a
/// ledger parsed from the root-owned file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    rows: BTreeMap<(Kind, String), u32>,
}

/// The principal table's application name grammar (td-authd/DESIGN.md,
/// "Principal registry prerequisite"), copied: td-firstboot compiles both
/// files, but the ledger's coming readers do not compile `principals.rs`.
/// The engine crate's tests hold the two equal. The grant table keys its
/// rows by the same names.
pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

fn decimal(value: &str, range: RangeInclusive<u32>) -> Result<u32, String> {
    let number = value
        .parse::<u32>()
        .map_err(|_| "invalid account ledger uid")?;
    if !range.contains(&number) || number.to_string() != value {
        return Err("noncanonical or out-of-range account ledger uid".into());
    }
    Ok(number)
}

impl Ledger {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.ends_with('\n') || !text.is_ascii() {
            return Err("account ledger is oversized or not canonical ASCII lines".into());
        }
        let rows = text
            .strip_prefix(MAGIC)
            .ok_or("unsupported account ledger format")?;
        let mut ledger = Self::default();
        let mut held = BTreeSet::new();
        let mut previous: Option<(Kind, &str)> = None;
        for (index, row) in rows.split_terminator('\n').enumerate() {
            if index >= MAX_ROWS {
                return Err("account ledger exceeds 4096 rows".into());
            }
            let fields: Vec<&str> = row.split('\t').take(4).collect();
            let [kind, name, uid] = fields.as_slice() else {
                return Err("invalid account ledger row shape".into());
            };
            let kind = Kind::parse(kind).ok_or("unknown account ledger kind")?;
            if !valid_name(name) {
                return Err("invalid account ledger name".into());
            }
            let uid = decimal(uid, kind.uids())?;
            if previous.is_some_and(|previous| previous >= (kind, *name)) {
                return Err("account ledger rows are duplicate or unsorted".into());
            }
            if !held.insert(uid) {
                return Err("account ledger assigns a uid more than once".into());
            }
            ledger.rows.insert((kind, (*name).to_string()), uid);
            previous = Some((kind, *name));
        }
        Ok(ledger)
    }

    pub fn encode(&self) -> String {
        let mut text = MAGIC.to_string();
        for ((kind, name), uid) in &self.rows {
            text.push_str(&format!("{}\t{name}\t{uid}\n", kind.as_str()));
        }
        text
    }

    pub fn uid_of(&self, kind: Kind, name: &str) -> Option<u32> {
        self.rows.get(&(kind, name.to_string())).copied()
    }

    /// The name of the `kind` account holding `uid`.
    pub fn name(&self, kind: Kind, uid: u32) -> Option<&str> {
        self.rows
            .iter()
            .find(|((held, _), row)| *held == kind && **row == uid)
            .map(|((_, name), _)| name.as_str())
    }

    /// The kind and uid of the account a reserved name spells, when this
    /// ledger holds it: `tda<uid>` an application row's, `tdw<uid>` a
    /// sandbox's or workspace's. Any other name, a uid no row of a kind its
    /// form names holds, or a noncanonical spelling, is none.
    pub fn account(&self, name: &str) -> Option<(Kind, u32)> {
        let (uid, kinds): (u32, &[Kind]) = match application_account_uid(name) {
            Some(uid) => (uid, &[Kind::Application]),
            None => (
                workload_account_uid(name)?,
                &[Kind::Sandbox, Kind::Workspace],
            ),
        };
        kinds
            .iter()
            .find(|kind| self.name(**kind, uid).is_some())
            .map(|kind| (*kind, uid))
    }

    /// Whether this ledger holds every row of `older` at the same uid. Rows
    /// are never removed or reassigned, so a later read of the published
    /// ledger always does; a reader keeps its copy when one does not.
    pub fn extends(&self, older: &Self) -> bool {
        older
            .rows
            .iter()
            .all(|(key, uid)| self.rows.get(key) == Some(uid))
    }

    /// This ledger with the carried reservations and `imported` application
    /// rows (the retained principal ledger's) added. An existing row is
    /// never reassigned and no uid is given to a second name.
    pub fn seed<'a>(
        &self,
        imported: impl IntoIterator<Item = (&'a str, u32)>,
    ) -> Result<Self, String> {
        let mut next = self.clone();
        for (name, uid) in CARRIED.iter().copied().chain(imported) {
            next.insert(Kind::Application, name, uid)?;
        }
        next.checked()
    }

    /// This ledger with `name` holding a `kind` account, that account's uid,
    /// and whether the row is new. An existing row is returned unchanged;
    /// otherwise the row takes the lowest uid of the kind's allocated range
    /// (`ALLOCATED_UIDS` for an application, never the carried block) that
    /// no row of any kind holds and `taken` does not name
    /// (td-authd/DESIGN.md, "Workload account ledger"). No caller supplies
    /// the uid. A new sandbox or workspace row is refused once those kinds
    /// hold `WORKLOAD_ROWS` together, so the rest of `MAX_ROWS` stays for
    /// applications and carried rows.
    pub fn allocate(
        &self,
        kind: Kind,
        name: &str,
        taken: &BTreeSet<u32>,
    ) -> Result<(Self, u32, bool), String> {
        if let Some(uid) = self.uid_of(kind, name) {
            return Ok((self.clone(), uid, false));
        }
        if kind != Kind::Application
            && self
                .rows
                .keys()
                .filter(|(held, _)| *held != Kind::Application)
                .count()
                >= WORKLOAD_ROWS
        {
            return Err(format!(
                "the account ledger holds {WORKLOAD_ROWS} sandbox and workspace accounts, \
                 its bound; refusing {name:?}"
            ));
        }
        let held: BTreeSet<u32> = self.rows.values().copied().collect();
        let uid = kind
            .allocated()
            .find(|uid| !held.contains(uid) && !taken.contains(uid))
            .ok_or_else(|| format!("no {} uid is free for {name:?}", kind.as_str()))?;
        let mut next = self.clone();
        next.insert(kind, name, uid)?;
        Ok((next.checked()?, uid, true))
    }

    fn insert(&mut self, kind: Kind, name: &str, uid: u32) -> Result<(), String> {
        if !valid_name(name) || !kind.uids().contains(&uid) {
            return Err(format!(
                "account {name:?} uid {uid} is outside the account ledger's rules"
            ));
        }
        match self.uid_of(kind, name) {
            Some(held) if held == uid => return Ok(()),
            Some(held) => {
                return Err(format!(
                    "account {name:?} holds uid {held}; refusing to reassign it {uid}"
                ))
            }
            None => {}
        }
        if self.rows.values().any(|held| *held == uid) {
            return Err(format!("uid {uid} already belongs to another account"));
        }
        self.rows.insert((kind, name.to_string()), uid);
        Ok(())
    }

    /// Aggregate bounds, checked on the canonical encoding readers parse.
    fn checked(self) -> Result<Self, String> {
        Self::parse(&self.encode())
    }
}

/// The published ledger, read as every consumer reads it.
pub fn load() -> Result<Ledger, String> {
    load_from(Path::new(DIRECTORY), (0, 0))
}

/// What identifies one published ledger: publication renames a new file
/// into place, so a later ledger has another inode, and an edit or a mode
/// repair in place changes its size, modification or change time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// `directory`, opened without following links, if `owner` owns it and no
/// one else may write it.
fn open_directory(directory: &Path, owner: (u32, u32)) -> Result<std::fs::File, String> {
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(directory)
        .map_err(|error| format!("open the account ledger's directory: {error}"))?;
    let metadata = parent.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || (metadata.uid(), metadata.gid()) != owner
        || metadata.mode() & 0o022 != 0
    {
        return Err("the account ledger's directory is not root-controlled".into());
    }
    Ok(parent)
}

/// The stamp of the ledger in `directory` as it stands, read without
/// opening it, so a reader can tell whether it changed since its last read.
pub fn stamp_from(directory: &Path, owner: (u32, u32)) -> Result<Stamp, String> {
    let parent = open_directory(directory, owner)?;
    std::fs::symlink_metadata(format!("/proc/self/fd/{}/{FILE_NAME}", parent.as_raw_fd()))
        .map(|metadata| Stamp::of(&metadata))
        .map_err(|error| format!("inspect {FILE_NAME}: {error}"))
}

/// The ledger in `directory`, which `owner` must own without other writers:
/// opened through the pinned directory without following links, a
/// single-link regular file of exact mode 0644 that `owner` owns, read to a
/// bound. An absent ledger is an error: firstboot seeds it before any
/// consumer starts.
pub fn load_from(directory: &Path, owner: (u32, u32)) -> Result<Ledger, String> {
    load_stamped_from(directory, owner).map(|(ledger, _)| ledger)
}

/// `load_from`, with the stamp of the file it read.
pub fn load_stamped_from(directory: &Path, owner: (u32, u32)) -> Result<(Ledger, Stamp), String> {
    let parent = open_directory(directory, owner)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(format!("/proc/self/fd/{}/{FILE_NAME}", parent.as_raw_fd()))
        .map_err(|error| format!("open {FILE_NAME}: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    let stamp = Stamp::of(&metadata);
    if !metadata.is_file()
        || (metadata.uid(), metadata.gid()) != owner
        || metadata.mode() & 0o7777 != MODE
        || metadata.nlink() != 1
        || metadata.len() > MAX_BYTES as u64
    {
        return Err(format!(
            "{FILE_NAME} must be one bounded, single-link, root-owned regular file of mode 0644"
        ));
    }
    let mut text = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read {FILE_NAME}: {error}"))?;
    Ledger::parse(&text).map(|ledger| (ledger, stamp))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const SEEDED: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\napplication\tnews\t65538\napplication\tweb\t65540\n";

    fn seeded() -> Ledger {
        Ledger::default().seed([]).unwrap()
    }

    /// `count` workspace rows of `width`-byte names, uids from 100000.
    fn workspaces(count: usize, width: usize) -> String {
        let mut text = String::new();
        for index in 0..count {
            let name = format!("w{index:05}{}", "a".repeat(width - 6));
            text.push_str(&format!("workspace\t{name}\t{}\n", 100000 + index));
        }
        text
    }

    #[test]
    fn the_seed_carries_the_image_reservations_canonically() {
        let ledger = seeded();
        assert_eq!(ledger.encode(), SEEDED);
        assert_eq!(Ledger::parse(SEEDED).unwrap(), ledger);
        assert_eq!(ledger.uid_of(Kind::Application, "web"), Some(65540));
        assert_eq!(ledger.uid_of(Kind::Application, "claude"), None);
        assert_eq!(Ledger::parse(MAGIC).unwrap(), Ledger::default());
        assert_eq!(Ledger::default().encode(), MAGIC);
    }

    #[test]
    fn later_kinds_parse_and_survive_seeding() {
        // A later deployment's rows, which this one must keep on rollback.
        let text = format!(
            "{SEEDED}sandbox\tdefault\t98304\nworkspace\tagent-1\t131071\nworkspace\tagent-2\t98305\n"
        );
        let ledger = Ledger::parse(&text).unwrap();
        assert_eq!(ledger.encode(), text);
        assert_eq!(ledger.uid_of(Kind::Sandbox, "default"), Some(98304));
        assert_eq!(ledger.uid_of(Kind::Workspace, "agent-1"), Some(131071));
        assert_eq!(ledger.seed([]).unwrap(), ledger);
        // A sandbox and a workspace may share a name, never a uid.
        assert!(Ledger::parse(&format!("{MAGIC}sandbox\tx\t98304\nworkspace\tx\t98305\n")).is_ok());
        for altered in [
            text.replace("98305", "98304"),
            text.replace("131071", "131072"),
            text.replace("98304", "98303"),
            text.replace("\t65540", "\t98306"),
            text.replace("sandbox\tdefault", "workspace\tdefault"),
        ] {
            assert!(Ledger::parse(&altered).is_err(), "{altered:?}");
        }
    }

    #[test]
    fn seeding_is_idempotent_and_imports_retired_reservations() {
        let ledger = seeded();
        assert_eq!(ledger.seed([]).unwrap(), ledger);
        // The image's own rows are the carried ones.
        let image = [
            ("firefox", 65536),
            ("mail", 65537),
            ("news", 65538),
            ("web", 65540),
        ];
        assert_eq!(ledger.seed(image).unwrap(), ledger);
        // An install enrolled while Claude Code shipped keeps 65539.
        let imported = Ledger::default().seed([("claude", 65539)]).unwrap();
        assert_eq!(imported.uid_of(Kind::Application, "claude"), Some(65539));
        assert_eq!(imported.seed([]).unwrap(), imported);
        assert_eq!(ledger.seed([("claude", 65539)]).unwrap(), imported);
        // A row the ledger already holds, an allocation's, survives.
        let allocated = Ledger::parse(&format!(
            "{}application\twriter\t65541\n",
            imported.encode()
        ))
        .unwrap();
        assert_eq!(allocated.seed([("claude", 65539)]).unwrap(), allocated);
    }

    #[test]
    fn seeding_refuses_reassignment_and_shared_uids() {
        let ledger = seeded();
        for imported in [
            ("mail", 65539),
            ("claude", 65536),
            ("firefox", 65537),
            // The principal table admits these uids; the ledger does not.
            ("claude", 98304),
            ("claude", 2147483647),
            ("claude", 65535),
            ("Claude", 65539),
        ] {
            assert!(ledger.seed([imported]).is_err(), "{imported:?}");
            assert!(Ledger::default().seed([imported]).is_err(), "{imported:?}");
        }
        // One name under two owners in the principal table.
        assert!(ledger.seed([("mail", 65537), ("mail", 65541)]).is_err());
        assert!(ledger.seed([("claude", 65539), ("other", 65539)]).is_err());
        let moved = Ledger::parse(&SEEDED.replace("mail\t65537", "mail\t65541")).unwrap();
        assert!(moved.seed([]).is_err());
        let taken = Ledger::parse("td-accounts-v1\napplication\tclaude\t65536\n").unwrap();
        assert!(taken.seed([]).is_err());
    }

    #[test]
    fn the_row_and_byte_bounds_are_exact() {
        let full = format!("{MAGIC}{}", workspaces(MAX_ROWS, 8));
        assert!(Ledger::parse(&full).is_ok());
        assert!(Ledger::parse(&format!("{full}workspace\tzzz\t131071\n")).is_err());
        // A full ledger cannot take the carried rows.
        let nearly = format!("{MAGIC}{}", workspaces(MAX_ROWS - 3, 8));
        assert!(Ledger::parse(&nearly).unwrap().seed([]).is_err());
        // 64-byte names fill the byte bound before the row bound: one row
        // of 18 bytes plus its name makes up the remainder exactly.
        let rows = (MAX_BYTES - MAGIC.len()) / 82;
        let remainder = MAX_BYTES - MAGIC.len() - rows * 82 - 18;
        let last = |width: usize| {
            format!(
                "workspace\tw{rows:05}{}\t{}\n",
                "a".repeat(width - 6),
                100000 + rows
            )
        };
        let exact = format!("{MAGIC}{}{}", workspaces(rows, 64), last(remainder));
        assert_eq!(exact.len(), MAX_BYTES);
        assert!(Ledger::parse(&exact).is_ok());
        let over = format!("{MAGIC}{}{}", workspaces(rows, 64), last(remainder + 1));
        assert_eq!(over.len(), MAX_BYTES + 1);
        assert!(Ledger::parse(&over).is_err());
    }

    #[test]
    fn the_carried_block_holds_every_carried_row_and_allocation_never_enters_it() {
        for (name, uid) in CARRIED {
            assert!(CARRIED_UIDS.contains(uid), "{name}");
        }
        // The two blocks split the application range exactly.
        assert_eq!(CARRIED_UIDS.start(), APPLICATION_UIDS.start());
        assert_eq!(*CARRIED_UIDS.end() + 1, *ALLOCATED_UIDS.start());
        assert_eq!(ALLOCATED_UIDS.end(), APPLICATION_UIDS.end());
        // Claude's retired 65539 and the rest of the block stay free, yet an
        // install takes the first allocated uid.
        let (_, uid, new) = seeded()
            .allocate(Kind::Application, "photo", &BTreeSet::new())
            .unwrap();
        assert_eq!((uid, new), (66560, true));
        // Even with every allocated uid taken, none comes from the block.
        let allocated: BTreeSet<u32> = ALLOCATED_UIDS.collect();
        assert!(seeded()
            .allocate(Kind::Application, "photo", &allocated)
            .is_err());
    }

    #[test]
    fn allocation_takes_the_lowest_free_uid_and_keeps_existing_rows() {
        let ledger = seeded();
        let none = BTreeSet::new();
        let (next, uid, new) = ledger.allocate(Kind::Application, "photo", &none).unwrap();
        assert_eq!((uid, new), (66560, true));
        assert_eq!(next.uid_of(Kind::Application, "photo"), Some(66560));
        assert!(next.extends(&ledger));
        assert_eq!(Ledger::parse(&next.encode()).unwrap(), next);
        // Skips a uid taken outside the ledger (a retained reservation, a
        // home left behind, passwd, group) and the rows already held.
        let (twice, uid, _) = next.allocate(Kind::Application, "writer", &none).unwrap();
        assert_eq!(uid, 66561);
        let taken = BTreeSet::from([66562, 66564]);
        let (skipped, uid, _) = twice.allocate(Kind::Application, "notes", &taken).unwrap();
        assert_eq!(uid, 66563);
        // An existing row is returned unchanged, even at a taken uid.
        let (again, uid, new) = skipped
            .allocate(Kind::Application, "notes", &BTreeSet::from([66563]))
            .unwrap();
        assert_eq!((uid, new), (66563, false));
        assert_eq!(again, skipped);
        assert_eq!(
            ledger
                .allocate(Kind::Application, "firefox", &BTreeSet::from([65536]))
                .unwrap(),
            (ledger.clone(), 65536, false)
        );
        // Another kind's rows hold their uids too: a workspace at 98304
        // pushes a sandbox to 98305.
        let workspace = Ledger::parse(&format!("{SEEDED}workspace\tagent\t98304\n")).unwrap();
        let (_, uid, _) = workspace.allocate(Kind::Sandbox, "default", &none).unwrap();
        assert_eq!(uid, 98305);
        // A sandbox may share a workspace's name, never its uid.
        let (_, uid, new) = workspace.allocate(Kind::Sandbox, "agent", &none).unwrap();
        assert_eq!((uid, new), (98305, true));
    }

    #[test]
    fn allocation_refuses_exhaustion_and_invalid_names() {
        let ledger = seeded();
        let everything: BTreeSet<u32> = APPLICATION_UIDS.collect();
        assert!(ledger
            .allocate(Kind::Application, "photo", &everything)
            .is_err());
        // Only the last uid of the range is free.
        let last: BTreeSet<u32> = (65536..98303).collect();
        let (_, uid, _) = ledger.allocate(Kind::Application, "photo", &last).unwrap();
        assert_eq!(uid, 98303);
        for name in ["", "Photo", "1photo", "photo/x", &"p".repeat(65)] {
            assert!(
                ledger
                    .allocate(Kind::Application, name, &BTreeSet::new())
                    .is_err(),
                "{name:?}"
            );
        }
        // A full ledger takes no further row.
        let full = Ledger::parse(&format!("{MAGIC}{}", workspaces(MAX_ROWS, 8))).unwrap();
        assert!(full
            .allocate(Kind::Application, "photo", &BTreeSet::new())
            .is_err());
    }

    /// A deployment that rolls back, or one that no longer declares an
    /// installed application, seeds over a ledger holding an allocated row
    /// and keeps it: rows are permanent.
    #[test]
    fn seeding_keeps_allocated_rows() {
        let (allocated, _, _) = seeded()
            .allocate(Kind::Application, "photo", &BTreeSet::new())
            .unwrap();
        assert_eq!(allocated.seed([]).unwrap(), allocated);
        assert_eq!(
            allocated
                .seed([("claude", 65539)])
                .unwrap()
                .uid_of(Kind::Application, "photo"),
            Some(66560)
        );
        // A retained reservation can never claim an allocated uid's name
        // at another uid, nor an allocated uid for another name.
        assert!(allocated.seed([("photo", 65541)]).is_err());
        assert!(allocated.seed([("other", 66560)]).is_err());
    }

    #[test]
    fn uids_stay_inside_their_kinds_range() {
        for uid in ["65536", "98303"] {
            let text = format!("{MAGIC}application\tmail\t{uid}\n");
            assert!(Ledger::parse(&text).is_ok(), "{uid}");
        }
        for uid in [
            "0",
            "1000",
            "65534",
            "65535",
            "98304",
            "2147483647",
            "4294967296",
            "+65536",
            "065536",
            " 65536",
            "-1",
            "",
        ] {
            let text = format!("{MAGIC}application\tmail\t{uid}\n");
            assert!(Ledger::parse(&text).is_err(), "{uid:?}");
        }
    }

    #[test]
    fn noncanonical_ledgers_are_refused() {
        for altered in [
            String::new(),
            "td-accounts-v1".to_string(),
            SEEDED.replacen("v1", "v2", 1),
            SEEDED.trim_end().to_string(),
            SEEDED.replace('\n', "\r\n"),
            format!("{SEEDED}\n"),
            SEEDED.replace("65540", "65540\textra"),
            SEEDED.replace("\tweb", ""),
            SEEDED.replace("application\tweb", "service\tweb"),
            SEEDED.replace("application\tweb", "Application\tweb"),
            // Unsorted, duplicate name, duplicate uid.
            SEEDED.replace("firefox\t65536", "zebra\t65536"),
            SEEDED.replace("news\t65538", "mail\t65538"),
            SEEDED.replace("news\t65538", "news\t65537"),
        ] {
            assert!(Ledger::parse(&altered).is_err(), "{altered:?}");
        }
        for name in [
            "",
            ".mail",
            "-mail",
            "1mail",
            "Mail",
            "mail/name",
            "mail name",
            "máil",
            "mail\0",
            &"a".repeat(65),
        ] {
            let text = SEEDED.replace("mail", name);
            assert!(Ledger::parse(&text).is_err(), "{name:?}");
        }
        assert!(Ledger::parse(&SEEDED.replace("mail", &"m".repeat(64))).is_ok());
    }
}
