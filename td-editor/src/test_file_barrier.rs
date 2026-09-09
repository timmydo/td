//! Compiled only into explicitly opted-in fixture builds, never default builds.
//! This channel schedules jobs; it carries neither paths nor document bytes.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

pub(crate) struct Barrier {
    stream: UnixStream,
    sequence: u64,
}

impl Barrier {
    pub(crate) fn connect() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os("TD_EDITOR_TEST_FILE_BARRIER") else {
            return Ok(None);
        };
        Self::open(Path::new(&path)).map(Some)
    }

    fn open(path: &Path) -> Result<Self, String> {
        let stream = UnixStream::connect(path).map_err(|e| format!("test barrier connect: {e}"))?;
        Ok(Self {
            stream,
            sequence: 0,
        })
    }

    pub(crate) fn checkpoint(&mut self, kind: &str) -> Result<(), String> {
        self.exchange(kind, Duration::from_secs(10))
    }

    fn exchange(&mut self, kind: &str, budget: Duration) -> Result<(), String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("test file barrier exhausted")?;
        let request = format!("td-file-v1 {} {kind}\n", self.sequence);
        let expected = format!("continue {}\n", self.sequence);
        let deadline = Instant::now() + budget;
        let result = (|| -> std::io::Result<()> {
            let mut bytes = request.as_bytes();
            while !bytes.is_empty() {
                self.stream.set_write_timeout(Some(remaining(deadline)?))?;
                match self.stream.write(bytes) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => bytes = bytes.get(n..).ok_or(std::io::ErrorKind::InvalidData)?,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            // Newline terminates this reply; following bytes belong to the next.
            for expected in expected.bytes() {
                loop {
                    self.stream.set_read_timeout(Some(remaining(deadline)?))?;
                    let mut byte = [0];
                    match self.stream.read(&mut byte) {
                        Ok(1) if byte == [expected] => break,
                        Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                        Ok(_) => return Err(std::io::ErrorKind::InvalidData.into()),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(())
        })();
        result.map_err(|e| format!("test file barrier refused before I/O: {e}"))
    }
}

/// The UI only polls channels; socket waits stay on this fixture thread.
pub(crate) struct QueueGate {
    requests: SyncSender<()>,
    replies: Receiver<Result<(), String>>,
    pending: bool,
}

impl QueueGate {
    pub(crate) fn start() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os("TD_EDITOR_TEST_QUEUE_BARRIER") else {
            return Ok(None);
        };
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, replies) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("td-editor-test-queue".into())
            .spawn(move || {
                let mut barrier = match Barrier::open(Path::new(&path)) {
                    Ok(barrier) => barrier,
                    Err(detail) => {
                        let _ = outgoing.send(Err(detail));
                        return;
                    }
                };
                while incoming.recv().is_ok() {
                    let result = barrier.checkpoint("queued-save");
                    let failed = result.is_err();
                    if outgoing.send(result).is_err() || failed {
                        break;
                    }
                }
            })
            .map_err(|e| format!("test queue barrier thread: {e}"))?;
        Ok(Some(Self {
            requests,
            replies,
            pending: false,
        }))
    }

    pub(crate) fn begin(&mut self) -> Result<(), String> {
        if self.pending {
            return Err("test queue barrier already pending".into());
        }
        match self.requests.try_send(()) {
            Ok(()) => {
                self.pending = true;
                Ok(())
            }
            Err(TrySendError::Full(())) => Err("test queue barrier already pending".into()),
            Err(TrySendError::Disconnected(())) => match self.replies.try_recv() {
                Ok(Err(detail)) => Err(detail),
                _ => Err("test queue barrier disconnected before admission".into()),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn from_channels(
        requests: SyncSender<()>,
        replies: Receiver<Result<(), String>>,
    ) -> Self {
        Self {
            requests,
            replies,
            pending: false,
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<(), String>> {
        let result = match self.replies.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err("test queue barrier disconnected before handoff".into())
            }
        };
        self.pending = false;
        Some(result)
    }
}

fn remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| std::io::ErrorKind::TimedOut.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn queued_gate_poll_is_nonblocking_and_refuses_full_or_dead_channels() {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, replies) = mpsc::sync_channel(1);
        let mut gate = QueueGate::from_channels(requests, replies);
        assert!(gate.poll().is_none());
        gate.begin().unwrap();
        assert!(gate.begin().is_err());
        incoming.try_recv().unwrap();
        assert!(
            gate.begin().is_err(),
            "a drained request is still in flight"
        );
        assert!(gate.poll().is_none());
        outgoing.send(Ok(())).unwrap();
        assert_eq!(gate.poll(), Some(Ok(())));
        gate.begin().unwrap();
        incoming.try_recv().unwrap();
        outgoing.send(Err("refused".into())).unwrap();
        assert_eq!(gate.poll(), Some(Err("refused".into())));
        drop(outgoing);
        assert!(gate.poll().unwrap().unwrap_err().contains("disconnected"));
        drop(incoming);
        assert!(gate.begin().is_err());
    }

    #[test]
    fn queued_gate_preserves_setup_failure_on_late_admission() {
        let (requests, incoming) = mpsc::sync_channel(1);
        let (outgoing, replies) = mpsc::sync_channel(1);
        let mut gate = QueueGate::from_channels(requests, replies);
        outgoing
            .send(Err("specific connect failure".into()))
            .unwrap();
        drop(incoming);
        assert_eq!(gate.begin(), Err("specific connect failure".into()));
        assert!(gate.begin().unwrap_err().contains("disconnected"));
    }

    #[test]
    fn exact_release_stale_reply_eof_and_deadline_are_distinct() {
        for reply in [b"continue 1\n".as_slice(), b"continue 0\n", b""] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.write_all(reply).unwrap();
            peer.shutdown(std::net::Shutdown::Write).unwrap();
            let mut barrier = Barrier {
                stream,
                sequence: 0,
            };
            assert_eq!(
                barrier.checkpoint("reload").is_ok(),
                reply == b"continue 1\n"
            );
            let mut request = [0; 20];
            peer.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"td-file-v1 1 reload\n");
        }
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut barrier = Barrier {
            stream,
            sequence: 0,
        };
        assert!(barrier.exchange("save", Duration::from_millis(10)).is_err());
        barrier.sequence = u64::MAX;
        assert!(barrier
            .checkpoint("save")
            .unwrap_err()
            .contains("exhausted"));
    }

    #[test]
    fn bytes_after_a_valid_reply_are_checked_by_the_next_exchange() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"continue 1\nX").unwrap();
        let mut barrier = Barrier {
            stream,
            sequence: 0,
        };
        barrier.checkpoint("reload").unwrap();
        assert!(barrier.checkpoint("save").is_err());
    }
}
