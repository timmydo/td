use td_editor::keys::{Action, Keymap, Profile};
use td_editor::model::{Command, Editor, Limits, Motion, Selection};
use td_editor::{fill, replay, text, Error};

#[allow(
    clippy::unwrap_used,
    reason = "test helper: valid fixture commands must succeed"
)]
fn apply(editor: &mut Editor, id: u64, command: Command) {
    let revision = editor.document(id).unwrap().revision();
    editor.dispatch(id, revision, command).unwrap();
}
fn select(editor: &mut Editor, id: u64, anchor: usize, caret: usize) {
    apply(editor, id, Command::Select(Selection { anchor, caret }));
}

#[test]
fn codec_roundtrips_bom_line_endings_and_missing_final_newline() {
    for original in [
        b"".as_slice(),
        b"a\n",
        b"a\r\nb",
        b"\xef\xbb\xbf",
        "\u{feff}naïve\r\nλ".as_bytes(),
        "a\u{feff}b".as_bytes(),
    ] {
        let decoded = text::decode(original).unwrap();
        assert_eq!(
            text::encode(&decoded.text, decoded.format).unwrap(),
            original
        );
    }
    for bad in [
        b"\xff".as_slice(),
        b"a\r",
        b"a\r\nb\nc",
        b"\x1b",
        b"\0",
        b"\x7f",
        "\u{feff}\u{feff}a".as_bytes(),
    ] {
        assert_eq!(text::decode(bad), Err(Error::InvalidText));
    }
}

#[test]
fn scalar_deletion_and_selection_undo_preserve_utf8() {
    let mut e = Editor::default();
    let id = e.load_bytes("aéλ\nz".as_bytes()).unwrap();
    select(&mut e, id, 5, 1);
    apply(&mut e, id, Command::Insert("猫".into()));
    assert_eq!(e.document(id).unwrap().text(), "a猫\nz");
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: 4,
            caret: 4
        }
    );
    apply(&mut e, id, Command::Undo);
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: 5,
            caret: 1
        }
    );
    assert!(!e.document(id).unwrap().dirty());
    select(&mut e, id, 5, 5);
    apply(&mut e, id, Command::Backspace);
    assert_eq!(e.document(id).unwrap().text(), "aé\nz");
    apply(&mut e, id, Command::Delete);
    assert_eq!(e.document(id).unwrap().text(), "aéz");
}

#[test]
fn bad_commands_leave_selection_revision_history_and_text_unchanged() {
    let mut e = Editor::default();
    let id = e.load_bytes("éhello".as_bytes()).unwrap();
    for command in [
        Command::Select(Selection {
            anchor: 1,
            caret: 0,
        }),
        Command::Insert("a\0b".into()),
        Command::FillColumn(0),
    ] {
        assert!(e.dispatch(id, 0, command).is_err());
        let d = e.document(id).unwrap();
        assert_eq!(d.text(), "éhello");
        assert_eq!(d.selection(), Selection::default());
        assert_eq!(d.revision(), 0);
        assert_eq!(d.history_depth(), (0, 0));
    }
    assert_eq!(
        e.dispatch(id, 1, Command::Delete),
        Err(Error::StaleRevision)
    );
    assert_eq!(e.dispatch(100, 0, Command::Delete), Err(Error::MissingTab));
}

#[test]
fn a_deletion_cannot_expose_an_interior_bom_as_file_metadata() {
    let mut e = Editor::default();
    let id = e.load_bytes("a\u{feff}b".as_bytes()).unwrap();
    assert_eq!(e.dispatch(id, 0, Command::Delete), Err(Error::InvalidText));
    assert_eq!(e.document(id).unwrap().text(), "a\u{feff}b");
    assert_eq!(e.document(id).unwrap().revision(), 0);
}

#[test]
fn snapshots_acknowledge_the_written_state_and_undo_restores_it() {
    let mut e = Editor::default();
    let id = e.new_tab().unwrap();
    apply(&mut e, id, Command::Insert("first".into()));
    let (point, bytes) = e.save_snapshot(id).unwrap();
    apply(&mut e, id, Command::Insert(" second".into()));
    e.acknowledge_saved(point).unwrap();
    assert_eq!(bytes, b"first");
    assert!(e.document(id).unwrap().dirty());
    assert_eq!(e.close_tab(id, 2), Err(Error::Dirty));
    apply(&mut e, id, Command::Undo);
    assert!(!e.document(id).unwrap().dirty());
    assert_eq!(e.document(id).unwrap().revision(), 3);
    apply(&mut e, id, Command::Redo);
    assert!(e.document(id).unwrap().dirty());
    apply(&mut e, id, Command::Undo);
    apply(&mut e, id, Command::Insert("!".into()));
    assert_eq!(e.document(id).unwrap().history_depth(), (2, 0));
    apply(&mut e, id, Command::Redo);
    assert_eq!(e.document(id).unwrap().text(), "first!");
}

#[test]
fn save_acknowledgements_belong_to_the_editor_that_captured_them() {
    let mut a = Editor::default();
    let mut b = Editor::default();
    let a_id = a.new_tab().unwrap();
    let b_id = b.new_tab().unwrap();
    apply(&mut a, a_id, Command::Insert("saved elsewhere".into()));
    apply(&mut b, b_id, Command::Insert("not saved".into()));
    let (point, _) = a.save_snapshot(a_id).unwrap();
    assert_eq!(b.acknowledge_saved(point), Err(Error::InvalidArgument));
    assert!(b.document(b_id).unwrap().dirty());
}

#[test]
fn wrapped_find_searches_from_the_selection_before_crossing_the_end() {
    let mut e = Editor::default();
    let id = e.load_bytes("é x é x é".as_bytes()).unwrap();
    for (backward, expected) in [
        (false, 0),
        (false, 5),
        (false, 10),
        (false, 0),
        (true, 10),
        (true, 5),
        (true, 0),
        (true, 10),
    ] {
        apply(
            &mut e,
            id,
            Command::Find {
                needle: "é".into(),
                backward,
                wrap: true,
            },
        );
        assert_eq!(
            e.document(id).unwrap().selection(),
            Selection {
                anchor: expected,
                caret: expected + 2
            }
        );
    }
    assert_eq!(e.document(id).unwrap().revision(), 0);
}

#[test]
fn replacing_a_small_span_does_not_charge_unchanged_text_to_history() {
    let mut e = Editor::with_limits(Limits {
        history_bytes: 4,
        ..Limits::default()
    })
    .unwrap();
    let original = format!("{}é{}", "p".repeat(100), "s".repeat(100));
    let id = e.load_bytes(original.as_bytes()).unwrap();
    apply(
        &mut e,
        id,
        Command::ReplaceAll {
            needle: "é".into(),
            replacement: "λ".into(),
        },
    );
    assert_eq!(e.history_usage(), (4, 1));
    apply(&mut e, id, Command::Undo);
    assert_eq!(e.document(id).unwrap().text(), original);
    apply(&mut e, id, Command::Redo);
    assert!(e.document(id).unwrap().text().contains('λ'));
}

#[test]
fn auto_fill_replaces_multiline_selection_and_undo_restores_all_removed_bytes() {
    let original = format!(
        "one two three four five {}six seven eight",
        "deleted\n".repeat(100)
    );
    let start = "one two three four five ".len();
    let end = original.len() - "six seven eight".len();
    let mut e = Editor::default();
    let id = e.load_bytes(original.as_bytes()).unwrap();
    apply(&mut e, id, Command::AutoFill(true));
    apply(&mut e, id, Command::FillColumn(20));
    select(&mut e, id, start, end);
    apply(&mut e, id, Command::Type(' '));
    assert_eq!(
        e.document(id).unwrap().text(),
        "one two three four\nfive  six seven\neight"
    );
    apply(&mut e, id, Command::Undo);
    assert_eq!(e.document(id).unwrap().text(), original);
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: start,
            caret: end
        }
    );
}

#[test]
fn evicting_an_old_undo_does_not_discard_independent_redo() {
    let mut e = Editor::with_limits(Limits {
        transactions: 3,
        ..Limits::default()
    })
    .unwrap();
    let a = e.new_tab().unwrap();
    let b = e.new_tab().unwrap();
    apply(&mut e, a, Command::Type('a'));
    apply(&mut e, a, Command::Type('b'));
    apply(&mut e, a, Command::Undo);
    apply(&mut e, b, Command::Type('c'));
    apply(&mut e, b, Command::Type('d'));
    assert_eq!(e.document(a).unwrap().history_depth(), (0, 1));
    apply(&mut e, a, Command::Redo);
    assert_eq!(e.document(a).unwrap().text(), "ab");
}

#[test]
fn auto_fill_preserves_interior_separators_and_maps_every_caret() {
    for original in [
        "aaaaa bbbb cccc dddd",
        "  alpha  beta gamma delta epsilon",
        "\talpha\tbeta gamma delta",
        "é 猫  alpha beta gamma delta",
    ] {
        for at in original
            .char_indices()
            .map(|(i, _)| i)
            .chain([original.len()])
        {
            for separator in [' ', '\t'] {
                let mut e = Editor::default();
                let id = e.load_bytes(original.as_bytes()).unwrap();
                apply(&mut e, id, Command::AutoFill(true));
                apply(&mut e, id, Command::FillColumn(20));
                select(&mut e, id, at, at);
                apply(&mut e, id, Command::Type(separator));
                let d = e.document(id).unwrap();
                assert!(d.text().len() > original.len());
                assert_eq!(d.revision(), 1);
                assert!(d.text().is_char_boundary(d.selection().caret));
                apply(&mut e, id, Command::Undo);
                assert_eq!(e.document(id).unwrap().text(), original);
                assert_eq!(
                    e.document(id).unwrap().selection(),
                    Selection {
                        anchor: at,
                        caret: at
                    }
                );
            }
        }
    }
    let edit = fill::auto_fill("aaaaa bbbb cccc dddd", 5..5, ' ', 20).unwrap();
    assert_eq!(edit.insert, "aaaaa  bbbb cccc\ndddd");
    let source = "alpha  beta gamma delta epsilon";
    let edit = fill::auto_fill(source, source.len()..source.len(), ' ', 20).unwrap();
    assert_eq!(edit.insert, "alpha  beta gamma\ndelta epsilon ");
}

#[test]
fn pristine_new_tabs_can_close_and_undo_returns_to_clean() {
    let mut s = replay::Session::default();
    for request in 1..=100 {
        let id = s.editor.new_tab().unwrap();
        assert!(!s.editor.document(id).unwrap().dirty());
        let close = format!("1\t{request}\tclose-tab\t{id}\t0");
        assert!(!s.request(close.as_bytes()).contains("error"));
    }
    let id = s.editor.new_tab().unwrap();
    apply(&mut s.editor, id, Command::Type('x'));
    assert!(s.editor.document(id).unwrap().dirty());
    apply(&mut s.editor, id, Command::Undo);
    assert!(!s.editor.document(id).unwrap().dirty());
}

#[test]
fn windows_cancel_keeps_selection_while_emacs_cancel_clears_it() {
    let mut s = replay::Session::default();
    let id = s.editor.load_bytes(b"abc").unwrap();
    select(&mut s.editor, id, 0, 2);
    assert!(!s
        .request(b"1\t1\tkey\t1\t0\t457363617065")
        .contains("error"));
    assert_eq!(
        s.editor.document(id).unwrap().selection(),
        Selection {
            anchor: 0,
            caret: 2
        }
    );
    s.request(b"1\t2\tset-key-profile\temacs");
    assert!(!s.request(b"1\t3\tkey\t1\t0\t432d67").contains("error"));
    assert_eq!(
        s.editor.document(id).unwrap().selection(),
        Selection {
            anchor: 2,
            caret: 2
        }
    );
}

#[test]
fn tab_ids_are_not_reused_and_tab_state_is_independent() {
    let mut e = Editor::default();
    let a = e.load_bytes(b"one").unwrap();
    let b = e.load_bytes(b"two").unwrap();
    apply(&mut e, a, Command::FillColumn(20));
    apply(&mut e, a, Command::AutoFill(true));
    select(&mut e, a, 1, 2);
    e.next_tab(false).unwrap();
    assert_eq!(e.active(), Some(a));
    e.close_tab(b, 0).unwrap();
    let c = e.load_bytes(b"three").unwrap();
    assert!(c > b);
    assert_eq!(
        e.document(a).unwrap().selection(),
        Selection {
            anchor: 1,
            caret: 2
        }
    );
    assert_eq!(e.document(c).unwrap().fill_column(), 72);
    assert!(!e.document(c).unwrap().auto_fill());
    assert_eq!(e.document(a).unwrap().text(), "one");
}

#[test]
fn encoded_and_global_limits_admit_transactions_atomically() {
    let mut e = Editor::with_limits(Limits {
        file_bytes: 8,
        text_bytes: 10,
        tabs: 2,
        ..Limits::default()
    })
    .unwrap();
    let a = e.load_bytes(b"a\r\nb").unwrap();
    select(&mut e, a, 3, 3);
    apply(&mut e, a, Command::Insert("\r\nx".into()));
    assert_eq!(e.save_snapshot(a).unwrap().1, b"a\r\nb\r\nx");
    assert_eq!(
        e.dispatch(a, 1, Command::Insert("\n".into())),
        Err(Error::Limit)
    );
    let b = e.load_bytes(b"12345").unwrap();
    assert_eq!(e.new_tab(), Err(Error::Limit));
    assert_eq!(
        e.dispatch(b, 0, Command::Insert("x".into())),
        Err(Error::Limit)
    );
    assert_eq!(e.document(b).unwrap().revision(), 0);
    assert_eq!(e.total_text_bytes(), 10);
}

#[test]
fn undo_that_would_exceed_global_budget_is_not_consumed() {
    let mut e = Editor::with_limits(Limits {
        text_bytes: 6,
        ..Limits::default()
    })
    .unwrap();
    let a = e.load_bytes(b"abcdef").unwrap();
    select(&mut e, a, 0, 6);
    apply(&mut e, a, Command::Delete);
    let b = e.load_bytes(b"123456").unwrap();
    assert_eq!(e.dispatch(a, 1, Command::Undo), Err(Error::Limit));
    assert_eq!(e.document(a).unwrap().history_depth(), (1, 0));
    e.close_tab(b, 0).unwrap();
    apply(&mut e, a, Command::Undo);
    assert_eq!(e.document(a).unwrap().text(), "abcdef");
}

#[test]
fn global_history_eviction_keeps_live_state_and_removes_oldest_whole_edits() {
    let mut e = Editor::with_limits(Limits {
        transactions: 2,
        history_bytes: 4,
        ..Limits::default()
    })
    .unwrap();
    let a = e.load_bytes(b"").unwrap();
    let b = e.load_bytes(b"").unwrap();
    apply(&mut e, a, Command::Insert("a".into()));
    apply(&mut e, b, Command::Insert("bb".into()));
    apply(&mut e, a, Command::Insert("c".into()));
    assert_eq!(e.history_usage(), (3, 2));
    apply(&mut e, a, Command::Undo);
    apply(&mut e, a, Command::Undo);
    assert_eq!(e.document(a).unwrap().text(), "a");
    assert!(e.document(a).unwrap().dirty());
    apply(&mut e, b, Command::Insert("dd".into()));
    assert!(e.history_usage().0 <= 4);
    assert!(e.history_usage().1 <= 2);
}

#[test]
fn fill_preserves_boundaries_indentation_and_word_relative_selection() {
    let original = "outside\n\n  alpha beta gamma delta epsilon\n  zeta eta theta\n\nend";
    let mut e = Editor::default();
    let id = e.load_bytes(original.as_bytes()).unwrap();
    let anchor = original.find("epsilon").unwrap() + 2;
    let caret = original.find("beta").unwrap() + 1;
    select(&mut e, id, anchor, caret);
    apply(&mut e, id, Command::FillColumn(20));
    apply(&mut e, id, Command::FillParagraph);
    let expected = "outside\n\n  alpha beta gamma\n  delta epsilon zeta\n  eta theta\n\nend";
    assert_eq!(e.document(id).unwrap().text(), expected);
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: expected.find("epsilon").unwrap() + 2,
            caret: expected.find("beta").unwrap() + 1,
        }
    );
    apply(&mut e, id, Command::FillParagraph);
    assert_eq!(e.document(id).unwrap().revision(), 1);
    assert_eq!(e.document(id).unwrap().history_depth(), (1, 0));
    apply(&mut e, id, Command::Undo);
    assert_eq!(e.document(id).unwrap().text(), original);
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection { anchor, caret }
    );
}

#[test]
fn fill_handles_blank_lines_tabs_and_overlong_words() {
    let blank = fill::paragraph(" \t\n", 0, 1, 20).unwrap();
    assert!(blank.insert.is_empty());
    let result = fill::paragraph("\tlonglonglonglongword foo bar", 1, 1, 20).unwrap();
    assert_eq!(result.insert, "\tlonglonglonglongword\n\tfoo bar");
    assert_eq!(text::column("\tλ\t"), 16);
    let result = fill::paragraph("a b\n  different indent\nc d", 0, 0, 20).unwrap();
    assert_eq!(result.range, 0..3);
}

#[test]
fn fill_keeps_a_caret_at_the_first_word_of_a_continuation_line() {
    let source = "  alpha beta gamma\n  delta epsilon zeta";
    let caret = source.find("delta").unwrap();
    let edit = fill::paragraph(source, caret, caret, 24).unwrap();
    assert_eq!(edit.caret, edit.insert.find("delta").unwrap());
}

#[test]
fn replacing_a_selection_with_identical_text_collapses_without_history() {
    let mut e = Editor::default();
    let id = e.load_bytes(b"abc").unwrap();
    select(&mut e, id, 0, 2);
    apply(&mut e, id, Command::Insert("ab".into()));
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: 2,
            caret: 2
        }
    );
    assert_eq!(e.document(id).unwrap().revision(), 0);
    select(&mut e, id, 0, 2);
    apply(
        &mut e,
        id,
        Command::Move {
            motion: Motion::Left,
            extend: false,
        },
    );
    assert_eq!(e.document(id).unwrap().selection(), Selection::default());
}

#[test]
fn the_executable_replays_without_a_display_and_refuses_editor_invocation() {
    use std::io::Write;
    use std::process::{Command as Process, Stdio};
    let binary = env!("CARGO_BIN_EXE_td-editor");
    let mut child = Process::new(binary)
        .arg("--replay")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = b"1\t1\tnew";
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stdin.write_all(payload).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.ends_with(b"1\t1\tok\t1"));
    assert!(output.stderr.is_empty());
    let failure = Process::new(binary).arg("file.txt").output().unwrap();
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("no Wayland UI yet"));
}

#[test]
fn auto_fill_is_one_undo_and_paste_does_not_trigger_it() {
    let mut e = Editor::default();
    let id = e.load_bytes(b"alpha beta gamma delta\nuntouched").unwrap();
    select(&mut e, id, 22, 22);
    apply(&mut e, id, Command::FillColumn(20));
    apply(&mut e, id, Command::AutoFill(true));
    apply(&mut e, id, Command::Type(' '));
    assert_eq!(
        e.document(id).unwrap().text(),
        "alpha beta gamma\ndelta \nuntouched"
    );
    assert_eq!(e.document(id).unwrap().selection().caret, 23);
    apply(&mut e, id, Command::Undo);
    assert_eq!(
        e.document(id).unwrap().text(),
        "alpha beta gamma delta\nuntouched"
    );
    apply(&mut e, id, Command::Insert(" more words ".into()));
    assert_eq!(
        e.document(id).unwrap().text(),
        "alpha beta gamma delta more words \nuntouched"
    );
}

#[test]
fn auto_fill_does_not_duplicate_an_indentation_only_line() {
    let text = "                        ";
    let result = fill::auto_fill(text, text.len()..text.len(), ' ', 20).unwrap();
    let mut output = text.to_string();
    output.replace_range(result.range, &result.insert);
    assert_eq!(output, format!("{text} "));
    assert_eq!(result.caret, output.len());
}

#[test]
fn fill_maps_every_unicode_endpoint_to_a_boundary_and_roundtrips() {
    let source = "\tαα    beta  猫dog\n\tmore words words  \n\n";
    for (point, _) in source
        .char_indices()
        .chain(std::iter::once((source.len(), '\0')))
    {
        let mut e = Editor::default();
        let id = e.load_bytes(source.as_bytes()).unwrap();
        select(&mut e, id, point, 3);
        apply(&mut e, id, Command::FillColumn(20));
        apply(&mut e, id, Command::FillParagraph);
        let doc = e.document(id).unwrap();
        assert!(doc.text().is_char_boundary(doc.selection().anchor));
        assert!(doc.text().is_char_boundary(doc.selection().caret));
        apply(&mut e, id, Command::Undo);
        assert_eq!(e.document(id).unwrap().text(), source);
    }
}

#[test]
fn search_failure_does_not_move_and_replace_all_is_nonoverlapping_and_undoable() {
    let mut e = Editor::default();
    let id = e.load_bytes(b"aaaa").unwrap();
    select(&mut e, id, 1, 3);
    assert_eq!(
        e.dispatch(
            id,
            0,
            Command::Find {
                needle: "x".into(),
                backward: false,
                wrap: false
            }
        ),
        Err(Error::Unavailable)
    );
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: 1,
            caret: 3
        }
    );
    apply(
        &mut e,
        id,
        Command::ReplaceAll {
            needle: "aa".into(),
            replacement: "b".into(),
        },
    );
    assert_eq!(e.document(id).unwrap().text(), "bb");
    apply(&mut e, id, Command::Undo);
    assert_eq!(e.document(id).unwrap().text(), "aaaa");
    assert_eq!(
        e.document(id).unwrap().selection(),
        Selection {
            anchor: 1,
            caret: 3
        }
    );
}

#[test]
fn logical_key_profiles_conflict_explicitly_and_cancel_prefixes() {
    let mut k = Keymap::default();
    assert!(matches!(
        k.translate("C-x").unwrap(),
        Action::Request("cut")
    ));
    assert!(matches!(k.translate("C-a").unwrap(), Action::SelectAll));
    k.set_profile(Profile::Emacs);
    assert!(matches!(k.translate("C-x").unwrap(), Action::Prefix));
    assert!(matches!(
        k.translate("C-s").unwrap(),
        Action::Request("save")
    ));
    assert!(matches!(
        k.translate("C-a").unwrap(),
        Action::Edit(Command::Move {
            motion: Motion::Home,
            ..
        })
    ));
    k.translate("C-x").unwrap();
    assert!(k.translate("nonsense").is_err());
    assert!(!k.pending());
    k.translate("C-x").unwrap();
    assert!(matches!(k.translate("C-g").unwrap(), Action::Cancel));
    k.translate("C-x").unwrap();
    k.set_profile(Profile::Windows);
    assert!(!k.pending());
    assert!(matches!(
        k.translate("F7").unwrap(),
        Action::Request("check-spelling")
    ));
}

#[test]
fn deterministic_generated_edit_history_matches_a_scalar_vector() {
    let mut e = Editor::default();
    let id = e.load_bytes(b"").unwrap();
    let mut reference = Vec::<char>::new();
    let mut seed = 71u64;
    for _ in 0..1500 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let at = (seed as usize) % (reference.len() + 1);
        let offset = reference.iter().take(at).map(|c| c.len_utf8()).sum();
        select(&mut e, id, offset, offset);
        if seed & 4 == 0 && at < reference.len() {
            reference.remove(at);
            apply(&mut e, id, Command::Delete);
        } else {
            let c = ['a', 'é', '猫', '\n', '\t'][(seed as usize >> 16) % 5];
            reference.insert(at, c);
            apply(&mut e, id, Command::Type(c));
        }
        assert_eq!(
            e.document(id).unwrap().text(),
            reference.iter().collect::<String>()
        );
        let snapshot = e.document(id).unwrap().text().to_string();
        apply(&mut e, id, Command::Undo);
        apply(&mut e, id, Command::Redo);
        assert_eq!(e.document(id).unwrap().text(), snapshot);
    }
}

#[test]
fn replay_rejects_malformed_stale_and_extra_fields_without_edits() {
    let mut session = replay::Session::default();
    assert_eq!(session.request(b"1\t1\tnew"), "1\t1\tok\t1");
    assert_eq!(session.request(b"1\t2\tinsert\t1\t0\tc3a9"), "1\t2\tok\t1");
    for request in [
        b"1\t3\tinsert\t1\t0\t61".as_slice(),
        b"1\t4\tdelete\t1\t1\textra",
        b"1\t5\tinsert\t1\t1\tA1",
        b"1\t6\tinsert\t1\t1\t00",
        b"1\t7\tselect-range\t1\t1\t1\t1",
    ] {
        assert!(session.request(request).contains("\terror\t"));
        assert_eq!(session.editor.document(1).unwrap().text(), "é");
        assert_eq!(session.editor.document(1).unwrap().revision(), 1);
    }
    assert_eq!(
        session.request(b"1\t8\ttext\t1\t1\t0\t4"),
        "1\t8\tok\t2\tc3a9"
    );
}

#[test]
fn replay_emacs_mark_motion_and_typing_use_document_transactions() {
    let mut s = replay::Session::default();
    s.request(b"1\t1\tload\t616263");
    s.request(b"1\t2\tset-key-profile\temacs");
    for key in ["C-Space", "C-f", "C-f", "Z"] {
        let request = format!("1\t3\tkey\t1\t0\t{}", replay::hex(key.as_bytes()));
        assert!(!s.request(request.as_bytes()).contains("error"));
    }
    assert_eq!(s.editor.document(1).unwrap().text(), "Zc");
}

#[test]
fn framed_replay_handles_split_reads_and_rejects_truncation_and_oversize() {
    use std::io::{self, Read};
    struct Bytewise<'a>(&'a [u8]);
    impl Read for Bytewise<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = out.len().min(1);
            self.0.read(&mut out[..count])
        }
    }
    let mut stream = Vec::new();
    for request in [
        b"1\t1\tnew".as_slice(),
        b"1\t2\tinsert\t1\t0\t61",
        b"1\t3\ttext\t1\t1\t0\t4",
    ] {
        stream.extend_from_slice(&(request.len() as u32).to_be_bytes());
        stream.extend_from_slice(request);
    }
    let mut output = Vec::new();
    replay::run(&mut Bytewise(&stream), &mut output).unwrap();
    assert!(output.ends_with(b"1\t3\tok\t1\t61"));
    for bad in [
        vec![0, 0],
        vec![0, 0, 0, 4, b'1'],
        vec![255, 255, 255, 255],
        vec![0, 0, 0, 0],
    ] {
        assert!(replay::run(&mut bad.as_slice(), &mut Vec::new()).is_err());
    }
}
