use super::*;
use td_ui::raster::Scale;
use td_ui::window::NoClipboard;

fn surface(width: usize, height: usize, scale: u8) -> Surface {
    Surface::new(width, height, Scale::new(scale).unwrap()).unwrap()
}

fn dialog(request: Request) -> Dialog {
    let mut dialog = Dialog::new(request, surface(800, 600, 1)).unwrap();
    dialog.input(Input::Focus(true), &mut NoClipboard);
    dialog
}

fn key(dialog: &mut Dialog, chord: &str) -> Flow {
    dialog.input(
        Input::Key {
            chord,
            repeat: false,
        },
        &mut NoClipboard,
    )
}

fn typed(dialog: &mut Dialog, text: &str) {
    for c in text.chars() {
        assert_eq!(key(dialog, c.encode_utf8(&mut [0; 4])), Flow::Continue);
    }
}

fn text(answer: Option<Answer>) -> String {
    match answer {
        Some(Answer::Text(secret)) => secret.as_str().to_owned(),
        other => panic!("not a text: {other:?}"),
    }
}

/// What the frame shows, read back from its draw stream.
fn shown(dialog: &Dialog) -> String {
    td_ui::driven::text(&Frame { dialog }).unwrap().2
}

fn twice() -> Request {
    Request::new(Kind::Text {
        masked: true,
        repeat: Some("Repeat:".to_owned()),
        repeat_error: "They differ.".to_owned(),
    })
}

fn button(dialog: &Dialog, index: usize) -> Rect {
    let buttons = dialog.buttons();
    let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
    dialog.layout(&labels).buttons.button(index).unwrap().rect()
}

fn click(dialog: &mut Dialog, rect: Rect) -> Flow {
    let (x, y) = (rect.x + 2, rect.y + 2);
    for phase in [PointerPhase::Press, PointerPhase::Release] {
        let flow = dialog.input(
            Input::Pointer {
                phase,
                x,
                y,
                extend: false,
                follow: false,
            },
            &mut NoClipboard,
        );
        if flow == Flow::Quit {
            return flow;
        }
    }
    Flow::Continue
}

#[test]
fn return_answers_what_was_typed_and_clears_the_field() {
    let mut dialog = dialog(Request::secret());
    typed(&mut dialog, "pass word%");
    key(&mut dialog, "Backspace");
    assert_eq!(key(&mut dialog, "Return"), Flow::Quit);
    assert!(dialog.first.text().is_empty());
    assert!(dialog.take_scrub());
    assert_eq!(text(dialog.take_answer()), "pass word");
}

#[test]
fn escape_and_closing_the_window_cancel() {
    let mut escaped = dialog(Request::secret());
    typed(&mut escaped, "x");
    assert_eq!(key(&mut escaped, "Escape"), Flow::Quit);
    assert_eq!(escaped.take_answer(), Some(Answer::Cancelled));
    assert!(escaped.first.text().is_empty());
    let mut closed = dialog(Request::secret());
    assert_eq!(closed.input(Input::Close, &mut NoClipboard), Flow::Quit);
    assert_eq!(closed.take_answer(), Some(Answer::Cancelled));
}

#[test]
fn a_masked_field_shows_no_character_of_the_secret() {
    let mut request = Request::secret();
    request.description = "Unlock the key\nID 1234".to_owned();
    request.prompt = "Passphrase:".to_owned();
    let mut dialog = dialog(request);
    typed(&mut dialog, "zqxjzqxj");
    let frame = shown(&dialog);
    assert!(frame.contains("Unlock the key"), "{frame}");
    assert!(frame.contains("ID 1234"), "{frame}");
    assert!(frame.contains("Passphrase:"), "{frame}");
    assert!(!frame.contains('z') && !frame.contains('q'), "{frame}");
}

#[test]
fn a_visible_field_shows_what_is_typed() {
    let mut dialog = dialog(crate::askpass::request(
        "Username for 'https://example.org': ",
        None,
    ));
    typed(&mut dialog, "someone");
    assert!(shown(&dialog).contains("someone"));
    key(&mut dialog, "Return");
    assert_eq!(text(dialog.take_answer()), "someone");
}

#[test]
fn a_text_typed_twice_must_match() {
    let mut dialog = dialog(twice());
    typed(&mut dialog, "abc");
    // Return in the first field moves to the second.
    assert_eq!(key(&mut dialog, "Return"), Flow::Continue);
    typed(&mut dialog, "abd");
    assert_eq!(key(&mut dialog, "Return"), Flow::Continue);
    assert!(dialog.take_answer().is_none());
    assert!(shown(&dialog).contains("They differ."));
    assert!(dialog.repeat.as_ref().unwrap().text().is_empty());
    assert_eq!(dialog.focus, Field::Repeat);
    typed(&mut dialog, "abc");
    assert_eq!(key(&mut dialog, "Return"), Flow::Quit);
    assert_eq!(text(dialog.take_answer()), "abc");
}

#[test]
fn tab_moves_between_the_two_fields() {
    let mut dialog = dialog(twice());
    key(&mut dialog, "Tab");
    assert_eq!(dialog.focus, Field::Repeat);
    key(&mut dialog, "S-Tab");
    assert_eq!(dialog.focus, Field::First);
    let mut once = self::dialog(Request::secret());
    key(&mut once, "Tab");
    assert_eq!(once.focus, Field::First);
}

#[test]
fn the_request_error_shows_until_replaced() {
    let mut request = Request::secret();
    request.error = Some("Bad passphrase (try 2 of 3)".to_owned());
    let dialog = dialog(request);
    assert!(shown(&dialog).contains("Bad passphrase (try 2 of 3)"));
}

#[test]
fn a_paste_lands_in_the_field_and_a_line_break_is_refused() {
    let mut dialog = dialog(Request::secret());
    dialog.input(Input::Paste("from-clip"), &mut NoClipboard);
    dialog.input(Input::Paste("two\nlines"), &mut NoClipboard);
    assert!(shown(&dialog).contains("Not pasted"));
    key(&mut dialog, "Return");
    assert_eq!(text(dialog.take_answer()), "from-clip");
}

#[test]
fn a_question_is_answered_by_its_buttons() {
    let mut request = Request::new(Kind::Confirm { one_button: false });
    request.not_ok = Some("No".to_owned());
    let mut confirm = dialog(request);
    // OK, No, Cancel, left to right.
    let no = button(&confirm, 1);
    assert_eq!(click(&mut confirm, no), Flow::Quit);
    assert_eq!(confirm.take_answer(), Some(Answer::Declined));
    let mut request = Request::new(Kind::Confirm { one_button: false });
    request.description = "Allow?".to_owned();
    let mut confirm = dialog(request);
    typed(&mut confirm, "ignored");
    let no = button(&confirm, 1);
    assert_eq!(click(&mut confirm, no), Flow::Quit);
    assert_eq!(confirm.take_answer(), Some(Answer::Cancelled));
    let mut confirm = dialog(Request::new(Kind::Confirm { one_button: false }));
    assert_eq!(key(&mut confirm, "Return"), Flow::Quit);
    assert_eq!(confirm.take_answer(), Some(Answer::Confirmed));
}

#[test]
fn a_message_has_one_button() {
    let dialog = dialog(Request::new(Kind::Confirm { one_button: true }));
    assert_eq!(dialog.buttons().len(), 1);
    let frame = shown(&dialog);
    assert!(frame.contains("OK") && !frame.contains("Cancel"), "{frame}");
}

#[test]
fn a_release_off_the_pressed_button_does_nothing() {
    let mut dialog = dialog(Request::secret());
    let ok = button(&dialog, 0);
    let cancel = button(&dialog, 1);
    for (phase, rect) in [(PointerPhase::Press, ok), (PointerPhase::Release, cancel)] {
        let flow = dialog.input(
            Input::Pointer {
                phase,
                x: rect.x + 2,
                y: rect.y + 2,
                extend: false,
                follow: false,
            },
            &mut NoClipboard,
        );
        assert_eq!(flow, Flow::Continue);
    }
    assert!(dialog.take_answer().is_none());
    typed(&mut dialog, "k");
    assert_eq!(click(&mut dialog, ok), Flow::Quit);
    assert_eq!(text(dialog.take_answer()), "k");
}

#[test]
fn the_timeout_gives_up_and_the_caret_blinks() {
    let mut request = Request::secret();
    request.timeout = Some(2);
    let mut dialog = dialog(request);
    assert_eq!(dialog.tick(1000), Flow::Continue);
    assert!(dialog.wait_ms(1000) <= MAX_WAIT_MS);
    let visible = dialog.caret_visible;
    assert_eq!(dialog.tick(1000 + BLINK_MS), Flow::Continue);
    assert_ne!(dialog.caret_visible, visible);
    assert_eq!(dialog.wait_ms(2999), 1);
    assert_eq!(dialog.tick(3000), Flow::Quit);
    assert_eq!(dialog.take_answer(), Some(Answer::TimedOut));
}

#[test]
fn a_caller_hanging_up_answers_nothing() {
    let mut dialog = dialog(Request::secret());
    typed(&mut dialog, "half");
    assert_eq!(dialog.hang_up(), Flow::Quit);
    assert_eq!(dialog.take_answer(), Some(Answer::Hangup));
    assert!(dialog.first.text().is_empty());
}

#[test]
fn every_part_fits_small_and_scaled_surfaces() {
    let mut request = twice();
    request.description = "word ".repeat(400);
    request.error = Some("an error".to_owned());
    request.prompt = "Passphrase:".to_owned();
    for (width, height, scale) in [(800, 600, 1), (320, 200, 1), (1600, 1200, 2), (200, 90, 1)] {
        let mut dialog = Dialog::new(request.clone(), surface(width, height, scale)).unwrap();
        dialog.notice("keyboard: refused");
        let surface = dialog.surface;
        let buttons = dialog.buttons();
        let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
        let layout = dialog.layout(&labels);
        for entry in [layout.first, layout.repeat].into_iter().flatten() {
            assert_eq!(
                entry.rect().intersection(surface.bounds()),
                Some(entry.rect())
            );
        }
        let s = scale as i64;
        let row = ROW as i64 * s;
        let fields_top = [layout.first, layout.repeat]
            .into_iter()
            .flatten()
            .map(|entry| entry.rect().y)
            .min()
            .unwrap_or(i64::MAX);
        let last = layout.description_top + row * layout.description.len() as i64;
        assert!(
            last <= fields_top,
            "{width}x{height}: {last} > {fields_top}"
        );
        if let Some(row_text) = layout.description.last() {
            if layout.description.len() > 1 {
                assert!(row_text.ends_with('…'), "{row_text}");
            }
        }
        // The remark and the error each keep their own row, and a short
        // window gives up the remark before the agent's error.
        if let (Some(remark), Some(error)) = (layout.remark_top, layout.error_top) {
            assert!(remark + row <= error, "{width}x{height}");
            assert!(last <= remark, "{width}x{height}");
        }
        if layout.remark_top.is_some() {
            assert!(layout.error_top.is_some(), "{width}x{height}");
        }
        // Painting is a frame's worth of draws that never fails.
        assert!(td_ui::driven::text(&Frame { dialog: &dialog }).is_ok());
        dialog.input(Input::Resize(surface), &mut NoClipboard);
    }
}

#[test]
fn wrapping_keeps_lines_and_splits_long_words() {
    assert_eq!(wrap("a b\n\nc", 10), vec!["a b", "", "c"]);
    assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
    assert_eq!(wrap("trailing\n", 20), vec!["trailing"]);
    assert_eq!(wrap("a\n  \nb", 10), vec!["a", "", "b"]);
    assert_eq!(wrap("a b\ncd", 0), vec!["a b", "cd"]);
    let mut rows = vec!["aaaa".to_owned(), "bbbb".to_owned(), "cc".to_owned()];
    shorten(&mut rows, 2, 4);
    assert_eq!(rows, vec!["aaaa", "bbb…"]);
}

#[test]
fn a_question_is_answered_from_the_keyboard_too() {
    let question = || {
        let mut request = Request::new(Kind::Confirm { one_button: false });
        request.not_ok = Some("No".to_owned());
        dialog(request)
    };
    // OK, No, Cancel: Right moves to No, Return presses it.
    let mut confirm = question();
    key(&mut confirm, "Right");
    assert_eq!(key(&mut confirm, "Return"), Flow::Quit);
    assert_eq!(confirm.take_answer(), Some(Answer::Declined));
    // Left from OK wraps to Cancel; Tab and S-Tab move as the arrows do.
    let mut confirm = question();
    key(&mut confirm, "Left");
    assert_eq!(key(&mut confirm, "Return"), Flow::Quit);
    assert_eq!(confirm.take_answer(), Some(Answer::Cancelled));
    let mut confirm = question();
    key(&mut confirm, "Tab");
    key(&mut confirm, "Tab");
    key(&mut confirm, "S-Tab");
    key(&mut confirm, "Return");
    assert_eq!(confirm.take_answer(), Some(Answer::Declined));
    // A message's one button is the only one.
    let mut message = dialog(Request::new(Kind::Confirm { one_button: true }));
    key(&mut message, "Right");
    key(&mut message, "Return");
    assert_eq!(message.take_answer(), Some(Answer::Confirmed));
}

#[test]
fn a_not_ok_label_shows_on_a_question_with_cancel_alone() {
    for kind in [Kind::Confirm { one_button: true }, Request::secret().kind] {
        let mut request = Request::new(kind);
        request.not_ok = Some("No".to_owned());
        let dialog = dialog(request);
        assert!(dialog.buttons().iter().all(|(_, c)| *c != Choice::NotOk));
    }
}

#[test]
fn typing_stops_the_timeout() {
    let mut request = Request::secret();
    request.timeout = Some(1);
    let mut dialog = dialog(request);
    dialog.tick(0);
    typed(&mut dialog, "a");
    assert_eq!(dialog.tick(5000), Flow::Continue);
    assert!(dialog.take_answer().is_none());
    // A key that moves the caret without an edit, and a paste that adds
    // nothing, do not stop it.
    let mut request = Request::secret();
    request.timeout = Some(1);
    let mut dialog = self::dialog(request);
    dialog.tick(0);
    dialog.input(Input::Paste(""), &mut NoClipboard);
    // Text put in the field behind the window's back, so Home moves the
    // caret: a move is no edit.
    dialog.first.set_text("ab").unwrap();
    assert_eq!(dialog.first.caret(), 2);
    key(&mut dialog, "Home");
    assert_eq!(dialog.first.caret(), 0);
    assert!(!dialog.typed);
    assert_eq!(dialog.tick(1000), Flow::Quit);
    assert_eq!(dialog.take_answer(), Some(Answer::TimedOut));
}

#[test]
fn a_refused_key_is_reported_without_naming_it() {
    let mut request = Request::secret();
    request.error = Some("Bad passphrase (try 2 of 3)".to_owned());
    let mut dialog = dialog(request);
    // Any other notice goes to standard error alone, keeping the agent's
    // error in its row.
    assert_eq!(
        dialog.notice("theme file unreadable"),
        "theme file unreadable"
    );
    assert!(shown(&dialog).contains("Bad passphrase (try 2 of 3)"));
    // The keyboard's refusal names the key pressed, so neither the row
    // nor standard error shows it.
    let refusal = r#"keyboard: XKB byte 0 ("<AC10>:eacute"): unsupported keysym"#;
    assert_eq!(dialog.notice(refusal), KEY_REFUSED);
    let frame = shown(&dialog);
    assert!(frame.contains("The keyboard refused a key"), "{frame}");
    assert!(
        !frame.contains("eacute") && !frame.contains("AC10"),
        "{frame}"
    );
    // In a row of its own: the agent's error stays.
    assert!(frame.contains("Bad passphrase (try 2 of 3)"), "{frame}");
    // A refused paste keeps it too.
    dialog.input(Input::Paste("two\nlines"), &mut NoClipboard);
    let frame = shown(&dialog);
    assert!(
        frame.contains("Not pasted") && frame.contains("Bad passphrase"),
        "{frame}"
    );
}

/// The filter above reads td-ui's prefix: td-ui must still spell every
/// keyboard notice with it, or a refused key's name would pass.
#[test]
fn td_ui_still_begins_every_keyboard_notice_with_the_prefix() {
    let window = include_str!("../../../td-ui/src/window.rs");
    // The one arm both refusals reach, and the prefixed notice it sends.
    let arm = format!(
        "KeyboardEvent::Keymap(Err(why)) | KeyboardEvent::Refused(why) => {{\n                \
         self.handler.notice(&format!(\"{KEYBOARD}{{why}}\"));"
    );
    assert_eq!(window.matches(&arm).count(), 1, "{arm}");
    assert_eq!(window.matches("KeyboardEvent::Refused(").count(), 1);
    assert_eq!(window.matches("KeyboardEvent::Keymap(Err(").count(), 1);
}

#[test]
fn the_refusal_fits_the_default_window() {
    let mut dialog = dialog(Request::secret());
    dialog.notice("keyboard: refused");
    assert!(shown(&dialog).contains(KEY_REFUSED));
}

#[test]
fn a_hang_up_after_the_answer_keeps_it() {
    let mut dialog = dialog(Request::secret());
    typed(&mut dialog, "kept");
    key(&mut dialog, "Return");
    assert_eq!(dialog.hang_up(), Flow::Quit);
    assert_eq!(text(dialog.take_answer()), "kept");
}
