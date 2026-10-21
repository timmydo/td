//! Response files: GCC's `@file` on the way in, LLD's on the way out.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

/// Nesting GCC would follow; deeper is a loop or an attack, not a link.
const MAX_DEPTH: usize = 16;

/// Expand every `@file` argument in place, recursively. Also returns
/// whether the caller used one, in which case the driver hands LLD its own.
pub fn expand(args: Vec<OsString>) -> Result<(Vec<OsString>, bool), String> {
    let mut used = false;
    let mut out = Vec::with_capacity(args.len());
    expand_into(args, 0, &mut out, &mut used)?;
    Ok((out, used))
}

fn expand_into(
    args: Vec<OsString>,
    depth: usize,
    out: &mut Vec<OsString>,
    used: &mut bool,
) -> Result<(), String> {
    for arg in args {
        let Some(name) = arg.as_bytes().strip_prefix(b"@") else {
            out.push(arg);
            continue;
        };
        if depth >= MAX_DEPTH {
            return Err(format!(
                "response files nest deeper than {MAX_DEPTH}: {}",
                arg.to_string_lossy()
            ));
        }
        let path = PathBuf::from(OsString::from_vec(name.to_vec()));
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("read response file {}: {e}", path.display()))?;
        *used = true;
        expand_into(parse(&bytes), depth + 1, out, used)?;
    }
    Ok(())
}

/// GCC's (libiberty's) response-file syntax: whitespace separates arguments,
/// a backslash takes the next byte literally, and single or double quotes
/// group bytes, whitespace included.
pub fn parse(bytes: &[u8]) -> Vec<OsString> {
    let mut args = Vec::new();
    let mut current = Vec::new();
    let mut started = false;
    let mut escaped = false;
    let mut quote = None;
    for &byte in bytes {
        if escaped {
            current.push(byte);
            escaped = false;
            continue;
        }
        match (quote, byte) {
            (_, b'\\') => {
                escaped = true;
                started = true;
            }
            (Some(q), b) if b == q => quote = None,
            (Some(_), b) => current.push(b),
            (None, b'\'' | b'"') => {
                quote = Some(byte);
                started = true;
            }
            (None, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c') => {
                if started {
                    args.push(OsString::from_vec(std::mem::take(&mut current)));
                    started = false;
                }
            }
            (None, b) => {
                current.push(b);
                started = true;
            }
        }
    }
    if started {
        args.push(OsString::from_vec(current));
    }
    args
}

/// LLD's GNU response-file syntax: one double-quoted argument per line, with
/// backslash and double quote escaped. `parse` reads it back unchanged.
pub fn encode(args: &[OsString]) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in args {
        out.push(b'"');
        for &byte in arg.as_bytes() {
            if matches!(byte, b'\\' | b'"') {
                out.push(b'\\');
            }
            out.push(byte);
        }
        out.extend_from_slice(b"\"\n");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use td_fs::TempDir;

    fn os(args: &[&[u8]]) -> Vec<OsString> {
        args.iter()
            .map(|a| OsString::from_vec(a.to_vec()))
            .collect()
    }

    #[test]
    fn gcc_syntax_splits_on_whitespace_and_honours_quotes_and_escapes() {
        assert_eq!(
            parse(b"-o out\n  a\\ b.o 'c d.o'\t\"e\\\"f\" \"\" g\\\\h\n"),
            os(&[b"-o", b"out", b"a b.o", b"c d.o", b"e\"f", b"", b"g\\h"])
        );
        assert!(parse(b" \n\t").is_empty());
    }

    #[test]
    fn rustc_escaping_reads_back() {
        // rustc's GNU form: one argument per line, space and backslash escaped.
        assert_eq!(
            parse(b"-L\n/a\\ dir/lib\n-Wl,--version-script=/t\\\\v\n"),
            os(&[b"-L", b"/a dir/lib", b"-Wl,--version-script=/t\\v"])
        );
    }

    #[test]
    fn encoding_round_trips_every_byte() {
        let args = os(&[
            b"plain",
            b"sp ace",
            b"q\"uote",
            b"back\\slash",
            b"",
            b"\xff\xfe",
        ]);
        assert_eq!(parse(&encode(&args)), args);
    }

    #[test]
    fn nested_files_expand_in_place_and_their_use_is_reported() {
        let dir = TempDir::new("td-link-response").unwrap();
        let inner = dir.join("inner");
        let outer = dir.join("outer");
        std::fs::write(&inner, b"b.o c.o").unwrap();
        std::fs::write(&outer, format!("a.o @{}\nd.o", inner.display())).unwrap();
        let (args, used) = expand(vec![
            OsString::from("-o"),
            OsString::from(format!("@{}", outer.display())),
            OsString::from("e.o"),
        ])
        .unwrap();
        assert_eq!(args, os(&[b"-o", b"a.o", b"b.o", b"c.o", b"d.o", b"e.o"]));
        assert!(used);
        assert_eq!(expand(os(&[b"a.o"])).unwrap(), (os(&[b"a.o"]), false));
    }

    #[test]
    fn a_missing_or_looping_response_file_is_refused() {
        let dir = TempDir::new("td-link-response-loop").unwrap();
        let missing = dir.join("missing");
        let error = expand(vec![OsString::from(format!("@{}", missing.display()))]).unwrap_err();
        assert!(error.contains("read response file"), "{error}");
        let looping = dir.join("loop");
        std::fs::write(&looping, format!("@{}", looping.display())).unwrap();
        let error = expand(vec![OsString::from(format!("@{}", looping.display()))]).unwrap_err();
        assert!(error.contains("nest deeper"), "{error}");
    }
}
