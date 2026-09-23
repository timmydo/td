//! Drafts: the drafts td-mail retained and the ones it sent, listed from
//! the drafts directory and the `sent` directory beside it, drafts first
//! and each newest first. Return on a draft opens it in the compose view
//! again, with its attachment sidecar when it has one; on a sent one it
//! shows the message as it was retired, read-only. The list is read again
//! when a view over it closes, so a draft sent meanwhile has moved.

use crate::backend::{BackendCommand, BackendResponse};
use crate::ui::input::Key;
use crate::ui::views::compose::ComposeView;
use crate::ui::views::{
    format_system_date_time, text_scroll, wrap_text, Body, Row, Scene, Scroll, View, ViewAction,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::SystemTime;

/// The most entries listed from each directory, of the first `EXAMINED`
/// names read from it.
const ENTRIES: usize = 1000;
const EXAMINED: usize = 8192;
/// The most of a file read for its headers.
const HEAD_BYTES: u64 = 16 * 1024;
/// The most characters of a subject or recipients shown.
const SHOWN_CHARS: usize = 120;

/// A retained file: where it is, whether it was sent, what its headers
/// say, when it last changed, and, for a draft, whether a send of it
/// whose answer was lost is on record.
#[derive(Clone, Debug, PartialEq)]
pub struct Retained {
    pub path: PathBuf,
    pub sent: bool,
    pub subject: String,
    pub to: String,
    pub modified: SystemTime,
    pub unsettled: bool,
}

/// What `list_retained` found.
#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub entries: Vec<Retained>,
    /// A directory held more than the list keeps.
    pub cut: bool,
    /// Each directory there that could not be read, and why.
    pub unread: Vec<String>,
}

/// The drafts in `drafts` and the sent ones in the `sent` directory
/// beside it: regular files named `td-mail-draft-ID.eml`, drafts first,
/// each newest first, at most `ENTRIES` of the first `EXAMINED` names read
/// from each, and only those read for their headers. A directory not
/// there lists nothing.
pub fn list_retained(drafts: &Path) -> Listing {
    // Only beside a directory named `drafts`, as the retire's
    // `submit::sent_dir_for` names it.
    let sent = drafts
        .parent()
        .filter(|_| drafts.file_name().is_some_and(|name| name == "drafts"))
        .map(|parent| parent.join("sent"));
    let mut listing = Listing::default();
    for (dir, is_sent) in [(Some(drafts.to_path_buf()), false), (sent, true)] {
        let Some(dir) = dir else {
            continue;
        };
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                listing.unread.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let mut listed = Vec::new();
        for (seen, entry) in entries.enumerate() {
            if seen >= EXAMINED {
                listing.cut = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let named = entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("td-mail-draft-")
                    .and_then(|rest| rest.strip_suffix(".eml"))
                    .is_some_and(|id| !id.is_empty())
            });
            if !named {
                continue;
            }
            // The entry's own metadata: a link is not followed.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                listed.push((entry.path(), modified));
            }
        }
        listed.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if listed.len() > ENTRIES {
            listed.truncate(ENTRIES);
            listing.cut = true;
        }
        for (path, modified) in listed {
            let (subject, to) = headers(&path);
            let unsettled = !is_sent
                && std::fs::symlink_metadata(crate::compose::lost_record_for(&path)).is_ok();
            listing.entries.push(Retained {
                path,
                sent: is_sent,
                subject,
                to,
                modified,
                unsettled,
            });
        }
    }
    listing
}

/// The Subject and To of the draft at `path`, read from its head as far
/// as its separator line or first empty line, a folded header joined;
/// each shown without controls and cut to `SHOWN_CHARS`. A file that
/// cannot be read, or is not a regular file once opened, shows neither.
fn headers(path: &Path) -> (String, String) {
    let mut head = Vec::new();
    let read = crate::submit::open_regular(path)
        .and_then(|file| file.take(HEAD_BYTES).read_to_end(&mut head));
    if read.is_err() {
        return (String::new(), String::new());
    }
    let text = String::from_utf8_lossy(&head);
    let (mut subject, mut to) = (String::new(), String::new());
    let mut current: Option<&mut String> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_end() == crate::submit::SEPARATOR {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(value) = current.as_deref_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        current = None;
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let target = match name.trim().to_ascii_lowercase().as_str() {
            "subject" => &mut subject,
            "to" => &mut to,
            _ => continue,
        };
        target.clear();
        target.push_str(value.trim());
        current = Some(target);
    }
    (shown(&subject), shown(&to))
}

/// `text` as a row shows it: controls as spaces, at most `SHOWN_CHARS`.
fn shown(text: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(SHOWN_CHARS)
        .collect();
    if text.chars().count() > SHOWN_CHARS {
        out.push('…');
    }
    out
}

pub struct DraftsView {
    drafts: PathBuf,
    entries: Vec<Retained>,
    cut: bool,
    unread: Vec<String>,
    cursor: usize,
    status: Option<String>,
    pending_click: bool,
    cmd_tx: Sender<BackendCommand>,
}

impl DraftsView {
    /// The drafts in `drafts` and those sent beside it, listed now.
    pub fn new(drafts: PathBuf, cmd_tx: Sender<BackendCommand>) -> Self {
        let mut view = DraftsView {
            drafts,
            entries: Vec::new(),
            cut: false,
            unread: Vec::new(),
            cursor: 0,
            status: None,
            pending_click: false,
            cmd_tx,
        };
        view.relist();
        view
    }

    /// The list read again, the selection kept on the file it was on by
    /// its name, which a retire keeps, when that is still listed, else on
    /// the row it was on.
    fn relist(&mut self) {
        let on = self
            .entries
            .get(self.cursor)
            .and_then(|entry| entry.path.file_name())
            .map(|name| name.to_os_string());
        let listing = list_retained(&self.drafts);
        self.entries = listing.entries;
        self.cut = listing.cut;
        self.unread = listing.unread;
        self.cursor = on
            .and_then(|on| {
                self.entries
                    .iter()
                    .position(|entry| entry.path.file_name() == Some(on.as_os_str()))
            })
            .unwrap_or(self.cursor)
            .min(self.entries.len().saturating_sub(1));
    }

    fn row(&self, index: usize) -> Row {
        let Some(entry) = self.entries.get(index) else {
            return Row::default();
        };
        let subject = if entry.subject.is_empty() {
            "(no subject)"
        } else {
            entry.subject.as_str()
        };
        let label = if entry.to.is_empty() {
            subject.to_string()
        } else {
            format!("{subject} — {}", entry.to)
        };
        Row {
            label,
            meta: format!(
                "{} {}",
                if entry.sent {
                    "sent"
                } else if entry.unsettled {
                    "draft, send unsettled"
                } else {
                    "draft"
                },
                format_system_date_time(entry.modified)
            ),
            marked: !entry.sent,
        }
    }

    /// The selected entry opened: a draft in the compose view, a sent one
    /// read-only.
    fn open(&mut self) -> ViewAction {
        let Some(entry) = self.entries.get(self.cursor).cloned() else {
            return ViewAction::Continue;
        };
        if entry.sent {
            return match SentView::open(&entry.path) {
                Ok(view) => ViewAction::Push(Box::new(view)),
                Err(e) => {
                    self.status = Some(format!("Cannot show {}: {e}", entry.path.display()));
                    ViewAction::Continue
                }
            };
        }
        let sidecar = crate::attach::sidecar_for(&entry.path)
            .ok()
            .filter(|dir| std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()));
        match ComposeView::open(entry.path.clone(), sidecar, self.cmd_tx.clone()) {
            Ok(view) => ViewAction::Push(Box::new(view)),
            Err(e) => {
                self.status = Some(format!("Cannot open {}: {e}", entry.path.display()));
                ViewAction::Continue
            }
        }
    }

    fn move_to(&mut self, index: usize) {
        self.cursor = index.min(self.entries.len().saturating_sub(1));
    }
}

const LABELS: &[&str] = &["Open", "Refresh", "Back"];
const KEYS: &[Key] = &[Key::Enter, Key::Char('g'), Key::Escape];

impl View for DraftsView {
    fn scene(&self) -> Scene<'_> {
        let body = if self.entries.is_empty() {
            Body::message(format!(
                "No drafts or sent drafts in {}",
                self.drafts.display()
            ))
        } else {
            Body::List {
                total: self.entries.len(),
                selected: self.cursor,
                row: Box::new(move |index| self.row(index)),
            }
        };
        let drafts = self.entries.iter().filter(|entry| !entry.sent).count();
        let status = self.status.clone().unwrap_or_else(|| {
            // What the next Send of the selected draft does comes first.
            if self
                .entries
                .get(self.cursor)
                .is_some_and(|entry| entry.unsettled)
            {
                return "A send of this draft is on record, unsettled: Send asks the server \
                        first, and sends none of the edits since if that send went"
                    .to_string();
            }
            if !self.unread.is_empty() {
                return format!("Cannot read {}", self.unread.join("; "));
            }
            format!(
                "{drafts} draft{}, {} sent{} | Enter:open g:refresh q:back",
                if drafts == 1 { "" } else { "s" },
                self.entries.len() - drafts,
                if self.cut { " (cut short)" } else { "" }
            )
        });
        Scene {
            title: "Drafts".to_string(),
            labels: LABELS,
            keys: KEYS,
            entry: None,
            body,
            status,
        }
    }

    fn handle_key(&mut self, key: Key, page: usize) -> ViewAction {
        self.status = None;
        match key {
            Key::Char('q') | Key::Escape => ViewAction::Pop,
            Key::Char('n') | Key::Char('j') | Key::Down | Key::ScrollDown => {
                self.move_to(self.cursor.saturating_add(1));
                ViewAction::Continue
            }
            Key::Char('p') | Key::Char('k') | Key::Up | Key::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(1);
                ViewAction::Continue
            }
            Key::PageDown => {
                self.move_to(self.cursor.saturating_add(page));
                ViewAction::Continue
            }
            Key::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                ViewAction::Continue
            }
            Key::Home => {
                self.cursor = 0;
                ViewAction::Continue
            }
            Key::End => {
                self.move_to(usize::MAX);
                ViewAction::Continue
            }
            Key::Enter => self.open(),
            Key::Char('g') => {
                self.relist();
                ViewAction::Continue
            }
            // The press selects; opening is the pending action, so the
            // selection is shown first.
            Key::Click(index) => {
                if index < self.entries.len() {
                    self.cursor = index;
                    self.pending_click = true;
                }
                ViewAction::Continue
            }
            _ => ViewAction::Continue,
        }
    }

    fn take_pending_action(&mut self) -> Option<ViewAction> {
        if std::mem::take(&mut self.pending_click) {
            Some(self.open())
        } else {
            None
        }
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }

    fn on_reveal(&mut self) -> bool {
        self.relist();
        true
    }
}

/// A sent draft as it was retired, read-only.
pub struct SentView {
    key: String,
    title: String,
    text: String,
}

impl SentView {
    /// The sent draft at `path`; one past the pane's ceiling is refused,
    /// as the compose view refuses a draft, rather than shown short.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let bytes = crate::submit::read_bounded(path, td_editor::text::MAX_FILE_BYTES)?;
        if bytes.len() > td_editor::text::MAX_FILE_BYTES {
            return Err(std::io::Error::other(format!(
                "the sent draft is larger than the pane's ceiling of {} bytes",
                td_editor::text::MAX_FILE_BYTES
            )));
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(SentView {
            key: format!("sent:{}", path.display()),
            title: format!("Sent {name}"),
            text: crate::ui::frame::pane_source(&String::from_utf8_lossy(&bytes)),
        })
    }
}

const SENT_LABELS: &[&str] = &["Back"];
const SENT_KEYS: &[Key] = &[Key::Escape];

impl View for SentView {
    fn scene(&self) -> Scene<'_> {
        Scene {
            title: self.title.clone(),
            labels: SENT_LABELS,
            keys: SENT_KEYS,
            entry: None,
            body: Body::Text {
                key: self.key.clone(),
                text: Box::new(|columns| wrap_text(&self.text, columns)),
            },
            status: "Sent, as retired | q:back j/k:scroll".to_string(),
        }
    }

    fn handle_key(&mut self, key: Key, _page: usize) -> ViewAction {
        if let Some(scroll) = text_scroll(key) {
            return ViewAction::Scroll(scroll);
        }
        match key {
            Key::Char('q') | Key::Escape => ViewAction::Pop,
            Key::Char('n') => ViewAction::Scroll(Scroll::Lines(1)),
            Key::Char('p') => ViewAction::Scroll(Scroll::Lines(-1)),
            _ => ViewAction::Continue,
        }
    }

    fn on_response(&mut self, _response: &BackendResponse) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drafts first and sent after, each newest first; only regular
    /// files named as td-mail names drafts; headers read to the
    /// separator, folded ones joined and controls blanked.
    #[test]
    fn retained_drafts_are_listed_drafts_first_newest_first() {
        let root = crate::testing::tempdir().unwrap();
        let drafts = root.path().join("td-mail/drafts");
        let sent = root.path().join("td-mail/sent");
        std::fs::create_dir_all(&drafts).unwrap();
        std::fs::create_dir_all(&sent).unwrap();
        let write = |path: &Path, text: &str, age: u64| {
            std::fs::write(path, text).unwrap();
            let when = SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        write(
            &drafts.join("td-mail-draft-1.eml"),
            "To: a@x\nSubject: old\n--text follows this line--\nSubject: body\n",
            300,
        );
        write(
            &drafts.join("td-mail-draft-2.eml"),
            "Subject: new\n  folded\u{7}on\nTo: b@x,\n c@x\n--text follows this line--\n",
            10,
        );
        write(
            &sent.join("td-mail-draft-3.eml"),
            "Subject: gone\n\nbody\n",
            5,
        );
        write(&drafts.join("notes.txt"), "Subject: no\n", 1);
        write(&drafts.join("td-mail-draft-1.eml.lost"), "x", 1);
        std::fs::create_dir(drafts.join("td-mail-att-2")).unwrap();
        std::fs::create_dir(drafts.join("td-mail-draft-6.eml")).unwrap();
        std::os::unix::fs::symlink(
            drafts.join("td-mail-draft-1.eml"),
            drafts.join("td-mail-draft-5.eml"),
        )
        .unwrap();

        let listing = list_retained(&drafts);
        assert!(!listing.cut && listing.unread.is_empty(), "{listing:?}");
        let seen: Vec<(&str, bool, &str, &str, bool)> = listing
            .entries
            .iter()
            .map(|e| {
                (
                    e.path.file_name().unwrap().to_str().unwrap(),
                    e.sent,
                    e.subject.as_str(),
                    e.to.as_str(),
                    e.unsettled,
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (
                    "td-mail-draft-2.eml",
                    false,
                    "new folded on",
                    "b@x, c@x",
                    false
                ),
                ("td-mail-draft-1.eml", false, "old", "a@x", true),
                ("td-mail-draft-3.eml", true, "gone", "", false),
            ]
        );
        assert_eq!(
            list_retained(&root.path().join("none/drafts")),
            Listing::default()
        );
        assert_eq!(shown(&"x".repeat(200)).chars().count(), SHOWN_CHARS + 1);
        // What is not a regular file once opened shows no headers.
        assert_eq!(
            headers(&drafts.join("td-mail-draft-6.eml")),
            (String::new(), String::new())
        );
    }

    /// Each directory keeps its newest `ENTRIES`, so a full drafts
    /// directory leaves the sent ones listed; a `sent` is listed only
    /// beside a directory named `drafts`; a directory that cannot be read
    /// is named rather than listed as empty.
    #[test]
    fn each_directory_keeps_its_newest_and_an_unreadable_one_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::testing::tempdir().unwrap();
        let drafts = root.path().join("td-mail/drafts");
        let sent = root.path().join("td-mail/sent");
        std::fs::create_dir_all(&drafts).unwrap();
        std::fs::create_dir_all(&sent).unwrap();
        for n in 0..=ENTRIES {
            std::fs::write(drafts.join(format!("td-mail-draft-{n}.eml")), "").unwrap();
        }
        std::fs::write(sent.join("td-mail-draft-s.eml"), "Subject: went\n").unwrap();
        let listing = list_retained(&drafts);
        assert!(listing.cut);
        assert_eq!(listing.entries.len(), ENTRIES + 1);
        assert_eq!(listing.entries.iter().filter(|e| e.sent).count(), 1);

        let elsewhere = root.path().join("td-mail/other");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("td-mail-draft-o.eml"), "").unwrap();
        let listing = list_retained(&elsewhere);
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.entries.iter().all(|e| !e.sent), "no sent beside it");

        std::fs::set_permissions(&sent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listing = list_retained(&drafts);
        std::fs::set_permissions(&sent, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A privileged test runner reads it all the same.
        if listing.entries.iter().all(|e| !e.sent) {
            assert_eq!(listing.unread.len(), 1, "{:?}", listing.unread);
            assert!(listing.unread[0].starts_with(&sent.display().to_string()));
        }
    }
}
