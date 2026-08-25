//! Shared plumbing for the applets: byte-oriented argv, input both whole (sed)
//! and streamed a record at a time (grep's `Records`), and a buffered stdout
//! that treats a closed pipe as "stop", not as a hard error.

use std::ffi::OsString;
use std::io::{IsTerminal, Read, Seek, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Program arguments as raw bytes. A pattern or a filename need not be UTF-8, so
/// nothing here goes through `String`.
pub fn args_bytes() -> Vec<Vec<u8>> {
    std::env::args_os().map(|a| a.into_vec()).collect()
}

pub fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

/// Whether `POSIXLY_CORRECT` is set. GNU reads only whether it is THERE -- an
/// empty value, `0` and `no` all count. It has THREE jobs and this answers all
/// three: ending option parsing at the first operand (both applets), selecting
/// GNU sed's middle `posixicity` level, and -- the one that is not that level
/// -- suppressing sed's confusing-bracket lint, which `dfawarn` gates on the
/// same `getenv` rather than on the level (sed/regexp.c:52). What it must NOT
/// do is set sed's `--posix`: that is a LOWER level than this one selects, and
/// spec/README measures the 76 scripts the two spellings disagree on.
pub fn posixly_correct() -> bool {
    std::env::var_os("POSIXLY_CORRECT").is_some()
}

pub fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(os_from_bytes(bytes))
}

/// A diagnostic with RAW bytes spliced into it, which is what GNU's `%s` writes.
/// A `String` cannot hold one: it is UTF-8 by construction, and a name need not
/// be, so re-encoding one names a file the operator never passed.
pub fn name_in(before: &str, name: &[u8], after: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(before.len() + name.len() + after.len());
    out.extend_from_slice(before.as_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(after.as_bytes());
    out
}

/// The same splice for one byte, which is GNU's `%c`. `format!` cannot do it:
/// `char::from` widens the byte to a Unicode scalar and encodes it as UTF-8, so
/// every byte from 0x80 up arrives as two.
pub fn byte_in(before: &str, byte: u8, after: &str) -> Vec<u8> {
    name_in(before, &[byte], after)
}

/// GNU's `quote()` -- gnulib `quotearg` at `locale_quoting_style` -- as the C
/// locale resolves it: ASCII `'` rather than the U+2018/U+2019 a UTF-8 locale
/// picks. C is the only locale td's image sets and the one every golden here was
/// derived under, so the curly pair would pin a message td's own userland never
/// prints.
///
/// The ESCAPING is the half that is not cosmetic. An argument reaches this from
/// argv, so a raw newline in one would split a single diagnostic across two lines
/// a consumer reads as two, and a raw high byte would leave the message no longer
/// valid in the encoding it is read as.
pub fn quote_arg(value: &[u8], out: &mut Vec<u8>) {
    out.push(b'\'');
    for &b in value {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            0x07 => out.extend_from_slice(b"\\a"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            // Always THREE octal digits, even where two would do: `\200` before a
            // literal `1` must not read back as `\2001`.
            0x00..=0x1f | 0x7f..=0xff => {
                out.push(b'\\');
                out.push(b'0' + (b >> 6));
                out.push(b'0' + ((b >> 3) & 7));
                out.push(b'0' + (b & 7));
            }
            _ => out.push(b),
        }
    }
    out.push(b'\'');
}

/// The prefix a C `%s` prints: the bytes before the first NUL. GNU builds each
/// diagnostic as a C string, so a NUL that reaches one ENDS it -- and only a
/// `-f` script can carry a NUL into a message, argv being NUL-terminated.
pub fn cstr(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|b| *b == 0) {
        Some(n) => bytes.get(..n).unwrap_or_default(),
        None => bytes,
    }
}

/// What `--version` reports. A literal rather than `env!("CARGO_PKG_VERSION")`
/// because the SHIPPED binary is compiled by a direct `rustc src/main.rs` with no
/// cargo to set that variable; `version_matches_the_manifest` pins the two.
pub const VERSION: &str = "0.1.0";

/// An errno as GNU spells it. Rust's `io::Error` Display appends ` (os error N)`
/// to the `strerror` text; these diagnostics are read next to every other tool's,
/// so the suffix comes back off.
pub fn errmsg(e: &std::io::Error) -> String {
    let text = e.to_string();
    let Some(n) = e.raw_os_error() else {
        return text;
    };
    match text.strip_suffix(&format!(" (os error {n})")) {
        Some(head) => head.to_string(),
        None => text,
    }
}

/// An input that is OPEN, so a caller can tell the two failures apart. GNU carries
/// on from a name it could not open -- a warning, exit 2 -- and treats one it
/// opened and could not READ as fatal to sed (`read error on NAME`, exit 4) and as
/// an empty file to grep. A DIRECTORY operand is the reachable case of the second:
/// `open(2)` on one succeeds and the first read fails.
pub enum Input {
    /// Descriptor 0, held as a DUPLICATE to read through. `std::io::Stdin`
    /// answers `Ok(0)` where the descriptor answers `EBADF`: std maps that one
    /// error on the standard streams to end of input, so a descriptor 0 that is
    /// write-only reads as an empty input and the diagnostic GNU prints for it
    /// is not reachable at all. A dup shares the file DESCRIPTION, so reading it
    /// moves descriptor 0 itself and a second reader of the same description
    /// sees what this one left.
    ///
    /// `None` when the duplication failed, which falls back to the buffered path
    /// rather than failing the run: EMFILE must not turn a working run into an
    /// error.
    Stdin(Option<std::fs::File>),
    File(std::fs::File),
}

/// `O_NONBLOCK`, pinned by value because it is passed as a raw flag word and not
/// through any named `std` API. Opening a FIFO nobody writes BLOCKS until a
/// writer arrives, so a device that is about to be SKIPPED has to be opened
/// without waiting for one that may never come -- and the decision is made after
/// the open, from the descriptor. GNU sets this flag on exactly that path
/// (`skip_devices (command_line) ? O_NONBLOCK : 0`, grep.c:1758) and nowhere
/// else, which is why it is a parameter here rather than always on: with
/// `-D read` the wait IS the requested behaviour.
///
/// The value is LINUX's, which td targets; a test asks the kernel to confirm it
/// took rather than trusting the header it was copied from.
const O_NONBLOCK: i32 = 0o4000;

impl Input {
    /// `dash_is_stdin` is false for a name grep's WALK produced -- those are always
    /// files, so `grep -r` over a directory holding one called `-` searches it --
    /// and for sed's `-i`, which rewrites a NAME and cannot rewrite a pipe.
    pub fn open(path: &[u8], dash_is_stdin: bool) -> std::io::Result<Self> {
        Self::open_maybe_nonblock(path, dash_is_stdin, false)
    }

    /// `open` for a caller that may SKIP what it opens, and so must not block on
    /// it: see `O_NONBLOCK`.
    pub fn open_maybe_nonblock(
        path: &[u8],
        dash_is_stdin: bool,
        nonblock: bool,
    ) -> std::io::Result<Self> {
        if dash_is_stdin && path == b"-" {
            return Ok(Self::stdin());
        }
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        if nonblock {
            opts.custom_flags(O_NONBLOCK);
        }
        Ok(Self::File(opts.open(path_from_bytes(path))?))
    }

    /// Descriptor 0 as an input, duplicated so reads reach the descriptor rather
    /// than std's wrapper over it. One dup per OPEN, not one per run: two `-`
    /// operands make two, which share the file DESCRIPTION and so share a
    /// position, but are not one reader. A caller that needs a single reader
    /// still arranges it -- sed's `open_fd0` does, and drops the second
    /// operand's `Input` and with it the dup it had just made.
    pub fn stdin() -> Self {
        Self::Stdin(stdin_unbuffered())
    }

    /// Move the duplicate out, for a caller that reads descriptor 0 through its
    /// own reader and must not make a SECOND dup to do it. `None` for a file
    /// operand and for a stdin whose dup failed; both mean the same thing to the
    /// caller, which is that it has no descriptor of its own to read or seek.
    ///
    /// A THIRD `None` is not the same thing: a stdin whose dup has ALREADY been
    /// taken still answers `is_stdin`, and reading it falls back to the wrapper
    /// with the `EBADF` masking that returns. No caller does that today -- the
    /// only one consumes the `Input` when it takes the dup -- and one that meant
    /// to read afterwards would have to keep the dup instead.
    pub fn take_stdin_dup(&mut self) -> Option<std::fs::File> {
        match self {
            Self::Stdin(dup) => dup.take(),
            Self::File(_) => None,
        }
    }

    /// Whether the OPEN descriptor is a directory. Stdin is never one for this
    /// purpose even when it genuinely is: GNU exempts it by DESCRIPTOR, so
    /// `grep -d skip x - < somedir` reports `(standard input): Is a directory`
    /// rather than skipping -- and so a directory merely NAMED `-` cannot make
    /// `-d skip` pass over a pipe.
    pub fn is_dir(&self) -> bool {
        match self {
            Self::Stdin(_) => false,
            Self::File(file) => file.metadata().is_ok_and(|m| m.is_dir()),
        }
    }

    /// Whether the OPEN descriptor is what GNU calls a device, which is FOUR file
    /// types and not the two the word suggests: character and block special, plus
    /// sockets and FIFOs (`is_device_mode`, grep.c:612). A regular file and a
    /// directory are the only things that are not one.
    ///
    /// Stdin is exempt for `is_dir`'s reason and by the same mechanism -- GNU
    /// tests `desc != STDIN_FILENO` before it asks -- so `grep -D skip x - <fifo`
    /// reads the fifo, and a device merely NAMED `-` cannot make `-D skip` pass
    /// over a pipe.
    pub fn is_device(&self) -> bool {
        match self {
            Self::Stdin(_) => false,
            Self::File(file) => file.metadata().is_ok_and(|m| {
                use std::os::unix::fs::FileTypeExt;
                let t = m.file_type();
                t.is_char_device() || t.is_block_device() || t.is_socket() || t.is_fifo()
            }),
        }
    }

    /// Why `-i` may not rewrite this, if it may not -- GNU's two refusals, in GNU's
    /// ORDER, since a terminal is also not a regular file and the first test is what
    /// names it. Asked of the OPEN descriptor rather than of the name, so the answer
    /// cannot be about a different file than the one that will be read. Stdin cannot
    /// arrive here (`-i` opens `-` as the ordinary name it is) and is refused as the
    /// non-file it would be.
    pub fn in_place_refusal(&self) -> Option<&'static str> {
        let Self::File(file) = self else {
            return Some("not a regular file");
        };
        if file.is_terminal() {
            return Some("is a terminal");
        }
        if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
            return Some("not a regular file");
        }
        None
    }

    /// Whether the bytes came off descriptor 0, which decides whether a caller
    /// that over-read may hand the surplus back. Asked of the OPEN input rather
    /// than by re-deciding what `-` meant, as `error_name` is.
    pub fn is_stdin(&self) -> bool {
        matches!(self, Self::Stdin(_))
    }

    /// How a READ failure names this input. GNU registers the standard input STREAM
    /// under the name `stdin` and reports read errors against that, where the same
    /// operand is `-` to `F` and to the `can't read` warning -- so
    /// `sed p < a-directory` is `read error on stdin`. Answered from the OPEN input
    /// rather than by re-deciding what `-` meant, which is a second place to get it
    /// wrong.
    pub fn error_name(&self, path: &[u8]) -> Vec<u8> {
        match self {
            Self::Stdin(_) => b"stdin".to_vec(),
            Self::File(_) => path.to_vec(),
        }
    }

    /// Read to EOF. On failure the bytes that DID arrive come back with it: a read
    /// can append and then fail, and reporting that file as empty would lose the
    /// matches in what was read.
    pub fn read_all(&mut self) -> Result<Vec<u8>, (std::io::Error, Vec<u8>)> {
        let mut buf = Vec::new();
        let read = match self {
            Self::Stdin(None) => std::io::stdin().lock().read_to_end(&mut buf),
            // ONE path for every descriptor held as a `File`, the stdin
            // DUPLICATE included: the size hint and its clamp must not depend on
            // which operand spelling produced the descriptor. `-f -` over a file
            // whose metadata is enormous has to answer as `-f NAME` over that
            // same file does, and before this arm shared the path it did not.
            Self::Stdin(Some(file)) | Self::File(file) => {
                // Size the buffer from the metadata, as `fs::read` does -- but
                // CLAMPED. That number comes from the filesystem and need not be a
                // size anything can hold (a sparse file, `/proc/kcore`), and
                // `with_capacity` ABORTS rather than failing, where the read below
                // reports out of memory and the applet carries on.
                const HINT_CAP: usize = 8 << 20;
                let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
                buf.reserve(size.saturating_add(1).min(HINT_CAP));
                file.read_to_end(&mut buf)
            }
        };
        match read {
            Ok(_) => Ok(buf),
            Err(err) => Err((err, buf)),
        }
    }
}

/// Descriptor 0 as an UNBUFFERED reader, sharing the file description so reads
/// through it move fd 0 itself. `Input::stdin` is the only caller: the duplicate
/// belongs to the open input, so no reader has to make one.
///
/// Two things need the descriptor rather than the wrapper. `std::io::Stdin` maps
/// `EBADF` on the standard streams to end of input, which hides a write-only
/// descriptor 0 completely. And it is a `BufReader`: its buffer is 8 KiB, and a
/// read SMALLER than that takes the whole 8 KiB off the descriptor -- so a reader
/// asking for 4 KiB blocks would leave a pipe positioned by std's buffer size
/// rather than by the block size it chose, and what a `q` leaves behind would not
/// be its own decision. `None` if the descriptor cannot be duplicated, which
/// leaves the caller to fall back rather than fail: EMFILE here must not turn a
/// working run into an error.
fn stdin_unbuffered() -> Option<std::fs::File> {
    let dup = std::io::stdin().as_fd().try_clone_to_owned().ok()?;
    Some(std::fs::File::from(dup))
}

/// Hand back to descriptor 0 what was read from it and never used, so the next
/// reader of that file description starts where this applet stopped instead of at
/// end of file. It is POSIX's "shall not consume more input than it needs", and
/// what makes `{ sed 1q; cat; } < f` the usable idiom it is meant to be. BOTH
/// applets call it now: sed for the record its cycle stopped after, grep for the
/// one `-m` counted to, which is `{ grep -m1 PAT; cat; } < f` and the same
/// promise under another name.
///
/// A REGULAR file only: a descriptor can answer a position query and still refuse
/// a negative seek, and the other common stdin -- a pipe -- has nowhere to put
/// what it was given back. Nothing is lost by declining, since not seeking is
/// exactly the behaviour that stands today.
///
/// The seek is RELATIVE, so a process that SHARES this description and moves the
/// offset first sends it somewhere else entirely. That is inherent to reading more
/// than is needed and returning the rest, bash and GNU sed included.
///
/// A dup is what keeps this in safe code: `Stdin` exposes no seek, while an
/// `OwnedFd` cloned from it shares the DESCRIPTION, so seeking the clone is what
/// moves descriptor 0. It is the CALLER's dup rather than one made here, and that
/// is the whole of `fd0`: a reader that already holds one has no business making a
/// second, which under descriptor pressure is a dup that fails on a run that
/// otherwise succeeded — leaving the position a whole block further on than it
/// should be, with nothing saying so.
pub fn give_back_stdin(fd0: &mut std::fs::File, unread: u64) -> std::io::Result<()> {
    if unread == 0 {
        return Ok(());
    }
    if !fd0.metadata()?.is_file() {
        return Ok(());
    }
    let back = i64::try_from(unread).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "read past what a seek can undo")
    })?;
    fd0.seek(std::io::SeekFrom::Current(-back))?;
    Ok(())
}

/// Read a whole input, or stdin for `-`, for a caller the two failures read alike
/// to. SED's readers and grep's `-f` pattern files, which need the whole text at
/// once; grep's own operands stream through `Records` and no longer come this
/// way.
pub fn read_input(path: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut input = Input::open(path, true)?;
    input.read_all().map_err(|(err, _)| err)
}

/// `read_input` for the SEARCH: it reports where the failure fell, and a name the
/// WALK produced is always a file. Only an operand spells stdin `-`, so `grep -r`
/// in a directory holding a file called `-` searches it rather than reading
/// stdin, as GNU does.
///
/// `skip_dirs` is grep's `-d skip`, and it is answered AFTER the open because
/// that is where GNU answers it -- an `fstat` in `grepdesc`, not a `stat` on the
/// name. A directory the process may not open is therefore still reported, and
/// still exit 2; deciding from the name instead swallows that error silently.
/// `Ok(None)` is the skip.
///
/// `devices` is `-D`'s half of the same question, resolved by the caller because
/// only it knows where the name came from.
pub fn open_search(
    path: &[u8],
    from_walk: bool,
    skip_dirs: bool,
    devices: DeviceRule,
) -> Result<Option<Input>, std::io::Error> {
    // A name found BELOW the walk's root is decided before the open, which is the
    // opposite of the rule above and is GNU's too: `grepdirent` reads the entry's
    // own stat and skips without opening, "since opening might have side effects
    // on a device". A SOCKET makes the difference observable rather than
    // theoretical -- opening one fails ENXIO, so deciding after the open turns a
    // silent skip into `No such device or address`. An OPERAND goes the other way
    // and really is opened first, which is why `-D skip` on a named socket reports
    // that error in GNU and here; the walk's ROOT is an operand for this purpose,
    // however deep the walk below it goes.
    //
    // The answer comes from the walk's own stat rather than a fresh one: asking
    // twice cost about 28% of `grep -r` over a wide tree of small files, every
    // one of which is stat'd by the walk moments earlier. A stat that FAILED
    // reports `false` and falls through to the open, so the error is reported by
    // whoever reports every other one.
    if matches!(devices, DeviceRule::Walked(true)) {
        return Ok(None);
    }
    // Only the descriptor case can still block, the walked one having been
    // answered above and `Read` being the request to wait.
    let descriptor = matches!(devices, DeviceRule::Descriptor);
    let input = Input::open_maybe_nonblock(path, !from_walk, descriptor)?;
    if skip_dirs && input.is_dir() {
        return Ok(None);
    }
    if descriptor && input.is_device() {
        return Ok(None);
    }
    Ok(Some(input))
}

/// How a name's TYPE is to be judged, which depends on where the name came from
/// and is therefore the caller's to say. The two arms that skip are the whole of
/// the walk/operand split: one is answered before the open and one after it.
pub enum DeviceRule {
    /// Read whatever it turns out to be -- `-D read`, or the default for a name
    /// the operand list held.
    Read,
    /// Judged ALREADY, from the walk's own stat, and `true` means it is a device.
    /// Nothing opens it, which is what keeps a socket found by `grep -r` silent
    /// rather than ENXIO, and what saves the second stat.
    Walked(bool),
    /// Judged from the descriptor, after opening. An operand, including the
    /// walk's root: GNU opens those and asks `fstat`, so `-D skip` on a named
    /// socket still reports the failed open rather than passing over it.
    Descriptor,
}

/// `Input::is_device` asked of a `Metadata` the walk already had.
fn is_device(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let t = meta.file_type();
    t.is_char_device() || t.is_block_device() || t.is_socket() || t.is_fifo()
}

impl std::io::Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Stdin(Some(fd0)) => fd0.read(buf),
            Self::Stdin(None) => std::io::stdin().lock().read(buf),
            Self::File(file) => file.read(buf),
        }
    }
}

/// grep's input one record at a time, so a search never holds the whole file.
/// This is what lets `grep -q a /dev/urandom` return at the first match instead
/// of reading forever, and what stops a large operand costing its own size in
/// memory. `-B N` is the exception and is not this type's: the caller retains N
/// whole records to print them as context.
///
/// The read buffer is FIXED. A record longer than it spills into `spill` and
/// grows there, which is why an input with no separator at all (`/dev/zero`) is
/// still unsearchable by anything line-based, GNU included -- one unbounded
/// record has to be held whole to be matched against.
///
/// `binary` is set by the FILL rather than by the record, because that is where
/// grep's binary test lives: a NUL anywhere in a buffer makes every match from
/// that buffer onward the notice, and matches already emitted stay emitted. It
/// is STICKY, as GNU's is -- a later NUL-free buffer does not make a file text
/// again. Which side of the flip a given match falls on depends on where the
/// buffer boundaries land, and that is NOT a promise: GNU GROWS its buffer to
/// hold the longest line, so its own boundary moves with the data (measured:
/// with 80-byte lines a match at 200000 is suppressed by a NUL at 250000, and
/// with 100000-byte lines the same pair is not). td-txt reproduces the RULE and
/// declines to reproduce the arithmetic.
pub struct Records<R> {
    src: R,
    buf: Vec<u8>,
    /// Valid bytes of `buf`.
    len: usize,
    /// Next unscanned byte of `buf`.
    pos: usize,
    /// A record that spans fills is assembled here; one that does not is handed
    /// out as a slice of `buf`, so the common case copies nothing.
    spill: Vec<u8>,
    spilled: bool,
    start: usize,
    end: usize,
    terminated: bool,
    sep: u8,
    eof: bool,
    binary: bool,
    /// Replace every NUL in a binary buffer with `sep` before it is scanned, as
    /// grep does. Off by default: sed reaches no binary verdict, so nothing may
    /// rewrite the bytes it is about to hand out.
    zap: bool,
    /// Discard a read that is ENTIRELY zeros rather than turning it into one
    /// empty record per byte. Only sound where an empty record cannot be
    /// selected, which is the caller's question, so this is the caller's flag.
    skip: bool,
    /// Empty records dropped by that, owed to whoever numbers lines.
    skipped: u64,
    /// Whether any of those went into an OPEN record rather than standing as
    /// empty records of their own. GNU numbers past them either way, but a
    /// joined gap leaves no records for `-B` to show.
    skipped_joined: bool,
    /// A read failure deferred until the bytes read before it were handed over.
    pending: Option<std::io::Error>,
    consumed: u64,
    /// Where the record `line()` returns STARTS, counted from the first byte of
    /// the input. `consumed` cannot answer this: it is bytes taken from the
    /// SOURCE and runs a whole buffer ahead of the record being handed out.
    at: u64,
    /// Bytes accounted for, records and their separators and any dropped run
    /// alike. A record's `at` is this as it stood when that record BEGAN.
    emitted: u64,
}

impl<R: std::io::Read> Records<R> {
    /// 96 KiB because that is GNU's own initial buffer, so on a REGULAR file --
    /// where a `read` fills what it is given -- the boundaries agree with GNU's
    /// for as long as GNU has not had to grow. Nothing depends on the number.
    pub const BUF: usize = 96 << 10;

    pub fn new(src: R, sep: u8) -> Self {
        Self::with_buffer(src, sep, Self::BUF)
    }

    /// The buffer size is the CALLER's, because it is the caller that has a
    /// reason: grep takes GNU's 96 KiB so its binary-verdict boundaries line up,
    /// sed takes GNU's 4 KiB so what it leaves in a pipe does. Neither number is
    /// a promise -- see spec/README.
    pub fn with_buffer(src: R, sep: u8, cap: usize) -> Self {
        Self {
            src,
            buf: vec![0; cap.max(1)],
            len: 0,
            pos: 0,
            spill: Vec::new(),
            spilled: false,
            start: 0,
            end: 0,
            terminated: false,
            sep,
            eof: false,
            binary: false,
            zap: false,
            skip: false,
            skipped: 0,
            skipped_joined: false,
            pending: None,
            consumed: 0,
            at: 0,
            emitted: 0,
        }
    }

    pub fn binary(&self) -> bool {
        self.binary
    }

    /// Turn on grep's NUL zapping (`zap_nuls`). GNU's own reason is memory --
    /// a run of zeros with no separator in it would otherwise be held as one
    /// unbounded line -- and what it does to MATCHING comes with it, since the
    /// separator it writes is a line boundary like any other.
    pub fn zap_nuls(&mut self, on: bool) {
        self.zap = on;
    }

    /// Turn on GNU's `skip_nuls`, the other half of that: zapping makes a run
    /// of zeros one empty record PER BYTE, and a read that holds nothing else
    /// is discarded instead. Pass whether an empty record is unselectable --
    /// GNU asks the same question once, of the pattern, before it reads.
    pub fn skip_zero_fills(&mut self, on: bool) {
        self.skip = on;
    }

    /// Empty records dropped since this was last asked. Added to a line number
    /// so a run that was skipped still counts: GNU credits `totalnl` for the
    /// same reason, and `grep -z -n` over a megabyte of NULs shows it.
    /// The count, and whether any of it was absorbed by an open record rather
    /// than standing as empty records `-B` could show.
    pub fn take_skipped(&mut self) -> (u64, bool) {
        (std::mem::take(&mut self.skipped), std::mem::take(&mut self.skipped_joined))
    }

    /// Input offset just PAST the record `line()` returns: its separator, and any
    /// run this reader DROPPED, included. Rebuilding it from `offset()` plus the
    /// record's own length does not work and is the bug that asked for this
    /// method -- a zero fill joined into a record moves its end without
    /// lengthening the slice, so the sum lands a whole fill short. grep's `-m`
    /// reposition needs this number and nothing weaker.
    pub fn record_end(&self) -> u64 {
        self.emitted
    }

    /// Bytes taken from the SOURCE so far, which is not what the caller has been
    /// handed: a fill reads a block, and the records in it are handed out one at
    /// a time. sed needs both numbers to give descriptor 0 back what it read and
    /// never used.
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Byte offset of the record `line()` returns, which is what `grep -b`
    /// prints. Counted over the INPUT rather than over what was searched, so a
    /// run of NULs dropped by `skip_zero_fills` still moves it -- those bytes
    /// are in the file whether or not anything looked at them.
    pub fn offset(&self) -> u64 {
        self.at
    }

    /// The source itself. sed hands descriptor 0 back its over-read by SEEKING the
    /// reader's own duplicate of it, which is here.
    pub fn source_mut(&mut self) -> &mut R {
        &mut self.src
    }

    /// Forget end of input alone, leaving the buffer and the position where they
    /// are. `rewind(3)` does this to a stream whose seek FAILED -- verified
    /// against glibc, `feof` goes 1 to 0 on a drained pipe while the seek reports
    /// ESPIPE -- so a source that ended because its last writer went away is read
    /// again if a new one appears.
    pub fn forget_eof(&mut self) {
        self.eof = false;
    }

    /// Read again from wherever the SOURCE now is, forgetting the buffer, the
    /// record and end of input. sed's `-s` rewinds an `R` source this way, having
    /// seeked the handle back itself: a reader that kept its buffer would hand out
    /// records from before the seek.
    pub fn restart(&mut self) {
        self.len = 0;
        self.pos = 0;
        self.spill.clear();
        self.spilled = false;
        self.start = 0;
        self.end = 0;
        self.terminated = false;
        self.eof = false;
        self.pending = None;
        self.binary = false;
        self.skipped = 0;
        self.skipped_joined = false;
        self.consumed = 0;
        self.at = 0;
        self.emitted = 0;
    }

    pub fn line(&self) -> &[u8] {
        match self.spilled {
            true => &self.spill,
            false => self.buf.get(self.start..self.end).unwrap_or_default(),
        }
    }

    pub fn terminated(&self) -> bool {
        self.terminated
    }

    /// ONE `read` per fill, which is `fillbuf`'s shape: looping until the buffer
    /// was full would make a pipe wait for 96 KiB before the first match could
    /// be reported. The loop here is not that -- it only retries reads that were
    /// DISCARDED whole, which deliver no record and so cannot delay one.
    fn fill(&mut self) -> std::io::Result<()> {
        loop {
            self.len = 0;
            self.pos = 0;
            // `Interrupted` is not a read failure, it is a signal that arrived
            // mid-call; `read_to_end` retried it and so must this, or a `SIGWINCH`
            // during a long search becomes `grep: f: Interrupted system call`.
            let n = loop {
                match self.src.read(&mut self.buf) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            };
            if n == 0 {
                self.eof = true;
                return Ok(());
            }
            self.len = n;
            self.consumed = self.consumed.saturating_add(n as u64);
            // The verdict as it stood BEFORE this read, which is what decides
            // whether this read may be skipped: GNU sets `skip_nuls` in the loop
            // body, after `fillbuf` has already delivered the buffer the verdict
            // trips on, so that buffer is always kept and zapped and only later
            // ones are dropped. Under `-z` there is no verdict to wait for.
            let was_binary = self.binary;
            if self.buf.get(..n).is_some_and(|s| s.contains(&0)) {
                self.binary = true;
            }
            // From the buffer the verdict trips ON, not the one after it: GNU zaps
            // the same buffer it decided from, and every later one whether or not
            // that one holds a NUL of its own. A separator of 0 (`grep -z`) is why
            // GNU reaches no verdict at all there, so there is nothing to zap.
            let sep = self.sep;
            let zapping = self.zap && self.binary && sep != 0;
            // A record still OPEN is not a reason to keep the read. GNU discards
            // it and keeps the carry, so an unterminated record joins across the
            // gap to whatever follows -- a record the unskipped reading never
            // had, and observably GNU's: `^foo.*bar$` selects it there.
            if self.skip
                && ((self.zap && was_binary && sep != 0) || sep == 0)
                && self.buf.get(..n).is_some_and(|s| s.iter().all(|b| *b == 0))
            {
                // One empty record per byte is what was dropped, and the count is
                // owed to whoever numbers lines. With a record OPEN there are no
                // records to owe -- the gap joins into it -- but GNU numbers past
                // it just the same, so only the `-B` replay is told apart.
                self.skipped = self.skipped.saturating_add(n as u64);
                self.skipped_joined |= self.spilled;
                self.emitted = self.emitted.saturating_add(n as u64);
                // With nothing carried, no record has begun, so the next one
                // starts PAST this run. With a record open it began before the
                // run and keeps the offset it already has -- the same split the
                // `-B` replay makes, seen from the byte count.
                if !self.spilled {
                    self.at = self.emitted;
                }
                continue;
            }
            if zapping {
                if let Some(s) = self.buf.get_mut(..n) {
                    s.iter_mut().filter(|b| **b == 0).for_each(|b| *b = sep);
                }
            }
            return Ok(());
        }
    }

    /// Advance to the next record. `false` is end of input.
    pub fn next(&mut self) -> std::io::Result<bool> {
        self.spill.clear();
        self.spilled = false;
        self.terminated = false;
        self.at = self.emitted;
        // A failure held back from the previous call so the bytes read BEFORE it
        // could be searched first. Reported now, once, and not again.
        if let Some(err) = self.pending.take() {
            return Err(err);
        }
        loop {
            let hay = self.buf.get(self.pos..self.len).unwrap_or_default();
            if let Some(rel) = hay.iter().position(|b| *b == self.sep) {
                let (s, e) = (self.pos, self.pos.saturating_add(rel));
                self.pos = e.saturating_add(1);
                self.terminated = true;
                if self.spilled {
                    let seg = self.buf.get(s..e).unwrap_or_default();
                    self.spill.try_reserve(seg.len()).map_err(|_| oom())?;
                    self.spill.extend_from_slice(seg);
                } else {
                    (self.start, self.end) = (s, e);
                }
                // `spill` already holds the whole record, the `s..e` tail
                // included: the branch above appended it before this ran.
                let len = match self.spilled {
                    true => self.spill.len(),
                    false => e.saturating_sub(s),
                };
                // The separator is a byte of the input too, so it counts.
                self.emitted =
                    self.emitted.saturating_add(len as u64).saturating_add(1);
                return Ok(true);
            }
            // No separator in what is held: carry the remainder and refill. The
            // carry is what makes a record able to cross a buffer at all.
            if self.pos < self.len {
                let seg = self.buf.get(self.pos..self.len).unwrap_or_default();
                self.spill.try_reserve(seg.len()).map_err(|_| oom())?;
                self.spill.extend_from_slice(seg);
                self.spilled = true;
                self.pos = self.len;
            }
            if self.eof {
                // A final record carrying no separator of its own.
                let live = self.spilled && !self.spill.is_empty();
                if live {
                    self.emitted =
                        self.emitted.saturating_add(self.spill.len() as u64);
                }
                return Ok(live);
            }
            if let Err(e) = self.fill() {
                // Bytes were read, and THEN the next read failed. Those bytes
                // are a record GNU searches -- the whole-file reader this
                // replaced kept them as `ReadFail::partial` for the same reason
                // -- so hand them over and report the failure on the next call.
                if self.spilled && !self.spill.is_empty() {
                    // Counted like the EOF record it resembles, and for a
                    // reason EOF does not have: `pending` is cleared when it is
                    // reported, so a reader that fails once and then yields
                    // more would carry this record's length as a shortfall in
                    // every offset after it.
                    self.emitted = self.emitted.saturating_add(self.spill.len() as u64);
                    self.pending = Some(e);
                    return Ok(true);
                }
                return Err(e);
            }
        }
    }
}

/// What `read_to_end` reports when an allocation fails, so an operand too big to
/// hold is a diagnosed `out of memory` and the applet carries on to the next.
/// td-txt's ONE spelling of that error: sed's `-i` buffer refuses the same way.
pub fn oom() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::OutOfMemory)
}

/// Write one line to stdout, treating a CLOSED READER as "done" rather than as a
/// panic — `println!` aborts the applet thread on EPIPE (`grep --help | head -1`),
/// which every other write here already avoids by going through `Out`, so the
/// one-shot informational outputs go through it too. A genuine write failure is
/// returned rather than turned into a status here: the two applets number their
/// errors differently (grep 2, sed 4), and picking one for them would be wrong for
/// the other.
pub fn print_line(text: &str) -> std::io::Result<()> {
    let mut out = Out::new()?;
    out.write(text.as_bytes())?;
    out.write(b"\n")?;
    out.flush()
}

/// C stdio's buffer, which is the one every stream td-txt writes has to use: both
/// programs are scored against GNU's, and GNU's streams are stdio's. Both of its
/// choices are asked of the DESCRIPTOR rather than assumed
/// (`_IO_file_doallocate`): the capacity is its `st_blksize`, and a TERMINAL is
/// line-buffered instead of block-buffered.
///
/// What blocks fill to is CAPACITY before writing anything, rather than flushing
/// early to make room: 4000 bytes buffered then a 100-byte line leaves exactly
/// 4096 written, where a `BufWriter` -- which writes the 4000 to make room, then
/// buffers the 100 -- leaves 4000. And a full buffer is not itself a reason to
/// write; having more to put and nowhere to put it is, so a write that exactly
/// fills leaves it sitting there. A second reader of the same file sees both
/// differences, which is why the fill is copied rather than delegated.
pub struct StdioBuf {
    file: std::fs::File,
    held: Vec<u8>,
    cap: usize,
    /// A terminal flushes through the last newline rather than when it fills, and
    /// on `\n` whatever `-z` made the RECORD separator: this is stdio's rule, not
    /// sed's.
    line: bool,
    /// stdio's buffer does not exist until the first overflow, so the FIRST write
    /// finds no room at all and takes the block path whole. That is not a detail
    /// of the allocation: it is why one 8193-byte record written first reaches
    /// the descriptor as 8192 then 1, and the same record written second as
    /// 4096, 4096.
    allocated: bool,
}

impl StdioBuf {
    /// stdio's own default, and what a descriptor's answer must BEAT to be used.
    const BUFSIZ: usize = 8192;
    /// Stdio's own threshold for block-aligning a direct write rather than
    /// writing the whole of it.
    const ALIGN: usize = 128;

    /// The capacity is not simply `st_blksize`: `_IO_file_doallocate` starts at
    /// `BUFSIZ` and takes the descriptor's answer only when it is positive AND
    /// SMALLER, so a filesystem reporting 64 KiB gets stdio's 8192 and not its
    /// own number. Every `st_blksize` this platform hands out is 4096, so the
    /// difference is invisible here and would be a divergence on NFS or ZFS --
    /// and on the sink specifically it would be a NEW one, `BufWriter::new`
    /// having been 8192 already.
    pub fn over(file: std::fs::File) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        let cap = match file.metadata().map(|m| usize::try_from(m.blksize())) {
            Ok(Ok(block)) if block > 0 && block < Self::BUFSIZ => block,
            _ => Self::BUFSIZ,
        };
        let line = file.is_terminal();
        Self { file, held: Vec::with_capacity(cap), cap, line, allocated: false }
    }

    /// The most a caller may hand over in one `put` and still get the boundaries
    /// a stream of SMALL writes would have produced. One byte under the capacity,
    /// not the capacity: a `put` of exactly `cap` finds no room at all on the
    /// first write of a run -- the buffer not existing yet -- and so goes
    /// straight to the descriptor, ahead of anything else sharing it.
    pub fn piece(&self) -> usize {
        self.cap.saturating_sub(1).max(1)
    }

    /// Zero until the first overflow has been through, as stdio's is.
    fn room(&self) -> usize {
        match self.allocated {
            true => self.cap.saturating_sub(self.held.len()),
            false => 0,
        }
    }

    /// A duplicate of descriptor 1, which is what standard output is written
    /// through rather than `std::io::Stdout` -- that is a `LineWriter`, and a
    /// buffer flushed on anything but a newline splits at the last one it holds,
    /// so a record and its separator reach a concurrent reader as two writes.
    /// Safe: `try_clone_to_owned` needs no `unsafe`.
    pub fn over_stdout() -> std::io::Result<Self> {
        Ok(Self::over(std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?)))
    }

    /// stdio scans for the newline only when the write FITS in what is left of
    /// the buffer; a longer one takes the block path below whether or not it
    /// holds one, so a terminal is not a promise that every record is written
    /// when it ends.
    pub fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.line && bytes.len() <= self.room() {
            if let Some(i) = bytes.iter().rposition(|b| *b == b'\n') {
                self.held.extend_from_slice(bytes.get(..=i).unwrap_or_default());
                self.flush()?;
                return self.fill(bytes.get(i + 1..).unwrap_or_default());
            }
        }
        self.fill(bytes)
    }

    /// Fill what room is left, and if that was not enough, flush and hand the
    /// kernel every WHOLE block DIRECTLY rather than pumping each through the
    /// buffer: stdio's `do_write = to_do - to_do % block_size`. Only the
    /// sub-block tail is buffered, which is why one 8193-byte record reaches the
    /// descriptor as 8192 then 1 rather than as 4096, 4096, 1.
    fn fill(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let take = self.room().min(bytes.len());
        self.held.extend_from_slice(bytes.get(..take).unwrap_or_default());
        let rest = bytes.get(take..).unwrap_or_default();
        // An exact fill is not a reason to write: stdio flushes because it has
        // more to put and nowhere to put it, which is this test and not a full
        // buffer.
        if rest.is_empty() {
            return Ok(());
        }
        self.allocated = true;
        self.flush()?;
        // Under 128 bytes stdio does not bother aligning and writes the lot.
        let tail = match self.cap >= Self::ALIGN {
            true => rest.len() % self.cap,
            false => 0,
        };
        let whole = rest.len().saturating_sub(tail);
        self.direct(rest.get(..whole).unwrap_or_default())?;
        // What is left is shorter than the buffer, which is empty, so it fits
        // whatever happens below. A line-buffered stream does NOT get the
        // last-newline treatment here, though: stdio hands this remainder to
        // `_IO_default_xsputn`, which for a line buffer has no room to copy into
        // and so goes a byte at a time through `__overflow` -- writing at EVERY
        // newline rather than once at the final one.
        let mut left = rest.get(whole..).unwrap_or_default();
        if self.line {
            while let Some(i) = left.iter().position(|b| *b == b'\n') {
                self.held.extend_from_slice(left.get(..=i).unwrap_or_default());
                self.flush()?;
                left = left.get(i + 1..).unwrap_or_default();
            }
        }
        self.held.extend_from_slice(left);
        Ok(())
    }

    /// Past the buffer rather than through it. Partial writes are resumed from
    /// where the descriptor stopped, as stdio's own write loop does.
    fn direct(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            match self.file.write(bytes) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Keeps only what did NOT reach the descriptor, so a failure part way
    /// through leaves the tail rather than the whole -- a retry follows, and
    /// `write_all` would hand it back bytes the descriptor already took.
    pub fn flush(&mut self) -> std::io::Result<()> {
        while !self.held.is_empty() {
            match self.file.write(&self.held) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.held.drain(..n.min(self.held.len()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// stdio's own teardown, which `exit` runs over every open stream: it writes
/// what it can and reports nothing. That is what carries the buffered output of
/// a run that ends by `?`-ing out of the middle of itself, where no explicit
/// flush is reached -- and, dropping in declaration order, what makes a `w`
/// target reach its file before the sink reaches descriptor 1.
impl Drop for StdioBuf {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

impl std::fmt::Debug for StdioBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `held` is shown by LENGTH and `allocated` at all, those two being what
        // decide whether the next `put` buffers or goes to the descriptor.
        f.debug_struct("StdioBuf")
            .field("cap", &self.cap)
            .field("held", &self.held.len())
            .field("line", &self.line)
            .field("allocated", &self.allocated)
            .finish()
    }
}

/// Buffered stdout. `broken` latches once the reader has gone away so a caller
/// can stop early instead of reporting an I/O failure per line.
pub struct Out {
    inner: StdioBuf,
    broken: bool,
    /// Flush after every write, for sed's `-u`. What that buys is ORDER against
    /// another stream on the same descriptor -- a `w /dev/stdout` the special-file
    /// table did not alias is a separate unbuffered `File`, and which of the two
    /// lands first is otherwise a question about buffer sizes.
    unbuffered: bool,
}

impl Out {
    /// Fallible because it DUPLICATES descriptor 1 (see `StdioBuf::over_stdout`).
    /// The dup can only fail on a closed or exhausted descriptor table, which is
    /// exactly when a silent fallback to a second buffering layer would be worst.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self { inner: StdioBuf::over_stdout()?, broken: false, unbuffered: false })
    }

    /// Write through from here on, which is what `-u` asks for.
    pub fn unbuffer(&mut self) {
        self.unbuffered = true;
    }

    /// The most a caller may hand over at once and still get stdio's write
    /// boundaries -- see `StdioBuf::piece`.
    pub fn piece(&self) -> usize {
        self.inner.piece()
    }

    /// A complete line has been written. Flushes only under `-u`, and per LINE
    /// rather than per write because that is where GNU flushes (`output_line`):
    /// flushing each write would put a line's text and its separator in separate
    /// `write(2)`s, which a concurrent reader can tell apart.
    pub fn end_line(&mut self) -> std::io::Result<()> {
        match self.unbuffered {
            true => self.flush(),
            false => Ok(()),
        }
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.broken {
            return Ok(());
        }
        match self.inner.put(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.broken = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        if self.broken {
            return Ok(());
        }
        match self.inner.flush() {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.broken = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Split a buffer into records on `sep`, reporting for each whether it ended
/// with the separator. A trailing record without one is still a record.
pub fn records(buf: &[u8], sep: u8) -> Vec<(&[u8], bool)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in buf.iter().enumerate() {
        if *b == sep {
            if let Some(rec) = buf.get(start..i) {
                out.push((rec, true));
            }
            start = i + 1;
        }
    }
    if start < buf.len() {
        if let Some(rec) = buf.get(start..) {
            out.push((rec, false));
        }
    }
    out
}

/// Something the walk has to say about a path.
pub enum Diag {
    /// A directory that could not be read. An error: it sets the exit status.
    Failed(std::io::Error),
    /// A directory skipped for being its own ancestor. GNU WARNS and carries on,
    /// so this one leaves the status to the search.
    Loop,
}

/// What a walk produced.
/// One name the walk produced, carrying what the walk's OWN stat already said so
/// that nothing downstream has to ask again.
pub struct Found {
    pub path: PathBuf,
    /// Character, block, socket or FIFO. False where the stat failed, which
    /// leaves the open to report it as it reports every other failure.
    pub device: bool,
    /// The ROOT of the walk, which GNU counts as a COMMAND-LINE name however far
    /// the walk goes below it (`fts_level == FTS_ROOTLEVEL`). It is the one place
    /// "the walk produced this" and "this is not an operand" come apart, and the
    /// device default is what asks: `grep -r PAT fifo` READS it, where the same
    /// fifo found under a directory is skipped.
    pub root: bool,
}

pub struct Walked {
    /// Files to search: each directory's entries in name order, and one
    /// descended where it was met — which is NOT the whole list in path order,
    /// since `-` and `.` sort below `/`.
    pub files: Vec<Found>,
    /// In the order the walk MET them, both kinds in one list: GNU interleaves
    /// its errors and its cycle warnings, and two lists could only be drained one
    /// after the other.
    pub diags: Vec<(PathBuf, Diag)>,
    /// Whether the root was a directory; the caller names its output on it, and a
    /// second copy of the test would drift the moment this one changes.
    pub descended: bool,
}

/// Directory walk for `grep -r`/`-R`: each directory's entries sorted, and one
/// descended where it is met, for a deterministic listing — GNU passes fts no
/// comparator (grep.c:1868) and so takes the kernel's order, which is not
/// reproducible across checkouts.
///
/// `logical` is `-R`. Without it the walk is PHYSICAL: the root is followed but
/// nothing below it is, so a symlink named as an operand is descended and one
/// found by the walk is skipped — and a symlink is the ONLY thing this walk
/// drops, that being `fts`'s FTS_PHYSICAL rather than a policy. Every other
/// non-directory is collected, devices included; whether one is SEARCHED is the
/// `--devices` policy's answer and not the walk's, which is why each is reported
/// with what the stat said rather than filtered on it. With `logical` every
/// symlink is followed, and `-R` also flips that policy's default, which is why
/// that flag CAN block where `-r` cannot. Following symlinks is the
/// usual way a directory becomes reachable from inside itself, but not the only one
/// -- a bind mount does it without any symlink -- so BOTH walks carry the ancestor
/// chain and refuse to re-enter one, as GNU's does.
///
/// Errors are collected rather than propagated: one 0700 subdirectory must not
/// discard the whole walk, which is what `?` here used to do — `grep -r` then
/// reported nothing at all and blamed the root. GNU searches what it can reach
/// and reports each directory it cannot.
///
/// `prune` is `--exclude-dir`, and it is a parameter of the WALK rather than a
/// filter over its result because an excluded directory must never be ENTERED:
/// GNU reports neither a permission error from inside one nor a symlink cycle
/// through one, and either would still be collected by a walk that filtered
/// afterwards. It is asked with the directory's path and whether that path is
/// the root, since the two are matched against different things.
pub fn walk(root: &Path, logical: bool, prune: &dyn Fn(&Path, bool) -> bool) -> Walked {
    let mut out = Walked { files: Vec::new(), diags: Vec::new(), descended: false };
    out.descended = walk_from(root, true, logical, prune, &mut Vec::new(), &mut out);
    out
}

/// `follow` is true for the operand and false for what the walk finds, which is
/// the asymmetry above; `logical` follows both. It is a parameter rather than a
/// choice of stat per call site so that a directory entry swapped for a symlink
/// between the two cannot be descended.
fn walk_from(
    root: &Path,
    follow: bool,
    logical: bool,
    prune: &dyn Fn(&Path, bool) -> bool,
    chain: &mut Vec<(u64, u64)>,
    out: &mut Walked,
) -> bool {
    let meta = match stat(root, follow || logical) {
        Ok(m) => m,
        Err(e) => {
            // A name that EXISTS without RESOLVING — a dangling symlink, a loop —
            // is passed on rather than reported here, because a search that never
            // opens it must stay silent: `grep -rm0 a dangling` is GNU's exit 1
            // with nothing said. The open reports it in every other case.
            match std::fs::symlink_metadata(root).is_ok() {
                true => out.files.push(Found {
                    path: root.to_path_buf(),
                    device: false,
                    root: true,
                }),
                false => out.diags.push((root.to_path_buf(), Diag::Failed(e))),
            }
            return false;
        }
    };
    if !meta.is_dir() {
        // `follow` is true here, so this is the walk's ROOT: an operand that
        // happened not to be a directory, and a command-line name to the policy.
        out.files.push(Found { path: root.to_path_buf(), device: is_device(&meta), root: true });
        return false;
    }
    chain.push(ident(&meta));
    let reader = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            out.diags.push((root.to_path_buf(), Diag::Failed(e)));
            chain.pop();
            return true;
        }
    };
    // The ROOT is excluded only once it has been opened, and the ordering is
    // measurable: `-r --exclude-dir=X PAT X` reports `Permission denied` when X
    // is unreadable and says nothing at all when X is readable. GNU's grepdesc
    // filters a descriptor it already holds, so the open's error outranks the
    // exclusion. A directory the WALK found is pruned before any open instead,
    // which is why an unreadable one NESTED under an excluded name stays quiet.
    //
    // Gated on `follow` because this function RECURSES: `follow` is true only
    // for the walk's own root, and the root is the one directory whose name is
    // matched as an operand rather than on its last component. Ungated, every
    // nested directory was asked twice and the second question was the wrong
    // one -- `--exclude-dir='e/f'` then pruned `e/f`, which GNU does not.
    if follow && prune(root, true) {
        chain.pop();
        return true;
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in reader {
        match entry {
            Ok(e) => entries.push(e.path()),
            Err(e) => out.diags.push((root.to_path_buf(), Diag::Failed(e))),
        }
    }
    entries.sort();
    for path in entries {
        match stat(&path, logical) {
            // The exclusion is asked BEFORE the cycle test, as GNU's grepdirent
            // asks it: `-R --exclude-dir=up` over a directory reachable through
            // itself warns about no loop, because the loop is never reached.
            Ok(meta) if meta.is_dir() => {
                if !prune(&path, false) {
                    match chain.contains(&ident(&meta)) {
                        true => out.diags.push((path, Diag::Loop)),
                        false => {
                            walk_from(&path, false, logical, prune, chain, out);
                        }
                    }
                }
            }
            // A SYMLINK found by a physical walk is skipped and never reaches the
            // search: that is `fts`'s FTS_PHYSICAL rather than any policy, so no
            // option turns it back on. A logical walk resolved it before the test
            // and so never sees one. Everything else goes through, fifos, sockets
            // and device nodes included -- which of them is SEARCHED is the
            // `--devices` policy's answer, and it is carried rather than re-asked.
            Ok(meta) if logical || !meta.file_type().is_symlink() => {
                out.files.push(Found { path, device: is_device(&meta), root: false });
            }
            Ok(_) => {}
            // Only a logical walk gets here, on a link that does not resolve. Passed
            // on for the same reason the root is: the OPEN reports it, so a search
            // that never opens stays silent as GNU's does.
            Err(e) => match std::fs::symlink_metadata(&path).is_ok() {
                true => out.files.push(Found { path, device: false, root: false }),
                false => out.diags.push((path, Diag::Failed(e))),
            },
        }
    }
    chain.pop();
    true
}

fn stat(path: &Path, follow: bool) -> std::io::Result<std::fs::Metadata> {
    match follow {
        true => std::fs::metadata(path),
        false => std::fs::symlink_metadata(path),
    }
}

/// The pair that says "the same directory": a logical walk re-entering one is a
/// cycle, and no path comparison can tell, two names resolving to one inode.
fn ident(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Bytes of a path, for output that must round-trip a non-UTF-8 name.
pub fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

/// Decimal rendering without `format!`'s UTF-8 detour.
pub fn number(n: u64) -> Vec<u8> {
    let mut digits = Vec::new();
    let mut v = n;
    loop {
        digits.push(b'0' + u8::try_from(v % 10).unwrap_or(0));
        v /= 10;
        if v == 0 {
            break;
        }
    }
    digits.reverse();
    digits
}

/// `fnmatch(3)` with none of its flags, which is what GNU grep's `--include`,
/// `--exclude` and `--exclude-dir` compare with: `*` crosses a `/` and matches a
/// leading dot, `?` takes exactly one byte, `[...]` is a set with `!`/`^`
/// negation and `a-z` ranges, and `\` escapes the byte after it. Bytes rather
/// than characters, the corpus being scored in the C locale.
///
/// Iterative with ONE backtrack point rather than recursive: a pattern of many
/// stars against a long name would otherwise recurse once per star per position,
/// and a `--include` glob is attacker-supplied in exactly the way a pattern is.
pub fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    glob_match_with(pat, name, caret_negates())
}

/// glibc reads `POSIXLY_CORRECT` once and keeps the answer for the life of the
/// process, so a program's own later `setenv` cannot change what a glob means
/// mid-run. Cached here for that reason, not for speed.
fn caret_negates() -> bool {
    static CARET: OnceLock<bool> = OnceLock::new();
    *CARET.get_or_init(|| !posixly_correct())
}

/// The matcher proper. `caret` is whether a leading `^` negates a set, which is
/// the environment's answer rather than the pattern's -- taking it as an
/// argument is what lets one process test both.
fn glob_match_with(pat: &[u8], name: &[u8], caret: bool) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    // Where the tail after the most recent `*` restarts when it fails.
    let mut resume: Option<(usize, usize)> = None;
    loop {
        if pat.get(p) == Some(&b'*') {
            // A run of stars is one star; remembering the first is enough.
            while pat.get(p) == Some(&b'*') {
                p = p.saturating_add(1);
            }
            resume = Some((p, n));
            continue;
        }
        let step = match p == pat.len() {
            // Pattern spent: a match only if the name is spent too. Otherwise
            // fall through and let a `*` behind us eat the rest.
            true if n == name.len() => return true,
            true => None,
            false => glob_step(pat, p, name, n, caret),
        };
        match step {
            Some((np, nn)) => (p, n) = (np, nn),
            None => match resume {
                Some((rp, rn)) if rn < name.len() => {
                    n = rn.saturating_add(1);
                    p = rp;
                    resume = Some((rp, n));
                }
                _ => return false,
            },
        }
    }
}

/// One pattern item against one name byte. `None` is a mismatch, which the
/// caller answers by backtracking rather than by failing outright.
fn glob_step(pat: &[u8], p: usize, name: &[u8], n: usize, caret: bool) -> Option<(usize, usize)> {
    let c = *pat.get(p)?;
    let g = *name.get(n)?;
    match c {
        b'?' => Some((p.saturating_add(1), n.saturating_add(1))),
        // An unclosed `[` is not a set at all: fnmatch reads it as the byte,
        // which is the literal compare at the bottom of this match.
        b'[' => match glob_class(pat, p, g, caret) {
            Class::Closed(next, true) => Some((next, n.saturating_add(1))),
            Class::Closed(_, false) => None,
            // The `[` was an ordinary byte after all; the caller re-reads what
            // follows it as pattern, which is what the literal arm below does.
            Class::Literal if g == b'[' => Some((p.saturating_add(1), n.saturating_add(1))),
            Class::Literal => None,
            Class::Invalid => None,
        },
        b'\\' => match pat.get(p.saturating_add(1)) {
            Some(&e) if e == g => Some((p.saturating_add(2), n.saturating_add(1))),
            // A TRAILING backslash escapes nothing and matches NOTHING: glibc
            // answers FNM_NOMATCH there rather than taking it as a literal, so
            // a `--include=a\` selects no file at all.
            _ => None,
        },
        _ if c == g => Some((p.saturating_add(1), n.saturating_add(1))),
        _ => None,
    }
}

/// How a `[...]` ended.
enum Class {
    /// Closed: index just past the `]`, and whether the byte is in the set.
    Closed(usize, bool),
    /// Ran off the end at an ordinary position. glibc then reads the `[` as an
    /// ordinary byte and re-parses what follows -- so `[a` matches `[a`, and
    /// `[\]` matches `[]` because the escape survives the fallback.
    Literal,
    /// Ran off the end INSIDE a range or a `[.`/`[:`/`[=` construct. There is no
    /// fallback here: glibc answers FNM_NOMATCH for the whole pattern, which is
    /// why `[a-` matches nothing at all where `[a` matches itself.
    Invalid,
}

/// The `[...]` opening at `open`, tested against `g`.
fn glob_class(pat: &[u8], open: usize, g: u8, caret: bool) -> Class {
    let mut i = open.saturating_add(1);
    // `!` always negates. `^` does so only when POSIXLY_CORRECT is UNSET; under
    // it glibc reads the `^` as an ordinary member, so `[^^]` selects `^` alone.
    let negate = pat.get(i) == Some(&b'!') || (caret && pat.get(i) == Some(&b'^'));
    if negate {
        i = i.saturating_add(1);
    }
    let mut found = false;
    // A `]` FIRST in the set is the byte, not the close -- `[]]` holds one `]`.
    let mut first = true;
    loop {
        // glibc stops TESTING at the first item that matches and walks the rest
        // of the set instead, with a stricter eye than the scan has: see
        // `glob_skip`. `[a[=]` selects `[` and `=` and NOT `a`, and that is the
        // whole of the difference.
        if found {
            return match glob_skip(pat, i) {
                Ok(end) => Class::Closed(end, found != negate),
                Err(class) => class,
            };
        }
        let Some(&c) = pat.get(i) else { return Class::Literal };
        if c == b']' && !first {
            return Class::Closed(i.saturating_add(1), found != negate);
        }
        first = false;
        // `[:alpha:]`, `[.a.]`, `[=a=]`. The delimiter repeats before the `]`.
        let construct = match (c, pat.get(i.saturating_add(1))) {
            // A CLASS name is scanned byte by byte, and glibc abandons the
            // CONSTRUCT -- not the pattern -- the moment a byte cannot belong
            // to a name, re-reading the `[` as an ordinary item. See
            // `glob_class_name` for where that boundary sits.
            (b'[', Some(b':')) => {
                let body = i.saturating_add(2);
                match glob_class_name(pat, body) {
                    Ok(end) => Some((b':', body, end)),
                    // Not a class after all: the `[` is an ordinary item.
                    Err(Class::Literal) => None,
                    Err(class) => return class,
                }
            }
            // `[=x=]` and `[.x.]` take EXACTLY one byte and their terminator
            // right after it; `glob_seek`-style scanning for a terminator
            // further along is not what glibc does. Where the shape does not
            // hold the two part company, and the asymmetry is measured rather
            // than reasoned: an equivalence class backs off to ordinary items
            // (`[[=ab]` is the set `[ = a b`) while a collating element VOIDS
            // the pattern (`[[.a]` and `[[.ab.]]` select nothing at all).
            (b'[', Some(&kind @ (b'.' | b'='))) => {
                let body = i.saturating_add(2);
                let shaped = pat.get(body).is_some()
                    && pat.get(body.saturating_add(1)) == Some(&kind)
                    && pat.get(body.saturating_add(2)) == Some(&b']');
                match (shaped, kind) {
                    (true, _) => Some((kind, body, body.saturating_add(1))),
                    (false, b'.') => return Class::Invalid,
                    (false, _) => None,
                }
            }
            _ => None,
        };
        // The low end of whatever comes next, where the pattern resumes, and
        // whether a COLLATING element produced it -- which changes how the `-`
        // after it is read.
        let (lo, mut next, collating) = match construct {
            Some((kind, body, end)) => {
                let name = pat.get(body..end).unwrap_or_default();
                let past = end.saturating_add(2);
                match kind {
                    // An UNKNOWN class name is an ERROR, not an empty set:
                    // glibc gives up on the pattern rather than matching
                    // nothing, so `[[:bogus:]x]` selects no `x`.
                    b':' => {
                        match glob_named_class(name, g) {
                            Some(true) => found = true,
                            Some(false) => {}
                            // An unknown name with nothing found yet gives the
                            // whole pattern up. There is no "already found"
                            // case to weigh against it here: the loop returns
                            // through `glob_skip` the moment anything matches,
                            // so `found` is false wherever this is reached.
                            // `[a[:bogus:]z]Q` selecting `aQ` is that walk's
                            // doing -- it never looks a name up -- not this
                            // arm's.
                            None => return Class::Invalid,
                        }
                        i = past;
                        continue;
                    }
                    // A COLLATING element of one byte is that byte here, the C
                    // locale having no others, and it is a range endpoint like
                    // any other: `[[.a.]-z]` selects `b`, where POSIX regex
                    // refuses the very same spelling.
                    b'.' => match name {
                        [b] => (*b, past, true),
                        _ => {
                            i = past;
                            continue;
                        }
                    },
                    // An EQUIVALENCE class is an item and NOTHING more: it
                    // cannot end a range, so the `-` in `[[=a=]-z]` is an
                    // ordinary byte and that set is `a`, `-`, `z` rather than
                    // the span between them.
                    _ => {
                        if name == [g] {
                            found = true;
                        }
                        i = past;
                        continue;
                    }
                }
            }
            // `\` escapes inside a bracket too, which POSIX regex does not do.
            None => match c {
                b'\\' => match pat.get(i.saturating_add(1)) {
                    Some(&e) => (e, i.saturating_add(2), false),
                    None => return Class::Invalid,
                },
                _ => (c, i.saturating_add(1), false),
            },
        };
        match glob_range(pat, &mut next, lo, g, collating, &mut found) {
            Ok(()) => {}
            Err(end) => return end,
        }
        i = next;
    }
}

/// `a-z` at `*i`, against the low end already read. Advances past what it takes.
/// `Err` carries how the whole bracket ends.
fn glob_range(
    pat: &[u8],
    i: &mut usize,
    lo: u8,
    g: u8,
    collating: bool,
    found: &mut bool,
) -> Result<(), Class> {
    let after = i.saturating_add(1);
    let hi_at = match pat.get(*i) {
        Some(b'-') => match pat.get(after) {
            // A `-` running off the END is an unfinished range, and glibc gives
            // up rather than falling back -- which is the whole difference
            // between `[a-`, matching nothing, and `[a`, matching `[a`. Except
            // after a bare `[`, where it falls back after all: a quirk of its
            // error path, not a rule, since `[[-a]` IS the range `[`..`a` and
            // selects `]`. Measured, not reasoned.
            None if lo == b'[' => return Err(Class::Literal),
            None => return Err(Class::Invalid),
            // A `-` just before the close is an ordinary byte -- but glibc
            // decides "this is a range" one way after a COLLATING element and
            // another after a plain byte, and the two spellings differ by
            // exactly this case. After a plain byte it is not a range, so the
            // byte still stands as an item and `[a-]` selects `a`. After
            // `[.a.]` it IS a range, which skips the item test and then
            // declines for want of a high end -- so `[[.a.]-]` selects the `-`
            // and NOT the `a`. Measured; the two lines differ by one clause in
            // glibc's own source.
            Some(b']') => None,
            Some(_) => Some(after),
        },
        _ => None,
    };
    match hi_at {
        Some(at) => {
            let (hi, past) = match (pat.get(at), pat.get(at.saturating_add(1))) {
                (Some(b'\\'), Some(&e)) => (e, at.saturating_add(2)),
                (Some(b'\\'), None) => return Err(Class::Invalid),
                // A collating element closes a range as well as opening one:
                // `[a-[.z.]]` is `a`..`z`, not `a`..`[`. It is the ONLY
                // construct that does. A `[:class:]` or `[=equiv=]` after the
                // `-` is not an endpoint and does not start a construct here
                // either: the `[` is the endpoint BYTE and the scan resumes at
                // the `:` or `=`, which then read as ordinary items. So
                // `[x-[:alpha:]]` is the empty range `x`..`[` plus the items
                // `:alpha:`, closed by the FIRST `]`, leaving the second `]`
                // to match literally -- which is why it selects nothing of one
                // byte. Reading those items as a class instead made
                // `[x-[:alpha:]]` select `a`, and erroring on them broke the
                // UNCLOSED `[x-[:alpha:]`, which glibc answers by this same
                // route. Measured in all three directions.
                (Some(b'['), Some(b'.')) => {
                    // Exactly one byte then `.]`, as everywhere else a
                    // collating element is read; any other shape voids the
                    // pattern rather than falling back.
                    let body = at.saturating_add(2);
                    let shaped = pat.get(body.saturating_add(1)) == Some(&b'.')
                        && pat.get(body.saturating_add(2)) == Some(&b']');
                    match (shaped, pat.get(body)) {
                        (true, Some(&e)) => (e, body.saturating_add(3)),
                        _ => return Err(Class::Invalid),
                    }
                }
                (Some(&e), _) => (e, at.saturating_add(1)),
                (None, _) => return Err(Class::Invalid),
            };
            if lo <= g && g <= hi {
                *found = true;
            }
            *i = past;
        }
        // No high end. The low end stands as an item unless glibc had already
        // called this a range, which after a collating element it has.
        None if collating && pat.get(*i) == Some(&b'-') => {}
        None => {
            if lo == g {
                *found = true;
            }
        }
    }
    Ok(())
}

/// glibc's post-match walk over the rest of a bracket set. Once an item has
/// matched, the remainder is walked rather than tested, and the walk re-parses
/// what the scan already passed under *different* rules -- which is why `[a[=]`
/// selects `[` and `=` but not `a`: the `a` matches, then the walk meets the
/// malformed `[=` and voids the whole pattern. A `[:...:]` is only shape-checked
/// here, its NAME never looked up. The unterminated-set literal fallback still
/// applies, so `*[[` selects `[[`. Returns the index past the closing `]`.
/// glibc also voids on a trailing `\` here, but that is not observable: the
/// fallback re-reads the same trailing `\` in ordinary position, where it
/// matches nothing either.
fn glob_skip(pat: &[u8], from: usize) -> Result<usize, Class> {
    let mut i = from;
    loop {
        let Some(&c) = pat.get(i) else { return Err(Class::Literal) };
        i = i.saturating_add(1);
        if c == b']' {
            return Ok(i);
        }
        match (c, pat.get(i)) {
            (b'\\', Some(_)) => i = i.saturating_add(1),
            (b'[', Some(b':')) => i = glob_skip_name(pat, i)?,
            (b'[', Some(b'=')) => {
                let body = i.saturating_add(1);
                let shaped = pat.get(body).is_some()
                    && pat.get(body.saturating_add(1)) == Some(&b'=')
                    && pat.get(body.saturating_add(2)) == Some(&b']');
                if !shaped {
                    return Err(Class::Invalid);
                }
                i = body.saturating_add(3);
            }
            (b'[', Some(b'.')) => match glob_skip_dot(pat, i) {
                Some(end) => i = end,
                None => return Err(Class::Invalid),
            },
            _ => {}
        }
    }
}

/// `[:` inside the skip walk. The name is never looked up, only shaped: a byte
/// outside `a`..`y` backs the construct off to its own `:`, which the walk then
/// reads as an ordinary byte. So `[a[:]` ends at the `]` right after the colon,
/// while `[a[:zz:]]` runs on to the FIRST `]` and leaves the second one in the
/// pattern. Running off the end backs off too, and the walk then falls back.
fn glob_skip_name(pat: &[u8], colon: usize) -> Result<usize, Class> {
    let mut i = colon;
    let mut seen = 0usize;
    loop {
        i = i.saturating_add(1);
        seen = seen.saturating_add(1);
        // glibc gives up on an over-long name before it looks for the `:]`,
        // so the bound bites one byte sooner here than in the scan.
        if seen == CLASS_NAME_CAP {
            return Err(Class::Invalid);
        }
        if pat.get(i) == Some(&b':') && pat.get(i.saturating_add(1)) == Some(&b']') {
            return Ok(i.saturating_add(2));
        }
        match pat.get(i) {
            Some(b'a'..=b'y') => {}
            _ => return Ok(colon),
        }
    }
}

/// glibc's `CHAR_CLASS_MAX_LENGTH`, which on Linux is the fixed
/// `CHARCLASS_NAME_MAX` from `bits/local_lim.h` rather than anything a locale
/// varies.
const CLASS_NAME_CAP: usize = 2048;

/// `[.` inside the skip walk, which unlike the scan's one-byte collating
/// element SEARCHES for the next `.]` -- so `[a[.xy.]]` selects `a`.
fn glob_skip_dot(pat: &[u8], dot: usize) -> Option<usize> {
    let mut i = dot;
    loop {
        i = i.saturating_add(1);
        match pat.get(i) {
            None => return None,
            Some(&b'.') if pat.get(i.saturating_add(1)) == Some(&b']') => {
                return Some(i.saturating_add(2))
            }
            Some(_) => {}
        }
    }
}

/// glibc's class-name scan (`fnmatch_loop.c`): bytes up to a `:` that is
/// followed by `]`, whose index is returned. `None` means this is NOT a class
/// after all and the `[` must be re-read as an ordinary item -- glibc decides
/// that on the first byte outside `a`..`y`, END OF PATTERN included. `z` is
/// outside on purpose (`c < 'a' || c >= 'z'` there), which is why `[[:az:]]`
/// selects `a]` as the set `[ : a z :` and a literal `]`. A name that ENDS
/// properly and is merely UNKNOWN is a different answer, made by the caller.
///
/// `Err(Literal)` is that re-read; `Err(Invalid)` is the length bound, which
/// glibc tests BEFORE it reads the next byte and answers by giving the whole
/// pattern up rather than by backing off. That is one byte later than the same
/// bound in `glob_skip_name`, which tests after reading.
fn glob_class_name(pat: &[u8], from: usize) -> Result<usize, Class> {
    let mut i = from;
    loop {
        if i.saturating_sub(from) == CLASS_NAME_CAP {
            return Err(Class::Invalid);
        }
        let Some(&c) = pat.get(i) else { return Err(Class::Literal) };
        if c == b':' && pat.get(i.saturating_add(1)) == Some(&b']') {
            return Ok(i);
        }
        if !matches!(c, b'a'..=b'y') {
            return Err(Class::Literal);
        }
        i = i.saturating_add(1);
    }
}

/// The ASCII classes, as `regex.rs` spells them -- the crate is scored in the C
/// locale, so each is its `is_ascii_*` predicate. `None` is a name that is not a
/// class at all, which is an error rather than an empty set.
fn glob_named_class(name: &[u8], g: u8) -> Option<bool> {
    Some(match name {
        b"alpha" => g.is_ascii_alphabetic(),
        b"digit" => g.is_ascii_digit(),
        b"alnum" => g.is_ascii_alphanumeric(),
        b"upper" => g.is_ascii_uppercase(),
        b"lower" => g.is_ascii_lowercase(),
        b"space" => g.is_ascii_whitespace() || g == 0x0b,
        b"blank" => g == b' ' || g == b'\t',
        b"punct" => g.is_ascii_punctuation(),
        b"print" => g.is_ascii_graphic() || g == b' ',
        b"graph" => g.is_ascii_graphic(),
        b"cntrl" => g.is_ascii_control(),
        b"xdigit" => g.is_ascii_hexdigit(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The LINE-buffered arm, which no corpus case can reach: stdio picks it for
    /// a terminal and the harness hands its children pipes. The rule is stdio's
    /// -- everything through the LAST newline goes out and the tail waits -- and
    /// it is `\n` rather than sed's record separator, which is why `-z` cannot be
    /// used to test it either.
    #[test]
    fn a_line_buffered_stream_writes_through_its_last_newline() {
        // Per-process: two builds of this crate under test at once -- a drift run
        // beside an ordinary one -- would otherwise read each other's files.
        let dir = std::env::temp_dir().join(format!("td-txt-stdiobuf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let path = dir.join("t");
        let mut buf = StdioBuf {
            file: std::fs::File::create(&path).unwrap(),
            held: Vec::new(),
            cap: 4096,
            line: true,
            allocated: false,
        };
        // No newline: nothing leaves, however much is written.
        buf.put(b"abc").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        // A newline takes the held bytes with it, and only up to the last one.
        buf.put(b"de\nfg\nhi").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcde\nfg\n");
        // The tail is still held, and the explicit flush is what ends it.
        buf.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcde\nfg\nhi");

        // A write LONGER than the room is not scanned for a newline at all --
        // it fills, flushes and blocks out like any other -- so a terminal is
        // no promise that a record is written when it ends.
        let path = dir.join("l");
        let mut buf = StdioBuf {
            file: std::fs::File::create(&path).unwrap(),
            held: Vec::new(),
            cap: 4096,
            line: true,
            allocated: true,
        };
        let mut long = vec![b'x'; 10];
        long.push(b'\n');
        long.extend(std::iter::repeat(b'y').take(5000));
        buf.put(&long).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            4096,
            "the newline at 10 did not cut the write short"
        );

        // A NUL is not a newline to stdio, whatever `-z` made the separator.
        let path = dir.join("z");
        let mut buf = StdioBuf {
            file: std::fs::File::create(&path).unwrap(),
            held: Vec::new(),
            cap: 4096,
            line: true,
            allocated: false,
        };
        buf.put(b"a\0b\0").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");

        // WHOLE BLOCKS go to the kernel directly rather than through the buffer,
        // and the first write of all finds no buffer to fill: 1000 bytes over a
        // 256-byte block write three blocks at once and hold the tail.
        let path = dir.join("b");
        let mut buf = StdioBuf {
            file: std::fs::File::create(&path).unwrap(),
            held: Vec::new(),
            cap: 256,
            line: false,
            allocated: false,
        };
        let first: Vec<u8> = b"0123456789".iter().copied().cycle().take(1000).collect();
        buf.put(&first).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().as_slice(), first.get(..768).unwrap());
        // An EXACT fill still leaves it held: stdio overflows rather than tops up.
        buf.put(&[b'z'; 24]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 768, "an exact fill does not write");
        // Having more to put and nowhere to put it is what writes.
        buf.put(b"!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 1024);
        buf.flush().unwrap();
        let mut want = first.clone();
        want.extend_from_slice(&[b'z'; 24]);
        want.push(b'!');
        assert_eq!(std::fs::read(&path).unwrap(), want);

        // Under 128 bytes stdio does not align at all and writes the lot, which
        // is why the arithmetic above is not simply a count of whole blocks.
        let path = dir.join("s");
        let mut buf = StdioBuf {
            file: std::fs::File::create(&path).unwrap(),
            held: Vec::new(),
            cap: 8,
            line: false,
            allocated: false,
        };
        buf.put(b"ab\ncd\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"ab\ncd\n");

        // Capacity and mode come from the DESCRIPTOR, not from constants -- but
        // the descriptor only gets to LOWER it: stdio starts at BUFSIZ and takes
        // `st_blksize` when that is positive and smaller. Every `st_blksize` on
        // this platform is 4096, so what this can tell apart is the wiring being
        // replaced by some other number; the BUFSIZ ceiling itself is unreachable
        // here and is asserted by arithmetic instead.
        use std::os::unix::fs::MetadataExt as _;
        let file = std::fs::File::create(dir.join("c")).unwrap();
        let block = usize::try_from(file.metadata().unwrap().blksize()).unwrap();
        let want = match block > 0 && block < StdioBuf::BUFSIZ {
            true => block,
            false => StdioBuf::BUFSIZ,
        };
        let sized = StdioBuf::over(file);
        assert_eq!(sized.cap, want);
        assert!(sized.cap <= StdioBuf::BUFSIZ, "a descriptor may only lower it");
        assert!(!sized.line, "a regular file is not a terminal");

        drop((buf, sized));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The corpus harness always PIPES stdin, so no case can make a read on it
    /// fail; the rule is pinned here instead and checked by hand against GNU.
    #[test]
    fn a_read_error_names_the_standard_input_stream_stdin() {
        let stdin = Input::open(b"-", true).unwrap();
        assert_eq!(stdin.error_name(b"-"), b"stdin".to_vec());
        // The refusal `-i` would give it, though `-i` opens `-` as a name and so
        // never routes the stream here.
        assert_eq!(stdin.in_place_refusal(), Some("not a regular file"));
    }

    /// The whole open/read split rests on `open(2)` SUCCEEDING on a directory and
    /// the first read failing, which is a property of the kernel and of `std`, not
    /// of this crate -- so it is asserted rather than assumed.
    #[test]
    fn a_directory_opens_and_will_not_read() {
        let dir = std::env::temp_dir().join(format!("td-txt-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = dir.as_os_str().as_bytes();
        let mut opened = Input::open(name, true).expect("a directory opens");
        assert_eq!(opened.in_place_refusal(), Some("not a regular file"));
        assert_eq!(opened.error_name(name), name.to_vec());
        let err = opened.read_all().expect_err("and does not read").0;
        assert_eq!(errmsg(&err), "Is a directory");

        let file = dir.join("f");
        std::fs::write(&file, b"x").unwrap();
        let mut opened = Input::open(file.as_os_str().as_bytes(), true).unwrap();
        assert_eq!(opened.in_place_refusal(), None);
        assert_eq!(opened.read_all().unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The give-back moves the descriptor it was HANDED. It used to duplicate
    /// descriptor 0 itself, which is a second dup on top of the one the reader
    /// already holds: under descriptor pressure that one fails, the seek is
    /// silently skipped, and the next reader starts a whole block late. Handing
    /// the descriptor in is what removes the failure, so what is pinned here is
    /// that an unrelated one really is what moves.
    #[test]
    fn the_give_back_seeks_the_descriptor_it_was_handed() {
        let dir = std::env::temp_dir().join(format!("td-txt-giveback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, vec![b'x'; 100]).unwrap();

        let mut file = std::fs::File::open(&path).unwrap();
        let mut buf = [0u8; 40];
        file.read_exact(&mut buf).unwrap();
        assert_eq!(file.stream_position().unwrap(), 40);
        give_back_stdin(&mut file, 10).unwrap();
        assert_eq!(file.stream_position().unwrap(), 30);

        // Nothing to hand back is answered before any syscall, so a descriptor
        // that could not take a seek is still not an error.
        give_back_stdin(&mut file, 0).unwrap();
        assert_eq!(file.stream_position().unwrap(), 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_keeps_a_final_unterminated_line() {
        let r = records(b"a\nb\nc", b'\n');
        assert_eq!(r.len(), 3);
        assert_eq!(r.get(2).map(|(l, nl)| (*l, *nl)), Some((&b"c"[..], false)));
    }

    #[test]
    fn records_of_a_terminated_buffer_all_carry_the_separator() {
        let r = records(b"a\nb\n", b'\n');
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|(_, nl)| *nl));
    }

    #[test]
    fn empty_input_has_no_records() {
        assert!(records(b"", b'\n').is_empty());
    }

    #[test]
    fn version_matches_the_manifest() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn errmsg_drops_rusts_os_error_suffix() {
        let e = std::io::Error::from_raw_os_error(2);
        assert!(e.to_string().contains("(os error 2)"));
        assert_eq!(errmsg(&e), "No such file or directory");
        // A non-errno error has no suffix to drop.
        let other = std::io::Error::other("boom");
        assert_eq!(errmsg(&other), "boom");
    }

    /// The FIRST NUL, not the last: a message holding two would otherwise keep
    /// the text between them, which no C `%s` prints.
    #[test]
    fn cstr_stops_at_the_first_nul() {
        assert_eq!(cstr(b"a\0b\0c"), b"a");
        assert_eq!(cstr(b"abc"), b"abc");
        assert_eq!(cstr(b"\0abc"), b"");
        assert_eq!(cstr(b""), b"");
    }

    /// A byte goes in as ITSELF; `char::from` would send 0x80 as two.
    #[test]
    fn byte_in_splices_one_raw_byte() {
        assert_eq!(byte_in("a`", 0x80, "'"), b"a`\x80'".to_vec());
        assert_eq!(byte_in("", 0, ""), vec![0u8]);
    }

    /// `name_in` on its own: `byte_in` only ever hands it a ONE-byte name, and
    /// the call sites also pass an empty prefix (`NAME: why`) and an empty
    /// suffix (`... for NAME`).
    #[test]
    fn name_in_splices_a_raw_name_between_two_pieces() {
        assert_eq!(name_in("a: ", b"n\xffm", " :b"), b"a: n\xffm :b".to_vec());
        assert_eq!(name_in("", b"\xff\xfe", ": why"), b"\xff\xfe: why".to_vec());
        assert_eq!(name_in("for ", b"\x80", ""), b"for \x80".to_vec());
        // A lone continuation byte is the case a re-encoding would destroy, and
        // it survives here as itself rather than as the three of U+FFFD.
        assert_eq!(name_in("", b"\xc3", ""), vec![0xc3]);
        assert_eq!(name_in("x", b"", "y"), b"xy".to_vec());
    }

    #[test]
    fn number_renders_decimal() {
        assert_eq!(number(0), b"0".to_vec());
        assert_eq!(number(1207), b"1207".to_vec());
    }

    /// A reader that hands back scripted results, so the failure paths a real
    /// descriptor will not produce on demand can still be driven.
    struct Scripted(Vec<std::io::Result<&'static [u8]>>);

    impl std::io::Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            match self.0.remove(0) {
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf.get_mut(..n).unwrap_or_default().copy_from_slice(&bytes[..n]);
                    Ok(n)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn err(kind: std::io::ErrorKind) -> std::io::Result<&'static [u8]> {
        Err(std::io::Error::from(kind))
    }

    #[test]
    fn a_read_failure_does_not_swallow_the_bytes_read_before_it() {
        // "match" arrives with no separator, and THEN the read fails. Those
        // bytes are a record; the failure is owed too, but after them.
        let src = Scripted(vec![Ok(b"match"), err(std::io::ErrorKind::Other)]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap(), "the partial record was dropped");
        assert_eq!(rec.line(), b"match");
        assert!(!rec.terminated());
        assert!(rec.next().is_err(), "the deferred failure was never reported");
    }

    /// The partial record a failed read leaves behind is counted like the EOF
    /// record it resembles. `pending` is cleared when it is reported, so a
    /// reader that fails once and then yields more keeps going -- and without
    /// the count every offset after it is short by that record's length. grep
    /// stops at the error, but `Records` is public and this is what it promises.
    #[test]
    fn a_partial_record_from_a_failed_read_still_counts_its_bytes() {
        let src = Scripted(vec![
            Ok(b"match"),
            err(std::io::ErrorKind::Other),
            Ok(b"after\n"),
        ]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"match");
        assert_eq!(rec.offset(), 0);
        assert!(rec.next().is_err());
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"after");
        assert_eq!(rec.offset(), 5, "the partial record's bytes went uncounted");
    }

    #[test]
    fn a_failure_with_nothing_buffered_is_reported_at_once() {
        let src = Scripted(vec![err(std::io::ErrorKind::PermissionDenied)]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().is_err());
    }

    #[test]
    fn an_interrupted_read_is_retried_rather_than_reported() {
        let src = Scripted(vec![err(std::io::ErrorKind::Interrupted), Ok(b"a\nb\n")]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"a");
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"b");
        assert!(!rec.next().unwrap());
    }

    #[test]
    fn a_record_spanning_reads_is_assembled_whole() {
        let src = Scripted(vec![Ok(b"ab"), Ok(b"cd"), Ok(b"ef\ntail\n")]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"abcdef");
        assert!(rec.terminated());
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"tail");
    }

    /// End of input is a LATCH until something clears it, and `rewind(3)` clears
    /// it whether or not the seek took. sed's `-s` needs both halves: `restart`
    /// for a source it seeked, and this for one it could not — a fifo whose last
    /// writer went away and whose next may not have yet.
    #[test]
    fn end_of_input_is_forgotten_without_disturbing_the_position() {
        // The empty read is end of input; the bytes after it are a new writer.
        let src = Scripted(vec![Ok(b"a\nb\n"), Ok(b""), Ok(b"c\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 64);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"a");
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"b");
        assert!(!rec.next().unwrap(), "the empty read should be end of input");
        // Latched: without clearing it, nothing later is ever read.
        assert!(!rec.next().unwrap());
        rec.forget_eof();
        assert!(rec.next().unwrap(), "a cleared end of input should read again");
        assert_eq!(rec.line(), b"c");
    }

    /// What a `q` leaves behind is this: one record out costs ONE BLOCK off the
    /// source, not the whole of it. sed's 4 KiB is chosen so that number matches
    /// GNU's, and the arithmetic is only meaningful if the reader really stops
    /// at a block.
    #[test]
    fn a_record_costs_one_block_of_the_source_and_no_more() {
        let data: Vec<u8> = (0..2500u32).flat_map(|i| format!("{i:07}\n").into_bytes()).collect();
        assert!(data.len() > 8192, "the source must span several blocks");
        let mut rec = Records::with_buffer(std::io::Cursor::new(data.clone()), b'\n', 4096);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"0000000");
        assert_eq!(rec.consumed(), 4096, "one record took more than one block");
        // Records already in the buffer cost nothing further.
        for _ in 0..100 {
            assert!(rec.next().unwrap());
        }
        assert_eq!(rec.consumed(), 4096);
        // Reading past the block is what pulls the next one.
        while rec.consumed() == 4096 {
            assert!(rec.next().unwrap());
        }
        assert_eq!(rec.consumed(), 8192);
    }

    /// A record LONGER than the block still arrives whole, which is what stops a
    /// small block from turning into a correctness bug.
    #[test]
    fn a_record_longer_than_the_block_is_still_assembled_whole() {
        let mut data = vec![b'x'; 10_000];
        data.push(b'\n');
        data.extend_from_slice(b"tail\n");
        let mut rec = Records::with_buffer(std::io::Cursor::new(data), b'\n', 4096);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line().len(), 10_000);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"tail");
    }

    #[test]
    fn the_binary_flag_is_set_by_the_fill_and_stays_set() {
        let src = Scripted(vec![Ok(b"a\n"), Ok(b"\x00\n"), Ok(b"b\n")]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap());
        assert!(!rec.binary(), "a NUL-free first buffer reported binary");
        assert!(rec.next().unwrap());
        assert!(rec.binary(), "the buffer holding the NUL did not report binary");
        assert!(rec.next().unwrap());
        assert!(rec.binary(), "a later NUL-free buffer cleared the verdict");
    }

    /// The buffer the verdict trips ON is zapped, not just the ones after it.
    /// The carry is NOT what this pins, though the record here spans one: a NUL
    /// in an earlier buffer would have tripped the verdict THERE and been
    /// zapped there, so a spill can never hold one and the "keeps its earlier
    /// bytes" half has no failing case to write.
    #[test]
    fn zapping_starts_with_the_buffer_that_tripped_the_verdict() {
        let src = Scripted(vec![Ok(b"a"), Ok(b"x\x00y\n")]);
        let mut rec = Records::new(src, b'\n');
        rec.zap_nuls(true);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"ax", "the NUL did not end the record it landed in");
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"y", "the rest of that buffer is a record of its own");
    }

    /// A fill that is nothing but zeros is DROPPED rather than handed over as
    /// one empty record per byte, and the count it owes comes back.
    #[test]
    fn an_all_zero_fill_is_dropped_and_its_records_counted() {
        // The first read carries the NUL that trips the verdict, so the all-zero
        // read AFTER it is the one that may be dropped -- see the ordering test
        // below for why it cannot be the tripping read itself.
        let src = Scripted(vec![Ok(b"a\x00"), Ok(b"\x00\x00\x00\x00"), Ok(b"b\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 4);
        rec.zap_nuls(true);
        rec.skip_zero_fills(true);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"a");
        assert_eq!(rec.take_skipped().0, 0);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"b", "the all-zero fill was handed over rather than dropped");
        assert_eq!(rec.take_skipped().0, 4, "the dropped records were not counted");
        // And the debt is owed once: asking again after taking it reports none.
        assert_eq!(rec.take_skipped().0, 0);
    }

    /// A record still OPEN is JOINED across the dropped fill, which is GNU's
    /// behaviour and not an accident of it: the separators that would have
    /// ended `ab` were in the read that went away, so `ab` runs on into `c`.
    /// Measured against GNU 3.11, where `^foo.*bar$` selects such a record.
    #[test]
    fn an_open_record_is_joined_across_a_dropped_fill() {
        let src =
            Scripted(vec![Ok(b"a\x00"), Ok(b"ab"), Ok(b"\x00\x00\x00\x00"), Ok(b"c\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 4);
        rec.zap_nuls(true);
        rec.skip_zero_fills(true);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"a");
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"abc", "the open record was split by a dropped fill");
        assert_eq!(rec.take_skipped().0, 4, "the dropped records were not counted");
    }

    /// The read that TRIPS the verdict is never dropped, only later ones. GNU
    /// sets `skip_nuls` in the loop body, after `fillbuf` has already delivered
    /// that buffer, so it is kept and zapped -- which is what terminates a
    /// record still open at the head of a run rather than joining it. Under a
    /// newline separator this is the whole difference between GNU splitting and
    /// GNU joining, and a sweep of 4900 cells found it: 133 of them diverged
    /// while the tripping read was being dropped too.
    #[test]
    fn the_read_that_trips_the_verdict_is_never_dropped() {
        // `ab` is open, and the NEXT read is both all zeros and the first NUL
        // seen -- so it trips the verdict, is kept, and ends `ab` there.
        let src = Scripted(vec![Ok(b"ab"), Ok(b"\x00\x00\x00\x00"), Ok(b"c\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 4);
        rec.zap_nuls(true);
        rec.skip_zero_fills(true);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"ab", "the read that tripped the verdict was dropped");
        assert_eq!(rec.take_skipped().0, 0);
    }

    /// A record CARRIED across a buffer contributes its whole length to the
    /// count, not just the tail that completed it. Nothing above reaches this:
    /// a corpus case cannot span a 96 KiB buffer, and the conformance tests that
    /// can end their file at the joined record, so the offset AFTER a spilled
    /// one goes unread. Taking the tail alone leaves every later record short by
    /// the carry -- `e - s` here is 1 where the record is 5.
    #[test]
    fn a_record_carried_across_a_buffer_counts_its_whole_length() {
        let src = Scripted(vec![Ok(b"abcd"), Ok(b"e\nf\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 4);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"abcde");
        assert_eq!(rec.offset(), 0);
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"f");
        assert_eq!(rec.offset(), 6, "the carried bytes were not counted");
    }

    /// Off unless asked, so sed and `grep -a` see every empty record the bytes
    /// really hold.
    #[test]
    fn a_reader_that_was_not_asked_to_skip_keeps_the_empty_records() {
        let src = Scripted(vec![Ok(b"\x00\x00\x00\x00"), Ok(b"b\n")]);
        let mut rec = Records::with_buffer(src, b'\n', 4);
        rec.zap_nuls(true);
        for _ in 0..4 {
            assert!(rec.next().unwrap());
            assert_eq!(rec.line(), b"", "a zapped NUL is an empty record");
        }
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"b");
        assert_eq!(rec.take_skipped().0, 0);
    }

    /// `O_NONBLOCK` is a raw flag word copied out of Linux's headers, and nothing
    /// in the type system says it is the right one -- a wrong value is a
    /// different flag the kernel accepts, which is how `-D skip` on a fifo would
    /// come to block with every other test still green. So the KERNEL is asked:
    /// `/proc/self/fdinfo/N` reports the flags an open actually took, and the bit
    /// has to be present when asked for and absent when not.
    #[test]
    fn the_nonblock_flag_is_the_one_the_kernel_took() {
        let dir =
            std::env::temp_dir().join(format!("td-txt-oflag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        let name = path_bytes(&path);
        for (asked, want) in [(false, false), (true, true)] {
            let input = Input::open_maybe_nonblock(&name, false, asked).unwrap();
            let Input::File(file) = &input else { panic!("not a file") };
            use std::os::unix::io::AsRawFd;
            let info = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
            // No /proc is not a failure of the flag: say nothing rather than red.
            let Ok(text) = std::fs::read_to_string(&info) else { return };
            let flags = text.lines().find_map(|l| l.strip_prefix("flags:"));
            let Some(flags) = flags else { return };
            let bits = u32::from_str_radix(flags.trim(), 8).unwrap();
            assert_eq!(
                bits & O_NONBLOCK as u32 != 0,
                want,
                "asked for nonblock={asked}, kernel reports flags {flags}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Off unless asked, because sed reaches no binary verdict and must hand
    /// over the bytes it was given.
    #[test]
    fn a_reader_that_was_not_asked_to_zap_hands_the_nul_over() {
        let src = Scripted(vec![Ok(b"a"), Ok(b"x\x00y\n")]);
        let mut rec = Records::new(src, b'\n');
        assert!(rec.next().unwrap());
        assert_eq!(rec.line(), b"ax\x00y");
    }

    /// Every row is glibc `fnmatch(PATTERN, NAME, 0)`'s own answer, taken from
    /// a C oracle rather than from POSIX: most of the rules below are glibc
    /// quirks that no reading of the standard predicts, and the differential
    /// harness that found them is not committed, so these rows are what is
    /// left holding them.
    #[test]
    fn a_caret_negates_a_set_only_when_posixly_correct_is_unset() {
        // Whether `^` negates is the ENVIRONMENT's answer, not the pattern's:
        // glibc reads POSIXLY_CORRECT and, under it, `^` is an ordinary member.
        // `!` is unconditional. Every row measured against fnmatch(3) both ways.
        let rows: &[(&[u8], &[u8], bool, bool)] = &[
            (b"[^^]", b"^", false, true),
            (b"[^^]", b"a", true, false),
            (b"[^a]", b"b", true, false),
            (b"[^a]", b"^", true, true),
            (b"[!a]", b"b", true, true),
            (b"[^]", b"^", false, true),
        ];
        for (pat, name, unset, set) in rows {
            assert_eq!(glob_match_with(pat, name, true), *unset, "{pat:?} {name:?} unset");
            assert_eq!(glob_match_with(pat, name, false), *set, "{pat:?} {name:?} set");
        }
    }

    #[test]
    fn an_over_long_class_name_in_the_scan_voids_rather_than_backing_off() {
        // The scan has the same bound as the walk and answers differently at
        // it: past the bound glibc gives the pattern up instead of re-reading
        // the `[` as an item, so the `!` that would have ended the name never
        // gets to. It counts one byte later than the walk does.
        let pat = |n: usize| {
            let mut p = b"[[:".to_vec();
            p.resize(p.len().saturating_add(n), b'a');
            p.extend_from_slice(b"!]");
            p
        };
        assert!(glob_match_with(&pat(2047), b"[", true), "2047 backs off");
        assert!(!glob_match_with(&pat(2048), b"[", true), "2048 gives the pattern up");
    }

    #[test]
    fn an_over_long_class_name_in_the_walk_voids_where_one_byte_less_does_not() {
        // glibc counts the name's bytes before it looks for the `:]`, so its
        // bound bites one byte sooner in the walk than in the scan. Both rows
        // were measured against fnmatch(3).
        let pat = |n: usize| {
            let mut p = b"[a[:".to_vec();
            p.resize(p.len().saturating_add(n), b'a');
            p.extend_from_slice(b":]]");
            p
        };
        assert!(glob_match_with(&pat(2046), b"a", true), "2046 is still a set");
        assert!(!glob_match_with(&pat(2047), b"a", true), "2047 gives the pattern up");
    }

    #[test]
    fn glob_match_answers_as_glibc_fnmatch_does() {
        let cases: &[(&str, &str, bool, &str)] = &[
            ("*.c", "a.c", true, "a plain star"),
            ("*.c", "a.h", false, ""),
            ("?.c", "a.c", true, "one byte, exactly one"),
            ("?.c", "ab.c", false, ""),
            ("*", "a/b", true, "a star crosses a slash, there being no FNM_PATHNAME"),
            ("*", ".hidden", true, "and matches a leading dot, there being no FNM_PERIOD"),
            ("[ab].c", "b.c", true, "a set"),
            ("[!ab].c", "c.c", true, "and its negation"),
            ("[^ab].c", "c.c", true, "^ negates too"),
            ("[a-c].c", "b.c", true, "a range"),
            ("[c-a].c", "b.c", false, ""),
            ("a\\*c", "a*c", true, "a backslash escapes the star"),
            ("a\\*c", "abc", false, ""),
            ("a\\", "a\\", false, "a TRAILING backslash matches nothing at all"),
            ("a\\", "a", false, ""),
            ("[abc", "[abc", true, "an unclosed bracket falls back to a literal ["),
            ("[a-", "[a-", false, "except mid-RANGE, where it matches nothing"),
            ("[[-", "[[-", true, "and except [[-, which falls back anyway"),
            ("[[.a", "[[.a", false, "mid-[. matches nothing"),
            ("[[:bogus:]]", "a", false, "an unknown class name aborts the pattern"),
            ("[a[:bogus:]]", "a", true, "unless an item already matched, when the scan carries on"),
            ("[:alpha:]", "a", true, "a bare [: with no :] is literal while the bracket closes"),
            ("[:alpha:]", ":", true, "the same pattern is the SET of those bytes"),
            ("[[.a.]-c]", "b", true, "only a COLLATING element may end a range"),
            // glibc asks "is this a range?" differently after a collating
            // element than after a plain byte, and these four rows are the
            // whole of the difference.
            ("[[.a.]-]", "a", false, "after `[.a.]` a `-]` IS a range, so the item is never tested"),
            ("[[.a.]-]", "-", true, "...but the `-` it declines to use is still an item"),
            ("[a-]", "a", true, "after a plain byte the same `-]` is NOT a range, so `a` stands"),
            ("[[=a=]-]", "a", true, "and an equivalence class is not a collating element"),
            ("[[.a.]-x]", "b", true, "a genuine range from a collating low end is unaffected"),
            ("[a-[.z.]]", "q", true, "a COLLATING element ends a range: this is [a-z]"),
            ("[a-[:alpha:]]", "a", false, "a CLASS does not, so the `[` is the endpoint BYTE"),
            ("[a-[:alpha:]]", "a]", true, "...the bracket closes at the FIRST ], and the second is literal"),
            ("[a-[=z=]]", "b", false, "an equivalence class is not an endpoint either"),
            ("[a-[=z=]q]", "q", false, "`q` is past that first ], so it is not in the set"),
            ("[x-[:alpha:]", "a", true, "unclosed: x..[ is empty and `:alpha:` are the items"),
            ("[x-[:alpha:]", "x", false, "...and `x` is not among them, which is what tells the two apart"),
            ("[[:alpha:]-c]", "-", true, "BEFORE the -, the class is an item and the - a byte"),
            ("[[=a=]-c]", "-", true, "the same for an equivalence class"),
            ("[[.a.]-c]", "b", true, "but a collating element there DOES open a range"),
            ("[[.a.]-c]", "-", false, ""),
            ("[[=a=]-c]", "b", false, "[=x=] is a plain item, so the - beside it is a byte"),
            ("[[=a=]-c]", "-", true, "and that byte is matchable"),
            ("[[:alpha:]-c]", "-", true, "a class is a plain item too"),
            ("[[:alpha:]]", "q", true, "a class inside a bracket is a class"),
            ("[[:digit:]]", "q", false, ""),
            // Once an item matches, glibc re-walks the REST of the set under
            // different rules than the scan that got there. These pin that walk.
            ("[a[=]", "a", false, "a malformed [= in the walk voids the whole pattern"),
            ("[a[=]", "[", true, "...so the set is exactly the two bytes the scan read first"),
            ("[a[=]", "=", true, ""),
            ("[a[.xy.]]", "a", true, "the walk SEARCHES for the next .], multi-byte or not"),
            ("[a[.xy.]]", "x", false, "where the scan takes one byte, so it never matched here"),
            ("[a[:]", "a", true, "a bad class name backs the walk off to its own colon"),
            ("[a[:zz:]]", "a", false, "z is outside a..y, so this backs off and ends at the FIRST ]"),
            ("[a[:zz:]]", "a]", true, "...leaving the second ] in the pattern, which is what proves it"),
            ("[a[:alpha:]]", "a", true, "a well-formed class name is shape-checked, never looked up"),
            ("*[[", "[[", true, "but an unterminated set still falls back after a match"),
            ("*[a[.xy.]]", "xa", true, "and a void is local to ONE star attempt, not the match"),
            ("low/c.c", "low/c.c", true, "the slash is an ordinary byte here"),
            ("", "", true, "the empty pattern matches only the empty name"),
            ("", "a", false, ""),
            ("*", "", true, "a star matches nothing at all"),
        ];
        for (pat, name, want, why) in cases {
            assert_eq!(
                // The flag is stated rather than read from the environment:
                // this table is about the pattern, and a maintainer with
                // POSIXLY_CORRECT exported must not get a different answer.
                glob_match_with(pat.as_bytes(), name.as_bytes(), true),
                *want,
                "fnmatch({pat:?}, {name:?}) is {want}: {why}"
            );
        }
    }
}
