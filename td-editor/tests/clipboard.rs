#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "asserted fixtures"
)]

use td_editor::clipboard::{Paste, Snapshot, MAX_BYTES};
use td_editor::model::{Command, Selection};
use td_editor::ui::{Controller, Event, Outcome};
use td_editor::Error;

fn loaded(bytes: &[u8]) -> Controller {
    let mut ui = Controller::default();
    ui.dispatch(Event::Load(bytes)).unwrap();
    ui
}
fn target(ui: &Controller) -> (u64, u64) {
    let tab = ui.editor().active().unwrap();
    (tab, ui.editor().document(tab).unwrap().revision())
}
fn edit(ui: &mut Controller, command: Command) {
    let (tab, revision) = target(ui);
    ui.dispatch(Event::Edit {
        tab,
        revision,
        command,
    })
    .unwrap();
}
fn select(ui: &mut Controller, anchor: usize, caret: usize) {
    edit(ui, Command::Select(Selection { anchor, caret }));
}
fn paste(ui: &Controller, bytes: &[u8]) -> Paste {
    let (tab, revision) = target(ui);
    let mut paste = Paste::begin(ui.editor(), tab, revision).unwrap();
    paste.push(bytes).unwrap();
    paste
}
fn snapshot(ui: &Controller) -> Snapshot {
    let (tab, revision) = target(ui);
    Snapshot::capture(ui.editor(), tab, revision)
        .unwrap()
        .unwrap()
}
fn state(ui: &Controller) -> String {
    format!(
        "{:?} {:?} {}",
        ui.editor(),
        ui.tab_view(target(ui).0),
        ui.generation()
    )
}

#[test]
fn copy_is_immutable_and_cut_is_one_undoable_selection_replacement() {
    let mut ui = loaded("aé中z".as_bytes());
    select(&mut ui, 6, 1);
    let before = state(&ui);
    let snapshot = snapshot(&ui);
    let text = snapshot.text();
    assert_eq!(text.as_ref(), "é中");
    assert_eq!(state(&ui), before);
    assert!(!format!("{snapshot:?}").contains("é中"));
    ui.dispatch(Event::Cut(snapshot)).unwrap();
    let (tab, _) = target(&ui);
    let doc = ui.editor().document(tab).unwrap();
    assert_eq!(doc.text(), "az");
    assert_eq!(
        doc.selection(),
        Selection {
            anchor: 1,
            caret: 1
        }
    );
    assert_eq!(doc.history_depth(), (1, 0));
    assert_eq!(text.as_ref(), "é中");
    edit(&mut ui, Command::Undo);
    let doc = ui.editor().document(tab).unwrap();
    assert_eq!(doc.text(), "aé中z");
    assert_eq!(
        doc.selection(),
        Selection {
            anchor: 6,
            caret: 1
        }
    );
    assert!(!doc.dirty());
}

#[test]
fn fragmented_paste_normalizes_crlf_without_changing_file_mode_or_auto_filling() {
    let mut ui = loaded(b"\xef\xbb\xbfa\r\nz");
    select(&mut ui, 2, 3);
    edit(&mut ui, Command::AutoFill(true));
    edit(&mut ui, Command::FillColumn(20));
    let (tab, revision) = target(&ui);
    let format = ui.editor().document(tab).unwrap().format();
    let mut paste = Paste::begin(ui.editor(), tab, revision).unwrap();
    let input = "é中\r\none two three four five six seven ";
    for byte in input.as_bytes() {
        paste.push(std::slice::from_ref(byte)).unwrap();
    }
    assert!(!format!("{paste:?}").contains("one two"));
    ui.dispatch(Event::Paste(paste)).unwrap();
    let doc = ui.editor().document(tab).unwrap();
    assert_eq!(doc.text(), "a\né中\none two three four five six seven ");
    assert_eq!(doc.format(), format);
    assert_eq!(doc.history_depth(), (1, 0));
    edit(&mut ui, Command::Undo);
    assert_eq!(ui.editor().document(tab).unwrap().text(), "a\nz");
    assert!(!ui.editor().document(tab).unwrap().dirty());
}

#[test]
fn empty_copy_empty_paste_and_dropped_partial_transfer_do_not_delete_selection() {
    let mut ui = loaded(b"keep");
    let (tab, revision) = target(&ui);
    assert!(Snapshot::capture(ui.editor(), tab, revision)
        .unwrap()
        .is_none());
    select(&mut ui, 0, 4);
    let before = state(&ui);
    let empty = paste(&ui, b"");
    assert_eq!(ui.dispatch(Event::Paste(empty)).unwrap(), Outcome::Ignored);
    drop(paste(&ui, b"partial"));
    drop(snapshot(&ui));
    assert_eq!(state(&ui), before);
}

#[test]
fn malformed_paste_is_atomic_and_leading_bom_obeys_document_admission() {
    for bytes in [
        &b"\xff"[..],
        b"\xc3",
        b"a\0b",
        b"a\rb",
        b"\x7f",
        b"\x1b",
        b"\xef\xbb\xbf",
    ] {
        let mut ui = loaded(b"keep");
        select(&mut ui, 0, 4);
        let incoming = paste(&ui, bytes);
        let before = state(&ui);
        assert_eq!(ui.dispatch(Event::Paste(incoming)), Err(Error::InvalidText));
        assert_eq!(state(&ui), before);
    }
    let mut ui = loaded(b"ab");
    select(&mut ui, 1, 1);
    let incoming = paste(&ui, b"\xef\xbb\xbf");
    ui.dispatch(Event::Paste(incoming)).unwrap();
    assert_eq!(
        ui.editor().document(target(&ui).0).unwrap().text(),
        "a\u{feff}b"
    );
}

#[test]
fn one_mib_ceiling_counts_wire_bytes_and_overflow_permanently_poisons_the_transfer() {
    let mut ui = loaded(b"");
    let bytes = vec![b'x'; MAX_BYTES];
    let incoming = paste(&ui, &bytes);
    ui.dispatch(Event::Paste(incoming)).unwrap();
    select(&mut ui, 0, MAX_BYTES);
    assert_eq!(snapshot(&ui).text().len(), MAX_BYTES);
    let mut incoming = paste(&ui, &bytes);
    assert_eq!(incoming.push(b"!"), Err(Error::Limit));
    assert_eq!(incoming.push(b""), Err(Error::Limit));
    let before = state(&ui);
    assert_eq!(ui.dispatch(Event::Paste(incoming)), Err(Error::Limit));
    assert_eq!(state(&ui), before);
    select(&mut ui, MAX_BYTES, MAX_BYTES);
    edit(&mut ui, Command::Insert("x".into()));
    select(&mut ui, 0, MAX_BYTES + 1);
    let (tab, revision) = target(&ui);
    assert!(matches!(
        Snapshot::capture(ui.editor(), tab, revision),
        Err(Error::Limit)
    ));
    // CRLF shrinking cannot evade the encoded-input ceiling.
    let mut incoming = paste(&ui, &b"\r\n".repeat(MAX_BYTES / 2));
    assert_eq!(incoming.push(b"\n"), Err(Error::Limit));
}

#[test]
fn changed_revision_selection_active_tab_and_owner_refuse_both_cut_and_paste() {
    for change in 0..5 {
        let mut ui = loaded(b"keep");
        select(&mut ui, 0, 4);
        let incoming = paste(&ui, b"new");
        let snapshot = snapshot(&ui);
        let expected = match change {
            0 => {
                edit(&mut ui, Command::Insert("changed".into()));
                Error::StaleRevision
            }
            1 => {
                select(&mut ui, 0, 3);
                Error::InvalidArgument
            }
            2 => {
                ui.dispatch(Event::New).unwrap();
                Error::InvalidArgument
            }
            3 => {
                ui = loaded(b"keep");
                select(&mut ui, 0, 4);
                Error::InvalidArgument
            }
            _ => {
                edit(&mut ui, Command::Insert("changed".into()));
                edit(&mut ui, Command::Undo);
                Error::StaleRevision
            }
        };
        let before = state(&ui);
        assert_eq!(ui.dispatch(Event::Paste(incoming)), Err(expected));
        assert_eq!(ui.dispatch(Event::Cut(snapshot)), Err(expected));
        assert_eq!(state(&ui), before);
    }
}

#[test]
fn stale_or_inactive_capture_refuses_before_copying_or_allocating_a_transfer() {
    let mut ui = loaded(b"keep");
    let (tab, revision) = target(&ui);
    assert!(matches!(
        Paste::begin(ui.editor(), tab, revision + 1),
        Err(Error::StaleRevision)
    ));
    assert!(matches!(
        Snapshot::capture(ui.editor(), tab, revision + 1),
        Err(Error::StaleRevision)
    ));
    ui.dispatch(Event::New).unwrap();
    assert!(matches!(
        Paste::begin(ui.editor(), tab, revision),
        Err(Error::InvalidArgument)
    ));
    assert!(matches!(
        Snapshot::capture(ui.editor(), tab, revision),
        Err(Error::InvalidArgument)
    ));
}

#[test]
fn cut_cannot_expose_an_initial_bom_and_keeps_the_snapshot_on_refusal() {
    let mut ui = loaded("a\u{feff}b".as_bytes());
    select(&mut ui, 0, 1);
    let snapshot = snapshot(&ui);
    let retained = snapshot.text();
    let before = state(&ui);
    assert_eq!(ui.dispatch(Event::Cut(snapshot)), Err(Error::InvalidText));
    assert_eq!(state(&ui), before);
    assert_eq!(retained.as_ref(), "a");
}

#[test]
fn poison_stays_limit_after_selection_revision_or_target_changes() {
    for change in 0..3 {
        let mut ui = loaded(b"keep");
        let mut incoming = paste(&ui, b"prefix");
        assert_eq!(incoming.push(&vec![b'x'; MAX_BYTES]), Err(Error::Limit));
        match change {
            0 => select(&mut ui, 0, 4),
            1 => edit(&mut ui, Command::Insert("new".into())),
            _ => {
                ui.dispatch(Event::New).unwrap();
            }
        }
        let before = state(&ui);
        assert_eq!(ui.dispatch(Event::Paste(incoming)), Err(Error::Limit));
        assert_eq!(state(&ui), before);
    }
}

#[test]
fn identical_paste_collapses_selection_without_creating_an_undo_transaction() {
    let mut ui = loaded(b"keep");
    select(&mut ui, 0, 4);
    let incoming = paste(&ui, b"keep");
    let (tab, revision) = target(&ui);
    assert_eq!(
        ui.dispatch(Event::Paste(incoming)).unwrap(),
        Outcome::Changed
    );
    let doc = ui.editor().document(tab).unwrap();
    assert_eq!(doc.text(), "keep");
    assert_eq!(
        doc.selection(),
        Selection {
            anchor: 4,
            caret: 4
        }
    );
    assert_eq!(doc.revision(), revision);
    assert_eq!(doc.history_depth(), (0, 0));
    assert!(!doc.dirty());
}

#[test]
fn closed_target_is_missing_and_focus_policy_belongs_to_the_native_adapter() {
    let mut ui = loaded(b"keep");
    select(&mut ui, 0, 4);
    let snapshot = snapshot(&ui);
    let incoming = paste(&ui, b"new");
    let (tab, revision) = target(&ui);
    ui.dispatch(Event::New).unwrap();
    ui.dispatch(Event::Close { tab, revision }).unwrap();
    let before = state(&ui);
    assert_eq!(ui.dispatch(Event::Cut(snapshot)), Err(Error::MissingTab));
    assert_eq!(ui.dispatch(Event::Paste(incoming)), Err(Error::MissingTab));
    assert_eq!(state(&ui), before);
    let incoming = paste(&ui, b"semantic");
    ui.dispatch(Event::Focus(false)).unwrap();
    ui.dispatch(Event::Paste(incoming)).unwrap();
    assert_eq!(
        ui.editor().document(target(&ui).0).unwrap().text(),
        "semantic"
    );
    assert!(!ui.focused());
}
