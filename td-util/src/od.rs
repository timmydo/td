//! `od` — dump bytes: one single-byte type (`-t x1`, `o1`, `u1` or `d1`), an
//! address radix (`-A n|o|d|x`), a skip (`-j`), a count (`-N`) and `-v`, over
//! the files given (or stdin) read as one stream. That is the subset recipes
//! read headers and compare bytes with; anything else is refused rather than
//! approximated. Output is GNU od's: 16 bytes a line, a repeated line folded
//! to `*` unless `-v`, and a closing address line unless `-A n`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;

const USAGE: &str = "usage: od [-v] [-A n|o|d|x] -t x1|o1|u1|d1 [-j SKIP] [-N COUNT] [FILE...]";
const LINE: usize = 16;
/// Read size, independent of the 16-byte line: a read per line is a syscall
/// per 16 bytes.
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum Radix {
    None,
    Oct,
    Dec,
    Hex,
}

#[derive(Clone, Copy)]
enum Type {
    Hex,
    Oct,
    Unsigned,
    Signed,
}

struct Opts {
    radix: Radix,
    ty: Type,
    skip: u64,
    count: Option<u64>,
    verbose: bool,
    files: Vec<String>,
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(msg) => {
            crate::emit_err(&format!("od: {msg}\n"));
            return Ok(1);
        }
    };
    let mut input = Input::new(opts.files.clone());
    // As GNU's: when no input opens at all there is nothing to address.
    if !input.prime() {
        return Ok(1);
    }
    if let Err(msg) = input.skip(opts.skip) {
        crate::emit_err(&format!("od: {msg}\n"));
        return Ok(1);
    }
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = dump(&opts, &mut input, &mut out).and_then(|()| out.flush());
    match result {
        Ok(()) => Ok(u8::from(input.failed)),
        // A closed reader is not a failure, as `crate::emit` treats it; an
        // input that already failed still is.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(u8::from(input.failed)),
        Err(e) => {
            crate::emit_err(&format!("od: write error: {e}\n"));
            Ok(1)
        }
    }
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut radix = Radix::Oct;
    let mut ty = None;
    let mut skip = 0;
    let mut count = None;
    let mut verbose = false;
    let mut files = Vec::new();
    let mut it = args.iter();
    let mut opts_done = false;
    while let Some(a) = it.next() {
        let flags = match a.strip_prefix('-') {
            Some(f) if !opts_done && !f.is_empty() => f,
            _ => {
                files.push(a.clone());
                continue;
            }
        };
        if flags == "-" {
            opts_done = true;
            continue;
        }
        // Short options group as getopt's do: `-vtx1` is `-v -t x1`.
        for (i, c) in flags.char_indices() {
            if c == 'v' {
                verbose = true;
                continue;
            }
            if !matches!(c, 'A' | 't' | 'j' | 'N') {
                return Err(format!("unrecognised option '-{c}'\n{USAGE}"));
            }
            let attached = flags.get(i + c.len_utf8()..).unwrap_or("");
            let value = if attached.is_empty() {
                it.next()
                    .cloned()
                    .ok_or_else(|| format!("option '-{c}' needs a value\n{USAGE}"))?
            } else {
                attached.to_string()
            };
            match c {
                'A' => {
                    radix = match value.as_str() {
                        "n" => Radix::None,
                        "o" => Radix::Oct,
                        "d" => Radix::Dec,
                        "x" => Radix::Hex,
                        r => return Err(format!("invalid address radix '{r}'\n{USAGE}")),
                    }
                }
                't' => {
                    if ty.is_some() {
                        return Err(format!("only one -t type is served\n{USAGE}"));
                    }
                    ty = Some(match value.as_str() {
                        "x1" => Type::Hex,
                        "o1" => Type::Oct,
                        "u1" => Type::Unsigned,
                        "d1" => Type::Signed,
                        t => return Err(format!("unserved type '{t}'\n{USAGE}")),
                    })
                }
                'j' => skip = number(&value)?,
                _ => count = Some(number(&value)?),
            }
            break;
        }
    }
    // GNU's default type is two-byte octal words, which nothing here reads;
    // naming the type keeps a caller from depending on a default we lack.
    let ty = ty.ok_or_else(|| format!("a -t type is required\n{USAGE}"))?;
    if let Some(c) = count {
        if skip.checked_add(c).is_none() {
            return Err("skip-bytes + read-bytes is too large".to_string());
        }
    }
    if files.is_empty() {
        files.push("-".to_string());
    }
    Ok(Opts {
        radix,
        ty,
        skip,
        count,
        verbose,
        files,
    })
}

/// A byte count as GNU od reads one: leading blanks and a `+`, then decimal,
/// `0x` hexadecimal or `0`-led octal. Its multiplier suffixes are not served.
fn number(s: &str) -> Result<u64, String> {
    let bad = || format!("invalid byte count '{s}'");
    let t = s.trim_start();
    let t = t.strip_prefix('+').unwrap_or(t);
    let (digits, radix) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (h, 16)
    } else if let Some(o) = t.strip_prefix('0').filter(|o| !o.is_empty()) {
        (o, 8)
    } else {
        (t, 10)
    };
    // from_str_radix would take a second sign; the digits are checked first.
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return Err(bad());
    }
    u64::from_str_radix(digits, radix).map_err(|_| bad())
}

/// The named files as one stream. A file that cannot be opened or read is
/// reported and passed over, and the run exits 1, as GNU od does.
struct Input {
    files: std::vec::IntoIter<String>,
    cur: Option<(String, BufReader<File>)>,
    failed: bool,
}

impl Input {
    fn new(files: Vec<String>) -> Self {
        Self {
            files: files.into_iter(),
            cur: None,
            failed: false,
        }
    }

    /// Open the next file, or `None` at the end of the list. Stdin is taken as
    /// a duplicate of descriptor 0, so it can be measured and sought like any
    /// other file and shares stdin's offset.
    fn open_next(&mut self) -> Option<(String, File)> {
        while let Some(name) = self.files.next() {
            let opened = if name == "-" {
                use std::os::fd::AsFd;
                std::io::stdin()
                    .as_fd()
                    .try_clone_to_owned()
                    .map(File::from)
            } else {
                File::open(&name)
            };
            match opened {
                Ok(f) => return Some((name, f)),
                Err(e) => self.fail(&name, &e),
            }
        }
        None
    }

    fn fail(&mut self, name: &str, e: &std::io::Error) {
        crate::emit_err(&format!("od: {name}: {e}\n"));
        self.failed = true;
    }

    /// Open the first input that will open; false when none does.
    fn prime(&mut self) -> bool {
        match self.open_next() {
            Some((name, f)) => {
                self.cur = Some((name, BufReader::with_capacity(CHUNK, f)));
                true
            }
            None => false,
        }
    }

    /// Discard `n` bytes. A file whose size can be believed is sought within,
    /// rather than reading a disk image's leading gigabytes; GNU believes it
    /// only for a regular file larger than its block size, since procfs and
    /// sysfs files report a size of 0 and have bytes all the same.
    fn skip(&mut self, mut n: u64) -> Result<(), String> {
        while n > 0 {
            let (name, mut f) = match self.cur.take() {
                Some((name, r)) => (name, r.into_inner()),
                None => match self.open_next() {
                    Some(opened) => opened,
                    None => return Err("cannot skip past end of combined input".to_string()),
                },
            };
            let size = f
                .metadata()
                .ok()
                .filter(|m| m.is_file() && m.blksize() > 0 && m.blksize() < m.len())
                .map(|m| m.len());
            let left = match (size, f.stream_position()) {
                (Some(len), Ok(at)) => Some(len.saturating_sub(at)),
                _ => None,
            };
            // A count a seek cannot carry is read past instead.
            let seek = left.filter(|l| *l > n).and_then(|_| i64::try_from(n).ok());
            match (left, seek) {
                (Some(left), _) if left <= n => n -= left,
                (_, Some(k)) => match f.seek(SeekFrom::Current(k)) {
                    Ok(_) => {
                        n = 0;
                        self.cur = Some((name, BufReader::with_capacity(CHUNK, f)));
                    }
                    Err(e) => self.fail(&name, &e),
                },
                _ => {
                    // Counted as it goes, so bytes dropped before a read
                    // error are not skipped again in the next file.
                    let mut r = BufReader::with_capacity(CHUNK, f);
                    let result = loop {
                        if n == 0 {
                            break Ok(());
                        }
                        match r.fill_buf() {
                            Ok([]) => break Ok(()),
                            Ok(b) => {
                                let k = b.len().min(usize::try_from(n).unwrap_or(usize::MAX));
                                r.consume(k);
                                n = n.saturating_sub(k as u64);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => break Err(e),
                        }
                    };
                    match result {
                        Ok(()) if n == 0 => self.cur = Some((name, r)),
                        Ok(()) => {}
                        Err(e) => self.fail(&name, &e),
                    }
                }
            }
        }
        Ok(())
    }

    /// Fill `buf` from the stream, crossing file boundaries; returns how many
    /// bytes it holds, short only at the end of the last file. A read error
    /// fails that file and the stream goes on with the next.
    fn fill(&mut self, buf: &mut [u8]) -> usize {
        let mut got = 0;
        while got < buf.len() {
            if self.cur.is_none() && !self.prime() {
                break;
            }
            let Some((name, r)) = self.cur.as_mut() else {
                break;
            };
            let Some(rest) = buf.get_mut(got..) else {
                break;
            };
            match r.read(rest) {
                Ok(0) => self.cur = None,
                Ok(k) => got += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let name = std::mem::take(name);
                    self.cur = None;
                    self.fail(&name, &e);
                }
            }
        }
        got
    }
}

fn dump(opts: &Opts, input: &mut Input, out: &mut impl Write) -> std::io::Result<()> {
    let mut addr = opts.skip;
    let mut left = opts.count;
    let mut prev: Option<[u8; LINE]> = None;
    let mut folded = false;
    let mut buf = [0u8; LINE];
    let mut text = String::new();
    loop {
        let want = left.map_or(LINE, |l| LINE.min(usize::try_from(l).unwrap_or(LINE)));
        let Some(line) = buf.get_mut(..want) else {
            break;
        };
        let n = input.fill(line);
        let Some(bytes) = buf.get(..n) else {
            break;
        };
        if n == 0 {
            break;
        }
        if n == LINE && !opts.verbose && prev == Some(buf) {
            if !folded {
                out.write_all(b"*\n")?;
                folded = true;
            }
        } else {
            folded = false;
            text.clear();
            address(&mut text, opts.radix, addr);
            for b in bytes {
                field(&mut text, opts.ty, *b);
            }
            text.push('\n');
            out.write_all(text.as_bytes())?;
        }
        prev = (n == LINE).then_some(buf);
        addr = addr.saturating_add(n as u64);
        if let Some(l) = left.as_mut() {
            *l = l.saturating_sub(n as u64);
        }
        if n < want {
            break;
        }
    }
    if !matches!(opts.radix, Radix::None) {
        text.clear();
        address(&mut text, opts.radix, addr);
        text.push('\n');
        out.write_all(text.as_bytes())?;
    }
    Ok(())
}

fn address(text: &mut String, radix: Radix, addr: u64) {
    use std::fmt::Write;
    let _ = match radix {
        Radix::None => Ok(()),
        Radix::Oct => write!(text, "{addr:07o}"),
        Radix::Dec => write!(text, "{addr:07}"),
        Radix::Hex => write!(text, "{addr:06x}"),
    };
}

fn field(text: &mut String, ty: Type, b: u8) {
    use std::fmt::Write;
    let _ = match ty {
        Type::Hex => write!(text, " {b:02x}"),
        Type::Oct => write!(text, " {b:03o}"),
        Type::Unsigned => write!(text, " {b:3}"),
        Type::Signed => write!(text, " {:4}", i8::from_ne_bytes([b])),
    };
}
