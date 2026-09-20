//! Nonblocking clipboard endpoints with explicit clock and per-turn work bounds.
//!
//! These owners are transport building blocks, not clipboard ownership or a
//! Wayland binding. Supply elapsed monotonic milliseconds; each transfer has a
//! five-second absolute deadline and at most four 16 KiB I/O attempts per step.
//! The caller must cancel incoming transfers on focus/target transitions and
//! dispatch the finished Paste through the controller for final admission.
//! `Outgoing`, the writer over the send's right, is the toolkit's
//! (`td_ui::clipboard`), which owns the destination's status flags under
//! UNSAFE.md §19; `Incoming` stays here because it admits the paste into
//! the editor's `Paste` as the bytes arrive.

use crate::clipboard::Paste;
use crate::model::{Editor, Selection, TabId};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

pub use td_ui::clipboard::Outgoing;

const CHUNK: usize = 16 * 1024;
const DEADLINE_MS: u64 = 5000;

/// A bounded private socket receiver. Dropping it cancels without editing.
pub struct Incoming {
    stream: UnixStream,
    paste: Paste,
    tab: TabId,
    revision: u64,
    selection: Selection,
    deadline: u64,
    clock: u64,
    read: Box<[u8]>,
    state: TransferState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TransferState {
    Pending,
    Complete,
    Failed,
}

impl Incoming {
    /// Whether a pending transfer must time out without reading any more bytes.
    pub fn expired(&self, now: u64) -> bool {
        self.state == TransferState::Pending && now >= self.deadline
    }

    /// Return a receiver and its peer endpoint, to pass to a text producer.
    /// Drop the local peer after handing it off so successful EOF is observable.
    pub fn begin(editor: &Editor, tab: TabId, revision: u64, now: u64) -> io::Result<(Self, File)> {
        let deadline = now
            .checked_add(DEADLINE_MS)
            .ok_or_else(|| io::Error::other("clipboard clock exhausted"))?;
        let paste = Paste::begin(editor, tab, revision).map_err(io::Error::other)?;
        let selection = editor.document(tab).map_err(io::Error::other)?.selection();
        let (stream, peer) = UnixStream::pair()?;
        stream.set_nonblocking(true)?;
        let file = File::from(OwnedFd::from(peer));
        Ok((
            Self {
                stream,
                paste,
                tab,
                revision,
                selection,
                deadline,
                clock: now,
                read: vec![0; CHUNK].into_boxed_slice(),
                state: TransferState::Pending,
            },
            file,
        ))
    }

    /// Return true only after EOF. A failure is permanent; no prefix is admitted.
    /// The controller rechecks editor identity and intent when consuming Paste.
    pub fn step(&mut self, editor: &Editor, now: u64) -> io::Result<bool> {
        match self.state {
            TransferState::Complete => return Ok(true),
            TransferState::Failed => return Err(io::Error::other("clipboard read already failed")),
            TransferState::Pending => {}
        }
        match self.read(editor, now) {
            Ok(done) => {
                if done {
                    self.state = TransferState::Complete;
                }
                Ok(done)
            }
            Err(e) => {
                self.state = TransferState::Failed;
                Err(e)
            }
        }
    }

    fn read(&mut self, editor: &Editor, now: u64) -> io::Result<bool> {
        if now < self.clock {
            return Err(io::Error::other("clipboard clock moved backwards"));
        }
        self.clock = now;
        if now >= self.deadline {
            return Err(io::Error::other("clipboard read deadline"));
        }
        if editor.active() != Some(self.tab)
            || !editor.document(self.tab).is_ok_and(|doc| {
                doc.revision() == self.revision && doc.selection() == self.selection
            })
        {
            return Err(io::Error::other(
                "paste cancelled: document or selection changed",
            ));
        }
        // Four bounded reads per turn, including Interrupted: peers cannot
        // starve protocol events by continuously refilling the endpoint.
        for _ in 0..4 {
            match self.stream.read(&mut self.read) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    let bytes = self
                        .read
                        .get(..count)
                        .ok_or_else(|| io::Error::other("clipboard read length"))?;
                    self.paste.push(bytes).map_err(io::Error::other)?;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// Consume a successfully completed transfer; this does not edit by itself.
    pub fn finish(self) -> io::Result<Paste> {
        if self.state != TransferState::Complete {
            return Err(io::Error::other("clipboard transfer has no successful EOF"));
        }
        Ok(self.paste)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ui::{Controller, Event};
    use std::io::Write;
    use std::sync::Arc;

    fn ui() -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"keep")).unwrap();
        ui
    }

    #[test]
    fn partial_read_waits_for_eof_and_preserves_state_until_controller_admission() {
        let mut ui = ui();
        let (mut incoming, mut peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        assert!(!incoming.step(ui.editor(), 1).unwrap());
        peer.write_all(&[0xc3]).unwrap();
        assert!(!incoming.step(ui.editor(), 2).unwrap());
        peer.write_all(&[0xa9, b'\r']).unwrap();
        assert!(!incoming.step(ui.editor(), 3).unwrap());
        peer.write_all(b"\n").unwrap();
        assert!(!incoming.step(ui.editor(), 4).unwrap());
        assert_eq!(ui.editor().document(1).unwrap().text(), "keep");
        drop(peer);
        assert!(incoming.step(ui.editor(), 5).unwrap());
        ui.dispatch(Event::Paste(incoming.finish().unwrap()))
            .unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "é\nkeep");
    }

    #[test]
    fn stalled_read_deadline_missing_eof_and_changed_selection_never_admit_a_prefix() {
        let mut ui = ui();
        let (mut incoming, mut peer) = Incoming::begin(ui.editor(), 1, 0, 10).unwrap();
        peer.write_all(b"prefix").unwrap();
        assert!(!incoming.step(ui.editor(), 5009).unwrap());
        assert!(incoming.step(ui.editor(), 5010).is_err());
        drop(peer);
        assert!(incoming.step(ui.editor(), 5011).is_err());
        assert!(incoming.finish().is_err());
        let (incoming, _peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        assert!(incoming.finish().is_err());
        let (mut incoming, _peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: crate::model::Command::Select(Selection {
                anchor: 1,
                caret: 1,
            }),
        })
        .unwrap();
        assert!(incoming.step(ui.editor(), 1).is_err());
        assert!(incoming.finish().is_err());
        assert_eq!(ui.editor().document(1).unwrap().text(), "keep");
        assert!(Incoming::begin(ui.editor(), 1, 0, u64::MAX).is_err());
    }

    #[test]
    fn socket_endpoints_round_trip_and_work_is_bounded_per_step() {
        let mut ui = ui();
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        let mut outgoing = Outgoing::begin(
            OwnedFd::from(peer),
            Arc::from("x".repeat(crate::clipboard::MAX_BYTES)),
            0,
        )
        .unwrap();
        // The writer's per-step bound is td-ui's own test's to pin; the
        // receiver's is that a 1 MiB text takes more than one step.
        assert!(!outgoing.step(0).unwrap());
        assert!(!incoming.step(ui.editor(), 0).unwrap());
        let mut done = false;
        for now in 1..100 {
            if !done {
                done = outgoing.step(now).unwrap();
            }
            if incoming.step(ui.editor(), now).unwrap() {
                assert!(done);
                ui.dispatch(Event::Paste(incoming.finish().unwrap()))
                    .unwrap();
                let text = ui.editor().document(1).unwrap().text();
                assert_eq!(text.len(), crate::clipboard::MAX_BYTES + 4);
                assert!(text
                    .bytes()
                    .take(crate::clipboard::MAX_BYTES)
                    .all(|byte| byte == b'x'));
                assert_eq!(text.get(crate::clipboard::MAX_BYTES..), Some("keep"));
                return;
            }
        }
        panic!("bounded socket transfer did not finish");
    }

    #[test]
    fn the_writer_is_the_toolkits_under_the_same_ceiling() {
        assert_eq!(crate::clipboard::MAX_BYTES, td_ui::clipboard::MAX_BYTES);
    }

    #[test]
    fn clock_reversal_and_terminal_states_cannot_revive_failed_transfers() {
        let ui = ui();
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 10).unwrap();
        assert!(incoming.step(ui.editor(), 9).is_err());
        drop(peer);
        assert!(incoming.step(ui.editor(), 10).is_err());
        assert!(incoming.finish().is_err());
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        let mut outgoing = Outgoing::begin(OwnedFd::from(peer), Arc::from(""), 0).unwrap();
        assert!(outgoing.step(0).unwrap());
        assert!(outgoing.step(u64::MAX).unwrap());
        assert!(incoming.step(ui.editor(), 1).unwrap());
        assert!(incoming.step(ui.editor(), u64::MAX).unwrap());
        assert!(incoming.finish().is_ok());
    }

    #[test]
    fn matching_foreign_editor_cannot_admit_the_finished_paste() {
        let original = ui();
        let mut replacement = ui();
        let (mut incoming, mut peer) = Incoming::begin(original.editor(), 1, 0, 0).unwrap();
        peer.write_all(b"foreign").unwrap();
        drop(peer);
        // The early guard is advisory and compares visible intent. Only final
        // controller admission checks the private editor-instance identity.
        assert!(incoming.step(replacement.editor(), 1).unwrap());
        assert!(replacement
            .dispatch(Event::Paste(incoming.finish().unwrap()))
            .is_err());
        assert_eq!(replacement.editor().document(1).unwrap().text(), "keep");
    }

    #[test]
    fn oversized_or_malformed_input_never_changes_the_document() {
        for text in [
            vec![b'x'; crate::clipboard::MAX_BYTES + 1],
            vec![0xc3],
            vec![0],
        ] {
            let mut ui = ui();
            let (mut incoming, mut peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
            peer.set_len(0).unwrap_err(); // socket, never a temporary data file
            for (now, chunk) in text.chunks(CHUNK).enumerate() {
                peer.write_all(chunk).unwrap();
                if incoming.step(ui.editor(), now as u64).is_err() {
                    break;
                }
            }
            drop(peer);
            if incoming.step(ui.editor(), 100).is_ok() {
                assert!(ui
                    .dispatch(Event::Paste(incoming.finish().unwrap()))
                    .is_err());
            } else {
                assert!(incoming.finish().is_err());
            }
            assert_eq!(ui.editor().document(1).unwrap().text(), "keep");
            assert_eq!(ui.editor().document(1).unwrap().history_depth(), (0, 0));
        }
    }
}
