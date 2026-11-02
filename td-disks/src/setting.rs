//! Bounded extra-volume changes shared with the secure-attention description.
//!
//! td-authd compiles this file into its consent description, so it stays
//! std-only and names nothing outside itself.

pub const MAX_NAME: usize = 16;
/// The settings intake carries at most 79 payload bytes, `volume ` among
/// them (td-authd/DESIGN.md); the longest add is 68.
pub const MAX_ENCODED: usize = 79 - "volume ".len();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Btrfs,
    Ext4,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "btrfs" => Ok(Self::Btrfs),
            "ext4" => Ok(Self::Ext4),
            _ => Err("filesystem must be btrfs or ext4".into()),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Btrfs => "btrfs",
            Self::Ext4 => "ext4",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// nodev,nosuid,noexec: nothing on the volume runs.
    Data,
    /// nodev,nosuid: programs on the volume may run, without privilege.
    Exec,
}

impl Mode {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "data" => Ok(Self::Data),
            "exec" => Ok(Self::Exec),
            _ => Err("mode must be data or exec".into()),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Exec => "exec",
        }
    }

    pub fn flags(self) -> &'static str {
        match self {
            Self::Data => "nodev,nosuid,noexec",
            Self::Exec => "nodev,nosuid",
        }
    }
}

/// A filesystem UUID, held as its 16 bytes and written in canonical
/// lowercase 8-4-4-4-12 form.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    pub fn parse(text: &str) -> Result<Self, String> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return Err("UUID must be 8-4-4-4-12 hexadecimal".into());
        }
        let mut out = [0u8; 16];
        let mut nibbles = 0usize;
        for (index, byte) in bytes.iter().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if *byte != b'-' {
                    return Err("UUID must be 8-4-4-4-12 hexadecimal".into());
                }
                continue;
            }
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => return Err("UUID must be lowercase hexadecimal".into()),
            };
            let slot = out
                .get_mut(nibbles / 2)
                .ok_or("UUID must be 8-4-4-4-12 hexadecimal")?;
            *slot = (*slot << 4) | value;
            nibbles += 1;
        }
        if out == [0; 16] {
            return Err("the nil UUID names no filesystem".into());
        }
        Ok(Self(out))
    }
}

impl std::fmt::Display for Uuid {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                out.write_str("-")?;
            }
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A volume name: its mount point under `/mnt`.
pub fn name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_NAME
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    Add {
        name: String,
        uuid: Uuid,
        kind: Kind,
        mode: Mode,
    },
    Remove {
        name: String,
    },
}

impl Change {
    /// The words after `volume`: `add NAME UUID KIND MODE` or `remove NAME`.
    pub fn parse(words: &[&str]) -> Result<Self, String> {
        match words {
            ["add", volume, uuid, kind, mode] if name(volume) => Ok(Self::Add {
                name: (*volume).into(),
                uuid: Uuid::parse(uuid)?,
                kind: Kind::parse(kind)?,
                mode: Mode::parse(mode)?,
            }),
            ["remove", volume] if name(volume) => Ok(Self::Remove {
                name: (*volume).into(),
            }),
            _ => Err(
                "expected add NAME UUID btrfs|ext4 data|exec, or remove NAME; \
                 NAME is 1-16 of a-z, 0-9 and -"
                    .into(),
            ),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Add { name, .. } | Self::Remove { name } => name,
        }
    }

    pub fn words(&self) -> Vec<String> {
        match self {
            Self::Add {
                name,
                uuid,
                kind,
                mode,
            } => vec![
                "add".into(),
                name.clone(),
                uuid.to_string(),
                kind.word().into(),
                mode.word().into(),
            ],
            Self::Remove { name } => vec!["remove".into(), name.clone()],
        }
    }

    pub fn encode(&self) -> String {
        self.words().join(" ")
    }

    pub fn decode(text: &str) -> Result<Self, String> {
        if text.len() > MAX_ENCODED {
            return Err("volume change is too long".into());
        }
        let words: Vec<&str> = text.split(' ').collect();
        let change = Self::parse(&words)?;
        if change.encode() != text {
            return Err("volume change is not canonical".into());
        }
        Ok(change)
    }

    /// What the secure-attention review shows, in the renderer's glyphs.
    /// Rows starting with an `ARGUMENTS` label may wrap; every other row
    /// fits the prompt's 34 columns.
    pub fn describe(&self) -> Vec<String> {
        match self {
            Self::Add {
                name,
                uuid,
                kind,
                mode,
            } => vec![
                format!("VOLUME: {name}"),
                format!("UUID: {uuid}"),
                format!(
                    "MOUNTS {} AT /MNT AT EVERY BOOT",
                    kind.word().to_uppercase()
                ),
                match mode {
                    Mode::Data => "NO PROGRAMS; NO SETUID; NO DEVICES".into(),
                    Mode::Exec => "PROGRAMS RUN; NO SETUID OR DEVICES".into(),
                },
                "FILES KEEP THEIR DISK OWNERS".into(),
                "ANY DISK WITH THIS UUID MOUNTS".into(),
            ],
            Self::Remove { name } => vec![
                format!("VOLUME: {name}"),
                "NO LONGER MOUNTED AT BOOT".into(),
                "UNMOUNTED NOW; REFUSED IF IN USE".into(),
            ],
        }
    }
}

/// Description rows whose text after the label is an argument.
pub const ARGUMENTS: &[&str] = &["VOLUME: ", "UUID: "];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn changes_round_trip_canonically() {
        for text in [
            "add backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "add old-root 00000000-0000-0000-0000-000000000001 ext4 exec",
            "remove backup",
        ] {
            assert_eq!(Change::decode(text).unwrap().encode(), text);
        }
    }

    #[test]
    fn refuses_paths_injection_and_noncanonical_text() {
        for text in [
            "add ../etc 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "add a/b 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "add -x 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "add Backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "add backup 3F1C2A4E-9B7D-4C21-8E55-0D6A7B8C9E10 btrfs data",
            "add backup 3f1c2a4e9b7d4c218e550d6a7b8c9e10 btrfs data",
            "add backup 00000000-0000-0000-0000-000000000000 btrfs data",
            "add backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 vfat data",
            "add backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs suid",
            "add backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data ",
            "add  backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "remove backup\n",
            "remove",
            "remove a b",
            "",
        ] {
            assert!(Change::decode(text).is_err(), "{text:?}");
        }
        let long = format!("remove {}", "a".repeat(MAX_NAME + 1));
        assert!(Change::decode(&long).is_err());
        let longest = format!("remove {}", "a".repeat(MAX_NAME));
        assert!(Change::decode(&longest).is_ok());
        // The longest change still fits the intake's 79-byte payload.
        let add = format!(
            "add {} 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data",
            "a".repeat(MAX_NAME)
        );
        assert!(Change::decode(&add).is_ok());
        assert!(format!("volume {add}").len() <= 79);
    }

    #[test]
    fn uuid_display_is_the_parsed_text() {
        let text = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
        assert_eq!(Uuid::parse(text).unwrap().to_string(), text);
    }

    #[test]
    fn descriptions_stay_in_the_renderers_glyphs() {
        let allowed = |c: char| c.is_ascii_alphanumeric() || " :;-+/_.?".contains(c);
        for change in [
            Change::decode("add backup 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 btrfs data").unwrap(),
            Change::decode("add x 3f1c2a4e-9b7d-4c21-8e55-0d6a7b8c9e10 ext4 exec").unwrap(),
            Change::decode("remove backup").unwrap(),
        ] {
            let lines = change.describe();
            let arguments = lines
                .iter()
                .filter(|line| ARGUMENTS.iter().any(|label| line.starts_with(label)))
                .count();
            let expected = match change {
                Change::Add { .. } => 2,
                Change::Remove { .. } => 1,
            };
            assert_eq!(arguments, expected);
            for line in lines {
                assert!(line.chars().all(allowed), "{line}");
                let argument = ARGUMENTS.iter().any(|label| line.starts_with(label));
                assert!(argument || line.len() <= 34, "{line}");
            }
        }
    }
}
