//! `xargs` — POSIX input splitting (blanks, quotes, backslash) plus GNU's
//! `-0`, `-d`, `-r`, `-I`, `-n`, `-s` and `-t`. Exit status follows POSIX:
//! 123 when an invocation failed, 124 when one exited 255 (which stops the
//! run), 125 when one was killed, 126/127 when the command could not run.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

/// The default per-invocation argument budget, far under any ARG_MAX.
const DEFAULT_SIZE: usize = 128 * 1024;

enum Split {
    Posix,
    Delim(u8),
}

struct Opts {
    split: Split,
    max_args: Option<usize>,
    replace: Option<Vec<u8>>,
    no_run_if_empty: bool,
    trace: bool,
    size: usize,
}

fn usage() -> String {
    "usage: xargs [-0] [-d C] [-r] [-t] [-n N] [-s SIZE] [-I REPLACE] [COMMAND [ARG...]]"
        .to_string()
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut o = Opts {
        split: Split::Posix,
        max_args: None,
        replace: None,
        no_run_if_empty: false,
        trace: false,
        size: DEFAULT_SIZE,
    };
    let mut i = 0;
    while let Some(a) = args.get(i) {
        let value = |i: &mut usize, inline: &str| -> Result<String, String> {
            if !inline.is_empty() {
                return Ok(inline.to_string());
            }
            *i += 1;
            args.get(*i).cloned().ok_or_else(usage)
        };
        match a.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--null" => o.split = Split::Delim(0),
            "--no-run-if-empty" => o.no_run_if_empty = true,
            "--verbose" => o.trace = true,
            // Short flags bundle (`-0r`, `-tn1`); one taking a value takes
            // the rest of the bundle, or the next argument.
            s if s.starts_with('-') && s.len() > 1 && !s.starts_with("--") => {
                for (at, c) in s.char_indices().skip(1) {
                    match c {
                        '0' => o.split = Split::Delim(0),
                        'r' => o.no_run_if_empty = true,
                        't' => o.trace = true,
                        'd' | 'n' | 's' | 'I' => {
                            let v = value(&mut i, s.get(at + 1..).unwrap_or(""))?;
                            set(&mut o, c, v)?;
                            break;
                        }
                        _ => return Err(format!("unrecognised option '-{c}'\n{}", usage())),
                    }
                }
            }
            s if s.starts_with("--") => {
                return Err(format!("unrecognised option '{s}'\n{}", usage()))
            }
            _ => break,
        }
        i += 1;
    }
    let mut cmd: Vec<OsString> = args
        .get(i..)
        .unwrap_or(&[])
        .iter()
        .map(OsString::from)
        .collect();
    if cmd.is_empty() {
        cmd.push(OsString::from("echo"));
    }
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|e| format!("read stdin: {e}"))?;

    if let Some(repl) = &o.replace {
        let mut status = 0u8;
        for line in lines(&input, &o.split)? {
            let argv: Vec<OsString> = cmd
                .iter()
                .map(|a| OsString::from_vec(replace(a.as_bytes(), repl, &line)))
                .collect();
            match invoke(&argv, o.trace) {
                Outcome::Ok => {}
                Outcome::Failed => status = 123,
                Outcome::Stop(code) => return Ok(code),
            }
        }
        return Ok(status);
    }

    let items = split(&input, &o.split)?;
    if items.is_empty() {
        if o.no_run_if_empty {
            return Ok(0);
        }
        return Ok(match invoke(&cmd, o.trace) {
            Outcome::Ok => 0,
            Outcome::Failed => 123,
            Outcome::Stop(code) => code,
        });
    }
    let base: usize = cmd.iter().map(|a| a.len() + 1).sum();
    let mut status = 0u8;
    let mut batch: Vec<OsString> = Vec::new();
    let mut used = base;
    let mut flush = |batch: &mut Vec<OsString>, used: &mut usize| -> Option<u8> {
        let mut argv = cmd.clone();
        argv.append(batch);
        *used = base;
        match invoke(&argv, o.trace) {
            Outcome::Ok => None,
            Outcome::Failed => {
                status = 123;
                None
            }
            Outcome::Stop(code) => Some(code),
        }
    };
    for item in items {
        let len = item.len() + 1;
        if base + len > o.size {
            return Err("argument line too long".to_string());
        }
        let full = !batch.is_empty()
            && (used + len > o.size || o.max_args.is_some_and(|n| batch.len() >= n));
        if full {
            if let Some(code) = flush(&mut batch, &mut used) {
                return Ok(code);
            }
        }
        used += len;
        batch.push(OsString::from_vec(item));
    }
    if !batch.is_empty() {
        if let Some(code) = flush(&mut batch, &mut used) {
            return Ok(code);
        }
    }
    Ok(status)
}

/// A value-taking flag's value.
fn set(o: &mut Opts, flag: char, v: String) -> Result<(), String> {
    let count = |v: &str| v.parse().ok().filter(|n: &usize| *n > 0);
    match flag {
        'd' => o.split = Split::Delim(delimiter(&v)?),
        'n' => o.max_args = Some(count(&v).ok_or_else(|| format!("invalid -n '{v}'"))?),
        's' => o.size = count(&v).ok_or_else(|| format!("invalid -s '{v}'"))?,
        _ => o.replace = Some(v.into_bytes()),
    }
    Ok(())
}

/// `-d`'s argument: one byte, or a C escape for one.
fn delimiter(d: &str) -> Result<u8, String> {
    match d.as_bytes() {
        [b] => Ok(*b),
        b"\\n" => Ok(b'\n'),
        b"\\t" => Ok(b'\t'),
        b"\\0" => Ok(0),
        b"\\\\" => Ok(b'\\'),
        _ => Err(format!("invalid delimiter '{d}' (one character)")),
    }
}

/// Split the input into arguments.
fn split(input: &[u8], how: &Split) -> Result<Vec<Vec<u8>>, String> {
    match how {
        Split::Delim(d) => {
            let mut items: Vec<Vec<u8>> = input.split(|b| b == d).map(<[u8]>::to_vec).collect();
            // A trailing delimiter ends the last item rather than starting an empty one.
            if items.last().is_some_and(Vec::is_empty) {
                items.pop();
            }
            Ok(items)
        }
        Split::Posix => posix_split(input),
    }
}

/// POSIX: blanks and newlines separate; `'...'` and `"..."` quote (not across
/// a newline); `\` escapes the next byte.
fn posix_split(input: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut items = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut in_item = false;
    let mut quote: Option<u8> = None;
    let mut iter = input.iter().copied();
    while let Some(b) = iter.next() {
        if let Some(q) = quote {
            match b {
                b'\n' => {
                    return Err(format!(
                        "unmatched {} quote",
                        if q == b'\'' { "single" } else { "double" }
                    ))
                }
                _ if b == q => quote = None,
                _ => cur.push(b),
            }
            continue;
        }
        match b {
            b' ' | b'\t' | b'\n' => {
                if in_item {
                    items.push(std::mem::take(&mut cur));
                    in_item = false;
                }
            }
            b'\'' | b'"' => {
                quote = Some(b);
                in_item = true;
            }
            b'\\' => {
                if let Some(n) = iter.next() {
                    cur.push(n);
                }
                in_item = true;
            }
            _ => {
                cur.push(b);
                in_item = true;
            }
        }
    }
    if let Some(q) = quote {
        return Err(format!(
            "unmatched {} quote",
            if q == b'\'' { "single" } else { "double" }
        ));
    }
    if in_item {
        items.push(cur);
    }
    Ok(items)
}

/// `-I`'s units: delimited items, or whole lines with leading blanks dropped
/// and quotes and backslashes still read, so a blank inside a line is kept
/// rather than splitting it (GNU's reading).
fn lines(input: &[u8], how: &Split) -> Result<Vec<Vec<u8>>, String> {
    match how {
        // An empty item between two delimiters is still an item, as GNU's -I
        // runs it; only the trailing delimiter's empty remainder is not.
        Split::Delim(_) => split(input, how),
        Split::Posix => input
            .split(|&b| b == b'\n')
            .map(|l| {
                let start = l
                    .iter()
                    .position(|&b| b != b' ' && b != b'\t')
                    .unwrap_or(l.len());
                l.get(start..).unwrap_or(&[])
            })
            .filter(|l| !l.is_empty())
            .map(unquote_line)
            .collect(),
    }
}

/// One `-I` line with its quotes and backslashes resolved and its blanks kept.
fn unquote_line(line: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(line.len());
    let mut quote: Option<u8> = None;
    let mut iter = line.iter().copied();
    while let Some(b) = iter.next() {
        match (quote, b) {
            (Some(q), _) if b == q => quote = None,
            (Some(_), _) => out.push(b),
            (None, b'\'' | b'"') => quote = Some(b),
            (None, b'\\') => out.extend(iter.next()),
            (None, _) => out.push(b),
        }
    }
    match quote {
        Some(q) => Err(format!(
            "unmatched {} quote",
            if q == b'\'' { "single" } else { "double" }
        )),
        None => Ok(out),
    }
}

fn replace(arg: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return arg.to_vec();
    }
    let mut out = Vec::with_capacity(arg.len());
    let mut i = 0;
    while i < arg.len() {
        if arg.get(i..i + from.len()) == Some(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else if let Some(&b) = arg.get(i) {
            out.push(b);
            i += 1;
        }
    }
    out
}

enum Outcome {
    Ok,
    Failed,
    Stop(u8),
}

fn invoke(argv: &[OsString], trace: bool) -> Outcome {
    let Some((prog, rest)) = argv.split_first() else {
        return Outcome::Stop(1);
    };
    if trace {
        let line: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        crate::emit_err(&format!("{}\n", line.join(" ")));
    }
    match Command::new(prog).args(rest).status() {
        Ok(s) => match (s.code(), s.signal()) {
            (Some(0), _) => Outcome::Ok,
            (Some(255), _) => {
                crate::emit_err(&format!(
                    "xargs: {}: exited with status 255; aborting\n",
                    prog.to_string_lossy()
                ));
                Outcome::Stop(124)
            }
            (Some(_), _) => Outcome::Failed,
            (None, Some(sig)) => {
                crate::emit_err(&format!(
                    "xargs: {}: terminated by signal {sig}\n",
                    prog.to_string_lossy()
                ));
                Outcome::Stop(125)
            }
            (None, None) => Outcome::Stop(125),
        },
        Err(e) => {
            crate::emit_err(&format!("xargs: {}: {e}\n", prog.to_string_lossy()));
            Outcome::Stop(if e.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn s(v: &[&str]) -> Vec<Vec<u8>> {
        v.iter().map(|x| x.as_bytes().to_vec()).collect()
    }

    #[test]
    fn posix_splitting_honours_quotes_and_escapes() {
        assert_eq!(posix_split(b"a b\n\tc").unwrap(), s(&["a", "b", "c"]));
        assert_eq!(
            posix_split(b"'a b' \"c d\" e\\ f").unwrap(),
            s(&["a b", "c d", "e f"])
        );
        assert_eq!(posix_split(b"'' x").unwrap(), s(&["", "x"]));
        assert!(posix_split(b"'open").is_err());
        assert!(posix_split(b"\"a\nb\"").is_err());
        assert!(posix_split(b"  \n ").unwrap().is_empty());
    }

    #[test]
    fn delimited_splitting_and_lines() {
        assert_eq!(
            split(b"a b\0c\0", &Split::Delim(0)).unwrap(),
            s(&["a b", "c"])
        );
        assert_eq!(
            split(b"a,,b", &Split::Delim(b',')).unwrap(),
            s(&["a", "", "b"])
        );
        assert_eq!(lines(b"  x y\n\nz\n", &Split::Posix), Ok(s(&["x y", "z"])));
        assert_eq!(delimiter("\\n").unwrap(), b'\n');
        assert!(delimiter("ab").is_err());
        assert_eq!(replace(b"<{}>{}", b"{}", b"Q"), b"<Q>Q".to_vec());
    }
}
