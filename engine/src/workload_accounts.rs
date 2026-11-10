//! The workload account ledger (td-authd/DESIGN.md, "Workload account
//! ledger"; WORKLOADS.md, "Accounts"): the uid each workload account holds,
//! keyed by kind and name. Rows are never removed, so no uid is reused,
//! after an application's removal included. Parsing and seeding perform no
//! I/O and use no other module, so target programs can `#[path]`-include
//! this file; callers establish file authority.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

const MAGIC: &str = "td-accounts-v1\n";
/// The ledger's name in td's persistent state directory, `/var/lib/td`.
pub const FILE_NAME: &str = "accounts.tsv";
/// Rows are permanent, so the v1 bounds hold every row a later deployment
/// may add, or rolling back to this one would refuse its ledger: 4096 rows
/// at a 64-byte average.
pub const MAX_BYTES: usize = 256 * 1024;
const MAX_ROWS: usize = 4096;
/// Application accounts.
pub const APPLICATION_UIDS: RangeInclusive<u32> = 65536..=98303;
/// Sandbox and workspace accounts, which share one range. Nothing writes
/// those kinds yet; v1 parses them so a deployment that does can roll back.
pub const WORKLOAD_UIDS: RangeInclusive<u32> = 98304..=131071;

/// The uids the image reserved before the ledger existed. Every seeded
/// ledger holds them, whether or not the deployment ships the application.
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
    fn parse(text: &str) -> Option<Self> {
        match text {
            "application" => Some(Self::Application),
            "sandbox" => Some(Self::Sandbox),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
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
/// The engine crate's tests hold the two equal.
fn valid_name(value: &str) -> bool {
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

    pub fn uid(&self, kind: Kind, name: &str) -> Option<u32> {
        self.rows.get(&(kind, name.to_string())).copied()
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

    fn insert(&mut self, kind: Kind, name: &str, uid: u32) -> Result<(), String> {
        if !valid_name(name) || !kind.uids().contains(&uid) {
            return Err(format!(
                "account {name:?} uid {uid} is outside the account ledger's rules"
            ));
        }
        match self.uid(kind, name) {
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
        assert_eq!(ledger.uid(Kind::Application, "web"), Some(65540));
        assert_eq!(ledger.uid(Kind::Application, "claude"), None);
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
        assert_eq!(ledger.uid(Kind::Sandbox, "default"), Some(98304));
        assert_eq!(ledger.uid(Kind::Workspace, "agent-1"), Some(131071));
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
        assert_eq!(imported.uid(Kind::Application, "claude"), Some(65539));
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
