//! Nonblocking clipboard endpoints with explicit clock and per-turn work bounds.
//!
//! These owners are transport building blocks, not clipboard ownership or a
//! Wayland binding. Supply elapsed monotonic milliseconds; each transfer has a
//! five-second absolute deadline and at most four 16 KiB I/O attempts per step.
//! The caller must cancel incoming transfers on focus/target transitions and
//! dispatch the finished Paste through the controller for final admission.

use crate::clipboard::{Paste, MAX_BYTES};
use crate::model::{Editor, Selection, TabId};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

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

/// An immutable UTF-8 snapshot and one exclusively used pipe/socket writer.
/// File status flags are shared with descriptor duplicates: no other owner may
/// concurrently write or change those flags. Completion/cancel restores them;
/// Drop makes the same best-effort attempt but cannot report an error.
/// As with ordinary std pipe writes, BrokenPipe assumes Rust's default ignored
/// SIGPIPE disposition. Embedders must not restore the default signal action.
pub struct Outgoing {
    destination: crate::sys::Destination,
    text: Arc<str>,
    offset: usize,
    deadline: u64,
    clock: u64,
    state: TransferState,
}

impl Outgoing {
    /// Whether a pending transfer must time out without writing any more bytes.
    pub fn expired(&self, now: u64) -> bool {
        self.state == TransferState::Pending && now >= self.deadline
    }

    /// Own the exact destination, reject non-pipe/socket or read-only endpoints,
    /// and enable/read back nonblocking mode before any writes.
    pub fn begin(fd: OwnedFd, text: Arc<str>, now: u64) -> io::Result<Self> {
        if text.len() > MAX_BYTES {
            return Err(io::Error::other("clipboard source byte budget"));
        }
        let deadline = now
            .checked_add(DEADLINE_MS)
            .ok_or_else(|| io::Error::other("clipboard clock exhausted"))?;
        Ok(Self {
            destination: crate::sys::Destination::new(fd)?,
            text,
            offset: 0,
            deadline,
            clock: now,
            state: TransferState::Pending,
        })
    }

    /// Return true after all bytes were written and flags restored. Failure is
    /// permanent and closes the endpoint; the peer may have received a prefix.
    pub fn step(&mut self, now: u64) -> io::Result<bool> {
        match self.state {
            TransferState::Complete => return Ok(true),
            TransferState::Failed => {
                return Err(io::Error::other("clipboard write already failed"))
            }
            TransferState::Pending => {}
        }
        if now < self.clock {
            return self.failed(io::Error::other("clipboard clock moved backwards"));
        }
        self.clock = now;
        if now >= self.deadline {
            return self.failed(io::Error::other("clipboard write deadline"));
        }
        for _ in 0..4 {
            if self.offset == self.text.len() {
                return self.complete();
            }
            let end = self.offset.saturating_add(CHUNK).min(self.text.len());
            let bytes = self
                .text
                .as_bytes()
                .get(self.offset..end)
                .ok_or_else(|| io::Error::other("clipboard write offset"))?;
            match self.destination.write(bytes) {
                Ok(0) => return self.failed(io::Error::other("clipboard write returned zero")),
                Ok(count) if count <= bytes.len() => self.offset += count,
                Ok(_) => return self.failed(io::Error::other("clipboard write length")),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return self.failed(e),
            }
        }
        if self.offset == self.text.len() {
            self.complete()
        } else {
            Ok(false)
        }
    }

    fn complete(&mut self) -> io::Result<bool> {
        match self.destination.close() {
            Ok(()) => {
                self.state = TransferState::Complete;
                Ok(true)
            }
            Err(error) => {
                self.state = TransferState::Failed;
                Err(io::Error::other(format!(
                    "clipboard payload sent; restoring destination failed: {error}"
                )))
            }
        }
    }

    fn failed(&mut self, error: io::Error) -> io::Result<bool> {
        self.state = TransferState::Failed;
        match self.destination.close() {
            Ok(()) => Err(error),
            Err(restore) => Err(io::Error::other(format!(
                "{error}; restoring destination: {restore}"
            ))),
        }
    }

    /// Close early and report restoration errors. Already sent bytes cannot be
    /// retracted; receivers need their own admission/cancellation policy.
    pub fn cancel(mut self) -> io::Result<()> {
        self.destination.close()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ui::{Controller, Event};
    use std::io::Write;
    use std::os::fd::AsRawFd;

    fn ui() -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(b"keep")).unwrap();
        ui
    }

    fn flags(file: &impl AsRawFd) -> usize {
        let info =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).unwrap();
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:\t"))
            .unwrap();
        usize::from_str_radix(flags, 8).unwrap()
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
    fn pipe_writer_is_nonblocking_and_restores_shared_flags_on_completion() {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let mirror = writer.try_clone().unwrap();
        let original = flags(&mirror);
        let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from("é\n"), 0).unwrap();
        assert_eq!(flags(&mirror), original | 0o4000);
        assert!(outgoing.step(0).unwrap());
        assert_eq!(flags(&mirror), original);
        drop(mirror);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, "é\n".as_bytes());
    }

    #[test]
    fn stalled_pipe_times_out_and_cancel_or_drop_restores_flags() {
        for action in 0..3 {
            let (_reader, writer) = std::io::pipe().unwrap();
            let mirror = writer.try_clone().unwrap();
            let original = flags(&mirror);
            let mut outgoing =
                Outgoing::begin(OwnedFd::from(writer), Arc::from("x".repeat(MAX_BYTES)), 0)
                    .unwrap();
            assert!(!outgoing.step(0).unwrap());
            assert!(!outgoing.step(4999).unwrap());
            match action {
                0 => {
                    assert!(outgoing.step(5000).is_err());
                }
                1 => outgoing.cancel().unwrap(),
                _ => drop(outgoing),
            }
            assert_eq!(flags(&mirror), original);
        }
    }

    #[test]
    fn socket_endpoints_round_trip_and_work_is_bounded_per_step() {
        let mut ui = ui();
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        let mut outgoing =
            Outgoing::begin(OwnedFd::from(peer), Arc::from("x".repeat(MAX_BYTES)), 0).unwrap();
        assert!(!outgoing.step(0).unwrap());
        assert!(outgoing.offset <= 4 * CHUNK);
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
                assert_eq!(text.len(), MAX_BYTES + 4);
                assert!(text.bytes().take(MAX_BYTES).all(|byte| byte == b'x'));
                assert_eq!(text.get(MAX_BYTES..), Some("keep"));
                return;
            }
        }
        panic!("bounded socket transfer did not finish");
    }

    #[test]
    fn fourth_write_finishes_in_the_same_turn_before_the_deadline() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut outgoing =
            Outgoing::begin(OwnedFd::from(writer), Arc::from("x".repeat(4 * CHUNK)), 0).unwrap();
        assert!(outgoing.step(4999).unwrap());
        assert!(outgoing.step(5000).unwrap());
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, vec![b'x'; 4 * CHUNK]);
    }

    #[test]
    fn reader_drop_breaks_writer_and_nonendpoint_or_readonly_destinations_are_refused() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from("x"), 0).unwrap();
        assert_eq!(
            outgoing.step(0).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        let (reader, _writer) = std::io::pipe().unwrap();
        assert!(Outgoing::begin(OwnedFd::from(reader), Arc::from("x"), 0).is_err());
        for path in [
            "/dev/null",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ] {
            let file = File::open(path).unwrap();
            assert!(Outgoing::begin(OwnedFd::from(file), Arc::from("x"), 0).is_err());
        }
    }

    #[test]
    fn clock_reversal_and_terminal_states_cannot_revive_failed_transfers() {
        let ui = ui();
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 10).unwrap();
        assert!(incoming.step(ui.editor(), 9).is_err());
        drop(peer);
        assert!(incoming.step(ui.editor(), 10).is_err());
        assert!(incoming.finish().is_err());
        let (_reader, writer) = std::io::pipe().unwrap();
        let mirror = writer.try_clone().unwrap();
        let original = flags(&mirror);
        let mut outgoing = Outgoing::begin(OwnedFd::from(writer), Arc::from(""), 10).unwrap();
        assert!(outgoing.step(9).is_err());
        assert!(outgoing.step(10).is_err());
        assert_eq!(flags(&mirror), original);
        assert!(outgoing.cancel().is_ok());
        let (mut incoming, peer) = Incoming::begin(ui.editor(), 1, 0, 0).unwrap();
        let mut outgoing = Outgoing::begin(OwnedFd::from(peer), Arc::from(""), 0).unwrap();
        assert!(outgoing.step(0).unwrap());
        assert!(outgoing.step(u64::MAX).unwrap());
        assert!(incoming.step(ui.editor(), 1).unwrap());
        assert!(incoming.step(ui.editor(), u64::MAX).unwrap());
        assert!(incoming.finish().is_ok());
    }

    #[test]
    fn originally_nonblocking_destination_keeps_its_flags() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        writer.set_nonblocking(true).unwrap();
        let mirror = writer.try_clone().unwrap();
        let original = flags(&mirror);
        assert_ne!(original & 0o4000, 0);
        let mut outgoing = Outgoing::begin(writer.into(), Arc::from("x"), 0).unwrap();
        assert!(outgoing.step(0).unwrap());
        assert_eq!(flags(&mirror), original);
        drop(mirror);
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "x");
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
        for text in [vec![b'x'; MAX_BYTES + 1], vec![0xc3], vec![0]] {
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
