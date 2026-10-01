//! The replay protocol over the shared editor core: td-editor's headless
//! session, its framed runner and the real binary's `--replay`, checked
//! against the core's typed events.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "asserted test fixtures"
)]

use td_editor::{font, replay};
use td_ui::editor::{Controller, Event, PointerPhase};
use td_ui::editor_model::{Command, Selection};
use td_ui::raster::Raster;

fn selection(ui: &Controller) -> Selection {
    let tab = ui.editor().active().unwrap();
    ui.editor().document(tab).unwrap().selection()
}
fn pixels(ui: &Controller) -> Vec<u8> {
    let geometry = ui.geometry();
    let (w, h) = (geometry.surface().width, geometry.surface().height);
    let mut pixels = vec![0; w * h * 4];
    let font = font::pinned().unwrap();
    Raster::new(&mut pixels, &font, geometry.surface(), w * 4)
        .unwrap()
        .paint(&ui.scene(&[]).unwrap(), geometry.bounds())
        .unwrap();
    pixels
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
        assert_eq!(session.ui.editor().document(1).unwrap().text(), "é");
        assert_eq!(session.ui.editor().document(1).unwrap().revision(), 1);
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
    assert_eq!(s.ui.editor().document(1).unwrap().text(), "Zc");
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

#[test]
fn replay_and_typed_events_produce_identical_state_and_pixels() {
    let mut session = replay::Session::default();
    let mut direct = Controller::default();
    let source = "abcde\nx\nabcde";
    let commands = [
        format!("load\t{}", replay::hex(source.as_bytes())),
        "resize\t64\t104\t1".into(),
        "select-range\t1\t0\t4\t4".into(),
        format!("key\t1\t0\t{}", replay::hex(b"Down")),
        "pointer\t1\t0\tpress\t8\t49\t0".into(),
        "pointer\t1\t0\trelease\t32\t65\t0".into(),
        "pointer\t1\t0\tpress\t24\t49\t0".into(),
        "pointer\t1\t0\trelease\t0\t0\t0".into(),
        "set-soft-wrap\t1\t0\t0".into(),
        "scroll\t1\t0\trows\tforward\t2".into(),
        "tick\t500".into(),
    ];
    let events = [
        Event::Load(source.as_bytes()),
        Event::Resize {
            width: 64,
            height: 104,
            scale: 1,
        },
        Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 4,
                caret: 4,
            }),
        },
        Event::Key {
            tab: 1,
            revision: 0,
            chord: "Down",
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Press,
            x: 8,
            cell_x: 8,
            y: 49,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Release,
            x: 32,
            cell_x: 32,
            y: 65,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Press,
            x: 24,
            cell_x: 24,
            y: 49,
            extend: false,
        },
        Event::Pointer {
            tab: 1,
            revision: 0,
            phase: PointerPhase::Release,
            x: i64::MIN,
            cell_x: i64::MIN,
            y: i64::MIN,
            extend: false,
        },
        Event::Wrap {
            tab: 1,
            revision: 0,
            enabled: false,
        },
        Event::Scroll {
            tab: 1,
            revision: 0,
            rows: 2,
            columns: 0,
        },
        Event::Tick(500),
    ];
    assert_eq!(commands.len(), events.len());
    for (command, event) in commands.iter().zip(events) {
        assert!(!session
            .request(format!("1\t7\t{command}").as_bytes())
            .contains("error"));
        direct.dispatch(event).unwrap();
        assert_eq!(session.ui.generation(), direct.generation());
        assert_eq!(session.ui.tab_view(1).unwrap(), direct.tab_view(1).unwrap());
        assert_eq!(selection(&session.ui), selection(&direct));
        assert_eq!(pixels(&session.ui), pixels(&direct));
    }
}

#[test]
fn malformed_ui_wire_commands_do_not_change_state() {
    let mut session = replay::Session::default();
    session.request(b"1\t0\tload\t616263");
    let before = session.request(b"1\t0\tstate");
    for command in [
        "resize\t0\t600\t1",
        "resize\t800\t600\t256",
        "focus\t2",
        "tick\t-1",
        "tick\t18446744073709551616",
        "pointer\t1\t0\tclick\t8\t49\t0",
        "pointer\t1\t0\tpress\t18446744073709551615\t49\t0",
        "pointer\t1\t0\tpress\t8\t49\t0\textra",
        "set-soft-wrap\t1\t0\t2",
        "scroll\t1\t0\trows\tnegative\t1",
        "scroll\t1\t0\trows\tforward\t18446744073709551615",
    ] {
        assert!(
            session
                .request(format!("1\t0\t{command}").as_bytes())
                .contains("error"),
            "{command}"
        );
        assert_eq!(session.request(b"1\t0\tstate"), before, "{command}");
    }
}

#[test]
fn the_real_replay_binary_accepts_ui_events_without_a_display() {
    use std::io::Write;
    use std::process::{Command as Process, Stdio};
    let requests = [
        "1\t1\tload\t6162630a78797a",
        "1\t2\tresize\t64\t104\t1",
        "1\t3\tkey\t1\t0\t446f776e",
        "1\t4\tpointer\t1\t0\tpress\t8\t49\t0",
        "1\t5\ttick\t500",
        "1\t6\tstate",
    ];
    let mut wire = Vec::new();
    let mut expected = Vec::new();
    let mut session = replay::Session::default();
    for request in requests {
        wire.extend_from_slice(&(request.len() as u32).to_be_bytes());
        wire.extend_from_slice(request.as_bytes());
        let response = session.request(request.as_bytes());
        assert!(!response.contains("error"));
        expected.extend_from_slice(&(response.len() as u32).to_be_bytes());
        expected.extend_from_slice(response.as_bytes());
    }
    let mut child = Process::new(env!("CARGO_BIN_EXE_td-editor"))
        .arg("--replay")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&wire).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, expected);
}

#[test]
fn pristine_new_tabs_can_close_and_undo_returns_to_clean() {
    let mut s = replay::Session::default();
    for request in 1..=100 {
        assert!(!s.request(b"1\t0\tnew").contains("error"));
        let id = s.ui.editor().active().unwrap();
        assert!(!s.ui.editor().document(id).unwrap().dirty());
        let close = format!("1\t{request}\tclose-tab\t{id}\t0");
        assert!(!s.request(close.as_bytes()).contains("error"));
    }
    s.request(b"1\t0\tnew");
    let id = s.ui.editor().active().unwrap();
    s.request(format!("1\t0\tinsert\t{id}\t0\t78").as_bytes());
    assert!(s.ui.editor().document(id).unwrap().dirty());
    s.request(format!("1\t0\tundo\t{id}\t1").as_bytes());
    assert!(!s.ui.editor().document(id).unwrap().dirty());
}

#[test]
fn windows_cancel_keeps_selection_while_emacs_cancel_clears_it() {
    let mut s = replay::Session::default();
    s.request(b"1\t0\tload\t616263");
    let id = s.ui.editor().active().unwrap();
    s.request(b"1\t0\tselect-range\t1\t0\t0\t2");
    assert!(!s
        .request(b"1\t1\tkey\t1\t0\t457363617065")
        .contains("error"));
    assert_eq!(
        s.ui.editor().document(id).unwrap().selection(),
        Selection {
            anchor: 0,
            caret: 2
        }
    );
    s.request(b"1\t2\tset-key-profile\temacs");
    assert!(!s.request(b"1\t3\tkey\t1\t0\t432d67").contains("error"));
    assert_eq!(
        s.ui.editor().document(id).unwrap().selection(),
        Selection {
            anchor: 2,
            caret: 2
        }
    );
}

#[test]
fn the_executable_replays_without_a_display_and_rejects_invalid_window_inputs() {
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
    let failure = Process::new(binary)
        .arg("--invalid-option")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("unknown window option"));
    let failure = Process::new(binary).env_clear().output().unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("XDG_RUNTIME_DIR"));
    let failure = Process::new(binary).arg("/dev/null").output().unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("NotRegular"));
    let failure = Process::new(binary)
        .args(["--window", "/dev/null"])
        .output()
        .unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("NotRegular"));
    let failure = Process::new(binary)
        .args(["--window", "--keys=invalid"])
        .output()
        .unwrap();
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("unknown window option"));
}
