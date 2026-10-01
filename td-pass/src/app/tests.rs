#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use crate::protocol::Role;
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
                Command::Unlock { op, .. } | Command::Create { op } | Command::Apply { op, .. },
            ) => Some(*op),
            _ => None,
        })
        .expect("an operation")
}

/// An app unlocked with the primary key over `entries`, its PIN typed
/// at the prompt.
fn unlocked(board: &mut Board, entries: Vec<Item>) -> App {
    let mut app = App::new().unwrap();
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
    app.reply(Reply::Unlocked { op, entries });
    assert!(matches!(app.phase, Phase::Unlocked(_)));
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
    let mut app = App::new().unwrap();
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
    let mut app = App::new().unwrap();
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
    assert_eq!(after, layout::dialog(surface)[0]);
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
    let mut app = App::new().unwrap();
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
    let centre = layout::dialog(app.surface)[0];
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
