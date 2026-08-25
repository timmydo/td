#![deny(unsafe_code)]

//! td-busd — td's session D-Bus broker.
//!
//! The wire format, the `EXTERNAL` handshake, and — since surface #10 — the
//! socket they happen on. `run` binds the session bus and serves it; `probe` is
//! what `APPLICATIONS.md` §A's `ready=` line calls to decide the bus is up.
//!
//! Rung 14's first half is here: a connection says `Hello`, earns a `:1.N`,
//! and is addressable by it. A call naming a connection that exists is RELAYED
//! to it with the broker's own `SENDER` stamped on; one addressed to the bus is
//! answered by the bus — `Hello`, `Ping`, `GetId`, the name lookups and the
//! credential lookups; one naming a name nobody owns comes back
//! `NameHasNoOwner`. Well-known names and match rules are the second half, so
//! `RequestName` and `AddMatch` are `UnknownMethod` until then and nothing on
//! this bus broadcasts.
//!
//! Rung 15's first increment is here too, on td's own interface rather than
//! the specification's: `td.Jail1.Register` and `td.Jail1.Complete` at
//! `/td/Jail1` record which jailed instance a process belongs to, and
//! `lineage` answers that question for a connection. Nothing consults the
//! answer to decide anything yet.
//!
//! What no call gets is silence. A caller waiting on a serial that will never
//! be answered hangs rather than fails, so a call this broker cannot serve is
//! ANSWERED with an error saying so rather than dropped. A signal or a reply
//! is consumed without one, because nothing is waiting on it and the
//! specification reserves replies for calls. A CALL marked
//! `NO_REPLY_EXPECTED` is consumed without a reply too — but the two
//! registration methods still do their work, because the flag withdraws the
//! answer and not the request.

mod auth;
mod authscript;
mod corpus;
mod lineage;
mod message;
mod name;
mod policy;
mod recorded;
mod registry;
mod sys;
mod transport;
mod wire;

use std::env;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::thread;

fn usage() -> String {
    "usage: td-busd selftest | run --socket PATH | probe PATH".into()
}

/// How long to wait after a failed accept before trying again, and how many
/// failures in a row end the process. Together they bound a standing failure
/// to a few seconds of backoff rather than an unbounded spin.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_ACCEPT_FAILURES: usize = 32;

/// How many refusals in a row share one journal line. A refusal is cheap to
/// provoke — connect, be over the share, be closed — so saying every one is a
/// log flood a peer can write on demand.
const REFUSALS_PER_LINE: usize = 64;

/// This process's uid, without a syscall of its own: procfs owns `/proc/self`
/// as the process does. `getuid(2)` would be a fourth entry on surface #10 for
/// a number a `stat` already answers.
fn current_uid() -> Result<u32, String> {
    let entry = std::fs::metadata("/proc/self")
        .map_err(|error| format!("cannot read /proc/self: {error}"))?;
    Ok(std::os::unix::fs::MetadataExt::uid(&entry))
}

/// Bind the bus and serve it. Never returns while the listener is good: a
/// broker that exited zero having served nothing would satisfy its supervision
/// unit and leave every portal call hanging.
fn run(socket: &Path) -> Result<String, String> {
    let bound = transport::bind(socket)
        .map_err(|error| format!("cannot listen on {}: {error}", socket.display()))?;
    let text = transport::guid_text().map_err(|error| format!("cannot make a guid: {error}"))?;
    // Validated once HERE, so a guid this bus cannot use fails at startup
    // rather than once per peer in a thread whose failure nobody is reading.
    auth::Guid::new(&text).map_err(|error| format!("bad guid: {error:?}"))?;
    eprintln!(
        "td-busd: listening on {} as {text}",
        bound.path().display()
    );

    // The quota is shared with every connection thread and outlives this
    // function's frame, so it is an `Arc` rather than a borrow. Same for the
    // guid text, which each thread re-validates into a `Guid` of its own.
    let quota = Arc::new(transport::Quota::new());
    let bus = Arc::new(registry::Bus::new());
    // Every jail instance this broker knows. Shared with every connection
    // thread: the registration methods write it and every accept reads it.
    let instances = Arc::new(lineage::Instances::new());
    let guid_text = Arc::new(text);
    // Consecutive failed accepts. Reset by any success, so a busy bus that
    // sheds the occasional peer never approaches the ceiling.
    let mut failures = 0usize;
    // Consecutive refusals, for the log rate below. Reset by any admission.
    let mut refusals = 0usize;

    // DETACHED threads, not `thread::scope`. A scope joins every thread it
    // spawned before it returns, and a connection thread returns only when its
    // peer leaves — so the give-up below, written inside a scope, would wait
    // for the silent peers whose existence is the reason it is giving up. The
    // threshold would be reached and the process would hang anyway, which is
    // the failure it was added to prevent.
    for peer in bound.listener().incoming() {
        let stream = match peer {
            Ok(stream) => {
                failures = 0;
                stream
            }
            // One failed accept is not a failed bus: a peer that hung up
            // between connect and accept is ordinary. A STANDING one is a
            // different thing, and logging-and-continuing is a livelock:
            // `EMFILE` does not dequeue the pending connection, so the next
            // `accept` fails on the same peer immediately, at one journal line
            // per iteration and a core at 100% — for ever, since `incoming()`
            // never ends. Back off, and give up before the log is the only
            // thing on the disk. Giving up is right rather than defeatist:
            // `td-svc` restarts this, and a broker that cannot accept is one
            // no new client can reach however long it spins.
            Err(error) => {
                failures += 1;
                eprintln!("td-busd: accept: {error}");
                if failures >= MAX_ACCEPT_FAILURES {
                    return Err(format!("accept failed {failures} times running: {error}"));
                }
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        // Who this is, before deciding whether there is room for them: §D's
        // ceiling has a per-peer half, and the peer is the kernel's answer
        // rather than anything the connection has said yet.
        let credential = match transport::peer_of(&stream) {
            Ok(credential) => credential,
            Err(error) => {
                eprintln!("td-busd: cannot identify a peer: {error}");
                continue;
            }
        };
        let admitted = match quota.try_admit(credential.pid) {
            Ok(admitted) => {
                refusals = 0;
                admitted
            }
            // Refusing is a CLOSE: there is nothing to say before a handshake,
            // and a peer left holding an accepted socket that nobody will ever
            // read is worse off than one told nothing.
            Err(why) => {
                // A peer that reconnects in a loop against a full share would
                // otherwise write one journal line per connect — the same
                // flood the accept backoff above exists to stop, arriving
                // through the door that IS working. Say it, then say it
                // rarely.
                refusals += 1;
                if refusals == 1 || refusals.is_multiple_of(REFUSALS_PER_LINE) {
                    eprintln!(
                        "td-busd: refused pid {} ({refusals} in a row): {why}",
                        credential.pid
                    );
                }
                drop(stream);
                continue;
            }
        };
        let text = Arc::clone(&guid_text);
        let directory = Arc::clone(&bus);
        let registered = Arc::clone(&instances);
        let spawned = thread::Builder::new().spawn(move || {
            // `admitted` is moved in, so this peer's place in the quota is
            // given back when this thread ends, however it ends.
            let _admitted = admitted;
            match auth::Guid::new(text.as_str()) {
                Ok(guid) => serve_one(stream, guid, &_admitted, &directory, &registered),
                Err(error) => eprintln!("td-busd: bad guid: {error:?}"),
            }
        });
        if let Err(error) = spawned {
            // A thread this process cannot make is not a peer it can serve.
            // `Scope::spawn` PANICS here rather than returning, and this binary
            // is built `panic=abort` — so the obvious spelling takes the whole
            // bus down at exactly the moment it is under most pressure.
            eprintln!("td-busd: cannot serve a peer: {error}");
        }
    }
    Err("the listener stopped accepting".into())
}

/// One peer, from accept to whatever ended it.
fn serve_one(
    stream: std::os::unix::net::UnixStream,
    guid: auth::Guid<'_>,
    admitted: &transport::Admitted,
    bus: &registry::Bus,
    instances: &lineage::Instances,
) {
    match transport::Connection::accept(stream, guid, admitted.quota(), bus, instances) {
        Ok(mut connection) => {
            let peer = connection.credential();
            let ended = connection.serve();
            // Named by the kernel's account of the peer rather than by
            // anything it said about itself, and by the uid the connection was
            // actually CHARGED to, which is `None` for one refused before
            // BEGIN.
            let who = format!(
                "pid {} uid {} (authenticated {:?})",
                peer.pid,
                peer.uid,
                connection.authenticated_uid()
            );
            match ended {
                transport::Ended::PeerLeft => {}
                transport::Ended::Refused(why) => eprintln!("td-busd: refused {who}: {why}"),
                transport::Ended::Failed(why) => eprintln!("td-busd: dropped {who}: {why}"),
            }
        }
        Err(error) => eprintln!("td-busd: cannot serve a peer: {error}"),
    }
}

fn dispatch(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("selftest") if args.len() == 1 => {
            let codec = corpus::selftest()?;
            let socket = transport::loopback(current_uid()?)?;
            Ok(format!("{codec}\n{socket}"))
        }
        Some("selftest") => Err(format!("selftest takes no arguments\n{}", usage())),
        Some("run") => match args.get(1).map(String::as_str) {
            Some("--socket") if args.len() == 3 => {
                run(Path::new(args.get(2).map(String::as_str).unwrap_or("")))
            }
            _ => Err(format!("run needs --socket PATH\n{}", usage())),
        },
        Some("probe") if args.len() == 2 => {
            let path = Path::new(args.get(1).map(String::as_str).unwrap_or(""));
            transport::probe(path, current_uid()?)
        }
        Some("probe") => Err(format!("probe takes one socket path\n{}", usage())),
        Some(other) => Err(format!("unrecognised subcommand '{other}'\n{}", usage())),
        None => Err(usage()),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(summary) => println!("{summary}"),
        Err(failure) => {
            eprintln!("td-busd: {failure}");
            process::exit(2);
        }
    }
}

#[cfg(test)]
const SOURCES: &[(&str, &str)] = &[
    ("main", include_str!("main.rs")),
    ("auth", include_str!("auth.rs")),
    ("sys", include_str!("sys.rs")),
    ("transport", include_str!("transport.rs")),
    ("authscript", include_str!("authscript.rs")),
    ("corpus", include_str!("corpus.rs")),
    ("lineage", include_str!("lineage.rs")),
    ("message", include_str!("message.rs")),
    ("name", include_str!("name.rs")),
    ("policy", include_str!("policy.rs")),
    ("recorded", include_str!("recorded.rs")),
    ("registry", include_str!("registry.rs")),
    ("wire", include_str!("wire.rs")),
];

#[cfg(test)]
fn source(module: &str) -> &'static str {
    SOURCES
        .iter()
        .find(|(name, _)| *name == module)
        .map(|(_, text)| *text)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn selftest_is_the_only_subcommand_that_does_anything() {
        assert!(dispatch(&argv(&["selftest"])).is_ok());
        assert!(dispatch(&argv(&["selftest", "extra"])).is_err());
        assert!(dispatch(&argv(&[])).is_err());
        assert!(dispatch(&argv(&["monitor"])).is_err());
    }

    /// The supervision units in APPLICATIONS.md §A spell both of these, and
    /// both need their arguments. A `run` that took none would bind whatever
    /// path an empty string names; a `probe` that took none would report a bus
    /// it never contacted.
    #[test]
    fn the_supervised_subcommands_need_their_arguments() {
        for wrong in [
            vec!["run"],
            vec!["run", "/run/user/1000/bus"],
            vec!["run", "--socket"],
            vec!["run", "--socket", "a", "b"],
            vec!["probe"],
            vec!["probe", "a", "b"],
        ] {
            let refusal = match dispatch(&argv(&wrong)) {
                Ok(output) => panic!("{wrong:?} claimed success: {output}"),
                Err(refusal) => refusal,
            };
            assert!(refusal.contains("usage:"), "{wrong:?}: {refusal}");
        }
    }

    /// Connect, and report whether the bus closed the connection without
    /// serving it. `Ok(0)` from a read is the refusal: there is nothing to say
    /// before a handshake, so a refused peer sees EOF.
    #[cfg(test)]
    fn refused_within(path: &Path, within: std::time::Duration) -> bool {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if let Ok(mut over) = UnixStream::connect(path) {
                if over
                    .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                    .is_ok()
                {
                    let mut byte = [0u8; 1];
                    if matches!(over.read(&mut byte), Ok(0)) {
                        return true;
                    }
                }
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    /// Complete the client half of EXTERNAL and return the bus's guid. `None`
    /// means this peer was not served — which is what distinguishes a
    /// connection the bus took from one it accepted and closed.
    #[cfg(test)]
    fn handshake(stream: &std::os::unix::net::UnixStream) -> Option<String> {
        use std::io::{Read, Write};

        let mut stream = stream.try_clone().ok()?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok()?;
        let uid = current_uid().ok()?;
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        stream
            .write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())
            .ok()?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => return None,
                Ok(_) => line.push(byte[0]),
            }
            if line.len() > 128 {
                return None;
            }
        }
        String::from_utf8(line)
            .ok()?
            .trim_end()
            .strip_prefix("OK ")
            .map(str::to_string)
    }

    /// §D's ceiling, against a real bus, from ONE process — which is the
    /// share half rather than the global half, and is what a test running in
    /// a single process can reach. Every connection it makes carries the same
    /// `SO_PEERCRED.pid`, so the bus holds it to `MAX_CONNECTIONS_PER_PEER`
    /// and closes the next.
    ///
    /// That is the point of the share: reaching the GLOBAL ceiling from one
    /// peer is precisely what §D says must not lock everyone else off the bus.
    /// The quota's own unit tests cover both halves with synthetic pids; this
    /// one proves the accept loop consults it at all.
    #[test]
    fn the_bus_holds_one_peer_to_its_share_and_closes_the_next() {
        use std::os::unix::net::UnixStream;

        let dir = std::env::temp_dir().join(format!("td-busd-ceiling-{}", process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("bus");
        let serving = path.clone();
        // `run` never returns while its listener is good, so it stays parked
        // on this thread for the life of the test binary.
        thread::spawn(move || {
            let _ = run(&serving);
        });
        let mut waited = 0;
        while !path.exists() && waited < 200 {
            thread::sleep(std::time::Duration::from_millis(10));
            waited += 1;
        }
        assert!(path.exists(), "the bus never bound {}", path.display());

        // Hold this process's whole share open. Each is a peer that connects
        // and says nothing, which is the shape being defended against.
        let mut held = Vec::new();
        for which in 0..transport::MAX_CONNECTIONS_PER_PEER {
            match UnixStream::connect(&path) {
                Ok(stream) => held.push(stream),
                Err(error) => panic!("peer {which} could not connect: {error}"),
            }
        }

        // Poll for the refusal rather than sleeping a fixed time and hoping.
        // The accept loop has to have taken all of the above before the next
        // connection is the one over the line; a fixed sleep that is too short
        // under a loaded parallel test binary makes this test fail on timing
        // rather than on behaviour.
        assert!(
            refused_within(&path, std::time::Duration::from_secs(20)),
            "the bus served a peer past this one's share"
        );

        // And the share is not a one-way door: letting one go makes room, and
        // the peer that takes it is SERVED — proved by completing the
        // handshake, not by a write. A one-byte write to an AF_UNIX socket
        // succeeds whether the far end accepted it, closed it, or never read
        // it, so the first draft of this half asserted nothing at all: gutting
        // the quota's release entirely still passed it.
        held.pop();
        let mut room = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if let Ok(stream) = UnixStream::connect(&path) {
                if let Some(guid) = handshake(&stream) {
                    room = Some(guid);
                    break;
                }
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            room.is_some(),
            "no peer was served after one left, so the share is never given back"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A probe against a path nothing is listening on FAILS. `ready=` reads
    /// this exit status, so a probe that shrugged would declare a bus that
    /// does not exist to be up.
    #[test]
    fn a_probe_against_nothing_fails() {
        let missing = "/nonexistent/td-busd/socket";
        match dispatch(&argv(&["probe", missing])) {
            Ok(output) => panic!("probe claimed success: {output}"),
            Err(refusal) => assert!(refusal.contains("connect"), "{refusal}"),
        }
    }

    /// `UNSAFE.md` surface #10, pinned. The roster there is worth what these
    /// assertions are worth: the file says two scoped allows in one module and
    /// three syscalls, and this is what makes that a checked property rather
    /// than a sentence somebody wrote once.
    ///
    /// The keyword is built at runtime so this test's own text is not what it
    /// finds.
    #[test]
    fn only_the_syscall_layer_names_the_keyword() {
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        // `deny`, not `forbid`: the two scoped allows below could not exist
        // under `forbid`, and that is the whole difference between this crate
        // before surface #10 and after it.
        assert_eq!(
            source("main").matches(&format!("#![deny({lint})]")).count(),
            1,
            "main.rs must deny the lint exactly once"
        );
        assert_eq!(
            source("main").matches(&format!("#![forbid({lint})]")).count(),
            0,
            "forbid would make the scoped allows impossible"
        );
        for (module, text) in SOURCES {
            let bare = text
                .matches(&keyword)
                .count()
                .saturating_sub(text.matches(&lint).count());
            if *module == "sys" {
                continue;
            }
            assert_eq!(bare, 0, "{module} names the {keyword} keyword");
        }
    }

    /// Two scoped allows, of two DIFFERENT shapes, and one assembly body.
    /// §D turned down a hand-rolled descriptor owner precisely to take the
    /// second one, so its count is a decision and not an accident.
    ///
    /// The adoption has two CALLERS — `receive` and `peer_pidfd` — and stays
    /// one allow, which is the property worth pinning: a second adoption site
    /// is a second place to get the ordering rule wrong, and the ordering rule
    /// is the whole discipline `UNSAFE.md` §10 records for it.
    #[test]
    fn the_syscall_layer_is_exactly_two_allows_and_one_instruction() {
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        let sys = source("sys");
        assert_eq!(
            sys.matches(&format!("#[allow({lint})]")).count(),
            2,
            "surface #10 is one syscall body and one descriptor adoption"
        );
        // One assembly block, one `syscall` instruction in it.
        assert_eq!(sys.matches("core::arch::asm!").count(), 1);
        assert_eq!(sys.matches("\"syscall\"").count(), 1);
        // One block per allow. `"{keyword} {{"` also matches the adoption's
        // one-line block, so this counts the opening rather than adding two
        // overlapping searches together — which the first draft did, and got
        // three.
        assert_eq!(
            sys.matches(&format!("{keyword} {{")).count(),
            2,
            "an added block is an amendment"
        );
        assert_eq!(
            sys.matches("OwnedFd::from_raw_fd").count(),
            1,
            "the adoption appears once and nowhere else"
        );
    }

    /// The ACCEPT arm, not just the helper it calls.
    ///
    /// `a_kernel_that_gives_no_pidfd_leaves_the_peer_unidentified` asserts
    /// what `unidentifiable` returns and says nothing about `accept` still
    /// calling it. On a kernel that HAS the option every test takes the `Ok`
    /// arm, so mutating the `Err` arm to `Unconfined` — the exact fail-open
    /// this whole design exists to prevent — leaves the helper and its test
    /// untouched. A review found that mutation surviving the entire suite.
    #[test]
    fn the_accept_path_refuses_a_peer_it_cannot_identify() {
        let transport = source("transport");
        assert!(
            transport.contains("Err(why) => Self::unidentifiable(&why),"),
            "accept no longer routes a missing pidfd to unidentifiable"
        );
        // And the answer it must never reach appears nowhere in the module.
        // `Unconfined` is a conclusion only `lineage` is in a position to
        // draw, and the transport naming it at all would be the shape of the
        // mutation above.
        let grant = format!("Identity::{}", "Unconfined");
        assert_eq!(
            transport.matches(&grant).count(),
            0,
            "the transport names the grant only lineage may conclude"
        );
    }

    /// The registry's two arms are told who is calling by the KERNEL.
    ///
    /// `credential.pid` is a number sampled at `connect(2)`, and both arms
    /// used it. The difference is invisible to a test that runs both ends in
    /// one process — there the two answers agree, and would agree under the
    /// mutation — and visible only once the caller has been reaped and its
    /// number handed on, which no in-process test can stage on demand. So the
    /// source is pinned: the pid the registry is given comes from
    /// `caller_pid`, and the sampled number appears in these two arms not at
    /// all.
    #[test]
    fn the_registry_is_told_who_is_calling_by_the_kernel() {
        // COMMENTS STRIPPED FIRST. A reviewer defeated the raw-text version by
        // commenting the guard out: the text `self.caller(&RealProcfs)` was
        // still there to be counted while the behaviour was gone, and all 236
        // tests passed. td-jail's confinement module learnt this the same way
        // and its comment says so; this is the same stripper.
        let transport = without_line_comments(&without_block_comments(source("transport")));
        let Some(from) = transport.find("fn jail_register(") else {
            panic!("the registration arm is gone");
        };
        let Some(span) = transport[from..].find("fn credentials_for(") else {
            panic!("the two arms no longer end where this test slices them");
        };
        let arms = transport.get(from..from + span).unwrap_or("");
        assert_eq!(
            arms.matches("self.caller(&RealProcfs)").count(),
            2,
            "a registry arm stopped asking the kernel who is calling"
        );
        // Every mention of the sampled credential EXCEPT its uid, which is
        // the one field that legitimately comes from `SO_PEERCRED`: §D says
        // registration is authenticated by uid. Counting the whole word and
        // subtracting `.uid` rather than searching for `.pid` is deliberate —
        // one alias binding, `let sampled = self.credential;`, walks past the
        // narrower spelling, and a reviewer wrote exactly that mutation.
        //
        // `self.credentials_for(` is subtracted for a duller reason: it
        // starts with the same eleven characters, so a helper in this slice
        // that merely CALLS it reads to the count above as a use of the
        // sampled credential. That fired once, on a helper that did exactly
        // that and nothing else.
        let sampled = format!("self.{}", "credential");
        let uid = format!("{sampled}.uid");
        let resolver = format!("{sampled}s_for(");
        assert_eq!(
            arms.matches(&sampled)
                .count()
                .saturating_sub(arms.matches(&uid).count())
                .saturating_sub(arms.matches(&resolver).count()),
            0,
            "a registry arm went back to the number sampled at connect"
        );
    }

    /// `Hello`'s reply is queued BEFORE the name is published, and nothing
    /// else in this suite says so.
    ///
    /// The ordering is load-bearing in the direction that has no observable
    /// consequence here: publishing first would let another peer's message
    /// reach a client before the reply that tells the client its own name,
    /// which is what `say_hello`'s comment exists to prevent. A reviewer
    /// swapped the two statements and all 241 tests stayed green, so the
    /// contract lived only in a comment. It is a source-level ordering the
    /// compiler cannot express, which is what this module is for.
    ///
    /// It also underwrites `Peer::arriving`'s barrier, whose whole argument
    /// is that `publish` is the LAST thing `say_hello` does.
    #[test]
    fn a_name_is_published_after_its_hello_is_answered() {
        let transport = without_line_comments(&without_block_comments(source("transport")));
        let Some(from) = transport.find("fn say_hello(") else {
            panic!("say_hello is gone");
        };
        let Some(span) = transport[from..].find("fn bus_method(") else {
            panic!("say_hello no longer ends where this test slices it");
        };
        let body = transport.get(from..from + span).unwrap_or("");
        let queued = body.find("self.queue_own(reply)");
        let published = body.find(".publish(");
        match (queued, published) {
            (Some(queued), Some(published)) => assert!(
                queued < published,
                "the name is published before its Hello is answered"
            ),
            (queued, published) => {
                panic!("say_hello no longer both answers and publishes: {queued:?} {published:?}")
            }
        }
    }

    /// The policy is consulted BEFORE the directory on the send path.
    ///
    /// An ordering, so no behavioural test can hold it: both orders refuse
    /// the same message with the same error, and only the TIMING differs.
    /// `route` walks the peer list and stops early when it finds the name, so
    /// deciding after it makes a refusal take a different amount of work
    /// depending on whether the name it will not admit to is there — which is
    /// the one fact the refusal is shaped to withhold. A reviewer found the
    /// first draft in the wrong order, and a mutation putting it back
    /// survived the whole suite.
    #[test]
    fn the_send_path_asks_the_policy_before_the_directory() {
        let transport = without_line_comments(&without_block_comments(source("transport")));
        let Some(from) = transport.find("fn route(") else {
            panic!("the routing arm is gone");
        };
        let Some(span) = transport[from..].find("fn deliver(") else {
            panic!("the routing arm no longer ends where this test slices it");
        };
        let body = transport.get(from..from + span).unwrap_or("");
        let asked = body.find("may_talk(");
        // Two spellings reach the directory: the plain lookup, and the one
        // that resolves and records a call under the same lock. The policy
        // has to precede whichever a future edit puts first.
        let looked = ["self.bus.route(", "self.bus.route_expecting("]
            .iter()
            .filter_map(|needle| body.find(needle))
            .min();
        match (asked, looked) {
            (Some(asked), Some(looked)) => assert!(
                asked < looked,
                "the directory is consulted before the policy"
            ),
            (asked, looked) => {
                panic!("the send path no longer both asks and looks: {asked:?} {looked:?}")
            }
        }
    }

    /// A reply is decided by OWNERSHIP, before the talk set is consulted.
    ///
    /// The two are different questions and the order records which governs. A
    /// method return is addressed by `reply_serial` to a caller that already
    /// reached this connection, so filtering it by the sender's talk set drops
    /// the answer to a call the broker itself delivered — §D grants a sandbox
    /// the portal's replies, so the symmetric direction cannot be a denial.
    /// What replaces the filter is stricter, not weaker: the reply is carried
    /// only if this connection is the one that call was routed to.
    ///
    /// Pinned at the source because the harness cannot reach it. Every peer on
    /// one test bus shares an identity — both ends of a socketpair are the
    /// test process — so a bus with a confined callee and an unconfined caller
    /// cannot be built, and a reply arm that consulted the talk set would pass
    /// every test in the suite.
    #[test]
    fn a_reply_is_decided_by_ownership_and_not_by_the_talk_set() {
        let transport = without_line_comments(&without_block_comments(source("transport")));
        let Some(from) = transport.find("fn route(") else {
            panic!("the routing arm is gone");
        };
        let Some(span) = transport[from..].find("fn deliver(") else {
            panic!("the routing arm no longer ends where this test slices it");
        };
        let body = transport.get(from..from + span).unwrap_or("");
        let claimed = body.find("claim_reply(");
        let filtered = body.find("may_talk(");
        match (claimed, filtered) {
            (Some(claimed), Some(filtered)) => assert!(
                claimed < filtered,
                "a reply is filtered by the talk set before its ownership is asked"
            ),
            (claimed, filtered) => panic!(
                "the send path no longer both claims and filters: {claimed:?} {filtered:?}"
            ),
        }
    }

    /// The reservation is ONE gate, ahead of the dispatch on the caller.
    ///
    /// `may_own` used to be a single expression with no ordering to get
    /// wrong. It now has three arms and a grant list, and what keeps a
    /// permission file from claiming `org.freedesktop.portal.Desktop` is that
    /// the reservation is decided before the broker looks at who is asking.
    ///
    /// Behaviour is pinned by `nobody_may_own_a_reserved_name`, which asks on
    /// behalf of a caller that WAS granted a reserved name. This pins the
    /// SHAPE, and the two are not the same claim: a rewrite that applied the
    /// reservation inside each arm passes that test — it is exactly
    /// equivalent today — and leaves the next arm somebody adds unguarded,
    /// with no test failing to say so.
    ///
    /// It took three attempts, and the failures are worth recording because
    /// each looked sufficient. The first compared where `is_reserved_name`
    /// and the grant first APPEARED, which the per-arm rewrite satisfies
    /// because the pattern binding comes later in the text. The second added
    /// "before `match caller`, and exactly once", which a reviewer defeated
    /// by hoisting the call into a `let reserved = …` above the match and
    /// writing `!reserved` into each arm — one call, before the dispatch, and
    /// the gate gone. What actually has to be true is that the reservation
    /// RETURNS, so this pins the early exit between the two.
    ///
    /// A source pin bounds spellings, not semantics, and this one is no
    /// exception: it says the function refuses before it dispatches, in the
    /// one shape that sentence has. That is worth having and is not proof.
    #[test]
    fn the_reservation_is_one_gate_before_may_own_dispatches() {
        let policy = without_line_comments(&without_block_comments(source("policy")));
        let Some(from) = policy.find("pub fn may_own(") else {
            panic!("may_own is gone");
        };
        let body = policy.get(from..).unwrap_or("");
        let Some(span) = body.find("\n}") else {
            panic!("may_own no longer ends where this test slices it");
        };
        let body = body.get(..span).unwrap_or("");
        // The GATE as one span, rather than two landmarks with an ordering
        // between them. A reviewer defeated the landmark version by adding an
        // unrelated early return -- `if matches!(caller, Identity::Unknown(_))
        // { return false; }` -- between the hoisted reservation and the
        // match: `is_reserved_name` still came first, a `return false` still
        // came second, `match caller` still came third, and the reservation
        // was a value the arms could forget. Landmarks in the right order do
        // not say the FIRST is what the SECOND returns for; the span does.
        let gate = "if is_reserved_name(name) {\n        return false;\n    }";
        let Some(gate_at) = body.find(gate) else {
            panic!("may_own no longer opens with the reservation gate: {body}");
        };
        let Some(dispatch) = body.find("match caller") else {
            panic!("may_own no longer dispatches on the caller");
        };
        assert!(
            gate_at < dispatch,
            "the reservation no longer RETURNS before the dispatch, so it is \
             a value each arm may apply or forget rather than a gate"
        );
        assert_eq!(
            body.matches("is_reserved_name(").count(),
            1,
            "the reservation is asked more than once, so one of them is the \
             one a later arm will be written without"
        );
        // And exactly one early exit, which is the gate's. A second `return`
        // above the match is how the landmark version was defeated, and it is
        // also how a future arm-specific shortcut would creep in.
        assert_eq!(
            body.matches("return ").count(),
            1,
            "may_own returns early somewhere other than the reservation \
             gate, so the gate is no longer the only thing between the \
             caller and the dispatch"
        );
    }

    /// A holder is told about ITSELF, by its own unique name.
    ///
    /// `askable`'s second arm admits a peer asking about a well-known name it
    /// holds. The name it then answers ABOUT is the load-bearing part and no
    /// behaviour test can reach it: resolving the well-known name in the gate
    /// and looking the answer up by that same well-known name is two lookups
    /// with a gap, and in the gap the name can change hands — so the reply
    /// would carry the NEW holder's uid and pid to a caller admitted for
    /// holding it a moment ago. Answering by the caller's own unique name is
    /// correct whatever happens in the gap. Both spellings pass every test
    /// in this crate, which is exactly why the rule is stated here.
    #[test]
    fn a_holder_is_told_about_itself_and_not_about_the_name() {
        let transport = without_line_comments(&without_block_comments(source("transport")));
        let Some(from) = transport.find("fn askable(") else {
            panic!("askable is gone, so the holder exemption moved somewhere \
                    this test does not watch");
        };
        let body = transport.get(from..).unwrap_or("");
        let Some(span) = body.find("\n    }") else {
            panic!("askable no longer ends where this test slices it");
        };
        let body = body.get(..span).unwrap_or("");
        assert!(
            body.contains("mine.then(|| unique.to_string())"),
            "the holder exemption answers about a name rather than about the \
             caller, so the peer it describes is whoever holds that name when \
             the second lookup runs: {body}"
        );
        assert_eq!(
            body.matches("owner_of(").count(),
            1,
            "the holder exemption resolves the name more than once, which is \
             the gap this rule exists to close"
        );
    }

    /// Comments out, so that commenting a check out is not a way to pass the
    /// test that pins it. Lifted from `td-jail/src/main.rs`, which arrived at
    /// it the same way; the two crates are separate dependency-free locks and
    /// cannot share the helper.
    fn without_block_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        let mut depth = 0_usize;
        loop {
            let open = rest.find("/*");
            let close = rest.find("*/");
            match (depth, open, close) {
                (0, None, _) => {
                    out.push_str(rest);
                    return out;
                }
                (0, Some(at), _) => {
                    out.push_str(rest.get(..at).unwrap_or(""));
                    rest = rest.get(at.saturating_add(2)..).unwrap_or("");
                    depth = 1;
                }
                (_, Some(at), Some(shut)) if at < shut => {
                    rest = rest.get(at.saturating_add(2)..).unwrap_or("");
                    depth = depth.saturating_add(1);
                }
                (_, _, Some(shut)) => {
                    rest = rest.get(shut.saturating_add(2)..).unwrap_or("");
                    depth = depth.saturating_sub(1);
                }
                (_, _, None) => return out,
            }
        }
    }

    fn without_line_comments(source: &str) -> String {
        source
            .lines()
            .map(|line| match line.split_once("//") {
                Some((code, _)) => code,
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The stripper stands between a commented-out guard and a green suite, so
    /// it is tested rather than assumed.
    #[test]
    fn comments_are_stripped_including_nested_and_unterminated_blocks() {
        assert_eq!(without_block_comments("a/*b*/c"), "ac");
        assert_eq!(without_block_comments("a/*b/*c*/d*/e"), "ae");
        assert_eq!(without_block_comments("a/*b\nc*/d"), "ad");
        assert_eq!(without_block_comments("a/*b"), "a");
        assert_eq!(without_block_comments("plain"), "plain");
        // A `*/` with nothing open is not a comment and is left alone.
        assert_eq!(without_block_comments("a*/b"), "a*/b");
        assert_eq!(without_line_comments("keep // drop\nkeep2"), "keep \nkeep2");
    }

    /// The adoption's ORDERING in `peer_pidfd`, which is the INVERSE of
    /// `receive`'s and is the one thing about this surface a reader is most
    /// likely to "fix".
    ///
    /// `receive` adopts before it refuses, because every descriptor a
    /// `recvmsg` reports is already installed and a refusal ahead of the
    /// adoption leaks one per malformed message. `peer_pidfd` refuses before
    /// it adopts, so that `adopt` is never handed a negative number —
    /// `OwnedFd` has a validity niche and `-1` is outside it. `UNSAFE.md` §10
    /// records both orders and says which is which.
    ///
    /// Pinned in the source because no behaviour can hold it: every kernel td
    /// runs on answers a whole `i32` or fails, so the wrong order passes every
    /// test there is. What it does NOT hold is the case a first draft credited
    /// it with — a wrong option number answering a full `i32` of not-a-
    /// descriptor. That one aborts the process either way, and
    /// `the_two_socket_options_are_pinned_by_value` is what stands in front of
    /// it.
    #[test]
    fn the_pidfd_is_judged_before_it_is_adopted() {
        let sys = source("sys");
        let body = sys
            .split_once("pub fn peer_pidfd")
            .and_then(|(_, rest)| rest.split_once("\n}\n"))
            .map(|(body, _)| body)
            .unwrap_or("<no peer_pidfd>");
        let adopt = body.find("adopt(number)");
        let judge = body.find("check_pidfd_answer(");
        assert!(
            adopt.is_some() && judge.is_some(),
            "peer_pidfd no longer has the shape this test reads"
        );
        assert!(judge < adopt, "the pidfd is adopted before it is judged");
        // And the judgement is PROPAGATED. `io::Result` is `#[must_use]`, so
        // dropping the `?` on the floor takes a deliberate binding to silence
        // — which is exactly the shape a plausible refactor has, and which no
        // behaviour can catch on a kernel that never gives a bad answer.
        assert!(
            body.contains("check_pidfd_answer(number, length)?;"),
            "peer_pidfd does not propagate what it judged"
        );
    }

    /// The socket options, by number.
    ///
    /// Both are pinned at their own call site rather than taken as arguments:
    /// a wrapper accepting a level and an option name would be a general
    /// `getsockopt`, and the roster says two named reads. A third appearing
    /// here is an `UNSAFE.md` amendment rather than a diff.
    #[test]
    fn the_two_socket_options_are_pinned_by_value() {
        let sys = source("sys");
        for (name, value) in [("SO_PEERCRED", "17"), ("SO_PEERPIDFD", "77")] {
            assert!(
                sys.contains(&format!("const {name}: i32 = {value};")),
                "{name} is not pinned to {value}"
            );
            assert_eq!(
                sys.matches(&format!("{name} as usize")).count(),
                1,
                "{name} is passed to more than one call"
            );
        }
        assert_eq!(
            sys.matches("const SO_").count(),
            2,
            "surface #10 reads two options"
        );
    }

    /// The asm OPTIONS, which are the property the compiler cannot be told
    /// about any other way. `nomem`/`readonly` must stay absent: the kernel
    /// writes `MsgHdr::flags` and `MsgHdr::control_len` through a pointer, and
    /// only a bare `asm!`'s implied memory clobber makes the compiler treat
    /// those buffers as written.
    ///
    /// This test is the reason that matters. Adding `nomem` in a DEBUG build
    /// fails nothing else at all — measured — so behaviour does not catch it;
    /// in release it also breaks a descriptor test, by which point the reason
    /// looks like an unrelated EFAULT. `td-util`, `td-sh` and `td-jail` each
    /// pin this about their own bodies, and the first draft of this surface
    /// pinned allow counts and syscall numbers and not this: the one contract
    /// that is load-bearing by ABSENCE, and so the one no reader can see is
    /// being relied on.
    #[test]
    fn the_syscall_body_keeps_the_memory_clobber_it_depends_on() {
        let sys = source("sys");
        // The options CLAUSE, not the file: the doc comment above the body
        // names both forbidden options in order to explain why they are
        // absent, and a whole-file scan finds its own explanation. The first
        // draft did exactly that and redded on the prose it was documenting.
        let clause = sys
            .split_once("core::arch::asm!")
            .and_then(|(_, body)| body.split_once("options("))
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inside, _)| inside)
            .unwrap_or("<no options clause>");
        assert_eq!(
            clause, "nostack, preserves_flags",
            "the options clause is pinned whole"
        );
        // The registers of the x86-64 syscall ABI, named rather than inferred.
        for register in [
            "inlateout(\"rax\")",
            "in(\"rdi\")",
            "in(\"rsi\")",
            "in(\"rdx\")",
            "in(\"r10\")",
            "in(\"r8\")",
            "lateout(\"rcx\")",
            "lateout(\"r11\")",
        ] {
            assert!(sys.contains(register), "{register} is not in the body");
        }
    }

    /// The roster, by number. `close(2)` is deliberately absent — taking the
    /// `OwnedFd` means `std` does every close — and a fourth number appearing
    /// here is an amendment to `UNSAFE.md` rather than a diff.
    #[test]
    fn the_syscall_numbers_are_the_rostered_three() {
        let sys = source("sys");
        for (name, number) in [
            ("SYS_SENDMSG", "46"),
            ("SYS_RECVMSG", "47"),
            ("SYS_GETSOCKOPT", "55"),
        ] {
            assert!(
                sys.contains(&format!("const {name}: usize = {number};")),
                "{name} is not pinned to {number}"
            );
        }
        assert_eq!(
            sys.matches("const SYS_").count(),
            3,
            "surface #10 is three syscalls"
        );
        assert!(
            !sys.contains("SYS_CLOSE"),
            "close(2) is not on this roster: OwnedFd closes"
        );
    }

    /// The callers. `sys` is reachable only from `transport`, which is
    /// narrower than §D's draft — that named `auth.rs` too, and the handshake
    /// turned out to need no syscall of its own, because `transport` feeds it
    /// bytes it has already read.
    #[test]
    fn only_the_transport_reaches_the_syscall_layer() {
        // Built at runtime so this test's own text is not what it finds, which
        // is what lets `main` be scanned like every other module. Excluding it
        // — as the first draft did, to dodge the literals above — meant the
        // test could not catch the one file most likely to acquire a call.
        let reach = format!("{}::", "sys");
        let qualified = format!("crate::{}", "sys");
        for (module, text) in SOURCES {
            if matches!(*module, "sys" | "transport") {
                continue;
            }
            let bare = text
                .matches(&reach)
                .count()
                .saturating_sub(text.matches(&format!("\"{reach}")).count());
            assert_eq!(bare, 0, "{module} reaches the syscall layer");
            assert!(
                !text.contains(&format!("use {qualified}")),
                "{module} imports the syscall layer"
            );
        }
        assert!(
            source("transport").contains(&format!("use {qualified}")),
            "the transport is the caller the roster names"
        );
    }

    /// A module missing from `SOURCES` is one the scan above cannot see.
    #[test]
    fn the_scan_covers_every_module_the_crate_declares() {
        let declared: Vec<&str> = source("main")
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
            })
            .collect();
        assert!(!declared.is_empty(), "no module declarations were found");
        for module in &declared {
            assert!(
                SOURCES.iter().any(|(name, _)| name == module),
                "{module} is declared but not scanned"
            );
        }
        assert_eq!(
            declared.len() + 1,
            SOURCES.len(),
            "SOURCES lists something the crate does not declare"
        );
    }
}
