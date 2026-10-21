//! The account tree beside the views, with two or more accounts: each
//! account's heading and, under it, its folders as its backend last
//! listed them. A heading opens the account's own page (its folder list,
//! set-up and reconnect); a folder opens that folder's mail. The tree
//! keeps its selection by entry, so a folder list arriving for one
//! account does not move it.

use super::views::mailbox_list::MailboxListView;
use super::views::Row;
use crate::jmap::types::Mailbox;

/// The tree's width in cells, beside a window at least three times as
/// wide; a narrower window shows the tree over the body while it has the
/// keys, and not otherwise.
pub const CELLS: usize = 24;

/// One entry of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The account at this place in the session's accounts.
    Account(usize),
    /// A folder of that account, by its mailbox id.
    Folder { account: usize, id: String },
}

/// What the tree knows of an account.
struct Account {
    name: String,
    /// The folders, sorted as the folder list sorts them; none until the
    /// first answer.
    folders: Option<Vec<Mailbox>>,
    /// Why the last folder fetch failed, when it did.
    error: Option<String>,
}

pub struct Sidebar {
    accounts: Vec<Account>,
    selected: Entry,
    /// The first entry shown, kept by the session as a list's is.
    pub first: usize,
    /// The tree has the keys rather than the view.
    pub focused: bool,
}

impl Sidebar {
    pub fn new(names: &[String], current: usize) -> Self {
        Sidebar {
            accounts: names
                .iter()
                .map(|name| Account {
                    name: name.clone(),
                    folders: None,
                    error: None,
                })
                .collect(),
            selected: Entry::Account(current),
            first: 0,
            focused: false,
        }
    }

    /// An account's answer to a folder fetch: a listing replaces what the
    /// tree held, a failure is noted over it.
    pub fn listed(&mut self, account: usize, result: &Result<Vec<Mailbox>, String>) {
        let Some(held) = self.accounts.get_mut(account) else {
            return;
        };
        match result {
            Ok(folders) => {
                let mut folders = folders.clone();
                MailboxListView::sort_mailboxes(&mut folders);
                held.folders = Some(folders);
                held.error = None;
            }
            Err(e) => held.error = Some(e.clone()),
        }
    }

    /// The account's folders as last listed.
    pub fn folders(&self, account: usize) -> Option<&[Mailbox]> {
        self.accounts.get(account)?.folders.as_deref()
    }

    fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.accounts
            .iter()
            .enumerate()
            .flat_map(|(index, account)| {
                std::iter::once(Entry::Account(index)).chain(account.folders.iter().flatten().map(
                    move |folder| Entry::Folder {
                        account: index,
                        id: folder.id.clone(),
                    },
                ))
            })
    }

    pub fn len(&self) -> usize {
        self.accounts
            .iter()
            .map(|a| 1 + a.folders.as_ref().map_or(0, Vec::len))
            .sum()
    }

    pub fn entry(&self, index: usize) -> Option<Entry> {
        self.entries().nth(index)
    }

    /// The selected entry's place; its account's heading when a new
    /// listing no longer holds the folder.
    pub fn cursor(&self) -> usize {
        let at = |wanted: &Entry| self.entries().position(|entry| &entry == wanted);
        at(&self.selected)
            .or_else(|| match &self.selected {
                Entry::Folder { account, .. } => at(&Entry::Account(*account)),
                Entry::Account(_) => None,
            })
            .unwrap_or(0)
    }

    pub fn selected(&self) -> Entry {
        self.entry(self.cursor()).unwrap_or(Entry::Account(0))
    }

    pub fn select(&mut self, entry: Entry) {
        self.selected = entry;
    }

    /// The selection moved by `delta` entries, clamped to the tree.
    pub fn move_by(&mut self, delta: isize) {
        let last = self.len().saturating_sub(1);
        let to = self.cursor().saturating_add_signed(delta).min(last);
        if let Some(entry) = self.entry(to) {
            self.selected = entry;
        }
    }

    /// Entry `index` as a row: a heading names its account, with its
    /// inbox's unread or why it has no folders at the right; a folder is
    /// indented under it, with its unread.
    pub fn row(&self, index: usize) -> Row {
        match self.entry(index) {
            Some(Entry::Account(account)) => {
                let Some(held) = self.accounts.get(account) else {
                    return Row::default();
                };
                let unread: u32 = held
                    .folders
                    .iter()
                    .flatten()
                    .filter(|f| f.role.as_deref() == Some("inbox"))
                    .map(|f| f.unread_emails)
                    .sum();
                let meta = match (&held.folders, &held.error) {
                    (_, Some(_)) => "offline".to_string(),
                    (None, None) => "...".to_string(),
                    (Some(_), None) if unread > 0 => unread.to_string(),
                    (Some(_), None) => String::new(),
                };
                Row {
                    label: held.name.clone(),
                    meta,
                    marked: unread > 0,
                }
            }
            Some(Entry::Folder { account, id }) => {
                let Some(folder) = self
                    .folders(account)
                    .and_then(|folders| folders.iter().find(|f| f.id == id))
                else {
                    return Row::default();
                };
                Row {
                    label: format!("  {}", folder.name.replace(['\n', '\r'], " ")),
                    meta: if folder.unread_emails > 0 {
                        folder.unread_emails.to_string()
                    } else {
                        String::new()
                    },
                    marked: folder.unread_emails > 0,
                }
            }
            None => Row::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn folder(id: &str, name: &str, role: Option<&str>, unread: u32) -> Mailbox {
        Mailbox {
            id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
            role: role.map(str::to_string),
            total_emails: unread + 1,
            unread_emails: unread,
            sort_order: 0,
        }
    }

    /// The tree lists each account's heading and then its folders, the
    /// inbox first; an account not yet listed says so, one whose fetch
    /// failed says it is offline; the selection stays on its entry as
    /// listings arrive, and falls back to the heading when its folder
    /// goes.
    #[test]
    fn the_tree_lists_each_accounts_folders_and_keeps_its_selection() {
        let mut tree = Sidebar::new(&["home".to_string(), "work".to_string()], 1);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.selected(), Entry::Account(1));
        assert_eq!(tree.row(0).meta, "...");
        tree.listed(
            1,
            &Ok(vec![
                folder("w2", "Projects", None, 0),
                folder("w1", "Inbox", Some("inbox"), 4),
            ]),
        );
        assert_eq!(tree.selected(), Entry::Account(1));
        let rows: Vec<_> = (0..tree.len()).map(|i| tree.row(i).label).collect();
        assert_eq!(rows, ["home", "work", "  Inbox", "  Projects"]);
        assert_eq!(tree.row(1).meta, "4");
        assert!(tree.row(1).marked);
        tree.move_by(2);
        assert_eq!(
            tree.selected(),
            Entry::Folder {
                account: 1,
                id: "w2".to_string()
            }
        );
        tree.listed(0, &Ok(vec![folder("h1", "Inbox", Some("inbox"), 0)]));
        assert_eq!(tree.cursor(), 4);
        tree.listed(0, &Err("no route to host".to_string()));
        assert_eq!(tree.row(0).meta, "offline");
        assert_eq!(tree.len(), 5, "a failure keeps the folders it had");
        tree.listed(1, &Ok(vec![folder("w1", "Inbox", Some("inbox"), 0)]));
        assert_eq!(tree.selected(), Entry::Account(1));
        tree.move_by(-10);
        assert_eq!(tree.selected(), Entry::Account(0));
        tree.move_by(10);
        assert_eq!(
            tree.selected(),
            Entry::Folder {
                account: 1,
                id: "w1".to_string()
            }
        );
    }
}
