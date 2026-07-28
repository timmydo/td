//! The control socket: `/run/td-svc/control`.
//!
//! One thread blocked in `accept`, handing each request to the main loop and
//! writing back what it answers. The main loop stays the sole owner of
//! supervision state (DESIGN.md §5), so nothing here touches a `Service` — a
//! request crosses as a message and the reply crosses back the same way.
//!
//! Connections are served ONE AT A TIME on that thread. Control traffic is an
//! operator typing, not a hot path, and serialising it means a slow client
//! cannot interleave with another mid-reply.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::supervise::{log, Event};

pub const DIR: &str = "/run/td-svc";
pub const PATH: &str = "/run/td-svc/control";

/// The directory is `0700` BEFORE the socket exists inside it.
///
/// A socket's own mode is not portably enforced — Linux does check it, but the
/// bind races: between `bind` and a `set_permissions` on the socket there is a
/// window where the default mode applies. Creating the *directory* restricted
/// first closes it, because a path that cannot be traversed cannot be connected
/// to whatever the socket's own bits say.
const DIR_MODE: u32 = 0o700;

/// How long a client waits for the main loop to answer.
///
/// Bounded rather than infinite so a wedged supervisor produces a diagnostic
/// instead of a hung shell — the operator reaching for this socket is often
/// doing so BECAUSE something is wrong, and a control tool that hangs then is
/// the least useful thing it could do.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a client may take to send its request line.
///
/// Connections are served ONE AT A TIME, so a client that connects and never
/// sends a newline would otherwise block the accept thread for good — and the
/// control socket is most wanted exactly when something is already wrong.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the reply may take to go out. Same argument as `READ_TIMEOUT`: a
/// client that asks and then stops reading must not hold the accept thread.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest reply a client will read. Bounded for the same reason the
/// request is: a client must not be a way to exhaust memory either.
const MAX_REPLY: u64 = 1 << 20;

/// The longest request accepted. Every verb is a word and a service name; a
/// megabyte of it is not a request, and `read_line` on an unbounded stream is
/// an unbounded allocation inside PID 1's only supervisor.
const MAX_REQUEST: u64 = 4096;

/// Remove a socket left by a supervisor that DIED — and only that.
///
/// td-svc is respawned by PID 1, so finding a socket here after a crash is the
/// ordinary case, and `bind` fails with EADDRINUSE on an existing path whether
/// or not anyone is listening; refusing to unlink would cost the socket until
/// the next reboot. But unlinking unconditionally is worse: `0700` keeps out
/// other USERS, not a second root td-svc, so a second instance would make the
/// first unreachable while both went on supervising, and every later `stop`
/// would address the wrong one.
///
/// Connecting is the test, because it is the same question a client asks. A
/// refused connection means nobody is accepting and the file is a corpse; a
/// successful one means a live supervisor owns this path and we must not.
fn clear_stale(path: &Path) -> std::io::Result<()> {
    // `symlink_metadata`, not `exists`: `exists` follows symlinks, so a
    // DANGLING one at this path reports absent, the unlink is skipped, and
    // `bind` then fails EADDRINUSE against a link pointing nowhere — no
    // control socket until the next reboot, for a file we could have removed.
    if path.symlink_metadata().is_err() {
        return Ok(());
    }
    // Only a refusal PROVES nobody is listening. Any other error — EACCES,
    // EMFILE, EINTR — says the question could not be asked, and unlinking on
    // one of those would take a LIVE supervisor's socket away because the
    // machine was briefly out of descriptors. Fail closed: keep the path.
    match UnixStream::connect(path) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("{} is served by a live supervisor", path.display()),
            ))
        }
        // Both mean nobody is listening, and we only got here because
        // something IS at this path: a refusal is a dead socket, and ENOENT is
        // a symlink pointing nowhere. Returning early on the latter would skip
        // the unlink and leave `bind` failing against the link forever.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) => {}
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "{}: cannot tell whether a supervisor is listening ({e}); \
                 leaving it alone",
                path.display()
            )))
        }
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create the directory AT its final mode, verify it, and bind.
///
/// `create_dir_all` would make it `0777 & ~umask`, and td-svc inherits PID 1's
/// umask, which PID 1 never sets — so the directory would exist world-writable
/// for the window before a `set_permissions` narrowed it. Anything that opened
/// a descriptor to it in that window keeps traversal rights afterwards, which
/// would put `stop`/`start` on this machine's services behind no barrier at
/// all. `mkdir(2)` applies the mode atomically and umask can only REMOVE bits
/// from it, so 0700 is a ceiling as well as a floor.
pub fn bind(dir: &str, path: &str) -> std::io::Result<UnixListener> {
    ensure_dir(dir)?;
    clear_stale(Path::new(path))?;
    UnixListener::bind(path)
}

/// The directory half of `bind`, shared with the eviction record.
///
/// Eviction writes into this directory BEFORE the socket is bound, so if it
/// created the directory itself with `create_dir_all` the 0700 reasoning above
/// would be defeated by the one caller that runs first.
pub fn ensure_dir(dir: &str) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(DIR_MODE).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Pre-existing: it was not created here, so its mode and owner are
            // not ours to assume. A directory somebody else owns is refused
            // rather than narrowed — narrowing does not revoke a descriptor
            // they already hold.
            let meta = std::fs::symlink_metadata(dir)?;
            if !meta.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{dir} exists and is not a directory"),
                ));
            }
            if meta.uid() != nix_getuid() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{dir} is owned by uid {}, not by this process", meta.uid()),
                ));
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DIR_MODE))?;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Is this directory a place a root process may take instructions from?
///
/// The eviction record names pids that td-svc will signal as root, so a
/// directory anyone else can WRITE is a way to choose them. `ensure_dir`
/// establishes this for a directory td-svc created; a reader that runs before
/// the socket is bound has to establish it for itself.
///
/// Write bits only, not `DIR_MODE`: a root-owned 0755 `/run/td-svc` that some
/// earlier boot step made is still a directory only root can put a record in,
/// and refusing it would disable eviction on a machine that is not under
/// attack. Being readable is not the threat here — the socket, where traversal
/// is what grants `stop`, is the stricter case and keeps 0700.
pub fn dir_is_trusted(dir: &str) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|meta| {
        meta.is_dir() && meta.uid() == nix_getuid() && meta.permissions().mode() & 0o022 == 0
    })
}

/// This process's effective uid, without `libc`. `/proc/self/status` carries
/// it, and td-svc already depends on `/proc` for everything else it knows
/// about processes (I3).
fn nix_getuid() -> u32 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        // Unreadable /proc: assume nothing matches, so a pre-existing directory
        // is refused rather than adopted.
        return u32::MAX;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(effective) = rest.split_whitespace().nth(1) {
                return effective.parse().unwrap_or(u32::MAX);
            }
        }
    }
    u32::MAX
}

/// Serve one connection: read a line, ask the main loop, write the reply.
///
/// A read or write error ends this connection and nothing else. The supervisor
/// must not fail because a client hung up mid-request.
fn serve(stream: UnixStream, tx: &Sender<Event>) {
    let Ok(write_half) = stream.try_clone() else {
        log("control: cannot duplicate a client connection");
        return;
    };
    if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        log(&format!("control: cannot bound a client read: {e}"));
        return;
    }
    let mut reader = BufReader::new(stream.take(MAX_REQUEST));
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            log(&format!("control: reading a request: {e}"));
            respond(write_half, "error: could not read the request\n");
            return;
        }
    }
    if !line.ends_with('\n') {
        // The cap was hit, or the client half-closed mid-line. Either way this
        // is not a request, and truncating it into one could run a DIFFERENT
        // verb than the client sent.
        respond(write_half, "error: request too long or incomplete\n");
        return;
    }
    let (reply_tx, reply_rx): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
    let request = line.trim().to_string();
    if tx
        .send(Event::Control {
            request,
            reply: reply_tx,
        })
        .is_err()
    {
        // The main loop is gone. Nothing can answer, and saying so beats
        // leaving the client on a socket that will never speak.
        respond(write_half, "error: supervisor is not accepting requests\n");
        return;
    }
    match reply_rx.recv_timeout(REPLY_TIMEOUT) {
        Ok(reply) => respond(write_half, &reply),
        Err(_) => respond(write_half, "error: supervisor did not answer in time\n"),
    }
}

fn respond(mut stream: UnixStream, reply: &str) {
    // The last unbounded wait on the accept thread. A client that sends a
    // request and then never reads blocks `write_all` once the reply outgrows
    // the socket buffer, and connections are served one at a time — so this is
    // the same argument as READ_TIMEOUT, at the other end of the exchange.
    if let Err(e) = stream.set_write_timeout(Some(WRITE_TIMEOUT)) {
        log(&format!("control: cannot bound a client write: {e}"));
        return;
    }
    if let Err(e) = stream.write_all(reply.as_bytes()) {
        log(&format!("control: writing a reply: {e}"));
    }
}

/// The client side: connect, send one request, read the whole reply.
///
/// Shutting down the write half is what ends the request — the server reads a
/// LINE, so a client that holds the connection open would be answered anyway,
/// but half-closing means the read below cannot block on a peer that has
/// nothing more to say.
pub fn ask(path: &str, request: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| {
        // The overwhelmingly likely cause is "no supervisor is running", and an
        // operator reading a bare ENOENT would go looking for a missing file.
        format!("{path}: {e}; is td-svc running?")
    })?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|e| format!("{path}: sending the request: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("{path}: {e}"))?;
    // The server's own wait is bounded, but nothing bounds a server that
    // accepted and then stopped writing. A control tool that hangs when the
    // supervisor is wedged is the least useful thing it could do.
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .map_err(|e| format!("{path}: {e}"))?;
    let mut reply = String::new();
    Read::by_ref(&mut stream)
        .take(MAX_REPLY)
        .read_to_string(&mut reply)
        .map_err(|e| format!("{path}: reading the reply: {e}"))?;
    Ok(reply)
}

/// Accept and serve until the listener dies. Split from `spawn` so a test can
/// drive the real loop on a scratch socket — the shipped path is under `/run`,
/// which only root can create in, so a `spawn` that binds it internally is a
/// loop nothing can exercise.
fn serve_forever(listener: UnixListener, tx: &Sender<Event>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => serve(stream, tx),
            // One refused connection is not a reason to stop accepting.
            Err(e) => log(&format!("control: accept: {e}")),
        }
    }
}

/// Start the accept loop. Failure to bind is reported and NOT fatal: a
/// supervisor with no control socket still supervises, and the console is worth
/// more than the socket.
pub fn spawn(tx: Sender<Event>) {
    let listener = match bind(DIR, PATH) {
        Ok(listener) => listener,
        Err(e) => {
            log(&format!("control: {PATH}: {e}; running without it"));
            return;
        }
    };
    // `std::thread::spawn` PANICS when the thread cannot be created, and the
    // release profile is `panic=abort` — so a resource limit would take the
    // supervisor down rather than cost it a socket.
    if std::thread::Builder::new()
        .name("td-svc-control".into())
        .spawn(move || serve_forever(listener, &tx))
        .is_err()
    {
        log("control: cannot start the accept thread; running without the socket");
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
    use super::*;
    use std::io::Read;
    use std::sync::mpsc::channel;

    fn scratch(name: &str) -> (String, String) {
        let dir = format!(
            "{}/td-svc-control-{}-{}",
            std::env::temp_dir().display(),
            std::process::id(),
            name
        );
        let path = format!("{dir}/control");
        let _ = std::fs::remove_dir_all(&dir);
        (dir, path)
    }

    /// A socket left behind by a supervisor that DIED must not stop the next
    /// one binding. PID 1 respawns td-svc, so this is the ordinary case after a
    /// crash — refusing here would cost the socket until the next reboot.
    ///
    /// Dropping the listener is what a dead supervisor leaves: the fd closes,
    /// the filesystem entry stays. An earlier version of this test used
    /// `mem::forget`, which keeps the listener OPEN — so it was asserting that
    /// a LIVE supervisor's socket gets stolen, the exact bug below.
    #[test]
    fn a_stale_socket_from_a_dead_supervisor_is_replaced() {
        let (dir, path) = scratch("stale");
        let first = bind(&dir, &path).unwrap();
        drop(first);
        assert!(
            Path::new(&path).exists(),
            "dropping a listener should leave the path behind; the test premise is gone"
        );
        let second = bind(&dir, &path);
        assert!(
            second.is_ok(),
            "rebinding over a dead supervisor's socket failed: {:?}",
            second.err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ...and a socket a LIVE supervisor is serving must NOT be taken.
    ///
    /// `0700` keeps out other users, not a second root td-svc. Unlinking
    /// unconditionally would make the first supervisor unreachable while both
    /// went on supervising, so every later `stop` would address the wrong one —
    /// and nothing would say so.
    #[test]
    fn a_live_supervisors_socket_is_not_stolen() {
        let (dir, path) = scratch("live");
        let live = bind(&dir, &path).unwrap();
        // Someone is accepting on it, exactly as a running supervisor is.
        let second = bind(&dir, &path);
        assert!(
            second.is_err(),
            "a second supervisor took a live socket out from under the first"
        );
        let e = second.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            e.contains("live supervisor"),
            "the refusal must say why, got {e:?}"
        );
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The directory must be private from the instant it exists, not from
    /// shortly after. `create_dir_all` uses `0777 & ~umask`, and td-svc
    /// inherits PID 1's umask, which PID 1 never sets — so a
    /// create-then-narrow leaves a window in which anything can open a
    /// descriptor to it and keep traversal rights forever after.
    #[test]
    fn the_directory_is_never_briefly_world_accessible() {
        let (dir, path) = scratch("atomic");
        // Prove the mode is applied by mkdir(2) itself under a permissive
        // umask, which is the condition the boot actually runs under.
        let listener = bind(&dir, &path).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, DIR_MODE,
            "the control directory is {mode:o}, not {DIR_MODE:o}"
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A client that connects and never finishes its request must not hold the
    /// accept thread, which serves connections one at a time. The bound is on
    /// the READ, so it applies before any request exists to answer.
    #[test]
    fn an_incomplete_request_is_refused_rather_than_held() {
        let (dir, path) = scratch("partial");
        let listener = bind(&dir, &path).unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                serve(stream, &tx);
            }
        });

        let mut client = UnixStream::connect(&path).unwrap();
        // No newline, then half-close: a truncated request, not a request.
        client.write_all(b"stat").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = String::new();
        BufReader::new(client).read_line(&mut got).unwrap();
        assert!(
            got.starts_with("error:"),
            "a truncated request was answered {got:?}; truncating it into a request \
             could run a different verb than the client sent"
        );
        // ...and nothing reached the loop, so no verb ran.
        assert!(rx.try_recv().is_err(), "a truncated request reached the loop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A DANGLING symlink at the socket path must not block the socket.
    ///
    /// `path.exists()` follows symlinks, so a link pointing nowhere reported
    /// "absent", the unlink was skipped, and `bind` then failed against it —
    /// no control socket until the next reboot, for a file that could simply
    /// have been removed. Connecting through such a link reports ENOENT rather
    /// than a refusal, so treating only a refusal as proof of a corpse leaves
    /// exactly the same hole; both arms are needed and this reds without either.
    #[test]
    fn a_dangling_symlink_at_the_socket_path_is_cleared() {
        let (dir, path) = scratch("dangling");
        std::fs::DirBuilder::new()
            .mode(DIR_MODE)
            .recursive(true)
            .create(&dir)
            .unwrap();
        if std::os::unix::fs::symlink(format!("{dir}/nothing-here"), &path).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let listener = bind(&dir, &path);
        assert!(
            listener.is_ok(),
            "a dangling symlink kept the control socket from binding: {:?}",
            listener.err()
        );
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A client that connects and says NOTHING must be released, not held.
    ///
    /// This is the case `READ_TIMEOUT` exists for, and the one the truncated-
    /// request test does not reach: half-closing ends `read_line` at EOF, so
    /// the timeout never runs. Connections are served one at a time, so a
    /// silent client that is never released takes the control socket with it —
    /// and it is wanted most when something is already wrong. Deleting
    /// `set_read_timeout` outright left the suite green before this.
    #[test]
    fn a_client_that_sends_nothing_does_not_hold_the_accept_thread() {
        let (dir, path) = scratch("silent");
        let listener = bind(&dir, &path).unwrap();
        let (tx, _rx) = channel();
        let (done_tx, done_rx) = channel();
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                serve(stream, &tx);
            }
            let _ = done_tx.send(());
        });

        // Connect, then hold the connection open and send nothing at all.
        let client = UnixStream::connect(&path).unwrap();
        let released = done_rx
            .recv_timeout(READ_TIMEOUT.saturating_mul(3))
            .is_ok();
        drop(client);
        assert!(
            released,
            "serve never returned for a silent client, so one connection that \
             sends nothing takes the whole control socket down"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The request cap binds, and binds without a newline ever arriving.
    ///
    /// `read_line` on an unbounded stream is an unbounded allocation inside
    /// PID 1's only supervisor. Raising `MAX_REQUEST` to `u64::MAX` left the
    /// suite green before this: the truncated-request test half-closes after
    /// four bytes, so nothing ever reached the cap.
    #[test]
    fn a_request_past_the_cap_is_refused() {
        let (dir, path) = scratch("toolong");
        let listener = bind(&dir, &path).unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                serve(stream, &tx);
            }
        });

        let mut client = UnixStream::connect(&path).unwrap();
        // Well past the cap, and no newline anywhere in it.
        let flood = vec![b'x'; usize::try_from(MAX_REQUEST).unwrap() * 2];
        // The peer stops reading at the cap, so a short write is the expected
        // outcome rather than a failure of the test.
        let _ = client.write_all(&flood);
        let mut got = String::new();
        let _ = BufReader::new(client).read_line(&mut got);
        assert!(
            got.starts_with("error:"),
            "a request past the cap was answered {got:?} instead of refused"
        );
        assert!(
            rx.try_recv().is_err(),
            "an over-long request reached the loop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `bind` refuses a path that is not a directory it owns.
    ///
    /// `0700` keeps out other users, not another root process, so the checks
    /// on a PRE-EXISTING path are the whole barrier — and replacing the owner
    /// check with `if false` left the suite green before this.
    #[test]
    fn bind_refuses_a_directory_it_did_not_make() {
        let (dir, path) = scratch("notadir");
        // A plain file where the directory belongs. `create` reports
        // AlreadyExists for it, so this is exactly the branch that then has to
        // decide whether to adopt what it found.
        std::fs::write(&dir, b"not a directory").unwrap();
        let err = bind(&dir, &path).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "bind adopted a non-directory at its socket path: {err}"
        );
        let _ = std::fs::remove_file(&dir);
    }

    /// A request reaches the main loop and its reply reaches the client. The
    /// point is the round trip through the channel, not the verb.
    #[test]
    fn a_request_crosses_to_the_loop_and_the_reply_comes_back() {
        let (dir, path) = scratch("roundtrip");
        let listener = bind(&dir, &path).unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                serve(stream, &tx);
            }
        });

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"status\n").unwrap();
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let Event::Control { request, reply } = event else {
            unreachable!("expected a control event")
        };
        assert_eq!(request, "status");
        reply.send("greeter ready pid=42 failures=0\n".into()).unwrap();

        let mut got = String::new();
        BufReader::new(client).read_line(&mut got).unwrap();
        assert_eq!(got, "greeter ready pid=42 failures=0\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A supervisor that is not accepting requests must ANSWER, not hang up
    /// silently. The operator reaching for this socket is usually doing so
    /// BECAUSE something is wrong, and a control tool that returns nothing
    /// tells them less than the error does.
    #[test]
    fn a_client_is_told_when_the_loop_is_not_accepting_requests() {
        let (dir, path) = scratch("gone");
        let listener = bind(&dir, &path).unwrap();
        let (tx, rx) = channel();
        // Exactly what a dead main loop looks like from this thread.
        drop(rx);
        std::thread::spawn(move || {
            if let Some(Ok(stream)) = listener.incoming().next() {
                serve(stream, &tx);
            }
        });

        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"status\n").unwrap();
        let mut got = String::new();
        BufReader::new(client).read_line(&mut got).unwrap();
        assert!(
            got.starts_with("error: supervisor is not accepting requests"),
            "a client of a dead supervisor got {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole path, once: a real `Runtime`, a real socket, a real client.
    ///
    /// Every other test here stubs one end. This one asserts that a request
    /// typed at the socket reaches supervision state and comes back with what
    /// that state actually says — which is the only claim the socket makes.
    #[test]
    fn a_real_runtime_answers_a_real_client_over_a_real_socket() {
        use crate::supervise::Runtime;
        use crate::table::parse;

        let (dir, path) = scratch("e2e");
        let listener = bind(&dir, &path).unwrap();
        let (units, problems) = parse(
            "[alpha]\ntype=oneshot\nexec=/bin/true\ntimeout=30\n\
             [beta]\ntype=daemon\nexec=/bin/true\nafter=alpha\nrestart=always\n",
        );
        assert!(problems.is_empty(), "{problems:?}");
        let (mut rt, complaints) = Runtime::new(units, &path);
        assert!(complaints.is_empty(), "{complaints:?}");

        let tx = rt.events();
        std::thread::spawn(move || serve_forever(listener, &tx));

        let ask = |request: &str| {
            let mut client = UnixStream::connect(&path).unwrap();
            client.write_all(format!("{request}\n").as_bytes()).unwrap();
            client
        };

        // The request has to be pumped by the loop's own dispatch, exactly as
        // `run` does it — the point is that nothing else touches the state.
        let client = ask("status");
        let event = rt.next_event(Duration::from_secs(5)).unwrap();
        rt.dispatch(event);
        let mut got = String::new();
        BufReader::new(client).read_to_string(&mut got).unwrap();
        assert!(
            got.contains("alpha down") && got.contains("beta down"),
            "status returned {got:?}"
        );

        // ...and a request that CHANGES state is visible in the next answer.
        let client = ask("stop alpha");
        let event = rt.next_event(Duration::from_secs(5)).unwrap();
        rt.dispatch(event);
        let mut got = String::new();
        BufReader::new(client).read_to_string(&mut got).unwrap();
        assert!(got.contains("was not running"), "stop returned {got:?}");

        let client = ask("status alpha");
        let event = rt.next_event(Duration::from_secs(5)).unwrap();
        rt.dispatch(event);
        let mut got = String::new();
        BufReader::new(client).read_to_string(&mut got).unwrap();
        assert!(
            got.starts_with("alpha stopped"),
            "the stop did not reach supervision state: {got:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ...and when it is merely SLOW, the wait is bounded. Asserted on the
    /// constant rather than by sleeping through it: a test that waits out a
    /// real timeout costs that long on every run, which is how a suite stops
    /// being run.
    #[test]
    fn the_wait_for_a_reply_is_bounded() {
        assert!(
            REPLY_TIMEOUT > Duration::ZERO && REPLY_TIMEOUT <= Duration::from_secs(30),
            "REPLY_TIMEOUT is {REPLY_TIMEOUT:?}; it must be finite, and short enough \
             that a wedged supervisor gives an operator a diagnostic rather than a \
             hung shell"
        );
    }
}
