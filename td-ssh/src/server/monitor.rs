//! One connection's monitor: root, single-threaded, the only stage that holds
//! the host key while hostile bytes are being parsed nearby, and the only one
//! that decides anything (DESIGN.md section 4). It never accepts signed data or
//! a verdict from the worker.

use super::account::{self, Account, SingleUser};
use super::config::ServerConfig;
use super::ipc::{self, SessionGrant, TransportState};
use super::keys;
use super::log;
use crate::client::signed_auth_data;
use crate::error::{Error, Result};
use crate::kex;
use crate::keyfile::{fingerprint_of_blob, PrivateKey};
use crate::wire::{Reader, Writer};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use td_primitives::curve25519::ed25519;

/// A hard cap on all questions, on top of `MaxAuthTries` counting the failed
/// ones, since a true answer costs the monitor file reads too.
const MAX_QUERIES: u32 = 32;
const WORKER_EXIT_WAIT: Duration = Duration::from_secs(2);

pub struct MonitorArgs<'a> {
    pub config_path: &'a str,
    pub single_user: Option<SingleUser>,
}

pub fn main(args: &MonitorArgs<'_>) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(error) => {
            log::line(format_args!("td-sshd: {error}"));
            1
        }
    }
}

fn exe_command() -> Command {
    Command::new("/proc/self/exe")
}

fn single_user_args(command: &mut Command, single_user: Option<&SingleUser>) {
    if let Some(single) = single_user {
        command
            .arg("--single-user")
            .arg(&single.name)
            .env("HOME", &single.home)
            .env("SHELL", &single.shell);
    }
}

/// Takes the command by value because `Command` keeps each `Stdio` it was given:
/// a copy of the stream's far end left here means its close is never seen, and a
/// copy of the socket that the connection never ends.
fn spawn_stage(mut command: Command, socket: OwnedFd, stream: UnixStream) -> Result<Child> {
    Ok(command
        .stdin(Stdio::from(socket))
        .stdout(Stdio::from(OwnedFd::from(stream)))
        .stderr(Stdio::inherit())
        .spawn()?)
}

fn run(args: &MonitorArgs<'_>) -> Result<()> {
    // Standard descriptors are this stage's whole interface: the socket on 0,
    // the listener's status pipe on 1, logging on 2.
    let stdin_fd = std::io::stdin().as_fd().try_clone_to_owned()?;
    let socket = TcpStream::from(stdin_fd);
    // A dual-stack listener reports IPv4 peers mapped. Everything past here, from=
    // above all, sees the address in its one plain form.
    let peer = socket.peer_addr()?;
    let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
    let local = socket.local_addr()?;
    let mut status_pipe = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);

    let text = std::fs::read_to_string(args.config_path)
        .map_err(|error| Error::refused(format!("{}: {error}", args.config_path)))?;
    let config = ServerConfig::parse(&text, args.single_user.is_some())
        .map_err(|error| Error::refused(format!("{}: {error}", args.config_path)))?;
    let host_key =
        keys::load_host_key(&config.host_key, args.single_user.as_ref()).map_err(Error::refused)?;
    let deadline = Instant::now() + Duration::from_secs(config.login_grace_seconds);

    let (mut worker_stream, worker_end) = UnixStream::pair()?;
    let mut worker_command = exe_command();
    worker_command.arg("--preauth");
    single_user_args(&mut worker_command, args.single_user.as_ref());
    let mut worker = spawn_stage(
        worker_command,
        OwnedFd::from(socket.try_clone()?),
        worker_end,
    )?;

    let host_blob = kex::ed25519_public_blob(&host_key.public().key);
    let mut init = Writer::message(ipc::MONITOR_INIT);
    init.string(&host_blob);
    ipc::write_frame(&mut worker_stream, init.as_slice())?;

    let mut decisions = Decisions {
        config: &config,
        single_user: args.single_user.as_ref(),
        peer,
        host_key: &host_key,
        session_id: None,
        deadline,
        failures: 0,
        queries: 0,
        exhausted: false,
        authenticated: None,
    };
    let outcome = serve_worker(&mut worker_stream, &mut decisions, deadline);
    let authenticated = match outcome {
        Ok(state) => state,
        Err(error) => {
            let worker_ended =
                matches!(&error, Error::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof);
            // A worker told it has no tries left gets to say so to its peer, and
            // is bounded by the same wait as any other. One that hung up is ending:
            // how it ended is the one thing here a failed exploit can show.
            let status = if decisions.exhausted || worker_ended {
                wait_exit(&mut worker)
            } else {
                kill(&mut worker)
            };
            report_death(status, peer);
            // A peer that just connected and left is not worth a log line.
            return if worker_ended { Ok(()) } else { Err(error) };
        }
    };
    let (state, account_info, grant) = authenticated;

    // No two processes ever read the socket: the worker must be gone first.
    let status = wait_exit(&mut worker);
    report_death(status, peer);
    log::line(format_args!(
        "Accepted publickey for {} from {} port {} ssh2: ED25519 {}",
        account_info.name,
        peer.ip(),
        peer.port(),
        fingerprint_of_blob(&grant.key_blob)
    ));
    // Tells the listener this connection no longer counts as unauthenticated.
    let _ = status_pipe.write_all(b"A");
    drop(status_pipe);

    let effective = config.effective(&account_info.name);
    let session_grant = SessionGrant {
        user: account_info.name.clone(),
        uid: account_info.uid,
        gid: account_info.gid,
        groups: account_info.groups.clone(),
        home: account_info.home.clone(),
        shell: account_info.shell.clone(),
        // ForceCommand overrides a key's command=, as in OpenSSH.
        force_command: effective
            .force_command
            .clone()
            .or(grant.grant.command.clone()),
        permit_tty: effective.permit_tty && !grant.grant.no_pty,
        max_sessions: effective.max_sessions,
        peer_address: peer.ip().to_string(),
        peer_port: peer.port(),
        local_address: local.ip().to_string(),
        local_port: local.port(),
    };

    let (mut session_stream, session_end) = UnixStream::pair()?;
    let mut session_command = exe_command();
    session_command.arg("--session");
    single_user_args(&mut session_command, args.single_user.as_ref());
    spawn_stage(session_command, OwnedFd::from(socket), session_end)?;

    let mut frame = Writer::new();
    state.encode(&mut frame);
    session_grant.encode(&mut frame);
    frame.string(&host_blob);
    ipc::write_frame(&mut session_stream, frame.as_slice())?;

    become_signer(&host_key, session_stream, args.single_user.as_ref())
}

/// Replaces this process with the signer, handing the key over a pipe so none
/// of the parsed policy or account tables survive (DESIGN.md section 4). The
/// write cannot block: a pipe's buffer holds the seed with no reader yet.
fn become_signer(
    host_key: &PrivateKey,
    session_stream: UnixStream,
    single_user: Option<&SingleUser>,
) -> Result<()> {
    let (key_reader, mut key_writer) = std::io::pipe()?;
    let mut seed = host_key.seed();
    key_writer.write_all(&seed)?;
    td_primitives::ct::zero(&mut seed);
    drop(key_writer);
    let mut command = exe_command();
    command.arg("--signer");
    single_user_args(&mut command, single_user);
    let error = command
        .stdin(Stdio::from(key_reader))
        .stdout(Stdio::from(OwnedFd::from(session_stream)))
        .stderr(Stdio::inherit())
        .exec();
    Err(Error::Io(error))
}

struct AcceptedKey {
    key_blob: Vec<u8>,
    grant: super::authkeys::Grant,
}

struct Decisions<'a> {
    config: &'a ServerConfig,
    single_user: Option<&'a SingleUser>,
    peer: SocketAddr,
    host_key: &'a PrivateKey,
    deadline: Instant,
    session_id: Option<[u8; 32]>,
    failures: u32,
    queries: u32,
    /// `TOO_MANY` has been answered, and the connection is ending.
    exhausted: bool,
    authenticated: Option<(Account, AcceptedKey)>,
}

/// What the account and key policy say for one offered key, or why not.
fn evaluate(
    decisions: &Decisions<'_>,
    user: &str,
    key: &[u8; 32],
) -> Option<(Account, super::authkeys::Grant)> {
    let account = match account::resolve(user, decisions.config, decisions.single_user) {
        Ok(account) => account,
        Err(reason) => {
            log::line(format_args!(
                "td-sshd: refusing {user:?} from {}: {reason}",
                decisions.peer.ip()
            ));
            return None;
        }
    };
    let effective = decisions.config.effective(user);
    let grant = account::find_key(
        &account,
        &effective,
        key,
        &decisions.peer.ip().to_string(),
        decisions.single_user.is_some(),
        decisions.deadline,
    )?;
    Some((account, grant))
}

impl Decisions<'_> {
    /// Counts a refusal. True when the connection has now used up its attempts.
    fn refuse(&mut self) -> bool {
        self.failures = self.failures.saturating_add(1);
        self.failures >= self.config.max_auth_tries
    }

    fn answer(&mut self, ok: bool) -> Vec<u8> {
        if !ok && self.refuse() {
            return vec![ipc::MONITOR_TOO_MANY];
        }
        let mut reply = Writer::message(ipc::MONITOR_ANSWER);
        reply.bool(ok);
        reply.into_vec()
    }

    fn on_key_acceptable(&mut self, reader: &mut Reader<'_>) -> Result<Vec<u8>> {
        let user = reader.str()?;
        let blob = reader.string()?;
        reader.finish()?;
        let key = kex::parse_ed25519_public(blob)?;
        let ok = evaluate(self, user, &key).is_some();
        Ok(self.answer(ok))
    }

    fn on_authenticate(&mut self, reader: &mut Reader<'_>) -> Result<Vec<u8>> {
        let user = reader.str()?;
        let blob = reader.string()?;
        let signature = reader.string()?;
        reader.finish()?;
        let session_id = self
            .session_id
            .ok_or_else(|| Error::protocol("authentication before key exchange"))?;
        let key = kex::parse_ed25519_public(blob)?;
        let signature = kex::parse_ed25519_signature(signature)?;
        // The signed bytes are rebuilt here: the worker supplies none of them
        // but the fields the signature must then cover.
        let signed = signed_auth_data(&session_id, user, blob);
        if ed25519::verify(&key, &signed, &signature).is_err() {
            log::line(format_args!(
                "td-sshd: bad signature for {user:?} from {}",
                self.peer.ip()
            ));
            return Ok(self.answer(false));
        }
        match evaluate(self, user, &key) {
            Some((account, grant)) => {
                self.authenticated = Some((
                    account,
                    AcceptedKey {
                        key_blob: blob.to_vec(),
                        grant,
                    },
                ));
                Ok(self.answer(true))
            }
            None => Ok(self.answer(false)),
        }
    }
}

type Authenticated = (TransportState, Account, AcceptedKey);

fn serve_worker(
    worker: &mut UnixStream,
    decisions: &mut Decisions<'_>,
    deadline: Instant,
) -> Result<Authenticated> {
    loop {
        // The one absolute deadline holds for the whole frame: neither staying busy
        // nor sending it a byte at a time can extend it.
        let mut bounded = ipc::UntilDeadline::new(worker, deadline);
        let frame = ipc::read_frame(&mut bounded).map_err(|error| match error {
            Error::Io(io) if ipc::is_timeout(&io) => Error::refused("login grace time expired"),
            other => other,
        })?;
        let mut reader = Reader::new(&frame);
        let kind = reader.u8()?;
        let reply = match kind {
            ipc::WORKER_SIGN => {
                if decisions.session_id.is_some() {
                    return Err(Error::protocol("a second signature request"));
                }
                let hash: [u8; 32] = reader.array()?;
                reader.finish()?;
                decisions.session_id = Some(hash);
                let signature = kex::ed25519_signature_blob(&decisions.host_key.sign(&hash)?);
                let mut reply = Writer::message(ipc::MONITOR_SIGNATURE);
                reply.string(&signature);
                reply.into_vec()
            }
            ipc::WORKER_KEY_ACCEPTABLE | ipc::WORKER_AUTHENTICATE => {
                if decisions.authenticated.is_some() {
                    return Err(Error::protocol("a question after authentication"));
                }
                decisions.queries = decisions.queries.saturating_add(1);
                if decisions.queries > MAX_QUERIES {
                    vec![ipc::MONITOR_TOO_MANY]
                } else if kind == ipc::WORKER_KEY_ACCEPTABLE {
                    decisions.on_key_acceptable(&mut reader)?
                } else {
                    decisions.on_authenticate(&mut reader)?
                }
            }
            ipc::WORKER_AUTHENTICATED => {
                let (account, accepted) = decisions
                    .authenticated
                    .take()
                    .ok_or_else(|| Error::protocol("no authentication succeeded"))?;
                let state = TransportState::decode(&mut reader)?;
                reader.finish()?;
                if Some(state.session_id) != decisions.session_id {
                    return Err(Error::protocol("transport state names another session"));
                }
                return Ok((state, account, accepted));
            }
            other => return Err(Error::protocol(format!("unknown worker request {other}"))),
        };
        ipc::write_frame(worker, &reply)?;
        // No further question gets an answer, whatever the worker sends next.
        if reply.first() == Some(&ipc::MONITOR_TOO_MANY) {
            decisions.exhausted = true;
            return Err(Error::refused(format!(
                "too many authentication failures from {}",
                decisions.peer.ip()
            )));
        }
    }
}

/// Ends the worker. How it ended, if that was already its own doing.
fn kill(worker: &mut Child) -> Option<ExitStatus> {
    if let Ok(Some(status)) = worker.try_wait() {
        return Some(status);
    }
    let _ = worker.kill();
    let _ = worker.wait();
    None
}

/// Waits for the worker's own exit, then ends it: nothing else may read the
/// socket while it might. How it ended, if that was its own doing.
fn wait_exit(worker: &mut Child) -> Option<ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < WORKER_EXIT_WAIT {
        match worker.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    kill(worker)
}

/// A worker that exits has nothing to say here, having logged what it had. One
/// that a signal ended, which the monitor did not send, is what a failed exploit
/// looks like. Under the filter that reads `SIGSYS` whatever it was: `std`'s handler
/// for a fault calls `rt_sigaction`, and the filter kills it for that.
fn abnormal_death(status: ExitStatus) -> Option<String> {
    let signal = status.signal()?;
    let name = match signal {
        4 => " (SIGILL)",
        6 => " (SIGABRT)",
        7 => " (SIGBUS)",
        9 => " (SIGKILL)",
        // Only a fault before the filter is on.
        11 => " (SIGSEGV)",
        31 => " (SIGSYS)",
        _ => "",
    };
    Some(format!("signal {signal}{name}"))
}

fn report_death(status: Option<ExitStatus>, peer: SocketAddr) {
    if let Some(death) = status.and_then(abnormal_death) {
        log::line(format_args!(
            "td-sshd: the pre-auth worker for {} port {} died by {death}",
            peer.ip(),
            peer.port()
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::keyfile::PublicKey;

    #[test]
    fn only_a_signal_is_an_abnormal_death() {
        // As `wait` encodes them: a status in the high byte, or a signal in the low.
        assert_eq!(abnormal_death(ExitStatus::from_raw(0)), None);
        assert_eq!(abnormal_death(ExitStatus::from_raw(1 << 8)), None);
        assert_eq!(
            abnormal_death(ExitStatus::from_raw(31)).unwrap(),
            "signal 31 (SIGSYS)"
        );
        assert_eq!(
            abnormal_death(ExitStatus::from_raw(11)).unwrap(),
            "signal 11 (SIGSEGV)"
        );
        assert_eq!(
            abnormal_death(ExitStatus::from_raw(15)).unwrap(),
            "signal 15"
        );
    }

    /// Only a death it did not bring about is news.
    #[test]
    fn a_worker_the_monitor_killed_reports_nothing_and_one_that_ended_does() {
        let mut running = Command::new("/bin/sh")
            .args(["-c", "read _"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        assert_eq!(kill(&mut running), None);
        let mut ended = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        assert_eq!(wait_exit(&mut ended).unwrap().code(), Some(3));
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use td_fs::TempDir;

    const USER: &str = "tester";

    /// A monitor for one fake worker, with `CLIENT` as the account's one key.
    struct Fixture {
        _dir: TempDir,
        config: ServerConfig,
        single: SingleUser,
        host_key: PrivateKey,
        client: PrivateKey,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new("td-ssh-monitor").unwrap();
        let client = PrivateKey::from_seed(&[7; 32], "").unwrap();
        let keys = dir.path().join("keys");
        let line = PublicKey {
            key: client.public().key,
            comment: "c".into(),
        }
        .to_line();
        std::fs::write(&keys, line).unwrap();
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = std::fs::metadata("/proc/self").unwrap().uid();
        let text = format!(
            "Port 22\nListenAddress 127.0.0.1\nHostKey /k\nAuthorizedKeysFile {}\nMaxAuthTries 3\n",
            keys.display()
        );
        Fixture {
            _dir: dir,
            config: ServerConfig::parse(&text, false).unwrap(),
            single: SingleUser {
                name: USER.into(),
                uid,
                gid: uid,
                home: "/".into(),
                shell: "/bin/sh".into(),
            },
            host_key: PrivateKey::from_seed(&[8; 32], "").unwrap(),
            client,
        }
    }

    impl Fixture {
        fn decisions(&self, deadline: Instant) -> Decisions<'_> {
            Decisions {
                config: &self.config,
                single_user: Some(&self.single),
                peer: "127.0.0.1:1".parse().unwrap(),
                host_key: &self.host_key,
                deadline,
                session_id: None,
                failures: 0,
                queries: 0,
                exhausted: false,
                authenticated: None,
            }
        }
    }

    fn ask(worker: &mut UnixStream, request: &Writer) -> Result<Vec<u8>> {
        ipc::write_frame(worker, request.as_slice())?;
        ipc::read_frame(worker)
    }

    fn query(user: &str, key: &[u8; 32]) -> Writer {
        let mut request = Writer::message(ipc::WORKER_KEY_ACCEPTABLE);
        request.str(user).string(&kex::ed25519_public_blob(key));
        request
    }

    fn signed(fixture: &Fixture, session_id: &[u8; 32]) -> Writer {
        let blob = kex::ed25519_public_blob(&fixture.client.public().key);
        let signature = fixture
            .client
            .sign(&signed_auth_data(session_id, USER, &blob))
            .unwrap();
        let mut request = Writer::message(ipc::WORKER_AUTHENTICATE);
        request
            .str(USER)
            .string(&blob)
            .string(&kex::ed25519_signature_blob(&signature));
        request
    }

    #[test]
    fn too_many_ends_the_conversation_and_a_later_valid_login_gets_no_answer() {
        let fixture = fixture();
        let (mut worker, mut monitor) = UnixStream::pair().unwrap();
        let fixture = &fixture;
        let deadline = Instant::now() + Duration::from_secs(20);
        let session_id = [3u8; 32];
        std::thread::scope(|scope| {
            let served = scope.spawn(move || {
                let mut decisions = fixture.decisions(deadline);
                let outcome = serve_worker(&mut monitor, &mut decisions, deadline);
                (outcome.is_err(), decisions.exhausted)
            });
            let mut sign = Writer::message(ipc::WORKER_SIGN);
            sign.string(&session_id);
            assert_eq!(ask(&mut worker, &sign).unwrap()[0], ipc::MONITOR_SIGNATURE);
            let stranger = [9u8; 32];
            for _ in 0..2 {
                let reply = ask(&mut worker, &query(USER, &stranger)).unwrap();
                assert_eq!(reply[0], ipc::MONITOR_ANSWER);
            }
            let reply = ask(&mut worker, &query(USER, &stranger)).unwrap();
            assert_eq!(reply, [ipc::MONITOR_TOO_MANY]);
            // The worker cannot tell the monitor it is still alive and retry.
            let request = signed(fixture, &session_id);
            let late = ipc::write_frame(&mut worker, request.as_slice());
            assert_eq!(served.join().unwrap(), (true, true));
            assert!(late.is_err() || ipc::read_frame(&mut worker).is_err());
        });
    }

    /// The same signed request before the tries run out is what the test above
    /// must be refusing, so it has to work.
    #[test]
    fn a_valid_login_succeeds_within_the_tries() {
        let fixture = fixture();
        let (mut worker, mut monitor) = UnixStream::pair().unwrap();
        let fixture = &fixture;
        let deadline = Instant::now() + Duration::from_secs(20);
        let session_id = [3u8; 32];
        std::thread::scope(|scope| {
            let served = scope.spawn(move || {
                let mut decisions = fixture.decisions(deadline);
                let _ = serve_worker(&mut monitor, &mut decisions, deadline);
                decisions.authenticated.is_some()
            });
            let mut sign = Writer::message(ipc::WORKER_SIGN);
            sign.string(&session_id);
            ask(&mut worker, &sign).unwrap();
            let reply = ask(&mut worker, &query(USER, &[9; 32])).unwrap();
            assert_eq!(reply[..2], [ipc::MONITOR_ANSWER, 0]);
            let reply = ask(&mut worker, &signed(fixture, &session_id)).unwrap();
            assert_eq!(reply[..2], [ipc::MONITOR_ANSWER, 1]);
            drop(worker);
            assert!(served.join().unwrap());
        });
    }

    #[test]
    fn a_worker_dribbling_a_frame_cannot_extend_the_grace_time() {
        let fixture = fixture();
        let (mut worker, mut monitor) = UnixStream::pair().unwrap();
        let fixture = &fixture;
        let start = Instant::now();
        let deadline = start + Duration::from_millis(300);
        std::thread::scope(|scope| {
            let served = scope.spawn(move || {
                let mut decisions = fixture.decisions(deadline);
                let outcome = serve_worker(&mut monitor, &mut decisions, deadline);
                (outcome.is_err(), start.elapsed())
            });
            for byte in [0u8, 0, 1, 0].into_iter().chain([1; 200]) {
                if worker.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
                if served.is_finished() {
                    break;
                }
            }
            let (failed, took) = served.join().unwrap();
            assert!(failed);
            // The bytes alone take about 10 s to send.
            assert!(took < Duration::from_secs(5), "took {took:?}");
        });
    }
}
