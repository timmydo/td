//! `cmp` — byte comparison: the first difference, every difference (`-l`), or
//! only the status (`-s`). 0 same, 1 different, 2 trouble.

use std::io::Read;

const USAGE: &str = "usage: cmp [-s|-l] FILE1 [FILE2]";

pub fn run(args: &[String]) -> Result<u8, String> {
    match compare(args) {
        Ok(code) => Ok(code),
        Err(msg) => {
            crate::emit_err(&format!("cmp: {msg}\n"));
            Ok(2)
        }
    }
}

fn compare(args: &[String]) -> Result<u8, String> {
    let mut silent = false;
    let mut list = false;
    let mut files: Vec<&str> = Vec::new();
    let mut opts_done = false;
    for a in args {
        match a.as_str() {
            "--" if !opts_done => opts_done = true,
            "-s" | "--silent" | "--quiet" if !opts_done => silent = true,
            "-l" | "--verbose" if !opts_done => list = true,
            s if !opts_done && s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unrecognised option '{s}'\n{USAGE}"));
            }
            s => files.push(s),
        }
    }
    let (a, b) = match files.as_slice() {
        [a] => (*a, "-"),
        [a, b] => (*a, *b),
        _ => return Err(USAGE.to_string()),
    };
    let ba = read_all(a)?;
    let bb = read_all(b)?;

    let mut line = 1u64;
    let mut differ = false;
    for (i, (x, y)) in ba.iter().zip(bb.iter()).enumerate() {
        if x != y {
            differ = true;
            if silent {
                return Ok(1);
            }
            if list {
                crate::emit(&format!("{} {:o} {:o}\n", i + 1, x, y))?;
            } else {
                crate::emit(&format!("{a} {b} differ: char {}, line {line}\n", i + 1))?;
                return Ok(1);
            }
        }
        if *x == b'\n' {
            line += 1;
        }
    }
    if ba.len() != bb.len() {
        if !silent {
            let shorter = if ba.len() < bb.len() { a } else { b };
            crate::emit_err(&format!("cmp: EOF on {shorter}\n"));
        }
        return Ok(1);
    }
    Ok(u8::from(differ))
}

/// A file's bytes, or stdin's for `-`.
pub fn read_all(path: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    if path == "-" {
        std::io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .map_err(|e| format!("-: {e}"))?;
        return Ok(buf);
    }
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}
