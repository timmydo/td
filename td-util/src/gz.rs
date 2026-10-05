//! `gzip`, `gunzip` and `zcat`. Compression is `deflate.rs` and always
//! writes the reproducible `-n` header; decompression is the engine's
//! inflater, the same one source preparation trusts. A FILE operand is
//! replaced by FILE.gz (or the reverse) unless `-c` or `-k` says otherwise.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

const USAGE: &str = "usage: gzip [-cdfkn1-9] [FILE...]";

#[derive(Clone, Copy)]
struct Opts {
    decompress: bool,
    stdout: bool,
    keep: bool,
    force: bool,
    level: u8,
}

pub fn gzip(args: &[String]) -> Result<u8, String> {
    main(args, false, false)
}

pub fn gunzip(args: &[String]) -> Result<u8, String> {
    main(args, true, false)
}

pub fn zcat(args: &[String]) -> Result<u8, String> {
    main(args, true, true)
}

fn main(args: &[String], decompress: bool, stdout: bool) -> Result<u8, String> {
    let mut o = Opts {
        decompress,
        stdout,
        keep: false,
        force: false,
        level: 6,
    };
    let mut files: Vec<&str> = Vec::new();
    let mut opts_done = false;
    for a in args {
        let s = a.as_str();
        if opts_done || !s.starts_with('-') || s == "-" {
            files.push(s);
            continue;
        }
        if s == "--" {
            opts_done = true;
            continue;
        }
        for c in s.chars().skip(1) {
            match c {
                'c' => o.stdout = true,
                'd' => o.decompress = true,
                'k' => o.keep = true,
                'f' => o.force = true,
                // The header never carries a name or time, so -n is the only mode.
                'n' => {}
                '1'..='9' => o.level = c as u8 - b'0',
                _ => return Err(format!("unrecognised option '-{c}'\n{USAGE}")),
            }
        }
    }
    if files.is_empty() {
        files.push("-");
    }
    let mut status = 0u8;
    for f in files {
        if let Err(e) = one(f, o) {
            crate::emit_err(&format!("gzip: {e}\n"));
            status = 1;
        }
    }
    Ok(status)
}

fn transform(data: &[u8], o: Opts) -> Result<Vec<u8>, String> {
    if o.decompress {
        if !data.starts_with(&[0x1f, 0x8b]) {
            return Err("not in gzip format".to_string());
        }
        crate::gzip::decompress_bytes(data)
    } else if data.len() > crate::gzip::MAX_GZIP_OUTPUT_BYTES {
        // The inflater's own ceiling: past it gzip would write, and then
        // delete the original for, a file gunzip refuses to read back.
        Err(format!(
            "input is over the {} MiB this gzip can read back",
            crate::gzip::MAX_GZIP_OUTPUT_BYTES >> 20
        ))
    } else {
        Ok(crate::deflate::gzip(data, o.level))
    }
}

fn one(path: &str, o: Opts) -> Result<(), String> {
    if path == "-" {
        let mut data = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut data)
            .map_err(|e| format!("stdin: {e}"))?;
        return write_stdout(&transform(&data, o)?);
    }
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let out = transform(&data, o).map_err(|e| format!("{path}: {e}"))?;
    if o.stdout {
        return write_stdout(&out);
    }
    let target = if o.decompress {
        if let Some(stem) = path.strip_suffix(".gz") {
            stem.to_string()
        } else if let Some(stem) = path.strip_suffix(".tgz") {
            format!("{stem}.tar")
        } else {
            return Err(format!("{path}: unknown suffix -- ignored"));
        }
    } else {
        if path.ends_with(".gz") && !o.force {
            return Err(format!("{path} already has .gz suffix -- unchanged"));
        }
        format!("{path}.gz")
    };
    // Created, never opened over: without -f an existing name, a dangling
    // link among them, is refused; with it the old name goes first.
    if o.force {
        match std::fs::remove_file(&target) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("{target}: {e}"));
            }
            _ => {}
        }
    }
    // 0600 until the input's mode is copied, so nothing is readable early.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&target)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => format!("{target} already exists"),
            _ => format!("{target}: {e}"),
        })?;
    if let Err(e) = file.write_all(&out) {
        // A partial output would only make the next run say it exists.
        let _ = std::fs::remove_file(&target);
        return Err(format!("{target}: {e}"));
    }
    // The input's mode and time carry over, as GNU's do; the header's own
    // mtime stays zero so the bytes depend only on the input.
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = file.set_permissions(meta.permissions());
        if let Ok(t) = meta.modified() {
            let _ = file.set_modified(t);
        }
    }
    if !o.keep {
        std::fs::remove_file(path).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
