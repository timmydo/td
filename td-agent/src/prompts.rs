//! The prompts the person may replace (DESIGN.md §13, "The person's
//! prompts"): the system prompt, the reasoning stage's two policies and
//! Jev's four questions. Each replaced one is a file of its own under the
//! state directory's `prompts`, read as a request is made, so an edit holds
//! from the next one; no file means the shipped text. A file that cannot be
//! taken is an error for whoever reads it, never the shipped text in its
//! place: the person chose that policy.

use std::path::Path;

/// The directory under the state directory.
pub const DIR: &str = "prompts";
/// The most a prompt may be.
pub const MAX_BYTES: usize = 64 * 1024;

/// One prompt the person may replace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Name {
    System,
    Classifier,
    ClassifierUnconfined,
    JevRequest,
    JevDiscloses,
    JevRequestUnconfined,
    JevDisclosesUnconfined,
}

/// Every one, in the order the window lists them.
pub const NAMES: &[Name] = &[
    Name::System,
    Name::Classifier,
    Name::ClassifierUnconfined,
    Name::JevRequest,
    Name::JevDiscloses,
    Name::JevRequestUnconfined,
    Name::JevDisclosesUnconfined,
];

impl Name {
    /// Its file's name, and its name on the wire.
    pub fn word(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Classifier => "classifier",
            Self::ClassifierUnconfined => "classifier-unconfined",
            Self::JevRequest => "jev-request",
            Self::JevDiscloses => "jev-discloses",
            Self::JevRequestUnconfined => "jev-request-unconfined",
            Self::JevDisclosesUnconfined => "jev-discloses-unconfined",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        NAMES.iter().copied().find(|name| name.word() == word)
    }

    /// As the window lists it.
    pub fn title(self) -> &'static str {
        match self {
            Self::System => "System prompt",
            Self::Classifier => "Classifier policy",
            Self::ClassifierUnconfined => "Classifier policy, unconfined",
            Self::JevRequest => "Jev: does the person ask for it",
            Self::JevDiscloses => "Jev: does it disclose",
            Self::JevRequestUnconfined => "Jev: does the person ask for it, unconfined",
            Self::JevDisclosesUnconfined => "Jev: does it disclose, unconfined",
        }
    }

    pub fn shipped(self) -> &'static str {
        match self {
            Self::System => crate::prompt::CONVERSATION,
            Self::Classifier => crate::classifier::POLICY,
            Self::ClassifierUnconfined => crate::classifier::UNCONFINED_POLICY,
            Self::JevRequest => include_str!("../prompt/jev-request.txt"),
            Self::JevDiscloses => include_str!("../prompt/jev-discloses.txt"),
            Self::JevRequestUnconfined => include_str!("../prompt/jev-request-unconfined.txt"),
            Self::JevDisclosesUnconfined => include_str!("../prompt/jev-discloses-unconfined.txt"),
        }
    }

    /// What the window says of its form above the text.
    pub fn form(self) -> &'static str {
        match self {
            Self::System => {
                "Sent first in every conversation's requests; {tools} is where the paragraph on its workspace goes."
            }
            Self::Classifier | Self::ClassifierUnconfined => {
                "The reasoning stage's system message: its reply must stay the JSON the text asks for."
            }
            Self::JevRequest | Self::JevRequestUnconfined => {
                "Lines beginning instructions:, matches:, exceeds: and unrelated:, each once; another line continues the one above."
            }
            Self::JevDiscloses | Self::JevDisclosesUnconfined => {
                "Lines beginning instructions:, true: and false:, each once; another line continues the one above."
            }
        }
    }

    /// A Jev question's options, the keys its answers are read by.
    fn options(self) -> &'static [&'static str] {
        match self {
            Self::JevRequest | Self::JevRequestUnconfined => &["matches", "exceeds", "unrelated"],
            Self::JevDiscloses | Self::JevDisclosesUnconfined => &["true", "false"],
            Self::System | Self::Classifier | Self::ClassifierUnconfined => &[],
        }
    }
}

/// A Jev question as its body asks it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Question {
    pub instructions: String,
    pub criteria: Vec<(&'static str, String)>,
}

fn question(name: Name, text: &str) -> Result<Question, String> {
    const INSTRUCTIONS: &str = "instructions";
    let options = name.options();
    let mut fields: Vec<(&'static str, String)> = Vec::new();
    for line in text.lines() {
        let begun = std::iter::once(&INSTRUCTIONS)
            .chain(options)
            .find(|key| line.strip_prefix(**key).is_some_and(|r| r.starts_with(':')));
        match (begun, fields.last_mut()) {
            (Some(key), _) => {
                if fields.iter().any(|(have, _)| have == key) {
                    return Err(format!("`{key}:` begins more than one line"));
                }
                let value = line.get(key.len() + 1..).unwrap_or_default().trim_start();
                fields.push((key, value.to_string()));
            }
            (None, Some((_, value))) => {
                value.push('\n');
                value.push_str(line);
            }
            (None, None) if line.trim().is_empty() => {}
            (None, None) => return Err("the first line must begin `instructions:`".to_string()),
        }
    }
    let mut take = |key: &str| {
        let at = fields.iter().position(|(have, _)| *have == key);
        let value = at.map(|at| fields.remove(at).1);
        match value.as_deref().map(str::trim) {
            Some(value) if !value.is_empty() => Ok(value.to_string()),
            _ => Err(format!("`{key}:` must begin a line and say something")),
        }
    };
    let instructions = take(INSTRUCTIONS)?;
    let mut criteria = Vec::new();
    for option in options {
        criteria.push((*option, take(option)?));
    }
    Ok(Question {
        instructions,
        criteria,
    })
}

/// Why `text` cannot be `name`, if it cannot.
pub fn check(name: Name, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(
            "it is empty; Shipped text in File > Prompts\u{2026} brings back what td-agent ships"
                .to_string(),
        );
    }
    if text.len() > MAX_BYTES {
        return Err(format!("it is past {MAX_BYTES} bytes"));
    }
    // The editor takes no others, and a text it cannot open it cannot repair.
    if text
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{feff}')
    {
        return Err("it holds a control character other than a tab or a new line".to_string());
    }
    if !name.options().is_empty() {
        question(name, text)?;
    }
    Ok(())
}

/// The prompts in force: the person's, the shipped text for the rest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Prompts {
    custom: Vec<(Name, String)>,
}

impl Prompts {
    /// Those under the state directory `root`, refused when one is there
    /// that cannot be taken.
    pub fn load(root: &Path) -> Result<Self, String> {
        let mut custom = Vec::new();
        for name in NAMES {
            if let Some(text) = read(root, *name)? {
                check(*name, &text).map_err(|why| file_trouble(root, *name, &why))?;
                custom.push((*name, text));
            }
        }
        Ok(Self { custom })
    }

    fn own(&self, name: Name) -> Option<&str> {
        self.custom
            .iter()
            .find(|(have, _)| *have == name)
            .map(|(_, text)| text.as_str())
    }

    /// What `name` says.
    pub fn get(&self, name: Name) -> &str {
        self.own(name).unwrap_or_else(|| name.shipped())
    }

    /// Jev's question `name`. A loaded text was checked and a shipped one is
    /// held to its form by a test, so neither fails here.
    pub fn question(&self, name: Name) -> Question {
        question(name, self.get(name)).unwrap_or_default()
    }

    /// Each one as the window edits it.
    pub fn entries(&self) -> Vec<Entry> {
        NAMES
            .iter()
            .map(|name| Entry {
                name: *name,
                text: self.get(*name).to_string(),
                custom: self.own(*name).is_some(),
                trouble: None,
            })
            .collect()
    }
}

fn file_trouble(root: &Path, name: Name, why: &str) -> String {
    format!("{}: {why}", root.join(DIR).join(name.word()).display())
}

/// The file of `name` under the state directory `root`, if there is one.
fn read(root: &Path, name: Name) -> Result<Option<String>, String> {
    let bytes = match td_fs::read_bounded(&root.join(DIR).join(name.word()), MAX_BYTES as u64) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        read => read.map_err(|e| file_trouble(root, name, &e.to_string()))?,
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| file_trouble(root, name, "it is not UTF-8"))
}

/// Each one under the state directory `root` as the window shows it, file by
/// file, so one that cannot be taken hides no other and can be replaced.
pub fn listing(root: &Path) -> Vec<Entry> {
    NAMES
        .iter()
        .map(|name| {
            let shipped = |custom, trouble| Entry {
                name: *name,
                text: name.shipped().to_string(),
                custom,
                trouble,
            };
            match read(root, *name) {
                Ok(None) => shipped(false, None),
                Err(why) => shipped(true, Some(why)),
                Ok(Some(text)) => {
                    let trouble = check(*name, &text)
                        .err()
                        .map(|why| file_trouble(root, *name, &why));
                    Entry {
                        name: *name,
                        text,
                        custom: true,
                        trouble,
                    }
                }
            }
        })
        .collect()
}

/// One prompt as the window shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub name: Name,
    pub text: String,
    pub custom: bool,
    /// Why its file cannot be taken, which stops turns until it is replaced.
    pub trouble: Option<String>,
}

/// Replaces `name` under the state directory `root` with `text`, or gives it
/// back to the shipped text: with none, or `text` being it.
pub fn save(root: &Path, name: Name, text: Option<&str>) -> Result<(), String> {
    let dir = root.join(DIR);
    let Some(text) = text.filter(|text| *text != name.shipped()) else {
        return match std::fs::remove_file(dir.join(name.word())) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    };
    check(name, text)?;
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    crate::store::replace(&dir, name.word(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn what_ships_takes_its_own_form() {
        for name in NAMES {
            check(*name, name.shipped()).unwrap();
            assert_eq!(Name::from_word(name.word()), Some(*name));
            if !name.options().is_empty() {
                let shipped = question(*name, name.shipped()).unwrap();
                assert!(!shipped.instructions.is_empty());
                assert_eq!(shipped.criteria.len(), name.options().len());
            }
        }
    }

    #[test]
    fn a_question_has_each_of_its_keys_once() {
        let name = Name::JevDiscloses;
        let asked = question(
            name,
            "instructions: Say.\nand more\ntrue: It leaves: all of it.\nfalse: It stays.\n",
        )
        .unwrap();
        assert_eq!(asked.instructions, "Say.\nand more");
        assert_eq!(
            asked.criteria,
            [
                ("true", "It leaves: all of it.".to_string()),
                ("false", "It stays.".to_string())
            ]
        );
        for wrong in [
            "",
            "true: a\nfalse: b",
            "instructions: a\ntrue: b",
            "instructions: a\ntrue:\nfalse: c",
            "instructions: a\ntrue: b\ntrue: c\nfalse: d",
            "first\ninstructions: a\ntrue: b\nfalse: c",
        ] {
            assert!(check(name, wrong).is_err(), "{wrong}");
        }
        // A key of another question continues the line above.
        assert!(check(
            Name::JevRequest,
            "instructions: a\nmatches: b\nexceeds: c\nunrelated: d\ntrue: e"
        )
        .is_ok());
    }

    #[test]
    fn a_saved_prompt_is_in_force_until_given_back() {
        let owned = td_fs::TempDir::new("prompts").unwrap();
        let root = owned.path();
        assert_eq!(Prompts::load(root).unwrap(), Prompts::default());
        save(root, Name::Classifier, Some("mine")).unwrap();
        let prompts = Prompts::load(root).unwrap();
        assert_eq!(prompts.get(Name::Classifier), "mine");
        assert_eq!(prompts.get(Name::System), Name::System.shipped());
        let entries = prompts.entries();
        assert_eq!(entries.len(), NAMES.len());
        assert!(entries
            .iter()
            .all(|e| e.custom == (e.name == Name::Classifier)));
        // The shipped text saved is no replacement.
        save(root, Name::Classifier, Some(Name::Classifier.shipped())).unwrap();
        assert_eq!(Prompts::load(root).unwrap(), Prompts::default());
        save(root, Name::Classifier, Some("mine")).unwrap();
        save(root, Name::Classifier, None).unwrap();
        assert_eq!(Prompts::load(root).unwrap(), Prompts::default());
        save(root, Name::Classifier, None).unwrap();
        // Refused, what is there stays.
        save(root, Name::Classifier, Some("mine")).unwrap();
        assert!(save(root, Name::Classifier, Some(" \n")).is_err());
        assert!(save(root, Name::JevRequest, Some("no form")).is_err());
        assert!(save(root, Name::System, Some(&"x".repeat(MAX_BYTES + 1))).is_err());
        assert_eq!(Prompts::load(root).unwrap().get(Name::Classifier), "mine");
    }

    #[test]
    fn a_file_that_cannot_be_taken_is_refused_not_passed_over() {
        let owned = td_fs::TempDir::new("prompts-bad").unwrap();
        let root = owned.path();
        std::fs::create_dir(root.join(DIR)).unwrap();
        let file = root.join(DIR).join(Name::Classifier.word());
        save(root, Name::System, Some("mine")).unwrap();
        for bad in [
            &b""[..],
            &b"\xff"[..],
            &b"a\x1bb"[..],
            "\u{feff}a".as_bytes(),
            &b"a\rb"[..],
            &[b'x'; MAX_BYTES + 1][..],
        ] {
            std::fs::write(&file, bad).unwrap();
            assert!(Prompts::load(root).is_err());
            // The rest show as they are; this one says why, and goes on being
            // replaceable.
            let listed = listing(root);
            let this = listed.iter().find(|e| e.name == Name::Classifier).unwrap();
            assert!(this.custom && this.trouble.is_some(), "{this:?}");
            let system = listed.iter().find(|e| e.name == Name::System).unwrap();
            assert_eq!(
                (system.text.as_str(), system.trouble.as_ref()),
                ("mine", None)
            );
        }
        // A form it does not keep is shown to be mended.
        std::fs::write(root.join(DIR).join("jev-request"), "no form").unwrap();
        let listed = listing(root);
        let this = listed.iter().find(|e| e.name == Name::JevRequest).unwrap();
        assert_eq!(this.text, "no form");
        std::fs::remove_file(root.join(DIR).join("jev-request")).unwrap();
        // Saving the shipped text takes the bad file away.
        save(root, Name::Classifier, Some(Name::Classifier.shipped())).unwrap();
        assert_eq!(Prompts::load(root).unwrap().get(Name::System), "mine");
    }
}
