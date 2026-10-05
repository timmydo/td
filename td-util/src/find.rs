//! `find` — the subset build systems and td's own scripts use: POSIX
//! primaries plus GNU's `-maxdepth`/`-mindepth`/`-print0`/`-delete`/`-iname`/
//! `-mmin`/`-empty`/`-quit`. Directory entries are visited in byte order, not
//! readdir order, so an archive built from `find` output is reproducible.

use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use crate::glob;

/// `-exec ... +` batches up to this many argument bytes per invocation, well
/// under any kernel's ARG_MAX.
const EXEC_BATCH_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmp {
    Less,
    Exactly,
    More,
}

impl Cmp {
    fn holds(self, have: u64, want: u64) -> bool {
        match self {
            Cmp::Less => have < want,
            Cmp::Exactly => have == want,
            Cmp::More => have > want,
        }
    }
}

#[derive(Debug)]
enum Perm {
    Exact(u32),
    All(u32),
    Any(u32),
}

#[derive(Debug)]
enum Node {
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Const(bool),
    Name(Vec<u8>, bool),
    Path(Vec<u8>, bool),
    Type(u8),
    Newer(SystemTime),
    /// `-mtime`: whole days of age, rounded down.
    Age {
        cmp: Cmp,
        n: u64,
        unit_secs: u64,
    },
    /// `-mmin`: the exact age against N minutes, as GNU compares it: `N` is
    /// over N-1 and at most N, `+N` over N, `-N` under N.
    Minutes(Cmp, u64),
    Size {
        cmp: Cmp,
        n: u64,
        unit: u64,
    },
    Perm(Perm),
    Links(Cmp, u64),
    Empty,
    Print,
    Print0,
    Prune,
    Delete,
    Quit,
    Exec {
        argv: Vec<OsString>,
        batch: Option<usize>,
    },
}

#[derive(Default)]
struct Opts {
    follow_all: bool,
    follow_roots: bool,
    maxdepth: Option<usize>,
    mindepth: usize,
    depth_first: bool,
    xdev: bool,
}

struct Parser<'a> {
    args: &'a [String],
    pos: usize,
    opts: Opts,
    has_action: bool,
    batches: usize,
    /// What `-prune` needs to know: -delete turns -depth on, which makes a
    /// prune do nothing.
    saw_prune: bool,
    saw_delete: bool,
    saw_depth: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    fn next(&mut self) -> Option<&str> {
        let a = self.args.get(self.pos).map(String::as_str);
        self.pos += 1;
        a
    }

    fn operand(&mut self, primary: &str) -> Result<String, String> {
        self.next()
            .map(str::to_string)
            .ok_or_else(|| format!("missing argument to '{primary}'"))
    }

    fn or(&mut self) -> Result<Node, String> {
        let mut left = self.and()?;
        while matches!(self.peek(), Some("-o" | "-or")) {
            self.pos += 1;
            let right = self.and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Node, String> {
        let mut left = self.not()?;
        loop {
            match self.peek() {
                None | Some("-o" | "-or" | ")") => return Ok(left),
                Some("-a" | "-and") => self.pos += 1,
                Some(_) => {}
            }
            let right = self.not()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
    }

    fn not(&mut self) -> Result<Node, String> {
        if matches!(self.peek(), Some("!" | "-not")) {
            self.pos += 1;
            return Ok(Node::Not(Box::new(self.not()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, String> {
        let Some(tok) = self.next().map(str::to_string) else {
            return Err("expected an expression".to_string());
        };
        let node = match tok.as_str() {
            "(" => {
                let inner = self.or()?;
                if self.next() != Some(")") {
                    return Err("missing ')'".to_string());
                }
                inner
            }
            "-true" => Node::Const(true),
            "-false" => Node::Const(false),
            "-name" | "-iname" => Node::Name(self.operand(&tok)?.into_bytes(), tok == "-iname"),
            "-path" | "-wholename" | "-ipath" => {
                Node::Path(self.operand(&tok)?.into_bytes(), tok == "-ipath")
            }
            "-type" => {
                let t = self.operand(&tok)?;
                match t.as_bytes() {
                    [c @ (b'f' | b'd' | b'l' | b'p' | b's' | b'c' | b'b')] => Node::Type(*c),
                    _ => return Err(format!("unknown argument to -type: {t}")),
                }
            }
            "-newer" => {
                let f = self.operand(&tok)?;
                // The reference is followed only under -H or -L, as GNU does.
                let m = if self.opts.follow_all || self.opts.follow_roots {
                    std::fs::metadata(&f)
                } else {
                    std::fs::symlink_metadata(&f)
                }
                .map_err(|e| format!("{f}: {e}"))?;
                Node::Newer(m.modified().map_err(|e| format!("{f}: {e}"))?)
            }
            "-mtime" => {
                let (cmp, n) = numeric(&self.operand(&tok)?)?;
                Node::Age {
                    cmp,
                    n,
                    unit_secs: 86_400,
                }
            }
            "-mmin" => {
                let (cmp, n) = numeric(&self.operand(&tok)?)?;
                Node::Minutes(cmp, n)
            }
            "-size" => {
                let spec = self.operand(&tok)?;
                const UNITS: &[(char, u64)] = &[
                    ('c', 1),
                    ('w', 2),
                    ('b', 512),
                    ('k', 1024),
                    ('M', 1024 * 1024),
                    ('G', 1024 * 1024 * 1024),
                ];
                let (num, unit) = UNITS
                    .iter()
                    .find_map(|(suffix, unit)| spec.strip_suffix(*suffix).map(|n| (n, *unit)))
                    .unwrap_or((spec.as_str(), 512));
                let (cmp, n) = numeric(num)?;
                Node::Size { cmp, n, unit }
            }
            "-perm" => {
                let spec = self.operand(&tok)?;
                let (kind, digits) = match spec.as_bytes().first() {
                    Some(b'-') => ('-', spec.get(1..).unwrap_or("")),
                    Some(b'/') => ('/', spec.get(1..).unwrap_or("")),
                    _ => ('=', spec.as_str()),
                };
                let mode = u32::from_str_radix(digits, 8)
                    .ok()
                    .filter(|m| *m <= 0o7777)
                    .ok_or_else(|| format!("invalid mode '{spec}' (octal only)"))?;
                Node::Perm(match kind {
                    '-' => Perm::All(mode),
                    '/' => Perm::Any(mode),
                    _ => Perm::Exact(mode),
                })
            }
            "-links" => {
                let (cmp, n) = numeric(&self.operand(&tok)?)?;
                Node::Links(cmp, n)
            }
            "-empty" => Node::Empty,
            "-print" => {
                self.has_action = true;
                Node::Print
            }
            "-print0" => {
                self.has_action = true;
                Node::Print0
            }
            "-prune" => {
                self.saw_prune = true;
                Node::Prune
            }
            "-quit" => Node::Quit,
            "-delete" => {
                self.has_action = true;
                self.saw_delete = true;
                self.opts.depth_first = true;
                Node::Delete
            }
            "-exec" => {
                self.has_action = true;
                let mut argv = Vec::new();
                let batch = loop {
                    let Some(a) = self.next().map(str::to_string) else {
                        return Err("missing argument to '-exec'".to_string());
                    };
                    if a == ";" {
                        break None;
                    }
                    if a == "+" && argv.last().is_some_and(|l: &OsString| l == "{}") {
                        argv.pop();
                        let id = self.batches;
                        self.batches += 1;
                        break Some(id);
                    }
                    argv.push(OsString::from(a));
                };
                if argv.is_empty() {
                    return Err("missing command for '-exec'".to_string());
                }
                Node::Exec { argv, batch }
            }
            "-maxdepth" | "-mindepth" => {
                let v = self.operand(&tok)?;
                let n: usize = v.parse().map_err(|_| format!("invalid {tok} '{v}'"))?;
                if tok == "-maxdepth" {
                    self.opts.maxdepth = Some(n);
                } else {
                    self.opts.mindepth = n;
                }
                Node::Const(true)
            }
            "-depth" => {
                self.saw_depth = true;
                self.opts.depth_first = true;
                Node::Const(true)
            }
            "-xdev" | "-mount" => {
                self.opts.xdev = true;
                Node::Const(true)
            }
            other => return Err(format!("unknown predicate '{other}'")),
        };
        Ok(node)
    }
}

/// `+N`, `-N` or `N`.
fn numeric(spec: &str) -> Result<(Cmp, u64), String> {
    let (cmp, digits) = match spec.as_bytes().first() {
        Some(b'+') => (Cmp::More, spec.get(1..).unwrap_or("")),
        Some(b'-') => (Cmp::Less, spec.get(1..).unwrap_or("")),
        _ => (Cmp::Exactly, spec),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid number '{spec}'"));
    }
    digits
        .parse()
        .map(|n| (cmp, n))
        .map_err(|e| format!("invalid number '{spec}': {e}"))
}

struct Walk<'a> {
    opts: &'a Opts,
    expr: &'a Node,
    now: SystemTime,
    out: std::io::BufWriter<std::io::StdoutLock<'static>>,
    status: u8,
    quit: bool,
    pruned: bool,
    /// Pending `-exec ... +` argument lists, by batch id, with their byte sizes.
    pending: Vec<(Vec<OsString>, usize)>,
    batch_argv: Vec<Vec<OsString>>,
    /// The directories being walked, outermost first, by (dev, ino): a
    /// followed link back to one of them is a loop, not more tree.
    ancestors: Vec<(u64, u64, PathBuf)>,
}

struct Entry<'p> {
    path: &'p Path,
    meta: Metadata,
}

impl Walk<'_> {
    fn complain(&mut self, what: &Path, e: &dyn std::fmt::Display) {
        let _ = self.out.flush();
        crate::emit_err(&format!("find: {}: {e}\n", what.display()));
        self.status = 1;
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.out.write_all(bytes) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                self.quit = true;
            } else {
                self.status = 1;
            }
        }
    }

    fn eval(&mut self, node: &Node, e: &Entry) -> bool {
        match node {
            Node::And(a, b) => self.eval(a, e) && !self.quit && self.eval(b, e),
            Node::Or(a, b) => self.eval(a, e) || (!self.quit && self.eval(b, e)),
            Node::Not(a) => !self.eval(a, e),
            Node::Const(v) => *v,
            Node::Name(pat, fold) => glob::matches(pat, base_name(e.path), *fold),
            Node::Path(pat, fold) => glob::matches(pat, e.path.as_os_str().as_bytes(), *fold),
            Node::Type(t) => file_type(&e.meta) == *t,
            Node::Newer(t) => e.meta.modified().is_ok_and(|m| m > *t),
            Node::Minutes(cmp, n) => {
                let m = e.meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                // Signed nanoseconds, so a file from the future is a
                // negative age rather than none.
                let age: i128 = match self.now.duration_since(m) {
                    Ok(d) => i128::try_from(d.as_nanos()).unwrap_or(i128::MAX),
                    Err(err) => -i128::try_from(err.duration().as_nanos()).unwrap_or(i128::MAX),
                };
                let unit = 60_000_000_000i128;
                let n = i128::from(*n);
                match cmp {
                    Cmp::Less => age < n * unit,
                    Cmp::More => age > n * unit,
                    Cmp::Exactly => age > (n - 1) * unit && age <= n * unit,
                }
            }
            Node::Age { cmp, n, unit_secs } => {
                let m = e.meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let age = self.now.duration_since(m).map_or(0, |d| d.as_secs());
                cmp.holds(age / unit_secs, *n)
            }
            Node::Size { cmp, n, unit } => {
                let units = e.meta.len().div_ceil(*unit);
                cmp.holds(units, *n)
            }
            Node::Perm(p) => {
                let mode = e.meta.permissions().mode() & 0o7777;
                match p {
                    Perm::Exact(m) => mode == *m,
                    Perm::All(m) => mode & m == *m,
                    Perm::Any(m) => *m == 0 || mode & m != 0,
                }
            }
            Node::Links(cmp, n) => cmp.holds(e.meta.nlink(), *n),
            Node::Empty => {
                if e.meta.is_dir() {
                    std::fs::read_dir(e.path).is_ok_and(|mut d| d.next().is_none())
                } else {
                    e.meta.is_file() && e.meta.len() == 0
                }
            }
            Node::Print => {
                let mut line = e.path.as_os_str().as_bytes().to_vec();
                line.push(b'\n');
                self.write(&line);
                true
            }
            Node::Print0 => {
                let mut line = e.path.as_os_str().as_bytes().to_vec();
                line.push(0);
                self.write(&line);
                true
            }
            Node::Prune => {
                self.pruned = true;
                true
            }
            Node::Quit => {
                self.quit = true;
                true
            }
            Node::Delete => {
                if e.path == Path::new(".") {
                    return true;
                }
                let r = if e.meta.is_dir() {
                    std::fs::remove_dir(e.path)
                } else {
                    std::fs::remove_file(e.path)
                };
                match r {
                    Ok(()) => true,
                    Err(err) => {
                        self.complain(e.path, &err);
                        false
                    }
                }
            }
            Node::Exec { argv, batch: None } => {
                let path = e.path.as_os_str();
                let args: Vec<OsString> = argv.iter().map(|a| substitute(a, path)).collect();
                self.run(&args) == Some(0)
            }
            Node::Exec {
                argv,
                batch: Some(id),
            } => {
                let id = *id;
                if self.batch_argv.len() <= id {
                    self.batch_argv.resize(id + 1, Vec::new());
                    self.pending.resize(id + 1, (Vec::new(), 0));
                }
                if let Some(slot) = self.batch_argv.get_mut(id) {
                    if slot.is_empty() {
                        slot.clone_from(argv);
                    }
                }
                let size = e.path.as_os_str().len() + 1;
                let full = self
                    .pending
                    .get(id)
                    .is_some_and(|(_, n)| n + size > EXEC_BATCH_BYTES);
                if full {
                    self.flush_batch(id);
                }
                if let Some((paths, n)) = self.pending.get_mut(id) {
                    paths.push(e.path.as_os_str().to_os_string());
                    *n += size;
                }
                true
            }
        }
    }

    fn flush_batch(&mut self, id: usize) {
        let paths = match self.pending.get_mut(id) {
            Some((paths, n)) if !paths.is_empty() => {
                *n = 0;
                std::mem::take(paths)
            }
            _ => return,
        };
        let mut args = self.batch_argv.get(id).cloned().unwrap_or_default();
        args.extend(paths);
        if self.run(&args) != Some(0) {
            self.status = 1;
        }
    }

    fn run(&mut self, args: &[OsString]) -> Option<i32> {
        let (prog, rest) = args.split_first()?;
        let _ = self.out.flush();
        match Command::new(prog).args(rest).status() {
            Ok(s) => s.code(),
            Err(err) => {
                crate::emit_err(&format!("find: {}: {err}\n", prog.to_string_lossy()));
                self.status = 1;
                None
            }
        }
    }

    fn visit(&mut self, path: &Path, depth: usize, root_dev: u64) {
        if self.quit {
            return;
        }
        let follow = self.opts.follow_all || (depth == 0 && self.opts.follow_roots);
        let meta = if follow {
            std::fs::metadata(path).or_else(|_| std::fs::symlink_metadata(path))
        } else {
            std::fs::symlink_metadata(path)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(err) => {
                self.complain(path, &err);
                return;
            }
        };
        let is_dir = meta.is_dir();
        let id = (meta.dev(), meta.ino());
        if is_dir {
            if let Some((_, _, first)) = self.ancestors.iter().find(|(d, i, _)| (*d, *i) == id) {
                let msg = format!(
                    "File system loop detected; '{}' is part of the same file system loop as '{}'.",
                    path.display(),
                    first.display()
                );
                let _ = self.out.flush();
                crate::emit_err(&format!("find: {msg}\n"));
                self.status = 1;
                return;
            }
        }
        let in_range = depth >= self.opts.mindepth;
        let entry = Entry { path, meta };
        self.pruned = false;
        if in_range && !self.opts.depth_first {
            let expr = self.expr;
            self.eval(expr, &entry);
        }
        let descend = is_dir
            && !self.pruned
            && self.opts.maxdepth.is_none_or(|m| depth < m)
            && !(self.opts.xdev && depth > 0 && entry.meta.dev() != root_dev);
        if descend && !self.quit {
            self.ancestors.push((id.0, id.1, path.to_path_buf()));
            match std::fs::read_dir(path) {
                Ok(rd) => {
                    let mut names: Vec<OsString> = Vec::new();
                    for d in rd {
                        match d {
                            Ok(d) => names.push(d.file_name()),
                            Err(err) => self.complain(path, &err),
                        }
                    }
                    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                    for name in names {
                        let child = join(path, &name);
                        self.visit(&child, depth + 1, root_dev);
                        if self.quit {
                            break;
                        }
                    }
                }
                Err(err) => self.complain(path, &err),
            }
            self.ancestors.pop();
        }
        if in_range && self.opts.depth_first && !self.quit {
            let expr = self.expr;
            self.eval(expr, &entry);
        }
    }
}

/// `{}` anywhere in an `-exec ... ;` argument becomes the path, as GNU does.
fn substitute(arg: &OsStr, path: &OsStr) -> OsString {
    let a = arg.as_bytes();
    if !a.windows(2).any(|w| w == b"{}") {
        return arg.to_os_string();
    }
    let mut out = Vec::with_capacity(a.len() + path.len());
    let mut i = 0;
    while i < a.len() {
        if a.get(i..i + 2) == Some(b"{}") {
            out.extend_from_slice(path.as_bytes());
            i += 2;
        } else if let Some(&b) = a.get(i) {
            out.push(b);
            i += 1;
        }
    }
    OsString::from_vec(out)
}

/// `root` + `/` + `name`, without doubling a slash the operand already ends in.
fn join(root: &Path, name: &OsStr) -> PathBuf {
    let mut bytes = root.as_os_str().as_bytes().to_vec();
    if bytes.last() != Some(&b'/') {
        bytes.push(b'/');
    }
    bytes.extend_from_slice(name.as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

/// The last component, ignoring trailing slashes (`find dir/ -name dir`).
fn base_name(path: &Path) -> &[u8] {
    let mut b = path.as_os_str().as_bytes();
    while b.len() > 1 && b.last() == Some(&b'/') {
        b = b.get(..b.len() - 1).unwrap_or(b);
    }
    match b.iter().rposition(|&c| c == b'/') {
        Some(i) if b.len() > 1 => b.get(i + 1..).unwrap_or(b),
        _ => b,
    }
}

fn file_type(m: &Metadata) -> u8 {
    use std::os::unix::fs::FileTypeExt;
    let t = m.file_type();
    if t.is_symlink() {
        b'l'
    } else if t.is_dir() {
        b'd'
    } else if t.is_fifo() {
        b'p'
    } else if t.is_socket() {
        b's'
    } else if t.is_char_device() {
        b'c'
    } else if t.is_block_device() {
        b'b'
    } else {
        b'f'
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut i = 0;
    let mut opts = Opts::default();
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-L" => opts.follow_all = true,
            "-H" => opts.follow_roots = true,
            "-P" => {
                opts.follow_all = false;
                opts.follow_roots = false;
            }
            _ => break,
        }
        i += 1;
    }
    let mut roots = Vec::new();
    while let Some(a) = args.get(i) {
        if (a.starts_with('-') && a.len() > 1) || a == "(" || a == "!" {
            break;
        }
        roots.push(PathBuf::from(a));
        i += 1;
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    let now = SystemTime::now();
    let rest = args.get(i..).unwrap_or(&[]);
    let mut p = Parser {
        args: rest,
        pos: 0,
        opts,
        has_action: false,
        batches: 0,
        saw_prune: false,
        saw_delete: false,
        saw_depth: false,
    };
    let expr = if rest.is_empty() {
        Node::Const(true)
    } else {
        let e = p.or()?;
        if let Some(extra) = p.peek() {
            return Err(format!("unexpected '{extra}'"));
        }
        e
    };
    if p.saw_prune && p.saw_delete && !p.saw_depth {
        return Err(
            "The -delete action automatically turns on -depth, but -prune does \
                    nothing when -depth is in effect. If you want to carry on anyway, just \
                    explicitly use the -depth option."
                .to_string(),
        );
    }
    let expr = if p.has_action {
        expr
    } else {
        Node::And(Box::new(expr), Box::new(Node::Print))
    };
    let opts = p.opts;
    let mut w = Walk {
        opts: &opts,
        expr: &expr,
        now,
        out: std::io::BufWriter::new(std::io::stdout().lock()),
        status: 0,
        quit: false,
        pruned: false,
        pending: Vec::new(),
        batch_argv: Vec::new(),
        ancestors: Vec::new(),
    };
    for root in &roots {
        let dev = std::fs::metadata(root).map_or(0, |m| m.dev());
        w.visit(root, 0, dev);
        if w.quit {
            break;
        }
    }
    for id in 0..w.pending.len() {
        w.flush_batch(id);
    }
    if let Err(e) = w.out.flush() {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(e.to_string());
        }
    }
    Ok(w.status)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn numeric_specs() {
        assert_eq!(numeric("+3"), Ok((Cmp::More, 3)));
        assert_eq!(numeric("-3"), Ok((Cmp::Less, 3)));
        assert_eq!(numeric("3"), Ok((Cmp::Exactly, 3)));
        assert!(numeric("x").is_err());
        assert!(numeric("+").is_err());
        assert_eq!(base_name(Path::new("a/b/")), b"b");
        assert_eq!(base_name(Path::new("/")), b"/");
        assert_eq!(substitute(OsStr::new("x{}y{}"), OsStr::new("P")), "xPyP");
    }
}
