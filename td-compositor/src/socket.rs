use std::fs::{self, Permissions};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub fn remove_stale(path: &Path, kind: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(path) {
            Ok(_) => Err(format!(
                "refusing to replace live {kind} socket {}",
                path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                fs::remove_file(path)
                    .map_err(|e| format!("remove stale {kind} socket {}: {e}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "probe existing {kind} socket {}: {error}",
                path.display()
            )),
        },
        Ok(_) => Err(format!(
            "refusing to replace non-socket {kind} path {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("stat {kind} socket {}: {error}", path.display())),
    }
}

/// Private to the user the service runs as. Both readiness sockets are the
/// same secret: that this machine has a graphical session up.
const READY_MODE: u32 = 0o600;

/// Consecutive failed accepts before the listener gives up. Generous, because
/// every retry is cheap and the alternative to retrying is a service that
/// reports itself down forever.
const MAX_ACCEPT_FAILURES: u32 = 64;

/// A bound on answering ONE caller, so a caller that never reads cannot stop
/// the socket answering anyone else.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// A bound readiness socket. Dropping it unlinks the path, so a program that
/// exits leaves nothing for the next one to refuse.
///
/// It does NOT retire the listener: that thread owns the descriptor and parks
/// in `accept`, which safe `std` cannot interrupt any more than it can a PTY
/// reader's `read`. Its only retirement is process exit — sound because these
/// are one process per session, and closing the session IS exiting. The
/// consequence is a contract rather than a mechanism: nothing may join it.
pub struct Published {
    path: PathBuf,
}

impl Drop for Published {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Bind a mode-0600 readiness socket that answers every caller with `answer`.
///
/// ONE publisher for both personalities. The demo answers nothing and the
/// terminal answers its grid, and an empty answer is not a special case — it
/// is a `write_all` of no bytes, so accept-and-drop falls out of the same
/// path rather than being a second copy of it.
/// `thread_name` is `'static` rather than `&str` because it is not runtime
/// data: `thread::Builder::name` panics at spawn on an interior NUL, and this
/// crate does not panic. Both callers pass a literal, and the signature is
/// what keeps it that way.
pub fn publish(
    path: &Path,
    thread_name: &'static str,
    answer: Vec<u8>,
) -> Result<Published, String> {
    publish_inner(path, thread_name, answer, None)
}

/// Publish an answer that becomes stale when the observed resource departs.
/// `live` is latched false by that resource's owner; later callers receive EOF
/// rather than evidence produced by a connection which no longer exists.
pub fn publish_while(
    path: &Path,
    thread_name: &'static str,
    answer: Vec<u8>,
    live: Arc<AtomicBool>,
) -> Result<Published, String> {
    publish_inner(path, thread_name, answer, Some(live))
}

fn publish_inner(
    path: &Path,
    thread_name: &'static str,
    answer: Vec<u8>,
    live: Option<Arc<AtomicBool>>,
) -> Result<Published, String> {
    remove_stale(path, "readiness")?;
    let listener = UnixListener::bind(path)
        .map_err(|e| format!("bind readiness socket {}: {e}", path.display()))?;
    // The guard is taken HERE, one line after the socket exists, so every `?`
    // below unlinks it on the way out. Hand-written cleanup at each failure
    // was a branch no test could reach — nothing after a successful bind can
    // be made to fail from inside this process — so it was a leak waiting for
    // whichever failure got added next without one.
    let published = Published {
        path: path.to_path_buf(),
    };
    fs::set_permissions(path, Permissions::from_mode(READY_MODE))
        .map_err(|e| format!("chmod readiness socket {}: {e}", path.display()))?;
    thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || serve(listener.incoming(), &answer, live.as_deref()))
        .map_err(|e| format!("start readiness listener {}: {e}", path.display()))?;
    Ok(published)
}

/// One failed `accept` is not the end of the socket. `ECONNABORTED` is a
/// caller that hung up between connecting and being accepted, and `EMFILE` is
/// the process briefly out of descriptors; retiring on either would leave a
/// healthy service unable to answer for the rest of its life, which is the
/// failure this whole socket exists to report. A run of them IS terminal
/// though — a listener that can no longer accept anything would otherwise spin
/// here forever — so the run is bounded rather than the first one.
fn serve(
    connections: impl Iterator<Item = std::io::Result<UnixStream>>,
    answer: &[u8],
    live: Option<&AtomicBool>,
) -> AcceptOutcome {
    let mut consecutive = 0;
    for connection in connections {
        let Ok(mut connection) = connection else {
            consecutive += 1;
            if consecutive > MAX_ACCEPT_FAILURES {
                return AcceptOutcome::GaveUp;
            }
            continue;
        };
        consecutive = 0;
        if live.is_some_and(|live| !live.load(Ordering::Acquire)) {
            continue;
        }
        // A caller that hung up mid-answer is the caller's business — but a
        // caller that never READS would park this thread in `write` and stop
        // every later probe, so the wait is bounded even though an answer is
        // far too small to fill a socket buffer today.
        let _ = connection.set_write_timeout(Some(ANSWER_TIMEOUT));
        let _ = connection.write_all(answer);
    }
    AcceptOutcome::Exhausted
}

/// How the accept loop ended. The live listener never ends, so this exists to
/// make the two endings distinguishable to a test — a run of failures long
/// enough to give up, versus a caller list that simply ran out.
#[derive(Debug, Eq, PartialEq)]
enum AcceptOutcome {
    GaveUp,
    Exhausted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A caller whose reads are BOUNDED. Every read below is against the live
    /// listener thread and `cargo test` has no per-test timeout, so a
    /// publisher that stopped answering would hang the gate rather than
    /// redden it.
    fn caller(path: &Path) -> UnixStream {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        stream
    }

    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "td-publish-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn an_empty_answer_is_accept_and_drop_rather_than_a_second_path() {
        let directory = scratch("empty");
        let path = directory.join("ready");
        let published = publish(&path, "test-ready", Vec::new()).unwrap();
        // The literal the design specifies, not the constant under test.
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut stream = caller(&path);
        let mut said = Vec::new();
        stream.read_to_end(&mut said).unwrap();
        assert!(said.is_empty(), "the demo's socket said something");

        drop(published);
        assert!(!path.exists());
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn an_answer_reaches_every_caller_not_just_the_first() {
        let directory = scratch("answer");
        let path = directory.join("ready");
        let published = publish(&path, "test-ready", b"hello\n".to_vec()).unwrap();
        for _ in 0..3 {
            let mut stream = caller(&path);
            let mut said = String::new();
            stream.read_to_string(&mut said).unwrap();
            assert_eq!(said, "hello\n");
        }
        drop(published);
        assert!(!path.exists());
        fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn a_caller_that_hangs_up_first_does_not_stop_the_next_one() {
        let directory = scratch("aborted");
        let path = directory.join("ready");
        let published = publish(&path, "test-ready", b"still here\n".to_vec()).unwrap();
        for _ in 0..8 {
            drop(UnixStream::connect(&path).unwrap());
        }
        let mut stream = caller(&path);
        let mut said = String::new();
        stream.read_to_string(&mut said).unwrap();
        assert_eq!(said, "still here\n", "the socket stopped answering");
        drop(published);
        fs::remove_dir(&directory).unwrap();
    }

    fn aborted() -> std::io::Result<UnixStream> {
        Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted))
    }

    /// The accept loop takes an ITERATOR rather than the listener, because a
    /// failed accept is what has to be survived and no in-process test can
    /// make a real listener produce one.
    #[test]
    fn a_run_of_failed_accepts_is_survived_but_not_an_endless_one() {
        let (caller, mut peer) = UnixStream::pair().unwrap();
        let refused = (0..MAX_ACCEPT_FAILURES).map(|_| aborted());
        let outcome = serve(
            refused.chain(std::iter::once(Ok(caller))),
            b"answered\n",
            None,
        );
        assert_eq!(outcome, AcceptOutcome::Exhausted);
        let mut said = String::new();
        peer.read_to_string(&mut said).unwrap();
        assert_eq!(said, "answered\n", "a failed accept ate the next caller");

        // One more than the bound, and it gives up rather than spinning on a
        // listener that will never accept anything again.
        let (caller, mut peer) = UnixStream::pair().unwrap();
        let refused = (0..=MAX_ACCEPT_FAILURES).map(|_| aborted());
        let outcome = serve(
            refused.chain(std::iter::once(Ok(caller))),
            b"answered\n",
            None,
        );
        assert_eq!(outcome, AcceptOutcome::GaveUp);
        let mut said = String::new();
        peer.read_to_string(&mut said).unwrap();
        assert!(said.is_empty(), "a retired loop answered anyway");
    }

    /// A success resets the run, so a socket that has been up for a long time
    /// is not retired by scattered failures that never happened together.
    #[test]
    fn a_served_caller_resets_the_failure_run() {
        let mut connections: Vec<std::io::Result<UnixStream>> = Vec::new();
        let mut peers = Vec::new();
        for _ in 0..3 {
            for _ in 0..MAX_ACCEPT_FAILURES {
                connections.push(aborted());
            }
            let (caller, peer) = UnixStream::pair().unwrap();
            connections.push(Ok(caller));
            peers.push(peer);
        }
        let outcome = serve(connections.into_iter(), b"answered\n", None);
        assert_eq!(outcome, AcceptOutcome::Exhausted);
        for mut peer in peers {
            let mut said = String::new();
            peer.read_to_string(&mut said).unwrap();
            assert_eq!(said, "answered\n");
        }
    }

    #[test]
    fn a_latched_departure_withholds_the_old_answer() {
        let directory = scratch("latched");
        let path = directory.join("ready");
        let live = Arc::new(AtomicBool::new(true));
        let published = publish_while(
            &path,
            "latched-ready",
            b"same-client\n".to_vec(),
            Arc::clone(&live),
        )
        .unwrap();

        let mut first = caller(&path);
        let mut said = String::new();
        first.read_to_string(&mut said).unwrap();
        assert_eq!(said, "same-client\n");

        live.store(false, Ordering::Release);
        let mut after = caller(&path);
        said.clear();
        after.read_to_string(&mut said).unwrap();
        assert!(said.is_empty(), "a departed resource retained readiness");

        drop(published);
        fs::remove_dir(&directory).unwrap();
    }

    /// The name is what an operator reading `ps -T` sees, so it has to reach
    /// the KERNEL rather than just the builder — dropping the `.name(...)`
    /// call leaves everything else here green. Bounded and polled because std
    /// names the thread from inside it: `publish` can return first.
    #[test]
    fn the_listener_thread_carries_the_name_it_was_published_under() {
        let directory = scratch("named");
        let path = directory.join("ready");
        let published = publish(&path, "named-ready", Vec::new()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut named = false;
        while !named && std::time::Instant::now() < deadline {
            for task in fs::read_dir("/proc/self/task").unwrap() {
                let comm = task.unwrap().path().join("comm");
                named |=
                    fs::read_to_string(&comm).is_ok_and(|name| name.trim_end() == "named-ready");
            }
            if !named {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(named, "the readiness listener is running unnamed");
        drop(published);
        fs::remove_dir(&directory).unwrap();
    }

    /// The demo's answer is the empty one. Nothing about `socket::publish`
    /// makes that true — it is the demo's own call — and a non-empty answer
    /// there would leave every test here green while the demo started
    /// replying to a probe that reads nothing.
    #[test]
    fn the_demo_publishes_an_answer_of_no_bytes() {
        let client = include_str!("client.rs");
        assert!(
            client
                .contains(r#"socket::publish(&options.ready_socket, "ui-demo-ready", Vec::new())"#),
            "the demo's readiness call is not the empty answer"
        );
    }
}
