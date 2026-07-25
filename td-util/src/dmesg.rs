//! `dmesg` — the kernel ring buffer, read from /dev/kmsg.
//!
//! /dev/kmsg returns one record per `read`, oldest first, and then blocks for
//! new ones. Opening it O_NONBLOCK turns that final block into EAGAIN, which is
//! the documented "buffer drained" signal -- so the applet is ordinary safe
//! `std` file I/O and needs no `klogctl`/`syslog` syscall wrapper (which is the
//! route busybox and util-linux take, and the reason a Rust dmesg usually wants
//! `unsafe`). If the reader falls behind and records are overwritten the kernel
//! returns EPIPE and re-seeks us to the oldest surviving record, so a retry is
//! the correct response.

use std::io::Read;

/// open(2)'s O_NONBLOCK on Linux. `OpenOptionsExt::custom_flags` takes the raw
/// value, which keeps this crate libc-free.
const O_NONBLOCK: i32 = 0o4000;

/// Bound the EPIPE retry: a pathologically busy log must not spin here forever.
const MAX_OVERRUN_RETRIES: u32 = 64;

/// Unlike util-linux's `SYSLOG_ACTION_READ_ALL`, a /dev/kmsg drain is not a
/// snapshot: EAGAIN only arrives once the reader catches up, so a kernel logging
/// at least as fast as we read would never terminate the loop and `text` would
/// grow until OOM -- exactly the "kernel is spewing" case you run dmesg for.
/// Stop at a ceiling above the largest configurable ring (CONFIG_LOG_BUF_SHIFT
/// caps at 25, i.e. 32 MiB), so reaching it means new records, not a big buffer.
const MAX_DRAIN_BYTES: usize = 32 << 20;

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut show_time = true;
    let mut raw = false;
    for a in args {
        match a.as_str() {
            "-t" | "--notime" => show_time = false,
            "-r" | "--raw" => raw = true,
            other => {
                return Err(format!(
                    "unrecognised option '{other}'\nusage: dmesg [-t] [-r]"
                ))
            }
        }
    }
    let text = read_kmsg("/dev/kmsg")?;
    crate::emit(&render(&text, show_time, raw))?;
    Ok(0)
}

pub fn render(text: &str, show_time: bool, raw: bool) -> String {
    let mut out = String::new();
    for line in text.lines() {
        // A continuation line (the `SUBSYSTEM=`/`DEVICE=` metadata the kernel
        // appends, indented by a tab) has no `;` header and is dropped, which is
        // what dmesg shows by default.
        if let Some(rec) = Record::parse(line) {
            out.push_str(&rec.render(show_time, raw));
            out.push('\n');
        }
    }
    out
}

fn read_kmsg(path: &str) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let mut text = String::new();
    let mut buf = vec![0u8; 8192];
    let mut overruns = 0u32;
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                text.push_str(&String::from_utf8_lossy(buf.get(..n).unwrap_or(&[])));
                if text.len() >= MAX_DRAIN_BYTES {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            // A signal during the read is not a failure to read.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                overruns = overruns.saturating_add(1);
                if overruns > MAX_OVERRUN_RETRIES {
                    return Err(format!(
                        "{path}: the ring buffer is being overwritten faster than it can be read"
                    ));
                }
            }
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(text)
}

pub struct Record<'a> {
    pub prio: u64,
    pub usec: u64,
    pub message: &'a str,
}

impl<'a> Record<'a> {
    /// `prio,seq,usec,flags[,extra];message`
    pub fn parse(line: &'a str) -> Option<Record<'a>> {
        let semi = line.bytes().position(|b| b == b';')?;
        let head = line.get(..semi)?;
        let message = line.get(semi + 1..)?;
        let mut parts = head.split(',');
        let prio: u64 = parts.next()?.parse().ok()?;
        let _seq = parts.next()?;
        let usec: u64 = parts.next()?.parse().ok()?;
        Some(Record {
            prio,
            usec,
            message,
        })
    }

    /// `raw` is the `<prio>` syslog prefix both util-linux `dmesg --raw` and
    /// busybox `dmesg -r` emit -- NOT the /dev/kmsg wire record, whose sequence
    /// and flag fields are an implementation detail no script parses.
    pub fn render(&self, show_time: bool, raw: bool) -> String {
        let mut out = String::new();
        if raw {
            out.push_str(&format!("<{}>", self.prio));
        }
        if show_time {
            out.push_str(&format!(
                "[{:5}.{:06}] ",
                self.usec / 1_000_000,
                self.usec % 1_000_000
            ));
        }
        out.push_str(&unescape(self.message));
        out
    }
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Since Linux 3.5 /dev/kmsg escapes every byte outside printable ASCII as
/// `\xNN`, so a record is always exactly one line. Undo it, or a kernel
/// backtrace — the thing you reach for dmesg to read — prints as one long line
/// of literal `\x0a`. Decoding at BYTE level (not char) is what reassembles a
/// multi-byte UTF-8 sequence the kernel escaped one byte at a time.
pub fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\' && bytes.get(i + 1) == Some(&b'x') {
            let hi = bytes.get(i + 2).copied().and_then(hexval);
            let lo = bytes.get(i + 3).copied().and_then(hexval);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 4;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    // The gate lints only non-test targets, but keep `cargo clippy --tests`
    // clean for local runs too.
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const LINE: &str = "6,339,5116980,-;Linux version 6.12.0";

    #[test]
    fn parses_priority_sequence_and_timestamp() {
        let r = Record::parse(LINE).unwrap();
        assert_eq!(r.prio, 6);
        assert_eq!(r.usec, 5116980);
        assert_eq!(r.message, "Linux version 6.12.0");
    }

    #[test]
    fn renders_with_and_without_the_timestamp() {
        let r = Record::parse(LINE).unwrap();
        assert_eq!(r.render(true, false), "[    5.116980] Linux version 6.12.0");
        assert_eq!(r.render(false, false), "Linux version 6.12.0");
    }

    /// `-r` must emit the `<prio>` syslog form util-linux and busybox both emit,
    /// NOT the kmsg wire record -- a script ported off busybox parses `<6>`.
    #[test]
    fn raw_emits_the_syslog_priority_prefix_not_the_wire_record() {
        let r = Record::parse(LINE).unwrap();
        assert_eq!(r.render(true, true), "<6>[    5.116980] Linux version 6.12.0");
        assert_eq!(r.render(false, true), "<6>Linux version 6.12.0");
        // The sequence number and flags field must never reach the output.
        assert!(!r.render(true, true).contains("339"));
        assert!(!r.render(true, true).contains(",-;"));
    }

    /// A message containing `;` must keep every character after the FIRST one --
    /// splitting on the last would truncate the text.
    #[test]
    fn only_the_first_semicolon_separates_header_from_message() {
        let r = Record::parse("6,1,7,-;a;b;c").unwrap();
        assert_eq!(r.message, "a;b;c");
    }

    #[test]
    fn malformed_and_continuation_lines_are_skipped() {
        assert!(Record::parse("").is_none());
        assert!(Record::parse("no semicolon here").is_none());
        assert!(Record::parse("6,1;too few header fields").is_none());
        assert!(Record::parse("6,1,notanumber,-;bad ts").is_none());
        assert!(Record::parse("notaprio,1,7,-;bad prio").is_none());
        // The kernel's indented metadata continuation.
        assert!(Record::parse(" SUBSYSTEM=acpi").is_none());
    }

    /// The kernel escapes a multi-line message so the record stays one line;
    /// without unescaping, a backtrace prints as literal `\x0a` runs.
    #[test]
    fn escaped_bytes_are_decoded() {
        assert_eq!(unescape(r"line1\x0aline2"), "line1\nline2");
        assert_eq!(unescape(r"tab\x09here"), "tab\there");
        // Byte-level decoding reassembles a UTF-8 sequence escaped byte by byte.
        assert_eq!(unescape(r"\xc3\xa9"), "é");
        // A lone or malformed escape is left alone rather than eating characters.
        assert_eq!(unescape(r"100\% \x"), r"100\% \x");
        assert_eq!(unescape(r"\xzz"), r"\xzz");
        assert_eq!(unescape("nothing to do"), "nothing to do");

        let r = Record::parse(r"6,1,7,-;oops\x0aat foo+0x1").unwrap();
        assert_eq!(r.render(false, false), "oops\nat foo+0x1");
    }

    #[test]
    fn render_drops_continuations_and_honours_raw() {
        let text = format!("{LINE}\n SUBSYSTEM=acpi\n");
        assert_eq!(render(&text, false, false), "Linux version 6.12.0\n");
        assert_eq!(render(&text, false, true), "<6>Linux version 6.12.0\n");
    }

    /// Exercise the read loop itself against a regular file. /dev/kmsg needs
    /// CAP_SYSLOG on a `dmesg_restrict` kernel, so the loop would otherwise only
    /// ever be covered on the boot oracle.
    #[test]
    fn read_loop_drains_a_file_and_reports_a_missing_one() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("td-util-kmsg-{}", std::process::id()));
        let body = format!("{LINE}\n6,340,5116999,-;second record\n");
        let mut fh = std::fs::File::create(&path).unwrap();
        fh.write_all(body.as_bytes()).unwrap();
        drop(fh);

        let path_s = path.display().to_string();
        assert_eq!(read_kmsg(&path_s).unwrap(), body);
        assert_eq!(render(&body, false, false).lines().count(), 2);

        let _ = std::fs::remove_file(&path);
        assert!(read_kmsg(&path_s).is_err(), "a missing ring buffer must be an error");
    }

    /// The drain must terminate on a source that never signals EAGAIN. Without
    /// the byte ceiling this test would hang (or OOM) rather than fail.
    #[test]
    fn the_drain_is_bounded_on_an_endless_source() {
        let text = read_kmsg("/dev/zero").unwrap_or_default();
        assert!(
            text.len() >= MAX_DRAIN_BYTES,
            "expected the drain to stop at its ceiling, got {} bytes",
            text.len()
        );
    }
}
