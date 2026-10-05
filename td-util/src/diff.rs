//! `diff` — line comparison in the normal or unified (`-u`/`-U N`) format,
//! `-q` for the status line only, `-r` over directories and `-N` to treat an
//! absent file as empty. Myers' linear-space algorithm, so memory stays
//! proportional to the inputs however many lines differ. 0 same, 1 different,
//! 2 trouble.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::cmp::read_all;

const USAGE: &str = "usage: diff [-q] [-r] [-N] [-u|-U N] FILE1 FILE2";

struct Opts {
    brief: bool,
    recursive: bool,
    new_file: bool,
    context: Option<usize>,
    /// The options as given, which GNU repeats in the `diff ...` line it
    /// puts before each differing pair found in a directory.
    flags: Vec<String>,
}

pub fn run(args: &[String]) -> Result<u8, String> {
    match start(args) {
        Ok(code) => Ok(code),
        Err(msg) => {
            crate::emit_err(&format!("diff: {msg}\n"));
            Ok(2)
        }
    }
}

fn start(args: &[String]) -> Result<u8, String> {
    let mut o = Opts {
        brief: false,
        recursive: false,
        new_file: false,
        context: None,
        flags: Vec::new(),
    };
    let mut files: Vec<&str> = Vec::new();
    let mut i = 0;
    let mut opts_done = false;
    while let Some(a) = args.get(i) {
        let s = a.as_str();
        if opts_done || !s.starts_with('-') || s == "-" {
            files.push(s);
        } else if s == "--" {
            opts_done = true;
        } else if s == "-U" {
            i += 1;
            let n = args.get(i).ok_or(USAGE)?;
            o.flags.push(format!("-U {n}"));
            o.context = Some(
                n.parse()
                    .map_err(|_| format!("invalid context length '{n}'"))?,
            );
        } else if let Some(n) = s.strip_prefix("-U") {
            o.flags.push(s.to_string());
            o.context = Some(
                n.parse()
                    .map_err(|_| format!("invalid context length '{n}'"))?,
            );
        } else {
            o.flags.push(s.to_string());
            for c in s.chars().skip(1) {
                match c {
                    'q' => o.brief = true,
                    'r' => o.recursive = true,
                    'N' => o.new_file = true,
                    'u' => o.context = Some(o.context.unwrap_or(3)),
                    _ => return Err(format!("unrecognised option '-{c}'\n{USAGE}")),
                }
            }
        }
        i += 1;
    }
    let [a, b] = files.as_slice() else {
        return Err(USAGE.to_string());
    };
    let (pa, pb) = (Path::new(a), Path::new(b));
    match (pa.is_dir(), pb.is_dir()) {
        (true, true) => dirs(pa, pb, &o),
        // `diff dir file` compares dir/<file's name> with file, and back.
        (true, false) => files_pair(&pa.join(base(pb)?), pb, &o, false),
        (false, true) => files_pair(pa, &pb.join(base(pa)?), &o, false),
        (false, false) => files_pair(pa, pb, &o, false),
    }
}

fn base(p: &Path) -> Result<&std::ffi::OsStr, String> {
    p.file_name()
        .ok_or_else(|| format!("{}: no file name", p.display()))
}

/// Two directories, entry by entry in byte order. A pair that cannot be read
/// is reported and the walk goes on, as GNU's does, with status 2.
fn dirs(a: &Path, b: &Path, o: &Opts) -> Result<u8, String> {
    let names = |d: &Path| -> Result<Vec<std::ffi::OsString>, String> {
        // `-N` reads a directory missing on one side as an empty one.
        if o.new_file && !d.exists() {
            return Ok(Vec::new());
        }
        let mut v: Vec<_> = std::fs::read_dir(d)
            .map_err(|e| format!("{}: {e}", d.display()))?
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        v.sort();
        Ok(v)
    };
    let (na, nb) = (names(a)?, names(b)?);
    let mut all: Vec<_> = na.iter().chain(nb.iter()).cloned().collect();
    all.sort();
    all.dedup();
    let mut status = 0u8;
    for n in all {
        let (ca, cb) = (a.join(&n), b.join(&n));
        let (ea, eb) = (na.contains(&n), nb.contains(&n));
        let code = if !ea || !eb {
            let dir = if ea { ca.is_dir() } else { cb.is_dir() };
            if o.new_file && dir && o.recursive {
                go(dirs(&ca, &cb, o))
            } else if o.new_file && !dir {
                go(files_pair(&ca, &cb, o, true))
            } else {
                let (dir, _) = if ea { (a, &ca) } else { (b, &cb) };
                crate::emit(&format!(
                    "Only in {}: {}\n",
                    dir.display(),
                    n.to_string_lossy()
                ))?;
                1
            }
        } else {
            match (ca.is_dir(), cb.is_dir()) {
                (true, true) if o.recursive => go(dirs(&ca, &cb, o)),
                (true, true) => {
                    crate::emit(&format!(
                        "Common subdirectories: {} and {}\n",
                        ca.display(),
                        cb.display()
                    ))?;
                    0
                }
                (false, false) => go(files_pair(&ca, &cb, o, true)),
                _ => {
                    crate::emit(&format!(
                        "File {} is a {} while file {} is a {}\n",
                        ca.display(),
                        kind(&ca),
                        cb.display(),
                        kind(&cb)
                    ))?;
                    1
                }
            }
        };
        status = status.max(code);
    }
    Ok(status)
}

/// A nested comparison's status, or 2 after reporting why it had none.
fn go(r: Result<u8, String>) -> u8 {
    r.unwrap_or_else(|e| {
        crate::emit_err(&format!("diff: {e}\n"));
        2
    })
}

fn kind(p: &Path) -> &'static str {
    if p.is_dir() {
        "directory"
    } else {
        "regular file"
    }
}

/// `in_dir`: found by walking directories, so a differing pair is headed by
/// the `diff FLAGS A B` line GNU writes there.
fn files_pair(a: &Path, b: &Path, o: &Opts, in_dir: bool) -> Result<u8, String> {
    let load = |p: &Path| -> Result<Vec<u8>, String> {
        let s = p.to_string_lossy();
        if o.new_file && s != "-" && !p.exists() {
            return Ok(Vec::new());
        }
        read_all(&s)
    };
    let (da, db) = (load(a)?, load(b)?);
    if da == db {
        return Ok(0);
    }
    let (sa, sb) = (a.display().to_string(), b.display().to_string());
    let (ra, rb) = (a.as_os_str().as_bytes(), b.as_os_str().as_bytes());
    if o.brief {
        crate::emit(&format!("Files {sa} and {sb} differ\n"))?;
        return Ok(1);
    }
    let binary = |d: &[u8]| d.iter().take(8192).any(|&c| c == 0);
    if binary(&da) || binary(&db) {
        crate::emit(&format!("Binary files {sa} and {sb} differ\n"))?;
        return Ok(1);
    }
    let la = lines(&da);
    let lb = lines(&db);
    let ops = script(&la, &lb);
    let mut out = Vec::new();
    if in_dir {
        out.extend_from_slice(b"diff ");
        for f in &o.flags {
            out.extend_from_slice(f.as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(ra);
        out.push(b' ');
        out.extend_from_slice(rb);
        out.push(b'\n');
    }
    match o.context {
        Some(n) => {
            for (mark, path) in [(&b"--- "[..], ra), (&b"+++ "[..], rb)] {
                out.extend_from_slice(mark);
                out.extend_from_slice(path);
                out.push(b'\n');
            }
            unified(&la, &lb, &ops, n, &mut out);
        }
        None => normal(&la, &lb, &ops, &mut out),
    }
    crate::emit_bytes(&out)?;
    Ok(1)
}

/// Lines with their terminators; a final line without one is kept as it is.
fn lines(d: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, &c) in d.iter().enumerate() {
        if c == b'\n' {
            v.push(d.get(start..=i).unwrap_or(&[]));
            start = i + 1;
        }
    }
    if start < d.len() {
        v.push(d.get(start..).unwrap_or(&[]));
    }
    v
}

/// One step of the edit script: keep a[i]==b[j], delete a[i], insert b[j].
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Keep,
    Del,
    Ins,
}

/// The shortest edit script from `a` to `b`.
fn script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
    let mut matched = vec![false; a.len()];
    let mut pair = vec![usize::MAX; a.len()];
    // Each distinct line once, as an integer the search compares instead.
    let mut ids: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
    let mut ia: Vec<u32> = Vec::with_capacity(a.len());
    let mut ib: Vec<u32> = Vec::with_capacity(b.len());
    for (lines, out) in [(a, &mut ia), (b, &mut ib)] {
        for &line in lines {
            let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
            out.push(*ids.entry(line).or_insert(next));
        }
    }
    // The common ends first, matched as they stand, so the filter below
    // cannot pair a line inside the change with its twin outside it.
    let head = ia.iter().zip(&ib).take_while(|(x, y)| x == y).count();
    let tail = ia
        .get(head..)
        .unwrap_or(&[])
        .iter()
        .rev()
        .zip(ib.get(head..).unwrap_or(&[]).iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    for i in 0..head {
        if let (Some(m), Some(p)) = (matched.get_mut(i), pair.get_mut(i)) {
            *m = true;
            *p = i;
        }
    }
    for k in 1..=tail {
        let (i, j) = (a.len() - k, b.len() - k);
        if let (Some(m), Some(p)) = (matched.get_mut(i), pair.get_mut(i)) {
            *m = true;
            *p = j;
        }
    }
    let ma = ia.get(head..ia.len() - tail).unwrap_or(&[]);
    let mb = ib.get(head..ib.len() - tail).unwrap_or(&[]);
    // A line absent from the other side is in no common subsequence, so the
    // LCS of what is left is the LCS of the whole, and the edit stays minimal.
    // Two files with nothing in common then cost nothing to compare.
    let mut seen = vec![0u8; ids.len()];
    for &x in ma {
        if let Some(f) = seen.get_mut(x as usize) {
            *f |= 1;
        }
    }
    for &y in mb {
        if let Some(f) = seen.get_mut(y as usize) {
            *f |= 2;
        }
    }
    let both = |x: &u32| seen.get(*x as usize) == Some(&3);
    let ka: Vec<usize> = (0..ma.len())
        .filter(|&i| ma.get(i).is_some_and(both))
        .collect();
    let kb: Vec<usize> = (0..mb.len())
        .filter(|&j| mb.get(j).is_some_and(both))
        .collect();
    let fa: Vec<u32> = ka.iter().filter_map(|&i| ma.get(i).copied()).collect();
    let fb: Vec<u32> = kb.iter().filter_map(|&j| mb.get(j).copied()).collect();
    let mut fmatched = vec![false; fa.len()];
    let mut fpair = vec![usize::MAX; fa.len()];
    lcs(
        &fa,
        &fb,
        0,
        fa.len(),
        0,
        fb.len(),
        &mut fmatched,
        &mut fpair,
    );
    for (k, &i) in ka.iter().enumerate() {
        if fmatched.get(k) == Some(&true) {
            let j = fpair.get(k).and_then(|&fj| kb.get(fj)).copied();
            if let (Some(m), Some(p), Some(j)) =
                (matched.get_mut(head + i), pair.get_mut(head + i), j)
            {
                *m = true;
                *p = head + j;
            }
        }
    }
    let mut ops = Vec::with_capacity(a.len() + b.len());
    let mut j = 0;
    for (i, &m) in matched.iter().enumerate() {
        if m {
            let pj = pair.get(i).copied().unwrap_or(j);
            while j < pj {
                ops.push(Op::Ins);
                j += 1;
            }
            ops.push(Op::Keep);
            j += 1;
        } else {
            ops.push(Op::Del);
        }
    }
    while j < b.len() {
        ops.push(Op::Ins);
        j += 1;
    }
    // Present each change as its deletions then its insertions.
    let mut k = 0;
    while k < ops.len() {
        let end = ops
            .get(k..)
            .and_then(|r| r.iter().position(|&o| o == Op::Keep))
            .map_or(ops.len(), |p| k + p);
        if let Some(run) = ops.get_mut(k..end) {
            run.sort_by_key(|&o| o == Op::Ins);
        }
        k = end + 1;
    }
    ops
}

/// Myers' divide and conquer: mark the lines of `a[a0..a1]` that belong to a
/// longest common subsequence with `b[b0..b1]`, and the `b` line each pairs with.
#[allow(clippy::too_many_arguments)]
fn lcs<T: PartialEq>(
    a: &[T],
    b: &[T],
    mut a0: usize,
    mut a1: usize,
    mut b0: usize,
    mut b1: usize,
    matched: &mut [bool],
    pair: &mut [usize],
) {
    let mark = |i: usize, j: usize, matched: &mut [bool], pair: &mut [usize]| {
        if let Some(m) = matched.get_mut(i) {
            *m = true;
        }
        if let Some(p) = pair.get_mut(i) {
            *p = j;
        }
    };
    while a0 < a1 && b0 < b1 && a.get(a0) == b.get(b0) {
        mark(a0, b0, matched, pair);
        a0 += 1;
        b0 += 1;
    }
    while a0 < a1 && b0 < b1 && a.get(a1 - 1) == b.get(b1 - 1) {
        mark(a1 - 1, b1 - 1, matched, pair);
        a1 -= 1;
        b1 -= 1;
    }
    if a0 == a1 || b0 == b1 {
        return;
    }
    // No snake is only possible for an empty range, excluded above; treating
    // the range as unmatched is still a valid, if longer, script.
    let Some((x, y, u, v)) = middle_snake(a, b, a0, a1, b0, b1) else {
        return;
    };
    for k in 0..(u - x) {
        mark(x + k, y + k, matched, pair);
    }
    lcs(a, b, a0, x, b0, y, matched, pair);
    lcs(a, b, u, a1, v, b1, matched, pair);
}

/// The middle snake of the edit graph for `a[a0..a1]` x `b[b0..b1]`: a
/// diagonal run (x,y)..(u,v) on some shortest path. Both ranges are nonempty
/// and their first and last lines differ.
fn middle_snake<T: PartialEq>(
    a: &[T],
    b: &[T],
    a0: usize,
    a1: usize,
    b0: usize,
    b1: usize,
) -> Option<(usize, usize, usize, usize)> {
    let n = (a1 - a0) as isize;
    let m = (b1 - b0) as isize;
    let delta = n - m;
    let odd = delta & 1 != 0;
    let max = ((n + m + 1) / 2) as usize + 1;
    let off = max as isize;
    let size = 2 * max + 2;
    let mut vf = vec![0isize; size];
    let mut vb = vec![0isize; size];
    let at = |v: &[isize], k: isize| v.get((k + off) as usize).copied().unwrap_or(0);
    let eq = |x: isize, y: isize| a.get(a0 + x as usize) == b.get(b0 + y as usize);
    for d in 0..=(max as isize) {
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && at(&vf, k - 1) < at(&vf, k + 1)) {
                at(&vf, k + 1)
            } else {
                at(&vf, k - 1) + 1
            };
            let mut y = x - k;
            let (sx, sy) = (x, y);
            while x < n && y < m && eq(x, y) {
                x += 1;
                y += 1;
            }
            if let Some(s) = vf.get_mut((k + off) as usize) {
                *s = x;
            }
            let kb = delta - k;
            if odd && (-(d - 1)..=(d - 1)).contains(&kb) && x + at(&vb, kb) >= n {
                return Some((
                    a0 + sx as usize,
                    b0 + sy as usize,
                    a0 + x as usize,
                    b0 + y as usize,
                ));
            }
            k += 2;
        }
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && at(&vb, k - 1) < at(&vb, k + 1)) {
                at(&vb, k + 1)
            } else {
                at(&vb, k - 1) + 1
            };
            let mut y = x - k;
            let (ex, ey) = (x, y);
            while x < n && y < m && eq(n - x - 1, m - y - 1) {
                x += 1;
                y += 1;
            }
            if let Some(s) = vb.get_mut((k + off) as usize) {
                *s = x;
            }
            let kf = delta - k;
            if !odd && (-d..=d).contains(&kf) && x + at(&vf, kf) >= n {
                return Some((
                    a0 + (n - x) as usize,
                    b0 + (m - y) as usize,
                    a0 + (n - ex) as usize,
                    b0 + (m - ey) as usize,
                ));
            }
            k += 2;
        }
    }
    None
}

/// A maximal change: lines `a[i0..i1]` replaced by `b[j0..j1]`.
struct Change {
    i0: usize,
    i1: usize,
    j0: usize,
    j1: usize,
}

fn changes(ops: &[Op]) -> Vec<Change> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    let mut k = 0;
    while k < ops.len() {
        if ops.get(k) == Some(&Op::Keep) {
            i += 1;
            j += 1;
            k += 1;
            continue;
        }
        let (i0, j0) = (i, j);
        while let Some(op) = ops.get(k).filter(|&&o| o != Op::Keep) {
            if *op == Op::Del {
                i += 1;
            } else {
                j += 1;
            }
            k += 1;
        }
        out.push(Change {
            i0,
            i1: i,
            j0,
            j1: j,
        });
    }
    out
}

fn push_line(out: &mut Vec<u8>, prefix: &[u8], line: &[u8]) {
    out.extend_from_slice(prefix);
    out.extend_from_slice(line);
    if line.last() != Some(&b'\n') {
        out.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

fn range(lo: usize, hi: usize) -> String {
    if hi - lo <= 1 {
        format!("{}", lo + 1)
    } else {
        format!("{},{}", lo + 1, hi)
    }
}

fn normal(a: &[&[u8]], b: &[&[u8]], ops: &[Op], out: &mut Vec<u8>) {
    for c in changes(ops) {
        let head = match (c.i0 == c.i1, c.j0 == c.j1) {
            (true, _) => format!("{}a{}\n", c.i0, range(c.j0, c.j1)),
            (_, true) => format!("{}d{}\n", range(c.i0, c.i1), c.j0),
            _ => format!("{}c{}\n", range(c.i0, c.i1), range(c.j0, c.j1)),
        };
        out.extend_from_slice(head.as_bytes());
        for l in a.get(c.i0..c.i1).unwrap_or(&[]) {
            push_line(out, b"< ", l);
        }
        if c.i0 != c.i1 && c.j0 != c.j1 {
            out.extend_from_slice(b"---\n");
        }
        for l in b.get(c.j0..c.j1).unwrap_or(&[]) {
            push_line(out, b"> ", l);
        }
    }
}

fn unified(a: &[&[u8]], b: &[&[u8]], ops: &[Op], ctx: usize, out: &mut Vec<u8>) {
    let all = changes(ops);
    let mut g = 0;
    while let Some(first) = all.get(g) {
        // Group changes whose context windows touch.
        let mut last = g;
        while let Some(next) = all.get(last + 1) {
            let prev = all.get(last).map_or(0, |c| c.i1);
            if next.i0 - prev > 2 * ctx {
                break;
            }
            last += 1;
        }
        let Some(end) = all.get(last) else { break };
        let lo_a = first.i0.saturating_sub(ctx);
        let lo_b = first.j0 - (first.i0 - lo_a);
        let hi_a = (end.i1 + ctx).min(a.len());
        let hi_b = end.j1 + (hi_a - end.i1);
        let span = |lo: usize, hi: usize| {
            let len = hi - lo;
            match len {
                0 => format!("{lo},0"),
                1 => format!("{}", lo + 1),
                _ => format!("{},{len}", lo + 1),
            }
        };
        out.extend_from_slice(
            format!("@@ -{} +{} @@\n", span(lo_a, hi_a), span(lo_b, hi_b)).as_bytes(),
        );
        let mut i = lo_a;
        for c in all.get(g..=last).unwrap_or(&[]) {
            for l in a.get(i..c.i0).unwrap_or(&[]) {
                push_line(out, b" ", l);
            }
            for l in a.get(c.i0..c.i1).unwrap_or(&[]) {
                push_line(out, b"-", l);
            }
            for l in b.get(c.j0..c.j1).unwrap_or(&[]) {
                push_line(out, b"+", l);
            }
            i = c.i1;
        }
        for l in a.get(i..hi_a).unwrap_or(&[]) {
            push_line(out, b" ", l);
        }
        g = last + 1;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn d(a: &str, b: &str, ctx: Option<usize>) -> String {
        let (la, lb) = (lines(a.as_bytes()), lines(b.as_bytes()));
        let ops = script(&la, &lb);
        let mut out = Vec::new();
        match ctx {
            Some(n) => unified(&la, &lb, &ops, n, &mut out),
            None => normal(&la, &lb, &ops, &mut out),
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn normal_format_add_delete_change() {
        assert_eq!(d("a\nb\nc\n", "a\nc\n", None), "2d1\n< b\n");
        assert_eq!(d("a\nc\n", "a\nb\nc\n", None), "1a2\n> b\n");
        assert_eq!(
            d("a\nb\nc\n", "a\nX\nY\nc\n", None),
            "2c2,3\n< b\n---\n> X\n> Y\n"
        );
        assert_eq!(d("", "x\n", None), "0a1\n> x\n");
        assert_eq!(
            d("x", "x\n", None),
            "1c1\n< x\n\\ No newline at end of file\n---\n> x\n"
        );
    }

    #[test]
    fn unified_format_groups_by_context() {
        let a = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let b = "1\n2\n3\nX\n5\n6\n7\n8\n9\n10\n";
        assert_eq!(d(a, b, Some(1)), "@@ -3,3 +3,3 @@\n 3\n-4\n+X\n 5\n");
        let b2 = "0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        assert_eq!(
            d(a, b2, Some(1)),
            "@@ -1 +1,2 @@\n+0\n 1\n@@ -9,2 +10 @@\n 9\n-10\n"
        );
        assert_eq!(d("", "", Some(3)), "");
    }

    #[test]
    fn the_script_is_minimal_and_reproduces_b() {
        let cases = [
            ("abcabba", "cbabac"),
            ("xaxbxcx", "abc"),
            ("aaaa", "aa"),
            ("abcdefghij", "jihgfedcba"),
            ("", "abc"),
            ("abc", ""),
        ];
        for (sa, sb) in cases {
            let a: Vec<String> = sa.chars().map(|c| format!("{c}\n")).collect();
            let b: Vec<String> = sb.chars().map(|c| format!("{c}\n")).collect();
            let la: Vec<&[u8]> = a.iter().map(|s| s.as_bytes()).collect();
            let lb: Vec<&[u8]> = b.iter().map(|s| s.as_bytes()).collect();
            let ops = script(&la, &lb);
            let (mut i, mut j) = (0, 0);
            let mut rebuilt: Vec<&[u8]> = Vec::new();
            for op in &ops {
                match op {
                    Op::Keep => {
                        assert_eq!(la[i], lb[j]);
                        rebuilt.push(la[i]);
                        i += 1;
                        j += 1;
                    }
                    Op::Del => i += 1,
                    Op::Ins => {
                        rebuilt.push(lb[j]);
                        j += 1;
                    }
                }
            }
            assert_eq!(rebuilt, lb, "{sa} -> {sb}");
            // A brute-force LCS length bounds the number of keeps from below.
            let keeps = ops.iter().filter(|o| **o == Op::Keep).count();
            assert_eq!(
                keeps,
                brute_lcs(sa.as_bytes(), sb.as_bytes()),
                "{sa} -> {sb}"
            );
        }
    }

    fn brute_lcs(a: &[u8], b: &[u8]) -> usize {
        let mut t = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                t[i][j] = if a[i - 1] == b[j - 1] {
                    t[i - 1][j - 1] + 1
                } else {
                    t[i - 1][j].max(t[i][j - 1])
                };
            }
        }
        t[a.len()][b.len()]
    }
}
