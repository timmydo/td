//! The editor core's policy for a document whose text is its own, as a
//! vault entry's is: no filling, copy and cut of the selection alone,
//! find, line endings kept, and a lock that forgets everything.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "asserted test fixtures"
)]

use td_ui::editor::{Controller, Event, Outcome};
use td_ui::editor_clipboard::{Paste, Snapshot};
use td_ui::editor_dialog::{Close, Scope};
use td_ui::editor_error::Error;
use td_ui::editor_keys::Profile;
use td_ui::editor_model::{Command, Selection};
use td_ui::editor_search::{Found, History};
use td_ui::editor_text::LineEnding;

fn pane(text: &str) -> (Controller, u64) {
    let mut ui = Controller::pane().unwrap();
    ui.dispatch(Event::Load(text.as_bytes())).unwrap();
    let tab = ui.editor().active().unwrap();
    (ui, tab)
}

fn revision(ui: &Controller, tab: u64) -> u64 {
    ui.editor().document(tab).unwrap().revision()
}

fn edit(ui: &mut Controller, tab: u64, command: Command) -> Result<Outcome, Error> {
    let revision = revision(ui, tab);
    ui.dispatch(Event::Edit {
        tab,
        revision,
        command,
    })
}

fn select(ui: &mut Controller, tab: u64, anchor: usize, caret: usize) {
    edit(ui, tab, Command::Select(Selection { anchor, caret })).unwrap();
}

#[test]
fn a_document_filling_may_not_rewrap_refuses_fill_and_ignores_its_key() {
    let long = "word ".repeat(40);
    let (mut ui, tab) = pane(&long);
    edit(&mut ui, tab, Command::AutoFill(true)).unwrap();
    assert!(ui.editor().document(tab).unwrap().auto_fill());
    assert_eq!(
        ui.dispatch(Event::Fillable {
            tab,
            enabled: false
        }),
        Ok(Outcome::Changed)
    );
    let doc = ui.editor().document(tab).unwrap();
    assert!(!doc.fillable() && !doc.auto_fill());
    assert_eq!(
        ui.dispatch(Event::Fillable {
            tab,
            enabled: false
        }),
        Ok(Outcome::Ignored)
    );

    assert_eq!(
        edit(&mut ui, tab, Command::FillParagraph),
        Err(Error::Unavailable)
    );
    assert_eq!(
        edit(&mut ui, tab, Command::AutoFill(true)),
        Err(Error::Unavailable)
    );
    edit(&mut ui, tab, Command::AutoFill(false)).unwrap();

    // Emacs's fill chord is ignored like an edit on a read-only document:
    // no change, no generation.
    ui.dispatch(Event::Profile(Profile::Emacs)).unwrap();
    let generation = ui.generation();
    let revision_before = revision(&ui, tab);
    let key = ui.dispatch(Event::Key {
        tab,
        revision: revision_before,
        chord: "M-q",
    });
    assert_eq!(key, Ok(Outcome::Ignored));
    assert_eq!(ui.generation(), generation);
    assert_eq!(revision(&ui, tab), revision_before);
    assert_eq!(ui.editor().document(tab).unwrap().text(), long);

    // Typing past the fill column inserts no line break.
    select(&mut ui, tab, long.len(), long.len());
    for c in "more words ".chars() {
        edit(&mut ui, tab, Command::Type(c)).unwrap();
    }
    assert!(!ui.editor().document(tab).unwrap().text().contains('\n'));

    // Filling comes back only when the host allows it again, and the same
    // chord then fills: the guard, not the keymap, ignored it above.
    ui.dispatch(Event::Fillable { tab, enabled: true }).unwrap();
    let revision_now = revision(&ui, tab);
    let key = ui.dispatch(Event::Key {
        tab,
        revision: revision_now,
        chord: "M-q",
    });
    assert_eq!(key, Ok(Outcome::Changed));
    assert!(ui.editor().document(tab).unwrap().text().contains('\n'));
}

#[test]
fn selection_capture_takes_exactly_the_selection_and_never_the_line() {
    let (mut ui, tab) = pane("first line\nsecond line\n");
    select(&mut ui, tab, 3, 3);
    let at = revision(&ui, tab);
    assert!(Snapshot::capture_selection(ui.editor(), tab, at)
        .unwrap()
        .is_none());
    // The editor's own copy takes the caret's line when nothing is selected.
    let line = Snapshot::capture(ui.editor(), tab, at).unwrap().unwrap();
    assert!(line.whole_line());
    assert_eq!(&*line.text(), "first line\n");

    select(&mut ui, tab, 6, 17);
    let at = revision(&ui, tab);
    let selected = Snapshot::capture_selection(ui.editor(), tab, at)
        .unwrap()
        .unwrap();
    assert!(!selected.whole_line());
    assert_eq!(&*selected.text(), "line\nsecond");
    // Cut deletes those bytes and no more.
    ui.dispatch(Event::Cut(selected)).unwrap();
    assert_eq!(ui.editor().document(tab).unwrap().text(), "first  line\n");
}

#[test]
fn line_endings_are_kept_and_a_paste_is_whole_or_refused() {
    let (mut ui, tab) = pane("user\r\npass\r\n");
    let doc = ui.editor().document(tab).unwrap();
    assert_eq!(doc.text(), "user\npass\n");
    assert_eq!(doc.format().ending, LineEnding::CrLf);
    let (_, bytes) = ui.editor().save_snapshot(tab).unwrap();
    assert_eq!(bytes, b"user\r\npass\r\n");

    // A copy carries the stored bytes, CRLF and all; the line copy the
    // editor's own window uses keeps the model's LF.
    select(&mut ui, tab, 0, 10);
    let at = revision(&ui, tab);
    let copied = Snapshot::capture_selection(ui.editor(), tab, at)
        .unwrap()
        .unwrap();
    assert_eq!(copied.text().as_bytes(), b"user\r\npass\r\n");
    select(&mut ui, tab, 0, 0);
    let at = revision(&ui, tab);
    let line = Snapshot::capture(ui.editor(), tab, at).unwrap().unwrap();
    assert_eq!(&*line.text(), "user\n");

    select(&mut ui, tab, 10, 10);
    let mut paste = Paste::begin(ui.editor(), tab, revision(&ui, tab)).unwrap();
    paste.push(b"note\r\n").unwrap();
    ui.dispatch(Event::Paste(paste)).unwrap();
    let (_, bytes) = ui.editor().save_snapshot(tab).unwrap();
    assert_eq!(bytes, b"user\r\npass\r\nnote\r\n");

    // A lone carriage return is refused whole; nothing is inserted.
    let before = revision(&ui, tab);
    let mut paste = Paste::begin(ui.editor(), tab, before).unwrap();
    paste.push(b"a\rb").unwrap();
    assert_eq!(ui.dispatch(Event::Paste(paste)), Err(Error::InvalidText));
    assert_eq!(revision(&ui, tab), before);
}

#[test]
fn find_in_a_pane_stops_at_the_end_before_it_wraps() {
    let (mut ui, tab) = pane("pin 1234\nrecovery pin 5678\n");
    let mut history = History::default();
    let revision = revision(&ui, tab);
    assert_eq!(
        history.find(&mut ui, tab, revision, "pin", false),
        Ok(Found::Match)
    );
    assert_eq!(ui.editor().document(tab).unwrap().selection().range(), 0..3);
    assert_eq!(
        history.find(&mut ui, tab, revision, "pin", false),
        Ok(Found::Match)
    );
    assert_eq!(
        history.find(&mut ui, tab, revision, "pin", false),
        Ok(Found::End)
    );
    assert_eq!(
        history.find(&mut ui, tab, revision, "pin", false),
        Ok(Found::Wrapped)
    );
    assert!(!ui.editor().document(tab).unwrap().dirty());
}

#[test]
fn clear_forgets_every_document_and_stales_what_was_held() {
    let (mut ui, first) = pane("secret one");
    edit(&mut ui, first, Command::Insert("typed ".into())).unwrap();
    edit(&mut ui, first, Command::Undo).unwrap();
    ui.dispatch(Event::Load(b"secret two")).unwrap();
    let second = ui.editor().active().unwrap();
    edit(&mut ui, second, Command::Insert("x".into())).unwrap();
    let point = ui
        .editor()
        .revision_point(second, revision(&ui, second))
        .unwrap();
    let close = Close::new(ui.editor(), Scope::Window).unwrap();
    let generation = ui.generation();

    assert_eq!(ui.dispatch(Event::Clear), Ok(Outcome::Changed));
    assert_eq!(ui.editor().tabs().count(), 0);
    assert_eq!(ui.editor().active(), None);
    assert_eq!(ui.generation(), generation + 1);
    assert!(ui.tab_view(first).is_err() && ui.tab_view(second).is_err());
    // Nothing held across the lock still names a document.
    assert!(ui.editor().check_revision(&point).is_err());
    assert!(close.next(ui.editor()).is_err());
    // A second lock with nothing loaded changes nothing.
    assert_eq!(ui.dispatch(Event::Clear), Ok(Outcome::Ignored));

    // Unlock loads afresh; tab IDs keep counting, so no earlier ID is
    // reused and the new document has no history.
    ui.dispatch(Event::Load(b"secret one")).unwrap();
    let reloaded = ui.editor().active().unwrap();
    assert!(reloaded > second);
    let doc = ui.editor().document(reloaded).unwrap();
    assert_eq!(doc.history_depth(), (0, 0));
    assert!(doc.fillable());
    assert!(ui.editor().check_revision(&point).is_err());
}

#[test]
fn clear_leaves_no_layout_derived_from_the_text_it_forgot() {
    // A window controller numbers its lines, so the gutter's width says
    // how many digits the line count has; a lock must not keep it.
    let mut ui = Controller::default();
    ui.dispatch(Event::Load("line\n".repeat(12_000).as_bytes()))
        .unwrap();
    let tab = ui.editor().active().unwrap();
    ui.dispatch(Event::Wrap {
        tab,
        revision: revision(&ui, tab),
        enabled: false,
    })
    .unwrap();
    let fresh = Controller::default().geometry();
    assert_ne!(ui.geometry(), fresh);
    ui.dispatch(Event::Clear).unwrap();
    assert_eq!(ui.geometry(), fresh);
}
