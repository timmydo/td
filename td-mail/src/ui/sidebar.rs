//! The account tree beside the views, with two or more accounts: the
//! unified inbox first, then each account's heading and, under it, its
//! folders as its backend last listed them. A heading opens the
//! account's own page (its folder list, set-up and reconnect); a folder
//! opens that folder's mail; the unified inbox every account's inbox.
//! The tree keeps its selection by entry, so a folder list arriving for
//! one account does not move it, and its scroll apart from it, so the
//! wheel moves the rows shown and leaves the selection where it was.

use super::views::mailbox_list::MailboxListView;
use super::views::Row;
use crate::jmap::types::Mailbox;
use td_ui::chrome::List;

/// The tree's width in cells until it is resized, and the window width,
/// three of these, from which it is a column beside the views; a narrower
/// window shows the tree over the body while it has the keys, and not
/// otherwise.
pub const CELLS: usize = 24;
/// The narrowest a resized tree goes.
pub const MIN_CELLS: usize = 12;

/// The tree's width in cells in a window `columns` cells wide: `wanted`,
/// held between `MIN_CELLS` and half the window, so the views keep at
/// least as much as the tree.
pub fn fit(wanted: usize, columns: usize) -> usize {
    wanted.min(columns / 2).max(MIN_CELLS)
}

/// One entry of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Every account's inbox in one list.
    Unified,
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

impl Account {
    /// The unread of its inbox, as last listed.
    fn inbox_unread(&self) -> u32 {
        self.folders
            .iter()
            .flatten()
            .filter(|f| f.role.as_deref() == Some("inbox"))
            .map(|f| f.unread_emails)
            .sum()
    }
}

pub struct Sidebar {
    accounts: Vec<Account>,
    selected: Entry,
    /// The first entry shown: moved to reveal the selection when it moves,
    /// and by the wheel and the scrollbar without moving it.
    pub first: usize,
    /// The selection moved since the tree was last laid out, which then
    /// scrolls it into view.
    follow: bool,
    /// The rows the tree showed when last laid out.
    rows: usize,
    /// The width asked for, in cells; `fit` holds it to the window.
    pub cells: usize,
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
            follow: true,
            rows: 0,
            cells: CELLS,
            focused: false,
        }
    }

    /// An account's answer to a folder fetch: a listing replaces what the
    /// tree held, a failure is noted over it. A selection shown before is
    /// kept shown, though entries above it come or go.
    pub fn listed(&mut self, account: usize, result: &Result<Vec<Mailbox>, String>) {
        let cursor = self.cursor();
        if cursor >= self.first && cursor < self.first.saturating_add(self.rows) {
            self.follow = true;
        }
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
        let accounts = self
            .accounts
            .iter()
            .enumerate()
            .flat_map(|(index, account)| {
                std::iter::once(Entry::Account(index)).chain(account.folders.iter().flatten().map(
                    move |folder| Entry::Folder {
                        account: index,
                        id: folder.id.clone(),
                    },
                ))
            });
        std::iter::once(Entry::Unified).chain(accounts)
    }

    pub fn len(&self) -> usize {
        1 + self
            .accounts
            .iter()
            .map(|a| 1 + a.folders.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
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
                Entry::Account(_) | Entry::Unified => None,
            })
            .unwrap_or(0)
    }

    pub fn selected(&self) -> Entry {
        self.entry(self.cursor()).unwrap_or(Entry::Unified)
    }

    pub fn select(&mut self, entry: Entry) {
        self.selected = entry;
        self.follow = true;
    }

    /// The selection moved by `delta` entries, clamped to the tree.
    pub fn move_by(&mut self, delta: isize) {
        let last = self.len().saturating_sub(1);
        let to = self.cursor().saturating_add_signed(delta).min(last);
        if let Some(entry) = self.entry(to) {
            self.selected = entry;
        }
        self.follow = true;
    }

    /// The selection brought into view at the next layout, as when the
    /// tree takes the keys, so the cursor that says it has them is shown.
    pub fn reveal(&mut self) {
        self.follow = true;
    }

    /// The rows shown moved by `rows`, down when positive, the selection
    /// left where it is.
    pub fn scroll(&mut self, rows: isize, list: List) {
        self.scroll_to(self.first.saturating_add_signed(rows), list);
    }

    /// The rows shown from entry `first`, as far as the last row allows.
    pub fn scroll_to(&mut self, first: usize, list: List) {
        self.first = first;
        self.follow = false;
        self.lay_out(list);
    }

    /// The first entry shown in `list`: the selection revealed when it
    /// moved, and otherwise held where the last row shows the last entry.
    pub fn lay_out(&mut self, list: List) {
        let len = self.len();
        self.rows = list.rows();
        self.first = if std::mem::take(&mut self.follow) {
            list.reveal(len, self.cursor(), self.first)
        } else {
            self.first.min(len.saturating_sub(list.rows()))
        };
    }

    /// Entry `index` as a row: the unified inbox with every inbox's
    /// unread; a heading names its account, with its inbox's unread or
    /// why it has no folders at the right; a folder is indented under
    /// it, with its unread.
    pub fn row(&self, index: usize) -> Row {
        let count = |unread: u32| {
            if unread > 0 {
                unread.to_string()
            } else {
                String::new()
            }
        };
        match self.entry(index) {
            Some(Entry::Unified) => {
                let unread: u32 = self.accounts.iter().map(Account::inbox_unread).sum();
                Row {
                    label: "All inboxes".to_string(),
                    meta: count(unread),
                    marked: unread > 0,
                }
            }
            Some(Entry::Account(account)) => {
                let Some(held) = self.accounts.get(account) else {
                    return Row::default();
                };
                let unread = held.inbox_unread();
                let meta = match (&held.folders, &held.error) {
                    (_, Some(_)) => "offline".to_string(),
                    (None, None) => "...".to_string(),
                    (Some(_), None) => count(unread),
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
                    meta: count(folder.unread_emails),
                    marked: folder.unread_emails > 0,
                }
            }
            None => Row::default(),
        }
    }

    /// Entry `index` as a row of a list `columns` cells wide: a label
    /// that would run into its count is cut short with an ellipsis a cell
    /// before it, so the count is not read as part of the name.
    pub fn row_within(&self, index: usize, columns: usize) -> Row {
        let mut row = self.row(index);
        // The label follows a cell of inset and the mark's two; the
        // count, a cell after it, ends a cell of inset short of the edge.
        let count = row.meta.chars().count();
        let room = columns.saturating_sub(count + usize::from(count > 0) + 4);
        if row.label.chars().count() > room {
            row.label = row
                .label
                .chars()
                .take(room.saturating_sub(1))
                .chain(['\u{2026}'])
                .collect();
        }
        row
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

    /// The tree lists the unified inbox, counting every inbox's unread,
    /// then each account's heading and its folders, the inbox first; an
    /// account not yet listed says so, one whose fetch failed says it is
    /// offline; the selection stays on its entry as listings arrive, and
    /// falls back to the heading when its folder goes.
    #[test]
    fn the_tree_lists_each_accounts_folders_and_keeps_its_selection() {
        let mut tree = Sidebar::new(&["home".to_string(), "work".to_string()], 1);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.selected(), Entry::Account(1));
        assert_eq!(tree.row(1).meta, "...");
        tree.listed(
            1,
            &Ok(vec![
                folder("w2", "Projects", None, 0),
                folder("w1", "Inbox", Some("inbox"), 4),
            ]),
        );
        assert_eq!(tree.selected(), Entry::Account(1));
        let rows: Vec<_> = (0..tree.len()).map(|i| tree.row(i).label).collect();
        assert_eq!(
            rows,
            ["All inboxes", "home", "work", "  Inbox", "  Projects"]
        );
        assert_eq!(tree.row(2).meta, "4");
        assert!(tree.row(2).marked);
        tree.move_by(2);
        assert_eq!(
            tree.selected(),
            Entry::Folder {
                account: 1,
                id: "w2".to_string()
            }
        );
        tree.listed(0, &Ok(vec![folder("h1", "Inbox", Some("inbox"), 3)]));
        assert_eq!(tree.cursor(), 5);
        assert_eq!(tree.row(0).meta, "7");
        tree.listed(0, &Err("no route to host".to_string()));
        assert_eq!(tree.row(1).meta, "offline");
        assert_eq!(tree.len(), 6, "a failure keeps the folders it had");
        tree.listed(1, &Ok(vec![folder("w1", "Inbox", Some("inbox"), 0)]));
        assert_eq!(tree.selected(), Entry::Account(1));
        tree.move_by(-10);
        assert_eq!(tree.selected(), Entry::Unified);
        tree.move_by(10);
        assert_eq!(
            tree.selected(),
            Entry::Folder {
                account: 1,
                id: "w1".to_string()
            }
        );
    }

    /// A name that would run into its count is cut short with an
    /// ellipsis a cell before it; one that fits is whole.
    #[test]
    fn a_long_name_stops_short_of_its_count() {
        let mut tree = Sidebar::new(&["home".to_string(), "work".to_string()], 0);
        tree.listed(
            0,
            &Ok(vec![
                folder("h1", "Inbox", Some("inbox"), 123),
                folder("h2", "A folder named at some length", None, 7),
            ]),
        );
        // Entries: All inboxes, home, Inbox, the long one.
        let row = tree.row_within(3, 22);
        assert_eq!(row.label, "  A folder name\u{2026}");
        assert_eq!(row.meta, "7");
        // A cell of inset, the mark's two, the label, a gap, the count and
        // the inset again fill the row.
        assert_eq!(1 + 2 + row.label.chars().count() + 1 + 1 + 1, 22);
        assert_eq!(tree.row_within(2, 22).label, "  Inbox");
        assert_eq!(tree.row_within(3, 4).label, "\u{2026}");
        // With no count there is no gap to keep: a name filling the row
        // to its inset is whole, one cell longer is cut.
        for (name, label) in [
            ("Sixteen cells ok", "  Sixteen cells ok"),
            ("Seventeen cells x", "  Seventeen cells\u{2026}"),
        ] {
            let mut tree = Sidebar::new(&["home".to_string()], 0);
            tree.listed(0, &Ok(vec![folder("h1", name, None, 0)]));
            assert_eq!(tree.row_within(2, 22).label, label);
        }
    }

    /// The wheel moves the rows shown and not the selection, held where
    /// the last entry is on the last row; a layout keeps the scroll until
    /// the selection moves, which brings it back into view.
    #[test]
    fn the_tree_scrolls_apart_from_its_selection() {
        let mut tree = Sidebar::new(&["home".to_string(), "work".to_string()], 0);
        let folders: Vec<_> = (0..20)
            .map(|n| folder(&format!("h{n}"), &format!("F{n:02}"), None, 0))
            .collect();
        tree.listed(0, &Ok(folders));
        assert_eq!(tree.len(), 23);
        let surface = td_ui::raster::Surface::new(800, 600, Default::default()).unwrap();
        let rect = td_ui::raster::Rect {
            x: 0,
            y: 0,
            width: 192,
            height: (td_ui::chrome::ROW * 5) as u32,
        };
        let list = List::new(surface, rect).unwrap();
        assert_eq!(list.rows(), 5);
        tree.lay_out(list);
        assert_eq!(tree.first, 0);
        tree.scroll(4, list);
        assert_eq!(tree.first, 4);
        assert_eq!(tree.selected(), Entry::Account(0));
        tree.lay_out(list);
        assert_eq!(tree.first, 4, "a layout keeps the scroll");
        tree.scroll(100, list);
        assert_eq!(tree.first, 18);
        tree.scroll(-100, list);
        assert_eq!(tree.first, 0);
        tree.scroll_to(10, list);
        tree.move_by(1);
        tree.lay_out(list);
        assert_eq!(tree.first, 2, "a moved selection is shown again");
        tree.scroll_to(10, list);
        tree.reveal();
        tree.lay_out(list);
        assert_eq!(tree.first, 2, "a revealed selection is shown again");

        // A listing above a shown selection keeps it shown; one under a
        // scrolled-away selection leaves the scroll.
        tree.move_by(4);
        tree.lay_out(list);
        assert_eq!((tree.first, tree.cursor()), (2, 6));
        let more: Vec<_> = (0..30)
            .map(|n| folder(&format!("e{n}"), &format!("E{n:02}"), None, 0))
            .collect();
        let mut both = more.clone();
        both.extend((0..20).map(|n| folder(&format!("h{n}"), &format!("F{n:02}"), None, 0)));
        tree.listed(0, &Ok(both));
        tree.lay_out(list);
        assert_eq!(tree.cursor(), 36);
        assert!((tree.first..tree.first + 5).contains(&36));
        tree.scroll_to(0, list);
        tree.listed(0, &Ok(more));
        tree.lay_out(list);
        assert_eq!(tree.first, 0);
    }
}
