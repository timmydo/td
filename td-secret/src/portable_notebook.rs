//! Entry operations over an unlocked session. Each change, or each batch
//! of changes, is one revision-checked save of the whole notebook under
//! the session's unlock.

use super::lifecycle::{Directory, Error, Session};
use super::{Entry, Notebook, MAX_BODY, MAX_ENTRIES, MAX_PLAIN, MAX_TITLE};
use std::io::Read;

pub(super) type EntryId = [u8; 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryError {
    Missing,
    /// The entry changed after the caller read `base`; nothing was saved.
    Stale,
    Title,
    Duplicate,
    Body,
    Full,
}

pub(super) struct Summary<'a> {
    pub id: EntryId,
    pub revision: u64,
    pub title: &'a str,
}

/// Caller text handed to a change. Cleared on drop, so a refused change
/// leaves no copy behind; neither Debug nor Clone.
pub(super) struct Text(String);

impl Text {
    pub fn new(text: String) -> Self {
        Self(text)
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn take(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        // Keeps the clearing from being removed as a dead store.
        std::hint::black_box(&mut bytes);
    }
}

/// A change against the entry revision the caller last read.
pub(super) enum Change {
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

/// The identity and revision a successful change committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Committed {
    pub id: EntryId,
    pub revision: Option<u64>,
}

fn title_error(entries: &[Entry], skip: Option<usize>, title: &str) -> Option<EntryError> {
    if title.is_empty() || title.len() > MAX_TITLE || title.chars().any(char::is_control) {
        return Some(EntryError::Title);
    }
    entries
        .iter()
        .enumerate()
        .any(|(index, entry)| Some(index) != skip && entry.title() == title)
        .then_some(EntryError::Duplicate)
}

fn plaintext_size(entries: &[Entry]) -> Option<usize> {
    entries.iter().try_fold(4usize, |size, entry| {
        size.checked_add(32 + entry.title().len() + entry.body().len())
    })
}

impl Session {
    /// Entries in stored order; titles and bodies remain session plaintext.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = Summary<'_>> {
        self.notebook().entries.iter().map(|entry| Summary {
            id: entry.id,
            revision: entry.revision,
            title: entry.title(),
        })
    }

    pub fn entry(&self, id: &EntryId) -> Option<&Entry> {
        self.notebook().entries.iter().find(|entry| &entry.id == id)
    }

    /// Builds the next notebook, then saves it under the session's unlock;
    /// `revoked` is the owner's lock, checked before publication.
    pub fn apply(
        &mut self,
        directory: &Directory,
        change: Change,
        revoked: &dyn Fn() -> bool,
        random: &mut impl Read,
    ) -> Result<Committed, Error> {
        self.apply_all(directory, vec![change], revoked, random)?
            .pop()
            .ok_or_else(|| Error::Refused("portable change committed nothing".into()))
    }

    /// Builds the next notebook from `changes`, each against the entries
    /// the ones before it left, then saves it once under the session's
    /// unlock: one vault revision, or nothing when any change or the save
    /// is refused, or `revoked` comes, before publication; a save
    /// uncertain after it began is uncertain for the whole batch. An
    /// entry changed twice reports the first change's revision, never
    /// published alone. No changes save nothing.
    pub fn apply_all(
        &mut self,
        directory: &Directory,
        changes: Vec<Change>,
        revoked: &dyn Fn() -> bool,
        random: &mut impl Read,
    ) -> Result<Vec<Committed>, Error> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<Entry> = self
            .notebook()
            .entries
            .iter()
            .map(|entry| {
                Entry::new(
                    entry.id,
                    entry.revision,
                    entry.title().to_owned(),
                    entry.body().to_vec(),
                )
            })
            .collect();
        let mut committed = Vec::with_capacity(changes.len());
        for change in changes {
            committed.push(apply_one(&mut entries, change, random)?);
        }
        if plaintext_size(&entries).is_none_or(|size| size > MAX_PLAIN) {
            return Err(Error::Entry(EntryError::Full));
        }
        self.save(directory, &Notebook { entries }, revoked, random)?;
        Ok(committed)
    }
}

/// Applies one change to the next notebook's `entries`.
fn apply_one(
    entries: &mut Vec<Entry>,
    change: Change,
    random: &mut impl Read,
) -> Result<Committed, Error> {
    let find = |entries: &[Entry], id: &EntryId, base: u64| {
        let index = entries
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or(Error::Entry(EntryError::Missing))?;
        if entries.get(index).map(|entry| entry.revision) != Some(base) {
            return Err(Error::Entry(EntryError::Stale));
        }
        Ok(index)
    };
    Ok(match change {
        Change::Create { title, body } => {
            if entries.len() >= MAX_ENTRIES {
                return Err(Error::Entry(EntryError::Full));
            }
            if let Some(error) = title_error(entries, None, title.as_str()) {
                return Err(Error::Entry(error));
            }
            if body.as_str().len() > MAX_BODY {
                return Err(Error::Entry(EntryError::Body));
            }
            let mut id = [0; 16];
            random
                .read_exact(&mut id)
                .map_err(|_| Error::Refused("portable entropy unavailable".into()))?;
            if entries.iter().any(|entry| entry.id == id) {
                return Err(Error::Refused("portable entry identity collided".into()));
            }
            entries.push(Entry::new(id, 1, title.take(), body.take().into_bytes()));
            Committed {
                id,
                revision: Some(1),
            }
        }
        Change::Edit {
            id,
            base,
            title,
            body,
        } => {
            let index = find(entries, &id, base)?;
            if let Some(error) = title_error(entries, Some(index), title.as_str()) {
                return Err(Error::Entry(error));
            }
            if body.as_str().len() > MAX_BODY {
                return Err(Error::Entry(EntryError::Body));
            }
            let revision = next(base)?;
            let entry = entries
                .get_mut(index)
                .ok_or(Error::Entry(EntryError::Missing))?;
            entry.replace_text(title.take(), body.take().into_bytes());
            entry.revision = revision;
            Committed {
                id,
                revision: Some(revision),
            }
        }
        Change::Rename { id, base, title } => {
            let index = find(entries, &id, base)?;
            if let Some(error) = title_error(entries, Some(index), title.as_str()) {
                return Err(Error::Entry(error));
            }
            let revision = next(base)?;
            let entry = entries
                .get_mut(index)
                .ok_or(Error::Entry(EntryError::Missing))?;
            let body = entry.body().to_vec();
            entry.replace_text(title.take(), body);
            entry.revision = revision;
            Committed {
                id,
                revision: Some(revision),
            }
        }
        Change::Delete { id, base } => {
            let index = find(entries, &id, base)?;
            entries.remove(index);
            Committed { id, revision: None }
        }
    })
}

fn next(base: u64) -> Result<u64, Error> {
    base.checked_add(1)
        .ok_or(Error::Refused("portable entry revision exhausted".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::lifecycle::tests::{created, unlocked, Bench, Place, Random};

    struct Fixture {
        place: Place,
        bench: Bench,
        random: Random,
        session: Session,
    }

    impl Fixture {
        fn new() -> Self {
            let place = Place::new();
            let mut bench = Bench::new(2);
            let mut random = Random(0);
            let session = created(&place, &mut bench, &mut random);
            Self {
                place,
                bench,
                random,
                session,
            }
        }
        fn apply(&mut self, change: Change) -> Result<Committed, Error> {
            let calls = self.bench.calls.len();
            let result = self
                .session
                .apply(&self.place.1, change, &|| false, &mut self.random);
            // A save asks no token, whether it commits or is refused.
            assert_eq!(self.bench.calls.len(), calls);
            result
        }
        fn create(&mut self, title: &str, body: &str) -> Committed {
            self.apply(Change::Create {
                title: title.into(),
                body: body.into(),
            })
            .unwrap()
        }
    }

    #[test]
    fn entry_changes_commit_revisions_that_every_key_reads() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "user: a\npass: b\n");
        let bank = fixture.create("Bank", "pin 1234");
        assert_eq!(mail.revision, Some(1));
        assert_ne!(mail.id, bank.id);
        assert_eq!(fixture.session.revision(), 3);
        let edited = fixture
            .apply(Change::Edit {
                id: mail.id,
                base: 1,
                title: "Mail".into(),
                body: "user: a\r\npass: c".into(),
            })
            .unwrap();
        assert_eq!(edited.revision, Some(2));
        fixture
            .apply(Change::Rename {
                id: bank.id,
                base: 1,
                title: "Bank/Checking".into(),
            })
            .unwrap();
        assert_eq!(fixture.session.entry(&bank.id).unwrap().body(), b"pin 1234");
        let deleted = fixture
            .apply(Change::Delete {
                id: bank.id,
                base: 2,
            })
            .unwrap();
        assert_eq!(deleted.revision, None);
        let Fixture {
            place,
            mut bench,
            session,
            ..
        } = fixture;
        drop(session);
        let other = unlocked(&place, &mut bench, 0);
        let listed: Vec<_> = other
            .entries()
            .map(|entry| (entry.title.to_owned(), entry.revision))
            .collect();
        assert_eq!(listed, [("Mail".to_owned(), 2)]);
        assert_eq!(other.entry(&mail.id).unwrap().body(), b"user: a\r\npass: c");
        assert!(other.entry(&bank.id).is_none());
    }

    #[test]
    fn refused_changes_present_no_token_and_publish_nothing() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "body");
        fixture.create("Bank", "body");
        let before = fixture.place.bytes();
        let calls = fixture.bench.calls.len();
        let big = || Text::new("x".repeat(64 * 1024 + 1));
        let long = || Text::new("t".repeat(513));
        let missing = [9; 16];
        let create = |title: Text, body: Text| Change::Create { title, body };
        let edit = |id, base, title: Text, body: Text| Change::Edit {
            id,
            base,
            title,
            body,
        };
        let rename = |id, base, title: Text| Change::Rename { id, base, title };
        let changes = [
            (create("".into(), "".into()), EntryError::Title),
            (create("a\nb".into(), "".into()), EntryError::Title),
            (create(long(), "".into()), EntryError::Title),
            (create("Bank".into(), "".into()), EntryError::Duplicate),
            (create("Big".into(), big()), EntryError::Body),
            (
                edit(mail.id, 2, "Mail".into(), "x".into()),
                EntryError::Stale,
            ),
            (
                edit(missing, 1, "X".into(), "x".into()),
                EntryError::Missing,
            ),
            (edit(mail.id, 1, "".into(), "x".into()), EntryError::Title),
            (
                edit(mail.id, 1, "Bank".into(), "x".into()),
                EntryError::Duplicate,
            ),
            (edit(mail.id, 1, "Mail".into(), big()), EntryError::Body),
            (rename(mail.id, 2, "M".into()), EntryError::Stale),
            (rename(missing, 1, "M".into()), EntryError::Missing),
            (rename(mail.id, 1, long()), EntryError::Title),
            (rename(mail.id, 1, "Bank".into()), EntryError::Duplicate),
            (
                Change::Delete {
                    id: mail.id,
                    base: 2,
                },
                EntryError::Stale,
            ),
            (
                Change::Delete {
                    id: missing,
                    base: 1,
                },
                EntryError::Missing,
            ),
        ];
        for (change, error) in changes {
            assert_eq!(fixture.apply(change).err(), Some(Error::Entry(error)));
        }
        assert_eq!(fixture.bench.calls.len(), calls);
        assert_eq!(fixture.place.bytes(), before);
        // Keeping its own title is not a duplicate.
        fixture
            .apply(Change::Rename {
                id: mail.id,
                base: 1,
                title: "Mail".into(),
            })
            .unwrap();
    }

    #[test]
    fn a_batch_commits_as_one_revision_that_every_key_reads() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "old");
        let bank = fixture.create("Bank", "pin");
        let revision = fixture.session.revision();
        let calls = fixture.bench.calls.len();
        let committed = fixture
            .session
            .apply_all(
                &fixture.place.1,
                vec![
                    Change::Create {
                        title: "Shop".into(),
                        body: "cart".into(),
                    },
                    Change::Edit {
                        id: mail.id,
                        base: 1,
                        title: "Mail".into(),
                        body: "new".into(),
                    },
                    Change::Delete {
                        id: bank.id,
                        base: 1,
                    },
                    // Bank's title is free once the delete before it ran.
                    Change::Create {
                        title: "Bank".into(),
                        body: "".into(),
                    },
                ],
                &|| false,
                &mut fixture.random,
            )
            .unwrap();
        assert_eq!(fixture.bench.calls.len(), calls);
        assert_eq!(fixture.session.revision(), revision + 1);
        let revisions: Vec<_> = committed.iter().map(|c| c.revision).collect();
        assert_eq!(revisions, [Some(1), Some(2), None, Some(1)]);
        assert_eq!(committed[1].id, mail.id);
        let Fixture {
            place,
            mut bench,
            session,
            ..
        } = fixture;
        drop(session);
        let other = unlocked(&place, &mut bench, 0);
        let listed: Vec<_> = other
            .entries()
            .map(|entry| entry.title.to_owned())
            .collect();
        assert_eq!(listed, ["Mail", "Shop", "Bank"]);
        assert_eq!(other.entry(&mail.id).unwrap().body(), b"new");
        assert!(other.entry(&bank.id).is_none());
    }

    #[test]
    fn edits_in_a_batch_build_on_the_ones_before_them() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "one");
        let edit = |base, body: &str| Change::Edit {
            id: mail.id,
            base,
            title: "Mail".into(),
            body: Text::new(body.to_owned()),
        };
        let before = fixture.place.bytes();
        // A second edit against the session's revision is stale.
        let stale = fixture.session.apply_all(
            &fixture.place.1,
            vec![edit(1, "two"), edit(1, "three")],
            &|| false,
            &mut fixture.random,
        );
        assert_eq!(stale.err(), Some(Error::Entry(EntryError::Stale)));
        assert_eq!(fixture.place.bytes(), before);
        let committed = fixture
            .session
            .apply_all(
                &fixture.place.1,
                vec![edit(1, "two"), edit(2, "three")],
                &|| false,
                &mut fixture.random,
            )
            .unwrap();
        let revisions: Vec<_> = committed.iter().map(|c| c.revision).collect();
        assert_eq!(revisions, [Some(2), Some(3)]);
        let entry = fixture.session.entry(&mail.id).unwrap();
        assert_eq!((entry.revision, entry.body()), (3, b"three".as_slice()));
    }

    #[test]
    fn a_batch_with_one_refused_change_publishes_nothing() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "old");
        let before = fixture.place.bytes();
        let revision = fixture.session.revision();
        let batch = || {
            vec![
                Change::Edit {
                    id: mail.id,
                    base: 1,
                    title: "Mail".into(),
                    body: "new".into(),
                },
                Change::Create {
                    title: "Shop".into(),
                    body: "".into(),
                },
            ]
        };
        let mut refused = batch();
        // Two creations of one title: the second sees the first.
        refused.push(Change::Create {
            title: "Shop".into(),
            body: "".into(),
        });
        let result =
            fixture
                .session
                .apply_all(&fixture.place.1, refused, &|| false, &mut fixture.random);
        assert_eq!(result.err(), Some(Error::Entry(EntryError::Duplicate)));
        // A lock before publication refuses the whole batch too.
        let locked =
            fixture
                .session
                .apply_all(&fixture.place.1, batch(), &|| true, &mut fixture.random);
        assert!(matches!(locked, Err(Error::Token(_))));
        assert_eq!(fixture.place.bytes(), before);
        assert_eq!(fixture.session.revision(), revision);
        let entry = fixture.session.entry(&mail.id).unwrap();
        assert_eq!((entry.revision, entry.body()), (1, b"old".as_slice()));
        assert_eq!(fixture.session.entries().count(), 1);
        // No changes save nothing.
        let none = fixture
            .session
            .apply_all(&fixture.place.1, Vec::new(), &|| false, &mut fixture.random)
            .unwrap();
        assert!(none.is_empty());
        assert_eq!(fixture.place.bytes(), before);
        assert_eq!(fixture.session.revision(), revision);
    }

    #[test]
    fn a_refused_save_keeps_the_entry_at_its_base_revision() {
        let mut fixture = Fixture::new();
        let mail = fixture.create("Mail", "old");
        let before = fixture.place.bytes();
        let edit = || Change::Edit {
            id: mail.id,
            base: 1,
            title: "Mail".into(),
            body: "new".into(),
        };
        // A lock before publication refuses the save.
        let locked = fixture
            .session
            .apply(&fixture.place.1, edit(), &|| true, &mut fixture.random);
        assert!(matches!(locked, Err(Error::Token(_))));
        assert_eq!(fixture.place.bytes(), before);
        let entry = fixture.session.entry(&mail.id).unwrap();
        assert_eq!((entry.revision, entry.body()), (1, b"old".as_slice()));
        assert_eq!(fixture.apply(edit()).unwrap().revision, Some(2));
    }

    #[test]
    fn full_notebooks_refuse_creation_before_any_token() {
        let mut fixture = Fixture::new();
        let entries = (0..MAX_ENTRIES as u32)
            .map(|n| {
                let mut id = [0; 16];
                id[..4].copy_from_slice(&n.to_be_bytes());
                Entry::new(id, 1, format!("entry {n}"), Vec::new())
            })
            .collect();
        fixture
            .session
            .save(
                &fixture.place.1,
                &Notebook { entries },
                &|| false,
                &mut fixture.random,
            )
            .unwrap();
        let calls = fixture.bench.calls.len();
        let change = Change::Create {
            title: "one more".into(),
            body: "".into(),
        };
        assert_eq!(
            fixture.apply(change).err(),
            Some(Error::Entry(EntryError::Full))
        );
        assert_eq!(fixture.bench.calls.len(), calls);

        // 63 maximal bodies fit the plaintext bound; a 64th does not.
        let mut fixture = Fixture::new();
        let body = "b".repeat(MAX_BODY);
        let entries = (0..63u8)
            .map(|n| Entry::new([n; 16], 1, format!("large {n}"), body.clone().into_bytes()))
            .collect();
        fixture
            .session
            .save(
                &fixture.place.1,
                &Notebook { entries },
                &|| false,
                &mut fixture.random,
            )
            .unwrap();
        let calls = fixture.bench.calls.len();
        let change = Change::Create {
            title: "overflow".into(),
            body: Text::new(body),
        };
        assert_eq!(
            fixture.apply(change).err(),
            Some(Error::Entry(EntryError::Full))
        );
        assert_eq!(fixture.bench.calls.len(), calls);

        // An entry 100 bytes under the bound, then grown by an edit or a
        // rename past it.
        let used = plaintext_size(&fixture.session.notebook().entries).unwrap();
        let room = MAX_PLAIN - used - 32 - "small".len() - 100;
        let small = fixture.create("small", &"s".repeat(room));
        assert_eq!(
            plaintext_size(&fixture.session.notebook().entries),
            Some(MAX_PLAIN - 100)
        );
        let calls = fixture.bench.calls.len();
        let grown = [
            Change::Edit {
                id: small.id,
                base: 1,
                title: "small".into(),
                body: Text::new("s".repeat(room + 101)),
            },
            Change::Rename {
                id: small.id,
                base: 1,
                title: Text::new("t".repeat(200)),
            },
        ];
        for change in grown {
            assert_eq!(
                fixture.apply(change).err(),
                Some(Error::Entry(EntryError::Full))
            );
        }
        assert_eq!(fixture.bench.calls.len(), calls);
    }
}
