//! What every td D-Bus client repeats before its first message: the one
//! socket a `unix:path=` address names, a connect with a bound, and the
//! SASL EXTERNAL opening. td-busd's probe declares this module; td-open,
//! td-portal and td-secret compile the same file by `#[path]` beside the
//! shared codec (APPLICATIONS.md §E), as td-jail's broker client by design
//! does not (its module doc). Std only, and no raw surface of its own.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// The one absolute socket path a `unix:path=` bus address names: no
/// second address (`;`), no key beyond the path (`,`), no escape (`%`) and
/// no NUL. `None` for anything else, which no td client tries to interpret.
pub fn unix_path(address: &str) -> Option<&str> {
    address
        .strip_prefix("unix:path=")
        .filter(|path| path.starts_with('/') && !path.contains([';', ',', '%', '\0']))
}

/// The SASL opening for `uid`: the NUL the transport begins with, then
/// `AUTH EXTERNAL` with the uid's decimal digits as lowercase hex, as the
/// D-Bus specification spells the identity.
pub fn auth_line(uid: u32) -> String {
    let identity: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\0AUTH EXTERNAL {identity}\r\n")
}

/// `UnixStream::connect(path)` with a bound, through `connect_by`.
pub fn connect_within(path: &Path, timeout: Duration, thread: &str) -> io::Result<UnixStream> {
    let owned = path.to_path_buf();
    connect_by(timeout, thread, move || UnixStream::connect(owned))
}

/// `connect` run on a helper thread named `thread`, waited for at most
/// `timeout`; a timeout is `TimedOut`.
///
/// `connect(2)` on a unix socket whose accept queue is full blocks against
/// the connecting socket's `SO_SNDTIMEO`, which is unset until after the
/// connect returns, so no socket option can bound it and the thread must.
/// A connect still pending at the timeout is left to the thread: after a
/// timeout nobody receives, and a stream that arrives late is dropped with
/// the channel, which closes it. Each attempt that times out so leaves one
/// thread blocked until its connect returns or the process exits; a caller
/// that retries must bound its retries for that reason.
///
/// The connect is an argument so a test can prove the bound against a
/// connect that is defined not to return, rather than by filling a listen
/// backlog whose depth belongs to the host.
pub fn connect_by<F>(timeout: Duration, thread: &str, connect: F) -> io::Result<UnixStream>
where
    F: FnOnce() -> io::Result<UnixStream> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(thread.to_owned())
        .spawn(move || {
            let _ = sender.send(connect());
        })
        .map_err(|error| io::Error::other(format!("cannot start the connect thread: {error}")))?;
    match receiver.recv_timeout(timeout) {
        Ok(outcome) => outcome,
        // The listener exists, or the connect would have been refused at
        // once, and is not accepting: a different fault from a bus that
        // accepts and then says nothing.
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the bus did not accept in time",
        )),
        // The thread ended without sending: only a panic in `connect`.
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
            "the connect thread ended without an answer",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_absolute_unescaped_path_is_a_bus_socket() {
        assert_eq!(
            unix_path("unix:path=/run/user/1000/bus"),
            Some("/run/user/1000/bus")
        );
        for refused in [
            "",
            "unix:path=",
            "unix:path=run/bus",
            "unix:path=/run/bus;unix:path=/other",
            "unix:path=/run/bus,guid=00",
            "unix:path=/run/b%75s",
            "unix:path=/run/bus\0",
            "unix:abstract=/run/bus",
            "tcp:host=localhost",
        ] {
            assert_eq!(unix_path(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn the_auth_line_spells_the_uid_digits_in_hex() {
        assert_eq!(auth_line(0), "\0AUTH EXTERNAL 30\r\n");
        assert_eq!(auth_line(1000), "\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(
            auth_line(u32::MAX),
            "\0AUTH EXTERNAL 34323934393637323935\r\n"
        );
    }

    #[test]
    fn a_refused_connect_says_why_and_a_missing_socket_is_not_a_timeout() {
        let error = connect_within(
            Path::new("/nonexistent/td-bus-client"),
            Duration::from_secs(10),
            "td-bus-client-test",
        )
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::NotFound));
    }

    /// A connect that never returns is given up on at the bound, as
    /// `TimedOut`, and one that dies without an answer is not a timeout.
    #[test]
    fn a_connect_that_never_returns_times_out_and_a_dead_one_does_not() {
        let (_hold, blocked) = mpsc::channel::<()>();
        let error = connect_by(Duration::from_millis(50), "td-bus-client-test", move || {
            let _ = blocked.recv();
            Err(io::Error::other("released"))
        })
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::TimedOut));
        let error = connect_by(Duration::from_secs(10), "td-bus-client-test", || {
            panic!("the connect thread dies")
        })
        .err()
        .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::Other));
    }
}
