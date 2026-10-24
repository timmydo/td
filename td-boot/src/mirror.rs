//! td-boot's console lines on the VT (td-install/ENCRYPTION.md "Keyboard
//! console").
//!
//! Every console line td-boot writes goes to standard error as before, which
//! the built-in command line makes the serial console. In the verbs the two
//! initramfs run (`mirrors`), when `/dev/console` is not the VT, the same
//! bytes also go to `/dev/tty1`, opened once, write-only, `O_NOCTTY` and
//! `O_NONBLOCK`. A line is mirrored whole, after its standard-error write.
//! Nothing waits on the mirror: a write that would block or is short (a VT
//! stopped by Scroll Lock or `^S`) is abandoned for that line and the next is
//! tried afresh. A mirror that cannot be opened, or whose write fails with
//! `EIO`, `ENXIO` or `EBADF`, is said once on standard error and dropped for
//! the rest of the run. Standard error itself stays blocking. Nothing td-boot
//! prints carries a secret. A complete write to either stderr or the screen
//! satisfies a diagnostic. If both fail in an early-boot verb, a nonblocking,
//! one-record /dev/kmsg write is the final fallback. Short writes do not count.
//! Each emitted stderr record is flushed before its success is reported;
//! fallback delivery is not invalidated by flushing the failed stderr again.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::console;

/// The first VT, the foreground one: nothing in either initramfs switches.
pub(crate) const VT: &str = "/dev/tty1";
const O_NOCTTY: i32 = 0o400;
const O_NONBLOCK: i32 = 0o4000;
/// The write errors that drop the mirror for the rest of the run.
const EIO: i32 = 5;
const ENXIO: i32 = 6;
const EBADF: i32 = 9;
/// A line longer than this without a newline is mirrored as it stands.
const LINE_MAX: usize = 4096;

/// Early-boot output routes; absent for ordinary runtime commands.
static OUTPUTS: Mutex<Option<Fallbacks<File>>> = Mutex::new(None);

struct Fallbacks<V> {
    screen: Option<Mirror<V>>,
    warned: bool,
}

impl<V: Write> Fallbacks<V> {
    fn line(
        &mut self,
        line: &[u8],
        primary_error: Option<&io::Error>,
        err: &mut dyn Write,
        mut kernel_log: impl FnMut(&[u8]) -> bool,
    ) -> bool {
        let screen_ok = self
            .screen
            .as_mut()
            .is_some_and(|screen| screen.line(line, err));
        let Some(error) = primary_error else {
            return screen_ok;
        };
        let delivered = screen_ok || kernel_log(line);
        if delivered && !self.warned {
            self.warned = true;
            let target = if screen_ok { VT } else { "/dev/kmsg" };
            let notice =
                format!("td-boot: standard error failed: {error}; continuing output on {target}\n");
            if screen_ok {
                if let Some(screen) = self.screen.as_mut() {
                    screen.line(notice.as_bytes(), err);
                }
            } else {
                kernel_log(notice.as_bytes());
            }
        }
        delivered
    }
}

/// One mirror over `vt`.
pub(crate) struct Mirror<V> {
    vt: Option<V>,
}

impl<V: Write> Mirror<V> {
    pub(crate) fn new(vt: V) -> Self {
        Self { vt: Some(vt) }
    }

    /// One write of `line`, never waited on. A hard error drops the mirror,
    /// said once on `err`.
    pub(crate) fn line(&mut self, line: &[u8], err: &mut dyn Write) -> bool {
        let Some(vt) = self.vt.as_mut() else {
            return false;
        };
        match vt.write(line) {
            Err(error) if matches!(error.raw_os_error(), Some(EIO | ENXIO | EBADF)) => {
                self.vt = None;
                let _ = writeln!(
                    err,
                    "td-boot: console mirror {VT}: {error}; td-boot's lines are no longer \
                     mirrored there"
                );
                false
            }
            Ok(written) => written == line.len(),
            // Short, would block, interrupted or any other error: this line
            // is abandoned and the next tried afresh.
            _ => false,
        }
    }
}

/// Enable fallbacks only for the early-boot verbs. An unknown console identity
/// suppresses duplicate screen writes, but still permits kmsg on stderr failure.
pub(crate) fn start(active: &Path) {
    let screen = if console::read(active).vt_is_second() {
        match OpenOptions::new()
            .write(true)
            .custom_flags(O_NOCTTY | O_NONBLOCK)
            .open(VT)
        {
            Ok(vt) => Some(Mirror::new(vt)),
            Err(error) => {
                let _ = writeln!(
                    io::stderr(),
                    "td-boot: console mirror {VT}: {error}; td-boot's lines are not mirrored there"
                );
                None
            }
        }
    } else {
        None
    };
    *OUTPUTS.lock().unwrap_or_else(PoisonError::into_inner) = Some(Fallbacks {
        screen,
        warned: false,
    });
}

fn kernel_log(line: &[u8]) -> bool {
    // Open only for this record: no descriptor survives to pin devtmpfs.
    let Ok(mut log) = OpenOptions::new()
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return false;
    };
    matches!(log.write(line), Ok(written) if written == line.len())
}

/// Return success only when a fallback accepted the complete record.
fn mirror(line: &[u8], primary_error: Option<&io::Error>) -> bool {
    let mut outputs = OUTPUTS.lock().unwrap_or_else(PoisonError::into_inner);
    outputs
        .as_mut()
        .is_some_and(|outputs| outputs.line(line, primary_error, &mut io::stderr(), kernel_log))
}

/// td-boot's console: standard error, each whole line then mirrored.
pub(crate) struct Console<E: Write, M: FnMut(&[u8], Option<&io::Error>) -> bool> {
    err: E,
    mirror: M,
    pending: Vec<u8>,
}

type Fallback = fn(&[u8], Option<&io::Error>) -> bool;

/// The console every td-boot line goes to.
pub(crate) fn stderr() -> Console<io::Stderr, Fallback> {
    Console::new(io::stderr(), mirror)
}

impl<E: Write, M: FnMut(&[u8], Option<&io::Error>) -> bool> Console<E, M> {
    pub(crate) fn new(err: E, mirror: M) -> Self {
        Self {
            err,
            mirror,
            pending: Vec::new(),
        }
    }

    fn emit_pending(&mut self) -> io::Result<()> {
        let line = std::mem::take(&mut self.pending);
        let result = self.err.write_all(&line).and_then(|()| self.err.flush());
        let delivered = (self.mirror)(&line, result.as_ref().err());
        if delivered {
            Ok(())
        } else {
            result
        }
    }

    fn emit_lines(&mut self) -> io::Result<()> {
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let rest = self.pending.split_off(end.saturating_add(1));
            let result = self.emit_pending();
            self.pending = rest;
            result?;
        }
        if self.pending.len() >= LINE_MAX {
            self.emit_pending()?;
        }
        Ok(())
    }
}

impl<E: Write, M: FnMut(&[u8], Option<&io::Error>) -> bool> Write for Console<E, M> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        self.emit_lines()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.emit_pending()?;
        }
        // Each emitted record was already flushed on its successful path.
        Ok(())
    }
}

impl<E: Write, M: FnMut(&[u8], Option<&io::Error>) -> bool> Drop for Console<E, M> {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let _ = self.emit_pending();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A VT that records what reached it and answers each write as told.
    struct Vt {
        shown: Rc<RefCell<Vec<u8>>>,
        answers: Vec<io::Result<Option<usize>>>,
    }

    impl Write for Vt {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = if self.answers.is_empty() {
                bytes.len()
            } else {
                self.answers.remove(0)?.unwrap_or(bytes.len())
            };
            self.shown.borrow_mut().extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn vt(answers: Vec<io::Result<Option<usize>>>) -> (Mirror<Vt>, Rc<RefCell<Vec<u8>>>) {
        let shown = Rc::new(RefCell::new(Vec::new()));
        let vt = Vt {
            shown: Rc::clone(&shown),
            answers,
        };
        (Mirror::new(vt), shown)
    }

    /// Each line is mirrored whole, once its standard-error write is done,
    /// however `writeln!` splits it into writes.
    #[test]
    fn a_line_is_mirrored_whole_after_its_standard_error_write() {
        let log = Rc::new(RefCell::new(Vec::<String>::new()));
        struct ErrLog(Rc<RefCell<Vec<String>>>);
        impl Write for ErrLog {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0
                    .borrow_mut()
                    .push(format!("err {}", String::from_utf8_lossy(bytes)));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mirrored = Rc::clone(&log);
        let mut console = Console::new(
            ErrLog(Rc::clone(&log)),
            move |line: &[u8], _error: Option<&io::Error>| {
                mirrored
                    .borrow_mut()
                    .push(format!("vt {}", String::from_utf8_lossy(line)));
                true
            },
        );
        let (word, count) = ("one", 2);
        writeln!(console, "td-boot: {word} of {count}").unwrap();
        console.write_all(b"two\nthr").unwrap();
        console.write_all(b"ee\n").unwrap();
        console.write_all(b"tail").unwrap();
        drop(console);
        let log = log.borrow();
        let vt: Vec<&String> = log.iter().filter(|l| l.starts_with("vt ")).collect();
        assert_eq!(
            vt,
            [
                "vt td-boot: one of 2\n",
                "vt two\n",
                "vt three\n",
                "vt tail"
            ]
        );
        let errors: Vec<&String> = log.iter().filter(|l| l.starts_with("err ")).collect();
        assert_eq!(
            errors,
            [
                "err td-boot: one of 2\n",
                "err two\n",
                "err three\n",
                "err tail"
            ]
        );
        let first_vt = log.iter().position(|l| l.starts_with("vt ")).unwrap();
        assert!(log[..first_vt].iter().all(|l| l.starts_with("err ")));
        assert!(
            log[first_vt - 1].ends_with('\n'),
            "the newline's write came first"
        );
    }

    #[test]
    fn a_partial_line_flushes_and_stderr_failure_still_reaches_the_mirror() {
        let lines = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&lines);
        let mut console = Console::new(
            Vec::new(),
            move |line: &[u8], _error: Option<&io::Error>| {
                seen.borrow_mut().push(line.to_vec());
                false
            },
        );
        console.write_all(b"tail").unwrap();
        assert!(console.err.is_empty());
        console.flush().unwrap();
        assert_eq!(console.err, b"tail");
        drop(console);
        assert_eq!(*lines.borrow(), vec![b"tail".to_vec()]);

        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let seen = Rc::clone(&lines);
        let mut console = Console::new(Closed, move |line: &[u8], _error: Option<&io::Error>| {
            seen.borrow_mut().push(line.to_vec());
            false
        });
        assert_eq!(
            console.write_all(b"error\n").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(console);
        assert_eq!(*lines.borrow(), vec![b"tail".to_vec(), b"error\n".to_vec()]);
    }

    #[test]
    fn volume_binding_survives_serial_eio_when_the_screen_receives_it() {
        struct FailedSerial;
        impl Write for FailedSerial {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(EIO))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (screen, shown) = vt(Vec::new());
        let mut routes = Fallbacks {
            screen: Some(screen),
            warned: false,
        };
        let mut output = Console::new(
            FailedSerial,
            move |line: &[u8], error: Option<&io::Error>| {
                routes.line(line, error, &mut io::sink(), |_| {
                    panic!("a working screen must not require kmsg")
                })
            },
        );
        let uuid = crate::volume::Uuid::parse("12345678-1234-1234-1234-123456789abc").unwrap();
        let result = crate::report_volume_binding(&mut output, &uuid, Path::new("/dev/sda2"));
        assert!(
            result.is_ok(),
            "visible volume report aborted boot: {result:?}"
        );
        output.flush().unwrap();
        let shown = String::from_utf8_lossy(&shown.borrow()).into_owned();
        assert!(
            shown.starts_with("TD-BOOT-VOLUME 12345678-1234-1234-1234-123456789abc /dev/sda2\n")
        );
        assert!(shown.contains("standard error failed: Input/output error (os error 5)"));
        assert!(shown.contains("continuing output on /dev/tty1"));
    }

    #[test]
    fn incomplete_screen_output_uses_kmsg_and_warns_only_once() {
        for first in [
            Ok(Some(3)),
            Err(io::ErrorKind::WouldBlock.into()),
            Err(io::Error::from_raw_os_error(EIO)),
        ] {
            let (screen, _) = vt(vec![first]);
            let mut routes = Fallbacks {
                screen: Some(screen),
                warned: false,
            };
            let error = io::Error::from_raw_os_error(EIO);
            let mut records = Vec::new();
            assert!(
                routes.line(b"volume binding\n", Some(&error), &mut io::sink(), |line| {
                    records.push(line.to_vec());
                    true
                })
            );
            assert_eq!(records.first().unwrap(), b"volume binding\n");
            assert_eq!(records.len(), 2);
            assert!(String::from_utf8_lossy(&records[1]).contains("continuing output on /dev/kmsg"));
            assert!(
                routes.line(b"next\n", Some(&error), &mut io::sink(), |line| {
                    records.push(line.to_vec());
                    true
                })
            );
            assert_eq!(
                records
                    .iter()
                    .filter(|line| line.starts_with(b"td-boot: standard error failed:"))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn no_output_path_must_not_turn_a_missing_report_into_success() {
        for screen in [None, Some(vt(vec![Ok(Some(1))]).0)] {
            let mut routes = Fallbacks {
                screen,
                warned: false,
            };
            let error = io::Error::from_raw_os_error(EIO);
            assert!(
                !routes.line(b"volume binding\n", Some(&error), &mut io::sink(), |_| {
                    false
                })
            );
            assert!(!routes.warned);
        }
        let mut routes: Fallbacks<Vec<u8>> = Fallbacks {
            screen: None,
            warned: false,
        };
        assert!(
            !routes.line(b"healthy stderr\n", None, &mut io::sink(), |_| {
                panic!("a successful stderr write must not be duplicated into kmsg")
            })
        );
    }

    #[test]
    fn a_failed_primary_flush_can_be_satisfied_by_a_complete_fallback() {
        struct BadFlush;
        impl Write for BadFlush {
            fn write(&mut self, line: &[u8]) -> io::Result<usize> {
                Ok(line.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(EIO))
            }
        }
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let seen = delivered.clone();
        let mut console = Console::new(BadFlush, move |line: &[u8], error: Option<&io::Error>| {
            assert_eq!(error.unwrap().raw_os_error(), Some(EIO));
            seen.borrow_mut().push(line.to_vec());
            true
        });
        console.write_all(b"full\ntail").unwrap();
        console.flush().unwrap();
        console.flush().unwrap();
        drop(console);
        assert_eq!(
            *delivered.borrow(),
            vec![b"full\n".to_vec(), b"tail".to_vec()]
        );
        let mut failed = Console::new(BadFlush, |_: &[u8], _: Option<&io::Error>| false);
        assert_eq!(
            failed.write_all(b"missing\n").unwrap_err().raw_os_error(),
            Some(EIO)
        );
    }

    /// A write that would block or is short abandons that line alone: the
    /// next is tried afresh, so a stopped VT misses lines only while stopped.
    #[test]
    fn a_stopped_or_short_vt_misses_only_that_line() {
        let (mut mirror, shown) = vt(vec![
            Err(io::ErrorKind::WouldBlock.into()),
            Ok(Some(3)),
            Err(io::ErrorKind::Interrupted.into()),
        ]);
        let mut err = Vec::new();
        for line in [&b"a\n"[..], b"bcdef\n", b"g\n", b"h\n"] {
            mirror.line(line, &mut err);
        }
        assert_eq!(&*shown.borrow(), b"bcdh\n");
        assert!(err.is_empty(), "nothing is said for a skipped line");
        assert!(mirror.vt.is_some());
    }

    /// A hard error is said once and drops the mirror for the run.
    #[test]
    fn a_hard_error_drops_the_mirror_once() {
        for errno in [EIO, ENXIO, EBADF] {
            let (mut mirror, shown) = vt(vec![Err(io::Error::from_raw_os_error(errno))]);
            let mut err = Vec::new();
            mirror.line(b"a\n", &mut err);
            mirror.line(b"b\n", &mut err);
            assert!(shown.borrow().is_empty());
            assert!(mirror.vt.is_none());
            let said = String::from_utf8(err).unwrap();
            assert_eq!(said.lines().count(), 1, "{said}");
            assert!(said.starts_with("td-boot: console mirror /dev/tty1: "));
        }
        // Any other error skips the line and keeps the mirror.
        let (mut mirror, shown) = vt(vec![Err(io::Error::from_raw_os_error(22))]);
        let mut err = Vec::new();
        mirror.line(b"a\n", &mut err);
        mirror.line(b"b\n", &mut err);
        assert_eq!(&*shown.borrow(), b"b\n");
        assert!(err.is_empty());
    }

    /// No mirror when `/dev/console` is the VT, or when `active` cannot be
    /// read or parsed: no line is written twice to one terminal.
    #[test]
    fn the_console_identity_decides_whether_to_mirror() {
        let dir = crate::test_temp::TempDir::new("td-boot-mirror").unwrap();
        let active = dir.path().join("active");
        for (text, second) in [
            (&b"tty0 ttyS0\n"[..], true),
            (b"ttyS0 tty0\n", false),
            (b"ttyS0 tty1\n", false),
            (b"tty0 ttyS0", false),
        ] {
            std::fs::write(&active, text).unwrap();
            assert_eq!(console::read(&active).vt_is_second(), second, "{text:?}");
        }
        assert!(!console::read(&dir.path().join("absent")).vt_is_second());
        // With no mirror started, a line reaches standard error alone.
        let mut console = Console::new(Vec::new(), mirror);
        console.write_all(b"td-boot: x\n").unwrap();
        assert!(OUTPUTS.lock().unwrap().is_none());
    }
}
