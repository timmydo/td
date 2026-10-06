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
//! prints carries a secret.
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

/// The VT the run mirrors to, once `start` opened it.
static MIRROR: Mutex<Option<Mirror<File>>> = Mutex::new(None);

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
    pub(crate) fn line(&mut self, line: &[u8], err: &mut dyn Write) {
        let Some(vt) = self.vt.as_mut() else {
            return;
        };
        match vt.write(line) {
            Err(error) if matches!(error.raw_os_error(), Some(EIO | ENXIO | EBADF)) => {
                self.vt = None;
                let _ = writeln!(
                    err,
                    "td-boot: console mirror {VT}: {error}; td-boot's lines are no longer \
                     mirrored there"
                );
            }
            // Short, would block, interrupted or any other error: this line
            // is abandoned and the next tried afresh.
            _ => {}
        }
    }
}

/// Start mirroring when `active` says `/dev/console` is not the VT. Called
/// once, before the verb's first line.
pub(crate) fn start(active: &Path) {
    if !console::read(active).vt_is_second() {
        return;
    }
    let opened = OpenOptions::new()
        .write(true)
        .custom_flags(O_NOCTTY | O_NONBLOCK)
        .open(VT);
    match opened {
        Ok(vt) => {
            *MIRROR.lock().unwrap_or_else(PoisonError::into_inner) = Some(Mirror::new(vt));
        }
        Err(error) => {
            let _ = writeln!(
                io::stderr(),
                "td-boot: console mirror {VT}: {error}; td-boot's lines are not mirrored there"
            );
        }
    }
}

/// Mirror one whole line, if the run mirrors.
fn mirror(line: &[u8]) {
    let mut mirror = MIRROR.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(mirror) = mirror.as_mut() {
        mirror.line(line, &mut io::stderr());
    }
}

/// td-boot's console: standard error, each whole line then mirrored.
pub(crate) struct Console<E: Write, M: FnMut(&[u8])> {
    err: E,
    mirror: M,
    pending: Vec<u8>,
}

/// The console every td-boot line goes to.
pub(crate) fn stderr() -> Console<io::Stderr, fn(&[u8])> {
    Console::new(io::stderr(), mirror)
}

impl<E: Write, M: FnMut(&[u8])> Console<E, M> {
    pub(crate) fn new(err: E, mirror: M) -> Self {
        Self {
            err,
            mirror,
            pending: Vec::new(),
        }
    }

    fn emit_lines(&mut self) {
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let rest = self.pending.split_off(end.saturating_add(1));
            (self.mirror)(&self.pending);
            self.pending = rest;
        }
        if self.pending.len() >= LINE_MAX {
            (self.mirror)(&self.pending);
            self.pending.clear();
        }
    }
}

impl<E: Write, M: FnMut(&[u8])> Write for Console<E, M> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.err.write(bytes)?;
        self.pending
            .extend_from_slice(bytes.get(..written).unwrap_or(bytes));
        self.emit_lines();
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.err.flush()
    }
}

impl<E: Write, M: FnMut(&[u8])> Drop for Console<E, M> {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            (self.mirror)(&self.pending);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
        let mut console = Console::new(ErrLog(Rc::clone(&log)), move |line: &[u8]| {
            mirrored
                .borrow_mut()
                .push(format!("vt {}", String::from_utf8_lossy(line)));
        });
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
        let first_vt = log.iter().position(|l| l.starts_with("vt ")).unwrap();
        assert!(log[..first_vt].iter().all(|l| l.starts_with("err ")));
        assert!(
            log[first_vt - 1].ends_with('\n'),
            "the newline's write came first"
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
        let dir = std::env::temp_dir().join(format!("td-boot-mirror-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let active = dir.join("active");
        for (text, second) in [
            (&b"tty0 ttyS0\n"[..], true),
            (b"ttyS0 tty0\n", false),
            (b"ttyS0 tty1\n", false),
            (b"tty0 ttyS0", false),
        ] {
            std::fs::write(&active, text).unwrap();
            assert_eq!(console::read(&active).vt_is_second(), second, "{text:?}");
        }
        assert!(!console::read(&dir.join("absent")).vt_is_second());
        std::fs::remove_dir_all(&dir).unwrap();
        // With no mirror started, a line reaches standard error alone.
        let mut console = Console::new(Vec::new(), mirror);
        console.write_all(b"td-boot: x\n").unwrap();
        assert!(MIRROR.lock().unwrap().is_none());
    }
}
