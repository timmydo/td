//! The window's keys, one table per view: the key list the window shows
//! on F1 or `?` and the CLI's `keybindings` export read the same rows.

use td_ui::keys::{Row, Section};

/// A view's keys: the CLI's name for the view, the list's title for its
/// section, and its bindings as (keys, action, description).
pub struct Table {
    pub id: &'static str,
    pub title: &'static str,
    pub bindings: &'static [(&'static str, &'static str, &'static str)],
}

pub const GLOBAL: Table = Table {
    id: "global",
    title: "Global",
    bindings: &[
        ("?", "show_keys", "Show this list of keys (F1 too)"),
        ("c", "compose", "Compose new email"),
    ],
};

pub const MAILBOX_LIST: Table = Table {
    id: "mailbox_list",
    title: "Mailbox List",
    bindings: &[
        ("q", "quit", "Quit"),
        ("n/j/Down", "next", "Next mailbox"),
        ("p/k/Up", "prev", "Previous mailbox"),
        ("Return", "open", "Open mailbox"),
        ("g", "refresh", "Refresh"),
        (
            "a",
            "next_account",
            "Next account; with one, reopen it (reconnect)",
        ),
        (
            "s",
            "set_up",
            "Set up an account with no server, or one not reached",
        ),
        ("D", "drafts", "Drafts: retained and sent; Return opens one"),
        ("+", "create_folder", "Create folder"),
        ("d", "delete_folder", "Delete selected folder"),
        (
            "u",
            "mark_all_read",
            "Mark all mail in selected folder read",
        ),
        ("x", "preview_retention", "Preview retention expiry list"),
        ("X", "expire_retention", "Expire retained mail now"),
        ("PageDown", "page_down", "Page down"),
        ("PageUp", "page_up", "Page up"),
        ("Home", "jump_top", "Jump to top"),
        ("End", "jump_bottom", "Jump to bottom"),
    ],
};

pub const EMAIL_LIST: Table = Table {
    id: "email_list",
    title: "Email List",
    bindings: &[
        ("q", "back", "Back to mailbox list"),
        ("n/j/Down", "next", "Next email"),
        ("p/k/Up", "prev", "Previous email"),
        ("Return", "open", "Open email"),
        ("t", "open_thread", "Open thread list view (same folder)"),
        (
            "T",
            "open_thread_cross_folder",
            "Open thread list view (all folders)",
        ),
        ("g", "refresh", "Refresh"),
        ("r", "reply", "Reply to selected email"),
        ("R", "reply_all", "Reply all to selected email"),
        ("e", "dry_run_rules", "Dry-run rules on loaded messages"),
        ("E", "run_rules", "Run rules on loaded messages"),
        ("a", "archive", "Archive selected email"),
        ("d", "delete", "Move selected email to deleted folder"),
        (
            "D",
            "destroy",
            "Expire selected email now (deleted folder only)",
        ),
        (
            "J",
            "mark_spam",
            "Mark spam: train classifier and move to Junk",
        ),
        (
            "H",
            "mark_ham",
            "Mark not-spam (ham): train classifier and move to Inbox",
        ),
        (
            "S",
            "show_spam_score",
            "Score selected message and tag it (S=spam, ?=unsure)",
        ),
        ("f", "toggle_flagged", "Toggle flagged"),
        ("u", "toggle_read", "Toggle read or unread"),
        ("m", "move", "Move to folder"),
        ("s", "search", "Search in mailbox"),
        ("l", "load_more", "Load more messages"),
        ("Escape", "clear_search", "Clear search"),
        ("PageDown", "page_down", "Page down"),
        ("PageUp", "page_up", "Page up"),
        ("Home", "jump_top", "Jump to top"),
        ("End", "jump_bottom", "Jump to bottom"),
    ],
};

pub const THREAD_VIEW: Table = Table {
    id: "thread_view",
    title: "Thread View",
    bindings: &[
        ("q", "back", "Back to email list"),
        ("n/j/Down", "next", "Next email"),
        ("p/k/Up", "prev", "Previous email"),
        ("Return", "open", "Open email"),
        ("g", "refresh", "Refresh"),
        ("a", "archive", "Archive selected email"),
        ("d", "delete", "Move selected email to deleted folder"),
        (
            "D",
            "destroy",
            "Expire selected email now (deleted folder only)",
        ),
        ("f", "toggle_flagged", "Toggle flagged"),
        ("u", "toggle_read", "Toggle read or unread"),
        ("PageDown", "page_down", "Page down"),
        ("PageUp", "page_up", "Page up"),
        ("Home", "jump_top", "Jump to top"),
        ("End", "jump_bottom", "Jump to bottom"),
    ],
};

pub const EMAIL_VIEW: Table = Table {
    id: "email_view",
    title: "Email View",
    bindings: &[
        ("q", "back", "Back to email list"),
        ("n", "next_unread", "Open next unread email"),
        ("p", "prev_unread", "Open previous unread email"),
        ("j/Down", "scroll_down", "Scroll down"),
        ("k/Up", "scroll_up", "Scroll up"),
        ("Space/PageDown", "page_down", "Page down"),
        ("PageUp", "page_up", "Page up"),
        ("Home", "jump_top", "Jump to top"),
        ("End", "jump_bottom", "Jump to bottom"),
        ("r", "reply", "Reply"),
        ("R", "reply_all", "Reply all"),
        (
            "F",
            "forward_attachment",
            "Forward as attachment (preserves HTML)",
        ),
        ("f", "forward", "Forward as inline quoted text"),
        ("a", "archive", "Archive message"),
        ("d", "delete", "Delete message (move to trash)"),
        ("m", "move", "Move to mailbox (interactive picker)"),
        ("A", "attachment", "Download or open an attachment"),
        ("b", "browse_urls", "Browse URLs found in message body"),
        (
            "1..9",
            "open_url",
            "Open URL by number in configured browser",
        ),
        (
            "C-click",
            "follow_link",
            "Open the link under the pointer in the browser",
        ),
        ("h", "toggle_html", "Toggle HTML vs plain text body"),
        (
            "v",
            "raw_headers",
            "Toggle raw headers (DKIM, Received, etc)",
        ),
        ("*", "toggle_flagged", "Toggle flagged"),
        ("u", "toggle_read", "Toggle read or unread"),
        (
            "J",
            "mark_spam",
            "Mark spam: train classifier and move to Junk",
        ),
        (
            "H",
            "mark_ham",
            "Mark not-spam (ham): train classifier and move to Inbox",
        ),
        (
            "S",
            "show_spam_score",
            "Show this message's spam score and verdict",
        ),
        ("D", "destroy", "Expire now (deleted folder only)"),
    ],
};

pub const DRAFTS: Table = Table {
    id: "drafts",
    title: "Drafts",
    bindings: &[
        (
            "Return",
            "open",
            "Reopen the draft to edit, or show the sent one read-only",
        ),
        ("g", "refresh", "Read the drafts and sent directories again"),
        ("q", "back", "Back to the mailbox list"),
    ],
};

/// A sent draft shown read-only, as it was retired.
pub const SENT: Table = Table {
    id: "sent",
    title: "Sent Draft",
    bindings: &[
        ("q/Escape", "back", "Back to the drafts"),
        ("n/j", "scroll_down", "Scroll down a line"),
        ("p/k", "scroll_up", "Scroll up a line"),
        ("Space/PageDown", "page_down", "Page down"),
        ("PageUp", "page_up", "Page up"),
        (
            "Up/Down",
            "caret_line",
            "Caret up or down a line; the view follows",
        ),
        (
            "Home/End",
            "caret_line_ends",
            "Caret to its line's start or end; the view follows",
        ),
    ],
};

/// The draft in the editable pane, whose keys are the editor core's
/// default profile; the rest of the keyboard types.
pub const COMPOSE: Table = Table {
    id: "compose",
    title: "Compose",
    bindings: &[
        (
            "C-Return",
            "send",
            "Send the draft through the account's server (saves it first)",
        ),
        (
            "C-S-a",
            "attach",
            "Attach a file: its tag at the caret's line, or at the end \
             (the finder: Return opens or attaches, Backspace on an empty \
             filter goes up, letters filter, Escape closes it)",
        ),
        ("C-s", "save", "Save the draft over its retained file"),
        (
            "C-w",
            "close",
            "Close (asks when unsaved: y saves, n keeps the file as saved)",
        ),
        (
            "C-x/C-c/C-v",
            "cut_copy_paste",
            "Cut, copy, paste within td-mail (a message's selection too)",
        ),
        ("C-z/C-y", "undo_redo", "Undo, redo"),
        ("C-a", "select_all", "Select all"),
    ],
};

const TABLES: &[&Table] = &[
    &GLOBAL,
    &MAILBOX_LIST,
    &EMAIL_LIST,
    &THREAD_VIEW,
    &EMAIL_VIEW,
    &DRAFTS,
    &SENT,
    &COMPOSE,
];

/// The key list's sections: `first`'s, the view shown, then the rest in
/// the tables' order.
pub fn sections(first: Option<&Table>) -> Vec<Section> {
    let first = first.map(|table| table.id);
    let (front, rest): (Vec<&Table>, Vec<&Table>) = TABLES
        .iter()
        .copied()
        .partition(|table| Some(table.id) == first);
    front
        .into_iter()
        .chain(rest)
        .map(|table| Section {
            title: table.title,
            rows: table
                .bindings
                .iter()
                .map(|&(keys, _, what)| Row { keys, what })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub view: &'static str,
    pub key: &'static str,
    pub action: &'static str,
    pub description: &'static str,
}

/// Every binding, for the CLI's export.
pub fn all_keybindings() -> Vec<KeyBinding> {
    TABLES
        .iter()
        .flat_map(|table| {
            table
                .bindings
                .iter()
                .map(|&(key, action, description)| KeyBinding {
                    view: table.id,
                    key,
                    action,
                    description,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// One section per view, titled by its table, the shown view's
    /// first; a row reads as the table wrote it.
    #[test]
    fn the_sections_are_the_tables_with_the_shown_view_first() {
        let titles = |sections: &[Section]| -> Vec<&str> {
            sections.iter().map(|section| section.title).collect()
        };
        let all = sections(None);
        assert_eq!(
            titles(&all),
            [
                "Global",
                "Mailbox List",
                "Email List",
                "Thread View",
                "Email View",
                "Drafts",
                "Sent Draft",
                "Compose"
            ]
        );
        assert_eq!(
            all[0].rows[0],
            Row {
                keys: "?",
                what: "Show this list of keys (F1 too)"
            }
        );
        let viewing = sections(Some(&EMAIL_VIEW));
        assert_eq!(
            titles(&viewing),
            [
                "Email View",
                "Global",
                "Mailbox List",
                "Email List",
                "Thread View",
                "Drafts",
                "Sent Draft",
                "Compose"
            ]
        );
        assert!(viewing[0].rows.contains(&Row {
            keys: "b",
            what: "Browse URLs found in message body"
        }));
        let compose = &viewing[7].rows;
        assert_eq!(compose[1].keys, "C-S-a");
    }

    /// The CLI's export is every row, each with its view, keys and
    /// action.
    #[test]
    fn the_cli_export_is_every_binding_with_its_keys() {
        let all = all_keybindings();
        assert_eq!(all[0].view, "global");
        assert_eq!(all[0].action, "show_keys");
        assert!(all
            .iter()
            .all(|binding| !binding.key.is_empty() && !binding.action.is_empty()));
        let rows: usize = TABLES.iter().map(|table| table.bindings.len()).sum();
        assert_eq!(all.len(), rows);
    }

    /// Every list the window can show, whichever view leads, is spelled
    /// and written as td-ui's key list holds every program's.
    #[test]
    fn every_list_passes_the_key_list_check() {
        let leads = std::iter::once(None).chain(TABLES.iter().copied().map(Some));
        for lead in leads {
            let problems = td_ui::keys::check(&sections(lead));
            assert!(
                problems.is_empty(),
                "lead {:?}:\n{}",
                lead.map(|table| table.id),
                problems.join("\n")
            );
        }
    }
}
