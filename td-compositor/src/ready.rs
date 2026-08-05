//! td-term's readiness socket, its marker, and the probe that reads it.
//!
//! §12 has the supervisor prove a terminal is up by connecting to a private
//! socket rather than by watching the console, and compares what the probe
//! prints against the `TD-TERM-READY` diagnostic the terminal itself emitted.
//! That comparison is only meaningful if the two cannot drift, so there is one
//! encoder: [`marker`] produces both.
//!
//! `publish` still has no production caller. The Wayland client exists now,
//! but §12 has readiness follow the first frame's `wl_buffer.release` and
//! frame callback, and an unmapped surface never gets past the compositor's
//! initial zero configure — so the client's handshake half cannot honestly
//! publish a grid. The frame landing is the caller. Each such item carries its
//! own `dead_code` allow rather than the module carrying one, so anything left
//! over once that lands is still visible.

use crate::pty;
use crate::socket;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

pub const MARKER: &str = "TD-TERM-READY";

/// Exactly the longest line [`marker`] can produce — both dimensions at their
/// widest. A byte more is not a readiness line, and the bound is what keeps a
/// peer that never ends its line from being read forever. A test pins this
/// against the encoder, so it cannot drift into being merely generous.
const MAX_LINE: usize = 39;

/// Inside td-svc's `PROBE_ATTEMPT`, which is the five seconds it gives ONE
/// probe before killing it — not the 30-second `DEFAULT_READY_TIMEOUT`, which
/// bounds the whole retry loop. Matching PROBE_ATTEMPT exactly would mean the
/// supervisor always won the race and the probe never reported its own
/// timeout, so an operator would see a kill where the terminal's silence is
/// the actual news.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// The one encoder of a readiness line. The QEMU diagnostic and the socket's
/// answer are the same bytes because both come from here — otherwise the
/// integration test compares two spellings that could drift apart while each
/// stays internally plausible.
pub fn marker(rows: u16, columns: u16) -> String {
    format!("{MARKER} rows={rows} columns={columns}\n")
}

/// The grid a readiness line describes, or why it is not one.
///
/// Fail-closed on every ambiguity, and deliberately order-pinned: the line is
/// compared against the diagnostic byte for byte, so a second accepted
/// spelling would be a second thing to compare.
pub fn parse(line: &str) -> Result<(u16, u16), String> {
    let body = line
        .strip_suffix('\n')
        .ok_or_else(|| "readiness line does not end".to_string())?
        .strip_prefix(MARKER)
        .and_then(|rest| rest.strip_prefix(' '))
        .ok_or_else(|| format!("readiness line does not begin with '{MARKER} '"))?;
    let mut fields = body.split(' ');
    let rows = field(fields.next(), "rows")?;
    let columns = field(fields.next(), "columns")?;
    if fields.next().is_some() {
        return Err("readiness line has more than a grid in it".into());
    }
    // The same definition the winsize ioctl is held to, so a readiness line
    // cannot describe a grid the terminal could never have been set to.
    let grid = pty::grid_size(usize::from(rows), usize::from(columns))?;
    Ok((grid.rows, grid.columns))
}

fn field(value: Option<&str>, name: &str) -> Result<u16, String> {
    let value = value.ok_or_else(|| format!("readiness line has no {name}"))?;
    let digits = value
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('='))
        .ok_or_else(|| format!("readiness field '{value}' is not {name}"))?;
    // `u16::from_str` also takes `+24` and `024`, which the encoder never
    // emits. Accepting them would make two more spellings of a ready terminal,
    // and the spelling is the thing §12 compares.
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return Err(format!("readiness {name} '{digits}' is not how a grid is written"));
    }
    digits
        .parse()
        .map_err(|_| format!("readiness {name} '{digits}' is not a grid dimension"))
}

/// Bind the readiness socket and answer every caller with this grid.
#[allow(dead_code)]
pub fn publish(path: &Path, rows: u16, columns: u16) -> Result<socket::Published, String> {
    let line = marker(rows, columns);
    // Refuse to publish a grid the probe would then reject, rather than
    // serving a line no caller can accept.
    parse(&line)?;
    socket::publish(path, "td-term-ready", line.into_bytes())
}

/// Ask a terminal whether it is up, and print what it answered.
///
/// The printed line is the terminal's own, unaltered, because §12 compares it
/// with the diagnostic that terminal emitted.
pub fn probe(path: &Path) -> Result<(), String> {
    probe_to(path, PROBE_TIMEOUT, &mut std::io::stdout())
}

/// Where the probe writes is a parameter so the BYTES it prints are testable:
/// §12 compares them with the terminal's own diagnostic, and a probe that
/// validated correctly and then printed something else would satisfy every
/// assertion about what it parsed.
fn probe_to(path: &Path, deadline: Duration, out: &mut impl Write) -> Result<(), String> {
    let line = answer(path, deadline)?;
    out.write_all(line.as_bytes())
        .map_err(|e| format!("write readiness answer: {e}"))?;
    out.flush()
        .map_err(|e| format!("flush readiness answer: {e}"))
}

/// The readiness line a terminal serves, bounded in both size and time.
///
/// The deadline is ABSOLUTE, which is the whole reason this is a hand-rolled
/// loop. `SO_RCVTIMEO` bounds one read, not the exchange: a peer dripping a
/// byte just inside the timeout renews it every time, and sixty-five of those
/// hold a probe open for sixty-five times as long as the number in
/// `PROBE_TIMEOUT` says. Each read is therefore given only what is LEFT.
fn answer(path: &Path, deadline: Duration) -> Result<String, String> {
    // `connect` itself is unbounded: `std` has no connect timeout for a Unix
    // socket, so a listening-but-never-accepting terminal is bounded only by
    // td-svc killing the attempt. Reaching it requires filling the backlog,
    // which nothing that answers in 39 bytes does.
    let mut stream = UnixStream::connect(path)
        .map_err(|e| format!("connect readiness socket {}: {e}", path.display()))?;
    let started = Instant::now();
    // Room for exactly one legal line: a buffer that fills without a newline
    // IS the oversized answer, so there is no second length check to keep in
    // step with this one. Reading to EOF instead would make the probe depend
    // on the terminal CLOSING the connection — a promise about the publisher
    // rather than about the protocol, and a publisher that later held
    // connections open would fail every health check with a good line in hand.
    let mut buffer = [0u8; MAX_LINE];
    let mut used = 0;
    loop {
        let left = deadline
            .checked_sub(started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                format!("readiness socket {} did not answer in time", path.display())
            })?;
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| format!("bound the readiness read: {e}"))?;
        let room = buffer
            .get_mut(used..)
            .ok_or_else(|| "readiness buffer walked off its own end".to_string())?;
        if room.is_empty() {
            return Err(format!(
                "readiness socket {} answered more than a grid",
                path.display()
            ));
        }
        let count = stream
            .read(room)
            .map_err(|e| format!("read readiness socket {}: {e}", path.display()))?;
        if count == 0 {
            return Err(format!(
                "readiness socket {} answered no complete line",
                path.display()
            ));
        }
        used = used.saturating_add(count);
        let filled = buffer
            .get(..used)
            .ok_or_else(|| "readiness buffer walked off its own end".to_string())?;
        let Some(end) = filled.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let line = filled
            .get(..=end)
            .ok_or_else(|| "readiness line walked off its own end".to_string())?;
        let line = std::str::from_utf8(line)
            .map_err(|_| format!("readiness socket {} answered non-UTF-8", path.display()))?;
        parse(line)?;
        return Ok(line.to_string());
    }
}

/// The packaged binary's own check of the readiness encoding, which needs no
/// socket and so runs wherever the artifact does.
pub fn selftest() -> Result<(), String> {
    let (rows, columns) = parse(&marker(24, 80))?;
    if (rows, columns) != (24, 80) {
        return Err("readiness marker did not survive its own parse".into());
    }
    if parse(&marker(0, 80)).is_ok() {
        return Err("readiness parse accepted a terminal with no rows".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// `cargo test` has no per-test timeout, so a probe that stopped bounding
    /// its wait would hang the gate rather than redden it.
    fn within<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver.recv_timeout(Duration::from_secs(30)).ok()
    }

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "td-ready-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn the_line_bound_is_the_longest_line_the_encoder_can_write() {
        // Not a generous round number: the widest grid must fit exactly, so
        // that one byte more is genuinely not a readiness line.
        assert_eq!(marker(u16::MAX, u16::MAX).len(), MAX_LINE);
        assert!(marker(1, 1).len() < MAX_LINE);
    }

    #[test]
    fn the_widest_grid_still_fits_through_a_real_socket() {
        let directory = scratch("widest");
        let path = directory.join("td-term-ready");
        let ready = publish(&path, u16::MAX, u16::MAX).unwrap();
        let mut printed = Vec::new();
        probe_to(&path, PROBE_TIMEOUT, &mut printed).unwrap();
        assert_eq!(printed, marker(u16::MAX, u16::MAX).into_bytes());
        drop(ready);
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn the_marker_and_the_parse_are_one_encoding() {
        assert_eq!(marker(24, 80), "TD-TERM-READY rows=24 columns=80\n");
        assert_eq!(parse(&marker(24, 80)).unwrap(), (24, 80));
        assert_eq!(parse(&marker(1, 1)).unwrap(), (1, 1));
        assert_eq!(parse(&marker(u16::MAX, u16::MAX)).unwrap(), (u16::MAX, u16::MAX));
    }

    #[test]
    fn a_readiness_line_is_refused_for_every_way_it_can_be_wrong() {
        // Not ready at all, or ready for something else.
        assert!(parse("").is_err());
        assert!(parse("TD-UI-CLIENT-READY rows=24 columns=80\n").is_err());
        assert!(parse("td-term-ready rows=24 columns=80\n").is_err());
        assert!(parse("TD-TERM-READYrows=24 columns=80\n").is_err());
        // A grid no terminal could have been set to.
        assert!(parse("TD-TERM-READY rows=0 columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=24 columns=0\n").is_err());
        assert!(parse("TD-TERM-READY rows=65536 columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=-1 columns=80\n").is_err());
        // Missing, misspelt, reordered, or padded out.
        assert!(parse("TD-TERM-READY rows=24\n").is_err());
        assert!(parse("TD-TERM-READY columns=80 rows=24\n").is_err());
        assert!(parse("TD-TERM-READY lines=24 columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=24 columns=80 pixels=0\n").is_err());
        assert!(parse("TD-TERM-READY rows= columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=24  columns=80\n").is_err());
        // Spellings `u16::from_str` would take but the encoder never writes.
        assert!(parse("TD-TERM-READY rows=+24 columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=024 columns=80\n").is_err());
        assert!(parse("TD-TERM-READY rows=24 columns=080\n").is_err());
        assert!(parse("TD-TERM-READY rows=00 columns=80\n").is_err());
        // Carriage returns and trailing content are not readiness either.
        assert!(parse("TD-TERM-READY rows=24 columns=80\r\n").is_err());
        assert!(parse("TD-TERM-READY rows=24 columns=80\n\n").is_err());
        assert!(parse("TD-TERM-READY rows=24 columns=80\nagain\n").is_err());
        assert!(parse(" TD-TERM-READY rows=24 columns=80\n").is_err());
        // A line that never ended is not a line. `read_line` returns exactly
        // this at EOF or at the bound, which is a terminal cut off mid-answer.
        assert!(parse("TD-TERM-READY rows=24 columns=80").is_err());
    }

    #[test]
    fn a_probe_reads_back_exactly_what_was_published() {
        let directory = scratch("published");
        let path = directory.join("td-term-ready");
        let ready = publish(&path, 24, 80).unwrap();
        // Private to the graphical user: §12's mode, not the umask's.
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        // The literal §12 specifies, not the constant under test.
        assert_eq!(mode, 0o600);

        let mut stream = UnixStream::connect(&path).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert_eq!(answer, marker(24, 80));
        // Serving one caller does not retire the socket -- and what the probe
        // PRINTS is the terminal's own line, byte for byte, since that is what
        // §12 compares against the diagnostic.
        let mut printed = Vec::new();
        probe_to(&path, PROBE_TIMEOUT, &mut printed).unwrap();
        assert_eq!(printed, marker(24, 80).into_bytes());

        drop(ready);
        assert!(!path.exists(), "a retired terminal left its socket behind");
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_live_terminal_is_never_displaced_but_a_dead_one_is() {
        let directory = scratch("stale");
        let path = directory.join("td-term-ready");
        let first = publish(&path, 24, 80).unwrap();
        // A second terminal must not take a socket someone is answering on.
        assert!(publish(&path, 12, 40).is_err());
        // The first is still the one serving.
        let mut printed = Vec::new();
        probe_to(&path, PROBE_TIMEOUT, &mut printed).unwrap();
        assert_eq!(printed, marker(24, 80).into_bytes());

        // A socket nobody answers is stale, and gets replaced.
        drop(first);
        UnixListener::bind(&path).unwrap();
        let _second = publish(&path, 12, 40).unwrap();
        let mut printed = Vec::new();
        probe_to(&path, PROBE_TIMEOUT, &mut printed).unwrap();
        assert_eq!(printed, marker(12, 40).into_bytes());

        // Anything that is not a socket is not something to replace at all.
        let occupied = directory.join("occupied");
        fs::write(&occupied, b"not a socket").unwrap();
        assert!(publish(&occupied, 24, 80).is_err());
        assert!(occupied.exists(), "a refused publication ate a real file");
        fs::remove_file(&occupied).unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_grid_the_probe_would_reject_is_never_published() {
        let directory = scratch("unpublishable");
        let path = directory.join("td-term-ready");
        assert!(publish(&path, 0, 80).is_err());
        assert!(
            !path.exists(),
            "a refused publication left a socket nobody serves"
        );
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn probing_something_that_is_not_a_ready_terminal_fails() {
        let directory = scratch("absent");
        let path = directory.join("td-term-ready");
        assert!(probe(&path).is_err());

        // A socket that answers something else is not readiness either.
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            if let Some(Ok(mut connection)) = listener.incoming().next() {
                let _ = connection.write_all(b"TD-TERM-READY rows=0 columns=0\n");
            }
        });
        assert!(probe(&path).is_err());
        handle.join().unwrap();

        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_probe_reads_one_line_without_waiting_for_the_peer_to_close() {
        let directory = scratch("held-open");
        let path = directory.join("td-term-ready");
        let listener = UnixListener::bind(&path).unwrap();
        let (done, held) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            if let Some(Ok(mut connection)) = listener.incoming().next() {
                let _ = connection.write_all(marker(24, 80).as_bytes());
                // Hold it open until the probe has already answered.
                let _ = held.recv_timeout(Duration::from_secs(30));
            }
        });
        probe(&path).unwrap();
        let _ = done.send(());
        handle.join().unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_silent_terminal_fails_the_probe_rather_than_holding_it() {
        let directory = scratch("silent");
        let path = directory.join("td-term-ready");
        let listener = UnixListener::bind(&path).unwrap();
        let (done, held) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            if let Some(Ok(connection)) = listener.incoming().next() {
                // Connected, and saying nothing.
                let _ = held.recv_timeout(Duration::from_secs(30));
                drop(connection);
            }
        });
        let probed = path.clone();
        let outcome = within(move || answer(&probed, Duration::from_millis(100)))
            .expect("the probe waited on a terminal that never answered");
        assert!(outcome.is_err());
        let _ = done.send(());
        handle.join().unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_terminal_dripping_an_answer_cannot_outlast_the_deadline() {
        let directory = scratch("drip");
        let path = directory.join("td-term-ready");
        let listener = UnixListener::bind(&path).unwrap();
        let (done, held) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            if let Some(Ok(mut connection)) = listener.incoming().next() {
                // A PERFECTLY GOOD line, one byte at a time, with every gap
                // comfortably inside the per-read timeout and the whole thing
                // far outside the deadline. A per-read bound renews on each
                // byte and lets this succeed; an absolute one cannot.
                for byte in marker(24, 80).into_bytes() {
                    if connection.write_all(&[byte]).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                let _ = held.recv_timeout(Duration::from_secs(30));
            }
        });
        let probed = path.clone();
        let outcome = within(move || answer(&probed, Duration::from_millis(120)))
            .expect("the probe waited past its own deadline");
        assert!(
            outcome.is_err(),
            "a drip-fed answer outlasted the probe deadline"
        );
        let _ = done.send(());
        handle.join().unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn an_answer_longer_than_a_grid_is_refused_rather_than_truncated() {
        let directory = scratch("oversized");
        let path = directory.join("td-term-ready");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            if let Some(Ok(mut connection)) = listener.incoming().next() {
                // A line that never ends inside the bound. The junk AFTER a
                // newline is not the case that matters -- the probe reads one
                // line and never sees it -- but a line with no newline in
                // reach would be read forever without this.
                let mut oversized = marker(24, 80).replace('\n', "");
                oversized.push_str(&"0".repeat(MAX_LINE));
                oversized.push('\n');
                let _ = connection.write_all(oversized.as_bytes());
            }
        });
        assert!(probe(&path).is_err());
        handle.join().unwrap();
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn the_selftest_covers_the_encoding() {
        selftest().unwrap();
    }
}
