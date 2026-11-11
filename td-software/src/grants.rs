//! The folders of the human's an application is granted, as the
//! compositor's control socket answers `grants`, `grant` and `revoke`
//! (td-compositor/DESIGN.md §15, "Grant requests"), read as untrusted
//! bounded text. Pure.

/// The most of a listing td-software reads: every directory row an
/// account holds at the longest path, with room.
pub const MAX_ANSWER_BYTES: usize = 40 * 1024;
/// The table's bound on a directory's bytes.
pub const MAX_PATH_BYTES: usize = 1024;

/// A grant's access: what the application may do in the folder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

impl Access {
    /// The control socket's word.
    pub fn word(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    /// The word a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadWrite => "writable",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "ro" => Some(Self::ReadOnly),
            "rw" => Some(Self::ReadWrite),
            _ => None,
        }
    }
}

/// One folder an application is granted: below the human's home.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub access: Access,
    pub path: String,
}

impl Grant {
    /// How a person reads it: the home's `~/`, then the directory.
    pub fn shown(&self) -> String {
        shown(&self.path)
    }
}

/// `path` below the home as td-software shows it, any control character
/// or one that reorders or hides text shown as `?`. Root's validator
/// refuses every one of those before a row is written; this keeps what a
/// line shows the directory it names whatever arrives.
pub fn shown(path: &str) -> String {
    let mut text = String::from("~/");
    text.extend(path.chars().map(|c| {
        let hidden = c.is_control()
            || matches!(
                c,
                '\u{061C}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{FEFF}'
            );
        if hidden {
            '?'
        } else {
            c
        }
    }));
    text
}

/// A listing's body: one `ro|rw<TAB>DIRECTORY` line per grant, at most 32.
pub fn listing(body: &str) -> Result<Vec<Grant>, String> {
    let malformed = || "the compositor's listing of grants is malformed".to_string();
    if !(body.is_empty() || body.ends_with('\n')) {
        return Err(malformed());
    }
    let mut grants = Vec::new();
    for line in body.split_terminator('\n') {
        let (access, path) = line.split_once('\t').ok_or_else(malformed)?;
        let access = Access::parse(access).ok_or_else(malformed)?;
        if path.is_empty() || path.len() > MAX_PATH_BYTES || grants.len() >= 32 {
            return Err(malformed());
        }
        grants.push(Grant {
            access,
            path: path.to_string(),
        });
    }
    Ok(grants)
}

/// What a change did, as its body's one word says: `added`, `changed` or
/// `unchanged` for a grant, `revoked` for a revoke.
pub fn change(body: &str, revoke: bool) -> Result<&'static str, String> {
    let words: &[&'static str] = if revoke {
        &["revoked"]
    } else {
        &["added", "changed", "unchanged"]
    };
    words
        .iter()
        .copied()
        .find(|word| body.strip_suffix('\n') == Some(*word))
        .ok_or_else(|| "the compositor's answer to a grant is malformed".into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_listing_is_one_tabbed_line_per_grant() {
        assert_eq!(listing("").unwrap(), []);
        assert_eq!(
            listing("ro\tMy Pictures\nrw\tDownloads\n").unwrap(),
            [
                Grant {
                    access: Access::ReadOnly,
                    path: "My Pictures".into()
                },
                Grant {
                    access: Access::ReadWrite,
                    path: "Downloads".into()
                }
            ]
        );
        let full: String = (0..33).map(|index| format!("ro\td{index}\n")).collect();
        for bad in ["ro\tx", "rx\tx\n", "ro\t\n", "ro x\n", "\n", full.as_str()] {
            assert!(listing(bad).is_err(), "{bad:?}");
        }
        assert_eq!(change("revoked\n", true), Ok("revoked"));
        assert_eq!(change("unchanged\n", false), Ok("unchanged"));
        assert!(change("revoked", true).is_err());
        assert!(change("revoked\n", false).is_err());
        assert!(change("added\n", true).is_err());
        assert!(change("deleted\n", false).is_err());
        assert_eq!(shown("a\u{7}b/c"), "~/a?b/c");
        assert_eq!(shown("Down\u{202E}loads\u{2066}"), "~/Down?loads?");
        assert_eq!(shown("Téléchargements"), "~/Téléchargements");
    }
}
