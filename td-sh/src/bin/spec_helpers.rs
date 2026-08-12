//! What the Oils spec corpus expects to find on its PATH, without Python and
//! without coreutils. A MULTICALL, chosen by `argv[0]`, because that is how the
//! corpus invokes every one of them -- bare, off the one-entry PATH `run_case`
//! stages: the two Oils Python helpers (`argv.py`, `printenv.py`) and the
//! externals the corpus reaches for most that need no pattern engine (`cat`,
//! `mkdir`, `touch`, `rm`, `wc`, `sleep`, `seq`, `chmod`).
//!
//! The externals serve only the flags the corpus actually uses, counted rather
//! than guessed, and REFUSE everything else loudly with status 2 -- a coreutils
//! failure is 1, so a plausible status here would grade as the shell under test
//! having done something wrong. That is the rule the whole file turns on: this
//! is graded output, so a wrong answer is worse than a missing one.
//!
//! ## `argv.py` -- print the arguments as a Python list
//!
//! The Oils spec corpus asks 333 of its cases what a word EXPANDED to, and the
//! way it asks is `argv.py a b c`, whose goldens are a `repr` of the argument
//! list — `['a', 'b', 'c']`. 294 of those were `skip`ped by td-sh's overlay for
//! want of that helper, which was 77% of every skip: cases about word
//! splitting, quoting and expansion, the parts of a shell this corpus is most
//! worth running against. The helper is a few lines of Python upstream and is
//! not vendored here, so this is it, in the language the rest of td is.
//!
//! It is NOT part of the shell. `recipes/src/recipes/td-sh.rs` compiles
//! `src/main.rs` plus a named list of modules with a direct `rustc`, so a
//! second `[[bin]]` cannot reach the image even by accident; this exists for
//! `run_case`'s staged PATH and nothing else.
//!
//! What has to be right is `repr`, because the goldens ARE its output and a
//! quoting rule off by one case fails cases that have nothing to do with it.
//! The rule that matters most is that this is a BYTE repr, not a text one: an
//! argument is a sequence of bytes and every byte outside printable ASCII is
//! `\xNN`, so `μ` is `'\xce\xbc'` and not `'μ'`. That is not a guess about
//! Python versions — it is what the pinned corpus's own goldens contain, in
//! all nine that carry a high byte, and not one golden anywhere carries a
//! literal non-ASCII character. Reading it the other way round is a way to
//! fail cases the shell got RIGHT. The equivalent modern spelling, used by the
//! differential test, is `repr(os.fsencode(arg))` with the `b` prefix dropped.
//!
//! The rest is ordinary `repr`, in the order the rules apply:
//!   * the list is `[` + `, `-joined element reprs + `]`, and `[]` when empty;
//!   * an element is quoted with `'` unless it CONTAINS one and no `"`, in
//!     which case `"` is used and the `'` inside needs no escape;
//!   * `\` is doubled, and the quote actually used is escaped;
//!   * `\n`, `\r`, `\t` are those names, and every other byte outside
//!     printable ASCII — C0, DEL and everything from `\x80` up — is `\xNN`
//!     with LOWERCASE hex.
//!
//! Because the unit of work is one byte, there is no decoding, no lookahead
//! and no cursor: the loop below advances by exactly one byte per iteration
//! and terminates structurally. An earlier text-oriented draft did not, and
//! spun forever on a UTF-8 continuation byte.
//!
//! ## `printenv.py` -- what actually reached the ENVIRONMENT
//!
//! One line per NAME, its value or Python's `None` when unset. 42 cases use it,
//! and what they are asking is a question the shell cannot answer about itself:
//! `$FOO` reads a shell variable whether or not it was ever EXPORTED, so only a
//! child can report which names crossed into the environment. That is why the
//! env-binding, `export` and `set -a` cases reach for a helper at all.
//!
//! `None` rather than a blank line is the load-bearing part. The three states
//! the goldens distinguish are a name never exported, one exported with a
//! value, and one exported as the EMPTY STRING -- and the last two are what a
//! shell can confuse. Printing nothing for an unset name would make the first
//! and third identical and quietly pass cases that should fail.

// A bin is its own CRATE ROOT, so `main.rs`'s attribute does not reach here and
// nothing but this line keeps td-sh's "one scoped allow, in sys.rs" true of the
// whole crate. `forbid` rather than `deny` because no scoped allow belongs in
// this one, and `forbid` is the spelling a later `#[allow]` cannot override;
// `lib.rs`'s `every_crate_root_refuses_unsafe` pins it over the roots it
// DISCOVERS, since one added later would be as quiet as this was.
#![forbid(unsafe_code)]

use std::io::Write;
use std::os::unix::ffi::OsStrExt;

/// `repr` of one argument, appended to `out`.
fn repr(arg: &[u8], out: &mut String) {
    // `'` unless that would need escaping and `"` would not.
    let quote = if arg.contains(&b'\'') && !arg.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for &byte in arg {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b if b == quote as u8 => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(char::from(byte)),
            _ => push_hex(out, byte),
        }
    }
    out.push(quote);
}

/// `\xNN`, lowercase. Written nibble by nibble rather than through `format!`,
/// which would heap-allocate once per escaped byte.
fn push_hex(out: &mut String, byte: u8) {
    out.push_str("\\x");
    for nibble in [byte >> 4, byte & 0x0f] {
        // `from_digit` is Some for every value below the radix, and a nibble is.
        if let Some(digit) = char::from_digit(u32::from(nibble), 16) {
            out.push(digit);
        }
    }
}

/// `argv.py`: the argument list as Python would print it. Pure ASCII by
/// construction -- `repr` escapes every byte above `\x7e` -- but returned as
/// BYTES like its sibling, so `main` has one output type and no applet can
/// acquire a lossy conversion by being written differently from the other.
fn argv(args: &[std::ffi::OsString]) -> Vec<u8> {
    let mut text = String::from("[");
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            text.push_str(", ");
        }
        repr(arg.as_bytes(), &mut text);
    }
    text.push_str("]\n");
    text.into_bytes()
}

/// `printenv.py`: one line per NAME, its value or `None` when unset (see the
/// module doc for why `None` and not a blank line). No names is no output; the
/// corpus never invokes it bare, so that arm is this crate's choice rather than
/// something a golden derives.
fn printenv(args: &[std::ffi::OsString]) -> Vec<u8> {
    printenv_with(args, |name| std::env::var_os(name))
}

/// The lookup is a parameter so the rule above is testable: mutating this
/// process's environment from a test would race every other test in the binary,
/// and the distinction being pinned -- unset versus set-to-empty -- is exactly
/// the one a racing test would report at random.
fn printenv_with(
    args: &[std::ffi::OsString],
    lookup: impl Fn(&std::ffi::OsStr) -> Option<std::ffi::OsString>,
) -> Vec<u8> {
    let mut out = Vec::new();
    for name in args {
        // BYTES, not a lossy string. An environment value is a byte string on
        // Unix and `var_os` exists to preserve one; passing it through
        // `from_utf8_lossy` would replace every invalid byte with U+FFFD, so a
        // shell that carried the bytes correctly would be graded as though it
        // had mangled them. td-sh handles non-UTF-8 entries deliberately --
        // `a_non_utf8_environment_entry_does_not_abort_the_shell` -- so this
        // helper is exactly where that would go unnoticed. Python's own
        // `printenv.py` round-trips them through `surrogateescape`.
        match lookup(name) {
            Some(value) => out.extend_from_slice(value.as_bytes()),
            None => out.extend_from_slice(b"None"),
        }
        out.push(b'\n');
    }
    out
}

/// Split `args` at the first `--`, so a case can name a file that begins with a
/// dash. Returns the options and the operands; without a `--`, everything from
/// the first non-option on is an operand, which is what these four applets need
/// and is NOT the GNU permutation rule (they take no option after an operand).
fn split_options(args: &[std::ffi::OsString]) -> (Vec<&std::ffi::OsStr>, Vec<&std::ffi::OsStr>) {
    let mut opts = Vec::new();
    let mut rest = Vec::new();
    let mut only_operands = false;
    for arg in args {
        let bytes = arg.as_bytes();
        if only_operands {
            rest.push(arg.as_os_str());
        } else if bytes == b"--" {
            only_operands = true;
        } else if bytes.len() > 1 && bytes.first() == Some(&b'-') {
            opts.push(arg.as_os_str());
        } else {
            only_operands = true;
            rest.push(arg.as_os_str());
        }
    }
    (opts, rest)
}

/// The short flags in a clustered option word (`-rf` is `r` and `f`).
fn flags(opt: &std::ffi::OsStr) -> Vec<u8> {
    opt.as_bytes().iter().skip(1).copied().collect()
}

/// `cat`: the operands' bytes in order, or stdin when there are none. 226 uses
/// in the corpus, all but one of them bare -- it is how a case makes a here-doc
/// or a pipeline VISIBLE, so it carries no interpretation of its own.
///
/// It STREAMS, and that is not an optimisation. `cat /dev/urandom | sleep 0.1`
/// and `cat </dev/zero` are both in the corpus, and an applet that read its
/// input to a buffer before writing any of it would never reach the write that
/// fails -- it would spin on an endless device forever, which is a HANG where
/// the shell under test is blameless. A draft of this did exactly that and put
/// three cases on the skip list; they are the reason the writer is a parameter.
fn cat<W: Write>(args: &[std::ffi::OsString], out: &mut W) -> Done {
    let (opts, files) = split_options(args);
    if let Some(bad) = opts.first() {
        return unsupported("cat", bad);
    }
    let mut err = Vec::new();
    let mut status = 0;
    // No operands is stdin, and `-` names it too -- which is what a pipeline
    // stage in the corpus relies on.
    let sources: Vec<&std::ffi::OsStr> = match files.is_empty() {
        true => vec![std::ffi::OsStr::new("-")],
        false => files,
    };
    for file in sources {
        let copied = match file.as_bytes() {
            b"-" => std::io::copy(&mut std::io::stdin().lock(), out),
            _ => match std::fs::File::open(file) {
                Ok(mut f) => std::io::copy(&mut f, out),
                Err(e) => Err(e),
            },
        };
        match copied {
            Ok(_) => {}
            // The reader is gone. Real `cat` DIES of SIGPIPE here and the shell
            // reports 128+13; this exits 141 outright, which is the same `$?`
            // without needing a disposition this crate cannot set safely. Stop
            // at once: the remaining operands have nowhere to go.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                return Done { out: Vec::new(), err, status: 141 };
            }
            Err(e) => {
                err.extend_from_slice(
                    format!("cat: {}: {e}\n", std::path::Path::new(file).display()).as_bytes(),
                );
                status = 1;
            }
        }
    }
    Done { out: Vec::new(), err, status }
}

/// `mkdir [-p]`. 94 of its 106 uses pass `-p`, which is the whole reason a case
/// reaches for it: somewhere to put a file, whether or not the run before left
/// one behind.
fn mkdir(args: &[std::ffi::OsString]) -> Done {
    let (opts, dirs) = split_options(args);
    let mut parents = false;
    for opt in &opts {
        for flag in flags(opt) {
            match flag {
                b'p' => parents = true,
                _ => return unsupported("mkdir", opt),
            }
        }
    }
    let mut err = Vec::new();
    let mut status = 0;
    if dirs.is_empty() {
        return Done {
            out: Vec::new(),
            err: b"mkdir: missing operand\n".to_vec(),
            status: 1,
        };
    }
    for dir in dirs {
        // `-p` is also what makes an EXISTING directory not an error, which is
        // the half a case depends on when it runs twice.
        let made = match parents {
            true => std::fs::create_dir_all(dir),
            false => std::fs::create_dir(dir),
        };
        if let Err(e) = made {
            err.extend_from_slice(
                format!("mkdir: {}: {e}\n", std::path::Path::new(dir).display()).as_bytes(),
            );
            status = 1;
        }
    }
    Done { out: Vec::new(), err, status }
}

/// `touch`: create the operands if absent. All 124 bare uses want existence and
/// nothing else, so the TIME is deliberately not served -- `-d` and `-r` name a
/// specific one, and answering them with "now" would be a wrong answer rather
/// than a missing feature.
///
/// A BARE `touch` does name a time, though -- now -- and that half is served,
/// because `[ a -nt b ]` is a question the corpus asks and a `touch` that never
/// touched would answer it wrongly rather than not at all. It is set through an
/// opened descriptor rather than by path, so it lands on the file the existence
/// check found.
///
/// Which leaves EXISTENCE as the rest of it, and an operand already there is
/// opened READ-ONLY rather than for writing. Opening to write was the first
/// draft and it is wrong in two directions real `touch` gets right: a DIRECTORY
/// cannot be opened for writing at all, and neither can a read-only file, even
/// by its owner -- both of which `touch` updates, so both would have reported a
/// failure that is this applet's and not the shell's.
fn touch(args: &[std::ffi::OsString]) -> Done {
    let (opts, files) = split_options(args);
    if let Some(bad) = opts.first() {
        return unsupported("touch", bad);
    }
    let mut err = Vec::new();
    let mut status = 0;
    if files.is_empty() {
        return Done {
            out: Vec::new(),
            err: b"touch: missing file operand\n".to_vec(),
            status: 1,
        };
    }
    for file in files {
        // `metadata`, which FOLLOWS: a dangling symlink is a name real `touch`
        // creates the target of, so it is absent for this purpose.
        let opened = match std::path::Path::new(file).metadata().is_ok() {
            true => std::fs::File::open(file),
            // Append rather than create-truncate: `touch` must never empty a
            // file, and this is the arm that would if the check above and the
            // filesystem ever disagreed.
            false => std::fs::OpenOptions::new().append(true).create(true).open(file),
        };
        let touched = opened.and_then(|f| {
            f.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        });
        if let Err(e) = touched {
            err.extend_from_slice(
                format!("touch: {}: {e}\n", std::path::Path::new(file).display()).as_bytes(),
            );
            status = 1;
        }
    }
    Done { out: Vec::new(), err, status }
}

/// The directory a removal may not leave. `run_case` stages the case's `cwd`,
/// `tmp` and `bin` as siblings of one throwaway root and exports `$TMP`, so the
/// parent of `$TMP` is the whole of the case's world. Outside the harness there
/// is no such world and the cwd is the most that can be assumed; when neither
/// resolves there is no answer at all, and `rm` then removes NOTHING, which is
/// the safe direction.
///
/// `tmp` is passed rather than read, so the root this all turns on is pinned by
/// a test instead of by whatever `$TMP` the suite's own process happens to hold.
fn workspace_root(tmp: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    // A bare relative `$TMP` has `""` for a parent, which `canonicalize` rejects
    // on its own -- so an unusable `$TMP` falls through to the cwd rather than
    // needing a length check of its own.
    let from_tmp = tmp
        .map(std::path::Path::new)
        .and_then(std::path::Path::parent)
        .and_then(|root| std::fs::canonicalize(root).ok());
    let cwd = || std::env::current_dir().ok().and_then(|c| std::fs::canonicalize(c).ok());
    let root = from_tmp.or_else(cwd)?;
    // `/` is NOT a workspace. It would admit every path on the host while every
    // refusal below still appeared to fire -- a confinement that silently
    // protects nothing, which is worse than none at all. `$TMP=/tmp` is the way
    // in, and it is what anyone running this helper by hand would have set.
    match root.parent().is_some() {
        true => Some(root),
        false => None,
    }
}

fn workspace() -> Option<std::path::PathBuf> {
    workspace_root(std::env::var_os("TMP").as_deref())
}

/// Whether `target` names an entry inside `root`.
///
/// The PARENT is resolved and the final name left alone, rather than resolving
/// the target: `rm` removes a symlink AS a link, so where the entry lives is
/// the question and not where it points. Resolving is also the only way `..`
/// is answerable -- `../../x` is outside a root that `starts_with` on the
/// unresolved path would call inside.
///
/// Three outcomes, two of which proceed. A path with no final component (`/`,
/// `.`, `..`, anything ending in `..`) is refused, since it names a directory
/// this cannot ask about -- `rm -rf /` is exactly that shape. A parent that
/// does not resolve is allowed through: nothing can be there to remove, so the
/// ordinary path reports it missing and `-f` silences it as it should.
fn inside(root: &std::path::Path, target: &std::path::Path) -> bool {
    if target.file_name().is_none() {
        return false;
    }
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        // A bare name lives in the cwd, which is confined on its own account.
        _ => std::path::PathBuf::from("."),
    };
    match std::fs::canonicalize(&parent) {
        Ok(dir) => dir.starts_with(root),
        Err(_) => true,
    }
}

/// `rm [-f] [-r]`. `-f` is 33 of its 38 uses: a case clearing up after itself
/// wants no complaint about a file the run before it never made.
///
/// Every operand is confined to the case workdir first, and that is not about a
/// hostile corpus. `rm -rf $TMP/x` is a shape the corpus already has, and a
/// shell that mis-expands `$TMP` turns it into `rm -rf /x` -- so the exposure
/// belongs to the very thing this harness exists to run, a shell that gets
/// expansion wrong. A refusal is LOUD and 2, and `-f` does not silence it: `-f`
/// is about a file that was never there, not about one this may not touch.
///
/// `root` is passed rather than read, so the applet stays a pure function of its
/// arguments and its confinement is testable without a process-wide `$TMP` the
/// suite's other threads share.
fn rm(args: &[std::ffi::OsString], root: Option<&std::path::Path>) -> Done {
    let (opts, files) = split_options(args);
    let (mut force, mut recursive) = (false, false);
    for opt in &opts {
        for flag in flags(opt) {
            match flag {
                b'f' => force = true,
                b'r' | b'R' => recursive = true,
                _ => return unsupported("rm", opt),
            }
        }
    }
    let mut err = Vec::new();
    let mut status = 0;
    if files.is_empty() && !force {
        return Done {
            out: Vec::new(),
            err: b"rm: missing operand\n".to_vec(),
            status: 1,
        };
    }
    for file in files {
        let path = std::path::Path::new(file);
        if !root.is_some_and(|r| inside(r, path)) {
            err.extend_from_slice(
                format!(
                    "spec_helpers: rm: refusing to remove outside the case workdir: {}\n",
                    path.display()
                )
                .as_bytes(),
            );
            status = status.max(2);
            continue;
        }
        // `symlink_metadata`, not `metadata`: a symlink TO a directory is
        // removed as a link, and following it would recurse into the target.
        let removed = match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() && recursive => std::fs::remove_dir_all(path),
            Ok(meta) if meta.is_dir() => Err(std::io::Error::other("is a directory")),
            Ok(_) => std::fs::remove_file(path),
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => {}
            // `-f` silences a MISSING file and nothing else, so a permission
            // error still reports rather than passing quietly.
            Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                err.extend_from_slice(format!("rm: {}: {e}\n", path.display()).as_bytes());
                status = status.max(1);
            }
        }
    }
    Done { out: Vec::new(), err, status }
}

/// The three counts `wc` reports, tallied as the bytes arrive rather than over a
/// buffer: the input may be a device, and counting is the one thing here that
/// must read all of it.
#[derive(Default)]
struct Counts {
    lines: u64,
    words: u64,
    bytes: u64,
    in_word: bool,
}

impl Counts {
    fn take(&mut self, chunk: &[u8]) {
        self.bytes = self.bytes.saturating_add(chunk.len() as u64);
        for b in chunk {
            if *b == b'\n' {
                self.lines = self.lines.saturating_add(1);
            }
            // A word ENDS at whitespace, so it is counted where it begins --
            // which is what makes a word split across two chunks one word.
            // Rust's `is_ascii_whitespace` is NOT C's `isspace`: it omits the
            // vertical tab, which GNU wc splits on, so that one is spelled out.
            match b.is_ascii_whitespace() || *b == 0x0b {
                true => self.in_word = false,
                false => {
                    if !self.in_word {
                        self.words = self.words.saturating_add(1);
                    }
                    self.in_word = true;
                }
            }
        }
    }

    fn tally<R: std::io::Read>(mut src: R) -> std::io::Result<Self> {
        let mut counts = Counts::default();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match src.read(&mut chunk) {
                Ok(0) => return Ok(counts),
                Ok(n) => counts.take(chunk.get(..n).unwrap_or_default()),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// `wc [-l] [-w] [-c|--bytes]`. 35 of its uses pass `-l`, 13 ask for bytes (as
/// `--bytes` or `-c`), one `-w`, and the rest are bare -- which is all three
/// counts in the order lines, words, bytes.
///
/// The FORMAT is what a golden actually compares, and this DIVERGES from GNU in
/// a way worth stating exactly. GNU pads to a column whenever more than one
/// count is selected: to the width of the input's size for a regular file, and
/// to 7 otherwise -- so `printf 'a bb\nccc\n' | wc` gives `      2       3
///       9` where a redirect of the same bytes gives `2 3 9`. One count is
/// never padded. This always emits single spaces, which agrees with GNU for
/// every one-field use and for a small regular file, and differs for a bare
/// `wc` on a pipe. No corpus case grades that shape: the only two bare uses are
/// an alias invoked as `WC -l` (one field) and a `wc FILE` in a case that reads
/// the repo tree and is statically skipped. Padding it properly needs the size
/// of stdin, so it waits for a case that wants it.
///
/// One named file appends the name; several would need GNU's column-and-`total`
/// layout and are refused rather than guessed at.
/// The single corpus case passing two files (`builtin-process`, `ulimit --all`)
/// opens with `case $SH in bash|dash|mksh|zsh) exit ;; esac`, so under the
/// ash/dash chain it exits before reaching the `wc` -- nothing grades that
/// format, and guessing GNU's column rule would be inventing an answer.
fn wc(args: &[std::ffi::OsString]) -> Done {
    let (opts, files) = split_options(args);
    let (mut lines, mut words, mut bytes) = (false, false, false);
    for opt in &opts {
        // A LONG option is one word, not a cluster of letters: `--bytes` is not
        // `-b -y -t -e -s`.
        match opt.as_bytes() {
            b"--lines" => lines = true,
            b"--words" => words = true,
            b"--bytes" | b"--chars" => bytes = true,
            other if other.starts_with(b"--") => return unsupported("wc", opt),
            _ => {
                for flag in flags(opt) {
                    match flag {
                        b'l' => lines = true,
                        b'w' => words = true,
                        b'c' | b'm' => bytes = true,
                        _ => return unsupported("wc", opt),
                    }
                }
            }
        }
    }
    // No selection is all three, in this order.
    if !(lines || words || bytes) {
        (lines, words, bytes) = (true, true, true);
    }
    if files.len() > 1 {
        return unsupported("wc", files.get(1).copied().unwrap_or_default());
    }
    // No operand is stdin, and `-` names it too -- the same rule `cat` follows,
    // and POSIX's.
    let counted = match files.first() {
        Some(name) if name.as_bytes() != b"-" => std::fs::File::open(name).and_then(Counts::tally),
        _ => Counts::tally(std::io::stdin().lock()),
    };
    let counts = match counted {
        Ok(c) => c,
        Err(e) => {
            let what = files.first().map(|f| std::path::Path::new(f).display().to_string());
            let what = what.unwrap_or_else(|| "-".to_string());
            return Done {
                out: Vec::new(),
                err: format!("wc: {what}: {e}\n").into_bytes(),
                status: 1,
            };
        }
    };
    let mut fields: Vec<String> = Vec::new();
    for (wanted, n) in [(lines, counts.lines), (words, counts.words), (bytes, counts.bytes)] {
        if wanted {
            fields.push(n.to_string());
        }
    }
    let mut line = fields.join(" ");
    if let Some(name) = files.first() {
        line.push(' ');
        line.push_str(&String::from_utf8_lossy(name.as_bytes()));
    }
    line.push('\n');
    Done::ok(line.into_bytes())
}

/// `sleep`. Every one of its 69 uses is a bare duration and 66 are fractional
/// (`0.1`, `0.01`), which is why this parses seconds as a float rather than the
/// integer POSIX requires -- the corpus is written for GNU's.
fn sleep(args: &[std::ffi::OsString]) -> Done {
    let (opts, operands) = split_options(args);
    if let Some(bad) = opts.first() {
        return unsupported("sleep", bad);
    }
    let mut total = std::time::Duration::ZERO;
    if operands.is_empty() {
        return Done {
            out: Vec::new(),
            err: b"sleep: missing operand\n".to_vec(),
            status: 1,
        };
    }
    for operand in &operands {
        let text = String::from_utf8_lossy(operand.as_bytes()).into_owned();
        // A suffix (`1s`, `2m`) is GNU's and nothing in the corpus uses one, so
        // it is refused rather than silently read as its leading number.
        // `try_from_secs_f64` is the ONLY range check: it already rejects a
        // negative, a NaN and an infinity, so a guard in front of it would be
        // an arm nothing reaches. The test asserts all three through here.
        let refused = text
            .parse::<f64>()
            .ok()
            .and_then(|secs| std::time::Duration::try_from_secs_f64(secs).ok());
        match refused {
            Some(d) => total = total.saturating_add(d),
            None => return unsupported("sleep", operand),
        }
    }
    std::thread::sleep(total);
    Done::ok(Vec::new())
}

/// `seq LAST`, `seq FIRST LAST`, `seq FIRST INCREMENT LAST` — one integer per
/// line. The corpus uses only the first two forms; the third is the same loop
/// and refusing it would be arbitrary.
fn seq(args: &[std::ffi::OsString]) -> Done {
    let (opts, operands) = split_options(args);
    if let Some(bad) = opts.first() {
        return unsupported("seq", bad);
    }
    let mut nums = Vec::new();
    for operand in &operands {
        match String::from_utf8_lossy(operand.as_bytes()).parse::<i64>() {
            Ok(n) => nums.push(n),
            // Floats are GNU's and nothing here uses one; refusing beats
            // truncating to an integer nobody asked for.
            Err(_) => return unsupported("seq", operand),
        }
    }
    let (first, incr, last) = match nums.as_slice() {
        [last] => (1, 1, *last),
        [first, last] => (*first, 1, *last),
        [first, incr, last] => (*first, *incr, *last),
        // Named apart, because a diagnostic is read by whoever is debugging a
        // case and "missing" for four operands sends them the wrong way.
        [] => {
            return Done {
                out: Vec::new(),
                err: b"seq: missing operand\n".to_vec(),
                status: 1,
            }
        }
        _ => {
            return Done {
                out: Vec::new(),
                err: b"seq: extra operand\n".to_vec(),
                status: 1,
            }
        }
    };
    if incr == 0 {
        return Done {
            out: Vec::new(),
            err: b"seq: increment must not be zero\n".to_vec(),
            status: 1,
        };
    }
    let mut out = Vec::new();
    let mut n = first;
    // An empty range prints NOTHING and succeeds, which `seq 0` relies on.
    while (incr > 0 && n <= last) || (incr < 0 && n >= last) {
        out.extend_from_slice(format!("{n}\n").as_bytes());
        match n.checked_add(incr) {
            Some(next) => n = next,
            None => break,
        }
    }
    Done::ok(out)
}

/// A `chmod` mode: octal, or one symbolic clause.
///
/// Parsing is SEPARATE from applying because whether a mode parses does not
/// depend on the file: one parse answers for every operand, and the apply that
/// follows cannot fail. Checking inside the per-file loop instead leaves an arm
/// no input reaches, which is an arm no test can pin.
enum ModeChange {
    Octal(u32),
    /// `op` is one of `+-=`, which is how `parse` finds the clause at all.
    Symbolic { op: u8, mask: u32, perms: u32 },
}

impl ModeChange {
    /// Only the shapes the corpus uses are served -- 34 `+x`, one `-w`, one
    /// `-r` -- plus octal, which is the same arithmetic. A `who` prefix is
    /// honoured where given; without one the change applies to all three, which
    /// is what `chmod +x` means to every case here. GNU excludes the umask for a
    /// bare `+`, and this deliberately does not: a test that chmods a file wants
    /// it executable, not executable-unless-the-umask-said-otherwise, and the
    /// corpus never sets one first.
    fn parse(mode: &str) -> Option<Self> {
        // Digits FIRST: `from_str_radix` accepts a leading sign, so it would
        // read `+7` as a number and never reach the symbolic parse below.
        if !mode.is_empty() && mode.bytes().all(|b| b.is_ascii_digit()) {
            return u32::from_str_radix(mode, 8)
                .ok()
                .map(|octal| Self::Octal(octal & 0o7777));
        }
        let at = mode.find(['+', '-', '='])?;
        let (who, rest) = (mode.get(..at)?, mode.get(at..)?);
        // Each `who` carries its SPECIAL bit as well as its rwx triple, which
        // is what makes `u+s` setuid and `g+s` setgid from one perm letter.
        let mut mask = 0u32;
        for w in who.bytes() {
            mask |= match w {
                b'u' => 0o4700,
                b'g' => 0o2070,
                b'o' => 0o1007,
                b'a' => 0o7777,
                _ => return None,
            };
        }
        if mask == 0 {
            mask = 0o7777;
        }
        let op = rest.bytes().next()?;
        let mut perms = 0u32;
        for p in rest.get(1..)?.bytes() {
            perms |= match p {
                b'r' => 0o444,
                b'w' => 0o222,
                b'x' => 0o111,
                // Both set-id bits; the `who` mask above is what picks one.
                b's' => 0o6000,
                b't' => 0o1000,
                _ => return None,
            };
        }
        Some(Self::Symbolic { op, mask, perms })
    }

    /// The new permission bits for a file currently at `current`.
    fn apply(&self, current: u32) -> u32 {
        let (op, mask, perms) = match *self {
            Self::Octal(bits) => return bits,
            Self::Symbolic { op, mask, perms } => (op, mask, perms),
        };
        let bits = perms & mask;
        match op {
            b'+' => current | bits,
            b'-' => current & !bits,
            // `parse` finds the clause BY this byte, so `=` is the only other.
            _ => (current & !mask) | bits,
        }
    }
}

/// `chmod MODE FILE...`.
///
/// The mode is argv[1] POSITIONALLY and is NOT put through `split_options`: two
/// of its corpus uses are `chmod -w` and `chmod -r`, which that would read as
/// option words and refuse. Every other applet here can split first; this one
/// cannot, and that is the whole reason it parses its own arguments.
///
/// Operands are confined exactly as `rm`'s are, and for a sharper reason than
/// tidiness: the staged applets are SIBLINGS of the case workdir, so `chmod 000
/// ../bin/cat` would leave every later case in the run unable to execute any of
/// them. Same shape as the `rm -rf $TMP/x` argument -- the shell under test is
/// the thing that gets a path wrong -- but a mode change needs no `-r` to do it.
fn chmod(args: &[std::ffi::OsString], root: Option<&std::path::Path>) -> Done {
    // A leading `--` is still honoured, since POSIX spells a mode that looks
    // like an option that way: `chmod -- -w f`. It is the ONE option word this
    // reads, and it is stepped over rather than parsed.
    let args = match args.first().map(|a| a.as_bytes() == b"--") {
        Some(true) => args.get(1..).unwrap_or_default(),
        _ => args,
    };
    let Some(mode) = args.first() else {
        return Done {
            out: Vec::new(),
            err: b"chmod: missing operand\n".to_vec(),
            status: 1,
        };
    };
    let mode_text = String::from_utf8_lossy(mode.as_bytes()).into_owned();
    let files = args.get(1..).unwrap_or_default();
    if files.is_empty() {
        return Done {
            out: Vec::new(),
            err: b"chmod: missing file operand\n".to_vec(),
            status: 1,
        };
    }
    // The MODE is parsed once, before any file is touched, and a bad one is
    // REFUSED rather than reported as a filesystem error. It is not one: a
    // status 1 here reads as the operation having failed, which grades as the
    // shell doing something wrong instead of this rig lacking a mode.
    let Some(change) = ModeChange::parse(&mode_text) else {
        return unsupported("chmod", mode);
    };
    let mut err = Vec::new();
    let mut status = 0;
    for file in files {
        let path = std::path::Path::new(file);
        if !root.is_some_and(|r| inside(r, path)) {
            err.extend_from_slice(
                format!(
                    "spec_helpers: chmod: refusing to change mode outside the case workdir: {}\n",
                    path.display()
                )
                .as_bytes(),
            );
            status = status.max(2);
            continue;
        }
        // Read, change, write: a symbolic clause is relative to what is there.
        // `metadata` FOLLOWS a symlink, as GNU chmod does without `-h`, and the
        // mask keeps `st_mode`'s file-type bits out of the permissions word.
        let changed = std::fs::metadata(path).and_then(|meta| {
            use std::os::unix::fs::PermissionsExt;
            let bits = change.apply(meta.permissions().mode() & 0o7777);
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits))
        });
        if let Err(e) = changed {
            err.extend_from_slice(format!("chmod: {}: {e}\n", path.display()).as_bytes());
            status = status.max(1);
        }
    }
    Done { out: Vec::new(), err, status }
}

/// `grep [-q] [-o] [-i] [-v] [-E|-F] [-A NUM] [-m NUM] PATTERN [FILE]`.
///
/// The option surface is counted like every other applet's, over 177
/// grep-family invocations: `-q` 30, `-o` 25, `-E` 12, `-oF` 12, `-i` 5, `-A1`
/// 2, `-v` 1, and one each of `--only-matching` and `--max-count`. Anything
/// else is refused.
///
/// `egrep` and `fgrep` are the same applet under another name, which is what
/// they are in coreutils too -- they only preset `-E` and `-F`.
///
/// Input is read to EOF before any of it is matched, unlike `wc` and `cat`,
/// which stream because their input may be a device. `-A` is why: a context
/// window needs the lines around a match, not just the match. The cost is that
/// `-q` does not exit at the first hit the way a real grep does, so an ENDLESS
/// producer would hang here where GNU's would close the pipe and stop it. No
/// corpus case pipes one in -- and the case timeout bounds it if one ever does.
fn grep(args: &[std::ffi::OsString], preset: Option<u8>) -> Done {
    let (mut quiet, mut only, mut icase, mut invert) = (false, false, false, false);
    let (mut ere, mut fixed) = (preset == Some(b'E'), preset == Some(b'F'));
    // `context` is whether one was ASKED FOR, which is not the same as its
    // being non-zero: GNU separates groups under `-A 0` too.
    let (mut after, mut context, mut max) = (0usize, false, usize::MAX);
    let mut operands: Vec<&std::ffi::OsStr> = Vec::new();
    let mut rest_are_operands = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        let bytes = arg.as_bytes();
        if rest_are_operands || bytes == b"-" || !bytes.starts_with(b"-") {
            operands.push(arg);
            continue;
        }
        if bytes == b"--" {
            rest_are_operands = true;
            continue;
        }
        // A numeric option takes the rest of its word, or the next argument.
        let number = |tail: &[u8], i: &mut usize| -> Option<usize> {
            let text = match tail.is_empty() {
                false => String::from_utf8_lossy(tail).into_owned(),
                true => {
                    let next = args.get(*i)?;
                    *i += 1;
                    String::from_utf8_lossy(next.as_bytes()).into_owned()
                }
            };
            text.parse::<usize>().ok()
        };
        if bytes.starts_with(b"--") {
            match bytes {
                b"--only-matching" => only = true,
                b"--invert-match" => invert = true,
                b"--quiet" | b"--silent" => quiet = true,
                b"--ignore-case" => icase = true,
                b"--extended-regexp" => ere = true,
                b"--fixed-strings" => fixed = true,
                // `--max-count N` or `--max-count=N` and nothing else: a bare
                // prefix match would read `--max-count1` as `-m 1`, which GNU
                // calls an unrecognized option, and an EMPTY attached value
                // would fall through to taking the next argument -- so
                // `--max-count= 1 foo` would silently consume the `1` and grep
                // for `foo` with a limit nobody wrote.
                b"--max-count" => match number(b"", &mut i) {
                    Some(n) => max = n,
                    None => return unsupported("grep", arg),
                },
                _ => match bytes.strip_prefix(b"--max-count=") {
                    Some(tail) if !tail.is_empty() => match number(tail, &mut i) {
                        Some(n) => max = n,
                        None => return unsupported("grep", arg),
                    },
                    _ => return unsupported("grep", arg),
                },
            }
            continue;
        }
        let letters = bytes.get(1..).unwrap_or_default();
        let mut j = 0;
        while let Some(c) = letters.get(j) {
            j += 1;
            match c {
                b'q' => quiet = true,
                b'o' => only = true,
                b'i' => icase = true,
                b'v' => invert = true,
                b'E' => ere = true,
                b'F' => fixed = true,
                b'A' | b'm' => {
                    let tail = letters.get(j..).unwrap_or_default();
                    j = letters.len();
                    match number(tail, &mut i) {
                        Some(n) if *c == b'A' => {
                            after = n;
                            context = true;
                        }
                        Some(n) => max = n,
                        None => return unsupported("grep", arg),
                    }
                }
                _ => return unsupported("grep", arg),
            }
        }
    }
    let Some(pattern) = operands.first() else {
        return Done {
            out: Vec::new(),
            err: b"grep: missing pattern\n".to_vec(),
            status: 2,
        };
    };
    // GNU refuses two matchers rather than letting one win: `grep -E -F 'a.b'`
    // is "conflicting matchers specified", status 2. Silently preferring
    // `fixed` -- which is what testing it first did -- turns a caller's mistake
    // into a different, well-formed search. The presets count, so `fgrep -E`
    // and `egrep -F` are refused too.
    if ere && fixed {
        return Done {
            out: Vec::new(),
            err: b"spec_helpers: grep: conflicting matchers specified\n".to_vec(),
            status: 2,
        };
    }
    let files = operands.get(1..).unwrap_or_default();
    if files.len() > 1 {
        // Several files need GNU's `file:` prefixes, which nothing grades.
        return unsupported("grep", files.get(1).copied().unwrap_or_default());
    }
    let src = match files.first() {
        Some(name) if name.as_bytes() != b"-" => std::fs::read(name),
        _ => {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut buf).map(|_| buf)
        }
    };
    let data = match src {
        Ok(d) => d,
        Err(e) => {
            let what = files.first().map(|f| std::path::Path::new(f).display().to_string());
            return Done {
                out: Vec::new(),
                err: format!("grep: {}: {e}\n", what.unwrap_or_else(|| "-".into())).into_bytes(),
                status: 2,
            };
        }
    };
    let built = match fixed {
        true => Pattern::literal(pattern.as_bytes(), icase),
        false => Pattern::parse(pattern.as_bytes(), ere, icase),
    };
    // The refusal this applet exists to be honest about: an approximated match
    // would be graded as the shell's output.
    let Some(pat) = built else {
        return unsupported("grep", pattern);
    };
    grep_over(&data, &pat, icase, quiet, only, invert, after, context, max)
}

/// The line loop, split out so the option parse above stays one screen.
#[allow(clippy::too_many_arguments)]
fn grep_over(
    data: &[u8],
    pat: &Pattern,
    icase: bool,
    quiet: bool,
    only: bool,
    invert: bool,
    after: usize,
    context: bool,
    max: usize,
) -> Done {
    // A trailing newline ends the last line rather than starting an empty one.
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    let lines: Vec<&[u8]> = match data.is_empty() {
        true => Vec::new(),
        false => body.split(|b| *b == b'\n').collect(),
    };
    let mut out = Vec::new();
    let mut hits = 0usize;
    // Which lines to print, so `-A`'s overlapping windows merge instead of
    // repeating a line, and a gap between them can be marked as GNU marks it.
    let mut wanted = vec![false; lines.len()];
    // One buffer for the whole run rather than one per line: `-i` folds the
    // haystack, and the fold is the only per-line allocation there would be.
    let mut folded: Vec<u8> = Vec::new();
    for (n, line) in lines.iter().enumerate() {
        if hits >= max {
            break;
        }
        let hay = match icase {
            true => {
                folded.clear();
                folded.extend(line.iter().map(u8::to_ascii_lowercase));
                folded.as_slice()
            }
            false => line,
        };
        if pat.find_from(hay, 0).is_some() == invert {
            continue;
        }
        hits += 1;
        if quiet {
            return Done { out: Vec::new(), err: Vec::new(), status: 0 };
        }
        // The window is CLAMPED to the last line: `n..=n + after` with `after`
        // near `usize::MAX` iterates past the end of `wanted` for as long as
        // the machine will let it, which is a hang rather than a wrong answer
        // -- `grep -A18446744073709551615 x` never returned.
        let far = n.saturating_add(after).min(lines.len().saturating_sub(1));
        for slot in n..=far {
            if let Some(w) = wanted.get_mut(slot) {
                *w = true;
            }
        }
    }
    // `-o` goes through the SAME wanted-set rather than printing as it scans.
    // Selection and printing are separate questions: `-A` selects a line's
    // neighbours, and `-o` then prints whatever matches on each selected line,
    // separators and all. Printing inside the scan skipped both, so
    // `grep -o -A1 a` lost the `--` and a context line's own matches.
    let mut last: Option<usize> = None;
    for (n, line) in lines.iter().enumerate() {
        if !wanted.get(n).copied().unwrap_or(false) {
            continue;
        }
        // GNU separates non-adjacent CONTEXT groups with `--`. Without a
        // context option there are no groups to separate, and plain
        // non-adjacent matching lines print with nothing between them.
        if context && last.is_some_and(|p| n > p + 1) {
            out.extend_from_slice(b"--\n");
        }
        last = Some(n);
        if !only {
            out.extend_from_slice(line);
            out.push(b'\n');
            continue;
        }
        // Every non-overlapping match on the line, each on its own line -- and
        // an empty match advances by one, or this would not terminate. The
        // bytes PRINTED come from the original line, not the folded haystack:
        // `-i` decides what matches, never what is shown. ASCII folding is
        // length-preserving, so the offsets carry over.
        let hay = match icase {
            true => {
                folded.clear();
                folded.extend(line.iter().map(u8::to_ascii_lowercase));
                folded.as_slice()
            }
            false => line,
        };
        let mut at = 0;
        while at <= hay.len() {
            let Some((from, to)) = pat.find_from(hay, at) else {
                break;
            };
            if to > from {
                out.extend_from_slice(line.get(from..to).unwrap_or_default());
                out.push(b'\n');
            }
            at = match to > from {
                true => to,
                false => from + 1,
            };
        }
    }
    Done {
        // `-q` returned above on its first hit, and with no hit there is
        // nothing here to suppress -- so this is not guarded a second time.
        out,
        err: Vec::new(),
        // 0 when something matched, 1 when nothing did -- which is what every
        // `grep -q` in the corpus is actually testing.
        status: match hits > 0 {
            true => 0,
            false => 1,
        },
    }
}

/// The longest pattern this will parse. Matching costs one stack frame per
/// term, and this crate builds `panic = "abort"`, so a stack overflow is an
/// abort with no diagnostic rather than an error a caller can report: measured,
/// a 120 KiB pattern overflowed. The corpus's longest is 66 bytes.
const MAX_PATTERN: usize = 4096;

/// One position of a pattern: what to match, and how many times.
#[derive(Clone)]
struct Term {
    atom: Atom,
    rep: Rep,
}

#[derive(Clone)]
enum Atom {
    /// `.` -- any byte. A newline cannot occur, since matching is per line.
    Any,
    Lit(u8),
    /// `[...]`, resolved at parse time into the 256 answers it can be asked.
    /// A set rather than the source ranges so `-i` folds ONCE, here, instead of
    /// at every byte of every line.
    Class(Box<[bool; 256]>),
}

#[derive(Clone, Copy, PartialEq)]
enum Rep {
    One,
    Star,
    Plus,
    Opt,
}

/// The subset of POSIX patterns the corpus actually uses.
///
/// Measured before it was written, over 177 grep-family invocations: 127 have a
/// literal pattern and the other 50 reach for `^`, `$`, `[...]` (with negation
/// and ranges), `.`, `*` and ERE's `+`. That is the whole surface here.
///
/// Everything else -- alternation, groups, backreferences, interval `{n,m}`,
/// named classes like `[:alpha:]` -- is REFUSED at parse time rather than
/// approximated, and the refusal is the applet's status 2. This matters more
/// here than in any other applet: a matcher that silently mishandled a
/// construct would report a WRONG match, and the case would be graded on it as
/// though the shell had produced that output. A refusal leaves the case xfail
/// and says which pattern it could not read.
struct Pattern {
    terms: Vec<Term>,
    /// A leading `^`: the match may only begin at the start of the line.
    start: bool,
    /// A trailing `$`: it may only END at the end of it.
    end: bool,
}

impl Atom {
    fn matches(&self, b: u8) -> bool {
        match self {
            Atom::Any => true,
            Atom::Lit(want) => *want == b,
            Atom::Class(set) => set.get(usize::from(b)).copied().unwrap_or(false),
        }
    }
}

impl Pattern {
    /// `None` for any construct outside the subset above -- see the type's doc.
    ///
    /// `ere` selects which characters are OPERATORS: `+?(){}|` are literals in a
    /// basic expression and operators in an extended one, which is the whole
    /// difference `-E` makes to the shapes the corpus uses.
    fn parse(src: &[u8], ere: bool, icase: bool) -> Option<Self> {
        // One frame per term is what matching costs, so a pattern longer than
        // anything the corpus writes is refused rather than allowed to recurse
        // the stack away -- a `panic = "abort"` crate cannot catch that.
        if src.len() > MAX_PATTERN {
            return None;
        }
        let mut terms: Vec<Term> = Vec::new();
        let (mut start, mut end) = (false, false);
        let mut i = 0;
        // `^` is an anchor where it leads. Elsewhere it is an ordinary
        // character in a BRE -- and in an ERE it is an anchor THERE TOO, which
        // is a construct this does not serve, so under `-E` it is refused
        // rather than read as the literal a BRE would make it.
        if src.first() == Some(&b'^') {
            start = true;
            i = 1;
        }
        while i < src.len() {
            let b = *src.get(i)?;
            // A trailing `$` is the end anchor; one in the middle is a literal
            // to a BRE and an anchor to an ERE, refused for the same reason.
            if b == b'$' && i + 1 == src.len() {
                end = true;
                i += 1;
                continue;
            }
            if ere && matches!(b, b'^' | b'$') {
                return None;
            }
            let atom = match b {
                b'.' => {
                    i += 1;
                    Atom::Any
                }
                b'[' => {
                    let (set, next) = parse_class(src, i, icase)?;
                    i = next;
                    Atom::Class(set)
                }
                b'\\' => {
                    // An escape makes the next byte a literal -- but only where
                    // GNU agrees. `\b`, `\w` and friends are OPERATORS to it
                    // (word boundary, word character), and `\1` is a
                    // backreference, so literalising them is a wrong match in
                    // both directions: `\bfoo\b` matched `bfoob` and missed
                    // `foo bar`. Refused by an ALLOW-list, since the escapes
                    // GNU gives meaning to are open-ended and the ones it does
                    // not are the punctuation this already handles. `\C` stays
                    // a literal `C` -- GNU warns "stray \" and reads it the
                    // same way, and the corpus spells `\C-o\C-s\C-h` eight
                    // times.
                    let next = *src.get(i + 1)?;
                    // Named one by one rather than by a class, because the two
                    // groups do not line up with anything simpler. GNU gives
                    // meaning to the word-boundary and word/space classes, to
                    // `\1`-`\9` as backreferences, and to word- and
                    // buffer-start/end -- and warns "stray \" for EVERY OTHER
                    // letter, reading it as the literal, which is what this
                    // does. `\C` is in the second group, and the corpus spells
                    // `\C-o\C-s\C-h` eight times.
                    if matches!(next, b'b' | b'B' | b'w' | b'W' | b's' | b'S')
                        || matches!(next, b'<' | b'>' | b'`' | b'\'')
                        || next.is_ascii_digit() && next != b'0'
                    {
                        return None;
                    }
                    if !ere && matches!(next, b'+' | b'?' | b'(' | b')' | b'{' | b'}' | b'|') {
                        return None;
                    }
                    i += 2;
                    Atom::Lit(next)
                }
                // Nothing to repeat: a literal asterisk to a BRE, and to an ERE
                // a repeat of the empty expression -- which GNU warns about and
                // matches, and this refuses rather than guesses at.
                b'*' if terms.is_empty() && !ere => {
                    i += 1;
                    Atom::Lit(b'*')
                }
                b'*' if terms.is_empty() => return None,
                b'(' | b')' | b'{' | b'}' | b'|' | b'+' | b'?' if ere => return None,
                other => {
                    i += 1;
                    Atom::Lit(other)
                }
            };
            // A repeat binds to the atom just read.
            let rep = match src.get(i) {
                Some(b'*') => {
                    i += 1;
                    Rep::Star
                }
                Some(b'+') if ere => {
                    i += 1;
                    Rep::Plus
                }
                Some(b'?') if ere => {
                    i += 1;
                    Rep::Opt
                }
                _ => Rep::One,
            };
            // A repeat OF a repeat is refused rather than read as the literal
            // the second operator would otherwise become: `a**+` would parse as
            // `a*` then one-or-more literal asterisks, which is a well-formed
            // pattern for something nobody asked to match. GNU rejects it.
            let doubled = match src.get(i) {
                Some(b'*') => true,
                Some(b'+' | b'?') => ere,
                _ => false,
            };
            if rep != Rep::One && doubled {
                return None;
            }
            terms.push(Term { atom: fold(atom, icase), rep });
        }
        Some(Pattern { terms, start, end })
    }

    /// A LITERAL pattern, for `-F`, where no byte is an operator. Bounded like
    /// `parse`, and for the same reason rather than by analogy: a 120 KiB `-F`
    /// pattern against a line that matches it recursed one frame per byte and
    /// aborted with a stack overflow, where `parse` would have refused it.
    fn literal(src: &[u8], icase: bool) -> Option<Self> {
        if src.len() > MAX_PATTERN {
            return None;
        }
        let terms = src
            .iter()
            .map(|b| Term { atom: fold(Atom::Lit(*b), icase), rep: Rep::One })
            .collect();
        Some(Pattern { terms, start: false, end: false })
    }

    /// The leftmost match in `hay` at or after `from`, as a byte range.
    ///
    /// `hay` is the WHOLE line and `from` an offset into it, rather than the
    /// caller passing a slice: `^` anchors to the start of the LINE, and a
    /// slice would re-anchor it at each step, so `grep -o '^z'` on `zz` would
    /// print two matches where GNU prints one. `hay` is already case-folded
    /// when `-i` is in force, so no comparison below has to know about it.
    fn find_from(&self, hay: &[u8], from: usize) -> Option<(usize, usize)> {
        let last = match self.start {
            true => 0,
            false => hay.len(),
        };
        // A (term, position) pair that has already FAILED fails again, so
        // recording it turns the backtracker from exponential into O(terms x
        // bytes). Without it `a*a*a*…b` over a run of `a`s explores every way
        // of splitting the run: 20 such terms against 30 bytes ran for over ten
        // seconds, entirely inside the subset this serves, so no refusal could
        // have caught it. One table for the whole line, since a failure at
        // (term, pos) does not depend on where the attempt started.
        let width = hay.len() + 1;
        let mut failed = vec![false; width.saturating_mul(self.terms.len() + 1)];
        for at in from..=last {
            if let Some(to) = match_here(&self.terms, 0, hay, at, self.end, &mut failed, width) {
                return Some((at, to));
            }
        }
        None
    }
}

/// Case folding happens ONCE, when the pattern is built: the haystack is folded
/// per line and the needle here, so no comparison has to know about `-i`.
///
/// A CLASS is deliberately not folded here -- `parse_class` does it, before it
/// applies the `^` negation. Folding after the negation instead widens the
/// complement rather than the set it was taken of, so `-i '[^abc]'` came to
/// match `a`.
fn fold(atom: Atom, icase: bool) -> Atom {
    match (icase, atom) {
        (true, Atom::Lit(b)) => Atom::Lit(b.to_ascii_lowercase()),
        (_, other) => other,
    }
}

/// `[...]` starting at `at`. Returns the resolved set and the index past the
/// closing bracket. `]` first is a literal and `-` first or last is a literal,
/// both as POSIX says; a named class is refused rather than guessed at.
/// Does a `:` at `at` open a `[:name:]` that closes before the class does?
/// Only then is the single-bracket typo unambiguous, and all three parts are
/// load-bearing -- GNU refuses `[:x:]` and accepts every near miss as the
/// ordinary set of the bytes it is spelled with. The NAME must be one or more
/// alphanumerics (`[::]` is the class `{:}`, and `[:a1:]` IS diagnosed even
/// though no class is called `a1`), it must be followed by `:` (`[:a-b:]` is a
/// range and a colon), and that colon must close the class (`[:ab:c]` is four
/// ordinary members).
fn ends_named_class(src: &[u8], at: usize) -> bool {
    let mut j = at + 1;
    while src.get(j).is_some_and(|c| c.is_ascii_alphanumeric()) {
        j += 1;
    }
    j > at + 1 && src.get(j) == Some(&b':') && src.get(j + 1) == Some(&b']')
}

fn parse_class(src: &[u8], at: usize, icase: bool) -> Option<(Box<[bool; 256]>, usize)> {
    let mut i = at + 1;
    let mut neg = false;
    if src.get(i) == Some(&b'^') {
        neg = true;
        i += 1;
    }
    let mut set = Box::new([false; 256]);
    let mut first = true;
    loop {
        let b = *src.get(i)?;
        if b == b']' && !first {
            i += 1;
            break;
        }
        // `[[:alpha:]]` and the equivalence/collating forms. Refused, not read
        // as the punctuation they are made of, which is what a plain parse
        // would silently do.
        if b == b'[' && matches!(src.get(i + 1), Some(b':' | b'.' | b'=')) {
            return None;
        }
        // …and the SINGLE-bracket typo `[:alpha:]`, which is the spelling that
        // actually turns up. GNU diagnoses it by name rather than reading it,
        // because as a plain class it silently means `{:,a,l,p,h}` -- a set
        // that matches, so nothing downstream can tell it went wrong.
        if first && b == b':' && ends_named_class(src, i) {
            return None;
        }
        first = false;
        // A range, unless the `-` is last.
        if src.get(i + 1) == Some(&b'-') && src.get(i + 2).is_some_and(|c| *c != b']') {
            let hi = *src.get(i + 2)?;
            if hi < b {
                return None;
            }
            // `[a-b-c]`: the byte after a range may not open another with the
            // `-` it would have to borrow. GNU calls that an invalid range end;
            // reading it as the literal `-` plus `c` is a class that matches
            // more than was asked for.
            if src.get(i + 3) == Some(&b'-') && src.get(i + 4).is_some_and(|c| *c != b']') {
                return None;
            }
            for c in b..=hi {
                if let Some(slot) = set.get_mut(usize::from(c)) {
                    *slot = true;
                }
            }
            i += 3;
            continue;
        }
        if let Some(slot) = set.get_mut(usize::from(b)) {
            *slot = true;
        }
        i += 1;
    }
    if icase {
        for b in b'A'..=b'Z' {
            if set.get(usize::from(b)).copied().unwrap_or(false) {
                if let Some(slot) = set.get_mut(usize::from(b.to_ascii_lowercase())) {
                    *slot = true;
                }
            }
        }
    }
    if neg {
        for slot in set.iter_mut() {
            *slot = !*slot;
        }
    }
    Some((set, i))
}

/// Match `terms` against `hay` from `pos`, returning where the match ENDS.
///
/// Greedy with backtracking: a repeat takes as much as it can and gives bytes
/// back only when what follows cannot match. Without alternation that is the
/// longest match at this position for every shape the subset can express, which
/// is what `-o` prints. `anchored_end` is checked where the terms run out
/// rather than beforehand, so a `$` makes the repeats give back until the match
/// reaches the end of the line -- `[0-9]*$` on `a12` matches the empty string
/// at 3, not the `12` a pre-check would have rejected outright.
fn match_here(
    terms: &[Term],
    ti: usize,
    hay: &[u8],
    pos: usize,
    anchored_end: bool,
    failed: &mut [bool],
    width: usize,
) -> Option<usize> {
    let slot = ti.checked_mul(width).and_then(|base| base.checked_add(pos));
    if slot.and_then(|s| failed.get(s)).copied().unwrap_or(false) {
        return None;
    }
    let give_up = |failed: &mut [bool]| {
        if let Some(cell) = slot.and_then(|s| failed.get_mut(s)) {
            *cell = true;
        }
        None
    };
    let Some(term) = terms.get(ti) else {
        return match !anchored_end || pos == hay.len() {
            true => Some(pos),
            false => give_up(failed),
        };
    };
    let (min, greedy) = match term.rep {
        Rep::One => (1usize, false),
        Rep::Opt => (0, false),
        Rep::Star => (0, true),
        Rep::Plus => (1, true),
    };
    // How many times the atom could match from here, capped at one where the
    // term is not a repeat.
    let mut most = 0usize;
    while let Some(b) = hay.get(pos + most) {
        if !term.atom.matches(*b) {
            break;
        }
        most += 1;
        if !greedy && most == 1 {
            break;
        }
    }
    let mut take = most;
    loop {
        if take < min {
            return give_up(failed);
        }
        if let Some(end) =
            match_here(terms, ti + 1, hay, pos + take, anchored_end, failed, width)
        {
            return Some(end);
        }
        if take == 0 {
            return give_up(failed);
        }
        take -= 1;
    }
}

/// What an applet produced. The two Oils helpers only ever needed stdout, but a
/// real external is graded on its STATUS as much as its output, and reports its
/// own errors -- so all three travel together and every applet stays a pure
/// function of its arguments, testable without running the binary.
struct Done {
    out: Vec<u8>,
    err: Vec<u8>,
    status: i32,
}

impl Done {
    fn ok(out: Vec<u8>) -> Self {
        Done { out, err: Vec::new(), status: 0 }
    }
}

/// An argument this rig does not serve. LOUD, and status 2 rather than 1: a
/// coreutils failure is 1, so a silent guess or a plausible status here would
/// grade as the shell having done something wrong. The corpus case stays xfail
/// and says why, which is the honest outcome for an option nobody implemented.
fn unsupported(applet: &str, what: &std::ffi::OsStr) -> Done {
    let what = String::from_utf8_lossy(what.as_bytes()).into_owned();
    Done {
        out: Vec::new(),
        err: format!("spec_helpers: {applet}: unsupported argument `{what}`\n").into_bytes(),
        status: 2,
    }
}

/// Write an applet's result out and report the process status.
///
/// A closed stdout is 141 here as it is inside `cat`, and it has to be caught in
/// BOTH places. Rust's stdout is line-buffered, so an output that fits the
/// buffer and ends without a newline reaches the pipe only at this flush --
/// `cat`'s own arm never sees the error, and ignoring it here would report 0 for
/// a `cat` whose reader had gone. An applet that already failed keeps ITS
/// status: the pipe closing is not what went wrong.
fn finish<W: Write>(out: &mut W, done: &Done) -> i32 {
    let written = out.write_all(&done.out).and_then(|()| out.flush());
    // stderr is unbuffered and the harness always reads it; a failure there is
    // the harness having gone, which the status already reports.
    let _ = std::io::stderr().write_all(&done.err);
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe && done.status == 0 => 141,
        _ => done.status,
    }
}

fn main() {
    let mut args = std::env::args_os();
    // The applet is argv[0]'s basename: `run_case` stages one binary under every
    // name, and exec'ing a symlink passes the name the CASE used.
    let applet = args
        .next()
        .map(std::path::PathBuf::from)
        .and_then(|p| p.file_name().map(|n| n.to_os_string()))
        .unwrap_or_default();
    let rest: Vec<std::ffi::OsString> = args.collect();
    let mut stdout = std::io::stdout().lock();
    let done = match applet.as_bytes() {
        b"argv.py" => Done::ok(argv(&rest)),
        b"printenv.py" => Done::ok(printenv(&rest)),
        // Writes as it reads, so it must own the handle rather than hand back
        // bytes -- see its doc comment.
        b"cat" => cat(&rest, &mut stdout),
        b"mkdir" => mkdir(&rest),
        b"touch" => touch(&rest),
        b"rm" => rm(&rest, workspace().as_deref()),
        b"wc" => wc(&rest),
        b"sleep" => sleep(&rest),
        b"seq" => seq(&rest),
        b"chmod" => chmod(&rest, workspace().as_deref()),
        // Three names, one applet: `egrep`/`fgrep` are `grep -E`/`grep -F`.
        b"grep" => grep(&rest, None),
        b"egrep" => grep(&rest, Some(b'E')),
        b"fgrep" => grep(&rest, Some(b'F')),
        other => {
            // Named as something this does not serve: say so rather than print
            // a plausible answer, which would grade as a shell result.
            let name = String::from_utf8_lossy(other);
            let _ = writeln!(std::io::stderr(), "spec_helpers: no applet `{name}`");
            std::process::exit(2);
        }
    };
    std::process::exit(finish(&mut stdout, &done));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(s)
    }

    /// A throwaway directory for the applets that touch the filesystem. Named
    /// per test, so the suite's threads cannot collide.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("spec-helpers-{tag}-{}", std::process::id()));
            // Only a directory this test could have left is cleared, and
            // `create_dir` must then SUCCEED: the name is predictable and in
            // shared `/tmp`, so adopting whatever is already there -- a planted
            // symlink, or a leak from a reused pid -- is exactly what
            // `CaseWorkdir` refuses on purpose three files away. Nor is it
            // DELETED: that would answer a predictable path by destroying
            // somebody else's file, where failing here costs only this test.
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).expect("a fresh scratch directory");
            Dir(path)
        }
        fn at(&self, name: &str) -> std::ffi::OsString {
            self.0.join(name).into_os_string()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A writer whose every write is a closed pipe, which is what `cat` meets
    /// when the stage reading it has gone.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An option word is split into its letters, and `--` ends the options so a
    /// file may be named `-foo.txt` -- which the corpus does.
    #[test]
    fn options_end_at_the_first_operand_or_a_double_dash() {
        let args = [os("-r"), os("-f"), os("a"), os("-b")];
        let (opts, rest) = split_options(&args);
        assert_eq!(opts.len(), 2);
        // `-b` after an operand is an OPERAND, not an option: these applets do
        // not permute as GNU does.
        assert_eq!(rest, vec![std::ffi::OsStr::new("a"), std::ffi::OsStr::new("-b")]);
        let args = [os("--"), os("-foo.txt")];
        let (opts, rest) = split_options(&args);
        assert!(opts.is_empty());
        assert_eq!(rest, vec![std::ffi::OsStr::new("-foo.txt")]);
        // A bare `-` is stdin, an operand, never an option.
        let args = [os("-")];
        let (opts, rest) = split_options(&args);
        assert!(opts.is_empty());
        assert_eq!(rest, vec![std::ffi::OsStr::new("-")]);
        assert_eq!(flags(std::ffi::OsStr::new("-rf")), b"rf");
    }

    #[test]
    fn cat_writes_its_operands_in_order() {
        let dir = Dir::new("cat");
        std::fs::write(dir.0.join("a"), b"one\n").unwrap();
        std::fs::write(dir.0.join("b"), b"two\n").unwrap();
        let mut out = Vec::new();
        let done = cat(&[dir.at("a"), dir.at("b")], &mut out);
        assert_eq!(done.status, 0);
        assert_eq!(out, b"one\ntwo\n");
        // A missing operand is status 1 and a message, and the operands around
        // it are still written -- GNU's rule, and what a case relies on when it
        // cats a file it only sometimes made.
        let mut out = Vec::new();
        let done = cat(&[dir.at("a"), dir.at("nope"), dir.at("b")], &mut out);
        assert_eq!(done.status, 1);
        assert_eq!(out, b"one\ntwo\n");
        assert!(!done.err.is_empty(), "no diagnostic for a missing file");
    }

    /// The reader has gone: 141, the status a shell reports for a `cat` killed
    /// by SIGPIPE, and NO further operand is opened.
    #[test]
    fn cat_stops_at_141_when_the_pipe_closes() {
        let dir = Dir::new("cat-pipe");
        std::fs::write(dir.0.join("a"), b"one\n").unwrap();
        let done = cat(&[dir.at("a"), dir.at("nope")], &mut Broken);
        assert_eq!(done.status, 141);
        // The unopened second operand must NOT have reported: stopping is the
        // point, and a diagnostic here would mean it carried on.
        assert!(done.err.is_empty(), "kept going past the closed pipe: {:?}", done.err);
    }

    /// An option nobody implemented is loud and 2, never a plausible guess.
    #[test]
    fn an_unsupported_option_is_refused_rather_than_guessed() {
        let mut out = Vec::new();
        for done in [
            cat(&[os("-n")], &mut out),
            mkdir(&[os("-v"), os("d")]),
            touch(&[os("-d"), os("x"), os("f")]),
            rm(&[os("-i"), os("f")], None),
        ] {
            assert_eq!(done.status, 2, "a guess got through: {:?}", done.err);
            assert!(String::from_utf8_lossy(&done.err).contains("unsupported"));
        }
    }

    #[test]
    fn mkdir_p_makes_parents_and_tolerates_an_existing_directory() {
        let dir = Dir::new("mkdir");
        assert_eq!(mkdir(&[os("-p"), dir.at("x/y/z")]).status, 0);
        assert!(dir.0.join("x/y/z").is_dir());
        // Twice is fine WITH -p and an error without it, which is the half a
        // case depends on when it runs after itself.
        assert_eq!(mkdir(&[os("-p"), dir.at("x/y/z")]).status, 0);
        assert_eq!(mkdir(&[dir.at("x/y/z")]).status, 1);
        assert_eq!(mkdir(&[]).status, 1);
    }

    /// `touch` creates, and must NOT truncate -- the one way it can be wrong
    /// and still look like it worked.
    #[test]
    fn touch_creates_without_truncating() {
        let dir = Dir::new("touch");
        assert_eq!(touch(&[dir.at("f")]).status, 0);
        assert!(dir.0.join("f").is_file());
        std::fs::write(dir.0.join("f"), b"keep\n").unwrap();
        assert_eq!(touch(&[dir.at("f")]).status, 0);
        assert_eq!(std::fs::read(dir.0.join("f")).unwrap(), b"keep\n");
        assert_eq!(touch(&[]).status, 1);
        // A file it cannot create is an error and a diagnostic, never silence.
        let done = touch(&[dir.at("no-such-dir/f")]);
        assert_eq!(done.status, 1);
        assert!(!done.err.is_empty(), "a failed touch said nothing");
    }

    /// `touch` also TOUCHES, which is the half `[ a -nt b ]` turns on. A version
    /// that only created would answer that question wrongly rather than refuse
    /// it, which is the failure this rig refuses `-d` to avoid.
    #[test]
    fn touch_moves_the_time_forward() {
        let dir = Dir::new("touch-time");
        let (old, new) = (dir.0.join("old"), dir.0.join("new"));
        std::fs::write(&old, b"").unwrap();
        std::fs::write(&new, b"").unwrap();
        // Put `old` decisively in the past, so the comparison cannot turn on
        // filesystem timestamp granularity.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(past)))
            .unwrap();
        let mtime = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        assert!(mtime(&old) < mtime(&new), "the fixture is not ordered");
        assert_eq!(touch(&[old.clone().into_os_string()]).status, 0);
        assert!(mtime(&old) > mtime(&new), "touch did not move the time");
    }

    #[test]
    fn rm_f_is_silent_only_about_a_missing_file() {
        let dir = Dir::new("rm");
        let root = Some(dir.0.as_path());
        std::fs::write(dir.0.join("f"), b"x").unwrap();
        assert_eq!(rm(&[dir.at("f")], root).status, 0);
        assert!(!dir.0.join("f").exists());
        // Missing: an error bare, silence under -f.
        assert_eq!(rm(&[dir.at("gone")], root).status, 1);
        let done = rm(&[os("-f"), dir.at("gone")], root);
        assert_eq!(done.status, 0);
        assert!(done.err.is_empty());
        // A directory needs -r, and -f alone does not stand in for it.
        std::fs::create_dir_all(dir.0.join("d/e")).unwrap();
        assert_eq!(rm(&[os("-f"), dir.at("d")], root).status, 1);
        assert!(dir.0.join("d/e").is_dir());
        assert_eq!(rm(&[os("-rf"), dir.at("d")], root).status, 0);
        assert!(!dir.0.join("d").exists());
    }

    /// The confinement, which exists for a shell that mis-expands `$TMP` rather
    /// than for a hostile case: every shape that leaves the root is refused and
    /// nothing outside it is removed.
    #[test]
    fn rm_refuses_to_leave_the_case_workdir() {
        let dir = Dir::new("rm-escape");
        let root = Some(dir.0.as_path());
        // A witness OUTSIDE the root, reached by each escaping spelling. Its own
        // `Dir` so it is cleaned even when an assertion below fails.
        let out = Dir::new("rm-escape-outside");
        let outside = out.0.clone();
        std::fs::create_dir_all(outside.join("keep")).unwrap();
        std::fs::write(outside.join("keep/f"), b"x").unwrap();
        let name = outside.file_name().unwrap_or_default().to_string_lossy().into_owned();
        std::fs::create_dir_all(dir.0.join("sub")).unwrap();
        for escape in [
            outside.join("keep").into_os_string(),                       // absolute
            dir.0.join(format!("../{name}/keep")).into_os_string(),      // `..` above the root
            dir.0.join(format!("sub/../../{name}/keep")).into_os_string(), // `..` mid-path
            std::ffi::OsString::from("/"),                               // the root of all
            dir.0.join("..").into_os_string(),                           // no final name
        ] {
            let done = rm(&[os("-rf"), escape.clone()], root);
            assert_eq!(done.status, 2, "not refused: {escape:?}");
            // `-f` must NOT swallow a refusal: silence here would be a removal
            // nobody could tell from a file that was never there.
            assert!(
                String::from_utf8_lossy(&done.err).contains("refusing"),
                "silent refusal for {escape:?}"
            );
            assert!(outside.join("keep/f").is_file(), "removed through {escape:?}");
        }
        // A symlink pointing out is followed for the CHECK -- `keep` is reached
        // through it -- while the link itself lives inside and may be removed.
        let link = dir.0.join("out");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(rm(&[os("-rf"), link.join("keep").into_os_string()], root).status, 2);
        assert!(outside.join("keep/f").is_file());
        // WITHOUT `-r`, which is what tells a symlink-to-a-directory from a
        // directory: `rm link` removes the link and needs no recursion, where
        // following it would report "is a directory" and remove nothing.
        assert_eq!(rm(&[link.clone().into_os_string()], root).status, 0);
        assert!(!link.exists(), "the link is still there");
        assert!(outside.is_dir(), "removed the target rather than the link");
        // No root at all removes NOTHING, which is the safe direction.
        std::fs::write(dir.0.join("g"), b"x").unwrap();
        assert_eq!(rm(&[os("-f"), dir.at("g")], None).status, 2);
        assert!(dir.0.join("g").is_file());
        // A parent that does not exist is NOT a refusal: nothing can be there,
        // so `-f` silences it exactly as it does a missing file. Refusing here
        // would turn every `rm -f` after a failed `mkdir` into a hard error.
        let done = rm(&[os("-f"), dir.at("no-such-dir/f")], root);
        assert_eq!(done.status, 0, "an absent parent was refused: {:?}", done.err);
        assert!(done.err.is_empty());
        // `rm -f` with nothing to remove is success and silence, as GNU's is.
        let done = rm(&[os("-f")], root);
        assert_eq!(done.status, 0);
        assert!(done.err.is_empty());
    }

    /// The root the confinement turns on. Getting this wrong is the one way the
    /// refusals above could all still fire and protect nothing.
    #[test]
    fn the_workspace_is_the_parent_of_tmp() {
        let dir = Dir::new("workspace");
        let real = std::fs::canonicalize(&dir.0).unwrap();
        std::fs::create_dir(dir.0.join("tmp")).unwrap();
        // What `run_case` exports: cwd, tmp and bin are SIBLINGS, so the case's
        // world is the parent of `$TMP` and not `$TMP` itself.
        assert_eq!(workspace_root(Some(&dir.at("tmp"))), Some(real));
        // No `$TMP` is the helper run outside the harness: the cwd is the most
        // that can be assumed, never everything.
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert_eq!(workspace_root(None), Some(cwd.clone()));
        // A `$TMP` with no usable parent -- bare and relative, or naming a
        // directory that is not there -- falls back to the cwd rather than to
        // `""`, which resolves to nothing and would refuse every removal.
        assert_eq!(workspace_root(Some(std::ffi::OsStr::new("tmp"))), Some(cwd.clone()));
        assert_eq!(workspace_root(Some(&dir.at("nope/tmp"))), Some(cwd));
        // The one root that must be refused outright: `/` admits every path on
        // the host, so a `$TMP` of `/tmp` -- what anyone running this helper by
        // hand would have -- would leave the refusals firing over nothing.
        assert_eq!(workspace_root(Some(std::ffi::OsStr::new("/tmp"))), None);
    }

    /// The two shapes real `touch` updates and an open-for-write cannot: a
    /// directory and a read-only file. Neither is a failure here.
    #[test]
    fn touch_leaves_what_is_already_there_alone() {
        let dir = Dir::new("touch-exists");
        std::fs::create_dir(dir.0.join("d")).unwrap();
        assert_eq!(touch(&[dir.at("d")]).status, 0, "a directory reported a failure");
        assert!(dir.0.join("d").is_dir());
        let ro = dir.0.join("ro");
        std::fs::write(&ro, b"keep\n").unwrap();
        let mut perms = std::fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();
        assert_eq!(touch(&[ro.clone().into_os_string()]).status, 0, "read-only reported a failure");
        assert_eq!(std::fs::read(&ro).unwrap(), b"keep\n");
        // A DANGLING symlink is a name whose target does not exist, and real
        // `touch` creates it -- so it must not be mistaken for present.
        std::os::unix::fs::symlink(dir.0.join("target"), dir.0.join("link")).unwrap();
        assert_eq!(touch(&[dir.at("link")]).status, 0);
        assert!(dir.0.join("target").is_file(), "the dangling link was taken for a file");
    }

    /// A writer that ACCEPTS every write and fails only on flush, which is what
    /// a line-buffered stdout does when the output has no trailing newline and
    /// fits the buffer: `cat` returns 0 and the pipe error surfaces at the end.
    struct BrokenFlush;

    impl Write for BrokenFlush {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn a_pipe_that_closes_before_the_flush_is_still_141() {
        // The buffered case: nothing reported an error until the flush.
        assert_eq!(finish(&mut BrokenFlush, &Done::ok(b"out".to_vec())), 141);
        assert_eq!(finish(&mut Broken, &Done::ok(b"out".to_vec())), 141);
        // An applet that already failed keeps ITS status: the reader going away
        // is not what went wrong, and 141 would name the wrong cause.
        let failed = Done { out: b"out".to_vec(), err: Vec::new(), status: 1 };
        assert_eq!(finish(&mut BrokenFlush, &failed), 1);
        // An intact writer is the applet's own status, untouched.
        assert_eq!(finish(&mut Vec::new(), &Done::ok(b"out".to_vec())), 0);
    }

    /// The three counts, and the FORMAT, which is what a golden compares: a
    /// stream carries no name, one named file does.
    #[test]
    fn wc_counts_and_formats_as_the_goldens_expect() {
        let dir = Dir::new("wc");
        std::fs::write(dir.0.join("f"), b"a bb\nccc\n").unwrap();
        let out = |d: Done| String::from_utf8_lossy(&d.out).into_owned();
        // Bare is lines, words, bytes IN THAT ORDER.
        assert_eq!(out(wc(&[dir.at("f")])), format!("2 3 9 {}\n", dir.0.join("f").display()));
        assert_eq!(out(wc(&[os("-l"), dir.at("f")])), format!("2 {}\n", dir.0.join("f").display()));
        assert_eq!(out(wc(&[os("-w"), dir.at("f")])), format!("3 {}\n", dir.0.join("f").display()));
        // `-c` and `--bytes` are the same question, and a long option is ONE
        // word rather than a cluster of letters.
        assert_eq!(out(wc(&[os("-c"), dir.at("f")])), format!("9 {}\n", dir.0.join("f").display()));
        assert_eq!(
            out(wc(&[os("--bytes"), dir.at("f")])),
            format!("9 {}\n", dir.0.join("f").display())
        );
        // Selected counts keep the same order regardless of flag order.
        assert_eq!(
            out(wc(&[os("-c"), os("-l"), dir.at("f")])),
            format!("2 9 {}\n", dir.0.join("f").display())
        );
        // `-m`/`--chars` count CHARACTERS to GNU and bytes here, which is the
        // same answer for the ASCII the corpus counts and a divergence for
        // anything else. Pinned so it is a decision rather than an accident.
        assert_eq!(out(wc(&[os("-m"), dir.at("f")])), format!("9 {}\n", dir.0.join("f").display()));
        assert_eq!(
            out(wc(&[os("--chars"), dir.at("f")])),
            format!("9 {}\n", dir.0.join("f").display())
        );
        // Two operands would need GNU's column format, which nothing grades.
        assert_eq!(wc(&[dir.at("f"), dir.at("f")]).status, 2);
        assert_eq!(wc(&[os("--total"), dir.at("f")]).status, 2);
        // A missing file is 1 and a diagnostic, never a count.
        let done = wc(&[dir.at("nope")]);
        assert_eq!(done.status, 1);
        assert!(!done.err.is_empty());
        // `-` is stdin rather than a file of that name, and is exercised in
        // `conformance.rs` instead: reading stdin from a unit test would
        // inherit whatever `cargo test` was given, and block on a terminal.
    }

    /// A word split across two reads is ONE word. The counter carries state
    /// between chunks for this, and a 16 KiB input is where it would show.
    #[test]
    fn a_word_spanning_two_reads_is_counted_once() {
        let mut counts = Counts::default();
        counts.take(b"one tw");
        counts.take(b"o three\n");
        assert_eq!((counts.lines, counts.words, counts.bytes), (1, 3, 14));
        // Leading and trailing whitespace start no word, and a line without a
        // newline is not a line -- both as GNU counts them.
        let mut counts = Counts::default();
        counts.take(b"  a  b  ");
        assert_eq!((counts.lines, counts.words, counts.bytes), (0, 2, 8));
        // C's `isspace` splits on the vertical tab and Rust's
        // `is_ascii_whitespace` does not, so GNU would read `a\x0bb` as two
        // words where the stock predicate reads one.
        let mut counts = Counts::default();
        counts.take(b"a\x0bb\x0cc\td\re\nf g");
        assert_eq!((counts.lines, counts.words, counts.bytes), (1, 7, 13));
    }

    /// `tally` must RETRY an interrupted read and propagate any other error.
    /// A signal arriving mid-count is not a short file, and treating `read`'s
    /// EINTR as end-of-input would give a count that is quietly too small.
    #[test]
    fn a_tally_retries_an_interrupted_read() {
        /// Yields a chunk, an EINTR, another chunk, then whatever `last` says.
        struct Flaky {
            step: usize,
            last: Option<std::io::ErrorKind>,
        }
        impl std::io::Read for Flaky {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.step += 1;
                let bytes: &[u8] = match self.step {
                    1 => b"one tw",
                    2 => return Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                    3 => b"o three\n",
                    _ => match self.last {
                        Some(kind) => return Err(std::io::Error::from(kind)),
                        None => b"",
                    },
                };
                let room = buf.get_mut(..bytes.len()).ok_or(std::io::ErrorKind::Other)?;
                room.copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        let counts = Counts::tally(Flaky { step: 0, last: None }).unwrap();
        assert_eq!((counts.lines, counts.words, counts.bytes), (1, 3, 14));
        // The interrupt is retried; anything else is reported rather than read
        // as an end of file, which would print a count for a failed read.
        let err = Counts::tally(Flaky { step: 0, last: Some(std::io::ErrorKind::PermissionDenied) });
        assert_eq!(err.map(|c| c.bytes).map_err(|e| e.kind()), Err(std::io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn seq_counts_from_one_or_from_where_it_is_told() {
        let out = |d: Done| String::from_utf8_lossy(&d.out).into_owned();
        assert_eq!(out(seq(&[os("3")])), "1\n2\n3\n");
        assert_eq!(out(seq(&[os("2"), os("4")])), "2\n3\n4\n");
        assert_eq!(out(seq(&[os("1"), os("2"), os("5")])), "1\n3\n5\n");
        // An empty range prints nothing and SUCCEEDS, which `seq 0` relies on.
        assert_eq!(out(seq(&[os("0")])), "");
        assert_eq!(seq(&[os("0")]).status, 0);
        assert_eq!(out(seq(&[os("3"), os("1")])), "");
        // Counting down needs the step to say so.
        assert_eq!(out(seq(&[os("3"), os("-1"), os("1")])), "3\n2\n1\n");
        // A zero step is an endless loop, not a sequence.
        assert_eq!(seq(&[os("1"), os("0"), os("5")]).status, 1);
        // Too FEW and too many are both errors, and say which: a diagnostic is
        // read by whoever is debugging a case.
        let done = seq(&[]);
        assert_eq!(done.status, 1);
        assert!(String::from_utf8_lossy(&done.err).contains("missing"));
        let done = seq(&[os("1"), os("2"), os("3"), os("4")]);
        assert_eq!(done.status, 1);
        assert!(String::from_utf8_lossy(&done.err).contains("extra"));
        // A float is GNU's; refusing beats truncating to something nobody asked
        // for.
        assert_eq!(seq(&[os("1.5")]).status, 2);
    }

    /// `sleep` must take a FRACTION -- every corpus use is one -- and an
    /// integer-only parse would sleep zero and look like it had worked.
    #[test]
    fn sleep_takes_a_fraction_and_refuses_a_suffix() {
        let start = std::time::Instant::now();
        assert_eq!(sleep(&[os("0.05")]).status, 0);
        assert!(start.elapsed() >= std::time::Duration::from_millis(45), "did not sleep");
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "slept far too long");
        // A suffix is GNU's, and reading `1s` as its leading number would sleep
        // a second where the case asked for something else entirely.
        assert_eq!(sleep(&[os("1s")]).status, 2);
        assert_eq!(sleep(&[os("nan")]).status, 2);
        assert_eq!(sleep(&[]).status, 1);
        // A negative duration has to arrive PAST `--` to be a duration at all --
        // bare, `-1` is an option word and is refused as one, which is a
        // different arm and would leave this one pinned by nothing.
        assert_eq!(sleep(&[os("-1")]).status, 2, "not refused as an option");
        assert_eq!(sleep(&[os("--"), os("-1")]).status, 2, "a negative duration was taken");
        // The two remaining shapes `try_from_secs_f64` is the sole check for.
        assert_eq!(sleep(&[os("inf")]).status, 2);
        assert_eq!(sleep(&[os("--"), os("-inf")]).status, 2);
        // Operands SUM, as GNU's does. Taking only the first would sleep 0.01s
        // here, which is inside the tolerance of every timing assertion above.
        let start = std::time::Instant::now();
        assert_eq!(sleep(&[os("0.01"), os("0.05")]).status, 0);
        assert!(start.elapsed() >= std::time::Duration::from_millis(55), "operands did not sum");
        // One bad operand refuses the whole call rather than sleeping the rest.
        let start = std::time::Instant::now();
        assert_eq!(sleep(&[os("0.01"), os("1s")]).status, 2);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    /// Does `pat` match `line`, and where? `None` for a pattern outside the
    /// subset, which is the applet's status 2.
    fn m(pat: &str, ere: bool, icase: bool, line: &str) -> Option<Option<(usize, usize)>> {
        let p = Pattern::parse(pat.as_bytes(), ere, icase)?;
        let hay = match icase {
            true => line.to_ascii_lowercase(),
            false => line.to_string(),
        };
        Some(p.find_from(hay.as_bytes(), 0))
    }

    /// The matcher's whole surface, since a wrong match here is graded as the
    /// shell's output rather than as this rig being wrong.
    #[test]
    fn the_pattern_subset_matches_what_posix_says_it_does() {
        let hit = |pat: &str, line: &str| m(pat, false, false, line).flatten();
        // Literals, and a match that is not at the start.
        assert_eq!(hit("foo", "foo"), Some((0, 3)));
        assert_eq!(hit("foo", "a foo b"), Some((2, 5)));
        assert_eq!(hit("foo", "bar"), None);
        // Anchors. `^`/`$` are anchors only at the ends; elsewhere literal.
        assert_eq!(hit("^foo", "foobar"), Some((0, 3)));
        assert_eq!(hit("^foo", "xfoobar"), None);
        assert_eq!(hit("foo$", "barfoo"), Some((3, 6)));
        assert_eq!(hit("foo$", "barfoox"), None);
        assert_eq!(hit("^$", ""), Some((0, 0)));
        assert_eq!(hit("^$", "x"), None);
        // …in a BRE. These three are asserted in the BASIC dialect on purpose,
        // and their ERE counterparts are refusals rather than matches, in
        // `a_pattern_outside_the_subset_is_refused_rather_than_approximated`:
        // asserting only this half is what let the code generalise the BRE rule
        // to a dialect where GNU makes both of them anchors.
        assert_eq!(hit("a^b", "a^b"), Some((0, 3)));
        assert_eq!(hit("a$b", "a$b"), Some((0, 3)));
        // `.` is any byte, `*` is zero-or-more of the atom before it.
        assert_eq!(hit("a.c", "abc"), Some((0, 3)));
        assert_eq!(hit("a.c", "ac"), None);
        assert_eq!(hit("ab*c", "ac"), Some((0, 2)));
        assert_eq!(hit("ab*c", "abbbc"), Some((0, 5)));
        // A leading `*` has nothing to repeat and is a literal asterisk.
        assert_eq!(hit("*x", "*x"), Some((0, 2)));
        // Classes: ranges, negation, `]` first and `-` last as literals.
        assert_eq!(hit("[0-9]", "ab7"), Some((2, 3)));
        assert_eq!(hit("[0-9]", "abc"), None);
        assert_eq!(hit("[^/]", "/x"), Some((1, 2)));
        assert_eq!(hit("[]]", "a]"), Some((1, 2)));
        assert_eq!(hit("[a-]", "-"), Some((0, 1)));
        assert_eq!(hit("[-a]", "-"), Some((0, 1)));
        // An escape makes the next byte a literal.
        assert_eq!(hit("a\\.c", "a.c"), Some((0, 3)));
        assert_eq!(hit("a\\.c", "abc"), None);
        // Greedy, and it gives bytes back when what follows cannot match --
        // which is the only way `a*ab` matches `aaab` at all.
        assert_eq!(hit("a*ab", "aaab"), Some((0, 4)));
        assert_eq!(hit("a*", "aaa"), Some((0, 3)));
        // `$` makes the repeat give back until the match reaches the end,
        // rather than being rejected for having taken too much.
        assert_eq!(hit("[0-9]*$", "a12"), Some((1, 3)));
        assert_eq!(hit("b*$", "ab"), Some((1, 2)));
        // ERE adds `+` and `?`; in a BRE both are ordinary characters.
        let ere = |pat: &str, line: &str| m(pat, true, false, line).flatten();
        assert_eq!(ere("[0-9]+", "a123b"), Some((1, 4)));
        assert_eq!(ere("ab?c", "ac"), Some((0, 2)));
        assert_eq!(ere("ab?c", "abc"), Some((0, 3)));
        // `?` is at most ONE. Without this, `?` behaving as `*` passes both
        // assertions above, since neither offers it a second `b` to take.
        assert_eq!(ere("ab?c", "abbc"), None);
        assert_eq!(ere("a+", "b"), None);
        // Greedy backtracking must not be exponential: a run of repeats over a
        // run of bytes explored every split, and 20 of them over 30 bytes ran
        // for over ten seconds -- entirely inside the served subset, so no
        // refusal could have caught it.
        let pathological = "a*".repeat(20) + "b";
        let hay = "a".repeat(30);
        let started = std::time::Instant::now();
        assert_eq!(ere(&pathological, &hay), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "matching blew up");
        assert_eq!(hit("a+b", "a+b"), Some((0, 3)));
        assert_eq!(hit("a?b", "a?b"), Some((0, 3)));
    }

    /// `-i` folds the pattern where the haystack is folded, and a NEGATED class
    /// is the one place the two can be composed the wrong way round: the fold
    /// belongs before the negation, or the complement is widened instead of the
    /// set it was taken of and `[^abc]` comes to match `a`.
    #[test]
    fn ignoring_case_does_not_widen_a_negated_class() {
        let i = |pat: &str, line: &str| m(pat, false, true, line).flatten();
        assert_eq!(i("foo", "FOO"), Some((0, 3)));
        assert_eq!(i("FOO", "foo"), Some((0, 3)));
        assert_eq!(i("[a-z]", "Q"), Some((0, 1)));
        assert_eq!(i("[A-Z]", "q"), Some((0, 1)));
        // The regression: `a` is excluded whichever case it arrives in.
        assert_eq!(i("[^abc]", "a"), None);
        assert_eq!(i("[^abc]", "A"), None);
        assert_eq!(i("[^abc]", "z"), Some((0, 1)));
        assert_eq!(i("[^ABC]", "a"), None);
        assert_eq!(m("^[^abc]+z+$", true, true, "=a:aazz").flatten(), None);
    }

    /// Everything outside the subset is REFUSED. This is the applet's whole
    /// safety argument: an approximated match would be graded as the shell's
    /// output, where a refusal leaves the case xfail and names the pattern.
    #[test]
    fn a_pattern_outside_the_subset_is_refused_rather_than_approximated() {
        let bad = |pat: &str, ere: bool| {
            assert!(
                Pattern::parse(pat.as_bytes(), ere, false).is_none(),
                "accepted {pat:?} (ere={ere})"
            );
        };
        // ERE's grouping, alternation and intervals.
        for pat in ["a|b", "(ab)", "a{2}", "a{2,3}", "a)", "}"] {
            bad(pat, true);
        }
        // A repeat where an ATOM belongs -- nothing precedes it to repeat.
        // These reach the atom arm rather than the repeat one, which is why
        // `+` and `?` have to be refused in BOTH places under `-E`.
        for pat in ["+a", "?a", "+", "?"] {
            bad(pat, true);
        }
        // A BRE spells those with backslashes, and they are refused there too
        // rather than read as the literals an unescaped parse would make them.
        for pat in ["\\|", "\\(ab\\)", "\\+", "\\?", "\\{2\\}"] {
            bad(pat, false);
        }
        // Named classes, which a plain parse would silently read as the
        // punctuation they are spelled with. BOTH spellings: the SINGLE-bracket
        // typo is the one that turns up, and as a plain class it means
        // `{:,a,l,p,h}` -- a set that matches, so nothing downstream can tell.
        bad("[[:alpha:]]", false);
        bad("[[:digit:]]x", false);
        bad("[[=a=]]", false);
        bad("[[.a.]]", false);
        bad("[:alpha:]", false);
        bad("[:digit:]", true);
        bad("x[:space:]y", false);
        // …but a class that merely CONTAINS a colon is not a named class, and
        // the typo is only a typo where the colon comes FIRST. GNU reads
        // `[a:alpha:]` as the ordinary set `{a,:,l,p,h}` and refuses only
        // `[:alpha:]`, so the position is the whole distinction.
        // Each of these is a NEAR MISS that GNU reads as the ordinary set of
        // the bytes it is spelled with, and every one of the three conditions
        // the scan tests separates one of them from the typo above: an empty
        // name (`[::]`), a name that is not all letters (`[:a-b:]`), a `:` that
        // does not close the class (`[:ab:c]`), and a colon that is not first.
        for pat in ["[:]", "[a:b]", "[:a]", "[a-z:]", "[a:alpha:]", "[x:digit:]", "[ab:]",
                    "[::]", "[:a-b:]", "[:ab:c]", "[:a-]", "[:ab]"] {
            assert!(Pattern::parse(pat.as_bytes(), false, false).is_some(), "refused {pat:?}");
        }
        // …and the name need not be a REAL class name for the diagnosis to
        // fire: GNU refuses `[:x:]` and `[:a1:]` too, so the shape is what is
        // being recognised, not the vocabulary.
        bad("[:x:]", false);
        bad("[:a1:]", false);
        let named = Pattern::parse(b"[a:alpha:]", false, false);
        assert_eq!(named.and_then(|p| p.find_from(b"qla", 0)), Some((1, 2)), "not the plain set");
        let colons = Pattern::parse(b"[::]", false, false);
        assert_eq!(colons.and_then(|p| p.find_from(b"x:y", 0)), Some((1, 2)), "`[::]` is a colon");
        // A range whose end opens another range: GNU calls it an invalid range
        // end, and reading it as a literal `-` widens the class silently.
        bad("[a-b-c]", false);
        // ERE makes `^` and `$` anchors ANYWHERE, where a BRE makes them
        // literals away from the ends. Serving neither, this refuses under
        // `-E` rather than matching the literal a BRE would.
        for pat in ["a$b", "a^b", "^^ab", "$$", "a$b$", "^a^"] {
            bad(pat, true);
        }
        // A leading `*` is a literal to a BRE and a repeat of nothing to an
        // ERE, which GNU warns about and matches.
        bad("*x", true);
        bad("^*x", true);
        // GNU escapes are OPERATORS, not literals: `\b` is a word boundary and
        // `\1` a backreference, so literalising them matched `bfoob` for
        // `\bfoo\b` and missed `foo bar`.
        for pat in ["\\bfoo\\b", "\\B", "\\w", "\\W", "\\s", "\\S", "\\<foo", "foo\\>", "\\1"] {
            bad(pat, false);
            bad(pat, true);
        }
        // …while an escape GNU calls a "stray backslash" stays the literal it
        // reads it as. The corpus spells `\C-o\C-s\C-h` eight times.
        for pat in ["\\C-o\\C-s\\C-h", "\\.", "\\*", "\\[", "\\-", "\\$", "\\^", "\\\\"] {
            assert!(Pattern::parse(pat.as_bytes(), false, false).is_some(), "refused {pat:?}");
        }
        // Malformed: unterminated, reversed range, trailing backslash.
        bad("[abc", false);
        bad("a[", false);
        bad("[z-a]", false);
        bad("\\", false);
        // A repeat OF a repeat, which would otherwise read the second operator
        // as a literal and match something nobody asked for.
        bad("a**", false);
        bad("a**+", true);
        bad("a+*", true);
        bad("a?+", true);
        // …and the shapes just inside the line still parse.
        for (pat, ere) in [("a+b", false), ("a{2}", false), ("[0-9]+", true), ("a*", false)] {
            assert!(Pattern::parse(pat.as_bytes(), ere, false).is_some(), "refused {pat:?}");
        }
    }

    /// `-F` takes no pattern at all: every byte is itself.
    #[test]
    fn a_fixed_pattern_has_no_operators() {
        let lit = |pat: &str, line: &str| {
            Pattern::literal(pat.as_bytes(), false)?.find_from(line.as_bytes(), 0)
        };
        assert_eq!(lit("a.c", "a.c"), Some((0, 3)));
        assert_eq!(lit("a.c", "abc"), None);
        assert_eq!(lit("^foo", "^foo"), Some((0, 4)));
        assert_eq!(lit("^foo", "foo"), None);
        assert_eq!(lit("[0-9]", "x[0-9]"), Some((1, 6)));
        assert_eq!(lit("a*", "aaa"), None);
        // Bounded like `parse`: one stack frame per byte is matched, and this
        // crate aborts on overflow rather than reporting it. A 120 KiB `-F`
        // pattern against a line that matches it DID overflow.
        let huge = "x".repeat(MAX_PATTERN + 1);
        assert!(Pattern::literal(huge.as_bytes(), false).is_none());
        assert!(Pattern::literal("x".repeat(MAX_PATTERN).as_bytes(), false).is_some());
        assert!(Pattern::parse(huge.as_bytes(), false, false).is_none());
    }

    /// The applet around the matcher: status, `-q`, `-v`, `-o`, `-m`, and the
    /// operands. `-o`'s anchor case is a regression -- searching a SLICE of the
    /// line re-anchors `^` at every step, so `grep -o '^z'` printed one match
    /// per leading `z` instead of one per line.
    #[test]
    fn grep_reports_its_status_and_prints_what_was_asked_for() {
        let run = |args: &[&std::ffi::OsStr], data: &str| {
            let owned: Vec<std::ffi::OsString> =
                args.iter().map(|a| (*a).to_os_string()).collect();
            let pat_args = owned;
            let dir = Dir::new("grep");
            let f = dir.0.join("in");
            std::fs::write(&f, data).unwrap();
            let mut argv = pat_args;
            argv.push(f.into_os_string());
            let done = grep(&argv, None);
            (done.status, String::from_utf8_lossy(&done.out).into_owned())
        };
        // Status is the point: 0 when something matched, 1 when nothing did.
        assert_eq!(run(&[os("foo").as_os_str()], "foo\nbar\n"), (0, "foo\n".into()));
        assert_eq!(run(&[os("nope").as_os_str()], "foo\n"), (1, String::new()));
        // `-q` prints nothing and still answers.
        assert_eq!(run(&[os("-q").as_os_str(), os("foo").as_os_str()], "foo\n"), (0, String::new()));
        assert_eq!(run(&[os("-q").as_os_str(), os("no").as_os_str()], "foo\n"), (1, String::new()));
        // Non-adjacent matching lines print with NOTHING between them: the
        // `--` separator belongs to context groups, and emitting it without a
        // context option was a divergence from GNU.
        assert_eq!(
            run(&[os("foo").as_os_str()], "foo\nbar\nfoobar\n"),
            (0, "foo\nfoobar\n".into())
        );
        // …and WITH one it appears, including at `-A 0`, which asks for a
        // context of no extra lines rather than for no context.
        assert_eq!(
            run(&[os("-A").as_os_str(), os("0").as_os_str(), os("foo").as_os_str()],
                "foo\nbar\nfoobar\n"),
            (0, "foo\n--\nfoobar\n".into())
        );
        assert_eq!(
            run(&[os("-A1").as_os_str(), os("foo").as_os_str()], "foo\nbar\nbaz\n"),
            (0, "foo\nbar\n".into())
        );
        // `-v` inverts the selection, not the status.
        assert_eq!(run(&[os("-v").as_os_str(), os("foo").as_os_str()], "foo\nbar\n"), (0, "bar\n".into()));
        assert_eq!(run(&[os("-v").as_os_str(), os("x").as_os_str()], "x\n"), (1, String::new()));
        // `-o` prints each match, and an anchored pattern can match only once.
        assert_eq!(run(&[os("-o").as_os_str(), os("o").as_os_str()], "foo\n"), (0, "o\no\n".into()));
        assert_eq!(run(&[os("-o").as_os_str(), os("^z").as_os_str()], "zz=2\n"), (0, "z\n".into()));
        assert_eq!(
            run(&[os("-o").as_os_str(), os("-i").as_os_str(), os("o").as_os_str()], "FOO\n"),
            (0, "O\nO\n".into()),
        );
        // A pattern that can match NOTHING is why the `-o` loop advances past
        // an empty match rather than to its end: `a*` matches the empty string
        // at every position, so a loop that trusted the match to move forward
        // would never terminate. GNU prints only the non-empty ones.
        assert_eq!(run(&[os("-o").as_os_str(), os("a*").as_os_str()], "b\nab\n"), (0, "a\n".into()));
        assert_eq!(run(&[os("-o").as_os_str(), os("x*").as_os_str()], "b\n"), (0, String::new()));
        // `-o` and a CONTEXT option compose, which they did not while `-o`
        // printed as it scanned: selection and printing are separate questions,
        // so `-A` selects a line's neighbours and `-o` then prints whatever
        // matches on each SELECTED line, separators included. This lost both.
        assert_eq!(
            run(&[os("-o").as_os_str(), os("-A1").as_os_str(), os("a").as_os_str()],
                "a\nb\nc\na\n"),
            (0, "a\n--\na\n".into())
        );
        // A context line that matches is printed even though `-v` did not
        // select it -- `-o` prints the line's matches, not the selection.
        assert_eq!(
            run(&[os("-o").as_os_str(), os("-A1").as_os_str(), os("-v").as_os_str(),
                  os("a").as_os_str()],
                "aX\nbb\ncc\naY\n"),
            (0, "a\n".into())
        );
        // A context window is CLAMPED to the last line. Unclamped, `-A` with a
        // huge count iterated to `usize::MAX` and never returned.
        let huge = format!("-A{}", usize::MAX);
        let started = std::time::Instant::now();
        assert_eq!(run(&[os(&huge).as_os_str(), os("x").as_os_str()], "x\n"), (0, "x\n".into()));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the context window hung");
        // `-m` stops after that many matching lines.
        assert_eq!(
            run(&[os("-m").as_os_str(), os("1").as_os_str(), os("a").as_os_str()], "a\na\na\n"),
            (0, "a\n".into())
        );
        // A file with no trailing newline still has a last line.
        assert_eq!(run(&[os("bar").as_os_str()], "foo\nbar"), (0, "bar\n".into()));
        // Empty input matches nothing rather than matching an empty line.
        assert_eq!(run(&[os("^$").as_os_str()], ""), (1, String::new()));
    }

    /// The refusals and errors the applet answers with, all status 2 so none
    /// can read as a grep that simply found nothing.
    #[test]
    fn grep_refuses_loudly_rather_than_guessing() {
        let dir = Dir::new("grep-refuse");
        let f = dir.0.join("in");
        std::fs::write(&f, "foo\n").unwrap();
        let arg = |s: &str| os(s);
        // An option nobody implemented.
        assert_eq!(grep(&[arg("-c"), arg("foo"), f.clone().into_os_string()], None).status, 2);
        assert_eq!(grep(&[arg("--colour"), arg("foo")], None).status, 2);
        // A pattern outside the subset, which is the important one.
        let done = grep(&[arg("-E"), arg("a|b"), f.clone().into_os_string()], None);
        assert_eq!(done.status, 2, "an unsupported pattern was matched anyway");
        assert!(String::from_utf8_lossy(&done.err).contains("unsupported"));
        // No pattern at all, and a file that is not there.
        assert_eq!(grep(&[], None).status, 2);
        assert_eq!(grep(&[arg("foo"), dir.at("nope")], None).status, 2);
        // Several files would need GNU's `file:` prefixes, which nothing grades.
        let two = [arg("foo"), f.clone().into_os_string(), f.clone().into_os_string()];
        assert_eq!(grep(&two, None).status, 2);
        // `--` ends the options, so a pattern that looks like one still works.
        let done = grep(&[arg("--"), arg("-v"), f.clone().into_os_string()], None);
        assert_eq!(done.status, 1, "`-v` after `--` was read as an option");
        // Two matchers are a REFUSAL, not a race between two booleans. Testing
        // `fixed` first made `-F` win silently whichever order they arrived in,
        // and a preset counts as one of the two.
        for argv in [
            vec![arg("-E"), arg("-F"), arg("a.b")],
            vec![arg("-F"), arg("-E"), arg("a.b")],
            vec![arg("-EF"), arg("a.b")],
        ] {
            let done = grep(&argv, None);
            assert_eq!(done.status, 2, "conflicting matchers were resolved silently");
            assert!(String::from_utf8_lossy(&done.err).contains("conflicting"));
        }
        assert_eq!(grep(&[arg("-F"), arg("a.b")], Some(b'E')).status, 2, "egrep -F");
        assert_eq!(grep(&[arg("-E"), arg("a.b")], Some(b'F')).status, 2, "fgrep -E");
        // The long-option table, which nothing else reaches: each arm sets a
        // DIFFERENT flag, so a table wired to the wrong one would otherwise be
        // invisible. Asked against an input where each answers distinctly.
        std::fs::write(&f, "foo\nbar\nfoo\n").unwrap();
        let long = |a: &str, b: &str| {
            let d = grep(&[arg(a), arg(b), f.clone().into_os_string()], None);
            (d.status, String::from_utf8_lossy(&d.out).into_owned())
        };
        assert_eq!(long("--only-matching", "o"), (0, "o\no\no\no\n".into()));
        assert_eq!(long("--invert-match", "foo"), (0, "bar\n".into()));
        assert_eq!(long("--quiet", "foo"), (0, String::new()));
        assert_eq!(long("--silent", "nope"), (1, String::new()));
        assert_eq!(long("--ignore-case", "FOO"), (0, "foo\nfoo\n".into()));
        assert_eq!(long("--extended-regexp", "o+"), (0, "foo\nfoo\n".into()));
        assert_eq!(long("--fixed-strings", "o+"), (1, String::new()));
        assert_eq!(long("--max-count=1", "foo"), (0, "foo\n".into()));
        assert_eq!(
            grep(&[arg("--max-count"), arg("1"), arg("foo"), f.clone().into_os_string()], None)
                .status,
            0
        );
        // …and the two spellings GNU refuses: a prefix that is not the option,
        // and an EMPTY attached value, which silently took the next argument.
        assert_eq!(grep(&[arg("--max-count1"), arg("foo")], None).status, 2);
        let eaten = grep(&[arg("--max-count="), arg("1"), arg("foo")], None);
        assert_eq!(eaten.status, 2, "`--max-count=` consumed the next argument");
        // The presets are the only difference between the three names: `+` is
        // a repeat to egrep, a literal to grep, and `.` is a full stop to fgrep.
        std::fs::write(&f, "a+b\naxb\n").unwrap();
        let out = |p: Option<u8>, pat: &str| {
            let d = grep(&[arg("-o"), arg(pat), f.clone().into_os_string()], p);
            String::from_utf8_lossy(&d.out).into_owned()
        };
        assert_eq!(out(None, "a+b"), "a+b\n");
        assert_eq!(out(Some(b'E'), "ax+b"), "axb\n");
        assert_eq!(out(Some(b'F'), "a+b"), "a+b\n");
        assert_eq!(out(Some(b'F'), "a.b"), "");
    }

    /// Parse-then-apply in one step, which is how every assertion below reads
    /// the mode arithmetic. The applet itself parses ONCE and applies per file.
    fn chmod_bits(mode: &str, current: u32) -> Option<u32> {
        ModeChange::parse(mode).map(|change| change.apply(current))
    }

    /// The mode arithmetic, which is the whole of `chmod`.
    #[test]
    fn chmod_modes_are_relative_to_what_is_there() {
        // The corpus's three: `+x`, `-w`, `-r`.
        assert_eq!(chmod_bits("+x", 0o644), Some(0o755));
        assert_eq!(chmod_bits("-w", 0o644), Some(0o444));
        assert_eq!(chmod_bits("-r", 0o644), Some(0o200));
        // A `who` applies the change to that class alone.
        assert_eq!(chmod_bits("u+x", 0o644), Some(0o744));
        assert_eq!(chmod_bits("go-r", 0o644), Some(0o600));
        // `=` REPLACES its class rather than adding to it.
        assert_eq!(chmod_bits("u=r", 0o777), Some(0o477));
        // Octal is absolute.
        assert_eq!(chmod_bits("755", 0o600), Some(0o755));
        assert_eq!(chmod_bits("0644", 0o777), Some(0o644));
        // The set-id and sticky bits: one perm letter, and the `who` is what
        // decides WHICH of the two `s` means.
        assert_eq!(chmod_bits("u+s", 0o644), Some(0o4644));
        assert_eq!(chmod_bits("g+s", 0o644), Some(0o2644));
        assert_eq!(chmod_bits("+s", 0o644), Some(0o6644));
        assert_eq!(chmod_bits("+t", 0o644), Some(0o1644));
        assert_eq!(chmod_bits("u-s", 0o4644), Some(0o644));
        // Anything else is refused rather than guessed.
        assert_eq!(chmod_bits("+X", 0o644), None);
        assert_eq!(chmod_bits("q+x", 0o644), None);
        assert_eq!(chmod_bits("rwx", 0o644), None);
        assert_eq!(chmod_bits("", 0o644), None);
        // Octal that is not octal, and one that will not fit.
        assert_eq!(chmod_bits("8", 0o644), None);
        assert_eq!(chmod_bits("99999999999", 0o644), None);
        // Digits-first, pinned directly: `from_str_radix` takes a leading sign,
        // so parsing octal before this check reads `+7` as the number seven.
        assert_eq!(chmod_bits("+7", 0o644), None);
        assert_eq!(chmod_bits("-7", 0o644), None);
    }

    #[test]
    fn chmod_applies_the_mode_and_reads_its_own_argv() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("chmod");
        let f = dir.0.join("f");
        std::fs::write(&f, b"x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let root = Some(dir.0.as_path());
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(chmod(&[os("+x"), f.clone().into_os_string()], root).status, 0);
        assert_eq!(mode(&f), 0o755);
        // `-w` is a MODE, not an option word. Splitting options first would
        // refuse it, which is why this applet parses its own arguments.
        assert_eq!(chmod(&[os("-w"), f.clone().into_os_string()], root).status, 0);
        assert_eq!(mode(&f), 0o555);
        // Several operands, each read from its OWN current mode: one parse
        // applied to two files that start differently must land differently.
        let g = dir.0.join("g");
        std::fs::write(&g, b"x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o640)).unwrap();
        let both = [os("+x"), f.clone().into_os_string(), g.clone().into_os_string()];
        assert_eq!(chmod(&both, root).status, 0);
        assert_eq!((mode(&f), mode(&g)), (0o711, 0o751));
        // A symlink is FOLLOWED, as GNU chmod is without `-h`: the mode lands
        // on the target, and `symlink_metadata` here would read the link's own
        // 0o777 and write that to the file instead.
        let link = dir.0.join("l");
        std::os::unix::fs::symlink(&g, &link).unwrap();
        std::fs::set_permissions(&g, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(chmod(&[os("g+r"), link.into_os_string()], root).status, 0);
        assert_eq!(mode(&g), 0o640, "the mode did not follow to the target");
        // A missing file reports rather than passing quietly.
        let done = chmod(&[os("+x"), dir.at("nope")], root);
        assert_eq!(done.status, 1);
        assert!(!done.err.is_empty());
        // A mode this does not serve is REFUSED (2), not reported as a
        // filesystem failure (1) — which would grade as the shell's fault. It
        // is decided before any file is touched, so a missing file cannot mask
        // it either.
        let done = chmod(&[os("+X"), f.clone().into_os_string()], root);
        assert_eq!(done.status, 2, "an unsupported mode came back as an error");
        assert!(String::from_utf8_lossy(&done.err).contains("unsupported"));
        assert_eq!(chmod(&[os("+X"), dir.at("nope")], root).status, 2);
        assert_eq!(chmod(&[], root).status, 1);
        assert_eq!(chmod(&[os("+x")], root).status, 1);
        // POSIX's way of spelling a mode that looks like an option. Reading
        // `--` AS the mode would fail with "unsupported mode".
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(chmod(&[os("--"), os("-w"), f.clone().into_os_string()], root).status, 0);
        assert_eq!(mode(&f), 0o444);
    }

    /// `chmod`'s confinement. The staged applets are SIBLINGS of the case
    /// workdir, so an unconfined `chmod 000 ../bin/cat` would leave every later
    /// case in the run unable to run any of them -- a failure that would look
    /// like the shell losing its PATH.
    #[test]
    fn chmod_refuses_to_leave_the_case_workdir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("chmod-escape");
        let root = Some(dir.0.as_path());
        let out = Dir::new("chmod-escape-outside");
        let victim = out.0.join("cat");
        std::fs::write(&victim, b"x").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let name = out.0.file_name().unwrap_or_default().to_string_lossy().into_owned();
        std::fs::create_dir_all(dir.0.join("sub")).unwrap();
        for escape in [
            victim.clone().into_os_string(),
            dir.0.join(format!("../{name}/cat")).into_os_string(),
            dir.0.join(format!("sub/../../{name}/cat")).into_os_string(),
            std::ffi::OsString::from("/"),
            dir.0.join("..").into_os_string(),
        ] {
            let done = chmod(&[os("000"), escape.clone()], root);
            assert_eq!(done.status, 2, "not refused: {escape:?}");
            assert!(
                String::from_utf8_lossy(&done.err).contains("refusing"),
                "silent refusal for {escape:?}"
            );
        }
        let left = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o7777;
        assert_eq!(left, 0o755, "a mode was changed outside the workdir");
        // With no root known at all, nothing is changed: the applet fails shut.
        assert_eq!(chmod(&[os("000"), victim.clone().into_os_string()], None).status, 2);
        // And the confinement does not cost the ordinary case.
        let ok = dir.0.join("f");
        std::fs::write(&ok, b"x").unwrap();
        assert_eq!(chmod(&[os("600"), ok.clone().into_os_string()], root).status, 0);
        assert_eq!(std::fs::metadata(&ok).unwrap().permissions().mode() & 0o7777, 0o600);
    }

    /// All three states the goldens distinguish, including the two a shell can
    /// confuse: exported-empty prints a blank LINE where never-exported prints
    /// `None`.
    #[test]
    fn printenv_reports_unset_as_none_and_empty_as_empty() {
        let env = |name: &std::ffi::OsStr| match name.to_str() {
            Some("SET") => Some(os("value")),
            Some("EMPTY") => Some(os("")),
            _ => None,
        };
        assert_eq!(printenv_with(&[os("SET")], env), b"value\n");
        assert_eq!(printenv_with(&[os("EMPTY")], env), b"\n");
        assert_eq!(printenv_with(&[os("MISSING")], env), b"None\n");
        // One line each, in argument order.
        assert_eq!(
            printenv_with(&[os("SET"), os("MISSING"), os("EMPTY")], env),
            b"value\nNone\n\n"
        );
        // No names is no output, not an empty line.
        assert_eq!(printenv_with(&[], env), b"");
    }

    /// An environment value is a byte string, and `var_os` exists to preserve
    /// one that is not UTF-8. Passing it out losslessly is what stops a shell
    /// that carried those bytes correctly being graded as though it mangled
    /// them -- the same text-versus-bytes error `repr` above is written around.
    #[test]
    fn printenv_passes_a_non_utf8_value_through_byte_for_byte() {
        use std::os::unix::ffi::OsStringExt;
        let raw = vec![0xffu8, 0xfe, b'a', 0x80];
        let value = std::ffi::OsString::from_vec(raw.clone());
        let env = |name: &std::ffi::OsStr| {
            (name.to_str() == Some("RAW")).then(|| value.clone())
        };
        let mut want = raw.clone();
        want.push(b'\n');
        assert_eq!(printenv_with(&[os("RAW")], env), want);
    }

    /// `argv.py` with no arguments is `[]`, not a blank list line.
    #[test]
    fn argv_with_no_arguments_is_the_empty_list() {
        assert_eq!(argv(&[]), b"[]\n");
        assert_eq!(argv(&[os("a"), os("b c")]), b"['a', 'b c']\n");
    }

    fn r(arg: &[u8]) -> String {
        let mut out = String::new();
        repr(arg, &mut out);
        out
    }

    /// Each expectation is a string the PINNED CORPUS contains as a golden, or
    /// `repr(os.fsencode(...))` from python3 with its `b` prefix dropped —
    /// never the rules as read.
    #[test]
    fn repr_matches_the_corpus_goldens() {
        assert_eq!(r(b"a"), "'a'");
        assert_eq!(r(b""), "''");
        // A `'` inside switches the quoting, and then needs no escape.
        assert_eq!(r(b"it's"), "\"it's\"");
        // ...unless a `"` is there too, when `'` comes back and IS escaped.
        assert_eq!(r(b"it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(r(b"say \"x\""), "'say \"x\"'");
        assert_eq!(r(b"back\\slash"), "'back\\\\slash'");
        assert_eq!(r(b"a\nb\tc\rd"), "'a\\nb\\tc\\rd'");
        assert_eq!(r(b"\x00\x1b\x7f"), "'\\x00\\x1b\\x7f'");
        // Non-ASCII is BYTES. These four are goldens in the corpus verbatim:
        // var-op-strip.test.sh's unicode strip cases and builtin-printf's
        // `☠` / `\U0000065f` / `\377`.
        assert_eq!(r("μabcμ".as_bytes()), "'\\xce\\xbcabc\\xce\\xbc'");
        assert_eq!(r("☠".as_bytes()), "'\\xe2\\x98\\xa0'");
        assert_eq!(r("ٟ".as_bytes()), "'\\xd9\\x9f'");
        assert_eq!(r(b"\xff"), "'\\xff'");
    }

    /// Every byte is accounted for exactly once, whatever it is. This is the
    /// test that does not depend on the corpus's contents: a text-oriented
    /// draft of `repr` decoded UTF-8 with a cursor and hung forever on a
    /// continuation byte, which no corpus case carries.
    #[test]
    fn every_byte_is_consumed_exactly_once() {
        for byte in 0..=u8::MAX {
            let out = r(&[byte]);
            assert!(out.len() >= 3, "byte {byte:#04x} produced {out:?}");
            // Everything outside printable ASCII is `\xNN`, bar the three
            // control bytes spelled by name.
            if !(0x20..=0x7e).contains(&byte) && !matches!(byte, b'\t' | b'\n' | b'\r') {
                assert_eq!(out, format!("'\\x{byte:02x}'"), "byte {byte:#04x}");
            }
        }
        // A high byte is escaped whether or not it is part of a valid
        // sequence, so neighbours are never swallowed.
        assert_eq!(r(b"a\xc3"), "'a\\xc3'");
        assert_eq!(r(b"\xc3a"), "'\\xc3a'");
        assert_eq!(r(b"\xe2\x82\xac"), "'\\xe2\\x82\\xac'");
    }
}
