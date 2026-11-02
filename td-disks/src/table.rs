//! The machine's extra-volume table: `/var/lib/td/volumes/table`.

use crate::setting::{name, Change, Kind, Mode, Uuid};

pub const HEADER: &str = "td-volumes-v1";
pub const MAX_ROWS: usize = 16;
pub const MAX_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub name: String,
    pub uuid: Uuid,
    pub kind: Kind,
    pub mode: Mode,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Table {
    pub rows: Vec<Row>,
}

impl Table {
    /// The whole file, refused on any line it does not write itself.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES {
            return Err("volume table is too large".into());
        }
        let mut lines = text.split_terminator('\n');
        if lines.next() != Some(HEADER) || !text.ends_with('\n') {
            return Err("volume table has no td-volumes-v1 header".into());
        }
        let mut table = Self::default();
        for line in lines {
            let words: Vec<&str> = line.split(' ').collect();
            let [volume, uuid, kind, mode] = words.as_slice() else {
                return Err(format!("malformed volume row {line:?}"));
            };
            if !name(volume) {
                return Err(format!("invalid volume name {volume:?}"));
            }
            table.insert(Row {
                name: (*volume).into(),
                uuid: Uuid::parse(uuid)?,
                kind: Kind::parse(kind)?,
                mode: Mode::parse(mode)?,
            })?;
        }
        Ok(table)
    }

    pub fn format(&self) -> String {
        let mut text = format!("{HEADER}\n");
        for row in &self.rows {
            text.push_str(&format!(
                "{} {} {} {}\n",
                row.name,
                row.uuid,
                row.kind.word(),
                row.mode.word()
            ));
        }
        text
    }

    pub fn get(&self, volume: &str) -> Option<&Row> {
        self.rows.iter().find(|row| row.name == volume)
    }

    fn insert(&mut self, row: Row) -> Result<(), String> {
        if self.rows.len() >= MAX_ROWS {
            return Err(format!("at most {MAX_ROWS} extra volumes"));
        }
        if self.get(&row.name).is_some() {
            return Err(format!("volume {} is already named", row.name));
        }
        if self.rows.iter().any(|other| other.uuid == row.uuid) {
            return Err(format!("filesystem {} is already a volume", row.uuid));
        }
        self.rows.push(row);
        Ok(())
    }

    /// The table after `change`, or why it cannot apply.
    pub fn changed(&self, change: &Change) -> Result<Self, String> {
        let mut next = self.clone();
        match change {
            Change::Add {
                name,
                uuid,
                kind,
                mode,
            } => next.insert(Row {
                name: name.clone(),
                uuid: *uuid,
                kind: *kind,
                mode: *mode,
            })?,
            Change::Remove { name } => {
                let before = next.rows.len();
                next.rows.retain(|row| &row.name != name);
                if next.rows.len() == before {
                    return Err(format!("no volume is named {name}"));
                }
            }
        }
        Ok(next)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10";
    const B: &str = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";

    #[test]
    fn table_round_trips_and_changes() {
        let empty = Table::parse("td-volumes-v1\n").unwrap();
        assert!(empty.rows.is_empty());
        let add = Change::decode(&format!("add backup {A} btrfs data")).unwrap();
        let one = empty.changed(&add).unwrap();
        let text = one.format();
        assert_eq!(text, format!("td-volumes-v1\nbackup {A} btrfs data\n"));
        assert_eq!(Table::parse(&text).unwrap(), one);
        assert!(one.changed(&add).is_err(), "same name twice");
        let same_fs = Change::decode(&format!("add other {A} btrfs data")).unwrap();
        assert!(one.changed(&same_fs).is_err(), "same filesystem twice");
        let two = one
            .changed(&Change::decode(&format!("add old {B} ext4 exec")).unwrap())
            .unwrap();
        let removed = two
            .changed(&Change::decode("remove backup").unwrap())
            .unwrap();
        assert_eq!(removed.rows.len(), 1);
        assert_eq!(removed.rows[0].name, "old");
        assert!(removed
            .changed(&Change::decode("remove backup").unwrap())
            .is_err());
    }

    #[test]
    fn table_refuses_what_it_did_not_write() {
        for text in [
            "",
            "td-volumes-v1",
            "td-volumes-v2\n",
            &format!("td-volumes-v1\nbackup {A} btrfs data"),
            &format!("td-volumes-v1\nbackup {A} btrfs data extra\n"),
            &format!("td-volumes-v1\n../x {A} btrfs data\n"),
            &format!("td-volumes-v1\nbackup {A} vfat data\n"),
            &format!("td-volumes-v1\nbackup {A} btrfs data\nbackup {B} ext4 data\n"),
            &format!("td-volumes-v1\nbackup {A} btrfs data\n\n"),
        ] {
            assert!(Table::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn table_holds_at_most_sixteen() {
        let mut table = Table::default();
        for index in 0..MAX_ROWS {
            let uuid = format!("00000000-0000-0000-0000-{:012x}", index + 1);
            table = table
                .changed(&Change::decode(&format!("add v{index} {uuid} ext4 data")).unwrap())
                .unwrap();
        }
        let extra = Change::decode(&format!("add more {A} ext4 data")).unwrap();
        assert!(table.changed(&extra).is_err());
        assert!(table.format().len() <= MAX_BYTES);
    }
}
