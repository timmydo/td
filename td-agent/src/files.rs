//! The file tools' semantics (DESIGN.md §12): what `read_file`,
//! `write_file`, `edit_file` and `glob` do, as the tool host performs them
//! inside a jail instance (§2, §8). Confinement is the jail's mount
//! namespace, not path checks here; these hold the tools to what they
//! promise: absolute paths, bounded reads, the read-before-write digest,
//! exact edits and capped, sorted globs. Every refusal is a sentence the
//! model can act on.
//!
//! The digest is SHA-256 over the whole file, returned with every read,
//! partial ones included, and with every write. It is a correctness aid,
//! not a security check: the tool host is jail-controlled.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::sha256::{to_base16, Sha256};

/// The most lines one `read_file` shows.
pub const MAX_LINES: u64 = 2000;
/// The most bytes of numbered text one `read_file` shows.
pub const MAX_READ_BYTES: usize = 100 * 1024;
/// The longest line shown whole; a longer one is cut and says by how much.
pub const MAX_LINE_CHARS: usize = 2000;
/// The largest file `edit_file` takes, whole, into memory.
pub const MAX_EDIT_BYTES: u64 = 8 * 1024 * 1024;
/// The most paths one `glob` returns.
pub const MAX_GLOB: usize = 1000;
/// The most directory entries one `glob` looks at.
pub const MAX_GLOB_VISITS: usize = 200_000;
/// How much of a file's start is looked at for a NUL, which marks it
/// binary.
const SNIFF: usize = 8192;
/// The longest `glob` pattern, and the most `{` groups in one.
pub const MAX_PATTERN: usize = 1024;
pub const MAX_GROUPS: usize = 16;
/// `O_NONBLOCK` on Linux's x86-64, ARM and RISC-V, td's targets: opening
/// a FIFO with it does not wait for a writer.
const O_NONBLOCK: i32 = 0o4000;
/// Held from a replacement's digest check to its write, so two calls
/// that read the same file cannot both replace it. Writers outside the
/// tool host, in the jail, are not held.
static MUTATIONS: Mutex<()> = Mutex::new(());

/// The most bytes of one line kept to show: `MAX_LINE_CHARS` characters
/// of four bytes each, and one more, so a line is never held whole.
const LINE_KEEP: usize = 4 * MAX_LINE_CHARS + 4;

/// What a `read_file` found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct View {
    /// The lines shown, each numbered as `cat -n` does.
    pub text: String,
    /// The whole file's digest.
    pub digest: String,
    /// The first line shown, counted from 1, and the last; `last` is
    /// `first - 1` when none is.
    pub first: u64,
    pub last: u64,
    /// The file's lines, whole.
    pub total: u64,
    /// Its bytes.
    pub bytes: u64,
    /// Whether it holds a NUL near its start, and so is shown as no lines.
    pub binary: bool,
    /// What else the reader should know: bytes that are not UTF-8, or a
    /// line cut.
    pub notes: Vec<String>,
}

impl View {
    /// The line to read on from, when the view stopped before the end.
    pub fn next(&self) -> Option<u64> {
        (!self.binary && self.last < self.total).then_some(self.last.saturating_add(1))
    }

    /// The result as the model is given it: the numbered lines, then a
    /// line saying what part this is and how to go on.
    pub fn render(&self, path: &str) -> String {
        let mut out = self.text.clone();
        let mut tail = Vec::new();
        if self.binary {
            tail.push(format!(
                "{path} is a binary file of {} bytes, not shown; its digest is in hand for write_file",
                self.bytes
            ));
        } else if self.total == 0 {
            tail.push(format!("{path} is empty ({} bytes)", self.bytes));
        } else if self.first > self.total {
            tail.push(format!(
                "{path} has {} lines; offset {} is past its end",
                self.total, self.first
            ));
        } else if let Some(next) = self.next() {
            tail.push(format!(
                "lines {}-{} of {}; read on with offset {next}",
                self.first, self.last, self.total
            ));
        }
        tail.extend(self.notes.iter().cloned());
        for line in tail {
            out.push_str(&format!("[{line}]\n"));
        }
        out
    }
}

/// What a `write_file` or `edit_file` left.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Written {
    /// The file's digest now, which a further edit expects.
    pub digest: String,
    /// Whether the file was made by this write.
    pub created: bool,
    /// How many places an edit replaced; 1 for a write.
    pub replaced: usize,
    pub bytes: u64,
}

/// What a `glob` matched.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Globbed {
    /// Absolute paths, sorted.
    pub paths: Vec<String>,
    /// Whether it stopped at `MAX_GLOB` matches or `MAX_GLOB_VISITS`
    /// entries, so there may be more.
    pub more: bool,
}

/// A path the model gave, which must be absolute; a relative one is
/// refused naming the worktrees it could be under (§12).
pub fn absolute(path: &str, name: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err(format!("`{name}` is empty"));
    }
    let given = Path::new(path);
    if given.is_absolute() {
        return Ok(given.to_path_buf());
    }
    let roots = if roots.is_empty() {
        "none".to_string()
    } else {
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(format!(
        "`{name}` is {path:?}, a relative path; paths are absolute, under a worktree: {roots}"
    ))
}

/// The digest of a whole regular file, read in pieces.
pub fn digest_file(path: &Path) -> Result<String, String> {
    let shown = path.display().to_string();
    let (mut file, _) = open_regular(path, &shown, "the digest")?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("{shown}: {e}"))?;
        if n == 0 {
            return Ok(to_base16(&hasher.finalize()));
        }
        hasher.update(buffer.get(..n).unwrap_or_default());
    }
}

/// The digest of bytes in hand.
pub fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_base16(&hasher.finalize())
}

/// What `path` is, as the tools refuse it: missing, or a directory.
fn regular(path: &Path, shown: &str, verb: &str) -> Result<fs::Metadata, String> {
    let meta = fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("{shown} does not exist"),
        _ => format!("{shown}: {e}"),
    })?;
    if meta.is_dir() {
        return Err(format!(
            "{shown} is a directory; {verb} takes a file, and glob lists a directory"
        ));
    }
    if !meta.is_file() {
        return Err(format!(
            "{shown} is not a regular file (a device, FIFO or socket); {verb} takes a regular file"
        ));
    }
    Ok(meta)
}

/// Opens a regular file to read: without waiting on a FIFO, and refused
/// when what was opened is not a regular file, whatever `path` was when
/// it was looked at, since a device or FIFO may never end.
fn open_regular(path: &Path, shown: &str, verb: &str) -> Result<(File, fs::Metadata), String> {
    regular(path, shown, verb)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{shown}: {e}"))?;
    let meta = file.metadata().map_err(|e| format!("{shown}: {e}"))?;
    if !meta.is_file() {
        return Err(format!(
            "{shown} is not a regular file (a device, FIFO or socket); {verb} takes a regular file"
        ));
    }
    Ok((file, meta))
}

/// `read_file`: lines `offset` (from 1) on, at most `limit` of them and
/// `MAX_LINES`, and at most `MAX_READ_BYTES` of numbered text; the digest
/// is always the whole file's, so the whole file is read, unless `cancel`
/// is set first.
pub fn read(
    path: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
    cancel: &AtomicBool,
) -> Result<View, String> {
    let shown = path.display().to_string();
    let (mut file, meta) = open_regular(path, &shown, "read_file")?;
    let first = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(MAX_LINES).clamp(1, MAX_LINES);
    let end = first.saturating_add(limit);
    let mut hasher = Sha256::new();
    let mut read = View {
        text: String::new(),
        digest: String::new(),
        first,
        last: first.saturating_sub(1),
        total: 0,
        bytes: meta.len(),
        binary: false,
        notes: Vec::new(),
    };
    let mut chunk = vec![0u8; 64 * 1024];
    // The line in progress: what is kept of it, the characters past
    // that, whether its last byte was `\r`, and whether it has begun.
    let mut line = Vec::new();
    let mut over = 0u64;
    let mut cr = false;
    let mut begun = false;
    let mut seen = 0usize;
    let mut so = Showing::default();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("reading {shown} was interrupted"));
        }
        let n = file.read(&mut chunk).map_err(|e| format!("{shown}: {e}"))?;
        if n == 0 {
            break;
        }
        let data = chunk.get(..n).unwrap_or_default();
        hasher.update(data);
        if seen < SNIFF && data.iter().take(SNIFF - seen).any(|b| *b == 0) {
            read.binary = true;
        }
        seen = seen.saturating_add(n);
        for piece in data.split_inclusive(|b| *b == b'\n') {
            let (body, ends) = match piece.strip_suffix(b"\n") {
                Some(body) => (body, true),
                None => (piece, false),
            };
            begun = true;
            let number = read.total.saturating_add(1);
            let wanted = !read.binary && !so.full && number >= first && number < end;
            if wanted {
                let room = LINE_KEEP.saturating_sub(line.len()).min(body.len());
                let (kept, rest) = body.split_at(room);
                line.extend_from_slice(kept);
                over = over.saturating_add(characters(rest));
                if let Some(last) = body.last() {
                    cr = *last == b'\r';
                }
            }
            if ends {
                read.total = number;
                if wanted {
                    so.take(&mut read, &line, over, cr);
                }
                line.clear();
                (over, cr, begun) = (0, false, false);
            }
        }
    }
    if begun {
        read.total = read.total.saturating_add(1);
        if !read.binary && !so.full && read.total >= first && read.total < end {
            so.take(&mut read, &line, over, cr);
        }
    }
    read.digest = to_base16(&hasher.finalize());
    if read.binary {
        read.text.clear();
        read.last = first.saturating_sub(1);
    } else {
        if so.lossy {
            read.notes
                .push("some lines are not UTF-8; their invalid bytes show as \u{fffd}".into());
        }
        if so.cut > 0 {
            read.notes.push(format!(
                "{} lines longer than {MAX_LINE_CHARS} characters are cut",
                so.cut
            ));
        }
    }
    Ok(read)
}

/// What a read has shown so far, past its text.
#[derive(Default)]
struct Showing {
    /// A line held bytes that are not UTF-8.
    lossy: bool,
    /// The lines cut.
    cut: u64,
    /// The next line would pass `MAX_READ_BYTES`, so no more are shown.
    full: bool,
}

impl Showing {
    /// Shows the line just ended, the view's last, unless it is full.
    fn take(&mut self, read: &mut View, line: &[u8], over: u64, cr: bool) {
        let number = read.total;
        let (numbered, lossy, long) = numbered(number, line, over, cr);
        if read.text.len().saturating_add(numbered.len()) > MAX_READ_BYTES {
            self.full = true;
            return;
        }
        self.lossy |= lossy;
        self.cut = self.cut.saturating_add(u64::from(long));
        read.text.push_str(&numbered);
        read.last = number;
    }
}

/// How many characters `bytes` holds, as UTF-8: its bytes that are not
/// continuation bytes.
fn characters(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|b| **b & 0xC0 != 0x80).count() as u64
}

/// One line as `read_file` shows it, from what was kept of it and the
/// characters past that; with whether it held bytes that are not UTF-8
/// and whether it was cut.
fn numbered(number: u64, kept: &[u8], mut over: u64, cr: bool) -> (String, bool, bool) {
    let mut body = kept;
    if over == 0 {
        body = body.strip_suffix(b"\r").unwrap_or(body);
    } else {
        over = over.saturating_sub(u64::from(cr));
        // A character the keeping cut in two is counted, not shown.
        if let Err(e) = std::str::from_utf8(body) {
            if e.error_len().is_none() {
                body = body.get(..e.valid_up_to()).unwrap_or(body);
                over = over.saturating_add(1);
            }
        }
    }
    let text = String::from_utf8_lossy(body);
    let lossy = matches!(text, std::borrow::Cow::Owned(_));
    let mut line: String = text.chars().take(MAX_LINE_CHARS).collect();
    let more = (text.chars().count() as u64)
        .saturating_add(over)
        .saturating_sub(MAX_LINE_CHARS as u64);
    if more > 0 {
        line.push_str(&format!(" [... {more} more characters]"));
    }
    (format!("{number:>6}\t{line}\n"), lossy, more > 0)
}

/// The refusal of a replacement whose digest does not match: `expected`
/// is the digest of this conversation's last read or write of the file,
/// and `now` the digest it has.
fn unchanged(shown: &str, now: &str, expected: Option<&str>) -> Result<(), String> {
    match expected {
        None => Err(format!(
            "{shown} exists and this conversation has not read it; read it with read_file before replacing or editing it"
        )),
        Some(expected) if expected != now => Err(format!(
            "{shown} changed since this conversation last read it; read it again with read_file"
        )),
        Some(_) => Ok(()),
    }
}

/// Writes `bytes` to `path`, in place, so its mode and links stay; a
/// `fresh` file is made, and refused if something is there by now.
fn put(path: &Path, shown: &str, bytes: &[u8], fresh: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    // Not waiting on a FIFO put where the file was looked at.
    options.write(true).custom_flags(O_NONBLOCK);
    if fresh {
        options.create_new(true);
    } else {
        options.truncate(true);
    }
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
            format!("{shown} appeared while it was being written; read it with read_file")
        }
        _ => format!("{shown}: {e}"),
    })?;
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(format!(
            "{shown} is not a regular file (a device, FIFO or socket); write_file takes a regular file"
        ));
    }
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|e| format!("{shown}: {e}"))
}

/// `write_file`: creates `path`, with its missing directories, or replaces
/// it when `expected` is the digest it has now (§12).
pub fn write(path: &Path, content: &str, expected: Option<&str>) -> Result<Written, String> {
    let shown = path.display().to_string();
    let _held = MUTATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let created = match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(format!("{shown} is a directory; write_file writes a file"))
        }
        Ok(_) => {
            let now = digest_file(path)?;
            unchanged(&shown, &now, expected)?;
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(format!(
                    "{shown} is a symbolic link to nothing; write to the path it names, or remove the link"
                ));
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            true
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    put(path, &shown, content.as_bytes(), created)?;
    Ok(Written {
        digest: digest(content.as_bytes()),
        created,
        replaced: 1,
        bytes: content.len() as u64,
    })
}

/// `edit_file`: replaces `old` with `new` in `path`, which must hold it
/// exactly once, or at least once with `all` (§12). No fuzzy matching.
pub fn edit(
    path: &Path,
    old: &str,
    new: &str,
    all: bool,
    expected: Option<&str>,
) -> Result<Written, String> {
    let shown = path.display().to_string();
    if old.is_empty() {
        return Err(
            "`old_string` is empty; give the exact text to replace, or create the file with write_file"
                .into(),
        );
    }
    if old == new {
        return Err("`old_string` and `new_string` are the same; nothing would change".into());
    }
    let _held = MUTATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (file, meta) = open_regular(path, &shown, "edit_file")?;
    let too_big = |bytes: u64| {
        format!("{shown} is {bytes} bytes, past edit_file's {MAX_EDIT_BYTES}; change it with sed")
    };
    if meta.len() > MAX_EDIT_BYTES {
        return Err(too_big(meta.len()));
    }
    // Bounded by what is read, not by the length looked at first.
    let mut bytes = Vec::new();
    file.take(MAX_EDIT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    if bytes.len() as u64 > MAX_EDIT_BYTES {
        return Err(too_big(bytes.len() as u64));
    }
    unchanged(&shown, &digest(&bytes), expected)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{shown} is not UTF-8 text; change it with sed"))?;
    let count = text.matches(old).count();
    if count == 0 {
        return Err(format!(
            "`old_string` is not in {shown}; read the file again and copy the text exactly, whitespace and indentation included"
        ));
    }
    if count > 1 && !all {
        return Err(format!(
            "`old_string` occurs {count} times in {shown}; include more of the surrounding text so it occurs once, or set replace_all to replace every one"
        ));
    }
    let changed = if all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    put(path, &shown, changed.as_bytes(), false)?;
    Ok(Written {
        digest: digest(changed.as_bytes()),
        created: false,
        replaced: count,
        bytes: changed.len() as u64,
    })
}

/// `glob`: the files under `base` whose path relative to it matches
/// `pattern`, sorted, at most `MAX_GLOB`. `*` and `?` match within a
/// path segment, `**` any number of segments, `[...]` a class, and
/// `{a,b}` either. `.git` directories are not entered, nor are links to
/// directories followed.
pub fn glob(pattern: &str, base: &Path) -> Result<Globbed, String> {
    let shown = base.display().to_string();
    if pattern.is_empty() {
        return Err("`pattern` is empty".into());
    }
    if pattern.len() > MAX_PATTERN {
        return Err(format!(
            "`pattern` is {} bytes, past {MAX_PATTERN}",
            pattern.len()
        ));
    }
    // Expansion recurses once a group, so groups are bounded.
    if pattern.matches('{').count() > MAX_GROUPS {
        return Err(format!("`pattern` has more than {MAX_GROUPS} `{{` groups"));
    }
    if pattern.starts_with('/') {
        return Err(format!(
            "`pattern` is {pattern:?}; it is matched under `path` ({shown}), so give it relative, such as **/*.rs"
        ));
    }
    match fs::metadata(base) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{shown} is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{shown} does not exist"))
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    }
    let patterns: Vec<Vec<String>> = expand(pattern)?
        .iter()
        .map(|p| {
            p.split('/')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .collect();
    let mut found = Globbed::default();
    let mut visits = 0usize;
    let mut stack = vec![(base.to_path_buf(), Vec::<String>::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visits = visits.saturating_add(1);
            if visits > MAX_GLOB_VISITS {
                found.more = true;
                stack.clear();
                break;
            }
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let mut path = rel.clone();
            path.push(name.clone());
            if kind.is_dir() {
                if name != ".git" {
                    stack.push((entry.path(), path));
                }
                continue;
            }
            let segments: Vec<&str> = path.iter().map(String::as_str).collect();
            if patterns.iter().any(|p| {
                let p: Vec<&str> = p.iter().map(String::as_str).collect();
                matches_path(&p, &segments)
            }) {
                found.paths.push(entry.path().display().to_string());
            }
        }
    }
    found.paths.sort();
    if found.paths.len() > MAX_GLOB {
        found.paths.truncate(MAX_GLOB);
        found.more = true;
    }
    Ok(found)
}

/// The patterns a `{a,b}` group stands for, groups nested and in series.
fn expand(pattern: &str) -> Result<Vec<String>, String> {
    let Some(open) = pattern.find('{') else {
        return Ok(vec![pattern.to_string()]);
    };
    let mut depth = 0usize;
    let mut close = None;
    let mut commas = Vec::new();
    for (at, c) in pattern.char_indices().skip_while(|(at, _)| *at < open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    close = Some(at);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(at),
            _ => {}
        }
    }
    let close = close.ok_or_else(|| format!("`pattern` {pattern:?} has a `{{` with no `}}`"))?;
    let head = pattern.get(..open).unwrap_or_default();
    let tail = pattern.get(close + 1..).unwrap_or_default();
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    let mut out = Vec::new();
    for pair in bounds.windows(2) {
        let (Some(&from), Some(&to)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        let choice = pattern.get(from + 1..to).unwrap_or_default();
        for expanded in expand(&format!("{head}{choice}{tail}"))? {
            if out.len() >= 64 {
                return Err(format!(
                    "`pattern` {pattern:?} expands to more than 64 patterns"
                ));
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

/// Whether path segments match pattern segments, `**` standing for any
/// number of them.
fn matches_path(pattern: &[&str], path: &[&str]) -> bool {
    wildcard(
        pattern,
        path,
        |p| *p == "**",
        |p, segment| {
            let p: Vec<char> = p.first()?.chars().collect();
            let s: Vec<char> = segment.chars().collect();
            matches_segment(&p, &s).then_some(1)
        },
    )
}

/// Whether one segment matches: `*` any run, `?` one character, and
/// `[...]` a class (`!` or `^` first negates, `a-z` a range). A leading
/// dot is matched only by a dot, as a shell does.
fn matches_segment(pattern: &[char], name: &[char]) -> bool {
    if name.first() == Some(&'.') && pattern.first() != Some(&'.') {
        return false;
    }
    wildcard(
        pattern,
        name,
        |p| *p == '*',
        |rest, c| match rest.split_first() {
            Some(('?', _)) => Some(1),
            Some(('[', after)) => match class(after) {
                Some((members, negate, used)) => {
                    (member(members, *c) != negate).then_some(used.saturating_add(1))
                }
                None => (*c == '[').then_some(1),
            },
            Some((p, _)) => (p == c).then_some(1),
            None => None,
        },
    )
}

/// Matches `items` against `pattern`, where `star(p)` marks a pattern
/// element that takes any run of items and `one(rest, item)` says how
/// many pattern elements, from the head of `rest`, take one item.
/// Backtracking to the last star alone suffices, so a match takes at
/// most the product of the two lengths, never an exponential search.
fn wildcard<P, I>(
    pattern: &[P],
    items: &[I],
    star: impl Fn(&P) -> bool,
    one: impl Fn(&[P], &I) -> Option<usize>,
) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let mut last: Option<(usize, usize)> = None;
    while let Some(item) = items.get(i) {
        let rest = pattern.get(p..).unwrap_or_default();
        if rest.first().is_some_and(&star) {
            last = Some((p.saturating_add(1), i));
            p = p.saturating_add(1);
            continue;
        }
        match (one(rest, item), last) {
            (Some(used), _) => {
                p = p.saturating_add(used);
                i = i.saturating_add(1);
            }
            (None, Some((after, from))) => {
                p = after;
                i = from.saturating_add(1);
                last = Some((after, i));
            }
            (None, None) => return false,
        }
    }
    pattern.get(p..).unwrap_or_default().iter().all(star)
}

/// A `[...]` class after its `[`: its members, whether it is negated,
/// and how many characters it takes through its `]`; none when there is
/// no `]`, so the `[` is literal. A `]` first is a member, not the end.
fn class(pattern: &[char]) -> Option<(&[char], bool, usize)> {
    let (negate, body, skip) = match pattern.split_first() {
        Some(('!' | '^', rest)) => (true, rest, 1),
        _ => (false, pattern, 0),
    };
    let end = body.iter().skip(1).position(|c| *c == ']')? + 1;
    Some((body.get(..end)?, negate, skip + end + 1))
}

/// Whether `c` is one of a class's members, `a-z` a range.
fn member(members: &[char], c: char) -> bool {
    let mut at = 0usize;
    while let Some(&m) = members.get(at) {
        if let (Some('-'), Some(&hi)) = (members.get(at + 1), members.get(at + 2)) {
            if (m..=hi).contains(&c) {
                return true;
            }
            at += 3;
            continue;
        }
        if m == c {
            return true;
        }
        at += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use std::process::Command;

    fn read(path: &Path, offset: Option<u64>, limit: Option<u64>) -> Result<View, String> {
        super::read(path, offset, limit, &AtomicBool::new(false))
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-agent-files-{tag}-{}-{}",
                std::process::id(),
                crate::store::random_hex(4).unwrap()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_relative_path_is_refused_naming_the_worktrees() {
        let roots = [PathBuf::from("/w/a"), PathBuf::from("/w/b")];
        assert_eq!(
            absolute("/w/a/x", "path", &roots).unwrap(),
            PathBuf::from("/w/a/x")
        );
        let e = absolute("src/x.rs", "path", &roots).unwrap_err();
        assert!(e.contains("relative") && e.contains("/w/a, /w/b"), "{e}");
        assert!(absolute("", "path", &roots).unwrap_err().contains("empty"));
    }

    #[test]
    fn a_read_is_numbered_bounded_and_carries_the_whole_digest() {
        let dir = Dir::new("read");
        let text: String = (1..=2500).map(|n| format!("line {n}\n")).collect();
        let path = dir.file("a.txt", text.as_bytes());
        let whole = digest(text.as_bytes());
        let first = read(&path, None, None).unwrap();
        assert_eq!((first.first, first.last, first.total), (1, 2000, 2500));
        assert!(first.text.starts_with("     1\tline 1\n"));
        assert_eq!(first.digest, whole);
        assert_eq!(first.next(), Some(2001));
        assert!(first
            .render("/a")
            .contains("lines 1-2000 of 2500; read on with offset 2001"));
        let rest = read(&path, Some(2001), Some(10)).unwrap();
        assert_eq!((rest.first, rest.last), (2001, 2010));
        assert!(rest.text.starts_with("  2001\tline 2001\n"));
        assert_eq!(rest.digest, whole, "a partial read has the whole digest");
        let end = read(&path, Some(2495), None).unwrap();
        assert_eq!((end.last, end.next()), (2500, None));
        let past = read(&path, Some(3000), None).unwrap();
        assert!(past.text.is_empty());
        assert!(past.render("/a").contains("past its end"));
    }

    #[test]
    fn a_read_stops_at_its_byte_bound_and_cuts_long_lines() {
        let dir = Dir::new("bytes");
        let long = "x".repeat(MAX_LINE_CHARS + 5);
        let text: String = (0..200).map(|_| format!("{long}\n")).collect();
        let path = dir.file("wide.txt", text.as_bytes());
        let view = read(&path, None, None).unwrap();
        assert!(view.text.len() <= MAX_READ_BYTES);
        assert!(view.last < 200 && view.next().is_some());
        assert!(view.text.contains("[... 5 more characters]"));
        assert!(view.notes.iter().any(|n| n.contains("are cut")));
    }

    #[test]
    fn binary_empty_missing_and_directories_are_said() {
        let dir = Dir::new("kinds");
        let bin = dir.file("b.bin", b"\x7fELF\x00\x01\x02\n");
        let view = read(&bin, None, None).unwrap();
        assert!(view.text.is_empty());
        assert!(view.render("/b").contains("binary file of 8 bytes"));
        assert_eq!(view.digest, digest(b"\x7fELF\x00\x01\x02\n"));
        let empty = dir.file("e.txt", b"");
        assert!(read(&empty, None, None)
            .unwrap()
            .render("/e")
            .contains("is empty"));
        let latin = dir.file("l.txt", b"caf\xe9\n");
        let view = read(&latin, None, None).unwrap();
        assert!(view.text.contains("caf\u{fffd}"));
        assert!(view.notes.iter().any(|n| n.contains("not UTF-8")));
        assert!(read(&dir.0.join("none"), None, None)
            .unwrap_err()
            .contains("does not exist"));
        assert!(read(&dir.0, None, None)
            .unwrap_err()
            .contains("glob lists a directory"));
    }

    #[test]
    fn a_write_creates_freely_and_replaces_only_what_was_read() {
        let dir = Dir::new("write");
        let path = dir.0.join("new/deep/f.txt");
        let made = write(&path, "one\n", None).unwrap();
        assert!(made.created);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
        // Unread, it is not replaced.
        let e = write(&path, "two\n", None).unwrap_err();
        assert!(e.contains("has not read it"), "{e}");
        // Read, or written by this conversation, it is.
        let replaced = write(&path, "two\n", Some(&made.digest)).unwrap();
        assert!(!replaced.created);
        assert_eq!(replaced.digest, digest(b"two\n"));
        // Changed since, it is not.
        fs::write(&path, "someone else\n").unwrap();
        let e = write(&path, "three\n", Some(&replaced.digest)).unwrap_err();
        assert!(e.contains("changed since"), "{e}");
        let now = read(&path, None, None).unwrap().digest;
        write(&path, "three\n", Some(&now)).unwrap();
        assert!(write(&dir.0, "x", None)
            .unwrap_err()
            .contains("is a directory"));
    }

    #[test]
    fn an_edit_is_exact_unique_and_read_first() {
        let dir = Dir::new("edit");
        let path = dir.file("e.rs", b"fn a() {}\nfn b() {}\nfn b() {}\n");
        let seen = read(&path, None, None).unwrap().digest;
        assert!(edit(&path, "fn a", "fn z", false, None)
            .unwrap_err()
            .contains("has not read it"));
        let e = edit(&path, "fn q", "fn z", false, Some(&seen)).unwrap_err();
        assert!(e.contains("is not in") && e.contains("exactly"), "{e}");
        let e = edit(&path, "fn b() {}", "fn c() {}", false, Some(&seen)).unwrap_err();
        assert!(
            e.contains("occurs 2 times") && e.contains("replace_all"),
            "{e}"
        );
        assert!(edit(&path, "", "x", false, Some(&seen))
            .unwrap_err()
            .contains("empty"));
        assert!(edit(&path, "a", "a", false, Some(&seen))
            .unwrap_err()
            .contains("the same"));
        let one = edit(&path, "fn a() {}", "fn z() {}", false, Some(&seen)).unwrap();
        assert_eq!(one.replaced, 1);
        let all = edit(&path, "fn b() {}", "fn c() {}", true, Some(&one.digest)).unwrap();
        assert_eq!(all.replaced, 2);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "fn z() {}\nfn c() {}\nfn c() {}\n"
        );
        assert_eq!(all.digest, digest_file(&path).unwrap());
        // A stale digest is refused.
        assert!(edit(&path, "fn z", "fn y", false, Some(&seen))
            .unwrap_err()
            .contains("changed since"));
        let bin = dir.file("b.bin", b"\xff\xfe");
        let d = digest(b"\xff\xfe");
        assert!(edit(&bin, "a", "b", false, Some(&d))
            .unwrap_err()
            .contains("not UTF-8"));
    }

    #[test]
    fn a_glob_matches_segments_sorted_and_skips_git() {
        let dir = Dir::new("glob");
        for rel in [
            "src/main.rs",
            "src/a/b.rs",
            "src/a/b.txt",
            "Cargo.toml",
            ".hidden.rs",
            ".git/config.rs",
            "x[1].rs",
        ] {
            dir.file(rel, b"");
        }
        let names = |pattern: &str| -> Vec<String> {
            glob(pattern, &dir.0)
                .unwrap()
                .paths
                .iter()
                .map(|p| {
                    p.strip_prefix(&format!("{}/", dir.0.display()))
                        .unwrap()
                        .to_string()
                })
                .collect()
        };
        assert_eq!(names("**/*.rs"), ["src/a/b.rs", "src/main.rs", "x[1].rs"]);
        assert_eq!(names("src/*.rs"), ["src/main.rs"]);
        assert_eq!(names("**/*.{rs,toml}").len(), 4);
        assert_eq!(names("src/?/b.[rt]*"), ["src/a/b.rs", "src/a/b.txt"]);
        assert_eq!(names("src/a/b.[!r]*"), ["src/a/b.txt"]);
        assert_eq!(names(".*.rs"), [".hidden.rs"]);
        assert_eq!(names("*.toml"), ["Cargo.toml"]);
        assert!(glob("/abs/*", &dir.0).unwrap_err().contains("relative"));
        assert!(glob("{a,b", &dir.0).unwrap_err().contains("no `}`"));
        assert!(glob("*", &dir.0.join("Cargo.toml"))
            .unwrap_err()
            .contains("not a directory"));
    }

    #[test]
    fn matching_is_never_exponential() {
        let name: Vec<char> = "a".repeat(250).chars().collect();
        let pattern: Vec<char> = "*a".repeat(30).chars().chain(['b']).collect();
        let started = std::time::Instant::now();
        assert!(!matches_segment(&pattern, &name));
        let deep: Vec<&str> = std::iter::repeat_n("d", 200).collect();
        let stars: Vec<&str> = std::iter::repeat_n("**", 30).chain(["x"]).collect();
        assert!(!matches_path(&stars, &deep));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let p = |s: &str| s.chars().collect::<Vec<_>>();
        assert!(matches_segment(&p("*.r?"), &p("main.rs")));
        assert!(matches_segment(&p("a*b*c"), &p("aXbYbZc")));
        assert!(!matches_segment(&p("a*b*c"), &p("aXbYbZ")));
        assert!(matches_segment(&p("[]x]*"), &p("]q")));
        assert!(matches_segment(&p("[!a-c]"), &p("d")));
        assert!(matches_segment(&p("x[1"), &p("x[1")));
        assert!(matches_path(&["**"], &[]));
        assert!(matches_path(&["a", "**", "b"], &["a", "b"]));
        assert!(matches_path(&["a", "**", "b"], &["a", "x", "y", "b"]));
        assert!(!matches_path(&["a", "**", "b"], &["a", "x", "c"]));
    }

    #[test]
    fn lines_are_split_across_chunks_and_kept_bounded() {
        let dir = Dir::new("chunks");
        // A CRLF split by the 64 KiB chunk boundary, and a last line with
        // no newline.
        let mut text = "x".repeat(64 * 1024 - 1);
        text.push_str("\r\nsecond\r\nlast");
        let path = dir.file("crlf.txt", text.as_bytes());
        let view = read(&path, Some(2), None).unwrap();
        assert_eq!(view.text, "     2\tsecond\n     3\tlast\n");
        let view = read(&path, None, Some(1)).unwrap();
        assert!(
            view.text.ends_with("x [... 63535 more characters]\n"),
            "{}",
            view.text
        );
        assert_eq!(view.total, 3);
        // One long line of two-byte characters, never held whole.
        let wide = "\u{e9}".repeat(1_000_000);
        let path = dir.file("wide.txt", wide.as_bytes());
        let view = read(&path, None, None).unwrap();
        assert!(
            view.text.ends_with(" [... 998000 more characters]\n"),
            "{}",
            view.text.len()
        );
        assert!(view.notes.iter().all(|n| !n.contains("not UTF-8")));
        assert_eq!(view.digest, digest(wide.as_bytes()));
    }

    #[test]
    fn devices_and_fifos_are_refused_and_reads_cancel() {
        for verb in ["read_file", "edit_file"] {
            let e = match verb {
                "read_file" => read(Path::new("/dev/zero"), None, None).unwrap_err(),
                _ => edit(Path::new("/dev/zero"), "a", "b", false, Some("x")).unwrap_err(),
            };
            assert!(e.contains("not a regular file"), "{verb}: {e}");
        }
        let dir = Dir::new("fifo");
        let fifo = dir.0.join("f");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        assert!(read(&fifo, None, None)
            .unwrap_err()
            .contains("not a regular file"));
        assert!(write(&fifo, "x", Some("x"))
            .unwrap_err()
            .contains("not a regular file"));
        let path = dir.file("a.txt", b"a\n");
        let e = super::read(&path, None, None, &AtomicBool::new(true)).unwrap_err();
        assert!(e.contains("interrupted"), "{e}");
    }

    #[test]
    fn replacements_of_one_read_do_not_both_land() {
        let dir = Dir::new("race");
        let path = dir.file("a.txt", b"zero\n");
        let seen = digest(b"zero\n");
        let results: Vec<Result<Written, String>> = std::thread::scope(|scope| {
            let calls: Vec<_> = (0..8)
                .map(|n| {
                    let (path, seen) = (&path, &seen);
                    scope.spawn(move || write(path, &format!("{n}\n"), Some(seen)))
                })
                .collect();
            calls.into_iter().map(|c| c.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        // Creation is exclusive: one of two makers of a new file wins.
        let dangling = dir.0.join("dangling");
        std::os::unix::fs::symlink(dir.0.join("nowhere"), &dangling).unwrap();
        assert!(write(&dangling, "x", None)
            .unwrap_err()
            .contains("link to nothing"));
        let fresh = dir.0.join("new.txt");
        let made: Vec<_> = std::thread::scope(|scope| {
            let calls: Vec<_> = (0..4)
                .map(|n| {
                    let fresh = &fresh;
                    scope.spawn(move || write(fresh, &format!("{n}"), None))
                })
                .collect();
            calls.into_iter().map(|c| c.join().unwrap()).collect()
        });
        assert_eq!(made.iter().filter(|r| r.is_ok()).count(), 1);
    }

    #[test]
    fn a_glob_pattern_is_bounded() {
        let dir = Dir::new("nest");
        let nested = format!("{}a{}", "{".repeat(2500), "}".repeat(2500));
        assert!(glob(&nested, &dir.0).unwrap_err().contains("bytes, past"));
        let groups = "{a}".repeat(MAX_GROUPS + 1);
        assert!(glob(&groups, &dir.0).unwrap_err().contains("groups"));
        assert!(glob(&"{a}".repeat(MAX_GROUPS), &dir.0).is_ok());
        assert_eq!(
            expand("{a,{b,c}}{1,2}").unwrap(),
            ["a1", "a2", "b1", "b2", "c1", "c2"]
        );
        assert!(expand("{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}{a,b}")
            .unwrap_err()
            .contains("more than 64"));
    }

    #[test]
    fn a_glob_is_capped() {
        let dir = Dir::new("cap");
        for n in 0..(MAX_GLOB + 3) {
            dir.file(&format!("f{n:05}.txt"), b"");
        }
        let found = glob("*.txt", &dir.0).unwrap();
        assert_eq!(found.paths.len(), MAX_GLOB);
        assert!(found.more);
        assert!(found.paths.windows(2).all(|w| w[0] < w[1]));
    }
}
