#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use crate::protocol::Role;
use std::path::PathBuf;
use td_ui::window::Refusal;

/// A clipboard that records what it is offered and answers pastes.
#[derive(Default)]
struct Board {
    copies: Vec<String>,
    pastes: usize,
}

impl Clipboard for Board {
    fn available(&self) -> bool {
        true
    }
    fn has_text(&self) -> bool {
        true
    }
    fn pasting(&self) -> bool {
        false
    }
    fn copy(&mut self, text: Arc<str>) -> Result<(), Refusal> {
        self.copies.push(text.to_string());
        Ok(())
    }
    fn paste(&mut self) -> Result<(), Refusal> {
        self.pastes += 1;
        Ok(())
    }
}

/// A new app whose host's lock is watched, so it may unlock.
fn watched() -> App {
    let mut app = App::new().unwrap();
    app.host(HostEvent::Watched);
    app
}

fn key(app: &mut App, board: &mut Board, chord: &str) {
    app.input(
        Input::Key {
            chord,
            repeat: false,
        },
        board,
    );
}

fn typed(app: &mut App, board: &mut Board, text: &str) {
    for c in text.chars() {
        key(app, board, &c.to_string());
    }
}

fn label(role: Role, fingerprint: &str) -> KeyLabel {
    KeyLabel {
        role,
        fingerprint: fingerprint.to_owned(),
    }
}

fn item(id: u8, title: &str) -> Item {
    Item {
        id: [id; 16],
        revision: 1,
        title: Text::new(title.to_owned()),
    }
}

fn op_of(out: &[Out]) -> Op {
    out.iter()
        .find_map(|out| match out {
            Out::Send(
                Command::Unlock { op, .. }
                | Command::Create { op }
                | Command::Apply { op, .. }
                | Command::UseKey { op, .. }
                | Command::AddKey { op }
                | Command::ReplaceKeys { op, .. }
                | Command::Export { op, .. }
                | Command::ReadCopy { op, .. }
                | Command::Import { op, .. },
            ) => Some(*op),
            _ => None,
        })
        .expect("an operation")
}

/// An app unlocked with the primary key over `entries`, its PIN typed
/// at the prompt.
fn unlocked(board: &mut Board, entries: Vec<Item>) -> App {
    let mut app = watched();
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Open)]));
    app.reply(Reply::Opened {
        keys: Some(vec![
            label(Role::Primary, "0a0b0c0d"),
            label(Role::Backup, "01020304"),
        ]),
    });
    key(&mut app, board, "Return");
    let out = app.take_out();
    assert!(matches!(
        out[..],
        [Out::Send(Command::Unlock { key: 0, .. })]
    ));
    let op = op_of(&out);
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "unlock",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: None,
        },
    });
    key(&mut app, board, "Return");
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Proceed)] if o == op));
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "unlock",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: Some(PinUse::Authorize),
        },
    });
    typed(&mut app, board, "1234");
    key(&mut app, board, "Return");
    match &app.take_out()[..] {
        [Out::Answer(o, Answer::Pin(pin))] => {
            assert_eq!(*o, op);
            assert_eq!(pin.as_slice(), b"1234");
        }
        other => panic!("{other:?}"),
    }
    assert!(app.prompt.is_none());
    app.reply(Reply::Unlocked {
        op,
        entries,
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Unlocked(_)));
    app
}

/// The primary, authorizing saves, and one backup.
fn two_keys() -> Keys {
    Keys {
        labels: vec![
            label(Role::Primary, "0a0b0c0d"),
            label(Role::Backup, "01020304"),
        ],
        using: Some(0),
    }
}

/// Opens the keys view of an unlocked notebook.
fn keys_view(board: &mut Board) -> App {
    let mut app = unlocked(board, vec![item(1, "Bank")]);
    key(&mut app, board, "C-k");
    assert!(notebook(&app).keys.showing);
    assert_eq!(app.focus, Focus::Keys);
    assert_eq!(notebook(&app).keys.list.selected(), Some(0));
    app
}

fn notebook(app: &App) -> &Notebook {
    match &app.phase {
        Phase::Unlocked(notebook) => notebook,
        _ => panic!("locked"),
    }
}

/// Opens the entry the list selects after `chord`, answering its read.
fn open(app: &mut App, board: &mut Board, chord: &str, body: &str) {
    key(app, board, chord);
    let id = match &app.take_out()[..] {
        [Out::Send(Command::Read { id })] => *id,
        other => panic!("{other:?}"),
    };
    let title = notebook(app)
        .entries
        .iter()
        .find(|item| item.id == id)
        .unwrap()
        .title
        .clone();
    app.reply(Reply::Entry {
        id,
        revision: 1,
        title,
        body: Bytes::copy(body.as_bytes()),
    });
}

fn text(app: &App) -> String {
    let (tab, _) = app.open_tab().unwrap();
    app.pane.editor().document(tab).unwrap().text().to_owned()
}

#[test]
fn unlocking_lists_the_titles_and_the_search_filters_them() {
    let mut board = Board::default();
    let mut app = unlocked(
        &mut board,
        vec![item(1, "Bank"), item(2, "Mail"), item(3, "Bike lock")],
    );
    assert_eq!(app.focus, Focus::Search);
    assert_eq!(notebook(&app).shown, [0, 1, 2]);
    typed(&mut app, &mut board, "b");
    assert_eq!(notebook(&app).shown, [0, 2]);
    key(&mut app, &mut board, "Escape");
    assert_eq!(notebook(&app).shown, [0, 1, 2]);
    assert!(app.take_out().is_empty());
}

#[test]
fn an_entry_opens_under_the_vault_policy_and_saves_its_revision() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    assert_eq!(app.focus, Focus::List);
    open(&mut app, &mut board, "Down", "pin 1234\r\nmore\r\n");
    let (tab, _) = app.open_tab().unwrap();
    let document = app.pane.editor().document(tab).unwrap();
    assert!(!document.fillable());
    assert!(!app.dirty());
    key(&mut app, &mut board, "Return");
    assert_eq!(app.focus, Focus::Editor);
    typed(&mut app, &mut board, "x");
    assert!(app.dirty());
    key(&mut app, &mut board, "C-s");
    let out = app.take_out();
    let op = op_of(&out);
    match &out[..] {
        [Out::Send(Command::Apply {
            change:
                Change::Edit {
                    id,
                    base,
                    title,
                    body,
                },
            ..
        })] => {
            assert_eq!((*id, *base), ([1; 16], 1));
            assert_eq!(title.as_str(), "Bank");
            // The entry keeps its own line endings.
            assert_eq!(body.as_str(), "xpin 1234\r\nmore\r\n");
        }
        other => panic!("{other:?}"),
    }
    // An edit while saving stays unsaved after the save.
    typed(&mut app, &mut board, "y");
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    assert_eq!(notebook(&app).open.as_ref().unwrap().revision, 2);
    assert!(app.dirty());
    assert_eq!(app.status, "Saved");
}

#[test]
fn a_failed_save_keeps_the_entry_dirty() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-s");
    let op = op_of(&app.take_out());
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "the entry changed since it was opened; nothing was saved".to_owned(),
            stale: true,
            uncertain: false,
            cancelled: false,
        },
    });
    assert!(app.dirty());
    assert!(app.busy.is_none());
    assert!(app.status.contains("nothing was saved"));
}

#[test]
fn leaving_a_dirty_entry_asks_and_discard_reads_the_next() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "F6");
    assert_eq!(app.focus, Focus::Search);
    key(&mut app, &mut board, "Return");
    key(&mut app, &mut board, "Down");
    assert!(app.dialog.is_some());
    assert!(app.take_out().is_empty());
    // The list keeps the open entry while the choice waits.
    assert_eq!(notebook(&app).list.selected(), Some(0));
    // Cancel, Discard, Save: focus starts on Cancel.
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    assert!(app.dialog.is_none());
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::Read { id: [2, ..] })]
    ));
    assert!(!app.dirty());
}

#[test]
fn copy_and_cut_take_exactly_the_selection() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "one\r\ntwo\r\n");
    key(&mut app, &mut board, "Return");
    // Without a selection a copy takes nothing, not the caret's line.
    key(&mut app, &mut board, "C-c");
    assert!(board.copies.is_empty());
    key(&mut app, &mut board, "C-a");
    key(&mut app, &mut board, "C-c");
    assert_eq!(board.copies, ["one\r\ntwo\r\n"]);
    key(&mut app, &mut board, "C-x");
    assert_eq!(board.copies.len(), 2);
    assert_eq!(text(&app), "");
    key(&mut app, &mut board, "C-v");
    assert_eq!(board.pastes, 1);
    app.input(Input::Paste("pasted\n"), &mut board);
    // Held with LF, saved with the entry's own ending.
    assert_eq!(text(&app), "pasted\n");
    key(&mut app, &mut board, "C-s");
    assert!(matches!(&app.take_out()[..], [Out::Send(Command::Apply {
        change: Change::Edit { body, .. }, ..
    })] if body.as_str() == "pasted\r\n"));
}

#[test]
fn lock_forgets_every_title_body_and_query_and_withdraws_the_clipboard() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    typed(&mut app, &mut board, "ba");
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "secret");
    key(&mut app, &mut board, "C-l");
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    assert!(matches!(app.phase, Phase::Locking));
    assert_eq!(app.pane.editor().tabs().count(), 0);
    assert!(app.take_withdrawal());
    assert!(!app.take_withdrawal());
    assert!(app.take_scrub());
    assert!(!app.take_scrub());
    app.reply(Reply::Locked {
        keys: Some(vec![label(Role::Primary, "0a0b0c0d")]),
    });
    assert!(matches!(app.phase, Phase::Locked { keys: Some(_), .. }));
}

#[test]
fn lock_during_an_operation_cancels_it_and_ignores_its_late_answers() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-s");
    let op = op_of(&app.take_out());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "save",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: None,
        },
    });
    // The key is connected; the save waits on the token.
    key(&mut app, &mut board, "Return");
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Proceed)] if o == op));
    // A dirty entry asks even on lock; a save in flight leaves Discard.
    key(&mut app, &mut board, "C-l");
    let (dialog, _) = app.dialog.as_ref().unwrap();
    assert!(dialog
        .action_rect(confirmations::Focus::Alternate)
        .is_none());
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "save",
            role: Role::Primary,
            key: None,
            pin: None,
        },
    });
    assert!(app.prompt.is_none());
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Decline)] if o == op));
}

#[test]
fn a_new_entry_needs_a_title_and_saves_as_a_creation() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, Vec::new());
    key(&mut app, &mut board, "C-n");
    assert_eq!(app.focus, Focus::Title);
    key(&mut app, &mut board, "C-s");
    assert_eq!(app.status, "An entry needs a title");
    typed(&mut app, &mut board, "Wifi");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "pass");
    key(&mut app, &mut board, "C-s");
    let out = app.take_out();
    let op = op_of(&out);
    match &out[..] {
        [Out::Send(Command::Apply {
            change: Change::Create { title, body },
            ..
        })] => {
            assert_eq!(title.as_str(), "Wifi");
            assert_eq!(body.as_str(), "pass");
        }
        other => panic!("{other:?}"),
    }
    app.reply(Reply::Committed {
        op,
        id: [9; 16],
        revision: Some(1),
    });
    assert_eq!(notebook(&app).entries.len(), 1);
    assert_eq!(notebook(&app).list.selected(), Some(0));
    assert!(!app.dirty());
}

#[test]
fn rename_saves_only_the_title_and_delete_asks_first() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "F2");
    assert_eq!(app.focus, Focus::Title);
    typed(&mut app, &mut board, "Credit union");
    key(&mut app, &mut board, "C-s");
    let out = app.take_out();
    assert!(matches!(&out[..], [Out::Send(Command::Apply {
        change: Change::Rename { title, .. }, ..
    })] if title.as_str() == "Credit union"));
    app.reply(Reply::Committed {
        op: op_of(&out),
        id: [1; 16],
        revision: Some(2),
    });
    assert_eq!(notebook(&app).entries[0].title.as_str(), "Credit union");
    key(&mut app, &mut board, "S-Tab");
    assert_eq!(app.focus, Focus::List);
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_some());
    assert!(app.take_out().is_empty());
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(matches!(
        &out[..],
        [Out::Send(Command::Apply {
            change: Change::Delete {
                id: [1, ..],
                base: 2
            },
            ..
        })]
    ));
    app.reply(Reply::Committed {
        op: op_of(&out),
        id: [1; 16],
        revision: None,
    });
    assert!(notebook(&app).entries.is_empty());
    assert!(app.open_tab().is_none());
}

#[test]
fn declining_the_prompt_answers_decline_and_a_cancel_reads_as_cancelled() {
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Opened { keys: None });
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(matches!(out[..], [Out::Send(Command::Create { .. })]));
    let op = op_of(&out);
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "create",
            role: Role::Primary,
            key: None,
            pin: None,
        },
    });
    key(&mut app, &mut board, "Escape");
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Decline)] if o == op));
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "the operation was cancelled".to_owned(),
            stale: false,
            uncertain: false,
            cancelled: true,
        },
    });
    assert_eq!(app.status, "Cancelled");
    assert!(matches!(app.phase, Phase::Locked { keys: None, .. }));
}

#[test]
fn the_pin_field_is_masked_and_refuses_copy() {
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Opened {
        keys: Some(vec![label(Role::Primary, "0a0b0c0d")]),
    });
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "unlock",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: Some(PinUse::Authorize),
        },
    });
    typed(&mut app, &mut board, "1234");
    key(&mut app, &mut board, "C-a");
    key(&mut app, &mut board, "C-c");
    assert!(board.copies.is_empty());
    let prompt = app.prompt.as_ref().unwrap();
    assert!(prompt.pin.masked());
    assert!(!format!("{:?}", prompt.pin).contains("1234"));
}

#[test]
fn every_phase_paints() {
    let font = td_ui::font::pinned().unwrap();
    let surface = Surface::new(640, 480, td_ui::raster::Scale::default()).unwrap();
    let mut pixels = vec![0u8; 640 * 480 * 4];
    let mut board = Board::default();
    let mut frames = Vec::new();
    let mut paint = |app: &mut App, frames: &mut Vec<Vec<u8>>| {
        let mut raster = Raster::new(&mut pixels, &font, surface, 640 * 4).unwrap();
        app.paint(&mut raster, surface).unwrap();
        assert!(!app.needs_redraw());
        frames.push(pixels.clone());
    };
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    app.input(Input::Resize(surface), &mut board);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    // The notebook with its list, title, pane and find field.
    key(&mut app, &mut board, "C-f");
    paint(&mut app, &mut frames);
    // The keys view, a key marked, and the question before replacing it.
    key(&mut app, &mut board, "Escape");
    key(&mut app, &mut board, "C-k");
    key(&mut app, &mut board, "Space");
    paint(&mut app, &mut frames);
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_some());
    paint(&mut app, &mut frames);
    key(&mut app, &mut board, "Escape");
    key(&mut app, &mut board, "Escape");
    assert!(!notebook(&app).keys.showing);
    key(&mut app, &mut board, "C-f");
    // The unsaved-changes question over it.
    key(&mut app, &mut board, "Escape");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-n");
    assert!(app.dialog.is_some());
    paint(&mut app, &mut frames);
    // Saving from the question brings the PIN prompt.
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "save",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: Some(PinUse::Authorize),
        },
    });
    typed(&mut app, &mut board, "12");
    paint(&mut app, &mut frames);
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "cancelled".to_owned(),
            stale: false,
            uncertain: false,
            cancelled: true,
        },
    });
    key(&mut app, &mut board, "C-l");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    paint(&mut app, &mut frames);
    for keys in [None, Some(vec![label(Role::Backup, "01020304")])] {
        app.reply(Reply::Locked { keys });
        paint(&mut app, &mut frames);
    }
    app.reply(Reply::Refused {
        text: "swap is on".to_owned(),
    });
    paint(&mut app, &mut frames);
    // Each phase drew something of its own.
    for (index, frame) in frames.iter().enumerate() {
        for other in frames.get(index + 1..).unwrap() {
            assert_ne!(frame, other);
        }
    }
}

#[test]
fn a_question_asked_during_a_save_is_asked_again_when_it_ends() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-s");
    let op = op_of(&app.take_out());
    key(&mut app, &mut board, "C-l");
    assert!(app.dialog.is_some());
    // A resize lays the open question out again rather than losing it.
    let surface = Surface::new(700, 500, td_ui::raster::Scale::default()).unwrap();
    let before = app.dialog.as_ref().unwrap().0.rect();
    app.input(Input::Resize(surface), &mut board);
    let after = app.dialog.as_ref().unwrap().0.rect();
    assert_ne!(after, before);
    assert_eq!(after, layout::dialog(surface, layout::DIALOG_ROWS)[0]);
    // The save committed what was asked: nothing is left to discard, and
    // the lock goes ahead.
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    assert!(app.dialog.is_none());
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
}

#[test]
fn discard_leaves_no_edit_on_screen_when_the_next_entry_is_gone() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "F6");
    key(&mut app, &mut board, "Return");
    key(&mut app, &mut board, "Down");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    app.take_out();
    app.reply(Reply::Missing { id: [2; 16] });
    assert!(app.open_tab().is_none());
    assert!(notebook(&app).open.is_none());
    assert_eq!(app.pane.editor().tabs().count(), 0);
}

/// An app with entry 1 of two open, its text edited in the pane, and its
/// save sent: the save's operation.
fn saving(app: &mut App, board: &mut Board) -> Op {
    key(app, board, "Return");
    open(app, board, "Down", "body");
    key(app, board, "Return");
    typed(app, board, "x");
    key(app, board, "C-s");
    op_of(&app.take_out())
}

#[test]
fn new_and_another_entry_wait_for_a_save_in_flight() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    saving(&mut app, &mut board);
    key(&mut app, &mut board, "C-n");
    assert!(app.dialog.is_none());
    assert!(app.status.starts_with("Wait"));
    key(&mut app, &mut board, "S-F6");
    key(&mut app, &mut board, "S-F6");
    assert_eq!(app.focus, Focus::List);
    key(&mut app, &mut board, "Down");
    assert!(app.dialog.is_none());
    assert!(app.take_out().is_empty());
    assert_eq!(notebook(&app).list.selected(), Some(0));
    assert_eq!(text(&app), "xbody");
}

#[test]
fn a_read_answered_after_edits_keeps_the_edited_entry() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Down");
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::Read { id: [2, ..] })]
    ));
    key(&mut app, &mut board, "F6");
    key(&mut app, &mut board, "F6");
    assert_eq!(app.focus, Focus::Editor);
    typed(&mut app, &mut board, "x");
    // No save can start for an entry about to be replaced.
    key(&mut app, &mut board, "C-s");
    assert!(app.take_out().is_empty());
    assert_eq!(app.status, "Wait for the entry to open");
    app.reply(Reply::Entry {
        id: [2; 16],
        revision: 1,
        title: Text::new("Mail".to_owned()),
        body: Bytes::copy(b"other"),
    });
    let open = notebook(&app).open.as_ref().unwrap();
    assert_eq!(open.id, Some([1; 16]));
    assert_eq!(text(&app), "xbody");
    assert_eq!(notebook(&app).list.selected(), Some(0));
}

#[test]
fn save_and_leave_asks_again_about_edits_made_while_saving() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-n");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    typed(&mut app, &mut board, "y");
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    // The later edit is not given up for New without a question.
    assert!(app.dialog.is_some());
    assert_eq!(text(&app), "xybody");
    assert!(app.dirty());
}

#[test]
fn closing_declines_a_waiting_prompt_and_asks_about_unsaved_text() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    let op = saving(&mut app, &mut board);
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "save",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: None,
        },
    });
    app.input(Input::Close, &mut board);
    assert!(app.prompt.is_none());
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Decline)] if o == op));
    assert!(app.dialog.is_some());
    assert!(!app.quitting());
}

#[test]
fn a_prompt_replaces_a_question_it_would_hide() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    let op = saving(&mut app, &mut board);
    key(&mut app, &mut board, "C-l");
    assert!(app.dialog.is_some());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "save",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: Some(PinUse::Authorize),
        },
    });
    assert!(app.dialog.is_none());
    assert!(app.prompt.is_some());
    // The save fails: the lock asked meanwhile is asked again.
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "cancelled".to_owned(),
            stale: false,
            uncertain: false,
            cancelled: true,
        },
    });
    assert_eq!(app.dialog.as_ref().unwrap().1, Some(Then::Lock));
}

#[test]
fn a_quit_asked_during_save_and_new_is_not_replaced_by_new() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-n");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    typed(&mut app, &mut board, "y");
    app.input(Input::Close, &mut board);
    assert_eq!(app.dialog.as_ref().unwrap().1, Some(Then::Quit));
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    assert_eq!(app.dialog.as_ref().unwrap().1, Some(Then::Quit));
    assert!(!app.quitting());
}

#[test]
fn no_delete_starts_while_another_entry_is_read() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Down");
    app.take_out();
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_none());
    assert_eq!(app.status, "Wait for the entry to open");
}

#[test]
fn a_paste_goes_only_where_it_was_asked() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    key(&mut app, &mut board, "C-v");
    assert_eq!(board.pastes, 1);
    // The entry is replaced before the clipboard answers.
    key(&mut app, &mut board, "C-n");
    app.input(Input::Paste("late"), &mut board);
    assert_eq!(text(&app), "");
    // A PIN paste ends with its prompt.
    app.reply(Reply::Locked { keys: None });
    let mut app = watched();
    app.reply(Reply::Opened {
        keys: Some(vec![label(Role::Primary, "0a0b0c0d")]),
    });
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    let ask = |pin| Ask {
        operation: "unlock",
        role: Role::Primary,
        key: Some("0a0b0c0d".to_owned()),
        pin,
    };
    app.reply(Reply::Ask {
        op,
        ask: ask(Some(PinUse::Authorize)),
    });
    key(&mut app, &mut board, "C-v");
    assert_eq!(app.paste, Some(Target::Pin));
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "no key".to_owned(),
            stale: false,
            uncertain: false,
            cancelled: false,
        },
    });
    assert_eq!(app.paste, None);
}

#[test]
fn a_question_opened_by_the_pointer_keeps_its_actions_away_from_it() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    let centre = layout::dialog(app.surface, layout::DIALOG_ROWS)[0];
    let details = ["This entry has changes that are not saved."];
    let probe = Model::new("Unsaved changes", "Save", &details, Act::Save, 0)
        .and_then(|model| model.with_alternate("Discard", Act::Discard))
        .and_then(|model| Dialog::new(model, app.surface, centre, None))
        .unwrap();
    for focus in [
        confirmations::Focus::Confirm,
        confirmations::Focus::Alternate,
    ] {
        let action = probe.action_rect(focus).unwrap();
        let at = (
            action.x + i64::from(action.width) / 2,
            action.y + i64::from(action.height) / 2,
        );
        app.request(Then::Lock, Some(at));
        let (dialog, _) = app.dialog.take().unwrap();
        assert_ne!(dialog.rect(), centre, "moved away from {focus:?}");
        for action in [
            confirmations::Focus::Confirm,
            confirmations::Focus::Alternate,
        ] {
            assert!(!dialog.action_rect(action).unwrap().contains(at.0, at.1));
        }
    }
    // Asked from the keys, the question takes the centre.
    app.request(Then::Lock, None);
    assert_eq!(app.dialog.as_ref().unwrap().0.rect(), centre);
}

#[test]
fn search_folds_case_without_copies() {
    assert!(matches_folded("Bank Ölbaum", "ölb"));
    assert!(matches_folded("MAIL", "ail"));
    assert!(matches_folded("abc", ""));
    assert!(!matches_folded("ab", "abc"));
    assert!(!matches_folded("", "a"));
}

#[test]
fn the_keys_view_lists_the_keys_and_use_chooses_the_key_for_saves() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    // The key already authorizing saves asks nothing.
    key(&mut app, &mut board, "Return");
    assert!(app.take_out().is_empty());
    key(&mut app, &mut board, "Down");
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(matches!(
        out[..],
        [Out::Send(Command::UseKey { key: 1, .. })]
    ));
    let op = op_of(&out);
    // A key operation in flight holds the next.
    key(&mut app, &mut board, "Return");
    assert!(app.take_out().is_empty());
    app.reply(Reply::Keys {
        op,
        keys: Keys {
            using: Some(1),
            ..two_keys()
        },
    });
    assert!(app.busy.is_none());
    assert_eq!(notebook(&app).keys.keys.using, Some(1));
    assert_eq!(
        app.status,
        "Saves are now authorized by the backup key 01020304"
    );
    // A late or foreign answer changes nothing.
    app.reply(Reply::Keys {
        op,
        keys: two_keys(),
    });
    assert_eq!(notebook(&app).keys.keys.using, Some(1));
    key(&mut app, &mut board, "Escape");
    assert!(!notebook(&app).keys.showing);
    // Back to the field focused before the view.
    assert_eq!(app.focus, Focus::Search);
}

#[test]
fn adding_a_key_goes_through_the_prompt_and_lists_the_new_key() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "Insert");
    let out = app.take_out();
    assert!(matches!(out[..], [Out::Send(Command::AddKey { .. })]));
    let op = op_of(&out);
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "add a key",
            role: Role::Backup,
            key: None,
            pin: Some(PinUse::Enroll),
        },
    });
    assert!(app.prompt.is_some());
    typed(&mut app, &mut board, "5678");
    key(&mut app, &mut board, "Return");
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Pin(_))] if o == op));
    let mut keys = two_keys();
    keys.labels.push(label(Role::Backup, "0f0f0f0f"));
    app.reply(Reply::Keys { op, keys });
    assert!(app.prompt.is_none());
    assert_eq!(notebook(&app).keys.keys.labels.len(), 3);
    // A narrow prompt row cuts the operation's words, not the key.
    assert!(asking(&Ask {
        operation: "authorize key replacement with a retained key",
        role: Role::Backup,
        key: Some("01020304".to_owned()),
        pin: None,
    })
    .starts_with("Connect the backup key 01020304, to "));
    assert_eq!(notebook(&app).keys.marked, vec![false; 3]);
    assert_eq!(app.status, "Added the backup key 0f0f0f0f");
}

#[test]
fn replacing_asks_first_revokes_the_marked_keys_and_keeps_one() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    // Every key marked leaves none to authorize the replacement.
    key(&mut app, &mut board, "Space");
    key(&mut app, &mut board, "Down");
    key(&mut app, &mut board, "Space");
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_none());
    assert!(app.status.starts_with("Keep at least one key"));
    // The backup unmarked, the primary alone is revoked, after the question.
    key(&mut app, &mut board, "Space");
    assert_eq!(notebook(&app).keys.marked, vec![true, false]);
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_some());
    assert!(app.take_out().is_empty());
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(
        matches!(&out[..], [Out::Send(Command::ReplaceKeys { revoked, .. })] if *revoked == [0]),
        "{out:?}"
    );
    let op = op_of(&out);
    app.reply(Reply::Keys {
        op,
        keys: Keys {
            labels: vec![
                label(Role::Backup, "01020304"),
                label(Role::Primary, "0e0e0e0e"),
            ],
            using: Some(1),
        },
    });
    assert_eq!(notebook(&app).keys.marked, vec![false, false]);
    assert!(app
        .status
        .starts_with("Replaced: the new primary key is 0e0e0e0e"));
    // With nothing marked, the selected key is the one asked about.
    key(&mut app, &mut board, "Home");
    key(&mut app, &mut board, "Delete");
    assert!(app.dialog.is_some());
    key(&mut app, &mut board, "Escape");
    assert!(app.dialog.is_none());
    assert!(app.take_out().is_empty());
}

#[test]
fn lock_during_a_key_operation_cancels_it_and_keeps_no_view() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "Insert");
    let op = op_of(&app.take_out());
    key(&mut app, &mut board, "C-l");
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    app.reply(Reply::Keys {
        op,
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Locking));
    app.reply(Reply::Locked {
        keys: Some(two_keys().labels),
    });
    assert!(matches!(app.phase, Phase::Locked { .. }));
}

#[test]
fn the_keys_view_keeps_the_open_entrys_unsaved_edits() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    // A paste asked for the pane is dropped when the view comes up.
    key(&mut app, &mut board, "C-v");
    key(&mut app, &mut board, "C-k");
    assert!(notebook(&app).keys.showing);
    app.input(Input::Paste("pasted"), &mut board);
    // The editor does not take keys while the view is up.
    typed(&mut app, &mut board, "y");
    key(&mut app, &mut board, "Escape");
    assert!(app.dirty());
    assert_eq!(text(&app), "xbody");
    // The focus goes back where it was.
    assert_eq!(app.focus, Focus::Editor);
}

/// Presses at `(x, y)`, with Shift when `extend`.
fn press(app: &mut App, board: &mut Board, (x, y): (i64, i64), extend: bool) {
    for phase in [PointerPhase::Press, PointerPhase::Release] {
        app.input(
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                follow: false,
            },
            board,
        );
    }
}

fn click_button(app: &mut App, board: &mut Board, labels: &'static [&'static str], index: usize) {
    let at = button(app, labels, index);
    press(app, board, at, false);
}

fn click_row(app: &mut App, board: &mut Board, index: usize, extend: bool) {
    let at = key_row(app, index);
    press(app, board, at, extend);
}

/// A point on the strip's button `index`.
fn button(app: &App, labels: &'static [&'static str], index: usize) -> (i64, i64) {
    let strip = layout::strip(app.surface, labels);
    let rect = strip.rect();
    let y = rect.y + i64::from(rect.height) / 2;
    let x = (rect.x..rect.x + i64::from(rect.width))
        .find(|&x| strip.hit(x, y) == Some(index))
        .expect("the button is on the strip");
    (x, y)
}

/// A point on the keys view's row `index`.
fn key_row(app: &App, index: usize) -> (i64, i64) {
    let list = layout::enrolled(app.surface).unwrap().rect();
    let row = layout::row(app.surface);
    (
        list.x + 4 * layout::cell(app.surface),
        list.y + row * index as i64 + row / 2,
    )
}

#[test]
fn the_strips_buttons_reach_the_keys_view_and_its_actions() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    click_button(&mut app, &mut board, &layout::NOTEBOOK, 5);
    assert!(notebook(&app).keys.showing);
    // Shift and a press marks a row; a press alone selects it.
    click_row(&mut app, &mut board, 1, true);
    assert_eq!(notebook(&app).keys.marked, vec![false, true]);
    assert_eq!(notebook(&app).keys.list.selected(), Some(1));
    click_row(&mut app, &mut board, 0, false);
    assert_eq!(notebook(&app).keys.marked, vec![false, true]);
    // Replace asks about the marked key, not the selected one.
    click_button(&mut app, &mut board, &layout::KEYS, 3);
    assert!(app.dialog.is_some());
    key(&mut app, &mut board, "Escape");
    assert!(app.take_out().is_empty());
    // Use for saves on the selected primary, which already authorizes.
    click_button(&mut app, &mut board, &layout::KEYS, 1);
    assert!(app.take_out().is_empty());
    click_row(&mut app, &mut board, 1, false);
    click_button(&mut app, &mut board, &layout::KEYS, 1);
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::UseKey { key: 1, .. })]
    ));
    app.busy = None;
    click_button(&mut app, &mut board, &layout::KEYS, 2);
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::AddKey { .. })]
    ));
    app.busy = None;
    click_button(&mut app, &mut board, &layout::KEYS, 0);
    assert!(!notebook(&app).keys.showing);
    assert_eq!(notebook(&app).keys.marked, vec![false, false]);
    click_button(&mut app, &mut board, &layout::NOTEBOOK, 6);
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
}

#[test]
fn a_held_space_marks_once_and_the_hint_does_not_outlive_the_view() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    assert_eq!(app.status, KEYS_HINT);
    key(&mut app, &mut board, "Space");
    app.input(
        Input::Key {
            chord: "Space",
            repeat: true,
        },
        &mut board,
    );
    assert_eq!(notebook(&app).keys.marked, vec![true, false]);
    key(&mut app, &mut board, "Escape");
    assert_eq!(app.status, "");
    // A failure's report stays when the view is put away.
    key(&mut app, &mut board, "C-k");
    key(&mut app, &mut board, "Insert");
    let op = op_of(&app.take_out());
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "the vault store did not answer".to_owned(),
            stale: false,
            uncertain: true,
            cancelled: false,
        },
    });
    let report =
        "the vault store did not answer: lock and unlock again to see what keys the vault holds";
    assert_eq!(app.status, report);
    key(&mut app, &mut board, "Escape");
    assert_eq!(app.status, report);
}

#[test]
fn a_list_laid_out_without_room_takes_its_keys_when_the_window_grows() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    let tiny = Surface::new(40, 30, td_ui::raster::Scale::default()).unwrap();
    app.input(Input::Resize(tiny), &mut board);
    let mut keys = two_keys();
    keys.labels.push(label(Role::Backup, "0f0f0f0f"));
    app.set_keys(keys);
    let roomy = Surface::new(800, 600, td_ui::raster::Scale::default()).unwrap();
    app.input(Input::Resize(roomy), &mut board);
    assert_eq!(notebook(&app).keys.list.count(), 3);
    // The locked list as well.
    app.input(Input::Resize(tiny), &mut board);
    key(&mut app, &mut board, "C-l");
    app.reply(Reply::Locked {
        keys: Some(two_keys().labels),
    });
    app.input(Input::Resize(roomy), &mut board);
    let Phase::Locked { list, .. } = &app.phase else {
        panic!("locked");
    };
    assert_eq!(list.count(), 2);
}

#[test]
fn an_operation_ending_under_the_keys_view_moves_the_focus_it_returns() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    assert_eq!(app.focus, Focus::Editor);
    app.delete(None);
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    key(&mut app, &mut board, "C-k");
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: None,
    });
    assert_eq!(app.focus, Focus::Keys, "the view keeps the keys");
    key(&mut app, &mut board, "Escape");
    assert_eq!(app.focus, Focus::List, "not the closed entry's pane");
}

/// A listing of `folders` then `files`, each file enabled.
fn listing(path: &str, folders: &[&str], files: &[&str]) -> finder::Listing {
    let entries = folders
        .iter()
        .map(|name| finder::Entry::new(name, "", finder::Kind::Folder, true).unwrap())
        .chain(
            files
                .iter()
                .map(|name| finder::Entry::new(name, "1 KiB", finder::Kind::File, true).unwrap()),
        )
        .collect();
    finder::Listing::new(path, entries, false).unwrap()
}

/// The open finder's number, for its listings.
fn chooser_id(app: &App) -> u64 {
    app.chooser.as_ref().map_or(0, |chooser| chooser.id)
}

/// The one listing the app asks for, as (folder, select, files).
fn asked(app: &mut App) -> (Option<PathBuf>, Option<String>, bool) {
    match app.take_out().into_iter().next() {
        Some(Out::List {
            folder,
            select,
            files,
            ..
        }) => (folder, select, files),
        other => panic!("{other:?}"),
    }
}

#[test]
fn export_writes_into_the_folder_the_finder_accepts() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "C-e");
    assert_eq!(asked(&mut app), (None, None, false));
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u"),
        Ok(listing("/home/u", &["docs"], &["notes.txt"])),
        None,
    );
    assert!(app.chooser.as_ref().unwrap().finder.is_some());
    // Return opens the selected folder; Alt+Up goes back, the folder
    // left selected.
    key(&mut app, &mut board, "Return");
    assert_eq!(
        asked(&mut app),
        (Some(PathBuf::from("/home/u/docs")), None, false)
    );
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u/docs"),
        Ok(listing("/home/u/docs", &[], &[])),
        None,
    );
    key(&mut app, &mut board, "M-Up");
    assert_eq!(
        asked(&mut app),
        (
            Some(PathBuf::from("/home/u")),
            Some("docs".to_owned()),
            false
        )
    );
    // A folder that cannot be read is noted and the finder stays.
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u"),
        Err("denied".to_owned()),
        None,
    );
    assert!(app.chooser.is_some());
    // Ctrl+Return exports into the listed folder.
    key(&mut app, &mut board, "C-Return");
    assert!(app.chooser.is_none());
    let out = app.take_out();
    let op = match &out[..] {
        [Out::Send(Command::Export { op, folder })] => {
            assert_eq!(folder, &PathBuf::from("/home/u/docs"));
            *op
        }
        other => panic!("{other:?}"),
    };
    app.reply(Reply::Exported {
        op,
        path: "/home/u/docs/td-pass-notebook-r3.tdpass".to_owned(),
    });
    assert!(app.busy.is_none());
    assert_eq!(
        app.status,
        "Exported an encrypted copy to /home/u/docs/td-pass-notebook-r3.tdpass"
    );
}

/// An app whose account holds no notebook.
fn empty() -> App {
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Opened { keys: None });
    app
}

#[test]
fn import_reads_the_chosen_copy_then_places_it_with_one_of_its_keys() {
    let mut board = Board::default();
    let mut app = empty();
    key(&mut app, &mut board, "C-o");
    assert_eq!(asked(&mut app), (None, None, true));
    app.listed(
        chooser_id(&app),
        PathBuf::from("/media/usb"),
        Ok(listing("/media/usb", &[], &["copy.tdpass"])),
        None,
    );
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    let op = match &out[..] {
        [Out::Send(Command::ReadCopy { op, path })] => {
            assert_eq!(path, &PathBuf::from("/media/usb/copy.tdpass"));
            *op
        }
        other => panic!("{other:?}"),
    };
    app.reply(Reply::Copy {
        op,
        keys: two_keys().labels,
    });
    assert!(matches!(app.phase, Phase::Importing { .. }));
    key(&mut app, &mut board, "Down");
    key(&mut app, &mut board, "Return");
    let out = app.take_out();
    assert!(matches!(
        out[..],
        [Out::Send(Command::Import { key: 1, .. })]
    ));
    let op = op_of(&out);
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "import a portable vault copy into this empty location",
            role: Role::Backup,
            key: Some("01020304".to_owned()),
            pin: Some(PinUse::Authorize),
        },
    });
    typed(&mut app, &mut board, "1234");
    key(&mut app, &mut board, "Return");
    assert!(matches!(app.take_out()[..], [Out::Answer(o, Answer::Pin(_))] if o == op));
    app.reply(Reply::Unlocked {
        op,
        entries: vec![item(1, "Bank")],
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Unlocked(_)));
    assert_eq!(notebook(&app).entries.len(), 1);
}

#[test]
fn a_copy_given_up_or_unread_leaves_the_account_without_a_notebook() {
    let mut board = Board::default();
    let mut app = empty();
    // Escape before any listing closes the finder.
    key(&mut app, &mut board, "C-o");
    app.take_out();
    key(&mut app, &mut board, "Escape");
    assert!(app.chooser.is_none());
    // A start folder that cannot be read closes it with the reason.
    key(&mut app, &mut board, "C-o");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/"),
        Err("/: denied".to_owned()),
        None,
    );
    assert!(app.chooser.is_none());
    assert_eq!(app.status, "/: denied");
    // A copy that cannot be read is reported; the account stays empty.
    key(&mut app, &mut board, "C-o");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/m"),
        Ok(listing("/m", &[], &["bad"])),
        None,
    );
    key(&mut app, &mut board, "Return");
    let op = op_of_read(&app.take_out());
    app.reply(Reply::Failed {
        op,
        failure: Failure {
            text: "not a portable vault".to_owned(),
            stale: false,
            uncertain: false,
            cancelled: false,
        },
    });
    assert!(matches!(app.phase, Phase::Locked { keys: None, .. }));
    assert_eq!(app.status, "not a portable vault");
    // A copy read and then given up goes back to the empty account.
    key(&mut app, &mut board, "C-o");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/m"),
        Ok(listing("/m", &[], &["good"])),
        None,
    );
    key(&mut app, &mut board, "Return");
    let op = op_of_read(&app.take_out());
    app.reply(Reply::Copy {
        op,
        keys: two_keys().labels,
    });
    key(&mut app, &mut board, "Escape");
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    app.reply(Reply::Locked { keys: None });
    assert!(matches!(app.phase, Phase::Locked { keys: None, .. }));
    // With a notebook, Import is not offered.
    app.reply(Reply::Locked {
        keys: Some(two_keys().labels),
    });
    key(&mut app, &mut board, "C-o");
    assert!(app.chooser.is_none());
    assert!(app.take_out().is_empty());
}

fn op_of_read(out: &[Out]) -> Op {
    match out {
        [Out::Send(Command::ReadCopy { op, .. })] => *op,
        other => panic!("{other:?}"),
    }
}

#[test]
fn lock_closes_the_finder_and_the_finder_paints() {
    let font = td_ui::font::pinned().unwrap();
    let surface = Surface::new(640, 480, td_ui::raster::Scale::default()).unwrap();
    let mut pixels = vec![0u8; 640 * 480 * 4];
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    app.input(Input::Resize(surface), &mut board);
    let mut paint = |app: &mut App| {
        let mut raster = Raster::new(&mut pixels, &font, surface, 640 * 4).unwrap();
        app.paint(&mut raster, surface).unwrap();
        pixels.clone()
    };
    let view = paint(&mut app);
    key(&mut app, &mut board, "C-e");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u"),
        Ok(listing("/home/u", &["docs"], &[])),
        None,
    );
    let chooser = paint(&mut app);
    assert_ne!(view, chooser);
    // The finder takes typed characters as its filter, not the view's.
    key(&mut app, &mut board, "d");
    assert_eq!(
        app.chooser
            .as_ref()
            .unwrap()
            .finder
            .as_ref()
            .unwrap()
            .query(),
        "d"
    );
    // Lock is not held back by the finder.
    key(&mut app, &mut board, "C-l");
    assert!(app.chooser.is_none());
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    // A listing for a finder since closed is dropped, even under a new one.
    let mut app = empty();
    key(&mut app, &mut board, "C-o");
    app.take_out();
    let first = chooser_id(&app);
    key(&mut app, &mut board, "Escape");
    key(&mut app, &mut board, "C-o");
    app.take_out();
    assert_ne!(chooser_id(&app), first);
    app.listed(
        first,
        PathBuf::from("/old"),
        Ok(listing("/old", &[], &["c"])),
        None,
    );
    assert!(app.chooser.as_ref().unwrap().finder.is_none());
    // The copy's keys paint as their own view.
    let mut app = empty();
    app.input(Input::Resize(surface), &mut board);
    let locked = paint(&mut app);
    key(&mut app, &mut board, "C-o");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/m"),
        Ok(listing("/m", &[], &["c"])),
        None,
    );
    key(&mut app, &mut board, "Return");
    let op = op_of_read(&app.take_out());
    app.reply(Reply::Copy {
        op,
        keys: two_keys().labels,
    });
    assert_ne!(paint(&mut app), locked);
}

#[test]
fn the_strips_lock_is_not_held_back_by_the_finder() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "C-e");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u"),
        Ok(listing("/home/u", &["docs"], &[])),
        None,
    );
    // A press inside the finder is the finder's.
    let inside = layout::finder(app.surface, false);
    press(
        &mut app,
        &mut board,
        (inside.x + 8, inside.y + i64::from(inside.height) / 2),
        false,
    );
    assert!(app.chooser.is_some());
    click_button(&mut app, &mut board, &layout::KEYS, 5);
    assert!(app.chooser.is_none());
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
}

#[test]
fn an_export_that_finishes_during_a_lock_is_reported_with_the_locked_view() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "C-e");
    app.take_out();
    app.listed(
        chooser_id(&app),
        PathBuf::from("/home/u"),
        Ok(listing("/home/u", &[], &[])),
        None,
    );
    key(&mut app, &mut board, "C-Return");
    let op = op_of(&app.take_out());
    key(&mut app, &mut board, "C-l");
    // The thread serves the export before the lock.
    app.reply(Reply::Exported {
        op,
        path: "/home/u/td-pass-notebook-r1.tdpass".to_owned(),
    });
    app.reply(Reply::Locked {
        keys: Some(two_keys().labels),
    });
    assert_eq!(
        app.status,
        "Exported an encrypted copy to /home/u/td-pass-notebook-r1.tdpass. \
         Locked: choose a key and press Unlock"
    );
}

fn primary_only() -> Option<Vec<KeyLabel>> {
    Some(vec![label(Role::Primary, "0a0b0c0d")])
}

/// Unlocks a locked app again with its first key; the prompts are not
/// what is tested.
fn unlock_again(app: &mut App, board: &mut Board) {
    key(app, board, "Return");
    let op = op_of(&app.take_out());
    app.reply(Reply::Unlocked {
        op,
        entries: vec![item(1, "Bank")],
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Unlocked(_)));
}

#[test]
fn a_host_lock_asks_nothing_and_the_next_unlock_reports_the_edits() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    assert!(app.dirty());
    assert!(!app.settled());
    app.host(HostEvent::Lock);
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    assert!(app.dialog.is_none());
    assert!(matches!(app.phase, Phase::Locking));
    assert_eq!(app.pane.editor().tabs().count(), 0);
    assert!(app.take_withdrawal());
    assert!(app.take_scrub());
    assert_eq!(app.status, "Locking with the screen");
    // Sleep waits on the thread's answer, not on the window alone.
    assert!(!app.settled());
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert!(app.settled());
    assert_eq!(
        app.status,
        "Locked with the screen: choose a key and press Unlock"
    );
    unlock_again(&mut app, &mut board);
    assert_eq!(
        app.status,
        "Unlocked: 1 entry. The host's lock gave up unsaved edits"
    );
    // Reported once: a lock of the person's own, with nothing unsaved,
    // reports nothing.
    key(&mut app, &mut board, "C-l");
    app.take_out();
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert_eq!(app.status, "Locked: choose a key and press Unlock");
    unlock_again(&mut app, &mut board);
    assert_eq!(app.status, "Unlocked: 1 entry");
}

#[test]
fn sleep_during_a_save_cancels_it_and_the_unlock_says_it_may_not_be_saved() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    let op = saving(&mut app, &mut board);
    app.host(HostEvent::Suspend);
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    assert!(app.busy.is_none());
    // The save's late result changes nothing shown.
    app.reply(Reply::Committed {
        op,
        id: [1; 16],
        revision: Some(2),
    });
    assert!(matches!(app.phase, Phase::Locking));
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert_eq!(
        app.status,
        "Locked for sleep: choose a key and press Unlock"
    );
    // The edits went with the save, which may have committed.
    unlock_again(&mut app, &mut board);
    assert_eq!(
        app.status,
        "Unlocked: 1 entry. The host's lock may have stopped a change"
    );
}

#[test]
fn an_edit_made_after_a_save_was_sent_is_reported_given_up() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    saving(&mut app, &mut board);
    typed(&mut app, &mut board, "y");
    app.host(HostEvent::Lock);
    app.take_out();
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    unlock_again(&mut app, &mut board);
    assert_eq!(
        app.status,
        "Unlocked: 1 entry. The host's lock gave up edits and may have stopped a change"
    );
}

#[test]
fn a_host_lock_during_an_unlock_has_the_thread_lock_as_well() {
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Opened {
        keys: primary_only(),
    });
    // Locked and idle: nothing to do.
    app.host(HostEvent::Lock);
    assert!(app.take_out().is_empty());
    assert!(app.settled());
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "unlock",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: None,
        },
    });
    assert!(app.prompt.is_some());
    app.host(HostEvent::Suspend);
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    assert!(app.prompt.is_none());
    assert!(!app.settled());
    // An unlock that finished as it was cancelled shows nothing; the
    // thread drops its vault on the lock that follows.
    app.reply(Reply::Unlocked {
        op,
        entries: vec![item(1, "Bank")],
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Locking));
    assert!(!app.settled());
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert!(app.settled());
    // Nothing was being edited.
    unlock_again(&mut app, &mut board);
    assert_eq!(app.status, "Unlocked: 1 entry");
}

#[test]
fn nothing_held_has_nothing_to_lock() {
    let mut app = watched();
    app.take_out();
    app.host(HostEvent::Lock);
    assert!(app.take_out().is_empty());
    assert!(app.settled());
    app.reply(Reply::Refused {
        text: "swap is on".to_owned(),
    });
    app.host(HostEvent::Suspend);
    assert!(app.take_out().is_empty());
    assert!(app.settled());
    assert_eq!(app.status, "swap is on");
}

#[test]
fn a_host_that_is_not_watched_is_warned_of_on_the_locked_view() {
    let mut board = Board::default();
    let mut app = App::new().unwrap();
    app.take_out();
    app.host(HostEvent::Unwatched("no system bus".to_owned()));
    assert!(app.unwatched);
    // A weaker warning does not hide it.
    app.host(HostEvent::Undelayed);
    app.reply(Reply::Opened {
        keys: primary_only(),
    });
    assert_eq!(
        app.status,
        "Locked: choose a key and press Unlock. Screen lock not watched: no system bus"
    );
    // Told, so the notebook may be unlocked.
    key(&mut app, &mut board, "Return");
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::Unlock { .. })]
    ));

    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    app.host(HostEvent::Undelayed);
    assert_eq!(
        app.status,
        "Sleep does not wait for the notebook to lock, so it may lock only on waking"
    );
    assert_eq!(
        app.host_warning.as_deref(),
        Some("Sleep may come before the lock")
    );
    assert!(!app.unwatched);
    app.host(HostEvent::Lost("the system bus closed".to_owned()));
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    assert!(app.unwatched);
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert_eq!(
        app.status,
        "Locked as the host's events stopped: choose a key and press Unlock. \
         Screen lock no longer watched: the system bus closed"
    );
}

#[test]
fn unlocking_waits_until_the_host_s_lock_is_watched() {
    let mut board = Board::default();
    let mut app = App::new().unwrap();
    app.take_out();
    app.reply(Reply::Opened {
        keys: primary_only(),
    });
    key(&mut app, &mut board, "Return");
    assert!(app.take_out().is_empty());
    assert_eq!(
        app.status,
        "Starting to watch the host's screen lock: try again in a moment"
    );
    app.host(HostEvent::Watched);
    key(&mut app, &mut board, "Return");
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::Unlock { .. })]
    ));

    // Creation waits as well.
    let mut app = App::new().unwrap();
    app.take_out();
    app.reply(Reply::Opened { keys: None });
    key(&mut app, &mut board, "Return");
    assert!(app.take_out().is_empty());
    app.host(HostEvent::Undelayed);
    key(&mut app, &mut board, "Return");
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::Create { .. })]
    ));
}

#[test]
fn a_host_event_while_locking_sends_no_second_lock() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    key(&mut app, &mut board, "C-l");
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    app.host(HostEvent::Lock);
    app.host(HostEvent::Suspend);
    assert!(app.take_out().is_empty());
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert_eq!(
        app.status,
        "Locked for sleep: choose a key and press Unlock"
    );
    // Locked and idle, a later event leaves the status as it is.
    app.host(HostEvent::Lock);
    assert!(app.take_out().is_empty());
    assert_eq!(
        app.status,
        "Locked for sleep: choose a key and press Unlock"
    );
}

#[test]
fn a_host_lock_during_a_key_change_says_it_may_be_stopped() {
    let mut board = Board::default();
    let mut app = keys_view(&mut board);
    key(&mut app, &mut board, "Insert");
    let op = op_of(&app.take_out());
    app.host(HostEvent::Lock);
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    app.reply(Reply::Keys {
        op,
        keys: two_keys(),
    });
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    unlock_again(&mut app, &mut board);
    assert_eq!(
        app.status,
        "Unlocked: 1 entry. The host's lock may have stopped a change"
    );
}

#[test]
fn a_host_lock_closes_the_question_it_finds() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank"), item(2, "Mail")]);
    key(&mut app, &mut board, "Return");
    open(&mut app, &mut board, "Down", "body");
    key(&mut app, &mut board, "Return");
    typed(&mut app, &mut board, "x");
    key(&mut app, &mut board, "C-l");
    assert!(app.dialog.is_some());
    assert!(app.take_out().is_empty());
    app.host(HostEvent::Suspend);
    assert!(app.dialog.is_none());
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Lock)]));
    assert!(matches!(app.phase, Phase::Locking));
}

#[test]
fn a_host_lock_gives_up_a_copy_being_imported() {
    let mut board = Board::default();
    let mut app = empty();
    key(&mut app, &mut board, "C-o");
    asked(&mut app);
    app.listed(
        chooser_id(&app),
        PathBuf::from("/media/usb"),
        Ok(listing("/media/usb", &[], &["copy.tdpass"])),
        None,
    );
    key(&mut app, &mut board, "Return");
    let op = op_of_read(&app.take_out());
    app.reply(Reply::Copy {
        op,
        keys: two_keys().labels,
    });
    assert!(matches!(app.phase, Phase::Importing { .. }));
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    app.host(HostEvent::Lock);
    let out = app.take_out();
    assert!(
        matches!(out[..], [Out::Cancel, Out::Send(Command::Lock)]),
        "{out:?}"
    );
    // A cancelled import may still have placed the copy.
    app.reply(Reply::Unlocked {
        op,
        entries: vec![item(1, "Bank")],
        keys: two_keys(),
    });
    assert!(matches!(app.phase, Phase::Locking));
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    unlock_again(&mut app, &mut board);
    assert_eq!(
        app.status,
        "Unlocked: 1 entry. The host's lock may have stopped a change"
    );
}

#[test]
fn a_warning_waits_behind_a_prompt() {
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Opened {
        keys: primary_only(),
    });
    key(&mut app, &mut board, "Return");
    let op = op_of(&app.take_out());
    app.reply(Reply::Ask {
        op,
        ask: Ask {
            operation: "unlock",
            role: Role::Primary,
            key: Some("0a0b0c0d".to_owned()),
            pin: None,
        },
    });
    let asking = app.status.clone();
    app.host(HostEvent::Undelayed);
    assert_eq!(app.status, asking);
    assert_eq!(
        app.host_warning.as_deref(),
        Some("Sleep may come before the lock")
    );
}

#[test]
fn a_later_warning_keeps_why_the_notebook_locked() {
    let mut board = Board::default();
    let mut app = unlocked(&mut board, vec![item(1, "Bank")]);
    app.host(HostEvent::Suspend);
    app.take_out();
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    app.host(HostEvent::Undelayed);
    assert_eq!(
        app.status,
        "Locked for sleep: choose a key and press Unlock. Sleep may come before the lock"
    );
    unlock_again(&mut app, &mut board);
    key(&mut app, &mut board, "C-l");
    app.take_out();
    app.reply(Reply::Locked {
        keys: primary_only(),
    });
    assert_eq!(
        app.status,
        "Locked: choose a key and press Unlock. Sleep may come before the lock"
    );
}

#[test]
fn swap_on_storage_is_asked_about_and_opens_only_when_accepted() {
    let mut board = Board::default();
    let mut app = watched();
    assert!(matches!(app.take_out()[..], [Out::Send(Command::Open)]));
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned(), "/swapfile".to_owned()],
    });
    assert!(matches!(&app.phase, Phase::Swap(devices) if devices == &["/dev/sda2", "/swapfile"]));
    assert!(app.dialog.is_some());
    assert!(app.settled());
    // Cancel, Open anyway: focus starts on Cancel, which keeps it closed.
    key(&mut app, &mut board, "Return");
    assert!(app.dialog.is_none());
    assert!(app.take_out().is_empty());
    assert!(matches!(app.phase, Phase::Swap(_)));
    assert!(app.status.contains("stays closed"));
    // Unlock is not offered while the swap stands.
    key(&mut app, &mut board, "C-o");
    assert!(app.take_out().is_empty());
    key(&mut app, &mut board, "Return");
    assert!(app.dialog.is_some());
    key(&mut app, &mut board, "Escape");
    assert!(app.dialog.is_none());
    assert!(app.take_out().is_empty());
    key(&mut app, &mut board, "Return");
    key(&mut app, &mut board, "Tab");
    key(&mut app, &mut board, "Return");
    assert!(matches!(
        app.take_out()[..],
        [Out::Send(Command::AcceptSwap)]
    ));
    assert!(matches!(app.phase, Phase::Opening));
    app.reply(Reply::Opened {
        keys: Some(vec![label(Role::Primary, "0a0b0c0d")]),
    });
    assert!(matches!(app.phase, Phase::Locked { .. }));
}

#[test]
fn the_swap_question_names_the_devices_and_what_they_risk() {
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned()],
    });
    let text = format!("{:?}", app.dialog.as_ref().map(|(dialog, _)| dialog));
    for said in [
        "/dev/sda2",
        "the PIN you type, the key that opens the vault and entry text",
        "stay after td-pass exits",
        "does not erase",
        "full-disk encryption",
        "Hibernation",
        "swapoff -a",
        "zram",
        "this run only",
        "Open anyway",
    ] {
        assert!(text.contains(said), "{said}: {text}");
    }
}

#[test]
fn all_the_swap_question_says_shows_in_an_800_by_600_window() {
    let mut app = watched();
    app.take_out();
    assert_eq!((app.surface.width, app.surface.height), (800, 600));
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned(), "/swapfile".to_owned()],
    });
    let (dialog, _) = app.dialog.as_ref().unwrap();
    let shown = i64::from(dialog.details_rect().height) / layout::row(app.surface);
    assert!(
        dialog.detail_rows() as i64 <= shown,
        "{} rows in {shown}",
        dialog.detail_rows()
    );
}

#[test]
fn any_swap_table_can_be_asked_about() {
    for devices in [
        vec!["/swap\u{7}one".to_owned()],
        vec!["/".repeat(5000)],
        (0..200).map(|n| format!("/swap/{n:0>300}")).collect(),
    ] {
        let mut app = watched();
        app.take_out();
        app.reply(Reply::Swap { devices });
        assert!(app.dialog.is_some(), "{}", app.status);
    }
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Swap {
        devices: vec!["/swap\u{7}one".to_owned()],
    });
    let text = format!("{:?}", app.dialog.as_ref().map(|(dialog, _)| dialog));
    assert!(text.contains(r"/swap\\u{7}one"), "{text}");
    assert_eq!(shown("/swap\\040one"), "/swap\\040one");
    assert_eq!(shown("/a\u{7}\u{a0}b"), "/a\\u{7}\u{a0}b");
    assert_eq!(
        shown(&"x".repeat(600)),
        format!("{}…", "x".repeat(SWAP_NAME))
    );
}

#[test]
fn the_swap_question_opens_away_from_the_pointer() {
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned()],
    });
    let (dialog, _) = app.dialog.as_ref().unwrap();
    let open = dialog.action_rect(confirmations::Focus::Confirm).unwrap();
    let pointer = (open.x + 1, open.y + 1);
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.pointer = Some(pointer);
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned()],
    });
    match app.dialog.as_ref() {
        Some((dialog, _)) => assert!(!dialog
            .action_rect(confirmations::Focus::Confirm)
            .unwrap()
            .contains(pointer.0, pointer.1)),
        None => {
            assert_eq!(
                app.status,
                "No place for the question away from the pointer; use the keys"
            );
            // Asked for with Return, it opens where it fits.
            key(&mut app, &mut board, "Return");
            assert!(app.dialog.is_some());
        }
    }
    assert!(app.take_out().is_empty());
}

#[test]
fn closing_while_swap_is_asked_about_quits_with_nothing_open() {
    let mut board = Board::default();
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned()],
    });
    // The question takes the keys; closing the window answers it.
    app.input(Input::Close, &mut board);
    assert!(app.quitting());
    assert!(!app
        .take_out()
        .iter()
        .any(|out| matches!(out, Out::Send(Command::AcceptSwap))));
    // Declined, Ctrl+Q quits.
    let mut app = watched();
    app.take_out();
    app.reply(Reply::Swap {
        devices: vec!["/dev/sda2".to_owned()],
    });
    key(&mut app, &mut board, "Escape");
    key(&mut app, &mut board, "C-q");
    assert!(app.quitting());
    assert!(app.take_out().is_empty());
}
