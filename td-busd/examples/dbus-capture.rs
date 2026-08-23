//! Record a real D-Bus conversation, for the corpus in `../spec`.
//!
//! HOST-SIDE tooling, not part of the shipped `td-busd` binary (the recipe
//! compiles `src/main.rs` and its sibling modules alone). It sits between a real
//! client and the reference `dbus-daemon`, forwards every byte, and writes down
//! what crossed in which direction and in which read.
//!
//! Frame boundaries are recorded rather than normalized away, because they are
//! the interesting part: libdbus puts the leading NUL and its `AUTH` line in one
//! write, and `BEGIN` and its first message in another, which is exactly the
//! case a handshake that consumed its whole buffer would get wrong.
//!
//! Usage:
//!
//! ```text
//! cargo run --example dbus-capture -- \
//!     --listen /tmp/td-capture.sock \
//!     --upstream /run/user/1000/bus \
//!     --name libdbus-hello \
//!     --out td-busd/spec/libdbus-hello.conversation
//! # then, against the SAME socket:
//! dbus-send --address=unix:path=/tmp/td-capture.sock --print-reply ...
//! ```
//!
//! No `unsafe`, and no descriptor passing: a proxy that forwarded SCM_RIGHTS
//! would need `UNSAFE.md` surface #10, which this crate does not have. A
//! recording therefore holds what a client sends, and a conversation that
//! carried a descriptor would record the message and lose the descriptor — so
//! the tool refuses to be used that way rather than writing a fixture that
//! quietly means something else. See `--allow-fds` below.

#![forbid(unsafe_code)]

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// One read's worth of bytes, in the direction it travelled.
struct Frame {
    from_client: bool,
    bytes: Vec<u8>,
}

struct Args {
    listen: PathBuf,
    upstream: PathBuf,
    out: PathBuf,
    name: String,
    note: Option<String>,
    allow_fds: bool,
    /// How long a silence ends the recording. A conversation's frames arrive
    /// back to back, so a gap means it is over — and waiting for EOF instead
    /// hangs forever on a client like `dbus-monitor`, which is idle by design
    /// and holds its connection open.
    idle: Duration,
    /// How long to wait for a client. Bounded because the usual reason none
    /// arrives is that the client's own command line was wrong — and a tool
    /// that blocks forever on that gets wrapped in `timeout`, which kills it
    /// before it writes anything down.
    accept: Duration,
}

fn usage() -> String {
    "usage: dbus-capture --listen <path> --upstream <path> --out <file> \
--name <case> [--note <text>] [--allow-fds] [--idle-ms <n>] [--accept-ms <n>]"
        .to_string()
}

fn parse_args() -> Result<Args, String> {
    let mut listen = None;
    let mut upstream = None;
    let mut out = None;
    let mut name = None;
    let mut note = None;
    let mut allow_fds = false;
    let mut idle_ms = 500u64;
    let mut accept_ms = 10_000u64;
    let mut argv = env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => listen = Some(PathBuf::from(value()?)),
            "--upstream" => upstream = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--name" => name = Some(value()?),
            "--note" => note = Some(value()?),
            "--allow-fds" => allow_fds = true,
            "--idle-ms" => {
                let raw = value()?;
                idle_ms = raw
                    .parse()
                    .map_err(|_| format!("--idle-ms wants a number, not {raw}"))?;
            }
            "--accept-ms" => {
                let raw = value()?;
                accept_ms = raw
                    .parse()
                    .map_err(|_| format!("--accept-ms wants a number, not {raw}"))?;
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    Ok(Args {
        listen: listen.ok_or_else(usage)?,
        upstream: upstream.ok_or_else(usage)?,
        out: out.ok_or_else(usage)?,
        name: name.ok_or_else(usage)?,
        note,
        allow_fds,
        idle: Duration::from_millis(idle_ms),
        accept: Duration::from_millis(accept_ms),
    })
}

/// Wait for one client, giving up rather than blocking forever.
fn accept_within(listener: &UnixListener, limit: Duration) -> Result<UnixStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("setting the listener nonblocking: {e}"))?;
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((client, _)) => {
                client
                    .set_nonblocking(false)
                    .map_err(|e| format!("restoring blocking mode: {e}"))?;
                return Ok(client);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if start.elapsed() >= limit {
                    return Err(format!(
                        "no client connected within {limit:?} — check the client's \
--address, which must name this socket"
                    ));
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("accepting a client: {e}")),
        }
    }
}

/// Forward one direction to EOF, recording each read.
///
/// Returns how it ENDED. A recording truncated at a frame boundary replays
/// exactly like a complete one — the corpus reader cannot tell that the client
/// stopped talking from that the recorder stopped listening — so an I/O error
/// has to reach the caller rather than end a loop quietly.
fn pump(
    mut src: UnixStream,
    mut dst: UnixStream,
    from_client: bool,
    log: Sender<Frame>,
) -> Result<(), String> {
    let side = if from_client { "client" } else { "daemon" };
    let mut buf = vec![0u8; 65536];
    let outcome = loop {
        let read = match src.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(read) => read,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // A reset after the peer is done is how a socket normally ends.
            Err(e) if ended(&e) => break Ok(()),
            Err(e) => break Err(format!("reading from the {side}: {e}")),
        };
        let Some(bytes) = buf.get(..read) else {
            break Err(format!("a short read from the {side}"));
        };
        // Logged BEFORE forwarding, and that order is load-bearing: it is what
        // makes a reply impossible to record before the request that caused it,
        // which is the causal ordering the whole corpus rests on. The cost is
        // that a failed write below could leave a frame recorded that never
        // crossed — which is why that failure is an error, not a break.
        if log
            .send(Frame {
                from_client,
                bytes: bytes.to_vec(),
            })
            .is_err()
        {
            break Err(format!("the recorder stopped reading the {side}"));
        }
        if let Err(e) = dst.write_all(bytes) {
            break Err(format!("forwarding to the far side of the {side}: {e}"));
        }
    };
    // Let the far side see the close rather than hanging on a half-open pair.
    let _ = dst.shutdown(std::net::Shutdown::Write);
    outcome
}

/// Errors that mean "the connection is over", as opposed to "something broke".
fn ended(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
    )
}

fn render(args: &Args, frames: &[Frame]) -> String {
    let mut out = String::new();
    out.push_str("# Recorded D-Bus conversation. See ./README for the format and\n");
    out.push_str("# for how this file was produced.\n");
    let _ = writeln!(out, "name: {}", args.name);
    if let Some(note) = &args.note {
        let _ = writeln!(out, "note: {note}");
    }
    out.push('\n');
    for frame in frames {
        let tag = if frame.from_client { 'C' } else { 'S' };
        let mut hex = String::with_capacity(frame.bytes.len().saturating_mul(2));
        for byte in &frame.bytes {
            let _ = write!(hex, "{byte:02x}");
        }
        // One frame per line: a reader splits on whitespace and never has to
        // guess where a read ended.
        let _ = writeln!(out, "{tag} {hex}");
    }
    out
}

fn run() -> Result<String, String> {
    let args = parse_args()?;

    if args.listen.exists() {
        // --listen and --out are both bare paths and adjacent in the documented
        // invocation. Unlinking whatever is there would turn one swapped
        // argument into a deleted, committed recording.
        let kind = fs::metadata(&args.listen)
            .map_err(|e| format!("inspecting {}: {e}", args.listen.display()))?
            .file_type();
        if !kind.is_socket() {
            return Err(format!(
                "{} exists and is not a socket — refusing to remove it",
                args.listen.display()
            ));
        }
        fs::remove_file(&args.listen)
            .map_err(|e| format!("removing a stale {}: {e}", args.listen.display()))?;
    }
    let listener = UnixListener::bind(&args.listen)
        .map_err(|e| format!("binding {}: {e}", args.listen.display()))?;
    println!("listening on {}", args.listen.display());

    let client = match accept_within(&listener, args.accept) {
        Ok(client) => client,
        Err(e) => {
            let _ = fs::remove_file(&args.listen);
            return Err(e);
        }
    };
    let upstream = UnixStream::connect(&args.upstream)
        .map_err(|e| format!("connecting to {}: {e}", args.upstream.display()))?;

    let client_out = client
        .try_clone()
        .map_err(|e| format!("cloning the client socket: {e}"))?;
    let upstream_out = upstream
        .try_clone()
        .map_err(|e| format!("cloning the upstream socket: {e}"))?;
    // Kept so the idle path below can unblock a pump that is parked in `read`
    // — joining one of those would hang exactly where the timeout was added.
    let client_stop = client
        .try_clone()
        .map_err(|e| format!("cloning the client socket: {e}"))?;
    let upstream_stop = upstream
        .try_clone()
        .map_err(|e| format!("cloning the upstream socket: {e}"))?;

    let (tx, rx) = mpsc::channel();
    let to_upstream = {
        let tx = tx.clone();
        thread::spawn(move || pump(client, upstream_out, true, tx))
    };
    let to_client = thread::spawn(move || pump(upstream, client_out, false, tx));

    // Collect until both sides hang up, or until the conversation goes quiet.
    let mut frames = Vec::new();
    loop {
        match rx.recv_timeout(args.idle) {
            Ok(frame) => frames.push(frame),
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Stop the pumps first, THEN drain: dropping the receiver here would
    // discard a frame already in flight and fail its send, which is a silently
    // shorter recording.
    let _ = client_stop.shutdown(std::net::Shutdown::Both);
    let _ = upstream_stop.shutdown(std::net::Shutdown::Both);
    let outcomes = [
        to_upstream.join().unwrap_or_else(|_| Err("the client pump panicked".to_string())),
        to_client.join().unwrap_or_else(|_| Err("the daemon pump panicked".to_string())),
    ];
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }
    let _ = fs::remove_file(&args.listen);

    // A recording that lost bytes is not a smaller recording, it is a wrong
    // one: it replays exactly like a complete conversation.
    for outcome in outcomes {
        outcome.map_err(|e| format!("{e} — refusing to write a truncated recording"))?;
    }

    if frames.is_empty() {
        return Err("nothing crossed: the client never connected".to_string());
    }
    // A conversation that negotiated descriptor passing may have carried one,
    // and a proxy without surface #10 did not forward it. Recording it anyway
    // would put a fixture in the corpus whose UNIX_FDS field describes
    // descriptors the bytes never had.
    let mut whole = Vec::new();
    for frame in &frames {
        whole.extend_from_slice(&frame.bytes);
    }
    let negotiated = whole.windows(13).any(|w| w == b"AGREE_UNIX_FD");
    if negotiated && !args.allow_fds {
        return Err("this conversation negotiated descriptor passing, so a \
message in it may reference a descriptor this proxy could not forward. Re-run \
with --allow-fds if you have checked that no message carries one."
            .to_string());
    }

    let rendered = render(&args, &frames);
    fs::write(&args.out, &rendered)
        .map_err(|e| format!("writing {}: {e}", args.out.display()))?;
    let bytes: usize = frames.iter().map(|f| f.bytes.len()).sum();
    Ok(format!(
        "recorded {} frames, {bytes} bytes, to {}",
        frames.len(),
        args.out.display()
    ))
}

fn main() {
    match run() {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("dbus-capture: {error}");
            std::process::exit(1);
        }
    }
}
